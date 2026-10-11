//! Playground API tests (`/v1/playground/*` and playground jobs on `GET /v1/jobs/{id}`).
//!
//! One loopback mock plays every outside party: Reducto's async job flow (the recorded parse
//! fixture), Cloudflare Turnstile's siteverify, and Supabase's `playground_*` RPC functions. Sign-in
//! tokens are HS256 JWTs signed with the configured legacy `jwt_secret`.

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::routing::{get, post};
use http_body_util::BodyExt;
use puffinparse_server::{AppState, Config};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const REDUCTO_RESULT: &str = include_str!("../../puffinparse-core/tests/fixtures/reducto_parse.json");
const JWT_SECRET: &str = "test-jwt-secret-at-least-32-bytes-long!!";
const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
const USER_A: &str = "6f1c2c1e-7d1f-4a5c-9b0e-1f2a3b4c5d6e";
const USER_B: &str = "0b7f4f39-1111-4e2e-8f00-aaaaaaaaaaaa";

#[derive(Default)]
struct Mock {
    job_seq: AtomicUsize,
    job_polls: Mutex<HashMap<String, usize>>,
    /// `Authorization` of every Reducto call.
    reducto_auth: Mutex<Vec<String>>,
    /// Body of every Reducto `parse_async` submit.
    reducto_bodies: Mutex<Vec<Value>>,
    turnstile_calls: AtomicUsize,
    /// Supabase RPC: calls (name, body, apikey, authorization) and the counters it keeps.
    rpc_calls: Mutex<Vec<(String, Value, String, String)>>,
    rpc_users: Mutex<HashMap<String, u64>>,
    rpc_spent: Mutex<f64>,
}

