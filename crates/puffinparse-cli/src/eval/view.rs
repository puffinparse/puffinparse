//! `puffinparse eval view`: a local viewer for one run.
//!
//! A small HTTP server on `127.0.0.1` (never another interface) serves a vanilla-JS page (ADR-17)
//! that shows each document next to every model's output, with agreement, failures, cost and
//! reviews, and lets the user record a verdict. Nothing leaves the machine.
//!
//! Every route sits under a random path token printed with the address, and requests whose `Host`
//! is not the loopback address are refused, so another web page in the browser cannot read the
//! run (DNS rebinding) or guess the address. Only documents listed in the run are served, by id.
//! HTML documents are served as plain text so they can never run script in the viewer's origin.

use super::{analyze, run};
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const INDEX_HTML: &str = include_str!("view/index.html");
const APP_JS: &str = include_str!("view/app.js");
const STYLES_CSS: &str = include_str!("view/styles.css");

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; frame-src 'self'; \
                   object-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

struct ViewState {
    dir: PathBuf,
    port: u16,
}

/// A running viewer.
#[derive(Debug, Clone)]
pub struct Viewer {
    pub url: String,
}

/// Start serving `run_dir` on `127.0.0.1:<port>` (0: any free port) in a background task.
pub async fn start(run_dir: &Path, port: u16) -> Result<Viewer, String> {
    let dir = std::fs::canonicalize(run_dir).map_err(|e| format!("cannot read {}: {e}", run_dir.display()))?;
    run::load_manifest(&dir)?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let state = Arc::new(ViewState { dir, port });
    let app = router(&token, state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!(error = %e, "eval view: server stopped");
        }
    });
    Ok(Viewer { url: format!("http://127.0.0.1:{port}/{token}/") })
}

fn router(token: &str, state: Arc<ViewState>) -> Router {
    let p = |s: &str| format!("/{token}/{s}");
    Router::new()
        .route(&p(""), get(index))
        .route(&p("app.js"), get(|| async { asset(APP_JS, "text/javascript; charset=utf-8") }))
        .route(&p("styles.css"), get(|| async { asset(STYLES_CSS, "text/css; charset=utf-8") }))
        .route(&p("api/run"), get(api_run))
        .route(&p("api/output"), get(api_output))
        .route(&p("api/review"), post(api_review))
        .route(&p("doc"), get(api_doc))
        .layer(axum::middleware::from_fn_with_state(state.clone(), loopback_only))
        .with_state(state)
}

async fn loopback_only(
    State(st): State<Arc<ViewState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let ok = [format!("127.0.0.1:{}", st.port), format!("localhost:{}", st.port)].iter().any(|h| h == host);
    if !ok {
        return (StatusCode::FORBIDDEN, "the viewer only answers on 127.0.0.1").into_response();
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}

fn asset(body: &'static str, ctype: &'static str) -> Response {
    ([(header::CONTENT_TYPE, ctype)], body).into_response()
}

async fn index() -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8")),
            (header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP)),
        ],
        INDEX_HTML,
    )
        .into_response()
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

async fn api_run(State(st): State<Arc<ViewState>>) -> Response {
    let dir = st.dir.clone();
    match tokio::task::spawn_blocking(move || analyze::analyze(&dir)).await {
        Ok(Ok(a)) => {
            let docs = run::load_manifest(&st.dir).map(|m| m.docs).unwrap_or_default();
            let mut v = serde_json::to_value(&a).unwrap_or_default();
            v["docs"] = serde_json::to_value(docs).unwrap_or_default();
            v["verdicts"] = serde_json::json!(analyze::VERDICTS);
            Json(v).into_response()
        }
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct OutputQuery {
    model: String,
    doc: String,
}

async fn api_output(State(st): State<Arc<ViewState>>, Query(q): Query<OutputQuery>) -> Response {
    let Ok(m) = run::load_manifest(&st.dir) else { return err(StatusCode::NOT_FOUND, "no run") };
    if !m.models.contains(&q.model) || !m.docs.iter().any(|d| d.id == q.doc) {
        return err(StatusCode::NOT_FOUND, "unknown model or document");
    }
    let results = run::load_results(&st.dir);
    match results.get(&(q.model.clone(), q.doc.clone())) {
        Some(r) if r.ok => match std::fs::read_to_string(run::output_path(&st.dir, &q.model, &q.doc)) {
            Ok(text) => Json(serde_json::json!({ "ok": true, "markdown": text, "record": r })).into_response(),
            Err(e) => err(StatusCode::NOT_FOUND, format!("output missing: {e}")),
        },
        Some(r) => Json(serde_json::json!({ "ok": false, "record": r })).into_response(),
        None => Json(serde_json::json!({ "ok": false, "record": null })).into_response(),
    }
}

#[derive(Deserialize)]
struct DocQuery {
    id: String,
}

fn content_type(kind: &str) -> &'static str {
    match kind {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        // Never render a user's HTML in the viewer's origin.
        "html" | "htm" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn api_doc(State(st): State<Arc<ViewState>>, Query(q): Query<DocQuery>) -> Response {
    let Ok(m) = run::load_manifest(&st.dir) else { return err(StatusCode::NOT_FOUND, "no run") };
    let Some(doc) = m.docs.iter().find(|d| d.id == q.id) else { return err(StatusCode::NOT_FOUND, "unknown document") };
    let path = Path::new(&m.dataset_root).join(&doc.id);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut h = HeaderMap::new();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type(&doc.kind)));
            h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("inline"));
            (h, Body::from(bytes)).into_response()
        }
        Err(e) => err(StatusCode::NOT_FOUND, format!("cannot read the document: {e}")),
    }
}

