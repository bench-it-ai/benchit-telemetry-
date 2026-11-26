//! `benchit-telemetry` command-line interface.
use anyhow::{bail, Context, Result};
use benchit_telemetry::daemon::{self, DaemonConfig};
use benchit_telemetry::paths;
use benchit_telemetry_core::panel;
use benchit_telemetry_core::store::{self, Store, Summary};
use benchit_telemetry_core::{timefmt, Collector, CollectorConfig, Snapshot};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "benchit-telemetry",
    version,
    about = "Workstation telemetry logger, history store, HTTP API and panel feed"
)]
struct Cli {
    /// Data directory (database, panel feeds). Default: $BENCHIT_TELEMETRY_DIR or ~/.local/state/benchit-telemetry
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
    /// Do not store samples (panel/API only).
    #[arg(long)]
    no_store: bool,
    /// Do not query nvidia-smi.
    #[arg(long)]
    no_gpu: bool,
    /// CPU temperature warning threshold (°C).
    #[arg(long, default_value_t = 95.0)]
    cpu_temp_warn: f64,
}

#[derive(Args, Clone, Default)]
struct Window {
    /// Window length back from now, e.g. 15m, 2h, 7d (default 15m).
    #[arg(long)]
    since: Option<String>,
    /// Start: unix seconds, -15m, or local 'YYYY-MM-DD HH:MM'.
    #[arg(long)]
    from: Option<String>,
    /// End (default now).
    #[arg(long)]
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
        (None, Some(d)) => now - timefmt::parse_duration(d).with_context(|| format!("bad duration {d:?}"))? as i64,
        (None, None) => now - 900,
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
    Collector::new(CollectorConfig::default()).sample_with_rates(Duration::from_millis(500))
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
    if let Some(wh) = sm.energy_wh {
        println!("{:<22}{:>40.3}", "energy_est_wh", wh);
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
            if a.tick <= 0.0 || a.interval < a.tick {
                bail!("need 0 < --tick <= --interval");
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
                collector: CollectorConfig {
                    nvidia: !a.no_gpu,
                    cpu_temp_warn_c: a.cpu_temp_warn,
                    ..Default::default()
                },
            })
        }
        Cmd::Serve { listen, read_only } => daemon::run(DaemonConfig {
            dir: std::env::temp_dir().join(format!("benchit-telemetry-serve-{}", std::process::id())),
            db: ctx.db.clone(),
            tick: Duration::from_secs(2),
            interval: 10.0,
            retain_days: 30,
            panel_json: vec![],
            store: false,
            listen: Some(listen),
            api_writable: !read_only,
            collector: CollectorConfig::default(),
        }),
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
            let host = Collector::new(CollectorConfig { nvidia: false, top_process: false, ..Default::default() })
                .host()
                .to_string();
            let t = timefmt::now_f64();
            match action {
                MarkAction::Start => {
                    let id = s.mark_start(&host, &name, t, None)?;
                    println!("mark {id} {name:?} started at {}", s.local_time(t));
                }
                MarkAction::End => match s.mark_end(&name, t, None)? {
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
            let s = ctx.rw()?;
            let fast = ctx.dir.join(paths::FAST_FILE);
            std::fs::create_dir_all(&ctx.dir)?;
            std::fs::write(&fast, (timefmt::now_f64() + 7.0 * 86400.0).to_string())?;
            std::thread::sleep(Duration::from_millis(2500));
            let host = Collector::new(CollectorConfig { nvidia: false, top_process: false, ..Default::default() })
                .host()
                .to_string();
            let t0 = timefmt::now_f64();
            let id = s.mark_start(&host, &name, t0, Some(&command.join(" ")))?;
            let status = std::process::Command::new(&command[0]).args(&command[1..]).status();
            let t1 = timefmt::now_f64();
            std::thread::sleep(Duration::from_millis(2500));
            let _ = std::fs::remove_file(&fast);
            let code = match &status {
                Ok(st) => st.code().unwrap_or(-1),
                Err(_) => 127,
            };
            s.mark_end_id(id, t1, Some(code))?;
            if let Err(e) = status {
                eprintln!("failed to run {:?}: {e}", command[0]);
            }
            let sm = s.summary(t0.floor() as i64, t1.ceil() as i64)?;
            print_summary(&s, &sm, Some(&format!("{name} (exit {code})")));
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
