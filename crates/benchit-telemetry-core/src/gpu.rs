//! NVIDIA GPU sampling.
//!
//! * Primary backend: NVML through `nvml-wrapper` (cargo feature `nvml`, on by default). The
//!   library (`libnvidia-ml.so.1`) is loaded at run time, so a machine without NVIDIA simply
//!   falls through to the next backend.
//! * Fallback: `nvidia-smi --query-gpu=...`, with an exit-status check, a timeout, and a field
//!   ladder that copes with drivers older than R535 (`clocks_event_reasons` was
//!   `clocks_throttle_reasons`).
//! * Hybrid laptops: a dGPU whose PCI `power/runtime_status` is `suspended` is **never**
//!   queried (a query would wake it, costing power and distorting power benchmarks). It is
//!   reported as `pstate = "D3"` with no readings.
//! * Collection runs on its own thread ([`GpuPoller`]) so a slow or hung driver can never stall
//!   the main sampling loop; stale data is dropped rather than served.
use crate::model::Gpu;
use crate::parse;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// GPU collection settings.
#[derive(Debug, Clone)]
pub struct GpuConfig {
    /// Filesystem root used to find `sys/bus/pci/devices` (tests point this at fixtures).
    pub root: PathBuf,
    /// Timeout for one `nvidia-smi` call.
    pub timeout: Duration,
    /// Time between polls.
    pub poll: Duration,
    /// Try NVML first (needs the `nvml` cargo feature to have any effect).
    pub use_nvml: bool,
    /// Program used for the fallback backend.
    pub smi_program: String,
}

impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("/"),
            timeout: Duration::from_secs(5),
            poll: Duration::from_secs(2),
            use_nvml: true,
            smi_program: "nvidia-smi".into(),
        }
    }
}

/// An NVIDIA display/3D controller found in sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PciGpu {
    pub bdf: String,
    pub suspended: bool,
}