fn auth_of(h: &HeaderMap) -> String {
    h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

async fn mock() -> (String, Arc<Mock>) {
    let m = Arc::new(Mock::default());
    let (a, b, c, d) = (m.clone(), m.clone(), m.clone(), m.clone());
    let app = axum::Router::new()
        .route("/rd/upload", post(|| async { axum::Json(json!({ "file_id": "reducto://file-1" })) }))
        .route(
            "/rd/parse_async",
            post(move |h: HeaderMap, axum::Json(v): axum::Json<Value>| async move {
                a.reducto_auth.lock().unwrap().push(auth_of(&h));
                a.reducto_bodies.lock().unwrap().push(v);
                let n = a.job_seq.fetch_add(1, Ordering::SeqCst) + 1;
                axum::Json(json!({ "job_id": format!("rj-{n}") }))
            }),
        )
        .route(
            "/rd/job/{id}",
            get(move |axum::extract::Path(id): axum::extract::Path<String>, h: HeaderMap| async move {
                b.reducto_auth.lock().unwrap().push(auth_of(&h));
                let polls = {
                    let mut p = b.job_polls.lock().unwrap();
                    let n = p.entry(id.clone()).or_default();
                    *n += 1;
                    *n
                };
                if polls == 1 {
                    return axum::Json(json!({ "status": "Pending" }));
                }
                let mut result: Value = serde_json::from_str(REDUCTO_RESULT).unwrap();
                result["job_id"] = json!(id);
                axum::Json(json!({ "status": "Completed", "result": result }))
            }),
        )
        .route(
            "/turnstile",
            post(move |axum::Json(v): axum::Json<Value>| async move {
                c.turnstile_calls.fetch_add(1, Ordering::SeqCst);
                let ok = v["secret"] == "ts-secret" && v["response"] == "good-token";
                axum::Json(json!({ "success": ok }))
            }),
        )
        .route(
            "/sb/rest/v1/rpc/{name}",
            post(
                move |axum::extract::Path(name): axum::extract::Path<String>,
                      h: HeaderMap,
                      axum::Json(v): axum::Json<Value>| async move {
                    let apikey = h.get("apikey").and_then(|x| x.to_str().ok()).unwrap_or("").to_string();
                    d.rpc_calls.lock().unwrap().push((name.clone(), v.clone(), apikey, auth_of(&h)));
                    let mut users = d.rpc_users.lock().unwrap();
                    let mut spent = d.rpc_spent.lock().unwrap();
                    let body = match name.as_str() {
                        "playground_reserve" => {
                            let user = v["p_user"].as_str().unwrap().to_string();
                            let (pages, limit) = (v["p_pages"].as_u64().unwrap(), v["p_day_limit"].as_u64().unwrap());
                            let used = users.get(&user).copied().unwrap_or(0);
                            if *spent >= v["p_budget_usd"].as_f64().unwrap() {
                                json!([{ "ok": false, "reason": "budget_exhausted", "remaining": limit - used }])
                            } else if used + pages > limit {
                                json!([{ "ok": false, "reason": "quota_exceeded", "remaining": limit - used }])
                            } else {
                                users.insert(user, used + pages);
                                json!([{ "ok": true, "reason": null, "remaining": limit - used - pages }])
                            }
                        }
                        "playground_release" => {
                            let e = users.entry(v["p_user"].as_str().unwrap().to_string()).or_default();
                            *e = e.saturating_sub(v["p_pages"].as_u64().unwrap());
                            Value::Null
                        }
                        "playground_charge" => {
                            *spent += v["p_usd"].as_f64().unwrap();
                            json!(*spent)
                        }
                        "playground_status" => {
                            let used = users.get(v["p_user"].as_str().unwrap()).copied().unwrap_or(0);
                            json!([{ "remaining": v["p_day_limit"].as_u64().unwrap() - used, "spent_usd": *spent }])
                        }
                        _ => return (StatusCode::NOT_FOUND, axum::Json(json!({}))),
                    };
                    (StatusCode::OK, axum::Json(body))
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), m)
}

/// `extra` goes inside `[playground]`; `supabase` is the whole `[playground.supabase]` body.
fn config(base: &str, extra: &str, supabase: &str) -> String {
    // A test may override the budget in `extra`.
    let budget = if extra.contains("free_daily_budget_usd") { "" } else { "free_daily_budget_usd = 1.0" };
    format!(
        r#"
        master_key = "sk-master"
        [server]
        max_retries = 0
        [providers.reducto]
        api_key = "gateway-reducto-key"
        base_url = "{base}/rd"

        [playground]
        enabled = true
        allowed_origins = ["https://puffinparse.com"]
        models = ["reducto/standard", "reducto/r-1", "reducto/agentic"]
        {budget}
        free_model_pages_per_day = 5
        turnstile_secret = "ts-secret"
        turnstile_verify_url = "{base}/turnstile"
        trust_forwarded_for = 1
        {extra}

        [playground.supabase]
        url = "{base}/sb"
        jwt_secret = "{JWT_SECRET}"
        {supabase}
        "#
    )
}

async fn app_with(extra: &str, supabase: &str) -> (axum::Router, Arc<Mock>) {
    let (base, m) = mock().await;
    let cfg = Config::from_toml(&config(&base, extra, supabase)).unwrap();
    let state = AppState::from_config_quiet(cfg).unwrap();
    (puffinparse_server::app(Arc::new(state)), m)
}

async fn app() -> (axum::Router, Arc<Mock>) {
    app_with("", "").await
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, HeaderMap, Value, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    (status, headers, serde_json::from_str(&text).unwrap_or(Value::Null), text)
}

fn token(sub: &str, base_iss: &str) -> String {
    let exp = chrono::Utc::now().timestamp() + 600;
    let claims = json!({ "sub": sub, "aud": "authenticated", "iss": base_iss, "exp": exp, "role": "authenticated" });
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )
    .unwrap()
}

/// The issuer the gateway expects: `<supabase.url>/auth/v1`.
fn issuer(app_base: &str) -> String {
    format!("{app_base}/sb/auth/v1")
}

const BOUNDARY: &str = "pgboundary";

fn form(file: &[u8], models: &[&str], turnstile: Option<&str>) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.bin\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    b.extend_from_slice(file);
    b.extend_from_slice(b"\r\n");
    for m in models {
        b.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"models\"\r\n\r\n{m}\r\n").as_bytes(),
        );
    }
    if let Some(t) = turnstile {
        b.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"turnstile_token\"\r\n\r\n{t}\r\n")
                .as_bytes(),
        );
    }
    b.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    b
}

