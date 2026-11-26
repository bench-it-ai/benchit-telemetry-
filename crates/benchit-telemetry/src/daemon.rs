//! Logger daemon: collect every tick, write panel feeds, store samples, prune, serve the API.
use crate::api::{self, AppState};
use crate::{lease, paths};
use anyhow::{bail, Context, Result};
use benchit_telemetry_core::collect::{self, recompute_totals};
use benchit_telemetry_core::panel::{self, RecInfo};
use benchit_telemetry_core::{timefmt, Collector, CollectorConfig, Snapshot, Store};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub dir: PathBuf,
    pub db: PathBuf,
    /// Collection / panel refresh period (at least one second).
    pub tick: Duration,
    /// Storage period in seconds.
    pub interval: f64,
    pub retain_days: u32,
    /// Extra flat-JSON panel feeds (in addition to `<dir>/latest.json`).
    pub panel_json: Vec<PathBuf>,
    pub store: bool,
    pub listen: Option<SocketAddr>,
    pub api_writable: bool,
    /// Extra `Host` header names the API accepts besides localhost.
    pub allow_hosts: Vec<String>,
    pub collector: CollectorConfig,
}

/// Atomically replace `path` with `data` (written 0600 to a per-process temp file first).
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(data)?;
    }
    std::fs::rename(&tmp, path)
}

/// Create `dir` (and parents) with mode 0700 where it does not exist yet.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Take an exclusive `flock` on `<dir>/daemon.lock`; the returned file must stay open.
pub fn lock_dir(dir: &Path) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let path = dir.join(paths::LOCK_FILE);
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: flock on a file descriptor we own.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another benchit-telemetry daemon is already using {} (lock held on {})", dir.display(), path.display());
    }
    Ok(f)
}

/// Mean GPU power/utilisation over the ticks of one storage interval (GPU values are point
/// samples, so a 2 s tick would otherwise stand in for the whole 10 s).
#[derive(Default)]
struct GpuAcc {
    by_idx: BTreeMap<u32, [(f64, u32); 2]>,
}

impl GpuAcc {
    fn add(&mut self, s: &Snapshot) {
        for g in &s.gpus {
            let e = self.by_idx.entry(g.idx).or_default();
            if let Some(p) = g.power_w {
                e[0].0 += p;
                e[0].1 += 1;
            }
            if let Some(u) = g.util_pct {
                e[1].0 += u;
                e[1].1 += 1;
            }
        }
    }

    /// Replace point values in `s` with the interval means, then reset.
    fn apply_and_reset(&mut self, s: &mut Snapshot) {
        for g in &mut s.gpus {
            if let Some(e) = self.by_idx.get(&g.idx) {
                let mean = |(sum, n): (f64, u32)| (n > 0).then(|| ((sum / f64::from(n)) * 10.0).round() / 10.0);
                if g.power_w.is_some() {
                    g.power_w = mean(e[0]).or(g.power_w);
                }
                if g.util_pct.is_some() {
                    g.util_pct = mean(e[1]).or(g.util_pct);
                }
            }
        }
        recompute_totals(s);
        self.by_idx.clear();
    }
}

