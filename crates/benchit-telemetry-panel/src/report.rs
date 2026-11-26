//! Panel exports behind the right-click menu (and `benchit-panel --export`): the stats text
//! copied to the clipboard, CSV and JSON dumps, and a PDF report.
//!
//! Every export holds the live snapshot from the daemon's feed plus the history the daemon keeps
//! in its database for the chosen window. Files go to `~/benchit-reports/` (or
//! `$BENCHIT_REPORTS_DIR`), are named `benchit-<host>-<range>-<UTC timestamp>.<ext>` and are
//! created with mode 0600, like the daemon's own exports.
use crate::pdf;
use anyhow::{bail, Context, Result};
use benchit_telemetry_core::model::Snapshot;
use benchit_telemetry_core::panel::RecInfo;
use benchit_telemetry_core::store::Summary;
use benchit_telemetry_core::{panel, paths, timefmt, ExportRange, Store, TempUnit};
use serde_json::{json, Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Directory name under `$HOME` for panel exports.
pub const REPORTS_DIR_NAME: &str = "benchit-reports";
/// JSON history is bucketed (averaged) so that it never holds more rows than this.
pub const JSON_MAX_ROWS: i64 = 10_000;
/// Points per chart in the PDF report.
const CHART_POINTS: i64 = 300;

/// Export file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Json,
    Pdf,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Csv, Format::Json, Format::Pdf];

    pub fn ext(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
            Self::Pdf => "pdf",
        }
    }

    /// Menu label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Csv => "Export CSV",
            Self::Json => "Export JSON",
            Self::Pdf => "PDF report",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.ext().eq_ignore_ascii_case(s.trim()))
    }
}

/// `1h`, `24h`, `7d`, `30d` (any `<n>h` or `<n>d`).
pub fn parse_range(s: &str) -> Option<ExportRange> {
    let s = s.trim().to_ascii_lowercase();
    let (num, unit) = s.split_at(s.len().checked_sub(1)?);
    let n: u32 = num.parse().ok().filter(|n| *n > 0)?;
    match unit {
        "h" => Some(ExportRange::Hours(n)),
        "d" => Some(ExportRange::Days(n)),
        _ => None,
    }
}

/// `$BENCHIT_REPORTS_DIR`, else `~/benchit-reports`.
pub fn reports_dir() -> PathBuf {
    reports_dir_from(std::env::var_os("BENCHIT_REPORTS_DIR"), std::env::var_os("HOME"))
}

fn reports_dir_from(explicit: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> PathBuf {
    match (explicit, home) {
        (Some(d), _) if !d.is_empty() => PathBuf::from(d),
        (_, Some(h)) if !h.is_empty() => PathBuf::from(h).join(REPORTS_DIR_NAME),
        _ => PathBuf::from(REPORTS_DIR_NAME),
    }
}

/// `~/x` for paths under `$HOME` (for the toast and notification).
pub fn display_path(p: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

fn safe_host(host: &str) -> String {
    let h: String =
        host.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    if h.is_empty() {
        "host".into()
    } else {
        h
    }
}

/// `benchit-<host>-<range>-<YYYYMMDDThhmmssZ>.<ext>`.
pub fn file_name(host: &str, format: Format, range: ExportRange, now: i64) -> String {
    format!("benchit-{}-{}-{}.{}", safe_host(host), range.tag(), timefmt::compact_utc(now), format.ext())
}

/// Local `YYYY-MM-DD HH:MM:SS TZ`.
pub fn local_datetime(ts: i64) -> String {
    let mut buf = [0u8; 64];
    // SAFETY: `t` and `tm` are valid for the duration of the calls; `buf` is large enough and
    // `strftime` NUL-terminates within its length argument.
    unsafe {
        let t: libc::time_t = ts as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return ts.to_string();
        }
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), c"%Y-%m-%d %H:%M:%S %Z".as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

/// The text the "Copy stats" item puts on the clipboard: the CLI `panel` text view plus a
/// timestamp line.
pub fn stats_text(s: &Snapshot, unit: TempUnit) -> String {
    format!("{}\n{}\n", panel::text_with_unit(s, 60, unit).trim_end(), local_datetime(s.ts))
}

/// One export to write.
#[derive(Debug, Clone)]
pub struct Request {
    pub format: Format,
    pub range: ExportRange,
    /// The daemon's data directory (holds `metrics.db`).
    pub data_dir: PathBuf,
    pub out_dir: PathBuf,
    pub now: i64,
    /// Latest snapshot from the daemon's feed, if any.
    pub snapshot: Option<Snapshot>,
    pub rec: Option<RecInfo>,
    pub unit: TempUnit,
}

impl Request {
    fn host(&self) -> String {
        match &self.snapshot {
            Some(s) if !s.host.is_empty() => s.host.clone(),
            _ => benchit_telemetry_core::collect::hostname(Path::new("/")),
        }
    }

    fn window(&self) -> (i64, i64) {
        (self.now - self.range.seconds(), self.now)
    }
}

/// History source for an export: the daemon's database, read-only.
fn open_store(data_dir: &Path) -> Option<Store> {
    Store::open_readonly(data_dir.join(paths::DB_FILE)).ok()
}

/// Write the export and return its path.
pub fn run(req: &Request) -> Result<PathBuf> {
    let store = open_store(&req.data_dir);
    if store.is_none() && req.snapshot.is_none() {
        bail!("nothing to export: no telemetry database in {} and no live feed", req.data_dir.display());
    }
    create_private_dir(&req.out_dir)?;
    let name = file_name(&req.host(), req.format, req.range, req.now);
    let (path, file) = create_new(&req.out_dir, &name)?;
    let mut out = std::io::BufWriter::new(file);
    let res = match req.format {
        Format::Csv => write_csv(req, store.as_ref(), &mut out),
        Format::Json => write_json(req, store.as_ref(), &mut out),
        Format::Pdf => write_pdf(req, store.as_ref(), &mut out),
    };
    let res = res.and_then(|()| out.flush().context("flush"));
    if let Err(e) = res {
        drop(out);
        let _ = std::fs::remove_file(&path);
        return Err(e.context(format!("writing {}", path.display())));
    }
    Ok(path)
}

fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create {}", dir.display()))?;
    }
    Ok(())
}