fn run_req(body: Vec<u8>, headers: &[(&str, &str)]) -> Request<Body> {
    let mut r = Request::post("/v1/playground/runs")
        .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={BOUNDARY}"))
        .header("x-forwarded-for", "203.0.113.7");
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    r.body(Body::from(body)).unwrap()
}

fn get_with(path: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut r = Request::get(path);
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    r.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn off_by_default() {
    let state = AppState::from_config_quiet(Config::from_toml("master_key = 'sk'").unwrap()).unwrap();
    let app = puffinparse_server::app(Arc::new(state));
    let (s, _, body, _) = send(&app, get_with("/v1/playground/config", &[])).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "not_found");
    let (s, _, _, _) = send(&app, run_req(form(PNG, &["reducto/r-1"], None), &[])).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // No CORS when the playground is off.
    let pre = Request::options("/v1/jobs/x")
        .header("origin", "https://puffinparse.com")
        .header("access-control-request-method", "GET")
        .body(Body::empty())
        .unwrap();
    let (_, h, _, _) = send(&app, pre).await;
    assert!(h.get("access-control-allow-origin").is_none());
}

#[test]
fn config_is_validated() {
    let ok = "[playground]\nenabled = true\nallowed_origins = ['https://puffinparse.com']\nmodels = ['reducto/r-1']";
    assert!(Config::from_toml(ok).is_ok());
    assert!(Config::from_toml("[playground]\nenabled = true\nmodels = ['reducto/r-1']").is_err(), "origins");
    assert!(Config::from_toml(&ok.replace("https://puffinparse.com", "*")).is_err());
    assert!(Config::from_toml(&ok.replace("https://puffinparse.com", "https://puffinparse.com/")).is_err());
    assert!(Config::from_toml(&ok.replace("reducto/r-1", "mistral/ocr-latest")).is_err(), "no job queue");
    assert!(Config::from_toml(&ok.replace("reducto/r-1", "nope/x")).is_err());
    assert!(Config::from_toml(&format!("{ok}\nmax_models = 0")).is_err());
    assert!(Config::from_toml(&format!("{ok}\nfree_daily_budget_usd = -1")).is_err());
    assert!(Config::from_toml(&format!("{ok}\nsurprise = 1")).is_err());
    assert!(Config::from_toml(&format!("{ok}\n[playground.supabase]\nservice_key = 'x'")).is_err(), "needs url");
}

#[test]
fn debug_hides_playground_secrets() {
    let cfg = Config::from_toml(
        "[playground]\nenabled = true\nallowed_origins = ['https://puffinparse.com']\nmodels = ['reducto/r-1']\n\
         turnstile_secret = 'TS-LITERAL'\n[playground.supabase]\nurl = 'https://abc.supabase.co'\n\
         jwt_secret = 'JWT-LITERAL'\nservice_key = 'SVC-LITERAL'",
    )
    .unwrap();
    let state = AppState::from_config_quiet(cfg.clone()).unwrap();
    let dbg = format!("{cfg:?} {cfg:#?} {state:?} {state:#?}");
    assert!(!dbg.contains("LITERAL"), "{dbg}");
}

#[tokio::test]
async fn config_reports_models_limits_and_the_free_tier() {
    let (app, _) = app().await;
    let (s, _, c, text) = send(&app, get_with("/v1/playground/config", &[])).await;
    assert_eq!(s, StatusCode::OK, "{text}");
    assert_eq!(c["limits"]["models_per_run"], 3);
    assert_eq!(c["limits"]["pages_per_run"], 10);
    assert_eq!(c["limits"]["max_file_bytes"], 4 * 1024 * 1024);
    assert_eq!(c["limits"]["model_pages_per_day"], 5);
    assert_eq!(c["free_tier"]["available"], true);
    assert_eq!(c["free_tier"]["user"], Value::Null);
    assert_eq!(c["byok"]["available"], true);
    let models: HashMap<String, Value> =
        c["models"].as_array().unwrap().iter().map(|m| (m["id"].as_str().unwrap().to_string(), m.clone())).collect();
    assert_eq!(models["reducto/r-1"]["free_tier"], true);
    assert_eq!(models["reducto/standard"]["free_tier"], true);
    assert_eq!(models["reducto/agentic"]["free_tier"], false, "list price above the ceiling");
    assert_eq!(models["reducto/r-1"]["provider_name"], "Reducto");

    // A bad token is refused; no free tier without Turnstile.
    let (s, _, _, _) = send(&app, get_with("/v1/playground/config", &[("authorization", "Bearer eyJx.eyJ.y")])).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (app, _) = app_with("free_daily_budget_usd = 0", "").await;
    let c = send(&app, get_with("/v1/playground/config", &[])).await.2;
    assert_eq!(c["free_tier"], json!({ "available": false, "reason": "paused", "user": null }));
}