/// NVIDIA PCI display controllers (vendor 0x10de, class 0x03xxxx), sorted by address, with
/// their runtime power-management state.
pub fn nvidia_pci(root: &Path) -> Vec<PciGpu> {
    let dir = root.join("sys/bus/pci/devices");
    let mut v: Vec<PciGpu> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let p = e.path();
                    let rd = |f: &str| std::fs::read_to_string(p.join(f)).ok().map(|s| s.trim().to_string());
                    (rd("vendor")?.eq_ignore_ascii_case("0x10de") && rd("class")?.starts_with("0x03")).then(|| PciGpu {
                        bdf: e.file_name().to_string_lossy().to_string(),
                        suspended: rd("power/runtime_status").as_deref() == Some("suspended"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| a.bdf.cmp(&b.bdf));
    v
}

#[derive(Debug)]
enum GpuError {
    /// No backend can ever work here (no NVML, no nvidia-smi).
    Unavailable,
    Failed(String),
}

/// Seconds to wait before the next attempt after `fails` consecutive failures (2 s doubling to 60 s).
pub fn backoff_secs(fails: u32) -> u64 {
    (2u64 << fails.saturating_sub(1).min(5)).min(60)
}

/// Synchronous GPU reader; wrap it in a [`GpuPoller`] for use from the collection loop.
pub struct GpuSource {
    cfg: GpuConfig,
    #[cfg(feature = "nvml")]
    nvml: Option<nvml_wrapper::Nvml>,
    #[cfg(feature = "nvml")]
    nvml_failed_at: Option<Instant>,
    smi_level: usize,
    names: HashMap<u32, String>,
    fails: u32,
    retry_at: Option<Instant>,
    disabled: bool,
}

impl GpuSource {
    pub fn new(cfg: GpuConfig) -> Self {
        Self {
            cfg,
            #[cfg(feature = "nvml")]
            nvml: None,
            #[cfg(feature = "nvml")]
            nvml_failed_at: None,
            smi_level: 0,
            names: HashMap::new(),
            fails: 0,
            retry_at: None,
            disabled: false,
        }
    }

    /// True once no backend can work on this machine.
    pub fn disabled(&self) -> bool {
        self.disabled
    }

    fn placeholder(&mut self, idx: u32) -> Gpu {
        Gpu {
            idx,
            name: self.names.get(&idx).cloned().unwrap_or_else(|| "NVIDIA GPU (runtime-suspended)".into()),
            pstate: Some("D3".into()),
            ..Default::default()
        }
    }

    /// One poll. Never blocks longer than the configured timeout; empty when nothing could be read.
    pub fn poll(&mut self) -> Vec<Gpu> {
        if self.disabled {
            return Vec::new();
        }
        if let Some(t) = self.retry_at {
            if Instant::now() < t {
                return Vec::new();
            }
        }
        let pci = nvidia_pci(&self.cfg.root);
        if !pci.is_empty() && pci.iter().all(|p| p.suspended) {
            // Everything is asleep: report that, touch nothing.
            self.fails = 0;
            return (0..pci.len() as u32).map(|i| self.placeholder(i)).collect();
        }
        match self.query(&pci) {
            Ok(mut gpus) => {
                self.fails = 0;
                self.retry_at = None;
                for g in &gpus {
                    self.names.insert(g.idx, g.name.clone());
                }
                // GPUs we skipped because they are asleep
                if pci.iter().any(|p| p.suspended) {
                    for (i, p) in pci.iter().enumerate() {
                        if p.suspended && !gpus.iter().any(|g| g.idx == i as u32) {
                            gpus.push(self.placeholder(i as u32));
                        }
                    }
                    gpus.sort_by_key(|g| g.idx);
                }
                gpus
            }
            Err(GpuError::Unavailable) => {
                self.disabled = true;
                Vec::new()
            }
            Err(GpuError::Failed(msg)) => {
                self.fails += 1;
                #[allow(clippy::manual_is_multiple_of)] // u32::is_multiple_of needs Rust 1.90; MSRV is 1.89
                let log_it = self.fails == 1 || self.fails % 30 == 0;
                if log_it {
                    eprintln!("benchit-telemetry: GPU query failed ({} in a row): {msg}", self.fails);
                }
                self.retry_at = Some(Instant::now() + Duration::from_secs(backoff_secs(self.fails)));
                Vec::new()
            }
        }
    }

    fn query(&mut self, pci: &[PciGpu]) -> Result<Vec<Gpu>, GpuError> {
        #[cfg(feature = "nvml")]
        if self.cfg.use_nvml {
            match self.query_nvml(pci) {
                Ok(v) => return Ok(v),
                Err(GpuError::Failed(m)) => {
                    // fall through to nvidia-smi, but remember why
                    if self.nvml.is_some() {
                        eprintln!("benchit-telemetry: NVML query failed ({m}); using nvidia-smi");
                    }
                    self.nvml = None;
                    self.nvml_failed_at = Some(Instant::now());
                }
                Err(GpuError::Unavailable) => {}
            }
        }
        self.query_smi(pci)
    }

    #[cfg(feature = "nvml")]
    fn query_nvml(&mut self, pci: &[PciGpu]) -> Result<Vec<Gpu>, GpuError> {
        use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
        if self.nvml.is_none() {
            // Do not reload the library every poll when it is absent or broken.
            if self.nvml_failed_at.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
                return Err(GpuError::Unavailable);
            }
            match nvml_wrapper::Nvml::init() {
                Ok(n) => {
                    self.nvml = Some(n);
                    self.nvml_failed_at = None;
                }
                Err(_) => {
                    // typically libnvidia-ml.so.1 missing: quietly use nvidia-smi
                    self.nvml_failed_at = Some(Instant::now());
                    return Err(GpuError::Unavailable);
                }
            }
        }
        let nvml = self.nvml.as_ref().ok_or(GpuError::Unavailable)?;
        let count = nvml.device_count().map_err(|e| GpuError::Failed(e.to_string()))?;
        let mut out = Vec::new();
        for i in 0..count {
            // Only trust the sysfs ordering when it lines up with NVML's device list.
            if pci.len() == count as usize && pci[i as usize].suspended {
                continue;
            }
            let d = nvml.device_by_index(i).map_err(|e| GpuError::Failed(e.to_string()))?;
            let mw = |v: Result<u32, _>| v.ok().map(|x| (f64::from(x) / 100.0).round() / 10.0); // mW -> W, 0.1 W
            let vram = d.memory_info().ok();
            let pstate = d.performance_state().ok().map(|p| p.as_c()).filter(|p| *p <= 15).map(|p| format!("P{p}"));
            out.push(Gpu {
                idx: i,
                name: d.name().unwrap_or_default(),
                temp_c: d.temperature(TemperatureSensor::Gpu).ok().map(f64::from),
                power_w: mw(d.power_usage()),
                power_limit_w: mw(d.enforced_power_limit()).filter(|v| *v > 0.0),
                util_pct: d.utilization_rates().ok().map(|u| f64::from(u.gpu)),
                vram_used_mb: vram.as_ref().map(|m| (m.used / (1024 * 1024)) as f64),
                vram_total_mb: vram.as_ref().map(|m| (m.total / (1024 * 1024)) as f64),
                sm_mhz: d.clock_info(Clock::SM).ok().map(f64::from),
                mem_mhz: d.clock_info(Clock::Memory).ok().map(f64::from),
                fan_pct: d.fan_speed(0).ok().map(f64::from),
                pstate,
                clk_event_reasons: d.current_throttle_reasons().ok().map(|r| format!("0x{:016X}", r.bits())),
            });
        }
        Ok(out)
    }

    fn query_smi(&mut self, pci: &[PciGpu]) -> Result<Vec<Gpu>, GpuError> {
        // Query only awake devices when some are suspended.
        let ids: Option<String> = pci
            .iter()
            .any(|p| p.suspended)
            .then(|| pci.iter().filter(|p| !p.suspended).map(|p| p.bdf.as_str()).collect::<Vec<_>>().join(","));
        let ladder = parse::NVSMI_FIELD_LADDER;
        let mut last_err = String::new();
        for (level, &(fields, with_reasons)) in ladder.iter().enumerate().skip(self.smi_level) {
            let mut args = vec![format!("--query-gpu={fields}"), "--format=csv,noheader,nounits".to_string()];
            if let Some(ids) = &ids {
                args.push(format!("--id={ids}"));
            }
            match run_with_timeout(&self.cfg.smi_program, &args, self.cfg.timeout) {
                Ok(out) => {
                    self.smi_level = level;
                    return Ok(parse::nvidia_smi_csv_with(&out, with_reasons));
                }
                Err(RunError::NotFound) => return Err(GpuError::Unavailable),
                Err(RunError::Timeout(m)) => return Err(GpuError::Failed(m)),
                Err(RunError::Failed(m)) => last_err = m,
            }
        }
        Err(GpuError::Failed(last_err))
    }
}

#[derive(Debug)]
enum RunError {
    NotFound,
    /// Killed after the timeout (transient; never changes which fields are requested).
    Timeout(String),
    /// Non-zero exit or I/O error.
    Failed(String),
}

/// Run `prog args`, killing it after `timeout`; Ok(stdout) only for exit status 0.
fn run_with_timeout(prog: &str, args: &[String], timeout: Duration) -> Result<String, RunError> {
    use std::os::unix::process::CommandExt;
    let mut attempt = 0;
    let mut child = loop {
        let spawned = Command::new(prog)
            .args(args)
            .process_group(0) // own group, so a timeout can kill helper processes too
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        match spawned {
            Ok(c) => break c,
            // ETXTBSY: the binary is momentarily open for writing by a concurrent fork (an updater, or a test)
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempt < 5 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(RunError::NotFound),
            Err(e) => return Err(RunError::Failed(format!("spawn {prog}: {e}"))),
        }
    };
    // Drain both pipes on threads so a chatty child cannot fill a pipe and deadlock.
    fn drain(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut s = String::new();
            let mut buf = Vec::new();
            let _ = r.by_ref().take(1 << 20).read_to_end(&mut buf);
            s.push_str(&String::from_utf8_lossy(&buf));
            s
        })
    }
    let out_t = child.stdout.take().map(drain);
    let err_t = child.stderr.take().map(drain);
    let start = Instant::now();
    let status: Result<std::process::ExitStatus, RunError> = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                // SAFETY: plain kill(2) on the process group we created above.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break Err(RunError::Timeout(format!("{prog} timed out after {timeout:?}")));
            }
            Err(e) => break Err(RunError::Failed(format!("waiting for {prog}: {e}"))),
        }
    };
    let out = out_t.and_then(|t| t.join().ok()).unwrap_or_default();
    let err = err_t.and_then(|t| t.join().ok()).unwrap_or_default();
    match status {
        Ok(st) if st.success() => Ok(out),
        Ok(st) => {
            let msg = err.lines().chain(out.lines()).next().unwrap_or("").trim().to_string();
            Err(RunError::Failed(format!("{prog} exited with {st}: {msg}")))
        }
        Err(e) => Err(e),
    }
}

