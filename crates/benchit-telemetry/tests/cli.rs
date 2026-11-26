//! End-to-end tests of the `benchit-telemetry` binary: `run` exit codes, signal handling and leases.
use benchit_telemetry_core::Store;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_benchit-telemetry");

fn cmd(dir: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.env("BENCHIT_TELEMETRY_DIR", dir).env_remove("XDG_STATE_HOME");
    c
}

fn leases(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("fast-until.d")).map(|r| r.count()).unwrap_or(0)
}

fn marks(dir: &Path) -> Vec<benchit_telemetry_core::store::Mark> {
    Store::open_readonly(dir.join("metrics.db")).and_then(|s| s.marks(10)).unwrap_or_default()
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_exit(mut c: Child) -> std::process::ExitStatus {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(st) = c.try_wait().unwrap() {
            return st;
        }
        if Instant::now() > end {
            let _ = c.kill();
            panic!("process did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Make the directory look like a daemon is feeding it, so `run` takes a lease.
fn fake_fresh_feed(dir: &Path) {
    let ts = benchit_telemetry_core::timefmt::now();
    let snap = benchit_telemetry_core::Snapshot { ts, host: "t".into(), ..Default::default() };
    std::fs::write(dir.join("current.json"), serde_json::to_vec(&snap).unwrap()).unwrap();
}

#[test]
fn run_reports_the_childs_exit_code_and_closes_the_mark() {
    let d = tempfile::tempdir().unwrap();
    let st = cmd(d.path()).args(["run", "job", "--", "sh", "-c", "exit 7"]).output().unwrap();
    assert_eq!(st.status.code(), Some(7));
    let err = String::from_utf8_lossy(&st.stderr);
    assert!(err.contains("no daemon is feeding"), "must warn when nothing records: {err}");
    let m = marks(d.path());
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].exit_code, Some(7));
    assert!(m[0].t_end.is_some());
    assert_eq!(leases(d.path()), 0);
}

#[test]
fn run_without_a_daemon_is_fast() {
    let d = tempfile::tempdir().unwrap();
    let t = Instant::now();
    let st = cmd(d.path()).args(["run", "quick", "--", "true"]).output().unwrap();
    assert_eq!(st.status.code(), Some(0));
    assert!(t.elapsed() < Duration::from_secs(3), "no fixed 2x2.5 s sleeps when no daemon is running");
}

#[test]
fn run_distinguishes_missing_and_unexecutable_commands() {
    let d = tempfile::tempdir().unwrap();
    let st = cmd(d.path()).args(["run", "nf", "--", "/nonexistent/binary"]).output().unwrap();
    assert_eq!(st.status.code(), Some(127));
    let f = d.path().join("not-executable");
    std::fs::write(&f, "#!/bin/sh\n").unwrap();
    let st = cmd(d.path()).args(["run", "pd", "--", f.to_str().unwrap()]).output().unwrap();
    assert_eq!(st.status.code(), Some(126), "permission denied is 126, not 127");
    assert!(marks(d.path()).iter().all(|m| m.t_end.is_some()), "failed starts still close their mark");
}

#[test]
fn run_stores_the_command_line_quoted() {
    let d = tempfile::tempdir().unwrap();
    cmd(d.path()).args(["run", "q", "--", "sh", "-c", "echo 'a b'"]).output().unwrap();
    let m = marks(d.path());
    assert_eq!(m[0].cmd.as_deref(), Some("sh -c 'echo '\\''a b'\\'''"));
}

fn spawn_run_sleeping(dir: &Path, name: &str) -> Child {
    cmd(dir).args(["run", name, "--", "sleep", "60"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
}

fn signal_survives(sig: i32, expect_exit: i32, expect_note: &str) {
    let d = tempfile::tempdir().unwrap();
    fake_fresh_feed(d.path());
    let child = spawn_run_sleeping(d.path(), "sig");
    let pid = child.id();
    // the lease exists while the benchmark is (about to be) running, and the mark is open
    wait_for("lease", || leases(d.path()) == 1);
    wait_for("open mark", || marks(d.path()).iter().any(|m| m.t_end.is_none()));
    // SAFETY: signalling the child process we spawned.
    unsafe { libc::kill(pid as i32, sig) };
    let st = wait_exit(child);
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(st.signal(), None, "run must not die from the signal");
    assert_eq!(st.code(), Some(expect_exit), "128 + signal");
    let m = marks(d.path());
    assert!(m[0].t_end.is_some(), "the mark must be closed");
    assert_eq!(m[0].exit_code, Some(i64::from(expect_exit)));
    assert!(m[0].note.as_deref().unwrap_or("").contains(expect_note), "{:?}", m[0].note);
    assert_eq!(leases(d.path()), 0, "the lease must be removed");
}

#[test]
fn run_survives_sigint_and_cleans_up() {
    signal_survives(libc::SIGINT, 130, "SIGINT");
}

#[test]
fn run_survives_sigterm_and_cleans_up() {
    signal_survives(libc::SIGTERM, 143, "SIGTERM");
}

#[test]
fn run_survives_sighup() {
    signal_survives(libc::SIGHUP, 129, "SIGHUP");
}

#[test]
fn a_child_killed_by_a_signal_exits_128_plus_signal() {
    let d = tempfile::tempdir().unwrap();
    let st = cmd(d.path()).args(["run", "k", "--", "sh", "-c", "kill -KILL $$"]).output().unwrap();
    assert_eq!(st.status.code(), Some(137));
    let m = marks(d.path());
    assert_eq!(m[0].exit_code, Some(137));
    assert!(m[0].note.as_deref().unwrap().contains("SIGKILL"));
}

#[test]
fn crashed_run_leaves_only_a_short_lease() {
    use benchit_telemetry::lease;
    let d = tempfile::tempdir().unwrap();
    // simulate `kill -9` of run: its lease stays behind, but it is short and its pid is gone
    lease::write(d.path(), 4_194_000, benchit_telemetry_core::timefmt::now_f64() + lease::LEASE_SECS).unwrap();
    const _: () = assert!(lease::LEASE_SECS <= 60.0, "leases must be short, not 7 days");
    assert!(!lease::fast_active(d.path(), benchit_telemetry_core::timefmt::now_f64()), "dead pid ignored");
}

#[test]
fn history_rejects_ambiguous_times_on_the_cli() {
    let d = tempfile::tempdir().unwrap();
    drop(Store::open(d.path().join("metrics.db")).unwrap());
    let o = cmd(d.path()).args(["history", "--from", "90"]).output().unwrap();
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("ambiguous"));
    let o = cmd(d.path()).args(["history", "--since", "5m", "--from", "-1h"]).output().unwrap();
    assert!(!o.status.success(), "--since and --from conflict");
    let o = cmd(d.path()).args(["history", "--mark", "x", "--from", "-1h"]).output().unwrap();
    assert!(!o.status.success(), "--mark and --from conflict");
}

#[test]
fn daemon_rejects_bad_tick_values() {
    let d = tempfile::tempdir().unwrap();
    for t in ["nan", "0", "0.5", "-1", "inf"] {
        let o = cmd(d.path()).args(["daemon", "--tick", t]).output().unwrap();
        assert!(!o.status.success(), "--tick {t}");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(!err.contains("panicked"), "--tick {t} must not panic: {err}");
    }
}

#[test]
fn old_database_is_migrated_by_writing_commands() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("metrics.db");
    drop(Store::open(&db).unwrap());
    let c = benchit_telemetry_core::store::raw_connection(&db).unwrap();
    c.pragma_update(None, "user_version", 0).unwrap(); // looks like the first release
    drop(c);
    let o = cmd(d.path()).args(["mark", "start", "m1"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = cmd(d.path()).arg("status").output().unwrap();
    assert!(String::from_utf8_lossy(&o.stdout).contains("schema v2"));
}

#[test]
fn newer_database_is_refused_not_downgraded() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("metrics.db");
    drop(Store::open(&db).unwrap());
    let c = benchit_telemetry_core::store::raw_connection(&db).unwrap();
    c.pragma_update(None, "user_version", 77).unwrap();
    drop(c);
    let o = cmd(d.path()).args(["mark", "start", "m1"]).output().unwrap();
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("newer benchit-telemetry"));
    let v: i64 = benchit_telemetry_core::store::raw_connection(&db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 77, "untouched");
}

#[test]
fn a_signal_during_the_pre_start_wait_still_closes_everything() {
    let d = tempfile::tempdir().unwrap();
    fake_fresh_feed(d.path());
    let child = spawn_run_sleeping(d.path(), "early");
    let pid = child.id();
    wait_for("lease", || leases(d.path()) == 1);
    // SAFETY: signalling the child process we spawned (it is still in its pre-start wait).
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    let st = wait_exit(child);
    assert_eq!(st.code(), Some(143));
    let m = marks(d.path());
    assert_eq!(m.len(), 1);
    assert!(m[0].t_end.is_some());
    assert!(m[0].note.as_deref().unwrap().contains("before the command started"));
    assert_eq!(leases(d.path()), 0);
}
