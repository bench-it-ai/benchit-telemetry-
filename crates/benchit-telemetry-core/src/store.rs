//! SQLite history store with a rolling retention window.
//!
//! The schema (version 1) is documented in the README. Tables: `samples` (one row per
//! sample), `gpu_samples` (one row per GPU per sample), `marks` (named benchmark windows),
//! `meta`, and the convenience view `samples_wide`.
use crate::model::Snapshot;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS samples(
  ts INTEGER NOT NULL, host TEXT NOT NULL,
  cpu_temp_c REAL, cpu_temp2_c REAL, cpu_temp3_c REAL, cpu_util_pct REAL, cpu_mhz REAL, load1 REAL,
  cpu_pkg_w REAL, platform_w REAL, gpu_total_w REAL, est_total_w REAL,
  mem_total_mb REAL, mem_avail_mb REAL, mem_used_pct REAL, swap_used_mb REAL,
  majflt_s REAL, swpin_s REAL, swpout_s REAL,
  root_used_gb REAL, root_total_gb REAL, root_used_pct REAL,
  home_used_gb REAL, home_total_gb REAL, home_used_pct REAL,
  disk_read_mbs REAL, disk_write_mbs REAL, disk_busy_pct REAL,
  net_rx_mbs REAL, net_tx_mbs REAL,
  bat_pct REAL, bat_w REAL, ac_online INTEGER,
  fans_json TEXT, top_proc TEXT, flags TEXT, uptime_s REAL,
  PRIMARY KEY(host, ts));
CREATE TABLE IF NOT EXISTS gpu_samples(
  ts INTEGER NOT NULL, host TEXT NOT NULL, idx INTEGER NOT NULL, name TEXT,
  temp_c REAL, power_w REAL, power_limit_w REAL, util_pct REAL,
  vram_used_mb REAL, vram_total_mb REAL, sm_mhz REAL, mem_mhz REAL, fan_pct REAL,
  pstate TEXT, clk_event_reasons TEXT,
  PRIMARY KEY(host, ts, idx));
CREATE TABLE IF NOT EXISTS marks(
  id INTEGER PRIMARY KEY AUTOINCREMENT, host TEXT, name TEXT NOT NULL,
  t_start REAL NOT NULL, t_end REAL, cmd TEXT, exit_code INTEGER, note TEXT);
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
CREATE INDEX IF NOT EXISTS samples_ts ON samples(ts);
CREATE INDEX IF NOT EXISTS gpu_samples_ts ON gpu_samples(ts);
DROP VIEW IF EXISTS samples_wide;
CREATE VIEW samples_wide AS
SELECT datetime(s.ts,'unixepoch','localtime') AS local_time, s.*,
  g0.temp_c AS gpu0_temp_c, g0.power_w AS gpu0_power_w, g0.util_pct AS gpu0_util_pct,
  g0.vram_used_mb AS gpu0_vram_used_mb, g0.sm_mhz AS gpu0_sm_mhz,
  g1.temp_c AS gpu1_temp_c, g1.power_w AS gpu1_power_w, g1.util_pct AS gpu1_util_pct,
  g1.vram_used_mb AS gpu1_vram_used_mb, g1.sm_mhz AS gpu1_sm_mhz
FROM samples s
LEFT JOIN gpu_samples g0 ON g0.host=s.host AND g0.ts=s.ts AND g0.idx=0
LEFT JOIN gpu_samples g1 ON g1.host=s.host AND g1.ts=s.ts AND g1.idx=1;
"#;

