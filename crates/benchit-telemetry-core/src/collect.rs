//! Hardware collection from `/proc`, `/sys` and `nvidia-smi`.
use crate::gpu::{GpuConfig, GpuPoller};
use crate::model::*;
use crate::parse::{self, DiskCounters};
use crate::timefmt::now_f64;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Collector settings.
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    /// Filesystem root used to find `proc/` and `sys/` (tests point this at fixtures).
    pub root: PathBuf,
    /// Second filesystem to report as `home` when it is a different device from `/`.
    pub home: Option<PathBuf>,
    /// Query NVIDIA GPUs (NVML, falling back to `nvidia-smi`; skipped automatically if neither exists).
    pub nvidia: bool,
    /// GPU backend settings (timeout, poll period). `gpu.root` is overridden by `root`.
    pub gpu: GpuConfig,
    /// Scan `/proc/<pid>` for the largest resident process.
    pub top_process: bool,
    /// How often the (relatively expensive) top-process scan runs.
    pub top_process_period: Duration,
    /// CPU temperature that raises `WARN_CPU_TEMP`.
    pub cpu_temp_warn_c: f64,
    /// GPU temperature that raises `WARN_GPU_TEMP`.
    pub gpu_temp_warn_c: f64,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("/"),
            home: std::env::var_os("HOME").map(PathBuf::from),
            nvidia: true,
            gpu: GpuConfig::default(),
            top_process: true,
            top_process_period: Duration::from_secs(10),
            cpu_temp_warn_c: 95.0,
            gpu_temp_warn_c: 88.0,
        }
    }
}

#[derive(Debug, Clone)]
struct Energy {
    path: PathBuf,
    range: u64,
}

