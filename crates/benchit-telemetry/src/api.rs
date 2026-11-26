//! HTTP JSON API (axum).
//!
//! # Threat model
//!
//! The API is **unauthenticated** and meant for the local machine only. It exposes the
//! hostname, CPU/GPU models, mount paths (which reveal `$HOME`), the largest process name and
//! the command lines of benchmark marks. Defences:
//!
//! * `serve`/`--listen` default to loopback;
//! * every request must carry a `Host` whose name is `localhost`, `127.0.0.1`, `[::1]` or an
//!   explicit `--allow-host` (blocks DNS rebinding: a rebinding page still sends its own name);
//! * a present `Origin` header must be local as well;
//! * no CORS headers are ever sent, POSTs need `Content-Type: application/json` (so a plain
//!   cross-site form or `fetch` cannot trigger them) and `OPTIONS` is not answered;
//! * result sizes, concurrency, request time and body size are bounded.
//!
//! Other users on the same machine can still reach a loopback port; use `--read-only` and a
//! firewall or unix permissions if that matters.
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use benchit_telemetry_core::panel::{self, RecInfo};
use benchit_telemetry_core::store::{self, BadRequest, Store, Unavailable};
use benchit_telemetry_core::{timefmt, Snapshot};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Semaphore;

/// Rows returned by one request at most.
pub const MAX_LIMIT: usize = 50_000;
/// Rows returned when `limit` is not given.
pub const DEFAULT_LIMIT: usize = 10_000;
/// Simultaneous database queries; further requests get 503 immediately.
pub const MAX_CONCURRENT_QUERIES: usize = 4;
/// Wall-clock budget for one request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// POST bodies larger than this are rejected.
pub const MAX_BODY: usize = 16 * 1024;
const MAX_FIELDS: usize = 64;
const MAX_STEP: i64 = 31 * 86_400;

/// Shared state: the database path and the most recent live snapshot.
#[derive(Clone)]
pub struct AppState {
    pub db: PathBuf,
    pub host: String,
    pub current: Arc<RwLock<Option<Snapshot>>>,
    pub rec: Arc<RwLock<Option<RecInfo>>>,
    /// Allow POST endpoints (marks).
    pub writable: bool,
    /// Extra `Host` names accepted besides localhost (`--allow-host`).
    pub allow_hosts: Vec<String>,
    /// `/health` reports 503 `stale` when the newest sample is older than this many seconds.
    pub stale_after_s: f64,
    queries: Arc<Semaphore>,
}

impl AppState {
    pub fn new(
        db: PathBuf,
        host: String,
        current: Arc<RwLock<Option<Snapshot>>>,
        rec: Arc<RwLock<Option<RecInfo>>>,
    ) -> Self {
        Self {
            db,
            host,
            current,
            rec,
            writable: true,
            allow_hosts: Vec::new(),
            stale_after_s: 10.0,
            queries: Arc::new(Semaphore::new(MAX_CONCURRENT_QUERIES)),
        }
    }
}

impl AppState {
    /// Change the number of simultaneous database queries (default [`MAX_CONCURRENT_QUERIES`]).
    pub fn with_query_limit(mut self, n: usize) -> Self {
        self.queries = Arc::new(Semaphore::new(n));
        self
    }
}

/// An API failure with the right HTTP status. Internal details are logged, never sent.
#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    Conflict(String),
    Forbidden(String),
    Unavailable(String),
    Busy,
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        if let Some(b) = e.downcast_ref::<BadRequest>() {
            ApiError::BadRequest(b.0.clone())
        } else if e.downcast_ref::<Unavailable>().is_some() {
            // the message names the database path; keep it out of the response
            eprintln!("api: {e:#}");
            ApiError::Unavailable("database not available".into())
        } else {
            ApiError::Internal(e)
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            ApiError::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
            ApiError::Busy => (StatusCode::SERVICE_UNAVAILABLE, "too many concurrent queries; retry shortly".into()),
            ApiError::Internal(e) => {
                eprintln!("api internal error: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (code, Json(json!({ "error": msg }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest(msg.into())
}

// ---------------------------------------------------------------- host / origin guard

/// Host part of a `Host` header value or URL authority: strips `:port`, keeps `[::1]` intact.
fn host_name(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority.split_inclusive(']').next().unwrap_or(authority);
    }
    authority.rsplit_once(':').map_or(
        authority,
        |(h, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) {
                h
            } else {
                authority
            }
        },
    )
}

fn is_local_name(name: &str, extra: &[String]) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(n.as_str(), "localhost" | "127.0.0.1" | "[::1]") || extra.iter().any(|h| h.eq_ignore_ascii_case(&n))
}