/// Builds the app and returns it with the mock and the issuer string tokens must carry.
async fn app_and_issuer(extra: &str, supabase: &str) -> (axum::Router, Arc<Mock>, String) {
    let (base, m) = mock().await;
    let cfg = Config::from_toml(&config(&base, extra, supabase)).unwrap();
    let state = AppState::from_config_quiet(cfg).unwrap();
    (puffinparse_server::app(Arc::new(state)), m, issuer(&base))
}

/// A free-tier job is limited to the pages that were reserved for it, so a PDF the gateway
/// undercounted cannot run more pages on the gateway's key; an own-key job is not limited.
#[tokio::test]
async fn free_tier_jobs_are_capped_to_the_reserved_pages() {
    let (app, m, iss) = app_and_issuer("", "").await;
    let bearer = format!("Bearer {}", token(USER_A, &iss));
    let (s, _, _, text) =
        send(&app, run_req(form(PNG, &["reducto/r-1"], Some("good-token")), &[("authorization", bearer.as_str())]))
            .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{text}");
    let key = [("x-provider-key-reducto", "user-reducto-key")];
    let (s, _, _, text) = send(&app, run_req(form(PNG, &["reducto/standard"], None), &key)).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{text}");
    let bodies = m.reducto_bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["settings"]["page_range"], json!([{ "start": 1, "end": 1 }]), "{}", bodies[0]);
    assert!(bodies[1]["settings"].get("page_range").is_none(), "{}", bodies[1]);
}