/// Numeric `samples` columns that may be requested through history/summary queries.
pub const SAMPLE_FIELDS: &[&str] = &[
    "cpu_temp_c",
    "cpu_temp2_c",
    "cpu_temp3_c",
    "cpu_util_pct",
    "cpu_mhz",
    "load1",
    "cpu_pkg_w",
    "platform_w",
    "gpu_total_w",
    "est_total_w",
    "mem_total_mb",
    "mem_avail_mb",
    "mem_used_pct",
    "swap_used_mb",
    "majflt_s",
    "swpin_s",
    "swpout_s",
    "root_used_gb",
    "root_total_gb",
    "root_used_pct",
    "home_used_gb",
    "home_total_gb",
    "home_used_pct",
    "disk_read_mbs",
    "disk_write_mbs",
    "disk_busy_pct",
    "net_rx_mbs",
    "net_tx_mbs",
    "bat_pct",
    "bat_w",
    "ac_online",
    "uptime_s",
];
/// Text `samples` columns (returned as-is, never averaged).
pub const SAMPLE_TEXT_FIELDS: &[&str] = &["host", "fans_json", "top_proc", "flags"];
/// Numeric `gpu_samples` columns.
pub const GPU_FIELDS: &[&str] = &[
    "temp_c",
    "power_w",
    "power_limit_w",
    "util_pct",
    "vram_used_mb",
    "vram_total_mb",
    "sm_mhz",
    "mem_mhz",
    "fan_pct",
];
/// Metrics included in [`Store::summary`] by default.
pub const SUMMARY_FIELDS: &[&str] = &[
    "cpu_temp_c",
    "cpu_util_pct",
    "cpu_mhz",
    "cpu_pkg_w",
    "platform_w",
    "gpu_total_w",
    "est_total_w",
    "mem_used_pct",
    "swap_used_mb",
    "disk_read_mbs",
    "disk_write_mbs",
    "disk_busy_pct",
    "net_rx_mbs",
    "net_tx_mbs",
];

pub struct Store {
    conn: Connection,
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Stats {
    pub field: String,
    pub n: usize,
    pub min: f64,
    pub avg: f64,
    pub p95: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub from: i64,
    pub to: i64,
    pub samples: usize,
    pub energy_wh: Option<f64>,
    pub metrics: Vec<Stats>,
    /// Per-GPU statistics keyed by index.
    pub gpus: Vec<(u32, Vec<Stats>)>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Mark {
    pub id: i64,
    pub host: Option<String>,
    pub name: String,
    pub t_start: f64,
    pub t_end: Option<f64>,
    pub cmd: Option<String>,
    pub exit_code: Option<i64>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub path: String,
    pub size_bytes: u64,
    pub samples: i64,
    pub gpu_samples: i64,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub schema_version: i64,
}

/// Validate a comma-separated field list against an allow-list.
pub fn validate_fields(requested: &str, allowed: &[&str]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for f in requested.split(',').map(str::trim).filter(|f| !f.is_empty()) {
        if !allowed.contains(&f) {
            bail!("unknown field {f:?}; allowed: {}", allowed.join(","));
        }
        out.push(f.to_string());
    }
    Ok(out)
}

fn stats(field: &str, mut v: Vec<f64>) -> Option<Stats> {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    let idx = ((0.95 * (n - 1) as f64).round() as usize).min(n - 1);
    Some(Stats { field: field.into(), n, min: v[0], avg: v.iter().sum::<f64>() / n as f64, p95: v[idx], max: v[n - 1] })
}

fn row_to_map(row: &rusqlite::Row, cols: &[String]) -> rusqlite::Result<Map<String, Value>> {
    let mut m = Map::new();
    for (i, c) in cols.iter().enumerate() {
        let v = match row.get_ref(i)? {
            rusqlite::types::ValueRef::Null => Value::Null,
            rusqlite::types::ValueRef::Integer(x) => Value::from(x),
            rusqlite::types::ValueRef::Real(x) => {
                serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
            }
            rusqlite::types::ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).to_string()),
            rusqlite::types::ValueRef::Blob(_) => Value::Null,
        };
        m.insert(c.clone(), v);
    }
    Ok(m)
}

