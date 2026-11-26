//! HTTP JSON API (axum).
use anyhow::anyhow;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use benchit_telemetry_core::panel::{self, RecInfo};
use benchit_telemetry_core::store::{self, Store};
use benchit_telemetry_core::{timefmt, Snapshot};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// Shared state: the database path and the most recent live snapshot.
#[derive(Clone)]
pub struct AppState {
    pub db: PathBuf,
    pub host: String,
    pub current: Arc<RwLock<Option<Snapshot>>>,
    pub rec: Arc<RwLock<Option<RecInfo>>>,
    /// Allow POST endpoints (marks).
    pub writable: bool,
}

pub struct ApiError(StatusCode, anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(StatusCode::BAD_REQUEST, e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": format!("{:#}", self.1) }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/metrics/current", get(current))
        .route("/metrics/panel", get(current_panel))
        .route("/metrics/history", get(history))
        .route("/metrics/gpus", get(gpus))
        .route("/metrics/summary", get(summary))
        .route("/metrics/fields", get(fields))
        .route("/marks", get(marks).post(mark_start))
        .route("/marks/end", post(mark_end))
        .with_state(state)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.into()))?
        .map_err(ApiError::from)
}

async fn index() -> Json<Value> {
    Json(json!({
        "name": "benchit-telemetry",
        "version": env!("CARGO_PKG_VERSION"),
        "endpoints": [
            "GET /health", "GET /metrics/current", "GET /metrics/panel",
            "GET /metrics/history?from=-1h&to=now&fields=cpu_util_pct,gpu_total_w&step=60&limit=10000",
            "GET /metrics/gpus?from=-1h&to=now&idx=0&step=60",
            "GET /metrics/summary?from=-15m&to=now | ?mark=NAME", "GET /metrics/fields",
            "GET /marks?limit=50", "POST /marks {\"name\":..}", "POST /marks/end {\"name\":..} | {\"id\":..}"
        ]
    }))
}

async fn health(State(st): State<AppState>) -> Json<Value> {
    let last = st.current.read().ok().and_then(|c| c.as_ref().map(|s| s.ts));
    let rec = st.rec.read().ok().and_then(|r| r.clone());
    Json(
        json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION"), "host": st.host, "last_sample_ts": last, "recording": rec }),
    )
}

async fn current(State(st): State<AppState>) -> Result<Json<Snapshot>, ApiError> {
    st.current
        .read()
        .ok()
        .and_then(|c| c.clone())
        .map(Json)
        .ok_or_else(|| ApiError(StatusCode::SERVICE_UNAVAILABLE, anyhow!("no sample collected yet")))
}

async fn current_panel(State(st): State<AppState>) -> ApiResult<Value> {
    let s = st
        .current
        .read()
        .ok()
        .and_then(|c| c.clone())
        .ok_or_else(|| ApiError(StatusCode::SERVICE_UNAVAILABLE, anyhow!("no sample yet")))?;
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

const DEFAULT_LIMIT: usize = 20_000;
const MAX_LIMIT: usize = 500_000;

fn window(s: &Store, q: &Range) -> anyhow::Result<(i64, i64)> {
    let now = timefmt::now();
    if let Some(name) = &q.mark {
        let m = s.mark_by_name(name)?.ok_or_else(|| anyhow!("no mark named {name:?}"))?;
        return Ok((m.t_start.floor() as i64, m.t_end.unwrap_or(now as f64).ceil() as i64));
    }
    let from = s.resolve_time(q.from.as_deref().unwrap_or("-1h"), now)?;
    let to = s.resolve_time(q.to.as_deref().unwrap_or("now"), now)?;
    if to < from {
        anyhow::bail!("`to` is before `from`");
    }
    Ok((from, to))
}

async fn history(State(st): State<AppState>, Query(q): Query<Range>) -> ApiResult<Value> {
    let v = blocking(move || {
        let s = Store::open_readonly(&st.db)?;
        let (from, to) = window(&s, &q)?;
        let mut fields = q
            .fields
            .as_deref()
            .map(|f| store::validate_fields(f, &[store::SAMPLE_FIELDS, store::SAMPLE_TEXT_FIELDS].concat()))
            .transpose()?
            .unwrap_or_default();
        fields.dedup();
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let rows = s.history(from, to, &fields, q.step.unwrap_or(0).max(0), limit)?;
        Ok(json!({ "from": from, "to": to, "step": q.step.unwrap_or(0), "count": rows.len(), "rows": rows }))
    })
    .await?;
    Ok(Json(v))
}

async fn gpus(State(st): State<AppState>, Query(q): Query<Range>) -> ApiResult<Value> {
    let v = blocking(move || {
        let s = Store::open_readonly(&st.db)?;
        let (from, to) = window(&s, &q)?;
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let rows = s.gpu_history(from, to, q.idx, q.step.unwrap_or(0).max(0), limit)?;
        Ok(json!({ "from": from, "to": to, "count": rows.len(), "rows": rows }))
    })
    .await?;
    Ok(Json(v))
}

async fn summary(State(st): State<AppState>, Query(q): Query<Range>) -> ApiResult<Value> {
    let v = blocking(move || {
        let s = Store::open_readonly(&st.db)?;
        let (from, to) = window(&s, &q)?;
        Ok(serde_json::to_value(s.summary(from, to)?)?)
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

async fn marks(State(st): State<AppState>, Query(q): Query<MarksQuery>) -> ApiResult<Value> {
    let v = blocking(move || {
        let s = Store::open_readonly(&st.db)?;
        Ok(serde_json::to_value(s.marks(q.limit.unwrap_or(50).min(10_000))?)?)
    })
    .await?;
    Ok(Json(v))
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
        Err(ApiError(StatusCode::FORBIDDEN, anyhow!("API started with --read-only")))
    }
}

async fn mark_start(State(st): State<AppState>, Json(b): Json<MarkBody>) -> ApiResult<Value> {
    require_writable(&st)?;
    let name = b
        .name
        .filter(|n| !n.trim().is_empty() && n.len() <= 200)
        .ok_or_else(|| anyhow!("`name` (1-200 chars) is required"))?;
    let v = blocking(move || {
        let s = Store::open(&st.db)?;
        let t = timefmt::now_f64();
        let id = s.mark_start(&st.host, &name, t, b.cmd.as_deref())?;
        Ok(json!({ "id": id, "name": name, "t_start": t }))
    })
    .await?;
    Ok(Json(v))
}

async fn mark_end(State(st): State<AppState>, Json(b): Json<MarkBody>) -> ApiResult<Value> {
    require_writable(&st)?;
    let v = blocking(move || {
        let s = Store::open(&st.db)?;
        let t = timefmt::now_f64();
        let id = match (b.id, b.name.as_deref()) {
            (Some(id), _) => {
                s.mark_end_id(id, t, b.exit_code)?;
                Some(id)
            }
            (None, Some(n)) => s.mark_end(n, t, b.exit_code)?,
            _ => anyhow::bail!("`id` or `name` is required"),
        };
        let id = id.ok_or_else(|| anyhow!("no open mark with that name"))?;
        Ok(json!({ "id": id, "t_end": t }))
    })
    .await?;
    Ok(Json(v))
}