/// Create `dir/name` (0600) without overwriting; `-2`, `-3`, ... are appended on a clash.
fn create_new(dir: &Path, name: &str) -> Result<(PathBuf, std::fs::File)> {
    use std::os::unix::fs::OpenOptionsExt;
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    for i in 1..100 {
        let n = if i == 1 { name.to_string() } else { format!("{stem}-{i}.{ext}") };
        let p = dir.join(n);
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p) {
            Ok(f) => return Ok((p, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("create {}", p.display())),
        }
    }
    bail!("too many exports named {name} in {}", dir.display())
}

/// The live snapshot as CSV (header + one row) with the columns of `samples_wide`.
fn snapshot_csv(s: &Snapshot) -> Result<String> {
    let mut mem = Store::open_in_memory()?;
    mem.insert(s)?;
    let mut buf = Vec::new();
    mem.export_csv(i64::MIN, i64::MAX, &mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Stored rows of the window at full resolution, then the live snapshot as the last row when it
/// is newer than the newest stored row.
fn write_csv(req: &Request, store: Option<&Store>, out: &mut dyn Write) -> Result<()> {
    let (from, to) = req.window();
    let mut rows = 0;
    let mut last_ts = None;
    if let Some(st) = store {
        rows = st.export_csv(from, to, out)?;
        last_ts = st.status().ok().and_then(|s| s.last_ts);
    }
    if let Some(s) = &req.snapshot {
        if rows == 0 || last_ts.is_none_or(|t| s.ts > t) {
            let live = snapshot_csv(s)?;
            let body = if rows == 0 { live.as_str() } else { live.split_once('\n').map_or("", |(_, b)| b) };
            out.write_all(body.as_bytes())?;
        }
    }
    Ok(())
}

/// Bucket size (seconds) that keeps `range` under `max_rows` rows; 0 = raw rows.
fn step_for(range_s: i64, interval_s: f64, max_rows: i64) -> i64 {
    let interval = if interval_s > 0.0 { interval_s } else { 10.0 };
    if (range_s as f64 / interval) as i64 <= max_rows {
        0
    } else {
        (range_s + max_rows - 1) / max_rows
    }
}

fn interval_s(req: &Request, store: Option<&Store>) -> f64 {
    req.rec
        .as_ref()
        .map(|r| r.interval_s)
        .filter(|v| *v > 0.0)
        .or_else(|| store.and_then(|s| s.meta("sample_interval_s").ok().flatten()).and_then(|v| v.parse().ok()))
        .unwrap_or(10.0)
}

/// One JSON document: export metadata, the live snapshot, the window summary, and the stored
/// history (raw rows, or averaged buckets of `history_step_s` when the window is long).
fn write_json(req: &Request, store: Option<&Store>, out: &mut dyn Write) -> Result<()> {
    let (from, to) = req.window();
    let step = step_for(to - from, interval_s(req, store), JSON_MAX_ROWS);
    let (summary, history, gpu_history) = match store {
        Some(st) => (
            st.summary(from, to).ok(),
            st.history(from, to, &[], step, JSON_MAX_ROWS as usize * 2)?,
            st.gpu_history(from, to, None, step, JSON_MAX_ROWS as usize * 8)?,
        ),
        None => (None, Vec::new(), Vec::new()),
    };
    let doc = json!({
        "generator": format!("benchit-panel {}", env!("CARGO_PKG_VERSION")),
        "host": req.host(),
        "generated_at": req.now,
        "generated_local": local_datetime(req.now),
        "range": req.range.tag(),
        "from": from,
        "to": to,
        "history_step_s": step,
        "current": req.snapshot,
        "recorder": req.rec,
        "summary": summary,
        "history": history.into_iter().map(Value::Object).collect::<Vec<_>>(),
        "gpu_history": gpu_history.into_iter().map(Value::Object).collect::<Vec<_>>(),
    });
    serde_json::to_writer_pretty(&mut *out, &doc)?;
    writeln!(out)?;
    Ok(())
}

fn fnum(v: f64) -> String {
    if v.abs() >= 1000.0 {
        format!("{v:.0}")
    } else if v.abs() >= 100.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

/// Metric table rows: name, n, min, avg, p95, max.
fn stats_rows(s: &Summary) -> Vec<String> {
    let mut v = vec![format!("{:<16}{:>8}{:>11}{:>11}{:>11}{:>11}", "metric", "n", "min", "avg", "p95", "max")];
    for m in &s.metrics {
        v.push(format!(
            "{:<16}{:>8}{:>11}{:>11}{:>11}{:>11}",
            m.field,
            m.n,
            fnum(m.min),
            fnum(m.avg),
            fnum(m.p95),
            fnum(m.max)
        ));
    }
    v
}

/// Series for one chart: (ts, value) points.
fn series(rows: &[Map<String, Value>], field: &str) -> Vec<(f64, f64)> {
    rows.iter()
        .filter_map(|r| Some((r.get("ts")?.as_f64()?, r.get(field)?.as_f64()?)))
        .filter(|(_, v)| v.is_finite())
        .collect()
}

/// Charts in the report: (field, title, unit).
const CHARTS: [(&str, &str, &str); 6] = [
    ("est_total_w", "Estimated total power (GPUs + CPU package)", "W"),
    ("gpu_total_w", "GPU board power, all GPUs", "W"),
    ("cpu_pkg_w", "CPU package power (RAPL)", "W"),
    ("cpu_temp_c", "CPU temperature", "C"),
    ("cpu_util_pct", "CPU utilisation", "%"),
    ("mem_used_pct", "Memory used", "%"),
];

fn write_pdf(req: &Request, store: Option<&Store>, out: &mut dyn Write) -> Result<()> {
    let (from, to) = req.window();
    let host = req.host();
    let mut d = pdf::Doc::new(&format!("benchit-telemetry report: {host}"));
    d.heading(&format!("benchit-telemetry report: {host}"), 18.0);
    d.text(&format!("Window: last {} ({} to {})", req.range.tag(), local_datetime(from), local_datetime(to)), 9.5);
    d.text(&format!("Generated {} by benchit-panel {}", local_datetime(req.now), env!("CARGO_PKG_VERSION")), 9.5);
    d.gap(10.0);

    d.heading("Current snapshot", 12.5);
    match &req.snapshot {
        Some(s) => {
            d.text(&format!("Taken {}", local_datetime(s.ts)), 8.5);
            for line in panel::text_with_unit(s, 92, req.unit).lines() {
                d.mono(line, 8.5);
            }
        }
        None => d.text("No live snapshot (is the benchit-telemetry daemon running?)", 9.5),
    }
    d.gap(10.0);

    d.heading("Summary", 12.5);
    let summary = store.and_then(|s| s.summary(from, to).ok());
    match &summary {
        Some(s) if s.samples > 0 => {
            let energy = |e: Option<f64>| e.map_or("n/a".to_string(), |v| format!("{v:.2} Wh"));
            d.text(
                &format!(
                    "{} stored samples; estimated energy {} ({}), covering {} of the window",
                    s.samples,
                    energy(s.energy_est_wh),
                    if s.energy_sources.is_empty() { "no power sources".into() } else { s.energy_sources.join(" + ") },
                    human_duration(s.coverage_s)
                ),
                9.5,
            );
            if s.platform_energy_est_wh.is_some() {
                d.text(&format!("Platform (RAPL psys) energy: {}", energy(s.platform_energy_est_wh)), 9.5);
            }
            d.gap(4.0);
            for row in stats_rows(s) {
                d.mono(&row, 8.0);
            }
            for (idx, st) in &s.gpus {
                d.gap(6.0);
                d.text(&format!("GPU {idx}"), 10.0);
                let gs = Summary { metrics: st.clone(), gpus: Vec::new(), energy_sources: Vec::new(), ..s.clone() };
                for row in stats_rows(&gs) {
                    d.mono(&row, 8.0);
                }
            }
        }
        _ => d.text("No stored history for this window.", 9.5),
    }

    if let Some(st) = store {
        let step = step_for(to - from, interval_s(req, Some(st)), CHART_POINTS).max(1);
        let fields: Vec<String> = CHARTS.iter().map(|(f, _, _)| f.to_string()).collect();
        let rows = st.history(from, to, &fields, step, (CHART_POINTS * 4) as usize).unwrap_or_default();
        let charts: Vec<_> =
            CHARTS.iter().map(|(f, t, u)| (*t, *u, series(&rows, f))).filter(|(_, _, s)| s.len() >= 2).collect();
        if !charts.is_empty() {
            d.gap(10.0);
            d.keep(170.0); // heading together with the first chart
            d.heading(&format!("History (averaged per {})", human_duration(step)), 12.5);
            for (title, unit, pts) in charts {
                d.chart(title, unit, &pts, from as f64, to as f64);
            }
        }
    }
    out.write_all(&d.finish(req.now))?;
    Ok(())
}

fn human_duration(s: i64) -> String {
    match s {
        s if s >= 86_400 && s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s >= 3600 => format!("{:.1}h", s as f64 / 3600.0),
        s if s >= 60 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

/// Best-effort desktop notification (`notify-send`); ignored where it is not installed.
pub fn notify(summary: &str, body: &str) {
    if cfg!(test) {
        return;
    }
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=benchit-panel", "--icon=document-save", "--expire-time=8000", summary, body])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use benchit_telemetry_core::model::{Cpu, Gpu};

    fn snap(ts: i64, w: f64) -> Snapshot {
        Snapshot {
            ts,
            host: "rig-1".into(),
            cpu: Cpu {
                model: Some("Test CPU".into()),
                temp_c: Some(50.0 + w / 10.0),
                util_pct: Some(w),
                pkg_w: Some(w),
                ..Default::default()
            },
            gpus: vec![Gpu { idx: 0, name: "Test GPU".into(), power_w: Some(2.0 * w), ..Default::default() }],
            gpu_total_w: Some(2.0 * w),
            est_total_w: Some(3.0 * w),
            uptime_s: Some(3600.0),
            ..Default::default()
        }
    }

    /// A data dir whose database holds 60 rows ending at `now - 10`.
    fn data_dir(now: i64) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let mut st = Store::open(d.path().join(paths::DB_FILE)).unwrap();
        for i in 0..60 {
            st.insert(&snap(now - 600 + i * 10 - 10, 10.0 + i as f64)).unwrap();
        }
        d
    }

    fn req(data: &Path, out: &Path, format: Format, now: i64, live: bool) -> Request {
        Request {
            format,
            range: ExportRange::Hours(1),
            data_dir: data.to_path_buf(),
            out_dir: out.to_path_buf(),
            now,
            snapshot: live.then(|| snap(now, 99.0)),
            rec: None,
            unit: TempUnit::C,
        }
    }

    const NOW: i64 = 1_795_000_000;

    #[test]
    fn parse_format_and_range() {
        assert_eq!(Format::parse("PDF"), Some(Format::Pdf));
        assert_eq!(Format::parse("xml"), None);
        assert_eq!(parse_range("24h"), Some(ExportRange::Hours(24)));
        assert_eq!(parse_range("7D"), Some(ExportRange::Days(7)));
        assert_eq!(parse_range("0h"), None);
        assert_eq!(parse_range("h"), None);
        assert_eq!(parse_range(""), None);
        for r in ExportRange::MENU {
            assert_eq!(parse_range(&r.tag()), Some(r));
        }
    }

    #[test]
    fn names_and_dirs() {
        assert_eq!(
            file_name("gaming-001", Format::Csv, ExportRange::Days(7), 0),
            "benchit-gaming-001-7d-19700101T000000Z.csv"
        );
        assert!(file_name("a/b c", Format::Pdf, ExportRange::Hours(1), 0).starts_with("benchit-a_b_c-1h-"));
        assert!(file_name("", Format::Json, ExportRange::Hours(1), 0).starts_with("benchit-host-"));
        assert_eq!(reports_dir_from(Some("/r".into()), Some("/h".into())), PathBuf::from("/r"));
        assert_eq!(reports_dir_from(Some("".into()), Some("/h".into())), PathBuf::from("/h/benchit-reports"));
        assert_eq!(reports_dir_from(None, None), PathBuf::from("benchit-reports"));
    }

    #[test]
    fn step_keeps_row_count_bounded() {
        assert_eq!(step_for(3600, 10.0, 10_000), 0);
        assert_eq!(step_for(30 * 86_400, 10.0, 10_000), 260);
        assert_eq!(step_for(86_400, 10.0, 300), 288);
        assert_eq!(step_for(3600, 0.0, 10_000), 0);
    }

    #[test]
    fn stats_text_has_host_and_time() {
        let t = stats_text(&snap(NOW, 10.0), TempUnit::C);
        assert!(t.starts_with("RIG-1"), "{t}");
        assert!(t.contains("Test GPU") || t.contains("GPU"), "{t}");
        assert!(t.ends_with('\n'));
    }

    #[test]
    fn csv_has_history_then_live_row() {
        let data = data_dir(NOW);
        let out = tempfile::tempdir().unwrap();
        let p = run(&req(data.path(), out.path(), Format::Csv, NOW, true)).unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 1 + 60 + 1, "header, 60 stored rows, live row");
        let cols = lines[0].split(',').count();
        assert!(lines.iter().all(|l| l.split(',').count() == cols), "every row has the header's columns");
        assert!(lines[61].contains(&format!(",{NOW},")), "live row last: {}", lines[61]);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        // a second export in the same second does not overwrite the first
        let p2 = run(&req(data.path(), out.path(), Format::Csv, NOW, true)).unwrap();
        assert_ne!(p, p2);
        assert!(p2.to_string_lossy().ends_with("-2.csv"));
    }

    #[test]
    fn csv_without_database_is_the_live_row() {
        let empty = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let p = run(&req(empty.path(), out.path(), Format::Csv, NOW, true)).unwrap();
        assert_eq!(std::fs::read_to_string(p).unwrap().lines().count(), 2);
        assert!(run(&req(empty.path(), out.path(), Format::Csv, NOW, false)).is_err(), "nothing to export");
    }

    #[test]
    fn json_has_current_summary_and_history() {
        let data = data_dir(NOW);
        let out = tempfile::tempdir().unwrap();
        let p = run(&req(data.path(), out.path(), Format::Json, NOW, true)).unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["host"], "rig-1");
        assert_eq!(v["range"], "1h");
        assert_eq!(v["current"]["ts"], NOW);
        assert_eq!(v["history"].as_array().unwrap().len(), 60);
        assert_eq!(v["gpu_history"].as_array().unwrap().len(), 60);
        assert_eq!(v["summary"]["samples"], 60);
        assert_eq!(v["history_step_s"], 0);
        assert!(p.file_name().unwrap().to_string_lossy().starts_with("benchit-rig-1-1h-"));
    }

    #[test]
    fn pdf_report_is_a_pdf() {
        let data = data_dir(NOW);
        let out = tempfile::tempdir().unwrap();
        let p = run(&req(data.path(), out.path(), Format::Pdf, NOW, true)).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert!(b.starts_with(b"%PDF-"), "PDF header");
        assert!(b.windows(5).any(|w| w == b"%%EOF"), "PDF trailer");
        assert!(b.len() > 2000);
        // live-only report (no database) still renders
        let empty = tempfile::tempdir().unwrap();
        let p = run(&req(empty.path(), out.path(), Format::Pdf, NOW + 1, true)).unwrap();
        assert!(std::fs::read(p).unwrap().starts_with(b"%PDF-"));
    }
}