impl Store {
    /// Open (creating if needed) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let new = !path.exists();
        let conn = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        if new {
            conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        }
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        conn.execute("INSERT OR REPLACE INTO meta VALUES('schema_version', ?1)", params![SCHEMA_VERSION.to_string()])?;
        Ok(Self { conn, path })
    }

    /// Open read-only (for API/CLI queries while the daemon writes).
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {} (is the daemon running?)", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        Ok(Self { conn, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO meta VALUES(?1, ?2)", params![key, value])?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional()?)
    }

    /// Insert one snapshot (replacing any row with the same host and second).
    pub fn insert(&mut self, s: &Snapshot) -> Result<()> {
        let tx = self.conn.transaction()?;
        let root = s.fs("root");
        let home = s.fs("home");
        let fans = (!s.fans.is_empty()).then(|| serde_json::to_string(&s.fans)).transpose()?;
        let flags = (!s.flags.is_empty()).then(|| s.flags.join(" "));
        let bat = s.battery.as_ref();
        tx.execute(
            "INSERT OR REPLACE INTO samples VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,\
             ?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32,?33,?34,?35,?36,?37)",
            rusqlite::params![
                s.ts,
                s.host,
                s.cpu.temp_c,
                s.cpu.temp2_c,
                s.cpu.temp3_c,
                s.cpu.util_pct,
                s.cpu.mhz,
                s.cpu.load1,
                s.cpu.pkg_w,
                s.cpu.platform_w,
                s.gpu_total_w,
                s.est_total_w,
                s.mem.total_mb,
                s.mem.avail_mb,
                s.mem.used_pct,
                s.mem.swap_used_mb,
                s.mem.majflt_s,
                s.mem.swpin_s,
                s.mem.swpout_s,
                root.map(|f| f.used_gb),
                root.map(|f| f.total_gb),
                root.map(|f| f.used_pct),
                home.map(|f| f.used_gb),
                home.map(|f| f.total_gb),
                home.map(|f| f.used_pct),
                s.disk.read_mbs,
                s.disk.write_mbs,
                s.disk.busy_pct,
                s.net.rx_mbs,
                s.net.tx_mbs,
                bat.and_then(|b| b.pct),
                bat.and_then(|b| b.watts),
                bat.and_then(|b| b.ac_online.map(i64::from)),
                fans,
                s.top_proc,
                flags,
                s.uptime_s,
            ],
        )?;
        for g in &s.gpus {
            tx.execute(
                "INSERT OR REPLACE INTO gpu_samples VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                rusqlite::params![
                    s.ts,
                    s.host,
                    g.idx,
                    g.name,
                    g.temp_c,
                    g.power_w,
                    g.power_limit_w,
                    g.util_pct,
                    g.vram_used_mb,
                    g.vram_total_mb,
                    g.sm_mhz,
                    g.mem_mhz,
                    g.fan_pct,
                    g.pstate,
                    g.clk_event_reasons,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete everything older than `retain_days`; returns the number of sample rows removed.
    pub fn prune(&self, retain_days: u32, now: i64) -> Result<usize> {
        let cut = now - i64::from(retain_days) * 86400;
        let n = self.conn.execute("DELETE FROM samples WHERE ts < ?1", [cut])?;
        self.conn.execute("DELETE FROM gpu_samples WHERE ts < ?1", [cut])?;
        self.conn.execute("DELETE FROM marks WHERE t_start < ?1", [cut as f64])?;
        self.conn.execute_batch("PRAGMA incremental_vacuum; PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(n)
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT count(*) FROM samples", [], |r| r.get(0))?)
    }

    pub fn status(&self) -> Result<Status> {
        let (samples, first_ts, last_ts) =
            self.conn.query_row("SELECT count(*), min(ts), max(ts) FROM samples", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
        let gpu_samples = self.conn.query_row("SELECT count(*) FROM gpu_samples", [], |r| r.get(0))?;
        let schema_version = self.meta("schema_version")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        Ok(Status {
            path: self.path.display().to_string(),
            size_bytes: std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0),
            samples,
            gpu_samples,
            first_ts,
            last_ts,
            schema_version,
        })
    }

    /// Resolve a time expression: unix seconds, a relative duration (`-15m`, `2h` ago), `now`,
    /// or a local timestamp such as `2026-01-31 08:00` / `2026-01-31T08:00:00`.
    pub fn resolve_time(&self, expr: &str, now: i64) -> Result<i64> {
        let e = expr.trim();
        if e.is_empty() || e == "now" {
            return Ok(now);
        }
        if let Ok(v) = e.parse::<f64>() {
            if v > 1e8 {
                return Ok(v as i64);
            }
        }
        let rel = e.strip_prefix('-').unwrap_or(e);
        if let Some(d) = crate::timefmt::parse_duration(rel) {
            if rel.chars().last().is_some_and(|c| c.is_ascii_alphabetic()) || e.starts_with('-') {
                return Ok(now - d as i64);
            }
        }
        let v: Option<i64> =
            self.conn
                .query_row("SELECT CAST(strftime('%s', ?1, 'utc') AS INTEGER)", [e.replace('T', " ")], |r| r.get(0))?;
        v.with_context(|| format!("cannot parse time {expr:?} (use unix seconds, -15m, or 'YYYY-MM-DD HH:MM')"))
    }

    /// Format unix seconds as local `YYYY-MM-DD HH:MM:SS` (uses SQLite's localtime).
    pub fn local_time(&self, ts: f64) -> String {
        self.conn
            .query_row("SELECT datetime(?1, 'unixepoch', 'localtime')", [ts], |r| r.get::<_, String>(0))
            .unwrap_or_else(|_| ts.to_string())
    }

    /// Rows from `samples` between `from` and `to` (inclusive). With `step` > 0, numeric fields
    /// are averaged into buckets of `step` seconds. `fields` empty = all numeric + text fields.
    pub fn history(
        &self,
        from: i64,
        to: i64,
        fields: &[String],
        step: i64,
        limit: usize,
    ) -> Result<Vec<Map<String, Value>>> {
        let all: Vec<String> = SAMPLE_FIELDS.iter().chain(SAMPLE_TEXT_FIELDS).map(|s| s.to_string()).collect();
        let fields: Vec<String> = if fields.is_empty() { all } else { fields.to_vec() };
        for f in &fields {
            if !SAMPLE_FIELDS.contains(&f.as_str()) && !SAMPLE_TEXT_FIELDS.contains(&f.as_str()) {
                bail!("unknown field {f:?}");
            }
        }
        let (sql, cols) = if step > 0 {
            let sel: Vec<String> = fields
                .iter()
                .map(|f| {
                    if SAMPLE_FIELDS.contains(&f.as_str()) {
                        format!("avg({f}) AS {f}")
                    } else {
                        format!("max({f}) AS {f}")
                    }
                })
                .collect();
            (
                format!(
                    "SELECT (ts/{step})*{step} AS bucket, count(*) AS n, {} FROM samples WHERE ts BETWEEN ?1 AND ?2 \
                     GROUP BY bucket ORDER BY bucket LIMIT ?3",
                    sel.join(", ")
                ),
                ["ts".to_string(), "n".to_string()].into_iter().chain(fields.iter().cloned()).collect::<Vec<_>>(),
            )
        } else {
            (
                format!(
                    "SELECT ts, {} FROM samples WHERE ts BETWEEN ?1 AND ?2 ORDER BY ts LIMIT ?3",
                    fields.join(", ")
                ),
                std::iter::once("ts".to_string()).chain(fields.iter().cloned()).collect(),
            )
        };
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![from, to, limit as i64], |r| row_to_map(r, &cols))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Rows from `gpu_samples`, optionally for one GPU and bucketed like [`Store::history`].
    pub fn gpu_history(
        &self,
        from: i64,
        to: i64,
        idx: Option<u32>,
        step: i64,
        limit: usize,
    ) -> Result<Vec<Map<String, Value>>> {
        let idx_filter = idx.map(|i| format!(" AND idx = {i}")).unwrap_or_default();
        let (sql, cols): (String, Vec<String>) = if step > 0 {
            let sel: Vec<String> = GPU_FIELDS.iter().map(|f| format!("avg({f}) AS {f}")).collect();
            (
                format!(
                    "SELECT (ts/{step})*{step} AS bucket, idx, max(name), count(*), {} FROM gpu_samples \
                     WHERE ts BETWEEN ?1 AND ?2{idx_filter} GROUP BY bucket, idx ORDER BY bucket, idx LIMIT ?3",
                    sel.join(", ")
                ),
                ["ts", "idx", "name", "n"]
                    .iter()
                    .map(|s| s.to_string())
                    .chain(GPU_FIELDS.iter().map(|s| s.to_string()))
                    .collect(),
            )
        } else {
            (
                format!(
                    "SELECT ts, idx, name, {}, pstate, clk_event_reasons FROM gpu_samples \
                     WHERE ts BETWEEN ?1 AND ?2{idx_filter} ORDER BY ts, idx LIMIT ?3",
                    GPU_FIELDS.join(", ")
                ),
                ["ts", "idx", "name"]
                    .iter()
                    .map(|s| s.to_string())
                    .chain(GPU_FIELDS.iter().map(|s| s.to_string()))
                    .chain(["pstate", "clk_event_reasons"].iter().map(|s| s.to_string()))
                    .collect(),
            )
        };
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![from, to, limit as i64], |r| row_to_map(r, &cols))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// min/avg/p95/max per metric plus an energy estimate (trapezoidal integral of `est_total_w`).
    pub fn summary(&self, from: i64, to: i64) -> Result<Summary> {
        let mut metrics = Vec::new();
        let mut samples = 0usize;
        let mut st = self.conn.prepare(&format!(
            "SELECT ts, {} FROM samples WHERE ts BETWEEN ?1 AND ?2 ORDER BY ts",
            SUMMARY_FIELDS.join(", ")
        ))?;
        let mut cols: Vec<Vec<f64>> = vec![Vec::new(); SUMMARY_FIELDS.len()];
        let mut power: Vec<(i64, f64)> = Vec::new();
        let est_idx = SUMMARY_FIELDS.iter().position(|f| *f == "est_total_w").unwrap_or(0);
        let mut rows = st.query(params![from, to])?;
        while let Some(r) = rows.next()? {
            samples += 1;
            let ts: i64 = r.get(0)?;
            for (i, col) in cols.iter_mut().enumerate() {
                if let Some(v) = r.get::<_, Option<f64>>(i + 1)? {
                    col.push(v);
                    if i == est_idx {
                        power.push((ts, v));
                    }
                }
            }
        }
        for (i, col) in cols.into_iter().enumerate() {
            if let Some(s) = stats(SUMMARY_FIELDS[i], col) {
                metrics.push(s);
            }
        }
        let energy_wh = (power.len() > 1).then(|| {
            power.windows(2).map(|w| (w[1].0 - w[0].0) as f64 * (w[0].1 + w[1].1) / 2.0).sum::<f64>() / 3600.0
        });
        let mut gpus = Vec::new();
        let idxs: Vec<u32> = {
            let mut q =
                self.conn.prepare("SELECT DISTINCT idx FROM gpu_samples WHERE ts BETWEEN ?1 AND ?2 ORDER BY idx")?;
            let v = q.query_map(params![from, to], |r| r.get(0))?.collect::<rusqlite::Result<Vec<u32>>>()?;
            v
        };
        for idx in idxs {
            let mut q = self.conn.prepare(&format!(
                "SELECT {} FROM gpu_samples WHERE idx=?1 AND ts BETWEEN ?2 AND ?3",
                GPU_FIELDS.join(", ")
            ))?;
            let mut cols: Vec<Vec<f64>> = vec![Vec::new(); GPU_FIELDS.len()];
            let mut rows = q.query(params![idx, from, to])?;
            while let Some(r) = rows.next()? {
                for (i, col) in cols.iter_mut().enumerate() {
                    if let Some(v) = r.get::<_, Option<f64>>(i)? {
                        col.push(v);
                    }
                }
            }
            let st: Vec<Stats> = cols.into_iter().enumerate().filter_map(|(i, c)| stats(GPU_FIELDS[i], c)).collect();
            gpus.push((idx, st));
        }
        Ok(Summary { from, to, samples, energy_wh, metrics, gpus })
    }

    pub fn mark_start(&self, host: &str, name: &str, t: f64, cmd: Option<&str>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO marks(host, name, t_start, cmd) VALUES(?1, ?2, ?3, ?4)",
            params![host, name, t, cmd],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Close the newest open mark with `name`; returns its id if one was open.
    pub fn mark_end(&self, name: &str, t: f64, exit_code: Option<i32>) -> Result<Option<i64>> {
        let id: Option<i64> = self
            .conn
            .query_row("SELECT max(id) FROM marks WHERE name=?1 AND t_end IS NULL", [name], |r| r.get(0))
            .optional()?
            .flatten();
        if let Some(id) = id {
            self.mark_end_id(id, t, exit_code)?;
        }
        Ok(id)
    }

    pub fn mark_end_id(&self, id: i64, t: f64, exit_code: Option<i32>) -> Result<()> {
        self.conn.execute("UPDATE marks SET t_end=?1, exit_code=?2 WHERE id=?3", params![t, exit_code, id])?;
        Ok(())
    }

    pub fn marks(&self, limit: usize) -> Result<Vec<Mark>> {
        let mut st = self.conn.prepare(
            "SELECT id, host, name, t_start, t_end, cmd, exit_code, note FROM marks ORDER BY id DESC LIMIT ?1",
        )?;
        let v = st
            .query_map([limit as i64], |r| {
                Ok(Mark {
                    id: r.get(0)?,
                    host: r.get(1)?,
                    name: r.get(2)?,
                    t_start: r.get(3)?,
                    t_end: r.get(4)?,
                    cmd: r.get(5)?,
                    exit_code: r.get(6)?,
                    note: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn mark_by_name(&self, name: &str) -> Result<Option<Mark>> {
        Ok(self.marks(10_000)?.into_iter().find(|m| m.name == name))
    }

    /// Write `samples_wide` rows between `from` and `to` as CSV.
    pub fn export_csv(&self, from: i64, to: i64, out: &mut dyn std::io::Write) -> Result<usize> {
        let mut st = self.conn.prepare("SELECT * FROM samples_wide WHERE ts BETWEEN ?1 AND ?2 ORDER BY ts")?;
        let cols: Vec<String> = st.column_names().map(|s| s.to_string()).collect();
        writeln!(out, "{}", cols.join(","))?;
        let mut rows = st.query(params![from, to])?;
        let mut n = 0;
        while let Some(r) = rows.next()? {
            let mut cells = Vec::with_capacity(cols.len());
            for i in 0..cols.len() {
                cells.push(match r.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => String::new(),
                    rusqlite::types::ValueRef::Integer(x) => x.to_string(),
                    rusqlite::types::ValueRef::Real(x) => x.to_string(),
                    rusqlite::types::ValueRef::Text(t) => csv_escape(&String::from_utf8_lossy(t)),
                    rusqlite::types::ValueRef::Blob(_) => String::new(),
                });
            }
            writeln!(out, "{}", cells.join(","))?;
            n += 1;
        }
        Ok(n)
    }
}

fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn snap(ts: i64, w: f64) -> Snapshot {
        Snapshot {
            ts,
            host: "test".into(),
            cpu: Cpu { temp_c: Some(50.0 + w / 10.0), util_pct: Some(w), pkg_w: Some(w), ..Default::default() },
            gpus: vec![Gpu {
                idx: 0,
                name: "GPU A".into(),
                power_w: Some(w),
                temp_c: Some(40.0),
                ..Default::default()
            }],
            gpu_total_w: Some(w),
            est_total_w: Some(2.0 * w),
            fans: vec![Fan { name: "cpu".into(), rpm: 900 }],
            flags: vec!["WARN_X".into()],
            ..Default::default()
        }
    }

    #[test]
    fn roundtrip_history_summary_prune() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(d.path().join("m.db")).unwrap();
        for i in 0..10 {
            s.insert(&snap(1_000_000 + i * 10, i as f64 * 10.0)).unwrap();
        }
        assert_eq!(s.count().unwrap(), 10);
        let h = s.history(1_000_000, 1_000_100, &["cpu_util_pct".into(), "flags".into()], 0, 100).unwrap();
        assert_eq!(h.len(), 10);
        assert_eq!(h[1]["cpu_util_pct"], serde_json::json!(10.0));
        assert_eq!(h[1]["flags"], serde_json::json!("WARN_X"));
        let b = s.history(1_000_000, 1_000_100, &["cpu_util_pct".into()], 50, 100).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0]["n"], serde_json::json!(5));
        let g = s.gpu_history(1_000_000, 1_000_100, Some(0), 0, 100).unwrap();
        assert_eq!(g.len(), 10);
        let sm = s.summary(1_000_000, 1_000_090).unwrap();
        assert_eq!(sm.samples, 10);
        let u = sm.metrics.iter().find(|m| m.field == "cpu_util_pct").unwrap();
        assert_eq!((u.min, u.max, u.avg), (0.0, 90.0, 45.0));
        // est_total_w = 2*w, w from 0..90 over 90 s => avg 90 W * 90 s = 2.25 Wh
        assert!((sm.energy_wh.unwrap() - 2.25).abs() < 1e-9);
        assert_eq!(sm.gpus.len(), 1);
        let removed = s.prune(30, 1_000_050 + 30 * 86400).unwrap();
        assert_eq!(removed, 5);
        assert_eq!(s.count().unwrap(), 5);
    }

    #[test]
    fn rejects_unknown_fields() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("m.db")).unwrap();
        assert!(s.history(0, 1, &["ts; DROP TABLE samples".into()], 0, 10).is_err());
        assert!(validate_fields("cpu_util_pct,bogus", SAMPLE_FIELDS).is_err());
        assert_eq!(validate_fields("cpu_util_pct, load1", SAMPLE_FIELDS).unwrap(), vec!["cpu_util_pct", "load1"]);
    }

    #[test]
    fn marks_and_time() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("m.db")).unwrap();
        let id = s.mark_start("test", "bench", 100.0, Some("echo hi")).unwrap();
        assert_eq!(s.mark_end("bench", 200.0, Some(0)).unwrap(), Some(id));
        let m = s.mark_by_name("bench").unwrap().unwrap();
        assert_eq!(m.t_end, Some(200.0));
        assert_eq!(s.resolve_time("-15m", 10_000).unwrap(), 10_000 - 900);
        assert_eq!(s.resolve_time("2h", 10_000).unwrap(), 10_000 - 7200);
        assert_eq!(s.resolve_time("1790000000", 0).unwrap(), 1_790_000_000);
        assert!(s.resolve_time("2026-01-31 08:00", 0).unwrap() > 1_700_000_000);
        assert!(s.resolve_time("not a time", 0).is_err());
    }

    #[test]
    fn csv_export() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(d.path().join("m.db")).unwrap();
        s.insert(&snap(1_000_000, 5.0)).unwrap();
        let mut buf = Vec::new();
        assert_eq!(s.export_csv(0, 2_000_000, &mut buf).unwrap(), 1);
        let text = String::from_utf8(buf).unwrap();
        assert!(text.starts_with("local_time,ts,host,"));
        assert!(text.contains("\"[{\"\"name\"\":\"\"cpu\"\",\"\"rpm\"\":900}]\""));
    }
}
