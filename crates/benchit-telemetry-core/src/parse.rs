//! Pure parsers for kernel and `nvidia-smi` text formats (unit-tested without hardware).
use crate::model::Gpu;
use std::collections::HashMap;

/// Aggregate CPU jiffies from the first line of `/proc/stat`: `(busy, total)`.
pub fn proc_stat_cpu(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    if v.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal (guest is already counted in user)
    let total: u64 = v.iter().take(8).sum();
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    Some((total.saturating_sub(idle), total))
}

/// `/proc/meminfo` as a map of key -> kB.
pub fn meminfo(text: &str) -> HashMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            let (k, rest) = l.split_once(':')?;
            let n = rest.split_whitespace().next()?.parse().ok()?;
            Some((k.trim().to_string(), n))
        })
        .collect()
}

/// `/proc/vmstat` as a map of key -> counter.
pub fn vmstat(text: &str) -> HashMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), it.next()?.parse().ok()?))
        })
        .collect()
}

/// `/proc/loadavg` -> (1, 5, 15 minute load).
pub fn loadavg(text: &str) -> Option<(f64, f64, f64)> {
    let v: Vec<f64> = text.split_whitespace().take(3).filter_map(|x| x.parse().ok()).collect();
    (v.len() == 3).then(|| (v[0], v[1], v[2]))
}

/// One `/proc/diskstats` device line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskCounters {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub io_ticks_ms: u64,
}

/// Parse `/proc/diskstats`, keeping devices accepted by `keep(name)`.
pub fn diskstats(text: &str, keep: impl Fn(&str) -> bool) -> HashMap<String, DiskCounters> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 13 || !keep(f[2]) {
                return None;
            }
            let n = |i: usize| f[i].parse::<u64>().unwrap_or(0);
            Some((
                f[2].to_string(),
                DiskCounters { read_bytes: n(5) * 512, write_bytes: n(9) * 512, io_ticks_ms: n(12) },
            ))
        })
        .collect()
}

/// Parse `/proc/net/dev` -> name -> (rx_bytes, tx_bytes), keeping interfaces accepted by `keep`.
pub fn net_dev(text: &str, keep: impl Fn(&str) -> bool) -> HashMap<String, (u64, u64)> {
    text.lines()
        .skip(2)
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let name = name.trim();
            if !keep(name) {
                return None;
            }
            let f: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            (f.len() >= 9).then(|| (name.to_string(), (f[0], f[8])))
        })
        .collect()
}

/// Difference of two monotonically increasing counters that may wrap at `range`
/// (0 = unknown range, assume 2^32 wrap).
pub fn wrapping_delta(prev: u64, cur: u64, range: u64) -> u64 {
    if cur >= prev {
        cur - prev
    } else {
        let r = if range > 0 { range } else { 1u64 << 32 };
        r.saturating_sub(prev).saturating_add(cur)
    }
}

/// Columns requested from `nvidia-smi`; `name` is last so commas in names cannot shift fields.
pub const NVSMI_FIELDS: &str = "index,temperature.gpu,power.draw,power.limit,utilization.gpu,memory.used,memory.total,clocks.sm,clocks.mem,fan.speed,pstate,clocks_event_reasons.active,name";

/// Query variants tried in order: the R535+ name, the older alias, and no throttle column at
/// all (very old drivers reject the whole query when one field is unknown).
pub const NVSMI_FIELD_LADDER: [(&str, bool); 3] = [
    (NVSMI_FIELDS, true),
    ("index,temperature.gpu,power.draw,power.limit,utilization.gpu,memory.used,memory.total,clocks.sm,clocks.mem,fan.speed,pstate,clocks_throttle_reasons.active,name", true),
    ("index,temperature.gpu,power.draw,power.limit,utilization.gpu,memory.used,memory.total,clocks.sm,clocks.mem,fan.speed,pstate,name", false),
];

fn num(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() || s.starts_with('[') || s.eq_ignore_ascii_case("n/a") {
        return None;
    }
    s.parse().ok()
}

fn text(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty() && !s.starts_with('[') && !s.eq_ignore_ascii_case("n/a")).then(|| s.to_string())
}

/// Parse `nvidia-smi --query-gpu=<NVSMI_FIELDS> --format=csv,noheader,nounits` output.
pub fn nvidia_smi_csv(out: &str) -> Vec<Gpu> {
    nvidia_smi_csv_with(out, true)
}

/// Like [`nvidia_smi_csv`]; `reasons` says whether the throttle-reasons column is present.
pub fn nvidia_smi_csv_with(out: &str, reasons: bool) -> Vec<Gpu> {
    let ncols = if reasons { 13 } else { 12 };
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.splitn(ncols, ',').collect();
            if f.len() < ncols {
                return None;
            }
            Some(Gpu {
                idx: f[0].trim().parse().ok()?,
                temp_c: num(f[1]),
                power_w: num(f[2]),
                power_limit_w: num(f[3]).filter(|v| *v > 0.0),
                util_pct: num(f[4]),
                vram_used_mb: num(f[5]),
                vram_total_mb: num(f[6]),
                sm_mhz: num(f[7]),
                mem_mhz: num(f[8]),
                fan_pct: num(f[9]),
                pstate: text(f[10]),
                clk_event_reasons: if reasons { text(f[11]) } else { None },
                name: f[ncols - 1].trim().to_string(),
            })
        })
        .collect()
}

