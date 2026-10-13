//! `puffinparse eval view`: a local viewer for one run.
//!
//! The page is the benchmark site's own viewer (`benchmark/site/src/`, ADR-28), compiled into the
//! binary: the same leaderboard, document list and inspector (page, layout boxes, outputs, word
//! diff, model comparison) as puffinparse.com. This server answers the same read-only data API the
//! site's build writes (`data/index.json`, `data/runs/<id>.json`, the dataset manifest, inputs and
//! outputs), built on the fly from the run folder, and marks the run as a review run: there is no
//! ground truth, so the viewer shows agreement between models and a verdict form, whose answers it
//! posts to `api/review`. pdf.js is served from the binary too, so nothing is fetched from the
//! network and nothing leaves the machine.
//!
//! The server listens on `127.0.0.1` only. Every route sits under a random path token printed with
//! the address, and requests whose `Host` is not the loopback address are refused, so another web
//! page in the browser cannot read the run (DNS rebinding) or guess the address. Only documents
//! listed in the run are served, by id. HTML documents are served as plain text so they can never
//! run script in the viewer's origin.

use super::{analyze, run};
use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod assets {
    include!(concat!(env!("OUT_DIR"), "/viewer_assets.rs"));
}

const PDF_JS: &[u8] = include_bytes!("../../assets/pdfjs/pdf.min.js");
const PDF_WORKER_JS: &[u8] = include_bytes!("../../assets/pdfjs/pdf.worker.min.js");

/// The dataset name a run's documents are served under (`data/datasets/<name>/`).
const DATASET: &str = "local";

struct ViewState {
    dir: PathBuf,
    port: u16,
    index_html: String,
    csp: String,
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
    let index_html = local_index_html(assets::INDEX_HTML);
    let csp = csp_for(&index_html);
    let state = Arc::new(ViewState { dir, port, index_html, csp });
    let app = router(&token, state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!(error = %e, "eval view: server stopped");
        }
    });
    Ok(Viewer { url: format!("http://127.0.0.1:{port}/{token}/") })
}

/// The site's `index.html`, switched to review mode with the bundled pdf.js.
fn local_index_html(html: &str) -> String {
    html.replace(
        r#"<meta name="puffinparse-mode" content="" />"#,
        r#"<meta name="puffinparse-mode" content="review" />"#,
    )
    .replace(
        r#"<meta name="puffinparse-pdfjs" content="" />"#,
        r#"<meta name="puffinparse-pdfjs" content="vendor/pdfjs/" />"#,
    )
    .replace("Loading benchmark data…", "Loading the run…")
}

/// Same-origin scripts only, plus the page's own inline theme script by hash. pdf.js runs its
/// worker from a blob URL. Inline `style` attributes are allowed: the viewer sets bar widths and
/// the overlay's visibility with them.
fn csp_for(html: &str) -> String {
    let mut hashes = String::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script>") {
        let body = &rest[start + "<script>".len()..];
        let Some(end) = body.find("</script>") else { break };
        hashes += &format!(" 'sha256-{}'", base64(&Sha256::digest(&body.as_bytes()[..end])));
        rest = &body[end..];
    }
    format!(
        "default-src 'none'; script-src 'self'{hashes}; worker-src blob:; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; \
         frame-ancestors 'none'"
    )
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= chunk.len() { ABC[(n >> shift & 63) as usize] as char } else { '=' });
        }
    }
    out
}

