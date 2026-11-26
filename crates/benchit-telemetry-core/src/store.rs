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
use std::io::Write;
use std::path::{Path, PathBuf};

/// Current schema version, stored in `PRAGMA user_version`.
/// v1: initial. v2: `marks(name, id)` index and a four-GPU `samples_wide` view.
pub const SCHEMA_VERSION: i64 = 2;

/// A caller mistake (bad time expression, unknown field, ...): reported as HTTP 400.
#[derive(Debug)]
pub struct BadRequest(pub String);
impl std::fmt::Display for BadRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for BadRequest {}

/// The database is missing or not readable yet: reported as HTTP 503.
#[derive(Debug)]
pub struct Unavailable(pub String);
impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Unavailable {}

fn bad(msg: impl Into<String>) -> anyhow::Error {
    BadRequest(msg.into()).into()
}

const TABLES: &str = r#"
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
CREATE INDEX IF NOT EXISTS marks_name ON marks(name, id);
"#;

/// Number of GPUs exposed as `gpuN_*` columns in `samples_wide`.
pub const WIDE_GPUS: usize = 4;

fn wide_view_sql() -> String {
    let mut cols = String::new();
    let mut joins = String::new();
    for i in 0..WIDE_GPUS {
        cols.push_str(&format!(
            ",\n  g{i}.temp_c AS gpu{i}_temp_c, g{i}.power_w AS gpu{i}_power_w, g{i}.util_pct AS gpu{i}_util_pct, \
             g{i}.vram_used_mb AS gpu{i}_vram_used_mb, g{i}.sm_mhz AS gpu{i}_sm_mhz"
        ));
        joins.push_str(&format!("\nLEFT JOIN gpu_samples g{i} ON g{i}.host=s.host AND g{i}.ts=s.ts AND g{i}.idx={i}"));
    }
    format!(
        "DROP VIEW IF EXISTS samples_wide;\nCREATE VIEW samples_wide AS\n\
         SELECT datetime(s.ts,'unixepoch','localtime') AS local_time, s.*{cols}\nFROM samples s{joins};"
    )
}

/// Create or migrate the schema. Runs only when `user_version` is not current.
fn init_schema(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        bail!(
            "database schema v{version} was written by a newer benchit-telemetry (this build understands up to v{SCHEMA_VERSION}); \
             refusing to touch it"
        );
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    // 0 = fresh database, or a v1 database from before user_version was used; 1 = v1.
    // CREATE ... IF NOT EXISTS makes the same script both "create" and "migrate v1 -> v2".
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(TABLES)?;
    tx.execute_batch(&wide_view_sql())?;
    tx.execute("INSERT OR REPLACE INTO meta VALUES('schema_version', ?1)", params![SCHEMA_VERSION.to_string()])?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}

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

/// A bare read-write SQLite connection (test helper; bypasses schema checks).
#[doc(hidden)]
pub fn raw_connection(path: &Path) -> Result<Connection> {
    Ok(Connection::open(path)?)
}

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
    /// Estimated energy in Wh: trapezoid integral of `est_total_w` (GPU board power + CPU package
    /// power when readable). Gaps longer than 3 sample intervals are skipped, not interpolated.
    pub energy_est_wh: Option<f64>,
    /// Which sources fed `energy_est_wh`: `gpu` and/or `cpu_pkg`.
    pub energy_sources: Vec<String>,
    /// Seconds of the window actually covered by the energy integral.
    pub coverage_s: i64,
    /// Same integral over `platform_w` (whole-platform RAPL "psys"), where the machine has it.
    pub platform_energy_est_wh: Option<f64>,
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
            return Err(bad(format!("unknown field {f:?}; allowed: {}", allowed.join(","))));
        }
        out.push(f.to_string());
    }
    Ok(out)
}