#[tokio::test]
async fn own_key_run_uses_only_the_callers_key_and_only_it_can_poll() {
    let (app, m, _) = app_and_issuer("", "").await;
    let key = [("x-provider-key-reducto", "user-reducto-key")];
    let (s, _, run, text) = send(&app, run_req(form(PNG, &["reducto/standard", "reducto/agentic"], None), &key)).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{text}");
    assert_eq!(run["mode"], "byok");
    assert_eq!(run["pages"], 1);
    assert_eq!(run["usage"], Value::Null);
    let jobs = run["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2, "dearer models are fine with your own key");
    assert!(jobs.iter().all(|j| j["status"] == "pending" && j["id"].as_str().unwrap().starts_with("job_")));
    assert!(!text.contains("user-reducto-key"));
    let id = jobs[0]["id"].as_str().unwrap().to_string();
    let path = format!("/v1/jobs/{id}");

    // Nobody else can read it: no key, another key, a gateway key, a sign-in token.
    for h in
        [vec![], vec![("x-provider-key-reducto", "someone-elses-key")], vec![("authorization", "Bearer sk-not-a-key")]]
    {
        let (s, _, body, _) = send(&app, get_with(&path, &h)).await;
        assert!(s == StatusCode::NOT_FOUND || s == StatusCode::UNAUTHORIZED, "{s} {body}");
    }
    let (s, _, first, _) = send(&app, get_with(&path, &key)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(first["status"], "pending");
    let (s, _, done, _) = send(&app, get_with(&path, &key)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(done["status"], "succeeded");
    assert!(done["result"]["markdown"].as_str().unwrap().contains("Hello LiteOCR"));
    // Every provider call (2 submits, 2 polls) carried the caller's key, never the gateway's.
    let auth = m.reducto_auth.lock().unwrap().clone();
    assert_eq!(auth.len(), 4, "{auth:?}");
    assert!(auth.iter().all(|a| a == "Bearer user-reducto-key"), "{auth:?}");
    // The master key may read it but has no key to poll the provider with.
    let (s, _, body, _) = send(&app, get_with(&path, &[("authorization", "Bearer sk-master")])).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    // Nothing is charged to any gateway key.
    let usage = send(&app, get_with("/v1/usage", &[("authorization", "Bearer sk-master")])).await.2;
    assert_eq!(usage["keys"], json!([]));
}

#[tokio::test]
async fn free_run_reserves_pages_checks_turnstile_and_settles() {
    let (app, m, iss) = app_and_issuer("", "").await;
    let jwt = token(USER_A, &iss);
    let bearer = format!("Bearer {jwt}");
    let auth = [("authorization", bearer.as_str())];

    // Turnstile is required and checked.
    let (s, _, e, _) = send(&app, run_req(form(PNG, &["reducto/r-1"], None), &auth)).await;
    assert_eq!((s, e["error"]["type"].as_str()), (StatusCode::FORBIDDEN, Some("turnstile_failed")));
    let (s, _, _, _) = send(&app, run_req(form(PNG, &["reducto/r-1"], Some("bad-token")), &auth)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Dearer models are own-key only.
    let (s, _, e, _) = send(&app, run_req(form(PNG, &["reducto/agentic"], Some("good-token")), &auth)).await;
    assert_eq!((s, e["error"]["type"].as_str()), (StatusCode::FORBIDDEN, Some("model_not_allowed")));

    let (s, _, run, text) =
        send(&app, run_req(form(PNG, &["reducto/r-1", "reducto/standard"], Some("good-token")), &auth)).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{text}");
    assert_eq!(run["mode"], "free");
    assert_eq!(run["usage"]["model_pages_remaining"], 3, "5 a day, 1 page x 2 models");
    let c = send(&app, get_with("/v1/playground/config", &auth)).await.2;
    assert_eq!(c["free_tier"]["user"]["model_pages_remaining"], 3);

    // The gateway's own key pays.
    assert!(m.reducto_auth.lock().unwrap().iter().all(|a| a == "Bearer gateway-reducto-key"));

    // Only the same user can poll.
    let id = run["jobs"][0]["id"].as_str().unwrap().to_string();
    let path = format!("/v1/jobs/{id}");
    let other = format!("Bearer {}", token(USER_B, &iss));
    let (s, _, _, _) = send(&app, get_with(&path, &[("authorization", other.as_str())])).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(send(&app, get_with(&path, &auth)).await.2["status"], "pending");
    let (s, _, done, _) = send(&app, get_with(&path, &auth)).await;
    assert_eq!((s, done["status"].as_str()), (StatusCode::OK, Some("succeeded")));

    // Over the daily allowance: 1 page x 3 models > 3 left... with 2 models it fits exactly once.
    let (s, _, e, _) =
        send(&app, run_req(form(PNG, &["reducto/r-1", "reducto/standard", "reducto/r-1"], Some("good-token")), &auth))
            .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "duplicate model: {e}");
    let four = form(PNG, &["reducto/r-1", "reducto/standard"], Some("good-token"));
    let (s, _, _, _) = send(&app, run_req(four.clone(), &auth)).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (s, _, e, _) = send(&app, run_req(four, &auth)).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(e["error"]["type"], "quota_exceeded");
    assert_eq!(e["error"]["details"], json!({ "requested": 2, "remaining": 1, "limit": 5 }));
    // Another user has their own allowance.
    let (s, _, _, _) =
        send(&app, run_req(form(PNG, &["reducto/r-1"], Some("good-token")), &[("authorization", other.as_str())]))
            .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert!(m.turnstile_calls.load(Ordering::SeqCst) >= 4);
}

#[tokio::test]
async fn free_spend_is_capped_per_day() {
    // reducto/r-1 lists at $0.01/page. A $0.005 budget cannot cover one page: refused up front.
    let (app, _, iss) = app_and_issuer("free_daily_budget_usd = 0.005", "").await;
    let bearer = format!("Bearer {}", token(USER_A, &iss));
    let auth = [("authorization", bearer.as_str())];
    let (s, _, e, _) = send(&app, run_req(form(PNG, &["reducto/r-1"], Some("good-token")), &auth)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(e["error"]["details"]["reason"], "budget_exhausted");

    // $0.015 covers one run. The spend is reserved at submit time, so a second run is refused
    // even though nobody has polled the first job (which is how it was bypassed before).
    let (app, _, iss) = app_and_issuer("free_daily_budget_usd = 0.015", "").await;
    let bearer = format!("Bearer {}", token(USER_A, &iss));
    let auth = [("authorization", bearer.as_str())];
    let (s, _, _, _) = send(&app, run_req(form(PNG, &["reducto/r-1"], Some("good-token")), &auth)).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let other = format!("Bearer {}", token(USER_B, &iss));
    let (s, _, e, _) =
        send(&app, run_req(form(PNG, &["reducto/r-1"], Some("good-token")), &[("authorization", other.as_str())]))
            .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{e}");
    assert_eq!(e["error"]["details"]["reason"], "budget_exhausted");
}

#[tokio::test]
async fn supabase_counters_are_used_when_a_service_key_is_set() {
    let (app, m, iss) = app_and_issuer("", "service_key = \"sb_secret_test\"").await;
    let bearer = format!("Bearer {}", token(USER_A, &iss));
    let auth = [("authorization", bearer.as_str())];
    let (s, _, run, text) = send(&app, run_req(form(PNG, &["reducto/r-1"], Some("good-token")), &auth)).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{text}");
    assert_eq!(run["usage"]["model_pages_remaining"], 4);
    let path = format!("/v1/jobs/{}", run["jobs"][0]["id"].as_str().unwrap());
    send(&app, get_with(&path, &auth)).await;
    assert_eq!(send(&app, get_with(&path, &auth)).await.2["status"], "succeeded");
    let calls = m.rpc_calls.lock().unwrap().clone();
    let reserve = calls.iter().find(|c| c.0 == "playground_reserve").expect("reserved");
    // The list-price estimate ($0.01 x 1 page) is reserved with the pages, in one call.
    assert_eq!(reserve.1["p_pages"], 1);
    assert_eq!(reserve.1["p_day_limit"], 5);
    assert_eq!(reserve.1["p_budget_usd"], 1.0);
    assert!((reserve.1["p_usd"].as_f64().unwrap() - 0.01).abs() < 1e-9, "{reserve:?}");
    // The new-style secret key goes in `apikey` only, never as a bearer.
    assert!(calls.iter().all(|c| c.2 == "sb_secret_test" && c.3.is_empty()), "{calls:?}");
}

/// (form body, extra headers, expected status, expected error type)
type Case = (Vec<u8>, Vec<(&'static str, String)>, StatusCode, &'static str);

#[tokio::test]
async fn runs_are_validated_before_any_provider_call() {
    let (app, m, iss) = app_and_issuer("", "").await;
    let key = [("x-provider-key-reducto", "k")];
    let cases: Vec<Case> = vec![
        (form(PNG, &["reducto/r-1"], None), vec![], StatusCode::UNAUTHORIZED, "unauthorized"),
        (
            form(PNG, &["reducto/r-1"], None),
            vec![("x-provider-key-reducto", "k".into()), ("authorization", format!("Bearer {}", token(USER_A, &iss)))],
            StatusCode::BAD_REQUEST,
            "input_error",
        ),
        (
            form(b"hello, not a document", &["reducto/r-1"], None),
            vec![],
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
        ),
        (form(PNG, &[], None), vec![], StatusCode::BAD_REQUEST, "input_error"),
        (
            form(PNG, &["reducto/r-1", "reducto/standard", "reducto/agentic", "reducto/r-1"], None),
            vec![],
            StatusCode::BAD_REQUEST,
            "too_many_models",
        ),
        (form(PNG, &["mistral/ocr-latest"], None), vec![], StatusCode::FORBIDDEN, "model_not_allowed"),
        (form(&eleven_page_pdf(), &["reducto/r-1"], None), vec![], StatusCode::BAD_REQUEST, "too_many_pages"),
        (
            form(&vec![0xff; 5 * 1024 * 1024], &["reducto/r-1"], None),
            vec![],
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
        ),
        (
            form(PNG, &["reducto/r-1"], None),
            vec![("x-provider-key-extend", "k".into())],
            StatusCode::BAD_REQUEST,
            "missing_provider_key",
        ),
    ];
    for (i, (body, headers, want, kind)) in cases.into_iter().enumerate() {
        let mut h: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        if h.is_empty() && i > 0 {
            h.extend_from_slice(&key);
        }
        let (s, _, e, text) = send(&app, run_req(body, &h)).await;
        assert_eq!((s, e["error"]["type"].as_str()), (want, Some(kind)), "case {i}: {text}");
    }
    assert!(m.reducto_auth.lock().unwrap().is_empty(), "no provider was called");
}

fn eleven_page_pdf() -> Vec<u8> {
    let mut pdf = b"%PDF-1.4\n".to_vec();
    for i in 0..11 {
        pdf.extend_from_slice(format!("{} 0 obj << /Type /Page >> endobj\n", i + 2).as_bytes());
    }
    pdf
}

#[tokio::test]
async fn runs_are_rate_limited_per_client_ip() {
    let (app, _, _) = app_and_issuer("ip_rpm = 2", "").await;
    let key = [("x-provider-key-reducto", "k")];
    for _ in 0..2 {
        assert_eq!(send(&app, run_req(form(PNG, &["reducto/r-1"], None), &key)).await.0, StatusCode::ACCEPTED);
    }
    let (s, h, e, _) = send(&app, run_req(form(PNG, &["reducto/r-1"], None), &key)).await;
    assert_eq!((s, e["error"]["type"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some("ip_rate_limited")));
    assert!(h.get("retry-after").is_some());
    // A different client address (the trusted proxy's entry) has its own budget; a spoofed
    // left-most entry does not help.
    let other = Request::post("/v1/playground/runs")
        .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={BOUNDARY}"))
        .header("x-forwarded-for", "203.0.113.7, 198.51.100.4")
        .header("x-provider-key-reducto", "k")
        .body(Body::from(form(PNG, &["reducto/r-1"], None)))
        .unwrap();
    assert_eq!(send(&app, other).await.0, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn cors_allows_only_the_configured_origin() {
    let (app, _, _) = app_and_issuer("", "").await;
    let preflight = |origin: &str, path: &str| {
        Request::options(path)
            .header("origin", origin)
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "x-provider-key-reducto,authorization")
            .body(Body::empty())
            .unwrap()
    };
    let (_, h, _, _) = send(&app, preflight("https://puffinparse.com", "/v1/playground/runs")).await;
    assert_eq!(h.get("access-control-allow-origin").unwrap(), "https://puffinparse.com");
    let allowed = h.get("access-control-allow-headers").unwrap().to_str().unwrap().to_ascii_lowercase();
    assert!(allowed.contains("x-provider-key-reducto") && allowed.contains("authorization"), "{allowed}");
    assert!(h.get("access-control-allow-credentials").is_none());
    let (_, h, _, _) = send(&app, preflight("https://evil.example", "/v1/playground/runs")).await;
    assert!(h.get("access-control-allow-origin").is_none());
    let (_, h, _, _) = send(&app, preflight("https://puffinparse.com", "/v1/jobs/job_x")).await;
    assert_eq!(h.get("access-control-allow-origin").unwrap(), "https://puffinparse.com");
    // The gateway's own API is not opened to browsers.
    let (_, h, _, _) = send(&app, preflight("https://puffinparse.com", "/v1/parse")).await;
    assert!(h.get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn polls_are_rate_limited_per_owner() {
    let (app, _, _) = app_and_issuer("poll_rpm = 2", "").await;
    let key = [("x-provider-key-reducto", "user-key")];
    let run = send(&app, run_req(form(PNG, &["reducto/r-1"], None), &key)).await.2;
    let path = format!("/v1/jobs/{}", run["jobs"][0]["id"].as_str().unwrap());
    assert_eq!(send(&app, get_with(&path, &key)).await.0, StatusCode::OK);
    assert_eq!(send(&app, get_with(&path, &key)).await.0, StatusCode::OK);
    assert_eq!(send(&app, get_with(&path, &key)).await.0, StatusCode::TOO_MANY_REQUESTS);
}
