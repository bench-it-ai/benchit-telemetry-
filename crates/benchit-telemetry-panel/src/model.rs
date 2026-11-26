//! Panel state: the latest snapshot, sparkline history, persisted settings.
use benchit_telemetry_core::model::Snapshot;
use benchit_telemetry_core::panel::RecInfo;
use benchit_telemetry_core::{timefmt, Store};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// Points kept per sparkline.
pub const HIST: usize = 150;

/// Read-only view handed to the layout.
pub struct Model<'a> {
    pub snap: &'a Snapshot,
    pub rec: Option<&'a RecInfo>,
    /// Seconds since the snapshot was taken.
    pub age: f64,
    /// Local wall clock, e.g. `08:12:03 PST`.
    pub clock: String,
}

impl Model<'_> {
    pub fn stale(&self) -> bool {
        self.age > 10.0
    }

    pub fn uptime_text(&self) -> String {
        let Some(up) = self.snap.uptime_s else { return String::new() };
        let u = (up + self.age.clamp(0.0, 60.0)) as u64;
        let (d, h, m, s) = (u / 86400, (u % 86400) / 3600, (u % 3600) / 60, u % 60);
        if d > 0 {
            format!("T+ {d}d {h:02}:{m:02}:{s:02}")
        } else {
            format!("T+ {h:02}:{m:02}:{s:02}")
        }
    }
}

pub type Series = VecDeque<Option<f32>>;

#[derive(Default, Clone)]
pub struct Hist {
    pub cpu_temp: Series,
    pub mem_used: Series,
    pub total_w: Series,
    pub gpu_power: HashMap<u32, Series>,
    last_ts: i64,
}

fn push(s: &mut Series, v: Option<f64>) {
    if s.len() >= HIST {
        s.pop_front();
    }
    s.push_back(v.map(|x| x as f32));
}

impl Hist {
    /// Append a live snapshot, at most once per `min_gap` seconds (the store's sample interval),
    /// so live points and points seeded from the database share one time scale.
    pub fn observe(&mut self, s: &Snapshot, min_gap: f64) {
        if self.last_ts != 0 && (s.ts - self.last_ts) as f64 + 0.5 < min_gap {
            return;
        }
        self.last_ts = s.ts;
        push(&mut self.cpu_temp, s.cpu.temp_c);
        push(&mut self.mem_used, s.mem.used_pct);
        push(&mut self.total_w, s.est_total_w);
        for g in &s.gpus {
            push(self.gpu_power.entry(g.idx).or_default(), g.power_w);
        }
    }

    /// Seed from the newest stored rows. Silent if the database is missing or empty.
    pub fn seed(db: &Path) -> Self {
        let mut h = Hist::default();
        let Ok(store) = Store::open_readonly(db) else { return h };
        let now = timefmt::now();
        let fields: Vec<String> = ["cpu_temp_c", "mem_used_pct", "est_total_w"].map(String::from).to_vec();
        let Ok(rows) = store.history(now - 3 * 3600, now, &fields, 0, 100_000) else { return h };
        let skip = rows.len().saturating_sub(HIST);
        let f = |r: &serde_json::Map<String, serde_json::Value>, k: &str| r.get(k).and_then(|v| v.as_f64());
        for r in rows.iter().skip(skip) {
            push(&mut h.cpu_temp, f(r, "cpu_temp_c"));
            push(&mut h.mem_used, f(r, "mem_used_pct"));
            push(&mut h.total_w, f(r, "est_total_w"));
            h.last_ts = r.get("ts").and_then(|v| v.as_i64()).unwrap_or(h.last_ts);
        }
        if let Some(first) = rows.get(skip).and_then(|r| r.get("ts")).and_then(|v| v.as_i64()) {
            if let Ok(g) = store.gpu_history(first, now, None, 0, 1_000_000) {
                for r in g {
                    let idx = r.get("idx").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    push(h.gpu_power.entry(idx).or_default(), f(&r, "power_w"));
                }
            }
        }
        h
    }
}

/// Persisted window settings (`~/.config/benchit-panel.json`).
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub x: Option<f32>,
    pub y: Option<f32>,
    #[serde(default)]
    pub compact: bool,
}

impl Config {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("benchit-panel.json")
    }

    pub fn load_from(p: &Path) -> Self {
        std::fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save_to(&self, p: &Path) {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(b) = serde_json::to_vec(self) {
            let _ = std::fs::write(p, b);
        }
    }
}

/// `HH:MM:SS TZ` in local time.
pub fn local_clock() -> String {
    let mut buf = [0u8; 64];
    // SAFETY: `t` and `tm` are valid for the duration of the calls; `buf` is large enough and
    // `strftime` NUL-terminates within its length argument.
    unsafe {
        let t: libc::time_t = timefmt::now();
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), c"%H:%M:%S %Z".as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_respects_gap_and_cap() {
        let mut h = Hist::default();
        let mut s = Snapshot { ts: 1000, ..Default::default() };
        h.observe(&s, 10.0);
        s.ts = 1002;
        h.observe(&s, 10.0);
        assert_eq!(h.cpu_temp.len(), 1);
        s.ts = 1010;
        h.observe(&s, 10.0);
        assert_eq!(h.cpu_temp.len(), 2);
        for i in 0..400 {
            s.ts = 2000 + i * 10;
            h.observe(&s, 10.0);
        }
        assert_eq!(h.cpu_temp.len(), HIST);
    }

    #[test]
    fn config_roundtrip_and_legacy() {
        let d = std::env::temp_dir().join(format!("bt-panel-{}", std::process::id()));
        let p = d.join("c.json");
        let c = Config { x: Some(1.0), y: Some(2.0), compact: true };
        c.save_to(&p);
        assert_eq!(Config::load_from(&p), c);
        std::fs::write(&p, br#"{"x": 2180, "y": 915}"#).unwrap();
        let l = Config::load_from(&p);
        assert_eq!((l.x, l.compact), (Some(2180.0), false));
        assert_eq!(Config::load_from(&d.join("missing.json")), Config::default());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn seed_missing_db_is_empty() {
        assert!(Hist::seed(Path::new("/nonexistent/x.db")).cpu_temp.is_empty());
    }

    #[test]
    fn uptime_format() {
        let s = Snapshot { uptime_s: Some(90_061.0), ..Default::default() };
        let m = Model { snap: &s, rec: None, age: 0.0, clock: String::new() };
        assert_eq!(m.uptime_text(), "T+ 1d 01:01:01");
    }
}