/// Trapezoid integral (Wh) of a `(ts, watts)` series, skipping spans longer than `max_gap` seconds.
/// Returns `(energy, seconds covered)`; `None` when no span qualifies.
fn integrate(series: &[(i64, f64)], max_gap: i64) -> (Option<f64>, i64) {
    let (mut ws, mut cov) = (0.0, 0i64);
    for w in series.windows(2) {
        let dt = w[1].0 - w[0].0;
        if dt > 0 && dt <= max_gap {
            ws += dt as f64 * (w[0].1 + w[1].1) / 2.0;
            cov += dt;
        }
    }
    ((cov > 0).then_some(ws / 3600.0), cov)
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
    /// Open (creating or migrating if needed) the database at `path`. Schema work happens only
    /// when `user_version` differs from [`SCHEMA_VERSION`], so routine opens (marks, the API)
    /// take no write lock for DDL.
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
        init_schema(&conn)?;
        // the history is private to the user, including databases created by older versions
        for suffix in ["", "-wal", "-shm"] {
            let mut p = path.clone().into_os_string();
            p.push(suffix);
            restrict_file(Path::new(&p));
        }
        Ok(Self { conn, path })
    }

    /// A private in-memory database with the current schema. Used to turn a live [`Snapshot`]
    /// into a row with exactly the columns of an on-disk export (see `benchit-panel` exports).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        init_schema(&conn)?;
        Ok(Self { conn, path: PathBuf::from(":memory:") })
    }

    /// Open read-only (for API/CLI queries while the daemon writes).
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(Unavailable(format!("no database at {} (is the daemon running?)", path.display())).into());
        }
        let conn = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| Unavailable(format!("opening {}: {e}", path.display())))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            bail!("database schema v{version} was written by a newer benchit-telemetry (this build understands up to v{SCHEMA_VERSION})");
        }
        if version < SCHEMA_VERSION {
            return Err(Unavailable(
                "database schema is older than this build; start the daemon (or any writing command) once to migrate it".into(),
            )
            .into());
        }
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

    /// Delete samples older than `retain_days`; returns the number of sample rows removed.
    /// Benchmark marks are tiny and are the benchmark record, so they are kept (see [`Store::prune_marks`]).
    pub fn prune(&self, retain_days: u32, now: i64) -> Result<usize> {
        let cut = now - i64::from(retain_days) * 86400;
        let n = self.conn.execute("DELETE FROM samples WHERE ts < ?1", [cut])?;
        self.conn.execute("DELETE FROM gpu_samples WHERE ts < ?1", [cut])?;
        self.conn.execute_batch("PRAGMA incremental_vacuum; PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(n)
    }

    /// Delete marks that started more than `days` ago (opt-in; `prune` never does this).
    pub fn prune_marks(&self, days: u32, now: i64) -> Result<usize> {
        let cut = (now - i64::from(days) * 86400) as f64;
        Ok(self.conn.execute("DELETE FROM marks WHERE t_start < ?1", [cut])?)
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
        let schema_version = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
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

    /// Resolve a time expression (see [`parse_time`] for the grammar); only timestamp strings
    /// go through SQLite, for local-time conversion.
    pub fn resolve_time(&self, expr: &str, now: i64) -> Result<i64> {
        match parse_time(expr, now)? {
            Parsed::At(t) => Ok(t),
            Parsed::DateTime(dt) => {
                let v: Option<i64> =
                    self.conn
                        .query_row("SELECT CAST(strftime('%s', ?1, 'utc') AS INTEGER)", [dt.as_str()], |r| r.get(0))?;
                v.ok_or_else(|| bad(format!("cannot parse time {expr:?}")))
            }
        }
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
                return Err(bad(format!("unknown field {f:?}")));
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

    /// min/avg/p95/max per metric plus an energy estimate (trapezoidal integral of `est_total_w`,
    /// skipping gaps longer than three sample intervals).
    pub fn summary(&self, from: i64, to: i64) -> Result<Summary> {
        let mut metrics = Vec::new();
        let mut samples = 0usize;
        let mut st = self.conn.prepare(&format!(
            "SELECT ts, {} FROM samples WHERE ts BETWEEN ?1 AND ?2 ORDER BY ts",
            SUMMARY_FIELDS.join(", ")
        ))?;
        let interval: f64 =
            self.meta("sample_interval_s")?.and_then(|v| v.parse().ok()).filter(|v: &f64| *v > 0.0).unwrap_or(10.0);
        let max_gap = (3.0 * interval) as i64;
        let pos = |name: &str| SUMMARY_FIELDS.iter().position(|f| *f == name);
        let (est_i, plat_i, gpu_i, pkg_i) =
            (pos("est_total_w"), pos("platform_w"), pos("gpu_total_w"), pos("cpu_pkg_w"));
        let mut cols: Vec<Vec<f64>> = vec![Vec::new(); SUMMARY_FIELDS.len()];
        // (ts, watts) series for the integrals
        let mut est: Vec<(i64, f64)> = Vec::new();
        let mut plat: Vec<(i64, f64)> = Vec::new();
        let mut rows = st.query(params![from, to])?;
        while let Some(r) = rows.next()? {
            samples += 1;
            let ts: i64 = r.get(0)?;
            for (i, col) in cols.iter_mut().enumerate() {
                if let Some(v) = r.get::<_, Option<f64>>(i + 1)? {
                    col.push(v);
                    if Some(i) == est_i {
                        est.push((ts, v));
                    }
                    if Some(i) == plat_i {
                        plat.push((ts, v));
                    }
                }
            }
        }
        let has = |i: Option<usize>| i.is_some_and(|i| !cols[i].is_empty());
        let mut energy_sources = Vec::new();
        if has(gpu_i) {
            energy_sources.push("gpu".to_string());
        }
        if has(pkg_i) {
            energy_sources.push("cpu_pkg".to_string());
        }
        for (i, col) in cols.into_iter().enumerate() {
            if let Some(s) = stats(SUMMARY_FIELDS[i], col) {
                metrics.push(s);
            }
        }
        let (energy, coverage_s) = integrate(&est, max_gap);
        let (plat_energy, _) = integrate(&plat, max_gap);
        let energy_est_wh = energy;
        let platform_energy_est_wh = plat_energy;
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
        Ok(Summary {
            from,
            to,
            samples,
            energy_est_wh,
            energy_sources,
            coverage_s,
            platform_energy_est_wh,
            metrics,
            gpus,
        })
    }

    pub fn mark_start(&self, host: &str, name: &str, t: f64, cmd: Option<&str>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO marks(host, name, t_start, cmd) VALUES(?1, ?2, ?3, ?4)",
            params![host, name, t, cmd],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Close the newest open mark named `name` (on `host` when given); returns its id if one was open.
    pub fn mark_end(&self, host: Option<&str>, name: &str, t: f64, exit_code: Option<i32>) -> Result<Option<i64>> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT max(id) FROM marks WHERE name=?1 AND t_end IS NULL AND (?2 IS NULL OR host=?2)",
                params![name, host],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(id) = id {
            self.mark_end_id(id, t, exit_code, None)?;
        }
        Ok(id)
    }

    /// Close mark `id`. Returns `false` (and changes nothing) if it does not exist or is already
    /// closed; use [`Store::mark_exists`] to tell the two apart.
    pub fn mark_end_id(&self, id: i64, t: f64, exit_code: Option<i32>, note: Option<&str>) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE marks SET t_end=?1, exit_code=?2, note=?4 WHERE id=?3 AND t_end IS NULL",
            params![t, exit_code, id, note],
        )?;
        Ok(n == 1)
    }

    pub fn mark_exists(&self, id: i64) -> Result<bool> {
        Ok(self.conn.query_row("SELECT count(*) FROM marks WHERE id=?1", [id], |r| r.get::<_, i64>(0))? > 0)
    }

    pub fn marks(&self, limit: usize) -> Result<Vec<Mark>> {
        self.marks_where("1=1", params![], limit)
    }

    fn marks_where(&self, cond: &str, args: &[&dyn rusqlite::ToSql], limit: usize) -> Result<Vec<Mark>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT id, host, name, t_start, t_end, cmd, exit_code, note FROM marks WHERE {cond} ORDER BY id DESC LIMIT {}",
            limit as i64
        ))?;
        let v = st
            .query_map(args, |r| {
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

    /// Newest mark with this name (any host).
    pub fn mark_by_name(&self, name: &str) -> Result<Option<Mark>> {
        Ok(self.marks_where("name=?1", params![name], 1)?.into_iter().next())
    }

    pub fn mark_by_id(&self, id: i64) -> Result<Option<Mark>> {
        Ok(self.marks_where("id=?1", params![id], 1)?.into_iter().next())
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

    /// Write `samples_wide` rows in `from..=to` as a pretty JSON array of objects.
    pub fn export_json(&self, from: i64, to: i64, out: &mut dyn std::io::Write) -> Result<usize> {
        let mut st = self.conn.prepare("SELECT * FROM samples_wide WHERE ts BETWEEN ?1 AND ?2 ORDER BY ts")?;
        let cols: Vec<String> = st.column_names().map(|s| s.to_string()).collect();
        let mut rows = st.query(params![from, to])?;
        let mut arr = Vec::new();
        while let Some(r) = rows.next()? {
            let mut obj = Map::new();
            for (i, name) in cols.iter().enumerate() {
                let v = match r.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(x) => Value::from(x),
                    rusqlite::types::ValueRef::Real(x) => Value::from(x),
                    rusqlite::types::ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
                    rusqlite::types::ValueRef::Blob(_) => Value::Null,
                };
                obj.insert(name.clone(), v);
            }
            arr.push(Value::Object(obj));
        }
        let n = arr.len();
        serde_json::to_writer_pretty(&mut *out, &arr)?;
        writeln!(out)?;
        Ok(n)
    }
}

/// Write a CSV or JSON export of `from..=to` under `<data_dir>/exports/`.
///
/// The file is named `telemetry-<tag>-<UTC timestamp>.{csv,json}` and created with mode 0600.
pub fn export_to_dir(
    store: &Store,
    data_dir: &Path,
    from: i64,
    to: i64,
    tag: &str,
    kind: crate::prefs::ExportKind,
    now: i64,
) -> Result<PathBuf> {
    let dir = data_dir.join(crate::paths::EXPORTS_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("telemetry-{tag}-{}.{}", crate::timefmt::compact_utc(now), kind.ext()));
    let mut f = std::io::BufWriter::new(std::fs::File::create(&path)?);
    restrict_file(&path);
    match kind {
        crate::prefs::ExportKind::Csv => store.export_csv(from, to, &mut f)?,
        crate::prefs::ExportKind::Json => store.export_json(from, to, &mut f)?,
    };
    f.flush().ok();
    Ok(path)
}

/// Quote a text cell for CSV, and neutralise spreadsheet formula injection (`=`, `+`, `-`, `@`,
/// tab or CR at the start of text such as `top_proc` or a mark's command line).
fn csv_escape(s: &str) -> String {
    let mut cell = s.to_string();
    if cell.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        cell.insert(0, '\'');
    }
    if cell.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", cell.replace('"', "\"\""))
    } else {
        cell
    }
}