async fn guard(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let h = req.headers();
    // A missing Host cannot come from a browser, so it is not a rebinding vector (and lets
    // in-process tests call the router directly).
    if let Some(host) = h.get(header::HOST) {
        let ok = host.to_str().is_ok_and(|v| is_local_name(host_name(v), &st.allow_hosts));
        if !ok {
            return (
                StatusCode::MISDIRECTED_REQUEST,
                Json(json!({ "error": "unexpected Host header; use localhost, 127.0.0.1 or [::1] (or --allow-host)" })),
            )
                .into_response();
        }
    }
    if let Some(origin) = h.get(header::ORIGIN) {
        let ok = origin.to_str().is_ok_and(|o| {
            o.split_once("://").is_some_and(|(scheme, rest)| {
                matches!(scheme, "http" | "https")
                    && is_local_name(host_name(rest.split('/').next().unwrap_or("")), &st.allow_hosts)
            })
        });
        if !ok {
            return ApiError::Forbidden("cross-origin requests are not allowed".into()).into_response();
        }
    }
    next.run(req).await
}

async fn timeout(req: Request, next: Next) -> Response {
    match tokio::time::timeout(REQUEST_TIMEOUT, next.run(req)).await {
        Ok(r) => r,
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, Json(json!({ "error": "request timed out" }))).into_response(),
    }
}

// ---------------------------------------------------------------- routing

fn data_routes() -> Router<AppState> {
    Router::new()
        .route("/current", get(current))
        .route("/panel", get(current_panel))
        .route("/history", get(history))
        .route("/gpus", get(gpus))
        .route("/summary", get(summary))
        .route("/fields", get(fields))
}

/// Router: `/api/v1/*` is the stable surface; `/metrics/*` and `/marks*` are kept as aliases
/// from the first release (and `/metrics` stays free for a future Prometheus exporter).
pub fn router(state: AppState) -> Router {
    let v1 = data_routes()
        .route("/health", get(health))
        .route("/marks", get(marks_v1).post(mark_start))
        .route("/marks/end", post(mark_end));
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .nest("/api/v1", v1)
        .nest("/metrics", data_routes())
        .route("/marks", get(marks_legacy).post(mark_start))
        .route("/marks/end", post(mark_end))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn(timeout))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Run blocking database work with a bounded number of concurrent queries. The permit moves
/// into the worker, so it is released only when the query really finishes (even if the client
/// gave up or the request timed out).
async fn blocking<T: Send + 'static>(
    st: &AppState,
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let permit = st.queries.clone().try_acquire_owned().map_err(|_| ApiError::Busy)?;
    tokio::task::spawn_blocking(move || {
        let r = f();
        drop(permit);
        r
    })
    .await
    .map_err(|e| ApiError::Internal(e.into()))?
}

async fn index() -> Json<Value> {
    Json(json!({
        "name": "benchit-telemetry",
        "version": env!("CARGO_PKG_VERSION"),
        "endpoints": [
            "GET /health", "GET /api/v1/current", "GET /api/v1/panel",
            "GET /api/v1/history?from=-1h&to=now&fields=cpu_util_pct,gpu_total_w&step=60&limit=10000",
            "GET /api/v1/gpus?from=-1h&to=now&idx=0&step=60",
            "GET /api/v1/summary?from=-15m&to=now | ?mark=NAME", "GET /api/v1/fields",
            "GET /api/v1/marks?limit=50", "POST /api/v1/marks {\"name\":..}",
            "POST /api/v1/marks/end {\"name\":..} | {\"id\":..}",
            "(deprecated aliases: /metrics/* and /marks*)"
        ]
    }))
}

async fn health(State(st): State<AppState>) -> Response {
    let last = st.current.read().ok().and_then(|c| c.as_ref().map(|s| s.ts));
    let rec = st.rec.read().ok().and_then(|r| r.clone());
    let age = last.map(|t| timefmt::now_f64() - t as f64);
    let (code, status) = match age {
        None => (StatusCode::SERVICE_UNAVAILABLE, "starting"),
        Some(a) if a > st.stale_after_s => (StatusCode::SERVICE_UNAVAILABLE, "stale"),
        Some(_) => (StatusCode::OK, "ok"),
    };
    (
        code,
        Json(json!({
            "status": status, "version": env!("CARGO_PKG_VERSION"), "host": st.host,
            "last_sample_ts": last, "last_sample_age_s": age.map(|a| (a * 10.0).round() / 10.0),
            "recording": rec
        })),
    )
        .into_response()
}