fn router(token: &str, state: Arc<ViewState>) -> Router {
    let p = |s: &str| format!("/{token}/{s}");
    Router::new()
        .route(&p(""), get(index))
        .route(&p("app.js"), get(|| async { text(assets::APP_JS, "text/javascript; charset=utf-8") }))
        .route(&p("styles.css"), get(|| async { text(assets::STYLES_CSS, "text/css; charset=utf-8") }))
        .route(&p("tokens.css"), get(|| async { text(assets::TOKENS_CSS, "text/css; charset=utf-8") }))
        .route(&p("vendor/pdfjs/pdf.min.js"), get(|| async { bytes(PDF_JS, "text/javascript; charset=utf-8") }))
        .route(
            &p("vendor/pdfjs/pdf.worker.min.js"),
            get(|| async { bytes(PDF_WORKER_JS, "text/javascript; charset=utf-8") }),
        )
        .route(&p("data/index.json"), get(data_index))
        .route(&p("data/runs/{file}"), get(data_run))
        .route(&p("data/datasets/local/manifest.json"), get(data_manifest))
        .route(&p("data/datasets/local/docs/{*id}"), get(data_doc))
        .route(&p("data/datasets/local/truth/{*id}"), get(data_truth))
        .route(&p("data/outputs/{run}/{slug}/{*file}"), get(data_output))
        .route(&p("api/review"), post(api_review))
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

fn text(body: &'static str, ctype: &'static str) -> Response {
    ([(header::CONTENT_TYPE, ctype)], body).into_response()
}

fn bytes(body: &'static [u8], ctype: &'static str) -> Response {
    ([(header::CONTENT_TYPE, ctype)], body).into_response()
}

async fn index(State(st): State<Arc<ViewState>>) -> Response {
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    if let Ok(v) = HeaderValue::from_str(&st.csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    (h, st.index_html.clone()).into_response()
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "not found")
}

// ---- the site's data API, built from the run folder ----------------------------------------------

/// `/`-separated path with every segment percent-encoded, so a file name with `#`, `?` or `%` still
/// makes a working URL in the manifest.
fn url_path(id: &str) -> String {
    id.split('/')
        .map(|seg| {
            seg.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The run in the shape of a benchmark result file (`data/runs/<id>.json`): per model, one record
/// per document with latency, cost and failures, plus the run's analysis under `review` (agreement,
/// outliers, verdicts, the review queue). Every model lists every document; a call that was never
/// made is `missing`.
fn site_run(dir: &Path) -> Result<Value, String> {
    let manifest = run::load_manifest(dir)?;
    let analysis = analyze::analyze(dir)?;
    let results = run::load_results(dir);
    let models: Vec<Value> = manifest
        .models
        .iter()
        .map(|model| {
            let report = analysis.per_model.iter().find(|r| &r.model == model);
            let docs: Vec<Value> = manifest
                .docs
                .iter()
                .map(|doc| match results.get(&(model.clone(), doc.id.clone())) {
                    Some(r) => json!({
                        "id": doc.id,
                        "error": if r.ok { None } else { Some(r.error.clone().unwrap_or_else(|| "failed".into())) },
                        "error_kind": r.error_kind,
                        "empty": r.ok && r.empty,
                        "latency_ms": r.latency_ms,
                        "cost_usd": r.cost_usd,
                        "pages": r.pages,
                        "chars": r.chars,
                    }),
                    None => json!({ "id": doc.id, "error": null, "missing": true }),
                })
                .collect();
            let summary = report.map_or(Value::Null, |r| {
                json!({
                    "documents": r.documents,
                    "failed": r.failed,
                    "empty_outputs": r.empty,
                    "cost_per_1k_pages_usd": r.usd_per_1k_pages.or(r.list_usd_per_1k_pages),
                    "latency_p50_ms": r.latency_p50_ms,
                    "latency_p95_ms": r.latency_p95_ms,
                })
            });
            json!({ "model": model, "summary": summary, "docs": docs, "review": report })
        })
        .collect();
    let mut review = serde_json::to_value(&analysis).map_err(|e| e.to_string())?;
    if let Some(obj) = review.as_object_mut() {
        obj.remove("per_model");
        obj.insert("verdicts".into(), json!(analyze::VERDICTS));
        obj.insert("max_cost_usd".into(), json!(manifest.max_cost_usd));
    }
    Ok(json!({
        "run_id": manifest.run_id,
        "created_at": manifest.created_at,
        "puffinparse_version": manifest.puffinparse_version,
        "dataset": { "name": DATASET, "version": "", "documents": manifest.docs.len(), "root": manifest.dataset_root },
        "normalize": {},
        "models": models,
        "review": review,
    }))
}

fn site_index(dir: &Path) -> Result<Value, String> {
    let manifest = run::load_manifest(dir)?;
    let results = run::load_results(dir);
    let mut outputs = serde_json::Map::new();
    for model in &manifest.models {
        let (mut with_json, mut missing) = (Vec::new(), Vec::new());
        for doc in &manifest.docs {
            let ok = results.get(&(model.clone(), doc.id.clone())).is_some_and(|r| r.ok);
            if !ok || !run::output_path(dir, model, &doc.id).is_file() {
                missing.push(doc.id.clone());
            } else if run::response_path(dir, model, &doc.id).is_file() {
                with_json.push(doc.id.clone());
            }
        }
        outputs.insert(run::model_slug(model), json!({ "json": with_json, "missing": missing }));
    }
    let models: Vec<Value> =
        manifest.models.iter().map(|m| json!({ "model": m, "slug": run::model_slug(m), "summary": {} })).collect();
    Ok(json!({
        "generated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "mode": "review",
        "runs": [{
            "run_id": manifest.run_id,
            "created_at": manifest.created_at,
            "puffinparse_version": manifest.puffinparse_version,
            "dataset": { "name": DATASET, "version": "", "documents": manifest.docs.len() },
            "normalize": {},
            "categories": [],
            "models": models,
            "outputs": outputs,
            "file": format!("data/runs/{}.json", manifest.run_id),
            "review": true,
        }],
        "datasets": { DATASET: { "name": DATASET, "version": "", "description": manifest.dataset_root } },
        "labels": { "sources": {}, "categories": {} },
    }))
}

fn site_manifest(dir: &Path) -> Result<Value, String> {
    let manifest = run::load_manifest(dir)?;
    let docs: Vec<Value> = manifest
        .docs
        .iter()
        .map(|d| {
            let mut v = json!({
                "id": d.id,
                "file": format!("docs/{}", url_path(&d.id)),
                "kind": d.kind,
                "pages": d.pages.unwrap_or(1),
                "title": d.id,
                "category": "",
            });
            if d.truth.is_some() {
                v["truth"] = json!(format!("truth/{}", url_path(&d.id)));
            }
            v
        })
        .collect();
    Ok(json!({ "name": DATASET, "version": "", "root": manifest.dataset_root, "documents": docs }))
}

async fn blocking<F>(st: &Arc<ViewState>, f: F) -> Response
where
    F: FnOnce(&Path) -> Result<Value, String> + Send + 'static,
{
    let dir = st.dir.clone();
    match tokio::task::spawn_blocking(move || f(&dir)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn data_index(State(st): State<Arc<ViewState>>) -> Response {
    blocking(&st, site_index).await
}

async fn data_run(State(st): State<Arc<ViewState>>, UrlPath(file): UrlPath<String>) -> Response {
    let Ok(m) = run::load_manifest(&st.dir) else { return not_found() };
    if file != format!("{}.json", m.run_id) {
        return not_found();
    }
    blocking(&st, site_run).await
}

async fn data_manifest(State(st): State<Arc<ViewState>>) -> Response {
    blocking(&st, site_manifest).await
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

async fn send_file(path: PathBuf, ctype: &'static str) -> Response {
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut h = HeaderMap::new();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
            h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("inline"));
            (h, Body::from(bytes)).into_response()
        }
        Err(_) => not_found(),
    }
}

async fn data_doc(State(st): State<Arc<ViewState>>, UrlPath(id): UrlPath<String>) -> Response {
    let Ok(m) = run::load_manifest(&st.dir) else { return not_found() };
    let Some(doc) = m.docs.iter().find(|d| d.id == id) else { return not_found() };
    send_file(Path::new(&m.dataset_root).join(&doc.id), content_type(&doc.kind)).await
}

async fn data_truth(State(st): State<Arc<ViewState>>, UrlPath(id): UrlPath<String>) -> Response {
    let Ok(m) = run::load_manifest(&st.dir) else { return not_found() };
    let Some(truth) = m.docs.iter().find(|d| d.id == id).and_then(|d| d.truth.clone()) else { return not_found() };
    send_file(Path::new(&m.dataset_root).join(truth), "text/markdown; charset=utf-8").await
}

async fn data_output(
    State(st): State<Arc<ViewState>>,
    UrlPath((run_id, slug, file)): UrlPath<(String, String, String)>,
) -> Response {
    let Ok(m) = run::load_manifest(&st.dir) else { return not_found() };
    let Some(model) = m.models.iter().find(|model| run::model_slug(model) == slug) else { return not_found() };
    if run_id != m.run_id {
        return not_found();
    }
    let (doc, json) = match (file.strip_suffix(".md"), file.strip_suffix(".json")) {
        (Some(doc), _) => (doc, false),
        (_, Some(doc)) => (doc, true),
        _ => return not_found(),
    };
    if !m.docs.iter().any(|d| d.id == doc) {
        return not_found();
    }
    if json {
        send_file(run::response_path(&st.dir, model, doc), "application/json").await
    } else {
        send_file(run::output_path(&st.dir, model, doc), "text/markdown; charset=utf-8").await
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
        std::fs::create_dir_all(dir.join("sub dir")).unwrap();
        std::fs::write(dir.join("sub dir/b#1.pdf"), tiny_pdf(1)).unwrap();
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
        std::fs::write(run::response_path(&prep.dir, "tesseract/default", "a.pdf"), r#"{"pages":[]}"#).unwrap();
        let ok = CallRecord {
            model: "tesseract/default".into(),
            doc: "a.pdf".into(),
            ok: true,
            pages: 1,
            chars: 7,
            ..Default::default()
        };
        let failed = CallRecord {
            model: "tesseract/default".into(),
            doc: "page.html".into(),
            ok: false,
            error: Some("unsupported".into()),
            ..Default::default()
        };
        let lines = [&ok, &failed].map(|r| serde_json::to_string(r).unwrap()).join("\n") + "\n";
        std::fs::write(run::results_path(&prep.dir), lines).unwrap();
        (start(&prep.dir, 0).await.unwrap(), prep.dir)
    }

    async fn get_json(c: &reqwest::Client, url: String) -> Value {
        let resp = c.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200, "{url}");
        resp.json().await.unwrap()
    }

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_site_page_switches_to_review_mode_and_its_inline_script_is_allowed_by_hash() {
        let html = local_index_html(assets::INDEX_HTML);
        assert!(html.contains(r#"<meta name="puffinparse-mode" content="review" />"#));
        assert!(html.contains(r#"<meta name="puffinparse-pdfjs" content="vendor/pdfjs/" />"#));
        let csp = csp_for(&html);
        assert!(csp.contains("script-src 'self' 'sha256-"), "{csp}");
        assert!(!csp.contains("unsafe-eval"));
    }

    #[test]
    fn url_paths_encode_each_segment() {
        assert_eq!(url_path("sub dir/b#1.pdf"), "sub%20dir/b%231.pdf");
        assert_eq!(url_path("a.pdf"), "a.pdf");
    }

    #[tokio::test]
    async fn serves_the_site_and_its_data_api_behind_its_token_on_loopback_only() {
        let (v, dir) = fixture().await;
        let c = reqwest::Client::new();
        let page = c.get(&v.url).send().await.unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.headers()["content-security-policy"].to_str().unwrap().contains("script-src 'self'"));
        let body = page.text().await.unwrap();
        assert!(body.contains("app.js") && body.contains(r#"content="review""#));

        let base = v.url.trim_end_matches('/');
        for asset in ["app.js", "styles.css", "tokens.css", "vendor/pdfjs/pdf.min.js", "vendor/pdfjs/pdf.worker.min.js"]
        {
            assert_eq!(c.get(format!("{base}/{asset}")).send().await.unwrap().status(), 200, "{asset}");
        }

        let index = get_json(&c, format!("{base}/data/index.json")).await;
        let entry = &index["runs"][0];
        assert_eq!(entry["review"], true);
        let run_id = entry["run_id"].as_str().unwrap().to_string();
        let inv = &entry["outputs"]["tesseract_default"];
        assert_eq!(inv["json"], json!(["a.pdf"]));
        assert!(inv["missing"].as_array().unwrap().iter().any(|d| d == "page.html"));

        let result = get_json(&c, format!("{base}/data/runs/{run_id}.json")).await;
        let docs = result["models"][0]["docs"].as_array().unwrap();
        assert_eq!(docs.len(), 3);
        assert!(docs.iter().any(|d| d["id"] == "page.html" && d["error"] == "unsupported"));
        assert!(docs.iter().any(|d| d["id"] == "sub dir/b#1.pdf" && d["missing"] == true));
        assert_eq!(result["review"]["verdicts"][0], "correct");
        assert_eq!(c.get(format!("{base}/data/runs/other.json")).send().await.unwrap().status(), 404);

        let manifest = get_json(&c, format!("{base}/data/datasets/local/manifest.json")).await;
        let files: Vec<&str> =
            manifest["documents"].as_array().unwrap().iter().map(|d| d["file"].as_str().unwrap()).collect();
        assert!(files.contains(&"docs/sub%20dir/b%231.pdf"));

        let md = c.get(format!("{base}/data/outputs/{run_id}/tesseract_default/a.pdf.md")).send().await.unwrap();
        assert_eq!(md.text().await.unwrap(), "# Hello");
        let unified = get_json(&c, format!("{base}/data/outputs/{run_id}/tesseract_default/a.pdf.json")).await;
        assert_eq!(unified["pages"], json!([]));
        assert_eq!(c.get(format!("{base}/data/outputs/{run_id}/nope/a.pdf.md")).send().await.unwrap().status(), 404);

        let pdf = c.get(format!("{base}/data/datasets/local/docs/a.pdf")).send().await.unwrap();
        assert_eq!(pdf.headers()["content-type"], "application/pdf");
        let nested = c.get(format!("{base}/data/datasets/local/docs/sub%20dir/b%231.pdf")).send().await.unwrap();
        assert_eq!(nested.status(), 200);
        let html = c.get(format!("{base}/data/datasets/local/docs/page.html")).send().await.unwrap();
        assert!(html.headers()["content-type"].to_str().unwrap().starts_with("text/plain"));
        let escape = c.get(format!("{base}/data/datasets/local/docs/..%2F..%2Fetc%2Fpasswd")).send().await.unwrap();
        assert_eq!(escape.status(), 404);

        // No token, wrong token, wrong Host.
        let port: u16 = v.url.split(':').nth(2).unwrap().split('/').next().unwrap().parse().unwrap();
        let root = format!("http://127.0.0.1:{port}/");
        assert_eq!(c.get(&root).send().await.unwrap().status(), 404);
        let wrong = c.get(&v.url).header("host", format!("evil.example:{port}")).send().await.unwrap();
        assert_eq!(wrong.status(), 403);

        // Reviews: JSON only, validated.
        let ok = c
            .post(format!("{base}/api/review"))
            .json(&json!({"doc": "a.pdf", "model": "tesseract/default", "verdict": "correct"}))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        assert_eq!(analyze::load_reviews(&dir).len(), 1);
        let after = get_json(&c, format!("{base}/data/runs/{run_id}.json")).await;
        let reviewed = after["review"]["per_doc"].as_array().unwrap().iter().find(|d| d["id"] == "a.pdf").unwrap();
        assert_eq!(reviewed["reviews"]["tesseract/default"]["verdict"], "correct");
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
