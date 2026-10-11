//! Gateway handler tests. The app is driven with `tower::ServiceExt::oneshot`; the provider is a
//! loopback mock of Mistral's `POST /v1/ocr` (one synchronous call, real redacted fixture payload)
//! reached through the gateway's per-provider / per-target `base_url` override.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::routing::{get, post};
use http_body_util::BodyExt;
use puffinparse_server::{AppState, Config};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const FIXTURE: &str = include_str!("../../puffinparse-core/tests/fixtures/mistral_ocr.json");
/// "iVBORw0KGgo=" is the PNG signature; the mock never looks at the bytes.
const PNG_B64: &str = "iVBORw0KGgo=";

const REDUCTO_RESULT: &str = include_str!("../../puffinparse-core/tests/fixtures/reducto_parse.json");

#[derive(Default)]
struct Mock {
    ok_hits: AtomicUsize,
    fail_hits: AtomicUsize,
    auth_seen: Mutex<Vec<String>>,
    /// Reducto emulation: submitted `/parse_async` bodies, and `GET /job/{id}` calls per job.
    job_seq: AtomicUsize,
    job_bodies: Mutex<Vec<Value>>,
    job_polls: Mutex<std::collections::HashMap<String, usize>>,
    job_auth: Mutex<Vec<String>>,
}

/// Reducto's async job flow: `POST /upload`, `POST /parse_async` → a job id, then
/// `GET /job/{id}` answers `Pending` the first time and the final state afterwards
/// (`Completed` with the recorded parse fixture under `/rd`, `Failed` under `/rdfail`).
fn reducto_routes(m: Arc<Mock>, prefix: &str, fail: bool) -> axum::Router {
    let (a, b) = (m.clone(), m);
    axum::Router::new()
        .route(&format!("{prefix}/upload"), post(|| async { axum::Json(json!({ "file_id": "reducto://file-1" })) }))
        .route(
            &format!("{prefix}/parse_async"),
            post(move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                let n = a.job_seq.fetch_add(1, Ordering::SeqCst) + 1;
                a.job_bodies.lock().unwrap().push(body);
                let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                a.job_auth.lock().unwrap().push(auth);
                axum::Json(json!({ "job_id": format!("rj-{n}") }))
            }),
        )
        .route(
            &format!("{prefix}/job/{{id}}"),
            get(
                move |axum::extract::Path(id): axum::extract::Path<String>, headers: axum::http::HeaderMap| async move {
                    let auth =
                        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    b.job_auth.lock().unwrap().push(auth);
                    let polls = {
                        let mut p = b.job_polls.lock().unwrap();
                        let n = p.entry(id.clone()).or_default();
                        *n += 1;
                        *n
                    };
                    let body = if polls == 1 {
                        json!({ "status": "Pending" })
                    } else if fail {
                        json!({ "status": "Failed", "reason": "mock parse failed: corrupt PDF" })
                    } else {
                        let mut result: Value = serde_json::from_str(REDUCTO_RESULT).unwrap();
                        result["job_id"] = json!(id);
                        json!({ "status": "Completed", "result": result })
                    };
                    axum::Json(body)
                },
            ),
        )
}

/// Start the mock provider; returns its base address (`http://127.0.0.1:port`).
async fn mock() -> (String, Arc<Mock>) {
    let m = Arc::new(Mock::default());
    let (a, b) = (m.clone(), m.clone());
    let app = axum::Router::new()
        .merge(reducto_routes(m.clone(), "/rd", false))
        .merge(reducto_routes(m.clone(), "/rdfail", true))
        .route(
            "/ok/v1/ocr",
            post(move |headers: axum::http::HeaderMap| async move {
                a.ok_hits.fetch_add(1, Ordering::SeqCst);
                let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                a.auth_seen.lock().unwrap().push(auth);
                ([(header::CONTENT_TYPE, "application/json")], FIXTURE)
            }),
        )
        .route(
            "/slow/v1/ocr",
            post(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                ([(header::CONTENT_TYPE, "application/json")], FIXTURE)
            }),
        )
        .route(
            "/fail/v1/ocr",
            post(move || async move {
                b.fail_hits.fetch_add(1, Ordering::SeqCst);
                (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({ "message": "mock upstream exploded" })))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), m)
}

fn config(base: &str) -> String {
    std::env::set_var("PUFFINPARSE_TEST_MISTRAL_KEY", "provider-key-from-env");
    format!(
        r#"
        master_key = "sk-master"
        [server]
        max_retries = 0
        [providers.mistral]
        api_key = "env:PUFFINPARSE_TEST_MISTRAL_KEY"
        base_url = "{base}/ok"

        [providers.reducto]
        api_key = "reducto-provider-key"
        base_url = "{base}/rd"

        [[models]]
        name = "failing-jobs"
        targets = [{{ model = "reducto/standard", base_url = "{base}/rdfail", api_key = "reducto-alias-key" }}, "reducto/r-1"]

        [[models]]
        name = "resilient"
        targets = [{{ model = "mistral/ocr-latest", base_url = "{base}/fail" }}, "mistral/ocr-2512"]

        [[models]]
        name = "brittle"
        targets = [{{ model = "mistral/ocr-latest", base_url = "{base}/fail" }}, "mistral/ocr-2512"]
        fallback_on = ["timeout"]

        [[keys]]
        id = "team-a"
        key = "sk-team-a"
        models = ["resilient", "mistral/*"]

        [[keys]]
        id = "broke"
        key = "sk-broke"
        monthly_budget_usd = 0.005

        [[keys]]
        id = "slow"
        key = "sk-slow"
        rpm = 1

        [[keys]]
        id = "narrow"
        key = "sk-narrow"
        models = ["brittle"]

        [[keys]]
        id = "jobber"
        key = "sk-jobber"
        models = ["reducto/*", "failing-jobs", "mistral/*"]
        "#
    )
}