async fn current(State(st): State<AppState>) -> Result<Json<Snapshot>, ApiError> {
    st.current
        .read()
        .ok()
        .and_then(|c| c.clone())
        .map(Json)
        .ok_or_else(|| ApiError::Unavailable("no sample collected yet".into()))
}

async fn current_panel(State(st): State<AppState>) -> ApiResult<Value> {
    let s = st
        .current
        .read()
        .ok()
        .and_then(|c| c.clone())
        .ok_or_else(|| ApiError::Unavailable("no sample collected yet".into()))?;
    let rec = st.rec.read().ok().and_then(|r| r.clone());
    Ok(Json(panel::flat_json(&s, rec.as_ref())))
}

#[derive(Deserialize, Default)]
pub struct Range {
    from: Option<String>,
    to: Option<String>,
    fields: Option<String>,
    step: Option<i64>,
    limit: Option<usize>,
    idx: Option<u32>,
    mark: Option<String>,
}

fn window(s: &Store, q: &Range) -> Result<(i64, i64), ApiError> {
    let now = timefmt::now();
    if let Some(name) = &q.mark {
        let m = s.mark_by_name(name)?.ok_or_else(|| ApiError::NotFound(format!("no mark named {name:?}")))?;
        return Ok((m.t_start.floor() as i64, m.t_end.unwrap_or(now as f64).ceil() as i64));
    }
    let from = s.resolve_time(q.from.as_deref().unwrap_or("-1h"), now)?;
    let to = s.resolve_time(q.to.as_deref().unwrap_or("now"), now)?;
    if to < from {
        return Err(bad("`to` is before `from`"));
    }
    Ok((from, to))
}

/// Clamp `limit`, and pick a bucket `step` automatically when the window would otherwise hold
/// more rows than `limit`. Returns `(limit, step)`.
fn shape(s: &Store, q: &Range, from: i64, to: i64) -> (usize, i64) {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let mut step = q.step.unwrap_or(0).clamp(0, MAX_STEP);
    if step == 0 {
        let interval: f64 = s.meta("sample_interval_s").ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(10.0);
        let rows = ((to - from) as f64 / interval.max(1.0)) as usize;
        if rows > limit {
            // round up to a whole number of storage intervals
            let raw = (to - from) as f64 / limit as f64;
            step = ((raw / interval.max(1.0)).ceil() * interval.max(1.0)) as i64;
        }
    }
    (limit, step.clamp(0, MAX_STEP))
}

async fn history(State(st): State<AppState>, Query(q): Query<Range>) -> ApiResult<Value> {
    let st2 = st.clone();
    let v = blocking(&st, move || {
        let s = Store::open_readonly(&st2.db)?;
        let (from, to) = window(&s, &q)?;
        let mut fields = q
            .fields
            .as_deref()
            .map(|f| store::validate_fields(f, &[store::SAMPLE_FIELDS, store::SAMPLE_TEXT_FIELDS].concat()))
            .transpose()?
            .unwrap_or_default();
        let mut seen = HashSet::new();
        fields.retain(|f| seen.insert(f.clone()));
        if fields.len() > MAX_FIELDS {
            return Err(bad(format!("at most {MAX_FIELDS} fields per request")));
        }
        let (limit, step) = shape(&s, &q, from, to);
        let rows = s.history(from, to, &fields, step, limit)?;
        Ok(json!({
            "from": from, "to": to, "step": step, "auto_step": q.step.unwrap_or(0) == 0 && step > 0,
            "limit": limit, "count": rows.len(), "truncated": rows.len() >= limit, "rows": rows
        }))
    })
    .await?;
    Ok(Json(v))
}

async fn gpus(State(st): State<AppState>, Query(q): Query<Range>) -> ApiResult<Value> {
    let st2 = st.clone();
    let v = blocking(&st, move || {
        let s = Store::open_readonly(&st2.db)?;
        let (from, to) = window(&s, &q)?;
        // several GPUs share each timestamp, so budget rows per GPU
        let (limit, step) = shape(&s, &q, from, to);
        let rows = s.gpu_history(from, to, q.idx, step, limit)?;
        Ok(json!({
            "from": from, "to": to, "step": step, "limit": limit, "count": rows.len(),
            "truncated": rows.len() >= limit, "rows": rows
        }))
    })
    .await?;
    Ok(Json(v))
}

async fn summary(State(st): State<AppState>, Query(q): Query<Range>) -> ApiResult<Value> {
    let st2 = st.clone();
    let v = blocking(&st, move || {
        let s = Store::open_readonly(&st2.db)?;
        let (from, to) = window(&s, &q)?;
        serde_json::to_value(s.summary(from, to)?).map_err(|e| ApiError::Internal(e.into()))
    })
    .await?;
    Ok(Json(v))
}