/// Make a freshly created data file private (0600).
fn restrict_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// Result of [`parse_time`].
#[derive(Debug, PartialEq)]
pub enum Parsed {
    /// Resolved to unix seconds.
    At(i64),
    /// A validated `YYYY-MM-DD[ T]HH:MM[:SS][Z|±HH:MM]` string that SQLite should convert to UTC
    /// (local time unless it carries an explicit zone).
    DateTime(String),
}

const MAX_SPAN_S: f64 = 3.2e9; // ~100 years: anything larger is a typo, not a window

/// Parse a time expression.
///
/// * `now`, or empty: the current time.
/// * `-15m`, `-90s`, `-2h`, `-7d`, `-1w`, `-90`: that long ago. A unit without the minus (`2h`)
///   also means "2 h ago".
/// * a bare number of at least 1e8: unix seconds. Smaller bare numbers are rejected as ambiguous
///   (use `-90s`); so are non-finite or absurd values.
/// * `YYYY-MM-DD HH:MM[:SS]` (or with `T`), local time; a trailing `Z` or `±HH:MM` makes it absolute.
pub fn parse_time(expr: &str, now: i64) -> Result<Parsed> {
    let e = expr.trim();
    if e.is_empty() || e == "now" {
        return Ok(Parsed::At(now));
    }
    let has_unit = e.chars().last().is_some_and(|c| c.is_ascii_alphabetic());
    let rel = e.strip_prefix('-');
    if rel.is_some() || has_unit {
        let body = rel.unwrap_or(e);
        if let Some(d) = crate::timefmt::parse_duration(body) {
            if d.is_finite() && d <= MAX_SPAN_S {
                return Ok(Parsed::At(now - d as i64));
            }
            return Err(bad(format!("duration {expr:?} is out of range")));
        }
    }
    if let Ok(v) = e.parse::<f64>() {
        if !v.is_finite() {
            return Err(bad(format!("{expr:?} is not a finite time")));
        }
        if (1e8..1e11).contains(&v) {
            return Ok(Parsed::At(v as i64));
        }
        return Err(bad(format!(
            "bare number {expr:?} is ambiguous: unix seconds must be >= 1e8; use -{e}s for \"{e} seconds ago\""
        )));
    }
    if looks_like_datetime(e) {
        return Ok(Parsed::DateTime(e.replace('T', " ")));
    }
    Err(bad(format!("cannot parse time {expr:?} (use unix seconds, -15m, now, or 'YYYY-MM-DD HH:MM[:SS][Z|+HH:MM]')")))
}

