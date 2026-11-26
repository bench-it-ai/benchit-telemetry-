//! Panel feeds: a flat JSON document for desktop widgets and a fixed-width text rendering
//! for conky / terminal / GNOME extensions.
use crate::model::Snapshot;
use serde_json::{json, Value};

/// Recording status included in the panel feed.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RecInfo {
    pub rows: i64,
    pub last_ts: i64,
    pub interval_s: f64,
    pub fast: bool,
    pub retain_days: u32,
}

/// Flat JSON consumed by the bundled GTK panel (`contrib/gtk-panel`). Key names are kept
/// stable for widget compatibility (`tctl`, `mem_pct` = *available* percent, etc.).
pub fn flat_json(s: &Snapshot, rec: Option<&RecInfo>) -> Value {
    let mem_avail_pct = match (s.mem.total_mb, s.mem.avail_mb) {
        (Some(t), Some(a)) if t > 0.0 => Some((100.0 * a / t).floor() as i64),
        _ => None,
    };
    let root = s.fs("root");
    let home = s.fs("home");
    let bat = s.battery.as_ref();
    let fans_txt = if s.fans.is_empty() {
        "none".to_string()
    } else {
        s.fans.iter().map(|f| format!("{} {}", f.name, f.rpm)).collect::<Vec<_>>().join(", ")
    };
    json!({
        "ts": s.ts,
        "ts_ext": s.ts,
        "host": s.host,
        "cpu_model": s.cpu.model,
        "tctl": s.cpu.temp_c,
        "tccd1": s.cpu.temp2_c,
        "tccd2": s.cpu.temp3_c,
        "cpu_util": s.cpu.util_pct.map(|v| v.round() as i64).unwrap_or(0),
        "cpu_mhz": s.cpu.mhz.map(|v| v as i64).unwrap_or(0),
        "load1": s.cpu.load1,
        "cpu_w": s.cpu.pkg_w,
        "sys_w": s.cpu.platform_w,
        "gpus": s.gpus.iter().map(|g| json!({
            "idx": g.idx, "name": g.name, "temp": g.temp_c, "power": g.power_w,
            "limit": g.power_limit_w.unwrap_or(0.0), "util": g.util_pct, "vram": g.vram_used_mb,
            "vram_total": g.vram_total_mb, "sm_mhz": g.sm_mhz, "mem_mhz": g.mem_mhz,
            "fan_pct": g.fan_pct, "pstate": g.pstate, "reasons": g.clk_event_reasons,
        })).collect::<Vec<_>>(),
        "gpu_w": s.gpu_total_w,
        "est_total_w": s.est_total_w,
        "mem_total": s.mem.total_mb,
        "mem_avail": s.mem.avail_mb,
        "mem_pct": mem_avail_pct,
        "swap_total": s.mem.swap_total_mb,
        "swap_free": match (s.mem.swap_total_mb, s.mem.swap_used_mb) { (Some(t), Some(u)) => Some(t - u), _ => None },
        "majflt": s.mem.majflt_s.unwrap_or(0.0) as i64,
        "swpin": s.mem.swpin_s.unwrap_or(0.0) as i64,
        "swpout": s.mem.swpout_s.unwrap_or(0.0) as i64,
        "top": s.top_proc,
        "fans": fans_txt,
        "fans_list": s.fans,
        "bat_pct": bat.and_then(|b| b.pct),
        "bat_w": bat.and_then(|b| b.watts),
        "bat_status": bat.and_then(|b| b.status.clone()),
        "ac_online": bat.and_then(|b| b.ac_online.map(i64::from)),
        "flags": s.flags.join(" "),
        "uptime": s.uptime_s,
        "root_used_gb": root.map(|f| f.used_gb), "root_total_gb": root.map(|f| f.total_gb), "root_used_pct": root.map(|f| f.used_pct),
        "home_used_gb": home.map(|f| f.used_gb), "home_total_gb": home.map(|f| f.total_gb), "home_used_pct": home.map(|f| f.used_pct),
        "disk_read_mbs": s.disk.read_mbs, "disk_write_mbs": s.disk.write_mbs, "disk_busy_pct": s.disk.busy_pct,
        "net_rx_mbs": s.net.rx_mbs, "net_tx_mbs": s.net.tx_mbs,
        "rec": rec,
    })
}

fn f(v: Option<f64>, nd: usize) -> String {
    v.map(|x| format!("{x:.nd$}")).unwrap_or_else(|| "--".into())
}

fn rate(v: Option<f64>) -> String {
    match v {
        None => "--".into(),
        Some(x) if x >= 100.0 => format!("{x:.0}"),
        Some(x) if x >= 1.0 => format!("{x:.1}"),
        Some(x) => format!("{x:.2}"),
    }
}

/// Shorten CPU/GPU model names for narrow panels.
pub fn short_name(s: &str) -> String {
    let mut out = s.to_string();
    for p in ["NVIDIA GeForce ", "NVIDIA ", "AMD ", "Intel(R) ", "Core(TM) ", "(R)", "(TM)", " Processor", " CPU"] {
        out = out.replace(p, "");
    }
    if let Some(i) = out.find(" with ") {
        out.truncate(i);
    }
    let words: Vec<&str> = out.split_whitespace().filter(|w| !w.ends_with("-Core")).collect();
    words.join(" ")
}