fn collect_loop(
    cfg: DaemonConfig,
    current: Arc<RwLock<Option<Snapshot>>>,
    rec_state: Arc<RwLock<Option<RecInfo>>>,
) -> Result<()> {
    let mut store = if cfg.store { Some(Store::open(&cfg.db)?) } else { None };
    let mut rows = 0;
    if let Some(s) = &store {
        s.set_meta("retain_days", &cfg.retain_days.to_string())?;
        s.set_meta("sample_interval_s", &cfg.interval.to_string())?;
        rows = s.count()?;
    }
    let mut collector = Collector::new(cfg.collector.clone());
    // Scheduling uses the monotonic clock: a wall-clock step (NTP at boot, manual change) must
    // never stop or burst storage. The wall clock is only used for the `ts` written to rows.
    let mut last_store: Option<Instant> = None;
    let mut last_store_ts = 0i64;
    let mut last_prune: Option<Instant> = None;
    let mut first = true;
    let mut acc = GpuAcc::default();
    let panel_paths: Vec<PathBuf> =
        std::iter::once(cfg.dir.join(paths::PANEL_FILE)).chain(cfg.panel_json.iter().cloned()).collect();
    loop {
        let started = Instant::now();
        let snap = collector.sample();
        acc.add(&snap);
        let fast = lease::fast_active(&cfg.dir, timefmt::now_f64());
        let interval = if fast { cfg.tick.as_secs_f64() } else { cfg.interval };
        if let Some(s) = store.as_mut() {
            let due = last_store.is_none_or(|t| t.elapsed().as_secs_f64() >= interval - 0.05);
            // the very first sample has no rate values, so it is not stored
            if !first && due {
                // rates, GPU power and energy inputs are averaged over the whole interval
                let span = last_store.map_or(interval, |t| t.elapsed().as_secs_f64());
                let mut stored = collector.interval_snapshot(&snap, (span * 3.0).max(30.0));
                acc.apply_and_reset(&mut stored);
                match s.insert(&stored) {
                    Ok(()) => {
                        rows += 1;
                        last_store = Some(Instant::now());
                        last_store_ts = stored.ts;
                    }
                    Err(e) => eprintln!("store insert failed: {e:#}"),
                }
            } else if first {
                let _ = collector.interval_snapshot(&snap, 1.0); // establish the interval baseline
            }
            if last_prune.is_none_or(|t| t.elapsed() >= Duration::from_secs(3600)) {
                match s.prune(cfg.retain_days, timefmt::now()) {
                    Ok(_) => rows = s.count().unwrap_or(rows),
                    Err(e) => eprintln!("prune failed: {e:#}"),
                }
                last_prune = Some(Instant::now());
            }
        }
        first = false;
        let rec = store.as_ref().map(|_| RecInfo {
            rows,
            last_ts: last_store_ts,
            interval_s: interval,
            fast,
            retain_days: cfg.retain_days,
        });
        if let Ok(j) = serde_json::to_vec(&snap) {
            if let Err(e) = write_atomic(&cfg.dir.join(paths::CURRENT_FILE), &j) {
                eprintln!("writing current.json failed: {e}");
            }
        }
        let flat = serde_json::to_vec(&panel::flat_json(&snap, rec.as_ref())).unwrap_or_default();
        for p in &panel_paths {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            if let Err(e) = write_atomic(p, &flat) {
                eprintln!("writing {} failed: {e}", p.display());
            }
        }
        if let Ok(mut c) = current.write() {
            *c = Some(snap);
        }
        if let Ok(mut r) = rec_state.write() {
            *r = rec;
        }
        std::thread::sleep(cfg.tick.saturating_sub(started.elapsed()).max(Duration::from_millis(100)));
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

/// Run until SIGINT/SIGTERM, or fail if the collector thread dies (including by panic) so the
/// supervisor (systemd `Restart=`) can restart the process instead of it serving stale data.
pub fn run(cfg: DaemonConfig) -> Result<()> {
    if !(cfg.tick.as_secs_f64() >= 1.0 && cfg.interval.is_finite() && cfg.interval >= cfg.tick.as_secs_f64()) {
        bail!("need 1 <= tick <= interval (seconds)");
    }
    create_private_dir(&cfg.dir).with_context(|| format!("creating {}", cfg.dir.display()))?;
    let _lock = lock_dir(&cfg.dir)?;
    let current = Arc::new(RwLock::new(None));
    let rec = Arc::new(RwLock::new(None));
    let host = collect::hostname(&cfg.collector.root);
    let mut state = AppState::new(cfg.db.clone(), host, current.clone(), rec.clone());
    state.writable = cfg.api_writable;
    state.allow_hosts = cfg.allow_hosts.clone();
    state.stale_after_s = (cfg.tick.as_secs_f64() * 3.0).max(10.0);
    let listen = cfg.listen;
    let c2 = cfg.clone();
    let handle = std::thread::Builder::new().name("collector".into()).spawn(move || {
        if let Err(e) = collect_loop(c2, current, rec) {
            eprintln!("collector stopped: {e:#}");
        }
    })?;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.block_on(async move {
        let failed = async {
            loop {
                if handle.is_finished() {
                    return anyhow::anyhow!("the collector thread stopped (panic or error); exiting so it can be restarted");
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        };
        match listen {
            Some(addr) => {
                let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
                eprintln!("benchit-telemetry API listening on http://{addr}");
                tokio::select! {
                    r = axum::serve(listener, api::router(state)).with_graceful_shutdown(shutdown_signal()) => r.map_err(Into::into),
                    e = failed => Err(e),
                }
            }
            None => tokio::select! {
                _ = shutdown_signal() => Ok(()),
                e = failed => Err(e),
            },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use benchit_telemetry_core::model::Gpu;

    #[test]
    fn write_atomic_is_private_and_leaves_no_temp() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.json");
        write_atomic(&p, b"{}").unwrap();
        write_atomic(&d.path().join("a.txt"), b"x").unwrap(); // used to share "a.tmp" with a.json
        assert_eq!(std::fs::read(&p).unwrap(), b"{}");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 2);
    }

    #[test]
    fn data_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a/b");
        create_private_dir(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn second_daemon_in_the_same_dir_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let first = lock_dir(d.path()).unwrap();
        let e = lock_dir(d.path()).unwrap_err();
        assert!(e.to_string().contains("already using"), "{e}");
        drop(first);
        assert!(lock_dir(d.path()).is_ok(), "the lock is released when the first daemon exits");
    }

    #[test]
    fn gpu_power_is_averaged_over_the_interval() {
        let mk = |p: f64| Snapshot {
            gpus: vec![Gpu { idx: 0, power_w: Some(p), util_pct: Some(p), ..Default::default() }],
            ..Default::default()
        };
        let mut acc = GpuAcc::default();
        for p in [10.0, 20.0, 60.0] {
            acc.add(&mk(p));
        }
        let mut stored = mk(60.0);
        acc.apply_and_reset(&mut stored);
        assert_eq!(stored.gpus[0].power_w, Some(30.0));
        assert_eq!(stored.gpus[0].util_pct, Some(30.0));
        assert_eq!(stored.gpu_total_w, Some(30.0));
        // reset: the next interval starts empty
        acc.add(&mk(5.0));
        let mut next = mk(5.0);
        acc.apply_and_reset(&mut next);
        assert_eq!(next.gpus[0].power_w, Some(5.0));
    }

    fn cfg(dir: &Path) -> DaemonConfig {
        DaemonConfig {
            dir: dir.to_path_buf(),
            db: dir.join("metrics.db"),
            tick: Duration::from_secs(1),
            interval: 2.0,
            retain_days: 30,
            panel_json: vec![],
            store: true,
            listen: None,
            api_writable: true,
            allow_hosts: vec![],
            collector: CollectorConfig { nvidia: false, top_process: false, ..Default::default() },
        }
    }

    #[test]
    fn bad_tick_settings_are_rejected_without_panicking() {
        let d = tempfile::tempdir().unwrap();
        for (tick, interval) in [(0.5, 10.0), (f64::NAN, 10.0), (2.0, 1.0), (2.0, f64::NAN), (2.0, f64::INFINITY)] {
            let mut c = cfg(d.path());
            c.tick = if tick.is_finite() { Duration::from_secs_f64(tick) } else { Duration::from_secs(0) };
            c.interval = interval;
            assert!(run(c).is_err(), "tick {tick} interval {interval}");
        }
    }

    #[test]
    fn collector_thread_death_stops_the_daemon() {
        // A collector that panics must make `run` return an error instead of serving stale data.
        let d = tempfile::tempdir().unwrap();
        let mut c = cfg(d.path());
        c.db = d.path().join("no-such-dir/\0/metrics.db"); // Store::open fails inside the thread
        let t = Instant::now();
        let e = run(c).unwrap_err();
        assert!(e.to_string().contains("collector thread stopped"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn daemon_stores_averaged_rows_end_to_end() {
        let d = tempfile::tempdir().unwrap();
        let c = cfg(d.path());
        let db = c.db.clone();
        let dir = d.path().to_path_buf();
        // run the real loop on a thread; it never returns, so it is simply abandoned at the end
        std::thread::spawn(move || {
            let _ = run(c);
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(s) = Store::open_readonly(&db) {
                if s.count().unwrap_or(0) >= 2 {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "no rows stored");
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(dir.join(paths::CURRENT_FILE).exists());
        assert!(dir.join(paths::PANEL_FILE).exists());
    }
}
