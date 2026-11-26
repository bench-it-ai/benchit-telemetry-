use axum::body::Body;
use axum::http::{Request, StatusCode};
use benchit_telemetry::api::{router, AppState};
use benchit_telemetry_core::model::{Cpu, Gpu, Snapshot};
use benchit_telemetry_core::Store;
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::{Arc, RwLock};
use tower::ServiceExt;

fn snap(ts: i64) -> Snapshot {
    Snapshot {
        ts,
        host: "test".into(),
        cpu: Cpu { util_pct: Some(50.0), temp_c: Some(60.0), ..Default::default() },
        gpus: vec![Gpu { idx: 0, name: "GPU".into(), power_w: Some(100.0), ..Default::default() }],
        gpu_total_w: Some(100.0),
        est_total_w: Some(150.0),
        ..Default::default()
    }
}

async fn call(app: axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn setup() -> (tempfile::TempDir, AppState) {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("metrics.db");
    let mut s = Store::open(&db).unwrap();
    let now = benchit_telemetry_core::timefmt::now();
    for i in 0..6 {
        s.insert(&snap(now - 60 + i * 10)).unwrap();
    }
    let state = AppState {
        db,
        host: "test".into(),
        current: Arc::new(RwLock::new(Some(snap(now)))),
        rec: Arc::new(RwLock::new(None)),
        writable: true,
    };
    (d, state)
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn endpoints() {
    let (_d, st) = setup();
    let app = router(st);
    let (c, v) = call(app.clone(), get("/health")).await;
    assert_eq!(c, StatusCode::OK);
    assert_eq!(v["status"], "ok");

    let (c, v) = call(app.clone(), get("/metrics/current")).await;
    assert_eq!(c, StatusCode::OK);
    assert_eq!(v["cpu"]["util_pct"], 50.0);

    let (c, v) = call(app.clone(), get("/metrics/history?from=-5m&fields=cpu_util_pct,est_total_w")).await;
    assert_eq!(c, StatusCode::OK, "{v}");
    assert_eq!(v["count"], 6);
    assert_eq!(v["rows"][0]["est_total_w"], 150.0);

    let (c, v) = call(app.clone(), get("/metrics/history?fields=nope")).await;
    assert_eq!(c, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("unknown field"));

    let (c, v) = call(app.clone(), get("/metrics/gpus?from=-5m&idx=0&step=30")).await;
    assert_eq!(c, StatusCode::OK);
    assert!(v["count"].as_u64().unwrap() >= 2);

    let (c, v) = call(app.clone(), get("/metrics/summary?from=-5m")).await;
    assert_eq!(c, StatusCode::OK);
    assert_eq!(v["samples"], 6);

    let req = Request::post("/marks")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"bench"}"#))
        .unwrap();
    let (c, v) = call(app.clone(), req).await;
    assert_eq!(c, StatusCode::OK, "{v}");
    let req = Request::post("/marks/end")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"bench"}"#))
        .unwrap();
    let (c, _) = call(app.clone(), req).await;
    assert_eq!(c, StatusCode::OK);
    let (_, v) = call(app.clone(), get("/marks")).await;
    assert_eq!(v[0]["name"], "bench");
    assert!(v[0]["t_end"].is_number());
}

#[tokio::test]
async fn read_only_rejects_writes() {
    let (_d, mut st) = setup();
    st.writable = false;
    let req =
        Request::post("/marks").header("content-type", "application/json").body(Body::from(r#"{"name":"x"}"#)).unwrap();
    let (c, _) = call(router(st), req).await;
    assert_eq!(c, StatusCode::FORBIDDEN);
}