struct Shared {
    latest: Mutex<Option<(Instant, Vec<Gpu>)>>,
    cv: Condvar,
}

/// Polls a [`GpuSource`] on a background thread and serves the most recent result.
pub struct GpuPoller {
    shared: Arc<Shared>,
    max_age: Duration,
    _stop: Sender<()>,
}

impl GpuPoller {
    pub fn spawn(cfg: GpuConfig) -> Self {
        let shared = Arc::new(Shared { latest: Mutex::new(None), cv: Condvar::new() });
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let poll = cfg.poll;
        let max_age = poll * 3 + cfg.timeout;
        let sh = shared.clone();
        let spawned = std::thread::Builder::new().name("gpu-poll".into()).spawn(move || {
            let mut src = GpuSource::new(cfg);
            loop {
                let gpus = src.poll();
                if let Ok(mut g) = sh.latest.lock() {
                    *g = Some((Instant::now(), gpus));
                }
                sh.cv.notify_all();
                if src.disabled() {
                    return;
                }
                match stop_rx.recv_timeout(poll) {
                    Err(RecvTimeoutError::Timeout) => {}
                    _ => return,
                }
            }
        });
        if spawned.is_err() {
            if let Ok(mut g) = shared.latest.lock() {
                *g = Some((Instant::now(), Vec::new()));
            }
        }
        Self { shared, max_age, _stop: stop_tx }
    }