/// Delta of a counter that wraps at `range`; `None` when it went backwards and the range is
/// unknown (a reset or an unreadable `max_energy_range_uj`), so the caller skips the sample
/// instead of reporting a wrong value.
pub fn wrapping_delta_checked(prev: u64, cur: u64, range: u64) -> Option<u64> {
    if cur >= prev {
        Some(cur - prev)
    } else if range > prev {
        Some(range - prev + cur)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_busy_total() {
        let t = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4\n";
        assert_eq!(proc_stat_cpu(t), Some((150, 1000)));
    }

    #[test]
    fn meminfo_and_vmstat() {
        let m = meminfo("MemTotal:       131234 kB\nMemAvailable:    65617 kB\nHugePages_Total: 0\n");
        assert_eq!(m["MemTotal"], 131234);
        assert_eq!(m["HugePages_Total"], 0);
        let v = vmstat("pgmajfault 42\npswpin 7\n");
        assert_eq!(v["pgmajfault"], 42);
    }

    #[test]
    fn load() {
        assert_eq!(loadavg("1.50 0.75 0.25 2/900 1234"), Some((1.5, 0.75, 0.25)));
        assert_eq!(loadavg("garbage"), None);
    }

    #[test]
    fn disks_filtered() {
        let t = " 259 0 nvme0n1 10 0 2048 5 20 0 4096 9 0 77 14 0 0 0 0\n 253 0 dm-0 1 0 8 0 1 0 8 0 0 1 1\n";
        let d = diskstats(t, |n| !n.starts_with("dm-"));
        assert_eq!(d.len(), 1);
        assert_eq!(d["nvme0n1"], DiskCounters { read_bytes: 2048 * 512, write_bytes: 4096 * 512, io_ticks_ms: 77 });
    }

    #[test]
    fn netdev() {
        let t = "Inter-|   Receive\n face |bytes\n    lo: 100 1 0 0 0 0 0 0 100 1 0 0 0 0 0 0\n  eth0: 5000 10 0 0 0 0 0 0 7000 9 0 0 0 0 0 0\n";
        let n = net_dev(t, |n| n != "lo");
        assert_eq!(n.len(), 1);
        assert_eq!(n["eth0"], (5000, 7000));
    }

    #[test]
    fn wrap() {
        assert_eq!(wrapping_delta(10, 25, 0), 15);
        assert_eq!(wrapping_delta(90, 10, 100), 20);
        assert_eq!(wrapping_delta(u64::from(u32::MAX) - 4, 5, 0), 10);
    }

    #[test]
    fn wrap_checked() {
        assert_eq!(wrapping_delta_checked(10, 25, 0), Some(15));
        assert_eq!(wrapping_delta_checked(90, 10, 100), Some(20));
        assert_eq!(wrapping_delta_checked(90, 10, 0), None);
        assert_eq!(wrapping_delta_checked(90, 10, 50), None);
    }

    #[test]
    fn nvidia_csv() {
        let out = "0, 45, 120.50, 180.00, 97, 15000, 16311, 2800, 13801, 55, P2, 0x0000000000000000, NVIDIA GeForce RTX 5060 Ti\n\
                   1, 50, 6.36, [N/A], 0, 768, 8151, 180, 405, [N/A], P8, 0x0000000000000400, NVIDIA GeForce RTX 5070 Laptop GPU\n";
        let g = nvidia_smi_csv(out);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].power_w, Some(120.5));
        assert_eq!(g[0].name, "NVIDIA GeForce RTX 5060 Ti");
        assert_eq!(g[1].power_limit_w, None);
        assert_eq!(g[1].fan_pct, None);
        assert_eq!(g[1].pstate.as_deref(), Some("P8"));
        assert!(nvidia_smi_csv("NVIDIA-SMI has failed").is_empty());
    }

    #[test]
    fn nvidia_csv_edge_cases() {
        // error strings in numeric columns, commas in the name, a missing throttle column
        let out = "0, [Unknown Error], [Not Supported], [N/A], 5, 1, 2, 3, 4, [N/A], P0, 0x0, NVIDIA RTX, Super\n\
                   GPU has fallen off the bus.\n\n";
        let g = nvidia_smi_csv(out);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].temp_c, None);
        assert_eq!(g[0].power_w, None);
        assert_eq!(g[0].name, "NVIDIA RTX, Super");
        let g = nvidia_smi_csv_with("2, 40, 10, 100, 1, 1, 2, 3, 4, 20, P5, Some, Name", false);
        assert_eq!(g[0].idx, 2);
        assert_eq!(g[0].clk_event_reasons, None);
        assert_eq!(g[0].name, "Some, Name");
    }
}
