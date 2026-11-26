//! Logger daemon: collect every tick, write panel feeds, store samples, prune, serve the API.
use crate::api::{self, AppState};
use crate::paths;
use anyhow::{Context, Result};
use benchit_telemetry_core::panel::{self, RecInfo};
use benchit_telemetry_core::{timefmt, Collector, CollectorConfig, Snapshot, Store};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub dir: PathBuf,
    pub db: PathBuf,
    /// Collection / panel refresh period.
    pub tick: Duration,
    /// Storage period in seconds (rounded to whole ticks).
    pub interval: f64,
    pub retain_days: u32,
    /// Extra flat-JSON panel feeds (in addition to `<dir>/latest.json`).
    pub panel_json: Vec<PathBuf>,
    pub store: bool,
    pub listen: Option<SocketAddr>,
    pub api_writable: bool,
    pub collector: CollectorConfig,
}

/// Atomically replace `path` with `data`.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

fn fast_mode(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(paths::FAST_FILE))
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .is_some_and(|t| t > timefmt::now_f64())
}

fn collect_loop(
    cfg: DaemonConfig,
    current: Arc<RwLock<Option<Snapshot>>>,
    rec_state: Arc<RwLock<Option<RecInfo>>>,
) -> Result<()> {
    std::fs::create_dir_all(&cfg.dir).with_context(|| format!("creating {}", cfg.dir.display()))?;
    let mut store = if cfg.store { Some(Store::open(&cfg.db)?) } else { None };
    let mut rows = 0;
    if let Some(s) = &store {
        s.set_meta("retain_days", &cfg.retain_days.to_string())?;
        s.set_meta("sample_interval_s", &cfg.interval.to_string())?;
        rows = s.count()?;
    }
    let mut collector = Collector::new(cfg.collector.clone());
    let mut last_store = 0.0f64;
    let mut last_prune = 0.0f64;
    let mut first = true;
    let panel_paths: Vec<PathBuf> =
        std::iter::once(cfg.dir.join(paths::PANEL_FILE)).chain(cfg.panel_json.iter().cloned()).collect();
    loop {
        let started = Instant::now();
        let snap = collector.sample();
        let now = timefmt::now_f64();
        let fast = fast_mode(&cfg.dir);
        let interval = if fast { cfg.tick.as_secs_f64() } else { cfg.interval };
        if let Some(s) = store.as_mut() {
            // the very first sample has no rate values, so it is not stored
            if !first && now - last_store >= interval - 0.05 {
                match s.insert(&snap) {
                    Ok(()) => {
                        rows += 1;
                        last_store = now;
                    }
                    Err(e) => eprintln!("store insert failed: {e:#}"),
                }
            }
            if now - last_prune >= 3600.0 {
                match s.prune(cfg.retain_days, now as i64) {
                    Ok(_) => rows = s.count().unwrap_or(rows),
                    Err(e) => eprintln!("prune failed: {e:#}"),
                }
                last_prune = now;
            }
        }
        first = false;
        let rec = store.as_ref().map(|_| RecInfo {
            rows,
            last_ts: last_store as i64,
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

/// Run until SIGINT/SIGTERM.
pub fn run(cfg: DaemonConfig) -> Result<()> {
    let current = Arc::new(RwLock::new(None));
    let rec = Arc::new(RwLock::new(None));
    let host = Collector::new(CollectorConfig { nvidia: false, top_process: false, ..cfg.collector.clone() })
        .host()
        .to_string();
    let state =
        AppState { db: cfg.db.clone(), host, current: current.clone(), rec: rec.clone(), writable: cfg.api_writable };
    let listen = cfg.listen;
    let (tx, rx) = std::sync::mpsc::channel::<anyhow::Error>();
    let c2 = cfg.clone();
    std::thread::Builder::new().name("collector".into()).spawn(move || {
        if let Err(e) = collect_loop(c2, current, rec) {
            let _ = tx.send(e);
        }
    })?;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.block_on(async move {
        let failed = async {
            loop {
                if let Ok(e) = rx.try_recv() {
                    return e;
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