async fn app() -> (axum::Router, Arc<Mock>) {
    let (base, m) = mock().await;
    let state = AppState::from_config_quiet(Config::from_toml(&config(&base)).unwrap()).unwrap();
    (puffinparse_server::app(Arc::new(state)), m)
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Value, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let json = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, headers, json, text)
}

fn post_json(path: &str, key: Option<&str>, body: Value) -> Request<Body> {
    let mut b = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(k) = key {
        b = b.header(header::AUTHORIZATION, format!("Bearer {k}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn doc(model: &str) -> Value {
    json!({ "model": model, "document": PNG_B64, "filename": "page.png" })
}

#[tokio::test]
async fn health_is_open_and_parse_requires_a_key() {
    let (app, m) = app().await;
    let (s, _, body, _) = send(&app, Request::get("/health").body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    let (s, h, body, _) = send(&app, post_json("/v1/parse", None, doc("mistral"))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "unauthorized");
    assert!(body["error"]["request_id"].is_string());
    assert!(h.contains_key("x-request-id"));

    let (s, _, _, _) = send(&app, post_json("/v1/parse", Some("sk-wrong"), doc("mistral"))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(m.ok_hits.load(Ordering::SeqCst), 0, "no provider call before auth");
}

#[tokio::test]
async fn parse_returns_unified_json_with_cost() {
    let (app, m) = app().await;
    let (s, h, body, text) = send(&app, post_json("/v1/parse", Some("sk-team-a"), doc("mistral/ocr-latest"))).await;
    assert_eq!(s, StatusCode::OK, "{text}");
    assert_eq!(body["model"], "mistral/ocr-latest");
    assert_eq!(body["provider"], "mistral");
    assert_eq!(body["usage"]["pages"], 2);
    assert!(body["markdown"].as_str().unwrap().contains("Hello LiteOCR"));
    assert!((body["cost_usd"].as_f64().unwrap() - 0.008).abs() < 1e-9);
    assert_eq!(h["x-puffinparse-model"], "mistral/ocr-latest");
    // The provider key came from the `env:` reference in [providers.mistral].
    assert_eq!(m.auth_seen.lock().unwrap().as_slice(), ["Bearer provider-key-from-env"]);

    // `x-api-key` works too, and the usage endpoint reflects the spend.
    let req = Request::get("/v1/usage").header("x-api-key", "sk-team-a").body(Body::empty()).unwrap();
    let (s, _, usage, _) = send(&app, req).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(usage["keys"].as_array().unwrap().len(), 1);
    assert_eq!(usage["keys"][0]["requests"], 1);
    assert_eq!(usage["keys"][0]["pages"], 2);
}

#[tokio::test]
async fn ocr_mode_and_multipart_upload() {
    let (app, _) = app().await;
    let boundary = "XBOUNDARYX";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nmistral\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"pages\"\r\n\r\n1-2\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"scan.png\"\r\n\
         Content-Type: image/png\r\n\r\n\u{89}PNG\r\n--{boundary}--\r\n"
    );
    let req = Request::post("/v1/ocr")
        .header(header::AUTHORIZATION, "Bearer sk-master")
        .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
        .body(Body::from(body))
        .unwrap();
    let (s, _, body, text) = send(&app, req).await;
    assert_eq!(s, StatusCode::OK, "{text}");
    assert!(body["text"].as_str().unwrap().contains("Invoice #1234"));
    assert!(body["pages"][0]["lines"].is_array());
}

#[tokio::test]
async fn output_format_renders_the_vendor_shape() {
    let (app, _) = app().await;
    let mut req = doc("mistral");
    req["output_format"] = json!("reducto");
    let (s, _, body, text) = send(&app, post_json("/v1/parse", Some("sk-master"), req)).await;
    assert_eq!(s, StatusCode::OK, "{text}");
    assert_eq!(body["response_type"], "parse");
    assert_eq!(body["usage"]["num_pages"], 2);
    assert!(body["result"]["chunks"].is_array());

    let mut bad = doc("mistral");
    bad["output_format"] = json!("acme");
    let (s, _, body, _) = send(&app, post_json("/v1/parse", Some("sk-master"), bad)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"], "input_error");

    let mut ocr = doc("mistral");
    ocr["output_format"] = json!("reducto");
    let (s, _, _, _) = send(&app, post_json("/v1/ocr", Some("sk-master"), ocr)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn alias_falls_back_on_provider_error() {
    let (app, m) = app().await;
    let (s, _, body, text) = send(&app, post_json("/v1/parse", Some("sk-team-a"), doc("resilient"))).await;
    assert_eq!(s, StatusCode::OK, "{text}");
    assert_eq!(body["model"], "mistral/ocr-2512");
    assert_eq!(body["metadata"]["puffinparse_fallback_index"], 1);
    assert!(body["metadata"]["puffinparse_fallback_from_error"].as_str().unwrap().contains("mock upstream exploded"));
    assert_eq!(m.fail_hits.load(Ordering::SeqCst), 1);
    assert_eq!(m.ok_hits.load(Ordering::SeqCst), 1);

    let metrics = send(&app, get_as("/metrics", "sk-master")).await.3;
    assert!(metrics.contains("puffinparse_fallbacks_total 1"), "{metrics}");
}

#[tokio::test]
async fn provider_error_without_fallback_keeps_the_provider_message() {
    let (app, m) = app().await;
    let (s, _, body, _) = send(&app, post_json("/v1/parse", Some("sk-narrow"), doc("brittle"))).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"]["type"], "provider_error");
    assert_eq!(body["error"]["message"], "mock upstream exploded");
    assert_eq!(body["error"]["provider"], "mistral");
    assert_eq!(body["error"]["provider_status"], 500);
    assert_eq!(m.ok_hits.load(Ordering::SeqCst), 0, "fallback_on = [timeout] must not fall back on a 500");
}

#[tokio::test]
async fn request_fallbacks_extend_the_plan() {
    let (app, m) = app().await;
    let mut req = doc("brittle");
    req["fallbacks"] = json!(["mistral/ocr-4-1"]);
    // `brittle`'s own fallback_on (timeout only) governs the whole plan, so no fallback here...
    let (s, _, _, _) = send(&app, post_json("/v1/parse", Some("sk-master"), req)).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    // ...while an alias with the default kinds falls back within its own targets first.
    let (base_fail_hits, base_ok_hits) = (m.fail_hits.load(Ordering::SeqCst), m.ok_hits.load(Ordering::SeqCst));
    let mut req = doc("resilient");
    req["fallbacks"] = json!(["mistral/ocr-4-1"]);
    let (s, _, body, _) = send(&app, post_json("/v1/parse", Some("sk-master"), req)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["model"], "mistral/ocr-2512");
    assert_eq!(m.fail_hits.load(Ordering::SeqCst), base_fail_hits + 1);
    assert_eq!(m.ok_hits.load(Ordering::SeqCst), base_ok_hits + 1);
}

#[tokio::test]
async fn key_model_allow_list_is_enforced() {
    let (app, m) = app().await;
    let (s, _, body, _) = send(&app, post_json("/v1/parse", Some("sk-narrow"), doc("mistral/ocr-latest"))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["type"], "model_not_allowed");
    // Fallbacks are checked too.
    let mut req = doc("brittle");
    req["fallbacks"] = json!(["mistral/ocr-latest"]);
    let (s, _, _, _) = send(&app, post_json("/v1/parse", Some("sk-narrow"), req)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(m.ok_hits.load(Ordering::SeqCst) + m.fail_hits.load(Ordering::SeqCst), 0);

    let req = Request::get("/v1/models").header(header::AUTHORIZATION, "Bearer sk-narrow").body(Body::empty()).unwrap();
    let (s, _, models, _) = send(&app, req).await;
    assert_eq!(s, StatusCode::OK);
    let ids: Vec<&str> = models["data"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["brittle"]);

    let req = Request::get("/v1/models").header(header::AUTHORIZATION, "Bearer sk-master").body(Body::empty()).unwrap();
    let (_, _, models, _) = send(&app, req).await;
    let reducto = models["data"].as_array().unwrap().iter().find(|m| m["id"] == "reducto/standard").unwrap();
    assert!(reducto["per_page_usd"]["parse"].is_number());
    assert_eq!(reducto["modes"], json!(["parse", "ocr"]));
}

#[tokio::test]
async fn budget_exhaustion_returns_402() {
    let (app, m) = app().await;
    // Budget $0.005; one 2-page call at $0.004/page spends $0.008.
    let (s, _, _, _) = send(&app, post_json("/v1/parse", Some("sk-broke"), doc("mistral"))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, body, _) = send(&app, post_json("/v1/parse", Some("sk-broke"), doc("mistral"))).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(body["error"]["type"], "budget_exceeded");
    assert_eq!(m.ok_hits.load(Ordering::SeqCst), 1);
    // The master key has no budget.
    let (s, _, _, _) = send(&app, post_json("/v1/parse", Some("sk-master"), doc("mistral"))).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn rate_limit_returns_429_with_retry_after() {
    let (app, _) = app().await;
    let (s, _, _, _) = send(&app, post_json("/v1/parse", Some("sk-slow"), doc("mistral"))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, h, body, _) = send(&app, post_json("/v1/parse", Some("sk-slow"), doc("mistral"))).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"]["type"], "key_rate_limited");
    let retry: u64 = h[header::RETRY_AFTER].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry));
}

#[tokio::test]
async fn rejects_bad_requests_before_calling_a_provider() {
    let (app, m) = app().await;
    let cases = [
        // clients may not redirect the gateway's provider credentials
        json!({ "model": "mistral", "document_url": "https://x/a.pdf", "base_url": "http://evil" }),
        json!({ "model": "mistral", "document_url": "https://x/a.pdf", "api_key": "k" }),
        json!({ "document_url": "https://x/a.pdf" }),
        json!({ "model": "mistral" }),
        json!({ "model": "mistral", "document": PNG_B64 }),
        json!({ "model": "mistral", "document_url": "file:///etc/passwd" }),
        json!({ "model": "mistral", "document": "@@@", "filename": "a.png" }),
        json!({ "model": "nope/x", "document_url": "https://x/a.pdf" }),
        json!({ "model": "mistral", "document_url": "https://x/a.pdf", "schema": {} }),
    ];
    for c in cases {
        let (s, _, body, _) = send(&app, post_json("/v1/parse", Some("sk-master"), c.clone())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{c}: {body}");
        assert!(body["error"]["message"].is_string());
    }
    // extract needs a schema; a parse-only alias cannot serve extract... datalab has no extract.
    let (s, _, _, _) = send(
        &app,
        post_json("/v1/extract", Some("sk-master"), json!({ "model": "mistral", "document_url": "https://x/a.pdf" })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _, body, _) = send(
        &app,
        post_json(
            "/v1/extract",
            Some("sk-master"),
            json!({ "model": "datalab", "document_url": "https://x/a.pdf", "schema": {"type": "object"} }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"], "unsupported_model_error");
    assert_eq!(m.ok_hits.load(Ordering::SeqCst) + m.fail_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn provider_options_cannot_choose_a_local_program() {
    let (app, _) = app().await;
    for (path, key) in [("/v1/parse", "cmd"), ("/v1/ocr", "pdftoppm_cmd"), ("/v1/jobs", "cmd")] {
        let mut body = doc("tesseract");
        body["provider_options"] = json!({ key: "/bin/sh" });
        let (s, _, body, _) = send(&app, post_json(path, Some("sk-master"), body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{path} {key}: {body}");
        let msg = body["error"]["message"].as_str().unwrap();
        assert!(msg.contains(&format!("provider_options.{key}")), "{msg}");
    }
}

#[tokio::test]
async fn metrics_count_requests_errors_pages_and_cost() {
    let (app, _) = app().await;
    send(&app, post_json("/v1/parse", Some("sk-master"), doc("mistral"))).await;
    send(&app, post_json("/v1/parse", None, doc("mistral"))).await;
    let (s, h, _, text) = send(&app, get_as("/metrics", "sk-master")).await;
    assert_eq!(s, StatusCode::OK);
    // Behind auth by default (any gateway key); `server.public_metrics = true` opens it.
    let (s, _, _, _) = send(&app, Request::get("/metrics").body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&app, get_as("/metrics", "sk-narrow")).await.0, StatusCode::OK);
    assert!(h[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/plain"));
    assert!(
        text.contains(r#"puffinparse_requests_total{mode="parse",model="mistral/ocr-latest",status="200"} 1"#),
        "{text}"
    );
    assert!(text.contains(r#"puffinparse_requests_total{mode="parse",model="-",status="401"} 1"#), "{text}");
    assert!(text.contains(r#"puffinparse_errors_total{type="unauthorized"} 1"#));
    assert!(text.contains(r#"puffinparse_pages_total{model="mistral/ocr-latest"} 2"#));
    assert!(text.contains(r#"puffinparse_cost_usd_total{model="mistral/ocr-latest"} 0.008"#));
    assert!(text.contains(r#"puffinparse_request_duration_seconds_count{mode="parse"} 2"#));
}

#[tokio::test]
async fn open_gateway_without_keys() {
    let (base, _) = mock().await;
    let cfg = Config::from_toml(&format!("[providers.mistral]\napi_key = \"k\"\nbase_url = \"{base}/ok\"")).unwrap();
    let app = puffinparse_server::app(Arc::new(AppState::from_config_quiet(cfg).unwrap()));
    let (s, _, _, _) = send(&app, post_json("/v1/parse", None, doc("mistral"))).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn aliases_only_mode_rejects_direct_models() {
    let (base, _) = mock().await;
    let cfg = Config::from_toml(&format!(
        "[server]\nallow_direct_models = false\n[providers.mistral]\napi_key = \"k\"\nbase_url = \"{base}/ok\"\n\
         [[models]]\nname = \"ocr\"\ntargets = [\"mistral/ocr-latest\"]"
    ))
    .unwrap();
    let app = puffinparse_server::app(Arc::new(AppState::from_config_quiet(cfg).unwrap()));
    let (s, _, body, _) = send(&app, post_json("/v1/parse", None, doc("mistral/ocr-latest"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"], "unsupported_model_error");
    let (s, _, _, _) = send(&app, post_json("/v1/parse", None, doc("ocr"))).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn request_log_has_ids_but_no_secrets_or_content() {
    let (base, _) = mock().await;
    let dir = std::env::temp_dir().join(format!("puffinparse-log-{}", uuid_like()));
    std::fs::create_dir_all(&dir).unwrap();
    let log_path = dir.join("requests.jsonl");
    let mut cfg = Config::from_toml(&config(&base)).unwrap();
    cfg.server.log_stdout = false;
    cfg.server.log_file = Some(log_path.clone());
    let app = puffinparse_server::app(Arc::new(AppState::from_config(cfg).unwrap()));
    let req = Request::post("/v1/parse")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, "Bearer sk-team-a")
        .header("x-request-id", "req-123")
        .body(Body::from(doc("mistral").to_string()))
        .unwrap();
    let (s, h, _, _) = send(&app, req).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["x-request-id"], "req-123");
    send(&app, post_json("/v1/parse", Some("sk-narrow"), doc("brittle"))).await;

    let text = std::fs::read_to_string(&log_path).unwrap();
    let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["request_id"], "req-123");
    assert_eq!(lines[0]["key_id"], "team-a");
    assert_eq!(lines[0]["served_model"], "mistral/ocr-latest");
    assert_eq!(lines[0]["provider"], "mistral");
    assert_eq!(lines[0]["pages"], 2);
    assert_eq!(lines[0]["status"], 200);
    assert!(lines[0]["latency_ms"].is_u64());
    assert_eq!(lines[1]["key_id"], "narrow");
    assert_eq!(lines[1]["status"], 502);
    assert_eq!(lines[1]["error_type"], "provider_error");
    assert_eq!(lines[1]["provider_status"], 500);
    for secret in ["sk-team-a", "sk-narrow", "sk-master", "provider-key-from-env", PNG_B64, "Hello LiteOCR", "exploded"]
    {
        assert!(!text.contains(secret), "log leaked {secret}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

fn uuid_like() -> String {
    format!("{}-{:?}", std::process::id(), std::time::SystemTime::now())
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

#[tokio::test]
async fn oversized_body_is_413() {
    let cfg = Config::from_toml("[server]\nmax_body_mb = 1").unwrap();
    let app = puffinparse_server::app(Arc::new(AppState::from_config_quiet(cfg).unwrap()));
    let big = "A".repeat(2 * 1024 * 1024);
    let req = post_json("/v1/parse", None, json!({ "model": "mistral", "document": big, "filename": "a.png" }));
    let (s, _, body, _) = send(&app, req).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["error"]["type"], "payload_too_large");
}

// ---- async jobs (POST /v1/jobs, GET /v1/jobs/{id}, POST /v1/webhooks/{provider}) ----------------

fn get_as(path: &str, key: &str) -> Request<Body> {
    Request::get(path).header(header::AUTHORIZATION, format!("Bearer {key}")).body(Body::empty()).unwrap()
}

async fn usage_of(app: &axum::Router, key: &str) -> Value {
    send(app, get_as("/v1/usage", key)).await.2["keys"][0].clone()
}

#[tokio::test]
async fn job_is_submitted_polled_and_charged_once() {
    let (app, m) = app().await;
    let mut req = doc("reducto/standard");
    req["webhook_url"] = json!("https://hooks.example.com/puffinparse");
    req["metadata"] = json!({ "batch": 7 });
    let (s, _, body, text) = send(&app, post_json("/v1/jobs", Some("sk-jobber"), req)).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{text}");
    let id = body["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("job_") && id.len() > 20, "{id}");
    assert_eq!(body["status"], "pending");
    assert_eq!(body["job"]["provider"], "reducto");
    assert_eq!(body["job"]["model"], "reducto/standard");
    assert_eq!(body["job"]["job_id"], "rj-1");
    assert!(body["job"].get("base_url").is_none(), "the operator's base URL is not exposed: {body}");
    assert!(!text.contains("reducto-provider-key"));
    // The document was uploaded and the webhook forwarded as Reducto's async.webhook.
    let sent = m.job_bodies.lock().unwrap()[0].clone();
    assert_eq!(sent["input"], "reducto://file-1");
    assert_eq!(sent["async"]["webhook"], json!({ "mode": "direct", "url": "https://hooks.example.com/puffinparse" }));
    // Nothing is charged at submit time.
    assert_eq!(usage_of(&app, "sk-jobber").await["requests"], 0);

    let path = format!("/v1/jobs/{id}");
    let (s, _, first, text) = send(&app, get_as(&path, "sk-jobber")).await;
    assert_eq!(s, StatusCode::OK, "{text}");
    assert_eq!(first["status"], "pending");
    assert_eq!(first["provider_job_id"], "rj-1");
    assert!(first.get("result").is_none());

    // Another key cannot see the job at all.
    let (s, _, other, _) = send(&app, get_as(&path, "sk-team-a")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(other["error"]["type"], "not_found");
    let (s, _, _, _) = send(&app, get_as("/v1/jobs/job_does_not_exist", "sk-jobber")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, _, done, text) = send(&app, get_as(&path, "sk-jobber")).await;
    assert_eq!(s, StatusCode::OK, "{text}");
    assert_eq!(done["status"], "succeeded");
    assert_eq!(done["result"]["model"], "reducto/standard");
    assert_eq!(done["result"]["provider_job_id"], "rj-1");
    assert_eq!(done["result"]["metadata"]["batch"], 7);
    assert!(done["result"]["markdown"].as_str().unwrap().contains("Hello LiteOCR"));
    let cost = done["result"]["cost_usd"].as_f64().unwrap();
    let u = usage_of(&app, "sk-jobber").await;
    assert_eq!((u["requests"].as_u64(), u["pages"].as_u64()), (Some(1), Some(1)));
    assert!((u["spend_usd"].as_f64().unwrap() - cost).abs() < 1e-12);

    // Later reads return the result again, in a vendor shape on request, without a second charge.
    let (s, _, again, _) = send(&app, get_as(&format!("{path}?output_format=reducto"), "sk-jobber")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(again["status"], "succeeded");
    assert!(again["result"]["result"]["chunks"].is_array(), "{again}");
    let (s, _, _, _) = send(&app, get_as(&format!("{path}?output_format=acme"), "sk-jobber")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // The master key can read any job.
    let (s, _, _, _) = send(&app, get_as(&path, "sk-master")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(usage_of(&app, "sk-jobber").await["requests"], 1, "charged exactly once");
    // Every provider call used the [providers.reducto] key.
    assert!(m.job_auth.lock().unwrap().iter().all(|a| a == "Bearer reducto-provider-key"));

    let metrics = send(&app, get_as("/metrics", "sk-master")).await.3;
    assert!(metrics.contains(r#"puffinparse_jobs_total{event="submitted"} 1"#), "{metrics}");
    assert!(metrics.contains(r#"puffinparse_jobs_total{event="succeeded"} 1"#), "{metrics}");
    assert!(
        metrics.contains(r#"puffinparse_requests_total{mode="job_submit",model="reducto/standard",status="202"} 1"#)
    );
    assert!(
        metrics.contains(r#"puffinparse_requests_total{mode="job_retrieve",model="reducto/standard",status="200"} 4"#),
        "{metrics}"
    );
    assert!(metrics.contains(r#"puffinparse_pages_total{model="reducto/standard"} 1"#), "{metrics}");
}

#[tokio::test]
async fn job_through_an_alias_uses_its_first_target_and_reports_failure() {
    let (app, m) = app().await;
    let mut req = doc("failing-jobs");
    req["output_format"] = json!("reducto");
    let (s, _, body, text) = send(&app, post_json("/v1/jobs", Some("sk-jobber"), req)).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{text}");
    assert_eq!(body["job"]["model"], "reducto/standard");
    let path = format!("/v1/jobs/{}", body["id"].as_str().unwrap());
    assert_eq!(send(&app, get_as(&path, "sk-jobber")).await.2["status"], "pending");
    let (s, _, failed, _) = send(&app, get_as(&path, "sk-jobber")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["error"]["type"], "provider_error");
    assert!(failed["error"]["message"].as_str().unwrap().contains("mock parse failed: corrupt PDF"), "{failed}");
    assert_eq!(failed["error"]["provider"], "reducto");
    assert!(failed["error"]["job_id"].is_string());
    // The alias target's own credentials, at submit and at retrieve.
    assert!(m.job_auth.lock().unwrap().iter().all(|a| a == "Bearer reducto-alias-key"));
    assert_eq!(usage_of(&app, "sk-jobber").await["requests"], 0, "failed jobs are not charged");
    let metrics = send(&app, get_as("/metrics", "sk-master")).await.3;
    assert!(metrics.contains(r#"puffinparse_jobs_total{event="failed"} 1"#), "{metrics}");
}

#[tokio::test]
async fn job_submission_applies_the_parse_checks() {
    let (app, m) = app().await;
    let (s, _, _, _) = send(&app, post_json("/v1/jobs", None, doc("reducto"))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, body, _) = send(&app, post_json("/v1/jobs", Some("sk-narrow"), doc("reducto"))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["type"], "model_not_allowed");
    let mut req = doc("reducto");
    req["fallbacks"] = json!(["reducto/r-1"]);
    let (s, _, body, _) = send(&app, post_json("/v1/jobs", Some("sk-jobber"), req)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"].as_str().unwrap().contains("fallbacks"));
    // Mistral has no job queue.
    let (s, _, body, _) = send(&app, post_json("/v1/jobs", Some("sk-jobber"), doc("mistral"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["type"], "unsupported_model_error");
    let mut req = doc("reducto");
    req["schema"] = json!({});
    assert_eq!(send(&app, post_json("/v1/jobs", Some("sk-jobber"), req)).await.0, StatusCode::BAD_REQUEST);
    // webhook_url is a jobs-only field.
    let mut req = doc("mistral");
    req["webhook_url"] = json!("https://hooks.example.com/x");
    assert_eq!(send(&app, post_json("/v1/parse", Some("sk-master"), req)).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(m.job_seq.load(Ordering::SeqCst), 0, "no provider job was started");

    // The rate limit applies to submissions like to /v1/parse.
    assert_eq!(send(&app, post_json("/v1/jobs", Some("sk-slow"), doc("reducto"))).await.0, StatusCode::ACCEPTED);
    let (s, h, _, _) = send(&app, post_json("/v1/jobs", Some("sk-slow"), doc("reducto"))).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert!(h.contains_key(header::RETRY_AFTER));

    // Budget $0.005; one Reducto page costs $0.015. Submitting is free until the job is seen done.
    let (_, _, job, _) = send(&app, post_json("/v1/jobs", Some("sk-broke"), doc("reducto"))).await;
    let path = format!("/v1/jobs/{}", job["id"].as_str().unwrap());
    assert_eq!(send(&app, get_as(&path, "sk-broke")).await.2["status"], "pending");
    assert_eq!(send(&app, get_as(&path, "sk-broke")).await.2["status"], "succeeded");
    let (s, _, body, _) = send(&app, post_json("/v1/jobs", Some("sk-broke"), doc("reducto"))).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(body["error"]["type"], "budget_exceeded");
    // Reading a job is never budget-gated.
    assert_eq!(send(&app, get_as(&path, "sk-broke")).await.0, StatusCode::OK);
}

#[tokio::test]
async fn multipart_job_submission() {
    let (app, m) = app().await;
    let boundary = "XBOUNDARYX";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nreducto\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"webhook_url\"\r\n\r\nhttps://h.example.com/x\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"scan.png\"\r\n\
         Content-Type: image/png\r\n\r\n\u{89}PNG\r\n--{boundary}--\r\n"
    );
    let req = Request::post("/v1/jobs")
        .header(header::AUTHORIZATION, "Bearer sk-master")
        .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
        .body(Body::from(body))
        .unwrap();
    let (s, _, body, text) = send(&app, req).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{text}");
    assert_eq!(body["job"]["model"], "reducto/standard");
    assert_eq!(m.job_bodies.lock().unwrap()[0]["async"]["webhook"]["url"], "https://h.example.com/x");
}

#[tokio::test]
async fn webhooks_are_off_unless_configured() {
    let (app, _) = app().await;
    let req = post_json("/v1/webhooks/reducto", None, json!({ "status": "Completed", "job_id": "rj-1" }));
    let (s, _, body, _) = send(&app, req).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "not_found");
}

#[tokio::test]
async fn webhook_resolves_and_charges_the_job_owner_once() {
    let (base, m) = mock().await;
    std::env::set_var("PUFFINPARSE_TEST_WEBHOOK_SECRET", "whsec-test");
    let toml =
        format!("{}\n[webhooks]\nenabled = true\nsecret = \"env:PUFFINPARSE_TEST_WEBHOOK_SECRET\"\n", config(&base));
    let app =
        puffinparse_server::app(Arc::new(AppState::from_config_quiet(Config::from_toml(&toml).unwrap()).unwrap()));
    let (s, _, job, _) = send(&app, post_json("/v1/jobs", Some("sk-jobber"), doc("reducto"))).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let id = job["id"].as_str().unwrap().to_string();
    let provider_id = job["job"]["job_id"].as_str().unwrap().to_string();
    let hook = json!({ "status": "Completed", "job_id": provider_id });

    let (s, _, _, _) = send(&app, post_json("/v1/webhooks/reducto?token=wrong", None, hook.clone())).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _, _) = send(&app, post_json("/v1/webhooks/reducto", None, hook.clone())).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let unknown = json!({ "status": "Completed", "job_id": "rj-unknown" });
    let (s, _, _, _) = send(&app, post_json("/v1/webhooks/reducto?token=whsec-test", None, unknown)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(m.job_polls.lock().unwrap().len(), 0, "no provider call before the job is matched");

    // The mock answers Pending on the first status check, Completed after.
    let (s, _, ack, text) = send(&app, post_json("/v1/webhooks/reducto?token=whsec-test", None, hook.clone())).await;
    assert_eq!(s, StatusCode::OK, "{text}");
    assert_eq!(ack, json!({ "ids": [id], "status": "pending" }));
    let req = Request::post("/v1/webhooks/reducto")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-puffinparse-webhook-secret", "whsec-test")
        .body(Body::from(hook.to_string()))
        .unwrap();
    let (s, _, ack, _) = send(&app, req).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(ack["status"], "succeeded");
    assert_eq!(usage_of(&app, "sk-jobber").await["requests"], 1);

    // The client's own poll sees the result; the charge is not repeated.
    let (_, _, done, _) = send(&app, get_as(&format!("/v1/jobs/{id}"), "sk-jobber")).await;
    assert_eq!(done["status"], "succeeded");
    assert_eq!(usage_of(&app, "sk-jobber").await["requests"], 1);
}

#[tokio::test]
async fn jobs_survive_a_restart_with_a_state_file() {
    let (base, _) = mock().await;
    let dir = std::env::temp_dir().join(format!("puffinparse-jobstate-{}", uuid_like()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = Config::from_toml(&config(&base)).unwrap();
    cfg.server.state_file = Some(dir.join("state.json"));
    let app = puffinparse_server::app(Arc::new(AppState::from_config_quiet(cfg.clone()).unwrap()));
    let (_, _, job, _) = send(&app, post_json("/v1/jobs", Some("sk-jobber"), doc("reducto"))).await;
    let path = format!("/v1/jobs/{}", job["id"].as_str().unwrap());
    let state = std::fs::read_to_string(dir.join("state.json")).unwrap();
    assert!(!state.contains("reducto-provider-key") && !state.contains("sk-jobber"), "no secrets persisted");

    let restarted = puffinparse_server::app(Arc::new(AppState::from_config_quiet(cfg).unwrap()));
    assert_eq!(send(&restarted, get_as(&path, "sk-jobber")).await.2["status"], "pending");
    assert_eq!(send(&restarted, get_as(&path, "sk-team-a")).await.0, StatusCode::NOT_FOUND);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn public_metrics_opens_the_endpoint() {
    let cfg = Config::from_toml("master_key = 'sk-m'\n[server]\npublic_metrics = true").unwrap();
    let app = puffinparse_server::app(Arc::new(AppState::from_config_quiet(cfg).unwrap()));
    let (s, _, _, _) = send(&app, Request::get("/metrics").body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::OK);
}

fn url_doc(model: &str, url: &str) -> Value {
    json!({ "model": model, "document_url": url })
}

#[tokio::test]
async fn document_url_is_refused_for_models_that_would_download_it_in_the_gateway() {
    let (base, m) = mock().await;
    let toml = |fetch: bool| {
        format!(
            "master_key = 'sk-m'\n[server]\nmax_retries = 0\nfetch_document_urls = {fetch}\n\
             [providers.mistral]\napi_key = 'k'\nbase_url = '{base}/ok'\n\
             [providers.openai]\napi_key = 'k'\nbase_url = '{base}/fail'\n\
             [providers.gemini]\napi_key = 'k'\nbase_url = '{base}/fail'\n\
             [[models]]\nname = 'mixed'\ntargets = ['mistral/ocr-latest', 'gemini']"
        )
    };
    let app = puffinparse_server::app(Arc::new(
        AppState::from_config_quiet(Config::from_toml(&toml(false)).unwrap()).unwrap(),
    ));
    let url = "https://docs.example.com/invoice.png";
    // Mistral fetches URLs itself: passed through.
    let (s, _, _, text) = send(&app, post_json("/v1/ocr", Some("sk-m"), url_doc("mistral", url))).await;
    assert_eq!(s, StatusCode::OK, "{text}");
    assert_eq!(m.ok_hits.load(Ordering::SeqCst), 1);
    // OpenAI / Gemini would download it in the gateway: refused with the reason, before any call.
    for model in ["openai", "gemini", "mixed"] {
        let (s, _, body, _) = send(&app, post_json("/v1/parse", Some("sk-m"), url_doc(model, url))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{model}: {body}");
        assert!(body["error"]["message"].as_str().unwrap().contains("fetch_document_urls"), "{body}");
    }
    // Also when only a request-level fallback would download.
    let mut req = url_doc("mistral", url);
    req["fallbacks"] = json!(["openai"]);
    let (s, _, _, _) = send(&app, post_json("/v1/ocr", Some("sk-m"), req)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(m.fail_hits.load(Ordering::SeqCst), 0);
    // Uploads are unaffected.
    let (s, _, _, _) = send(&app, post_json("/v1/ocr", Some("sk-m"), doc("mistral"))).await;
    assert_eq!(s, StatusCode::OK);

    // fetch_document_urls = true: allowed, but the download itself refuses private addresses.
    let app = puffinparse_server::app(Arc::new(
        AppState::from_config_quiet(Config::from_toml(&toml(true)).unwrap()).unwrap(),
    ));
    for url in ["http://127.0.0.1:9/a.png", "http://169.254.169.254/latest/meta-data/", "http://[::1]/a.png"] {
        let (s, _, body, _) = send(&app, post_json("/v1/parse", Some("sk-m"), url_doc("openai", url))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{url}: {body}");
        assert!(body["error"]["message"].as_str().unwrap().contains("non-public"), "{body}");
    }
    assert_eq!(m.fail_hits.load(Ordering::SeqCst), 0, "the provider was never called");
}

#[tokio::test]
async fn local_engines_need_an_explicit_grant() {
    let (base, _) = mock().await;
    let toml = |allow: bool| {
        format!(
            "master_key = 'sk-m'\n[server]\nmax_retries = 0\nallow_local_engines = {allow}\n\
             [providers.docling]\nbase_url = '{base}/fail'\n\
             [[models]]\nname = 'layout'\ntargets = ['docling']\n\
             [[keys]]\nid = 'all'\nkey = 'sk-all'\n\
             [[keys]]\nid = 'star'\nkey = 'sk-star'\nmodels = ['*']\n\
             [[keys]]\nid = 'named'\nkey = 'sk-named'\nmodels = ['docling/*']\n\
             [[keys]]\nid = 'alias'\nkey = 'sk-alias'\nmodels = ['layout']"
        )
    };
    let app = puffinparse_server::app(Arc::new(
        AppState::from_config_quiet(Config::from_toml(&toml(false)).unwrap()).unwrap(),
    ));
    for key in ["sk-all", "sk-star"] {
        let (s, _, body, _) = send(&app, post_json("/v1/parse", Some(key), doc("docling"))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{key}: {body}");
        assert!(body["error"]["message"].as_str().unwrap().contains("allow_local_engines"), "{body}");
        let mut fb = doc("reducto");
        fb["fallbacks"] = json!(["docling/default"]);
        assert_eq!(send(&app, post_json("/v1/parse", Some(key), fb)).await.0, StatusCode::FORBIDDEN);
        // ...and /v1/models does not offer it.
        let models = send(&app, get_as("/v1/models", key)).await.2;
        assert!(!models.to_string().contains("docling/default"), "{models}");
    }
    // Named by the key, behind an alias, or the master key: allowed (the mock then fails the call).
    for (key, model) in [("sk-named", "docling"), ("sk-alias", "layout"), ("sk-m", "docling")] {
        let (s, _, body, _) = send(&app, post_json("/v1/parse", Some(key), doc(model))).await;
        assert_ne!(s, StatusCode::FORBIDDEN, "{key} {model}: {body}");
    }
    let app = puffinparse_server::app(Arc::new(
        AppState::from_config_quiet(Config::from_toml(&toml(true)).unwrap()).unwrap(),
    ));
    let (s, _, body, _) = send(&app, post_json("/v1/parse", Some("sk-all"), doc("docling"))).await;
    assert_ne!(s, StatusCode::FORBIDDEN, "{body}");
}

#[test]
fn refuses_an_open_gateway_on_a_public_address() {
    let state = |toml: &str| AppState::from_config_quiet(Config::from_toml(toml).unwrap()).unwrap();
    let err = state("[server]\nhost = '0.0.0.0'").check_exposure().unwrap_err();
    assert!(err.contains("allow_unauthenticated") && err.contains("master_key"), "{err}");
    assert!(state("[server]\nhost = '0.0.0.0'\nallow_unauthenticated = true").check_exposure().is_ok());
    assert!(state("master_key = 'sk'\n[server]\nhost = '0.0.0.0'").check_exposure().is_ok());
    assert!(state("[[keys]]\nid = 'a'\nkey = 'k'\n[server]\nhost = '10.0.0.5'").check_exposure().is_ok());
    for host in ["127.0.0.1", "localhost", "::1", "[::1]", "127.0.0.2"] {
        assert!(state(&format!("[server]\nhost = '{host}'")).check_exposure().is_ok(), "{host}");
    }
    assert!(state("[server]\nhost = 'gateway.internal'").check_exposure().is_err());
}

#[test]
fn debug_output_never_contains_secrets() {
    std::env::set_var("PUFFINPARSE_TEST_DEBUG_KEY", "env-resolved-SECRET");
    let cfg = Config::from_toml(
        "master_key = 'master-SECRET'\n\
         [providers.reducto]\napi_key = 'provider-SECRET'\n\
         [providers.mistral]\napi_key = 'env:PUFFINPARSE_TEST_DEBUG_KEY'\n\
         [[models]]\nname = 'a'\ntargets = [{ model = 'reducto/standard', api_key = 'target-SECRET' }]\n\
         [[keys]]\nid = 'team'\nkey = 'vkey-SECRET'\n\
         [webhooks]\nenabled = true\nsecret = 'hook-SECRET'",
    )
    .unwrap();
    assert!(!format!("{cfg:?}").contains("SECRET"));
    let state = AppState::from_config_quiet(cfg).unwrap();
    let dbg = format!("{state:?} {state:#?}");
    assert!(!dbg.contains("SECRET"), "{dbg}");
    assert!(dbg.contains("team") && dbg.contains("reducto/standard"), "{dbg}");
}

#[tokio::test]
async fn request_timeout_and_concurrency_limit() {
    let (base, _) = mock().await;
    let cfg = Config::from_toml(&format!(
        "master_key = 'sk-m'\n[server]\nmax_retries = 0\nmax_timeout_secs = 0.4\nrequest_timeout_secs = 0.7\n\
         max_concurrent_requests = 1\n[providers.mistral]\napi_key = 'k'\nbase_url = '{base}/slow'"
    ))
    .unwrap();
    let app = puffinparse_server::app(Arc::new(AppState::from_config_quiet(cfg).unwrap()));
    let (a, b) = (app.clone(), app.clone());
    let first = tokio::spawn(async move { send(&a, post_json("/v1/ocr", Some("sk-m"), doc("mistral"))).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let second = tokio::spawn(async move { send(&b, post_json("/v1/ocr", Some("sk-m"), doc("mistral"))).await });
    // /health is outside the limit and answers while both document requests are busy.
    let (s, _, _, _) = send(&app, Request::get("/health").body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::OK);
    let (s1, _, b1, _) = first.await.unwrap();
    let (s2, _, b2, _) = second.await.unwrap();
    // The first hits the provider deadline (0.4 s); the second waited for the only slot and then
    // ran into the gateway's own request limit (0.7 s).
    assert_eq!(s1, StatusCode::GATEWAY_TIMEOUT, "{b1}");
    assert_eq!(s2, StatusCode::GATEWAY_TIMEOUT, "{b2}");
    assert!(b2["error"]["message"].as_str().unwrap().contains("request_timeout_secs"), "{b2}");
}