#[derive(Debug, Clone, Default)]
struct TempSources {
    t1: Option<PathBuf>,
    t2: Option<PathBuf>,
    t3: Option<PathBuf>,
    /// Directory whose `temp*_input` maximum is used as `temp2` (Intel coretemp).
    hottest_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
struct Prev {
    t: f64,
    cpu: Option<(u64, u64)>,
    disk: HashMap<String, DiskCounters>,
    net: HashMap<String, (u64, u64)>,
    vm: HashMap<String, u64>,
    pkg_uj: Option<u64>,
    psys_uj: Option<u64>,
}

/// Stateful collector; call [`Collector::sample`] periodically. Rates are computed from the
/// previous call, so the first sample has no rate values.
pub struct Collector {
    cfg: CollectorConfig,
    host: String,
    cpu_model: Option<String>,
    threads: usize,
    temps: TempSources,
    pkg: Option<Energy>,
    psys: Option<Energy>,
    gpu: Option<GpuPoller>,
    gpu_first: bool,
    prev: Option<Prev>,
    /// Counters at the last storage-interval boundary (see [`Collector::interval_snapshot`]).
    interval_base: Option<Prev>,
    top_proc: Option<(Instant, Option<String>)>,
    discovered: Instant,
}

/// Short host name (`/proc/sys/kernel/hostname` up to the first dot) without building a full [`Collector`].
pub fn hostname(root: &Path) -> String {
    read_trim(root.join("proc/sys/kernel/hostname"))
        .map(|h| h.split('.').next().unwrap_or("").to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

fn read(p: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(p).ok()
}

fn read_trim(p: impl AsRef<Path>) -> Option<String> {
    read(p).map(|s| s.trim().to_string())
}

fn read_u64(p: impl AsRef<Path>) -> Option<u64> {
    read_trim(p)?.parse().ok()
}

fn round(v: f64, nd: i32) -> f64 {
    let m = 10f64.powi(nd);
    (v * m).round() / m
}

impl Collector {
    pub fn new(cfg: CollectorConfig) -> Self {
        let host = hostname(&cfg.root);
        let cpuinfo = read(cfg.root.join("proc/cpuinfo")).unwrap_or_default();
        let cpu_model = cpuinfo
            .lines()
            .find(|l| l.starts_with("model name"))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string());
        let threads = cpuinfo.lines().filter(|l| l.starts_with("processor")).count();
        let temps = discover_temps(&cfg.root.join("sys/class/hwmon"));
        let (pkg, psys) = discover_rapl(&cfg.root.join("sys/class/powercap"));
        let gpu = cfg.nvidia.then(|| {
            let mut g = cfg.gpu.clone();
            g.root = cfg.root.clone();
            GpuPoller::spawn(g)
        });
        Self {
            cfg,
            host,
            cpu_model,
            threads,
            temps,
            pkg,
            psys,
            gpu,
            gpu_first: true,
            prev: None,
            interval_base: None,
            top_proc: None,
            discovered: Instant::now(),
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.cfg.root.join(rel)
    }

    /// Take one snapshot.
    pub fn sample(&mut self) -> Snapshot {
        let t = now_f64();
        let mut s = Snapshot { ts: t as i64, host: self.host.clone(), ..Default::default() };
        let prev = self.prev.take();
        let mut cur = Prev { t, ..Default::default() };
        if self.discovered.elapsed() > Duration::from_secs(600) {
            // sensor drivers (k10temp, nvidia, ...) can appear after boot
            self.temps = discover_temps(&self.cfg.root.join("sys/class/hwmon"));
            let (pkg, psys) = discover_rapl(&self.cfg.root.join("sys/class/powercap"));
            (self.pkg, self.psys) = (pkg, psys);
            self.discovered = Instant::now();
        }

        // CPU
        s.cpu.model = self.cpu_model.clone();
        s.cpu.threads = self.threads;
        cur.cpu = read(self.p("proc/stat")).as_deref().and_then(parse::proc_stat_cpu);
        s.cpu.mhz = self.cpu_mhz();
        if let Some((a, b, c)) = read(self.p("proc/loadavg")).as_deref().and_then(parse::loadavg) {
            (s.cpu.load1, s.cpu.load5, s.cpu.load15) = (Some(a), Some(b), Some(c));
        }
        let temp = |p: &Option<PathBuf>| p.as_ref().and_then(read_u64).map(|v| round(v as f64 / 1000.0, 1));
        s.cpu.temp_c = temp(&self.temps.t1);
        s.cpu.temp2_c = temp(&self.temps.t2);
        s.cpu.temp3_c = temp(&self.temps.t3);
        if s.cpu.temp2_c.is_none() {
            if let Some(dir) = &self.temps.hottest_dir {
                s.cpu.temp2_c = hottest(dir);
            }
        }
        cur.pkg_uj = self.pkg.as_ref().and_then(|e| read_u64(&e.path));
        cur.psys_uj = self.psys.as_ref().and_then(|e| read_u64(&e.path));

        // Memory
        let mi = parse::meminfo(&read(self.p("proc/meminfo")).unwrap_or_default());
        let mb = |k: &str| mi.get(k).map(|v| round(*v as f64 / 1024.0, 0));
        s.mem.total_mb = mb("MemTotal");
        s.mem.avail_mb = mb("MemAvailable");
        if let (Some(t), Some(a)) = (s.mem.total_mb, s.mem.avail_mb) {
            if t > 0.0 {
                s.mem.used_pct = Some(round(100.0 - 100.0 * a / t, 1));
            }
        }
        s.mem.swap_total_mb = mb("SwapTotal");
        if let (Some(t), Some(f)) = (s.mem.swap_total_mb, mb("SwapFree")) {
            s.mem.swap_used_mb = Some((t - f).max(0.0));
        }
        cur.vm = parse::vmstat(&read(self.p("proc/vmstat")).unwrap_or_default());

        // Disk + filesystems
        let block = self.p("sys/block");
        cur.disk = parse::diskstats(&read(self.p("proc/diskstats")).unwrap_or_default(), |n| {
            block.join(n).join("device").exists()
        });
        s.disk.filesystems = self.filesystems();

        // Network
        let netdir = self.p("sys/class/net");
        cur.net = parse::net_dev(&read(self.p("proc/net/dev")).unwrap_or_default(), |n| {
            netdir.join(n).join("device").exists()
        });
        if let Some(p) = prev.as_ref() {
            self.fill_rates(&mut s, p, &cur);
        }

        // GPUs, fans, battery, processes
        if let Some(g) = &self.gpu {
            // the first call waits for the poller's first result; later calls never block
            let wait = if self.gpu_first { self.cfg.gpu.timeout + Duration::from_secs(1) } else { Duration::ZERO };
            self.gpu_first = false;
            s.gpus = g.latest(wait);
        }
        recompute_totals(&mut s);
        s.fans = fans(&self.p("sys/class/hwmon"));
        s.battery = battery(&self.p("sys/class/power_supply"));
        if self.cfg.top_process {
            let stale = self.top_proc.as_ref().is_none_or(|(t, _)| t.elapsed() >= self.cfg.top_process_period);
            if stale {
                self.top_proc = Some((Instant::now(), top_process(&self.p("proc"))));
            }
            s.top_proc = self.top_proc.as_ref().and_then(|(_, v)| v.clone());
        }
        s.uptime_s = read(self.p("proc/uptime")).and_then(|u| u.split_whitespace().next().and_then(|x| x.parse().ok()));
        s.flags = self.flags(&s);
        self.prev = Some(cur);
        s
    }

    /// Fill every counter-derived field of `s` from two counter sets.
    fn fill_rates(&self, s: &mut Snapshot, p: &Prev, cur: &Prev) {
        let dt = (cur.t - p.t).max(1e-3);
        if let (Some((b1, t1)), Some((b0, t0))) = (cur.cpu, p.cpu) {
            if t1 > t0 {
                s.cpu.util_pct = Some(round(100.0 * (b1.saturating_sub(b0)) as f64 / (t1 - t0) as f64, 1));
            }
        }
        let watts = |e: &Option<Energy>, c: Option<u64>, o: Option<u64>| match (e, c, o) {
            (Some(e), Some(c), Some(o)) => {
                parse::wrapping_delta_checked(o, c, e.range).map(|d| round(d as f64 / 1e6 / dt, 1))
            }
            _ => None,
        };
        s.cpu.pkg_w = watts(&self.pkg, cur.pkg_uj, p.pkg_uj);
        s.cpu.platform_w = watts(&self.psys, cur.psys_uj, p.psys_uj);
        let rate = |k: &str| match (cur.vm.get(k), p.vm.get(k)) {
            (Some(c), Some(o)) => Some(round(c.saturating_sub(*o) as f64 / dt, 0)),
            _ => None,
        };
        s.mem.majflt_s = rate("pgmajfault");
        s.mem.swpin_s = rate("pswpin");
        s.mem.swpout_s = rate("pswpout");
        let (mut r, mut w, mut busy) = (0u64, 0u64, 0u64);
        for (n, c) in &cur.disk {
            if let Some(o) = p.disk.get(n) {
                r += c.read_bytes.saturating_sub(o.read_bytes);
                w += c.write_bytes.saturating_sub(o.write_bytes);
                // io_ticks is a 32-bit kernel counter: it wraps after ~49.7 days
                busy = busy.max(parse::wrapping_delta(o.io_ticks_ms, c.io_ticks_ms, 1 << 32));
            }
        }
        s.disk.read_mbs = Some(round(r as f64 / dt / 1e6, 2));
        s.disk.write_mbs = Some(round(w as f64 / dt / 1e6, 2));
        s.disk.busy_pct = Some(round((busy as f64 / (dt * 10.0)).min(100.0), 1));
        let (mut rx, mut tx) = (0u64, 0u64);
        for (n, (r, t)) in &cur.net {
            if let Some((or, ot)) = p.net.get(n) {
                rx += r.saturating_sub(*or);
                tx += t.saturating_sub(*ot);
            }
        }
        s.net.rx_mbs = Some(round(rx as f64 / dt / 1e6, 3));
        s.net.tx_mbs = Some(round(tx as f64 / dt / 1e6, 3));
    }

    /// The snapshot to **store** for a storage interval. Every counter-derived field (CPU %,
    /// package/platform watts, fault rates, disk and network throughput) is re-computed as the
    /// average over the whole span since the previous call, rather than over the last tick only.
    /// Call it right after [`Collector::sample`] each time a row is stored. If the span is
    /// longer than `max_span` seconds (suspend, stalled loop) those fields are left empty
    /// instead of averaging across the gap. The first call has no baseline and returns `snap`.
    pub fn interval_snapshot(&mut self, snap: &Snapshot, max_span: f64) -> Snapshot {
        let mut out = snap.clone();
        let cur = self.prev.clone();
        if let (Some(base), Some(c)) = (self.interval_base.take(), cur.as_ref()) {
            out.cpu.util_pct = None;
            out.cpu.pkg_w = None;
            out.cpu.platform_w = None;
            out.mem.majflt_s = None;
            out.mem.swpin_s = None;
            out.mem.swpout_s = None;
            (out.disk.read_mbs, out.disk.write_mbs, out.disk.busy_pct) = (None, None, None);
            (out.net.rx_mbs, out.net.tx_mbs) = (None, None);
            if c.t > base.t && c.t - base.t <= max_span {
                self.fill_rates(&mut out, &base, c);
            }
            recompute_totals(&mut out);
        }
        self.interval_base = cur;
        out
    }

    /// Take two samples `gap` apart and return the second (so rates are populated).
    pub fn sample_with_rates(&mut self, gap: Duration) -> Snapshot {
        if self.prev.is_none() {
            let _ = self.sample();
            std::thread::sleep(gap);
        }
        self.sample()
    }

    fn cpu_mhz(&self) -> Option<f64> {
        let dir = self.p("sys/devices/system/cpu");
        let mut sum = 0u64;
        let mut n = 0u64;
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("cpu") && name[3..].chars().all(|c| c.is_ascii_digit()) && name.len() > 3 {
                    if let Some(v) = read_u64(e.path().join("cpufreq/scaling_cur_freq")) {
                        sum += v;
                        n += 1;
                    }
                }
            }
        }
        if n > 0 {
            return Some(round(sum as f64 / n as f64 / 1000.0, 0));
        }
        let ci = read(self.p("proc/cpuinfo"))?;
        let v: Vec<f64> = ci
            .lines()
            .filter(|l| l.starts_with("cpu MHz"))
            .filter_map(|l| l.split_once(':')?.1.trim().parse().ok())
            .collect();
        (!v.is_empty()).then(|| round(v.iter().sum::<f64>() / v.len() as f64, 0))
    }

    fn filesystems(&self) -> Vec<Filesystem> {
        let mut out = Vec::new();
        let root_dev = statfs(&self.cfg.root).map(|(d, ..)| d);
        if let Some((_, used, total, avail)) = statfs(&self.cfg.root) {
            out.push(fs_entry("root", "/", used, total, avail));
        }
        if let Some(home) = &self.cfg.home {
            if let Some((dev, used, total, avail)) = statfs(home) {
                if Some(dev) != root_dev {
                    out.push(fs_entry("home", &home.to_string_lossy(), used, total, avail));
                }
            }
        }
        out
    }

    fn flags(&self, s: &Snapshot) -> Vec<String> {
        let mut f = Vec::new();
        if s.cpu.temp_c.is_some_and(|t| t >= self.cfg.cpu_temp_warn_c) {
            f.push("WARN_CPU_TEMP".into());
        }
        if s.gpus.iter().any(|g| g.temp_c.is_some_and(|t| t >= self.cfg.gpu_temp_warn_c)) {
            f.push("WARN_GPU_TEMP".into());
        }
        if s.mem.used_pct.is_some_and(|u| u >= 90.0) {
            f.push("WARN_MEM_LOW".into());
        }
        if let Some(b) = &s.battery {
            if b.ac_online == Some(false) && b.pct.is_some_and(|p| p <= 15.0) {
                f.push("WARN_BATTERY_LOW".into());
            }
        }
        f
    }
}

fn fs_entry(role: &str, path: &str, used: u64, total: u64, avail: u64) -> Filesystem {
    Filesystem {
        role: role.into(),
        path: path.into(),
        used_gb: round(used as f64 / 1e9, 1),
        total_gb: round(total as f64 / 1e9, 1),
        used_pct: if used + avail > 0 { round(100.0 * used as f64 / (used + avail) as f64, 1) } else { 0.0 },
    }
}

/// `(device id, used bytes, total bytes, bytes available to non-root)` for the filesystem holding `path`.
fn statfs(path: &Path) -> Option<(u64, u64, u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let dev = fs::metadata(path).ok()?.dev();
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    let total = st.f_blocks as u64 * frsize;
    let free = st.f_bfree as u64 * frsize;
    let avail = st.f_bavail as u64 * frsize;
    // like `df`: percentage of what a normal user can use (excludes root-reserved blocks)
    Some((dev, total.saturating_sub(free), total, avail))
}

fn hwmons(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = fs::read_dir(dir)
        .map(|rd| rd.flatten().filter_map(|e| Some((read_trim(e.path().join("name"))?, e.path()))).collect())
        .unwrap_or_default();
    v.sort_by(|a, b| a.1.cmp(&b.1));
    v
}

fn labelled(dir: &Path, prefix: &str) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    if !(n.starts_with(prefix) && n.ends_with("_input")) {
                        return None;
                    }
                    let base = n.trim_end_matches("_input");
                    let label = read_trim(dir.join(format!("{base}_label"))).unwrap_or_else(|| base.to_string());
                    Some((label, e.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| a.1.cmp(&b.1));
    v
}

fn discover_temps(hwmon: &Path) -> TempSources {
    let mons = hwmons(hwmon);
    if let Some((_, d)) = mons.iter().find(|(n, _)| n == "k10temp") {
        let l = labelled(d, "temp");
        let get = |want: &str| l.iter().find(|(lab, _)| lab == want).map(|(_, p)| p.clone());
        return TempSources {
            t1: get("Tctl").or_else(|| l.first().map(|x| x.1.clone())),
            t2: get("Tccd1"),
            t3: get("Tccd2"),
            hottest_dir: None,
        };
    }
    if let Some((_, d)) = mons.iter().find(|(n, _)| n == "coretemp") {
        let l = labelled(d, "temp");
        let pkg = l.iter().find(|(lab, _)| lab.starts_with("Package")).map(|(_, p)| p.clone());
        return TempSources {
            t1: pkg.or_else(|| l.first().map(|x| x.1.clone())),
            t2: None,
            t3: None,
            hottest_dir: Some(d.clone()),
        };
    }
    for vendor in ["alienware_wmi", "zenpower", "cpu_thermal", "thinkpad", "asus", "dell_smm", "acpitz"] {
        if let Some((_, d)) = mons.iter().find(|(n, _)| n == vendor) {
            let l = labelled(d, "temp");
            let cpu = l.iter().find(|(lab, _)| lab.eq_ignore_ascii_case("cpu")).map(|(_, p)| p.clone());
            return TempSources { t1: cpu.or_else(|| l.first().map(|x| x.1.clone())), ..Default::default() };
        }
    }
    TempSources::default()
}

fn hottest(dir: &Path) -> Option<f64> {
    labelled(dir, "temp")
        .iter()
        .filter(|(label, _)| !label.starts_with("Package"))
        .filter_map(|(_, p)| read_u64(p))
        .max()
        .map(|v| round(v as f64 / 1000.0, 1))
}

fn discover_rapl(powercap: &Path) -> (Option<Energy>, Option<Energy>) {
    let (mut pkg, mut psys) = (None, None);
    let mut dirs: Vec<PathBuf> =
        fs::read_dir(powercap).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    dirs.sort();
    for d in dirs {
        let fname = d.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
        // top-level zones only (intel-rapl:0, not intel-rapl:0:0)
        if fname.matches(':').count() != 1 {
            continue;
        }
        let e = d.join("energy_uj");
        if fs::read_to_string(&e).is_err() {
            continue; // usually root-only
        }
        let en = Energy { path: e, range: read_u64(d.join("max_energy_range_uj")).unwrap_or(0) };
        match read_trim(d.join("name")).as_deref() {
            Some("package-0") | Some("package") if pkg.is_none() => pkg = Some(en),
            Some("psys") | Some("platform") if psys.is_none() => psys = Some(en),
            _ => {}
        }
    }
    (pkg, psys)
}

fn fans(hwmon: &Path) -> Vec<Fan> {
    let mons = hwmons(hwmon);
    let prefer = mons.iter().find(|(n, _)| n == "alienware_wmi").map(|(_, p)| p.clone());
    let mut out = Vec::new();
    for (name, dir) in &mons {
        if matches!(name.as_str(), "dell_smm" | "dell_ddv") {
            continue;
        }
        if prefer.as_ref().is_some_and(|p| p != dir) {
            continue;
        }
        for (label, p) in labelled(dir, "fan") {
            if let Some(rpm) = read_u64(&p) {
                let label =
                    if label.starts_with("fan") && prefer.is_none() { format!("{name}/{label}") } else { label };
                out.push(Fan { name: label, rpm: rpm as u32 });
            }
        }
    }
    out
}

fn battery(ps: &Path) -> Option<Battery> {
    let mut entries: Vec<PathBuf> = fs::read_dir(ps).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    let mut bats = Vec::new();
    let mut ac_seen = false;
    let mut ac_online = false;
    for p in entries {
        match read_trim(p.join("type")).as_deref() {
            Some("Battery") if read_trim(p.join("scope")).as_deref() != Some("Device") => {
                let pct = read_u64(p.join("capacity")).map(|v| v as f64);
                let status = read_trim(p.join("status"));
                let mut w = read_u64(p.join("power_now")).map(|v| v as f64 / 1e6).or_else(|| {
                    let c = read_u64(p.join("current_now"))? as f64;
                    let v = read_u64(p.join("voltage_now"))? as f64;
                    Some(c * v / 1e12)
                });
                if status.as_deref() == Some("Discharging") {
                    w = w.map(|x| -x.abs());
                }
                // energy_full/energy_now (µWh) weight the percentage when there are several packs
                let full =
                    read_u64(p.join("energy_full")).or_else(|| read_u64(p.join("charge_full"))).map(|v| v as f64);
                bats.push((pct, w, status, full));
            }
            // USB-C power delivery supplies count as external power too
            Some("Mains") | Some("USB") => {
                if let Some(v) = read_u64(p.join("online")) {
                    ac_seen = true;
                    ac_online |= v == 1;
                }
            }
            _ => {}
        }
    }
    if bats.is_empty() {
        return None;
    }
    let weight_total: f64 = bats.iter().filter_map(|b| b.3).sum();
    let pct = if bats.len() == 1 || weight_total <= 0.0 {
        bats[0].0
    } else {
        let sum: f64 = bats.iter().filter_map(|b| Some(b.0? * b.3?)).sum();
        Some(round(sum / weight_total, 0))
    };
    let watts = bats.iter().filter_map(|b| b.1).fold(None, |a: Option<f64>, x| Some(a.unwrap_or(0.0) + x));
    let status = bats.iter().find(|b| b.2.as_deref() == Some("Discharging")).or(bats.first()).and_then(|b| b.2.clone());
    Some(Battery { pct, watts: watts.map(|x| round(x, 2)), status, ac_online: ac_seen.then_some(ac_online) })
}

/// GPU board power + CPU package power (both estimates; see `energy_sources` in summaries).
pub fn recompute_totals(s: &mut Snapshot) {
    s.gpu_total_w = (!s.gpus.is_empty()).then(|| round(s.gpus.iter().filter_map(|g| g.power_w).sum::<f64>(), 1));
    s.est_total_w = match (s.gpu_total_w, s.cpu.pkg_w) {
        (None, None) => None,
        (g, c) => Some(round(g.unwrap_or(0.0) + c.unwrap_or(0.0), 1)),
    };
}

fn top_process(proc_dir: &Path) -> Option<String> {
    // SAFETY: sysconf has no preconditions; it only reads a system constant.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let mut best: Option<(u64, String)> = None;
    for e in fs::read_dir(proc_dir).ok()?.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Some(statm) = read(e.path().join("statm")) else { continue };
        let rss = statm.split_whitespace().nth(1).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) * page;
        if best.as_ref().is_none_or(|(b, _)| rss > *b) {
            if let Some(comm) = read_trim(e.path().join("comm")) {
                best = Some((rss, comm));
            }
        }
    }
    best.map(|(rss, comm)| format!("{}:{}", rss / (1024 * 1024), comm))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    #[test]
    fn fixture_tree() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        w(r, "proc/sys/kernel/hostname", "testbox.example\n");
        w(r, "proc/cpuinfo", "processor\t: 0\nmodel name\t: Test CPU 8-Core\ncpu MHz\t\t: 3000.0\nprocessor\t: 1\n");
        w(r, "proc/stat", "cpu  100 0 100 800 0 0 0 0 0 0\n");
        w(r, "proc/loadavg", "1.00 0.50 0.25 1/100 1\n");
        w(r, "proc/meminfo", "MemTotal: 8192000 kB\nMemAvailable: 4096000 kB\nSwapTotal: 1024 kB\nSwapFree: 1024 kB\n");
        w(r, "proc/vmstat", "pgmajfault 10\npswpin 0\npswpout 0\n");
        w(r, "proc/diskstats", " 8 0 sda 1 0 100 0 1 0 100 0 0 10 0\n");
        w(r, "sys/block/sda/device/x", "");
        w(r, "proc/net/dev", "h\nh\n eth0: 1000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n");
        w(r, "sys/class/net/eth0/device/x", "");
        w(r, "sys/class/hwmon/hwmon0/name", "k10temp\n");
        w(r, "sys/class/hwmon/hwmon0/temp1_input", "55250\n");
        w(r, "sys/class/hwmon/hwmon0/temp1_label", "Tctl\n");
        w(r, "sys/class/hwmon/hwmon0/temp3_input", "48000\n");
        w(r, "sys/class/hwmon/hwmon0/temp3_label", "Tccd1\n");
        w(r, "sys/class/hwmon/hwmon1/name", "nct6798\n");
        w(r, "sys/class/hwmon/hwmon1/fan1_input", "1200\n");
        w(r, "sys/class/power_supply/BAT0/type", "Battery\n");
        w(r, "sys/class/power_supply/BAT0/capacity", "80\n");
        w(r, "sys/class/power_supply/BAT0/status", "Discharging\n");
        w(r, "sys/class/power_supply/BAT0/power_now", "15000000\n");
        w(r, "sys/class/power_supply/AC/type", "Mains\n");
        w(r, "sys/class/power_supply/AC/online", "0\n");
        let cfg = CollectorConfig {
            root: r.to_path_buf(),
            home: None,
            nvidia: false,
            top_process: false,
            ..Default::default()
        };
        let mut c = Collector::new(cfg);
        assert_eq!(c.host(), "testbox");
        let s1 = c.sample();
        assert_eq!(s1.cpu.util_pct, None);
        w(r, "proc/stat", "cpu  200 0 200 1400 0 0 0 0 0 0\n");
        w(r, "proc/net/dev", "h\nh\n eth0: 1001000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n");
        std::thread::sleep(Duration::from_millis(50));
        let s = c.sample();
        assert_eq!(s.cpu.model.as_deref(), Some("Test CPU 8-Core"));
        assert_eq!(s.cpu.threads, 2);
        assert_eq!(s.cpu.util_pct, Some(25.0));
        assert_eq!(s.cpu.temp_c, Some(55.3));
        assert_eq!(s.cpu.temp2_c, Some(48.0));
        assert_eq!(s.cpu.mhz, Some(3000.0));
        assert_eq!(s.mem.used_pct, Some(50.0));
        assert!(s.net.rx_mbs.unwrap() > 0.0);
        assert_eq!(s.fans, vec![Fan { name: "nct6798/fan1".into(), rpm: 1200 }]);
        let b = s.battery.unwrap();
        assert_eq!(b.watts, Some(-15.0));
        assert_eq!(b.ac_online, Some(false));
        assert!(s.gpus.is_empty());
        assert_eq!(s.est_total_w, None);
    }

    fn basic(r: &Path) {
        w(r, "proc/sys/kernel/hostname", "box\n");
        w(r, "proc/cpuinfo", "processor\t: 0\nmodel name\t: X\n");
        w(r, "proc/stat", "cpu  100 0 100 800 0 0 0 0 0 0\n");
        w(r, "proc/loadavg", "1 1 1 1/1 1\n");
        w(r, "proc/meminfo", "MemTotal: 1000 kB\nMemAvailable: 500 kB\n");
        w(r, "proc/vmstat", "pgmajfault 0\n");
        w(r, "proc/diskstats", " 8 0 sda 1 0 100 0 1 0 100 0 0 10 0\n");
        w(r, "sys/block/sda/device/x", "");
        w(r, "proc/net/dev", "h\nh\n eth0: 1000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n");
        w(r, "sys/class/net/eth0/device/x", "");
    }

    fn quiet(r: &Path) -> Collector {
        Collector::new(CollectorConfig {
            root: r.to_path_buf(),
            home: None,
            nvidia: false,
            top_process: false,
            ..Default::default()
        })
    }

    #[test]
    fn stored_values_average_the_whole_interval_not_the_last_tick() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        basic(r);
        let mut c = quiet(r);
        let s0 = c.sample();
        let _ = c.interval_snapshot(&s0, 1e6); // baseline
                                               // tick 1: 1 MB received, CPU busy
        std::thread::sleep(Duration::from_millis(30));
        w(r, "proc/net/dev", "h\nh\n eth0: 1001000 0 0 0 0 0 0 0 2000 0 0 0 0 0 0 0\n");
        w(r, "proc/stat", "cpu  200 0 100 900 0 0 0 0 0 0\n");
        let t1 = c.sample();
        assert!(t1.net.rx_mbs.unwrap() > 0.0);
        // tick 2: idle again. The tick values say "nothing happened", the interval value must not.
        std::thread::sleep(Duration::from_millis(30));
        w(r, "proc/stat", "cpu  200 0 100 1000 0 0 0 0 0 0\n");
        let t2 = c.sample();
        assert_eq!(t2.net.rx_mbs, Some(0.0));
        assert_eq!(t2.cpu.util_pct, Some(0.0));
        let stored = c.interval_snapshot(&t2, 1e6);
        assert!(stored.net.rx_mbs.unwrap() > 0.0, "traffic from the first tick is part of the interval");
        let util = stored.cpu.util_pct.unwrap();
        assert!((util - 100.0 / 3.0).abs() < 0.1, "100 busy of 300 total jiffies = 33.3 %, got {util}");
        // the next interval starts fresh
        std::thread::sleep(Duration::from_millis(10));
        let t3 = c.sample();
        let stored3 = c.interval_snapshot(&t3, 1e6);
        assert_eq!(stored3.net.rx_mbs, Some(0.0));
    }

    #[test]
    fn interval_snapshot_does_not_average_across_a_gap() {
        let d = tempfile::tempdir().unwrap();
        basic(d.path());
        let mut c = quiet(d.path());
        let s0 = c.sample();
        let _ = c.interval_snapshot(&s0, 0.01);
        std::thread::sleep(Duration::from_millis(60));
        let s1 = c.sample();
        let stored = c.interval_snapshot(&s1, 0.01); // span 60 ms > max_span 10 ms
        assert_eq!((stored.cpu.util_pct, stored.net.rx_mbs, stored.disk.read_mbs), (None, None, None));
    }

    #[test]
    fn rapl_wrap_without_range_skips_the_sample() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        basic(r);
        w(r, "sys/class/powercap/intel-rapl:0/name", "package-0\n");
        w(r, "sys/class/powercap/intel-rapl:0/energy_uj", "5000000\n");
        let mut c = quiet(r);
        let _ = c.sample();
        std::thread::sleep(Duration::from_millis(20));
        w(r, "sys/class/powercap/intel-rapl:0/energy_uj", "100\n"); // went backwards, range unknown
        assert_eq!(c.sample().cpu.pkg_w, None);
        // with a known range the wrap is accounted for
        w(r, "sys/class/powercap/intel-rapl:0/max_energy_range_uj", "10000000\n");
        let mut c = quiet(r);
        w(r, "sys/class/powercap/intel-rapl:0/energy_uj", "9000000\n");
        let _ = c.sample();
        std::thread::sleep(Duration::from_millis(20));
        w(r, "sys/class/powercap/intel-rapl:0/energy_uj", "1000000\n");
        assert!(c.sample().cpu.pkg_w.unwrap() > 0.0);
    }

    #[test]
    fn disk_busy_counter_wraps_at_32_bits() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        basic(r);
        w(r, "proc/diskstats", " 8 0 sda 1 0 100 0 1 0 100 0 0 4294967290 0\n");
        let mut c = quiet(r);
        let _ = c.sample();
        std::thread::sleep(Duration::from_millis(20));
        w(r, "proc/diskstats", " 8 0 sda 1 0 100 0 1 0 100 0 0 10 0\n"); // wrapped: +16 ms busy
        let busy = c.sample().disk.busy_pct.unwrap();
        assert!(busy > 0.0 && busy <= 100.0, "{busy}");
    }

    #[test]
    fn batteries_are_summed_and_ac_includes_usb_c() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        basic(r);
        for (n, cap, full, pw) in [("BAT0", "50", "30000000", "5000000"), ("BAT1", "100", "10000000", "1000000")] {
            w(r, &format!("sys/class/power_supply/{n}/type"), "Battery\n");
            w(r, &format!("sys/class/power_supply/{n}/capacity"), &format!("{cap}\n"));
            w(r, &format!("sys/class/power_supply/{n}/energy_full"), &format!("{full}\n"));
            w(r, &format!("sys/class/power_supply/{n}/status"), "Discharging\n");
            w(r, &format!("sys/class/power_supply/{n}/power_now"), &format!("{pw}\n"));
        }
        w(r, "sys/class/power_supply/AC/type", "Mains\n");
        w(r, "sys/class/power_supply/AC/online", "0\n");
        w(r, "sys/class/power_supply/ucsi-source-psy-1/type", "USB\n");
        w(r, "sys/class/power_supply/ucsi-source-psy-1/online", "1\n");
        let b = quiet(r).sample().battery.unwrap();
        assert_eq!(b.pct, Some(63.0), "(50*30 + 100*10) / 40 = 62.5 weighted by capacity");
        assert_eq!(b.watts, Some(-6.0));
        assert_eq!(b.ac_online, Some(true));
    }

    #[test]
    fn intel_temp2_is_not_suppressed_when_equal_to_package() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        basic(r);
        w(r, "sys/class/hwmon/hwmon0/name", "coretemp\n");
        w(r, "sys/class/hwmon/hwmon0/temp1_label", "Package id 0\n");
        w(r, "sys/class/hwmon/hwmon0/temp1_input", "60000\n");
        w(r, "sys/class/hwmon/hwmon0/temp2_label", "Core 0\n");
        w(r, "sys/class/hwmon/hwmon0/temp2_input", "60000\n");
        let s = quiet(r).sample();
        assert_eq!((s.cpu.temp_c, s.cpu.temp2_c), (Some(60.0), Some(60.0)));
    }
}