async fn fields() -> Json<Value> {
    Json(
        json!({ "samples": store::SAMPLE_FIELDS, "samples_text": store::SAMPLE_TEXT_FIELDS, "gpus": store::GPU_FIELDS }),
    )
}

#[derive(Deserialize)]
pub struct MarksQuery {
    limit: Option<usize>,
}

async fn list_marks(st: &AppState, q: MarksQuery) -> Result<Value, ApiError> {
    let st2 = st.clone();
    blocking(st, move || {
        let s = Store::open_readonly(&st2.db)?;
        serde_json::to_value(s.marks(q.limit.unwrap_or(50).clamp(1, 1_000))?).map_err(|e| ApiError::Internal(e.into()))
    })
    .await
}

/// `/marks` (deprecated alias): a bare array, as in the first release.
async fn marks_legacy(State(st): State<AppState>, Query(q): Query<MarksQuery>) -> ApiResult<Value> {
    Ok(Json(list_marks(&st, q).await?))
}

/// `/api/v1/marks`: an object like every other endpoint.
async fn marks_v1(State(st): State<AppState>, Query(q): Query<MarksQuery>) -> ApiResult<Value> {
    let marks = list_marks(&st, q).await?;
    Ok(Json(json!({ "count": marks.as_array().map_or(0, Vec::len), "marks": marks })))
}

#[derive(Deserialize)]
pub struct MarkBody {
    name: Option<String>,
    id: Option<i64>,
    cmd: Option<String>,
    exit_code: Option<i32>,
}

fn require_writable(st: &AppState) -> Result<(), ApiError> {
    if st.writable {
        Ok(())
    } else {
        Err(ApiError::Forbidden("API started with --read-only".into()))
    }
}

async fn mark_start(State(st): State<AppState>, Json(b): Json<MarkBody>) -> ApiResult<Value> {
    require_writable(&st)?;
    let name = b
        .name
        .filter(|n| !n.trim().is_empty() && n.len() <= 200)
        .ok_or_else(|| bad("`name` (1-200 chars) is required"))?;
    if b.cmd.as_ref().is_some_and(|c| c.len() > 4096) {
        return Err(bad("`cmd` is limited to 4096 bytes"));
    }
    let st2 = st.clone();
    let v = blocking(&st, move || {
        let s = Store::open(&st2.db)?;
        let t = timefmt::now_f64();
        let id = s.mark_start(&st2.host, &name, t, b.cmd.as_deref())?;
        Ok(json!({ "id": id, "name": name, "t_start": t }))
    })
    .await?;
    Ok(Json(v))
}

async fn mark_end(State(st): State<AppState>, Json(b): Json<MarkBody>) -> ApiResult<Value> {
    require_writable(&st)?;
    if b.id.is_none() && b.name.is_none() {
        return Err(bad("`id` or `name` is required"));
    }
    let st2 = st.clone();
    let v = blocking(&st, move || {
        let s = Store::open(&st2.db)?;
        let t = timefmt::now_f64();
        let id = match (b.id, b.name.as_deref()) {
            (Some(id), _) => {
                if !s.mark_end_id(id, t, b.exit_code, None)? {
                    return Err(if s.mark_exists(id)? {
                        ApiError::Conflict(format!("mark {id} is already closed"))
                    } else {
                        ApiError::NotFound(format!("no mark with id {id}"))
                    });
                }
                id
            }
            (None, Some(n)) => s
                .mark_end(Some(&st2.host), n, t, b.exit_code)?
                .ok_or_else(|| ApiError::NotFound(format!("no open mark named {n:?}")))?,
            (None, None) => return Err(bad("`id` or `name` is required")),
        };
        Ok(json!({ "id": id, "t_end": t }))
    })
    .await?;
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_names() {
        assert_eq!(host_name("localhost:9184"), "localhost");
        assert_eq!(host_name("127.0.0.1"), "127.0.0.1");
        assert_eq!(host_name("[::1]:9184"), "[::1]");
        assert_eq!(host_name("[::1]"), "[::1]");
        assert_eq!(host_name("evil.example:80"), "evil.example");
        assert_eq!(host_name("localhost.evil.example:80"), "localhost.evil.example");
    }

    #[test]
    fn local_name_check_is_exact() {
        assert!(is_local_name("LOCALHOST", &[]));
        assert!(!is_local_name("localhost.evil.example", &[]));
        assert!(!is_local_name("127.0.0.1.evil.example", &[]));
        assert!(!is_local_name("0.0.0.0", &[]));
        assert!(is_local_name("bench.lan", &["Bench.LAN".to_string()]));
    }
}
