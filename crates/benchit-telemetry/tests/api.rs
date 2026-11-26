use axum::body::Body;
use axum::http::{Request, StatusCode};
use benchit_telemetry::api::{router, AppState, MAX_LIMIT};
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
    let state = AppState::new(db, "test".into(), Arc::new(RwLock::new(Some(snap(now)))), Arc::new(RwLock::new(None)));
    (d, state)
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

fn post(uri: &str, body: &str) -> Request<Body> {
    Request::post(uri).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn endpoints() {
    let (_d, st) = setup();
    let app = router(st);
    let (c, v) = call(app.clone(), get("/health")).await;
    assert_eq!(c, StatusCode::OK);
    assert_eq!(v["status"], "ok");

    for prefix in ["/metrics", "/api/v1"] {
        let (c, v) = call(app.clone(), get(&format!("{prefix}/current"))).await;
        assert_eq!(c, StatusCode::OK, "{prefix}");
        assert_eq!(v["cpu"]["util_pct"], 50.0);
        let (c, v) =
            call(app.clone(), get(&format!("{prefix}/history?from=-5m&fields=cpu_util_pct,est_total_w"))).await;
        assert_eq!(c, StatusCode::OK, "{v}");
        assert_eq!(v["count"], 6);
        assert_eq!(v["rows"][0]["est_total_w"], 150.0);
    }

    let (c, v) = call(app.clone(), get("/metrics/history?fields=nope")).await;
    assert_eq!(c, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("unknown field"));

    let (c, v) = call(app.clone(), get("/metrics/gpus?from=-5m&idx=0&step=30")).await;
    assert_eq!(c, StatusCode::OK);
    assert!(v["count"].as_u64().unwrap() >= 2);

    let (c, v) = call(app.clone(), get("/metrics/summary?from=-5m")).await;
    assert_eq!(c, StatusCode::OK);
    assert_eq!(v["samples"], 6);
    assert!(v["energy_est_wh"].is_number());
    assert_eq!(v["coverage_s"], 50);
    assert!(v.get("energy_wh").is_none(), "renamed: it is an estimate");

    let (c, v) = call(app.clone(), post("/marks", r#"{"name":"bench"}"#)).await;
    assert_eq!(c, StatusCode::OK, "{v}");
    let (c, _) = call(app.clone(), post("/marks/end", r#"{"name":"bench"}"#)).await;
    assert_eq!(c, StatusCode::OK);
    // legacy /marks is a bare array, /api/v1/marks an object
    let (_, v) = call(app.clone(), get("/marks")).await;
    assert_eq!(v[0]["name"], "bench");
    assert!(v[0]["t_end"].is_number());
    let (_, v) = call(app.clone(), get("/api/v1/marks")).await;
    assert_eq!(v["count"], 1);
    assert_eq!(v["marks"][0]["name"], "bench");
}

#[tokio::test]
async fn read_only_rejects_writes() {
    let (_d, mut st) = setup();
    st.writable = false;
    let (c, _) = call(router(st.clone()), post("/marks", r#"{"name":"x"}"#)).await;
    assert_eq!(c, StatusCode::FORBIDDEN);
    let (c, _) = call(router(st), post("/marks/end", r#"{"name":"x"}"#)).await;
    assert_eq!(c, StatusCode::FORBIDDEN);
}

// ---- B2: DNS rebinding and cross-origin protection

fn with_host(uri: &str, host: &str) -> Request<Body> {
    Request::get(uri).header("host", host).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn foreign_host_headers_are_refused() {
    let (_d, st) = setup();
    let app = router(st);
    for host in [
        "attacker.example:19184",
        "attacker.example",
        "localhost.attacker.example:9184",
        "127.0.0.1.attacker.example",
        "0.0.0.0:9184",
        "192.168.1.5:9184",
    ] {
        for uri in ["/metrics/current", "/api/v1/history", "/health", "/marks"] {
            let (c, _) = call(app.clone(), with_host(uri, host)).await;
            assert_eq!(c, StatusCode::MISDIRECTED_REQUEST, "{host} {uri}");
        }
    }
    // POSTs from a rebound page are refused too
    let req = Request::post("/marks")
        .header("host", "attacker.example:9184")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"x"}"#))
        .unwrap();
    let (c, _) = call(app, req).await;
    assert_eq!(c, StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn local_host_headers_are_accepted() {
    let (_d, st) = setup();
    let app = router(st);
    for host in ["localhost:9184", "localhost", "LOCALHOST:80", "127.0.0.1:9184", "[::1]:9184", "[::1]"] {
        let (c, _) = call(app.clone(), with_host("/metrics/current", host)).await;
        assert_eq!(c, StatusCode::OK, "{host}");
    }
}

#[tokio::test]
async fn allow_host_extends_the_list() {
    let (_d, mut st) = setup();
    st.allow_hosts = vec!["bench.lan".into()];
    let app = router(st);
    let (c, _) = call(app.clone(), with_host("/health", "bench.lan:9184")).await;
    assert_eq!(c, StatusCode::OK);
    let (c, _) = call(app, with_host("/health", "other.lan:9184")).await;
    assert_eq!(c, StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn non_local_origins_are_forbidden() {
    let (_d, st) = setup();
    let app = router(st);
    let with_origin = |o: &str| {
        Request::get("/metrics/current")
            .header("host", "127.0.0.1:9184")
            .header("origin", o)
            .body(Body::empty())
            .unwrap()
    };
    for o in ["http://evil.example", "https://evil.example:443", "null", "http://localhost.evil.example", "garbage"] {
        let (c, _) = call(app.clone(), with_origin(o)).await;
        assert_eq!(c, StatusCode::FORBIDDEN, "{o}");
    }
    for o in ["http://localhost:3000", "http://127.0.0.1:9184", "http://[::1]:8080"] {
        let (c, _) = call(app.clone(), with_origin(o)).await;
        assert_eq!(c, StatusCode::OK, "{o}");
    }
}

#[tokio::test]
async fn posts_need_json_content_type_and_no_cors_is_offered() {
    let (_d, st) = setup();
    let app = router(st);
    // a cross-site <form>/fetch can only send these without a preflight
    let req = Request::post("/marks").header("content-type", "text/plain").body(Body::from(r#"{"name":"x"}"#)).unwrap();
    let (c, _) = call(app.clone(), req).await;
    assert_eq!(c, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let req = Request::options("/marks")
        .header("origin", "http://evil.example")
        .header("access-control-request-method", "POST")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none());
    assert_ne!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn oversized_bodies_are_rejected() {
    let (_d, st) = setup();
    let big = format!(r#"{{"name":"x","cmd":"{}"}}"#, "a".repeat(40_000));
    let (c, _) = call(router(st), post("/marks", &big)).await;
    assert_eq!(c, StatusCode::PAYLOAD_TOO_LARGE);
}

// ---- M6: bounded results

#[tokio::test]
async fn limits_are_capped_and_big_windows_are_bucketed() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("metrics.db");
    let mut s = Store::open(&db).unwrap();
    s.set_meta("sample_interval_s", "10").unwrap();
    let now = benchit_telemetry_core::timefmt::now();
    for i in 0..300 {
        s.insert(&snap(now - 3000 + i * 10)).unwrap();
    }
    let st = AppState::new(db, "t".into(), Arc::new(RwLock::new(Some(snap(now)))), Arc::new(RwLock::new(None)));
    let app = router(st);
    // an absurd limit is clamped, not honoured
    let (c, v) = call(app.clone(), get("/api/v1/history?from=-1h&limit=99999999&fields=cpu_util_pct")).await;
    assert_eq!(c, StatusCode::OK);
    assert!(v["limit"].as_u64().unwrap() as usize <= MAX_LIMIT);
    // a window with more rows than the limit is bucketed automatically instead of being cut off
    let (c, v) = call(app.clone(), get("/api/v1/history?from=-1h&limit=50&fields=cpu_util_pct")).await;
    assert_eq!(c, StatusCode::OK, "{v}");
    assert_eq!(v["auto_step"], true);
    assert!(v["step"].as_i64().unwrap() >= 60, "{v}");
    assert!(v["count"].as_u64().unwrap() <= 50);
    // explicit negative or huge steps are clamped and echoed as the clamped value
    let (_, v) = call(app.clone(), get("/api/v1/history?step=-5&fields=cpu_util_pct")).await;
    assert_eq!(v["step"], 0);
    let (_, v) = call(app.clone(), get("/api/v1/history?step=99999999999&fields=cpu_util_pct")).await;
    assert_eq!(v["step"], 31 * 86400);
    // duplicate (non-adjacent) fields collapse
    let (_, v) = call(app, get("/api/v1/history?from=-5m&fields=cpu_util_pct,load1,cpu_util_pct")).await;
    assert_eq!(v["rows"][0].as_object().unwrap().len(), 3, "ts + 2 distinct fields: {v}");
}

// ---- status codes

#[tokio::test]
async fn time_expression_errors_are_400() {
    let (_d, st) = setup();
    let app = router(st);
    for q in ["from=90", "from=inf", "from=1e30", "from=-1e30s", "from=garbage", "to=nan"] {
        let (c, v) = call(app.clone(), get(&format!("/metrics/history?{q}"))).await;
        assert_eq!(c, StatusCode::BAD_REQUEST, "{q}: {v}");
    }
    let (c, _) = call(app.clone(), get("/metrics/history?from=-1m&to=-1h")).await;
    assert_eq!(c, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn sql_injection_through_fields_is_rejected() {
    let (_d, st) = setup();
    let app = router(st);
    for f in ["ts;%20DROP%20TABLE%20samples", "cpu_util_pct%2C(select%201)", "1%3D1"] {
        let (c, _) = call(app.clone(), get(&format!("/metrics/history?fields={f}"))).await;
        assert_eq!(c, StatusCode::BAD_REQUEST, "{f}");
    }
    // and the table is still there
    let (c, v) = call(app, get("/metrics/history?from=-5m&fields=cpu_util_pct")).await;
    assert_eq!(c, StatusCode::OK);
    assert_eq!(v["count"], 6);
}

#[tokio::test]
async fn unknown_marks_and_ids() {
    let (_d, st) = setup();
    let app = router(st);
    let (c, _) = call(app.clone(), post("/marks/end", r#"{"id":424242}"#)).await;
    assert_eq!(c, StatusCode::NOT_FOUND);
    let (c, _) = call(app.clone(), post("/marks/end", r#"{"name":"never-started"}"#)).await;
    assert_eq!(c, StatusCode::NOT_FOUND);
    let (c, _) = call(app.clone(), post("/marks/end", r#"{}"#)).await;
    assert_eq!(c, StatusCode::BAD_REQUEST);
    let (c, _) = call(app.clone(), get("/metrics/summary?mark=never-started")).await;
    assert_eq!(c, StatusCode::NOT_FOUND);

    let (_, v) = call(app.clone(), post("/marks", r#"{"name":"b"}"#)).await;
    let id = v["id"].as_i64().unwrap();
    let (c, _) = call(app.clone(), post("/marks/end", &format!(r#"{{"id":{id},"exit_code":0}}"#))).await;
    assert_eq!(c, StatusCode::OK);
    // closing again must not rewrite the end time
    let (_, before) = call(app.clone(), get("/marks")).await;
    let (c, _) = call(app.clone(), post("/marks/end", &format!(r#"{{"id":{id},"exit_code":9}}"#))).await;
    assert_eq!(c, StatusCode::CONFLICT);
    let (_, after) = call(app.clone(), get("/marks")).await;
    assert_eq!(before, after);
    let (c, _) = call(app.clone(), post("/marks", r#"{"name":""}"#)).await;
    assert_eq!(c, StatusCode::BAD_REQUEST);
    let (c, _) = call(app, get("/nope")).await;
    assert_eq!(c, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn missing_database_is_503_and_does_not_leak_paths() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("secret-home-dir/metrics.db");
    let st = AppState::new(db, "t".into(), Arc::new(RwLock::new(None)), Arc::new(RwLock::new(None)));
    let app = router(st);
    let res = app.clone().oneshot(get("/metrics/history")).await.unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(!body.contains("secret-home-dir"), "{body}");
    let (c, _) = call(app, get("/metrics/current")).await;
    assert_eq!(c, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn health_reports_starting_and_stale() {
    let d = tempfile::tempdir().unwrap();
    let db = d.path().join("m.db");
    let cur = Arc::new(RwLock::new(None));
    let st = AppState::new(db, "t".into(), cur.clone(), Arc::new(RwLock::new(None)));
    let app = router(st);
    let (c, v) = call(app.clone(), get("/health")).await;
    assert_eq!((c, v["status"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("starting")));
    // an old sample means the collector has stopped (e.g. panicked): not "ok"
    *cur.write().unwrap() = Some(snap(benchit_telemetry_core::timefmt::now() - 600));
    let (c, v) = call(app.clone(), get("/health")).await;
    assert_eq!((c, v["status"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("stale")));
    *cur.write().unwrap() = Some(snap(benchit_telemetry_core::timefmt::now()));
    let (c, v) = call(app, get("/api/v1/health")).await;
    assert_eq!((c, v["status"].as_str()), (StatusCode::OK, Some("ok")));
}

#[tokio::test]
async fn a_newer_schema_is_an_internal_error_without_details() {
    let (d, st) = setup();
    rusqlite_set_user_version(&d.path().join("metrics.db"), 99);
    let res = router(st).oneshot(get("/metrics/history")).await.unwrap();
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(!body.contains(d.path().to_str().unwrap()), "{body}");
}

fn rusqlite_set_user_version(path: &std::path::Path, v: i64) {
    // go through the library's own re-export so the test needs no extra dependency
    let c = benchit_telemetry_core::store::raw_connection(path).unwrap();
    c.pragma_update(None, "user_version", v).unwrap();
}

#[tokio::test]
async fn saturated_query_slots_answer_503_immediately() {
    let (_d, st) = setup();
    let app = router(st.with_query_limit(0));
    let (c, v) = call(app.clone(), get("/api/v1/history")).await;
    assert_eq!(c, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert!(v["error"].as_str().unwrap().contains("concurrent"));
    // cheap endpoints that touch no database are unaffected
    let (c, _) = call(app, get("/health")).await;
    assert_eq!(c, StatusCode::OK);
}