    /// Most recent readings; waits up to `wait_first` only if nothing has been polled yet.
    /// Data older than a few poll periods is dropped (empty) instead of being served as current.
    pub fn latest(&self, wait_first: Duration) -> Vec<Gpu> {
        let Ok(mut g) = self.shared.latest.lock() else { return Vec::new() };
        if g.is_none() && !wait_first.is_zero() {
            if let Ok((ng, _)) = self.shared.cv.wait_timeout_while(g, wait_first, |v| v.is_none()) {
                g = ng;
            } else {
                return Vec::new();
            }
        }
        match g.as_ref() {
            // a disabled source stops polling, so its (empty) answer never goes stale
            Some((t, v)) if v.is_empty() || t.elapsed() <= self.max_age => v.clone(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn w(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn pci(root: &Path, bdf: &str, vendor: &str, class: &str, status: &str) {
        w(root, &format!("sys/bus/pci/devices/{bdf}/vendor"), vendor);
        w(root, &format!("sys/bus/pci/devices/{bdf}/class"), class);
        w(root, &format!("sys/bus/pci/devices/{bdf}/power/runtime_status"), status);
    }

    fn script(dir: &Path, name: &str, body: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(2), 4);
        assert_eq!(backoff_secs(5), 32);
        assert_eq!(backoff_secs(6), 60);
        assert_eq!(backoff_secs(500), 60);
    }

    #[test]
    fn finds_only_nvidia_display_devices() {
        let d = tempfile::tempdir().unwrap();
        pci(d.path(), "0000:01:00.0", "0x10de\n", "0x030000\n", "active\n");
        pci(d.path(), "0000:02:00.0", "0x10DE\n", "0x030200\n", "suspended\n");
        pci(d.path(), "0000:00:02.0", "0x8086\n", "0x030000\n", "active\n"); // Intel iGPU
        pci(d.path(), "0000:01:00.1", "0x10de\n", "0x040300\n", "suspended\n"); // HDMI audio function
        let v = nvidia_pci(d.path());
        assert_eq!(
            v,
            vec![
                PciGpu { bdf: "0000:01:00.0".into(), suspended: false },
                PciGpu { bdf: "0000:02:00.0".into(), suspended: true }
            ]
        );
    }

    #[test]
    fn suspended_gpu_is_never_queried() {
        let d = tempfile::tempdir().unwrap();
        pci(d.path(), "0000:01:00.0", "0x10de\n", "0x030200\n", "suspended\n");
        // If this script ran, it would leave a marker file behind.
        let marker = d.path().join("ran");
        let prog = script(d.path(), "smi", &format!("touch {}\nexit 0", marker.display()));
        let mut s = GpuSource::new(GpuConfig {
            root: d.path().to_path_buf(),
            smi_program: prog,
            use_nvml: false,
            ..Default::default()
        });
        let g = s.poll();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].pstate.as_deref(), Some("D3"));
        assert_eq!(g[0].power_w, None);
        assert!(!marker.exists(), "a runtime-suspended GPU must not be woken");
    }

    #[test]
    fn smi_exit_status_is_checked_and_backs_off() {
        let d = tempfile::tempdir().unwrap();
        let counter = d.path().join("count");
        let prog = script(
            d.path(),
            "smi",
            &format!("echo x >> {}\necho 'NVIDIA-SMI has failed' >&2\nexit 9", counter.display()),
        );
        let mut s = GpuSource::new(GpuConfig {
            root: d.path().to_path_buf(),
            smi_program: prog,
            use_nvml: false,
            ..Default::default()
        });
        assert!(s.poll().is_empty());
        assert!(s.retry_at.is_some(), "failure must schedule a backoff");
        let calls = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert!(s.poll().is_empty()); // inside the backoff window: no new process
        assert_eq!(std::fs::read_to_string(&counter).unwrap().lines().count(), calls);
        assert!(!s.disabled());
    }

    #[test]
    fn smi_missing_disables_source() {
        let d = tempfile::tempdir().unwrap();
        let mut s = GpuSource::new(GpuConfig {
            root: d.path().to_path_buf(),
            smi_program: "/nonexistent/nvidia-smi".into(),
            use_nvml: false,
            ..Default::default()
        });
        assert!(s.poll().is_empty());
        assert!(s.disabled());
    }

    #[test]
    fn old_driver_falls_back_down_the_field_ladder() {
        let d = tempfile::tempdir().unwrap();
        // fails whenever either "reasons" spelling is requested (a pre-R535 driver rejects the new
        // name, and here also the alias), succeeds without it
        let prog = script(
            d.path(),
            "smi",
            "case \"$1\" in *reasons*) echo 'Field not found' >&2; exit 2;; esac\n\
             echo '0, 41, 25.5, 180.00, 3, 100, 16000, 300, 900, 30, P8, NVIDIA Test, GPU'",
        );
        let mut s = GpuSource::new(GpuConfig {
            root: d.path().to_path_buf(),
            smi_program: prog,
            use_nvml: false,
            ..Default::default()
        });
        let g = s.poll();
        assert_eq!(g.len(), 1, "fell back to the query without the reasons column");
        assert_eq!(g[0].name, "NVIDIA Test, GPU");
        assert_eq!(g[0].clk_event_reasons, None);
        assert_eq!(s.smi_level, 2);
    }

    #[test]
    fn large_output_does_not_deadlock() {
        let d = tempfile::tempdir().unwrap();
        let prog = script(d.path(), "big", "head -c 300000 /dev/zero | tr '\\0' 'a'");
        let out = run_with_timeout(&prog, &[], Duration::from_secs(5)).unwrap();
        assert_eq!(out.len(), 300_000);
    }

    #[test]
    fn hung_process_times_out() {
        let d = tempfile::tempdir().unwrap();
        let prog = script(d.path(), "hang", "sleep 30");
        let t = Instant::now();
        assert!(matches!(run_with_timeout(&prog, &[], Duration::from_millis(200)), Err(RunError::Timeout(_))));
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn poller_serves_and_expires_data() {
        let d = tempfile::tempdir().unwrap();
        let prog = script(d.path(), "smi", "echo '0, 41, 25.5, 180.00, 3, 100, 16000, 300, 900, 30, P8, 0x0, GPU A'");
        let p = GpuPoller::spawn(GpuConfig {
            root: d.path().to_path_buf(),
            smi_program: prog,
            use_nvml: false,
            poll: Duration::from_millis(50),
            ..Default::default()
        });
        let g = p.latest(Duration::from_secs(5));
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].name, "GPU A");
        assert_eq!(g[0].clk_event_reasons.as_deref(), Some("0x0"));
    }
}