fn fit(line: String, width: usize) -> String {
    if line.chars().count() <= width {
        line
    } else {
        let mut s: String = line.chars().take(width.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

/// Fixed-width multi-line text; every line is at most `width` characters (never cut off).
pub fn text(s: &Snapshot, width: usize) -> String {
    let width = width.max(24);
    let mut lines = Vec::new();
    let up = s.uptime_s.unwrap_or(0.0) as u64;
    let (d, h, m) = (up / 86400, (up % 86400) / 3600, (up % 3600) / 60);
    let status = if s.flags.is_empty() { "NOMINAL".to_string() } else { s.flags.join(" ") };
    lines.push(format!("{}  up {d}d {h:02}:{m:02}", s.host.to_uppercase()));
    lines.push(status);
    lines.push(format!("CPU  {}", short_name(s.cpu.model.as_deref().unwrap_or("cpu"))));
    lines.push(format!(
        "  {}°C  {}%  {} MHz  {} W",
        f(s.cpu.temp_c, 0),
        f(s.cpu.util_pct, 0),
        f(s.cpu.mhz, 0),
        f(s.cpu.pkg_w, 0)
    ));
    for g in &s.gpus {
        lines.push(format!("GPU{} {}", g.idx, short_name(&g.name)));
        let lim = g.power_limit_w.map(|l| format!("/{l:.0}")).unwrap_or_default();
        lines.push(format!(
            "  {}°C  {}%  {}{} W  {:.1}/{:.0} GB",
            f(g.temp_c, 0),
            f(g.util_pct, 0),
            f(g.power_w, 0),
            lim,
            g.vram_used_mb.unwrap_or(0.0) / 1024.0,
            g.vram_total_mb.unwrap_or(0.0) / 1024.0
        ));
    }
    lines.push(format!(
        "MEM  {}% used  {:.0}/{:.0} GB  swap {:.1} GB",
        f(s.mem.used_pct, 0),
        (s.mem.total_mb.unwrap_or(0.0) - s.mem.avail_mb.unwrap_or(0.0)) / 1024.0,
        s.mem.total_mb.unwrap_or(0.0) / 1024.0,
        s.mem.swap_used_mb.unwrap_or(0.0) / 1024.0
    ));
    for fs in &s.disk.filesystems {
        lines.push(format!(
            "DISK {} {:.0}/{:.0} GB ({:.0}%)",
            if fs.role == "root" { "/" } else { "home" },
            fs.used_gb,
            fs.total_gb,
            fs.used_pct
        ));
    }
    lines.push(format!("IO   R {} W {} MB/s", rate(s.disk.read_mbs), rate(s.disk.write_mbs)));
    lines.push(format!("NET  RX {} TX {} MB/s", rate(s.net.rx_mbs), rate(s.net.tx_mbs)));
    if let Some(b) = &s.battery {
        lines.push(format!("BAT  {}%  {} W  {}", f(b.pct, 0), f(b.watts, 1), b.status.clone().unwrap_or_default()));
    }
    if !s.fans.is_empty() {
        lines.push(format!(
            "FANS {}",
            s.fans.iter().map(|x| format!("{} {}", x.name, x.rpm)).collect::<Vec<_>>().join(", ")
        ));
    }
    lines.push(format!(
        "PWR  {} W total{}",
        f(s.est_total_w, 0),
        if s.cpu.pkg_w.is_none() { " (gpus only)" } else { "" }
    ));
    lines.into_iter().map(|l| fit(l, width)).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn sample() -> Snapshot {
        Snapshot {
            ts: 1,
            host: "box".into(),
            cpu: Cpu {
                model: Some("AMD Ryzen 9 5900XT 16-Core Processor".into()),
                temp_c: Some(40.0),
                ..Default::default()
            },
            gpus: vec![Gpu {
                idx: 0,
                name: "NVIDIA GeForce RTX 5060 Ti".into(),
                power_limit_w: Some(180.0),
                ..Default::default()
            }],
            mem: Mem { total_mb: Some(1000.0), avail_mb: Some(250.0), ..Default::default() },
            fans: vec![Fan { name: "A very long fan sensor name that would overflow".into(), rpm: 1 }],
            ..Default::default()
        }
    }

    #[test]
    fn names() {
        assert_eq!(short_name("AMD Ryzen 9 5900XT 16-Core Processor"), "Ryzen 9 5900XT");
        assert_eq!(short_name("NVIDIA GeForce RTX 5070 Laptop GPU"), "RTX 5070 Laptop GPU");
        assert_eq!(short_name("Intel(R) Core(TM) Ultra 9 275HX"), "Ultra 9 275HX");
    }

    #[test]
    fn text_never_exceeds_width() {
        let t = text(&sample(), 30);
        assert!(t.lines().all(|l| l.chars().count() <= 30), "{t}");
        assert!(t.contains("GPU0 RTX 5060 Ti"));
    }

    #[test]
    fn flat_json_keys() {
        let v = flat_json(&sample(), None);
        assert_eq!(v["mem_pct"], 25);
        assert_eq!(v["gpus"][0]["limit"], 180.0);
        assert_eq!(v["host"], "box");
    }
}
