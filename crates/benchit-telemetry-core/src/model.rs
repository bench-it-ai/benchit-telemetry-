//! Data model shared by the collector, the store, the API and the panel feed.
use serde::{Deserialize, Serialize};

/// One point-in-time reading of the whole machine. Rates (`*_mbs`, `*_s`, `util_pct`, power
/// derived from energy counters) are averaged over the interval since the previous reading.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    /// Unix time in seconds.
    pub ts: i64,
    /// Short hostname.
    pub host: String,
    pub cpu: Cpu,
    pub mem: Mem,
    pub disk: Disk,
    pub net: Net,
    pub gpus: Vec<Gpu>,
    /// Sum of GPU board power (W).
    pub gpu_total_w: Option<f64>,
    /// GPU board power + CPU package power (W); GPU-only when RAPL is unreadable.
    pub est_total_w: Option<f64>,
    pub battery: Option<Battery>,
    pub fans: Vec<Fan>,
    /// Largest process by resident memory, formatted `"<rss MiB>:<comm>"`.
    pub top_proc: Option<String>,
    pub uptime_s: Option<f64>,
    /// Warning flags such as `WARN_CPU_TEMP`.
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Cpu {
    pub model: Option<String>,
    /// Primary temperature: AMD Tctl, Intel package, or vendor WMI CPU sensor.
    pub temp_c: Option<f64>,
    /// Secondary temperature: AMD Tccd1 or hottest Intel core.
    pub temp2_c: Option<f64>,
    /// Tertiary temperature: AMD Tccd2.
    pub temp3_c: Option<f64>,
    pub util_pct: Option<f64>,
    pub mhz: Option<f64>,
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    pub load15: Option<f64>,
    pub threads: usize,
    /// RAPL package power (W); `None` when the counter is root-only.
    pub pkg_w: Option<f64>,
    /// RAPL psys/platform power (W), mostly on laptops.
    pub platform_w: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Mem {
    pub total_mb: Option<f64>,
    pub avail_mb: Option<f64>,
    pub used_pct: Option<f64>,
    pub swap_total_mb: Option<f64>,
    pub swap_used_mb: Option<f64>,
    pub majflt_s: Option<f64>,
    pub swpin_s: Option<f64>,
    pub swpout_s: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Disk {
    pub read_mbs: Option<f64>,
    pub write_mbs: Option<f64>,
    /// Busy time of the busiest physical disk.
    pub busy_pct: Option<f64>,
    pub filesystems: Vec<Filesystem>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Filesystem {
    /// `"root"` or `"home"`.
    pub role: String,
    pub path: String,
    pub used_gb: f64,
    pub total_gb: f64,
    pub used_pct: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Net {
    pub rx_mbs: Option<f64>,
    pub tx_mbs: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Gpu {
    pub idx: u32,
    pub name: String,
    pub temp_c: Option<f64>,
    pub power_w: Option<f64>,
    pub power_limit_w: Option<f64>,
    pub util_pct: Option<f64>,
    pub vram_used_mb: Option<f64>,
    pub vram_total_mb: Option<f64>,
    pub sm_mhz: Option<f64>,
    pub mem_mhz: Option<f64>,
    pub fan_pct: Option<f64>,
    pub pstate: Option<String>,
    /// `clocks_event_reasons.active` bitmask as reported by nvidia-smi (hex string).
    pub clk_event_reasons: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Battery {
    pub pct: Option<f64>,
    /// Positive while charging, negative while discharging.
    pub watts: Option<f64>,
    pub status: Option<String>,
    pub ac_online: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Fan {
    pub name: String,
    pub rpm: u32,
}

impl Snapshot {
    pub fn fs(&self, role: &str) -> Option<&Filesystem> {
        self.disk.filesystems.iter().find(|f| f.role == role)
    }
}