#[derive(Deserialize)]
struct ReviewBody {
    doc: String,
    model: String,
    verdict: String,
    #[serde(default)]
    notes: String,
}

async fn api_review(State(st): State<Arc<ViewState>>, headers: HeaderMap, Json(b): Json<ReviewBody>) -> Response {
    // Only the page itself posts here (JSON + same origin); a cross-site form cannot send JSON
    // without a preflight that this server never answers.
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map_or(true, |v| !v.starts_with("application/json"))
    {
        return err(StatusCode::UNSUPPORTED_MEDIA_TYPE, "JSON only");
    }
    match analyze::record_review(&st.dir, &b.doc, &b.model, &b.verdict, &b.notes, "user") {
        Ok(r) => Json(r).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

/// Best effort: open the URL in the default browser.
pub fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::dataset::{tempdir, tiny_pdf};
    use crate::eval::run::{CallRecord, RunSpec, Via};

    async fn fixture() -> (Viewer, PathBuf) {
        let dir = tempdir("view");
        std::fs::write(dir.join("a.pdf"), tiny_pdf(1)).unwrap();
        std::fs::write(dir.join("page.html"), "<script>alert(1)</script>").unwrap();
        let spec = RunSpec {
            dataset: dir.clone(),
            models: vec!["tesseract".into()],
            pages: None,
            limit: None,
            filter: None,
            max_cost_usd: None,
            out_dir: None,
            requirements: None,
        };
        let (_, m) = run::plan(&spec, &Via::Keys).unwrap();
        let prep = run::prepare_new(&spec, &Via::Keys, m).unwrap();
        let p = run::output_path(&prep.dir, "tesseract/default", "a.pdf");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "# Hello").unwrap();
        let rec = CallRecord {
            model: "tesseract/default".into(),
            doc: "a.pdf".into(),
            ok: true,
            pages: 1,
            chars: 7,
            ..Default::default()
        };
        std::fs::write(run::results_path(&prep.dir), serde_json::to_string(&rec).unwrap() + "\n").unwrap();
        (start(&prep.dir, 0).await.unwrap(), prep.dir)
    }

    #[tokio::test]
    async fn serves_the_run_behind_its_token_on_loopback_only() {
        let (v, dir) = fixture().await;
        let c = reqwest::Client::new();
        let page = c.get(&v.url).send().await.unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.headers()["content-security-policy"].to_str().unwrap().contains("script-src 'self'"));
        assert!(page.text().await.unwrap().contains("app.js"));

        let base = v.url.trim_end_matches('/');
        let run: serde_json::Value = c.get(format!("{base}/api/run")).send().await.unwrap().json().await.unwrap();
        assert_eq!(run["models"][0], "tesseract/default");
        assert_eq!(run["docs"].as_array().unwrap().len(), 2);

        let out: serde_json::Value = c
            .get(format!("{base}/api/output?model=tesseract/default&doc=a.pdf"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(out["markdown"], "# Hello");

        let pdf = c.get(format!("{base}/doc?id=a.pdf")).send().await.unwrap();
        assert_eq!(pdf.headers()["content-type"], "application/pdf");
        let html = c.get(format!("{base}/doc?id=page.html")).send().await.unwrap();
        assert!(html.headers()["content-type"].to_str().unwrap().starts_with("text/plain"));
        assert_eq!(c.get(format!("{base}/doc?id=../../etc/passwd")).send().await.unwrap().status(), 404);

        // No token, wrong token, wrong Host.
        let port: u16 = v.url.split(':').nth(2).unwrap().split('/').next().unwrap().parse().unwrap();
        let root = format!("http://127.0.0.1:{port}/");
        assert_eq!(c.get(&root).send().await.unwrap().status(), 404);
        let wrong = c.get(&v.url).header("host", format!("evil.example:{port}")).send().await.unwrap();
        assert_eq!(wrong.status(), 403);

        // Reviews: JSON only, validated.
        let ok = c
            .post(format!("{base}/api/review"))
            .json(&serde_json::json!({"doc": "a.pdf", "model": "tesseract/default", "verdict": "correct"}))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        assert_eq!(analyze::load_reviews(&dir).len(), 1);
        let form = c
            .post(format!("{base}/api/review"))
            .header("content-type", "text/plain")
            .body(r#"{"doc":"a.pdf","model":"tesseract/default","verdict":"correct"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(form.status(), 415);
    }
}