fn looks_like_datetime(e: &str) -> bool {
    let b = e.as_bytes();
    let d = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    if !(d(0)
        && d(1)
        && d(2)
        && d(3)
        && b.get(4) == Some(&b'-')
        && d(5)
        && d(6)
        && b.get(7) == Some(&b'-')
        && d(8)
        && d(9))
    {
        return false;
    }
    if b.len() == 10 {
        return true; // date only: midnight
    }
    if !matches!(b.get(10), Some(b' ') | Some(b'T')) || !(d(11) && d(12) && b.get(13) == Some(&b':') && d(14) && d(15))
    {
        return false;
    }
    let mut i = 16;
    if b.get(i) == Some(&b':') {
        if !(d(17) && d(18)) {
            return false;
        }
        i = 19;
        if b.get(i) == Some(&b'.') {
            i += 1;
            let start = i;
            while d(i) {
                i += 1;
            }
            if i == start {
                return false;
            }
        }
    }
    match &e[i..] {
        "" | "Z" | "z" => true,
        z => {
            let zb = z.as_bytes();
            zb.len() == 6
                && matches!(zb[0], b'+' | b'-')
                && zb[1].is_ascii_digit()
                && zb[2].is_ascii_digit()
                && zb[3] == b':'
                && zb[4].is_ascii_digit()
                && zb[5].is_ascii_digit()
        }
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
        assert!((sm.energy_est_wh.unwrap() - 2.25).abs() < 1e-9);
        assert_eq!(sm.coverage_s, 90);
        assert_eq!(sm.energy_sources, vec!["gpu", "cpu_pkg"]);
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
    fn marks_lifecycle() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("m.db")).unwrap();
        let id = s.mark_start("test", "bench", 100.0, Some("echo hi")).unwrap();
        assert_eq!(s.mark_end(Some("other-host"), "bench", 150.0, Some(0)).unwrap(), None, "wrong host");
        assert_eq!(s.mark_end(Some("test"), "bench", 200.0, Some(0)).unwrap(), Some(id));
        let m = s.mark_by_name("bench").unwrap().unwrap();
        assert_eq!(m.t_end, Some(200.0));
        // a closed mark is never overwritten
        assert!(!s.mark_end_id(id, 999.0, Some(1), None).unwrap());
        assert!(s.mark_exists(id).unwrap());
        assert!(!s.mark_exists(id + 100).unwrap());
        assert_eq!(s.mark_by_name("bench").unwrap().unwrap().t_end, Some(200.0));
        let id2 = s.mark_start("test", "bench", 300.0, None).unwrap();
        assert!(s.mark_end_id(id2, 310.0, Some(130), Some("signal 2")).unwrap());
        assert_eq!(s.mark_by_id(id2).unwrap().unwrap().note.as_deref(), Some("signal 2"));
        // marks outlive the sample retention window
        s.prune(30, 100 + 365 * 86400).unwrap();
        assert_eq!(s.marks(10).unwrap().len(), 2);
        assert_eq!(s.prune_marks(30, 100 + 365 * 86400).unwrap(), 2);
    }

    fn at(e: &str, now: i64) -> i64 {
        match parse_time(e, now).unwrap_or_else(|err| panic!("{e}: {err}")) {
            Parsed::At(t) => t,
            Parsed::DateTime(_) => panic!("{e} is a datetime"),
        }
    }

    #[test]
    fn time_expressions() {
        assert_eq!(at("-15m", 10_000), 10_000 - 900);
        assert_eq!(at("2h", 10_000), 10_000 - 7200, "a unit without '-' means ago");
        assert_eq!(at("-90", 10_000), 10_000 - 90);
        assert_eq!(at("-90s", 10_000), 10_000 - 90);
        assert_eq!(at("now", 77), 77);
        assert_eq!(at("", 77), 77);
        assert_eq!(at("1790000000", 0), 1_790_000_000);
    }

    #[test]
    fn bare_small_numbers_are_not_julian_days() {
        // regression: "90" used to fall through to SQLite's strftime and mean Julian day 90
        for bad in ["90", "3600", "99999999", "0"] {
            let e = parse_time(bad, 1_000_000_000).unwrap_err();
            assert!(e.downcast_ref::<BadRequest>().is_some(), "{bad}: {e}");
            assert!(e.to_string().contains("ambiguous"), "{e}");
        }
    }

    #[test]
    fn non_finite_and_absurd_times_are_rejected() {
        for bad in
            ["inf", "-inf", "nan", "1e30", "-1e30s", "1e30s", "1e12", "9999999999999", "-", "--5m", "5x", "2026-13"]
        {
            assert!(parse_time(bad, 1_000_000_000).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn datetime_strings_with_zones() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("m.db")).unwrap();
        // explicit zones are absolute and independent of the machine's timezone
        assert_eq!(s.resolve_time("2026-03-08T10:00:00Z", 0).unwrap(), 1_772_964_000);
        assert_eq!(s.resolve_time("2026-03-08 03:00:00-07:00", 0).unwrap(), 1_772_964_000);
        assert_eq!(s.resolve_time("2026-03-08T12:00+02:00", 0).unwrap(), 1_772_964_000);
        assert_eq!(s.resolve_time("2026-03-08 10:00:00.250Z", 0).unwrap(), 1_772_964_000);
        // local times resolve to *something* sane (the DST-gap hour shifts forward, per SQLite)
        assert!(s.resolve_time("2026-03-08 02:30", 0).unwrap() > 1_700_000_000);
        assert!(s.resolve_time("2026-01-31 08:00", 0).unwrap() > 1_700_000_000);
        assert!(s.resolve_time("2026-01-31", 0).unwrap() > 1_700_000_000);
        assert!(s.resolve_time("not a time", 0).is_err());
        assert!(s.resolve_time("2026-01-31 25:00 junk", 0).is_err());
    }

    #[test]
    fn energy_skips_gaps_and_reports_coverage() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(d.path().join("m.db")).unwrap();
        s.set_meta("sample_interval_s", "10").unwrap();
        // 10 W for 30 s, an 8-hour gap, then 10 W for 30 s
        for i in 0..4 {
            s.insert(&snap(1_000_000 + i * 10, 5.0)).unwrap(); // est_total_w = 10 W
        }
        for i in 0..4 {
            s.insert(&snap(1_000_000 + 8 * 3600 + i * 10, 5.0)).unwrap();
        }
        let sm = s.summary(0, 2_000_000).unwrap();
        assert_eq!(sm.coverage_s, 60, "the gap must not be interpolated");
        let wh = sm.energy_est_wh.unwrap();
        assert!((wh - 10.0 * 60.0 / 3600.0).abs() < 1e-9, "{wh}");
        assert_eq!(sm.energy_sources, vec!["gpu", "cpu_pkg"]);
        // a window with a single sample has no energy figure
        assert_eq!(s.summary(1_000_000, 1_000_005).unwrap().energy_est_wh, None);
    }

    #[test]
    fn database_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("m.db");
        // an older database that was created world-readable
        drop(Store::open(&p).unwrap());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut s = Store::open(&p).unwrap();
        s.insert(&snap(1_000_000, 1.0)).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let f = format!("{}{suffix}", p.display());
            if let Ok(m) = std::fs::metadata(&f) {
                assert_eq!(m.permissions().mode() & 0o777, 0o600, "{f}");
            }
        }
    }

    #[test]
    fn fresh_database_is_current_and_reopen_is_cheap() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("m.db");
        drop(Store::open(&p).unwrap());
        let s = Store::open(&p).unwrap();
        assert_eq!(s.status().unwrap().schema_version, SCHEMA_VERSION);
        // the read-only open works against a current database
        assert!(Store::open_readonly(&p).is_ok());
    }

    #[test]
    fn migrates_v1_database_in_place() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("old.db");
        {
            // what the first release created: v1 tables, meta.schema_version=1, user_version 0
            let c = Connection::open(&p).unwrap();
            c.execute_batch(TABLES).unwrap();
            c.execute_batch("DROP INDEX marks_name;").unwrap();
            c.execute("INSERT INTO meta VALUES('schema_version','1')", []).unwrap();
            c.execute("INSERT INTO samples(ts,host,cpu_util_pct) VALUES(1000,'h',42.0)", []).unwrap();
        }
        assert!(Store::open_readonly(&p).is_err(), "read-only open must not pretend an old schema is current");
        let s = Store::open(&p).unwrap();
        assert_eq!(s.status().unwrap().schema_version, SCHEMA_VERSION);
        assert_eq!(s.meta("schema_version").unwrap().as_deref(), Some("2"));
        assert_eq!(s.count().unwrap(), 1, "data survives the migration");
        let idx: i64 =
            s.conn.query_row("SELECT count(*) FROM sqlite_master WHERE name='marks_name'", [], |r| r.get(0)).unwrap();
        assert_eq!(idx, 1);
        assert!(Store::open_readonly(&p).is_ok());
    }

    #[test]
    fn refuses_a_newer_database() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("new.db");
        drop(Store::open(&p).unwrap());
        Connection::open(&p).unwrap().pragma_update(None, "user_version", SCHEMA_VERSION + 1).unwrap();
        let e = Store::open(&p).err().unwrap();
        assert!(e.to_string().contains("newer benchit-telemetry"), "{e}");
        assert!(Store::open_readonly(&p).is_err());
        // and it was left untouched
        let v: i64 = Connection::open(&p).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION + 1);
    }

    #[test]
    fn wide_view_covers_four_gpus() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(d.path().join("m.db")).unwrap();
        let mut sn = snap(1_000_000, 5.0);
        sn.gpus = (0..4)
            .map(|i| Gpu { idx: i, name: format!("G{i}"), power_w: Some(10.0 * (i + 1) as f64), ..Default::default() })
            .collect();
        s.insert(&sn).unwrap();
        let mut buf = Vec::new();
        s.export_csv(0, 2_000_000, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let header = text.lines().next().unwrap();
        assert!(header.contains("gpu3_power_w"), "{header}");
        assert!(text.lines().nth(1).unwrap().contains(",40,"), "gpu3 power present");
    }

    #[test]
    fn csv_cells_are_not_formulas() {
        assert_eq!(csv_escape("=HYPERLINK(\"x\")"), "\"'=HYPERLINK(\"\"x\"\")\"");
        assert_eq!(csv_escape("+1"), "'+1");
        assert_eq!(csv_escape("@SUM(A1)"), "'@SUM(A1)");
        assert_eq!(csv_escape("a\rb"), "\"a\rb\"");
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
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
    #[test]
    fn json_export_and_export_to_dir() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(d.path().join("m.db")).unwrap();
        s.insert(&snap(1_000_000, 5.0)).unwrap();
        let mut buf = Vec::new();
        assert_eq!(s.export_json(0, 2_000_000, &mut buf).unwrap(), 1);
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        let arr = v.as_array().expect("json array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["ts"], 1_000_000);
        assert_eq!(arr[0]["host"], "test");

        let path = export_to_dir(&s, d.path(), 0, 2_000_000, "1h", crate::prefs::ExportKind::Csv, 0).unwrap();
        assert_eq!(path.file_name().unwrap(), "telemetry-1h-19700101T000000Z.csv");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("local_time,ts,host,"));
        let mode = std::fs::metadata(&path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.mode() & 0o777, 0o600);

        let path = export_to_dir(&s, d.path(), 0, 2_000_000, "24h", crate::prefs::ExportKind::Json, 0).unwrap();
        assert_eq!(path.extension().unwrap(), "json");
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }
}
