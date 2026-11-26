//! `benchit-telemetry` command-line interface.
use anyhow::{bail, Context, Result};
use benchit_telemetry::daemon::{self, DaemonConfig};
use benchit_telemetry::{lease, paths};
use benchit_telemetry_core::collect;
use benchit_telemetry_core::panel;
use benchit_telemetry_core::store::{self, Store, Summary};
use benchit_telemetry_core::{timefmt, Collector, CollectorConfig, Snapshot};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "benchit-telemetry",
    version,
    about = "Workstation telemetry logger, history store, HTTP API and panel feed"
)]
struct Cli {
    /// Data directory (database, panel feeds). Default: $BENCHIT_TELEMETRY_DIR, else an existing legacy
    /// ~/telemetry (if it holds metrics.db), else ~/.local/state/benchit-telemetry
    #[arg(long, global = true, env = "BENCHIT_TELEMETRY_DIR")]
    dir: Option<PathBuf>,
    /// Database path (default: <dir>/metrics.db)
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the logger: collect every tick, write panel feeds, store samples, prune old rows,
    /// and optionally serve the HTTP API.
    Daemon(DaemonArgs),
    /// Serve the HTTP API with live collection but without storing samples.
    Serve {
        #[arg(long, default_value = "127.0.0.1:9184")]
        listen: SocketAddr,
        /// Reject POST endpoints.
        #[arg(long)]
        read_only: bool,
        /// Also accept this `Host` header name (repeatable); localhost names are always accepted.
        #[arg(long)]
        allow_host: Vec<String>,
    },
    /// Print the current snapshot as JSON (from the daemon feed if fresh, else a live sample).
    Current {
        #[arg(long)]
        pretty: bool,
    },
    /// Print panel output for conky, scripts or terminals.
    Panel {
        #[arg(long, value_enum, default_value = "text")]
        format: PanelFormat,
        /// Maximum line width for text output.
        #[arg(long, default_value_t = 44)]
        width: usize,
    },
    /// Database status.
    Status,
    /// Print stored samples (TSV, or JSON with --json).
    History {
        #[command(flatten)]
        w: Window,
        /// Comma-separated fields (see `fields`).
        #[arg(long)]
        fields: Option<String>,
        /// Average into buckets of N seconds.
        #[arg(long, default_value_t = 0)]
        step: i64,
        #[arg(long, default_value_t = 100_000)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Print stored per-GPU samples.
    Gpus {
        #[command(flatten)]
        w: Window,
        #[arg(long)]
        idx: Option<u32>,
        #[arg(long, default_value_t = 0)]
        step: i64,
        #[arg(long, default_value_t = 100_000)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// min/avg/p95/max per metric and estimated energy for a window or mark.
    Summary {
        #[command(flatten)]
        w: Window,
        #[arg(long)]
        json: bool,
    },
    /// Export the samples_wide view as CSV.
    Export {
        #[command(flatten)]
        w: Window,
        /// Output file (default stdout).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Start or end a named benchmark mark.
    Mark {
        #[arg(value_enum)]
        action: MarkAction,
        name: String,
    },
    /// List recent marks.
    Marks {
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Run a command inside a mark with fast (per-tick) sampling, then print its summary.
    Run {
        name: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// List queryable fields.
    Fields,
}

#[derive(Args)]
struct DaemonArgs {
    /// Collection and panel refresh period (seconds).
    #[arg(long, default_value_t = 2.0)]
    tick: f64,
    /// Storage period (seconds).
    #[arg(long, default_value_t = 10.0)]
    interval: f64,
    /// Days of history to keep.
    #[arg(long, default_value_t = 30)]
    retain_days: u32,
    /// Additional flat panel JSON feed path (repeatable).
    #[arg(long)]
    panel_json: Vec<PathBuf>,
    /// Serve the HTTP API on this address (e.g. 127.0.0.1:9184).
    #[arg(long)]
    listen: Option<SocketAddr>,
    /// Reject POST endpoints on the API.
    #[arg(long)]
    read_only: bool,
    /// Also accept this `Host` header name on the API (repeatable); localhost names are always accepted.
    #[arg(long)]
    allow_host: Vec<String>,
    /// Do not store samples (panel/API only).
    #[arg(long)]
    no_store: bool,
    /// Do not query NVIDIA GPUs (NVML / nvidia-smi).
    #[arg(long)]
    no_gpu: bool,
    /// Do not scan /proc for the largest process (it is also served over the API).
    #[arg(long)]
    no_top_process: bool,
    /// CPU temperature warning threshold (°C).
    #[arg(long, default_value_t = 95.0)]
    cpu_temp_warn: f64,
}

#[derive(Args, Clone, Default)]
struct Window {
    /// Window length back from now, e.g. 15m, 2h, 7d (default 1h, like the API).
    #[arg(long, conflicts_with_all = ["from", "mark"])]
    since: Option<String>,
    /// Start: unix seconds (>= 1e8), -15m, or local 'YYYY-MM-DD HH:MM[:SS]' (a Z or +HH:MM suffix makes it absolute).
    #[arg(long, conflicts_with = "mark")]
    from: Option<String>,
    /// End (default now).
    #[arg(long, conflicts_with = "mark")]
    to: Option<String>,
    /// Use the newest mark with this name as the window.
    #[arg(long)]
    mark: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum PanelFormat {
    Text,
    Json,
    Flat,
}

#[derive(Clone, Copy, ValueEnum)]
enum MarkAction {
    Start,
    End,
}

struct Ctx {
    dir: PathBuf,
    db: PathBuf,
}

impl Ctx {
    fn ro(&self) -> Result<Store> {
        if !self.db.exists() {
            bail!("no database at {} (start `benchit-telemetry daemon` first)", self.db.display());
        }
        Store::open_readonly(&self.db)
    }
    fn rw(&self) -> Result<Store> {
        Store::open(&self.db)
    }
}

fn resolve(s: &Store, w: &Window) -> Result<(i64, i64)> {
    let now = timefmt::now();
    if let Some(name) = &w.mark {
        let m = s.mark_by_name(name)?.with_context(|| format!("no mark named {name:?}"))?;
        return Ok((m.t_start.floor() as i64, m.t_end.unwrap_or(now as f64).ceil() as i64));
    }
    let from = match (&w.from, &w.since) {
        (Some(f), _) => s.resolve_time(f, now)?,
        (None, Some(d)) => {
            let secs = timefmt::parse_duration(d).filter(|v| v.is_finite() && *v <= 3.2e9);
            now - secs.with_context(|| format!("bad duration {d:?} (examples: 90s, 15m, 2h, 7d)"))? as i64
        }
        (None, None) => now - 3600,
    };
    let to = w.to.as_deref().map(|t| s.resolve_time(t, now)).transpose()?.unwrap_or(now);
    Ok((from, to))
}

fn live_snapshot(ctx: &Ctx) -> Snapshot {
    if let Ok(txt) = std::fs::read_to_string(ctx.dir.join(paths::CURRENT_FILE)) {
        if let Ok(s) = serde_json::from_str::<Snapshot>(&txt) {
            if timefmt::now() - s.ts <= 10 {
                return s;
            }
        }
    }
    Collector::new(CollectorConfig { top_process: true, ..Default::default() })
        .sample_with_rates(Duration::from_millis(500))
}

fn print_summary(s: &Store, sm: &Summary, title: Option<&str>) {
    if let Some(t) = title {
        println!("=== {t} ===");
    }
    println!(
        "window {} -> {}  ({} s, {} samples)",
        s.local_time(sm.from as f64),
        s.local_time(sm.to as f64),
        sm.to - sm.from,
        sm.samples
    );
    if sm.samples == 0 {
        return;
    }
    println!("{:<22}{:>10}{:>10}{:>10}{:>10}", "metric", "min", "avg", "p95", "max");
    for m in &sm.metrics {
        println!("{:<22}{:>10.2}{:>10.2}{:>10.2}{:>10.2}", m.field, m.min, m.avg, m.p95, m.max);
    }
    for (idx, st) in &sm.gpus {
        for m in st {
            println!(
                "{:<22}{:>10.2}{:>10.2}{:>10.2}{:>10.2}",
                format!("gpu{idx}_{}", m.field),
                m.min,
                m.avg,
                m.p95,
                m.max
            );
        }
    }
    if let Some(wh) = sm.energy_est_wh {
        println!(
            "{:<22}{:>40.3}   (estimate: {} only; {} s covered)",
            "energy_est_wh",
            wh,
            sm.energy_sources.join("+"),
            sm.coverage_s
        );
    }
    if let Some(wh) = sm.platform_energy_est_wh {
        println!("{:<22}{:>40.3}   (estimate: platform_w)", "platform_energy_est_wh", wh);
    }
}

fn print_rows(rows: &[serde_json::Map<String, serde_json::Value>], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(rows)?);
        return Ok(());
    }
    if let Some(first) = rows.first() {
        println!("{}", first.keys().cloned().collect::<Vec<_>>().join("\t"));
    }
    for r in rows {
        let cells: Vec<String> = r
            .values()
            .map(|v| match v {
                serde_json::Value::Null => String::new(),
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
        println!("{}", cells.join("\t"));
    }
    Ok(())
}

static GOT_SIGNAL: AtomicI32 = AtomicI32::new(0);
static CHILD_PID: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    GOT_SIGNAL.store(sig, Ordering::SeqCst);
    let child = CHILD_PID.load(Ordering::SeqCst);
    // SAFETY: only async-signal-safe operations (atomics and kill(2)) happen here, and `info`
    // is the valid pointer the kernel passes to an SA_SIGINFO handler.
    unsafe {
        // A terminal's Ctrl-C reaches the whole foreground process group, child included, so
        // forward only signals sent to us alone by kill(2) (si_code <= 0), not tty-generated ones.
        if child > 0 && !info.is_null() && (*info).si_code <= 0 {
            libc::kill(child, sig);
        }
    }
}

/// Make SIGINT/SIGTERM/SIGHUP record themselves instead of killing `run`, so it can always close
/// its mark and remove its fast-mode lease. Child processes get default dispositions back on exec.
fn install_signal_handlers() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: a zeroed sigaction is a valid starting point; the handler is async-signal-safe.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// Sleep up to `d`, returning early if a signal arrived.
fn interruptible_sleep(d: Duration) {
    let end = std::time::Instant::now() + d;
    while std::time::Instant::now() < end && GOT_SIGNAL.load(Ordering::SeqCst) == 0 {
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Quote `args` so the stored command line can be pasted back into a shell.
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            let plain = !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
            if plain {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGINT => "SIGINT".into(),
        libc::SIGTERM => "SIGTERM".into(),
        libc::SIGHUP => "SIGHUP".into(),
        libc::SIGKILL => "SIGKILL".into(),
        libc::SIGSEGV => "SIGSEGV".into(),
        libc::SIGABRT => "SIGABRT".into(),
        other => format!("signal {other}"),
    }
}

fn daemon_is_feeding(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(paths::CURRENT_FILE))
        .ok()
        .and_then(|t| serde_json::from_str::<Snapshot>(&t).ok())
        .is_some_and(|s| timefmt::now() - s.ts <= 10)
}

/// Removes this run's fast-mode lease (and stops its refresher) however `run` ends.
struct LeaseGuard {
    dir: PathBuf,
    pid: u32,
    stop: Arc<AtomicBool>,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        lease::remove(&self.dir, self.pid);
    }
}

/// Run `command` inside a mark with per-tick sampling. SIGINT/SIGTERM/SIGHUP are forwarded to the
/// child (when sent to `run` alone) and never kill `run` itself, so the mark is always closed and
/// the lease always removed. Returns the exit code to use: the child's, or 128+signal, or 127/126
/// when the command could not be started.
fn run_command(ctx: &Ctx, name: &str, command: &[String]) -> Result<i32> {
    install_signal_handlers();
    let s = ctx.rw()?;
    let host = collect::hostname(Path::new("/"));
    let pid = std::process::id();
    let daemon_up = daemon_is_feeding(&ctx.dir);
    let _lease = if daemon_up {
        let stop = Arc::new(AtomicBool::new(false));
        lease::write(&ctx.dir, pid, timefmt::now_f64() + lease::LEASE_SECS)?;
        let (d, st2) = (ctx.dir.clone(), stop.clone());
        std::thread::spawn(move || {
            // refresh every 5 s (in short sleeps so the guard can stop us promptly)
            'outer: loop {
                for _ in 0..50 {
                    if st2.load(Ordering::SeqCst) {
                        break 'outer;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = lease::write(&d, pid, timefmt::now_f64() + lease::LEASE_SECS);
            }
        });
        Some(LeaseGuard { dir: ctx.dir.clone(), pid, stop })
    } else {
        eprintln!(
            "benchit-telemetry: warning: no daemon is feeding {}; no samples will be recorded during this run \
             (start `benchit-telemetry daemon`)",
            ctx.dir.display()
        );
        None
    };
    if daemon_up {
        interruptible_sleep(Duration::from_millis(2500)); // let the daemon switch to per-tick storage
    }
    let t0 = timefmt::now_f64();
    let id = s.mark_start(&host, name, t0, Some(&shell_join(command)))?;
    let mut note: Option<String> = None;
    let code = if GOT_SIGNAL.load(Ordering::SeqCst) != 0 {
        note = Some("interrupted before the command started".into());
        128 + GOT_SIGNAL.load(Ordering::SeqCst)
    } else {
        match std::process::Command::new(&command[0]).args(&command[1..]).spawn() {
            Err(e) => {
                eprintln!("benchit-telemetry: failed to run {:?}: {e}", command[0]);
                note = Some(format!("failed to start: {e}"));
                if e.kind() == std::io::ErrorKind::NotFound {
                    127
                } else {
                    126
                }
            }
            Ok(mut child) => {
                CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
                let early = GOT_SIGNAL.load(Ordering::SeqCst);
                if early != 0 {
                    // SAFETY: plain kill(2) to the child we just spawned.
                    unsafe { libc::kill(child.id() as i32, early) };
                }
                let status = loop {
                    match child.try_wait() {
                        Ok(Some(st)) => break Some(st),
                        Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                        Err(_) => break None,
                    }
                };
                CHILD_PID.store(0, Ordering::SeqCst);
                let got = GOT_SIGNAL.load(Ordering::SeqCst);
                use std::os::unix::process::ExitStatusExt;
                match status {
                    Some(st) => match (st.code(), st.signal()) {
                        (Some(c), _) => {
                            if got != 0 {
                                note = Some(format!("run received {}", signal_name(got)));
                            }
                            c
                        }
                        (None, Some(sig)) => {
                            note = Some(format!("terminated by {}", signal_name(sig)));
                            128 + sig
                        }
                        _ => 1,
                    },
                    None => {
                        note = Some("lost track of the child process".into());
                        1
                    }
                }
            }
        }
    };
    let t1 = timefmt::now_f64();
    if daemon_up && GOT_SIGNAL.load(Ordering::SeqCst) == 0 {
        interruptible_sleep(Duration::from_millis(2500)); // capture the final tick
    }
    // Always close the mark, whatever happened.
    if let Err(e) = s.mark_end_id(id, t1, Some(code), note.as_deref()) {
        eprintln!("benchit-telemetry: could not close mark {id}: {e:#}");
    }
    drop(_lease);
    match s.summary(t0.floor() as i64, t1.ceil() as i64) {
        Ok(sm) => print_summary(&s, &sm, Some(&format!("{name} (exit {code})"))),
        Err(e) => eprintln!("benchit-telemetry: could not summarise: {e:#}"),
    }
    Ok(code)
}

fn main() -> Result<()> {
    // Behave like a normal Unix filter when piped into `head` etc.
    // SAFETY: called before any other threads exist; restores the default SIGPIPE action.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let dir = cli.dir.clone().unwrap_or_else(paths::default_dir);
    let db = cli.db.clone().unwrap_or_else(|| dir.join(paths::DB_FILE));
    let ctx = Ctx { dir, db };
    match cli.cmd {
        Cmd::Daemon(a) => {
            if !(a.tick.is_finite() && a.tick >= 1.0 && a.interval.is_finite() && a.interval >= a.tick) {
                bail!("need 1 <= --tick <= --interval (seconds; a stored row has one-second resolution)");
            }
            daemon::run(DaemonConfig {
                dir: ctx.dir.clone(),
                db: ctx.db.clone(),
                tick: Duration::from_secs_f64(a.tick),
                interval: a.interval,
                retain_days: a.retain_days.max(1),
                panel_json: a.panel_json,
                store: !a.no_store,
                listen: a.listen,
                api_writable: !a.read_only,
                allow_hosts: a.allow_host,
                collector: CollectorConfig {
                    nvidia: !a.no_gpu,
                    top_process: !a.no_top_process,
                    cpu_temp_warn_c: a.cpu_temp_warn,
                    ..Default::default()
                },
            })
        }
        Cmd::Serve { listen, read_only, allow_host } => {
            // scratch directory for the feeds `serve` does not need; removed when serving ends
            let scratch = tempfile::Builder::new().prefix("benchit-telemetry-serve-").tempdir()?;
            daemon::run(DaemonConfig {
                dir: scratch.path().to_path_buf(),
                db: ctx.db.clone(),
                tick: Duration::from_secs(2),
                interval: 10.0,
                retain_days: 30,
                panel_json: vec![],
                store: false,
                listen: Some(listen),
                api_writable: !read_only,
                allow_hosts: allow_host,
                collector: CollectorConfig::default(),
            })
        }
        Cmd::Current { pretty } => {
            let s = live_snapshot(&ctx);
            println!("{}", if pretty { serde_json::to_string_pretty(&s)? } else { serde_json::to_string(&s)? });
            Ok(())
        }
        Cmd::Panel { format, width } => {
            let s = live_snapshot(&ctx);
            match format {
                PanelFormat::Text => println!("{}", panel::text(&s, width)),
                PanelFormat::Json => println!("{}", serde_json::to_string(&s)?),
                PanelFormat::Flat => println!("{}", panel::flat_json(&s, None)),
            }
            Ok(())
        }
        Cmd::Status => {
            let s = ctx.ro()?;
            let st = s.status()?;
            println!("db {}  ({:.1} MB)  schema v{}", st.path, st.size_bytes as f64 / 1e6, st.schema_version);
            println!(
                "samples {}  gpu_samples {}  interval {} s  retention {} d",
                st.samples,
                st.gpu_samples,
                s.meta("sample_interval_s")?.unwrap_or_else(|| "?".into()),
                s.meta("retain_days")?.unwrap_or_else(|| "?".into())
            );
            if let (Some(a), Some(b)) = (st.first_ts, st.last_ts) {
                println!(
                    "range {} -> {}  (last sample {} s ago)",
                    s.local_time(a as f64),
                    s.local_time(b as f64),
                    timefmt::now() - b
                );
            }
            Ok(())
        }
        Cmd::History { w, fields, step, limit, json } => {
            let s = ctx.ro()?;
            let (from, to) = resolve(&s, &w)?;
            let allowed = [store::SAMPLE_FIELDS, store::SAMPLE_TEXT_FIELDS].concat();
            let f = fields.as_deref().map(|f| store::validate_fields(f, &allowed)).transpose()?.unwrap_or_default();
            print_rows(&s.history(from, to, &f, step.max(0), limit)?, json)
        }
        Cmd::Gpus { w, idx, step, limit, json } => {
            let s = ctx.ro()?;
            let (from, to) = resolve(&s, &w)?;
            print_rows(&s.gpu_history(from, to, idx, step.max(0), limit)?, json)
        }
        Cmd::Summary { w, json } => {
            let s = ctx.ro()?;
            let (from, to) = resolve(&s, &w)?;
            let sm = s.summary(from, to)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sm)?);
            } else {
                print_summary(&s, &sm, w.mark.as_deref());
            }
            Ok(())
        }
        Cmd::Export { w, out } => {
            let s = ctx.ro()?;
            let (from, to) = resolve(&s, &w)?;
            let n = match &out {
                Some(p) => {
                    let mut f = std::io::BufWriter::new(std::fs::File::create(p)?);
                    s.export_csv(from, to, &mut f)?
                }
                None => s.export_csv(from, to, &mut std::io::stdout().lock())?,
            };
            if let Some(p) = out {
                eprintln!("wrote {n} rows to {}", p.display());
            }
            Ok(())
        }
        Cmd::Mark { action, name } => {
            let s = ctx.rw()?;
            let host = collect::hostname(Path::new("/"));
            let t = timefmt::now_f64();
            match action {
                MarkAction::Start => {
                    let id = s.mark_start(&host, &name, t, None)?;
                    println!("mark {id} {name:?} started at {}", s.local_time(t));
                }
                MarkAction::End => match s.mark_end(Some(&host), &name, t, None)? {
                    Some(id) => println!("mark {id} {name:?} ended at {}", s.local_time(t)),
                    None => bail!("no open mark named {name:?}"),
                },
            }
            Ok(())
        }
        Cmd::Marks { limit } => {
            let s = ctx.ro()?;
            for m in s.marks(limit)? {
                println!(
                    "{:>5}  {:<24} {} -> {}  exit={}  {}",
                    m.id,
                    m.name,
                    s.local_time(m.t_start),
                    m.t_end.map(|t| s.local_time(t)).unwrap_or_else(|| "open".into()),
                    m.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
                    m.cmd.unwrap_or_default()
                );
            }
            Ok(())
        }
        Cmd::Run { name, command } => {
            let code = run_command(&ctx, &name, &command)?;
            std::process::exit(code);
        }
        Cmd::Fields => {
            println!("samples (numeric): {}", store::SAMPLE_FIELDS.join(","));
            println!("samples (text):    {}", store::SAMPLE_TEXT_FIELDS.join(","));
            println!("gpus:              {}", store::GPU_FIELDS.join(","));
            Ok(())
        }
    }
}
