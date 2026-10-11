//! Routes, request decoding, auth / budget / rate-limit checks, and the fallback loop.

use crate::error::ApiError;
use crate::log::RequestLog;
use crate::metrics::Observation;
use crate::{default_fallback_on, AppState, Deployment};
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use puffinparse_core::model::ModelRef;
use puffinparse_core::{
    DocumentInput, DocumentRequest, Error, ErrorKind, ExtractRequest, Mode, OutputFormat, Strategy,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

pub(crate) fn router(state: Arc<AppState>) -> axum::Router {
    let limit = state.server.max_body_mb.saturating_mul(1024 * 1024);
    let request_timeout = state.server.request_timeout();
    // One semaphore shared by every document route: at most `max_concurrent_requests` provider
    // calls in flight; the rest wait (bounded by the request timeout below).
    let concurrency = tower::limit::GlobalConcurrencyLimitLayer::new(state.server.max_concurrent_requests);
    axum::Router::new()
        .route("/v1/parse", post(|s: State<Arc<AppState>>, r: Request| run(s, Mode::Parse, r)))
        .route("/v1/ocr", post(|s: State<Arc<AppState>>, r: Request| run(s, Mode::Ocr, r)))
        .route("/v1/extract", post(|s: State<Arc<AppState>>, r: Request| run(s, Mode::Extract, r)))
        .route("/v1/jobs", post(crate::jobs::submit))
        .route("/v1/jobs/{id}", get(crate::jobs::retrieve))
        .route("/v1/webhooks/{provider}", post(crate::jobs::webhook))
        .route_layer(concurrency)
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/models", get(models))
        .route("/v1/usage", get(usage))
        .layer(DefaultBodyLimit::max(limit))
        .layer(axum::middleware::from_fn(move |req: Request, next: axum::middleware::Next| async move {
            match tokio::time::timeout(request_timeout, next.run(req)).await {
                Ok(resp) => resp,
                Err(_) => ApiError::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "timeout_error",
                    format!(
                        "the gateway's request limit ({}s, server.request_timeout_secs) was reached",
                        request_timeout.as_secs()
                    ),
                )
                .into_response(),
            }
        }))
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(|_| {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", "internal server error").into_response()
        }))
        .with_state(state)
}

// ---------------------------------------------------------------------------------------------
// Auth

/// Who is calling.
#[derive(Debug, Clone)]
pub(crate) enum Principal {
    /// Auth disabled (no master key, no virtual keys).
    Anonymous,
    Master,
    Key(usize),
}

impl Principal {
    pub(crate) fn id<'a>(&self, state: &'a AppState) -> &'a str {
        match self {
            Principal::Anonymous => "anonymous",
            Principal::Master => "master",
            Principal::Key(i) => &state.keys[*i].id,
        }
    }
}

/// Constant-time byte comparison, so response timing does not leak key prefixes.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        let token = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")).unwrap_or(v);
        return Some(token.trim());
    }
    headers.get("x-api-key").and_then(|v| v.to_str().ok()).map(str::trim)
}

pub(crate) fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Principal, ApiError> {
    if !state.auth_enabled() {
        return Ok(Principal::Anonymous);
    }
    let token = bearer(headers).filter(|t| !t.is_empty()).ok_or_else(|| {
        ApiError::unauthorized("missing API key: send 'Authorization: Bearer <key>' or 'x-api-key: <key>'")
    })?;
    if state.master_key.as_deref().is_some_and(|m| ct_eq(m.as_bytes(), token.as_bytes())) {
        return Ok(Principal::Master);
    }
    // Compare against every key (no early exit) to keep timing independent of position.
    let mut found = None;
    for (i, k) in state.keys.iter().enumerate() {
        if ct_eq(k.secret.as_bytes(), token.as_bytes()) {
            found = Some(i);
        }
    }
    found.map(Principal::Key).ok_or_else(|| ApiError::unauthorized("invalid API key"))
}

/// `models` entries: exact names, `*`, or a prefix ending in `/*` (`provider/*`, or
/// `opendocrouter/google/*` for model ids that themselves contain '/').
fn allowed(patterns: &[String], name: &str) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let qualified = ModelRef::parse(name).ok().map(|m| m.qualified());
    patterns.iter().any(|p| {
        let matches = |n: &str| {
            p == "*"
                || p == n
                || p.strip_suffix("/*").is_some_and(|prefix| {
                    n.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/') && rest.len() > 1)
                })
        };
        matches(name) || qualified.as_deref().is_some_and(matches)
    })
}

/// Like [`allowed`], but only for a pattern that names the model or its provider: `*` and an
/// empty list (which allow everything) do not count.
fn explicitly_allowed(patterns: &[String], name: &str) -> bool {
    let named: Vec<String> = patterns.iter().filter(|p| p.as_str() != "*").cloned().collect();
    !named.is_empty() && allowed(&named, name)
}

/// Self-hosted engines run on the gateway host (Tesseract, `pdftoppm`) or on servers on the
/// operator's network, so naming one directly needs more than a catch-all key: an alias, a key
/// whose `models` names it, the master key, or `server.allow_local_engines`.
pub(crate) fn may_use_local_engine(state: &AppState, who: &Principal, name: &str) -> bool {
    let Ok(r) = ModelRef::parse(name) else { return true };
    if !puffinparse_core::model::SELF_HOSTED.contains(&r.provider.as_str()) || state.server.allow_local_engines {
        return true;
    }
    match who {
        Principal::Master => true,
        Principal::Key(i) => explicitly_allowed(&state.keys[*i].models, name),
        Principal::Anonymous => false,
    }
}

fn check_local_engine(state: &AppState, who: &Principal, name: &str) -> Result<(), ApiError> {
    if may_use_local_engine(state, who, name) {
        return Ok(());
    }
    Err(ApiError::forbidden(format!(
        "'{name}' is a self-hosted engine and runs on the gateway's own machine or network: it is only served \
         through an alias, to a key whose 'models' names it, or with server.allow_local_engines = true"
    )))
}

/// `document_url` with a model that would download it inside the gateway (rather than hand it to
/// the provider) needs `server.fetch_document_urls`.
pub(crate) fn check_document_url(
    state: &AppState,
    doc: &DocumentRequest,
    mode: Mode,
    deployments: &[Deployment],
) -> Result<(), ApiError> {
    if state.server.fetch_document_urls || !matches!(doc.input, DocumentInput::Url { .. }) {
        return Ok(());
    }
    for d in deployments {
        let provider = d.model.split('/').next().unwrap_or("");
        if puffinparse_core::fetch::fetches_url_in_process(provider, mode) {
            return Err(ApiError::input(format!(
                "'{}' cannot take 'document_url' on this gateway: that provider does not fetch URLs itself, so the \
                 gateway would have to download the document from its own network, which is off \
                 (server.fetch_document_urls = false). Upload the file instead ('file' or base64 'document'), \
                 or use a model whose provider fetches URLs (Reducto, Extend, LlamaParse, Mistral, Azure, ...)",
                d.model
            )));
        }
    }
    Ok(())
}

pub(crate) fn check_model_access(state: &AppState, who: &Principal, name: &str) -> Result<(), ApiError> {
    if let Principal::Key(i) = who {
        let key = &state.keys[*i];
        if !allowed(&key.models, name) {
            return Err(ApiError::forbidden(format!("key '{}' is not allowed to use model '{name}'", key.id)));
        }
    }
    Ok(())
}

/// Budget and rate limit, checked before any provider call.
pub(crate) fn check_limits(state: &AppState, who: &Principal) -> Result<(), ApiError> {
    let Principal::Key(i) = who else { return Ok(()) };
    let key = &state.keys[*i];
    if let Some(budget) = key.monthly_budget_usd {
        let used = state.usage.get(&key.id);
        if used.spend_usd >= budget {
            return Err(ApiError::budget(format!(
                "key '{}' has spent ${:.4} of its ${budget:.2} budget for {}",
                key.id, used.spend_usd, used.month
            )));
        }
    }
    check_rate(state, who)
}

/// The key's `rpm` limit alone (job status checks are not budget-gated: the job is already paid).
pub(crate) fn check_rate(state: &AppState, who: &Principal) -> Result<(), ApiError> {
    let Principal::Key(i) = who else { return Ok(()) };
    let key = &state.keys[*i];
    if let Some(rpm) = key.rpm {
        state.usage.check_rate(&key.id, rpm).map_err(ApiError::rate_limited)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Request body

/// JSON body of `/v1/parse`, `/v1/ocr`, `/v1/extract`. Multipart forms carry the same fields as
/// text parts plus a `file` part. `api_key` / `base_url` are deliberately absent: callers must
/// never be able to redirect the gateway's provider credentials.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApiRequest {
    pub(crate) model: Option<String>,
    document_url: Option<String>,
    /// Base64 document bytes (a `data:` URL prefix is accepted); needs `filename`.
    document: Option<String>,
    filename: Option<String>,
    pages: Option<String>,
    language: Option<String>,
    output: Option<OutputFormat>,
    pub(crate) output_format: Option<String>,
    provider_options: Option<Value>,
    include_raw: Option<bool>,
    /// Seconds, capped at `server.max_timeout_secs`.
    timeout: Option<f64>,
    max_retries: Option<u32>,
    /// Extra aliases / models tried after `model`'s own targets.
    pub(crate) fallbacks: Option<Vec<String>>,
    metadata: Option<BTreeMap<String, Value>>,
    // extract only
    pub(crate) schema: Option<Value>,
    pub(crate) instructions: Option<String>,
    pub(crate) citations: Option<bool>,
    /// `/v1/jobs` only: the provider's per-job webhook (SPEC §15).
    pub(crate) webhook_url: Option<String>,
    #[serde(skip)]
    file: Option<(bytes::Bytes, String)>,
}

pub(crate) async fn decode_body(req: Request) -> Result<ApiRequest, ApiError> {
    let ct = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    if ct.starts_with("multipart/form-data") {
        let mp = Multipart::from_request(req, &()).await.map_err(|e| body_error(e.status(), e.body_text()))?;
        return decode_multipart(mp).await;
    }
    let body = bytes::Bytes::from_request(req, &()).await.map_err(|e| body_error(e.status(), e.body_text()))?;
    if body.is_empty() {
        return Err(ApiError::input("empty request body: send JSON or multipart/form-data"));
    }
    serde_json::from_slice(&body).map_err(|e| ApiError::input(format!("invalid JSON body: {e}")))
}

pub(crate) fn body_error(status: StatusCode, text: String) -> ApiError {
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::new(status, "payload_too_large", "request body exceeds server.max_body_mb")
    } else {
        ApiError::input(text)
    }
}

async fn decode_multipart(mut mp: Multipart) -> Result<ApiRequest, ApiError> {
    let mut r = ApiRequest::default();
    let bad = |e: axum::extract::multipart::MultipartError| body_error(e.status(), e.body_text());
    while let Some(field) = mp.next_field().await.map_err(bad)? {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            let filename = field.file_name().map(str::to_string).filter(|f| !f.trim().is_empty());
            let data = field.bytes().await.map_err(bad)?;
            r.file = Some((data, filename.unwrap_or_default()));
            continue;
        }
        let text = field.text().await.map_err(bad)?;
        let json = |what: &str| {
            serde_json::from_str::<Value>(&text)
                .map_err(|e| ApiError::input(format!("field '{what}' is not JSON: {e}")))
        };
        match name.as_str() {
            "model" => r.model = Some(text),
            "document_url" => r.document_url = Some(text),
            "filename" => r.filename = Some(text),
            "pages" => r.pages = Some(text),
            "language" => r.language = Some(text),
            "output" => {
                r.output = Some(serde_json::from_value(json!(text)).map_err(|_| {
                    ApiError::input(format!("field 'output' must be 'markdown' or 'text', got '{text}'"))
                })?)
            }
            "output_format" => r.output_format = Some(text),
            "provider_options" => r.provider_options = Some(json("provider_options")?),
            "schema" => r.schema = Some(json("schema")?),
            "metadata" => {
                r.metadata = Some(
                    serde_json::from_value(json("metadata")?)
                        .map_err(|_| ApiError::input("field 'metadata' must be a JSON object"))?,
                )
            }
            "instructions" => r.instructions = Some(text),
            "webhook_url" => r.webhook_url = Some(text),
            "include_raw" => r.include_raw = Some(parse_bool("include_raw", &text)?),
            "citations" => r.citations = Some(parse_bool("citations", &text)?),
            "timeout" => {
                r.timeout = Some(text.trim().parse().map_err(|_| ApiError::input("field 'timeout' must be a number"))?)
            }
            "max_retries" => {
                r.max_retries =
                    Some(text.trim().parse().map_err(|_| ApiError::input("field 'max_retries' must be an integer"))?)
            }
            "fallbacks" => {
                let t = text.trim();
                r.fallbacks = Some(if t.starts_with('[') {
                    serde_json::from_str(t)
                        .map_err(|_| ApiError::input("field 'fallbacks' must be a JSON array of strings"))?
                } else {
                    t.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
                });
            }
            other => return Err(ApiError::input(format!("unknown form field '{other}'"))),
        }
    }
    Ok(r)
}

fn parse_bool(field: &str, s: &str) -> Result<bool, ApiError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" | "" => Ok(false),
        _ => Err(ApiError::input(format!("field '{field}' must be true or false"))),
    }
}

/// Decode base64 (standard alphabet, padding optional, whitespace and a `data:` prefix ignored)
/// by reusing the core's serde codec for `DocumentInput::Bytes`.
fn decode_base64(s: &str, filename: &str) -> Result<DocumentInput, ApiError> {
    let s = match s.split_once(";base64,") {
        Some((prefix, rest)) if prefix.starts_with("data:") => rest,
        _ => s,
    };
    let cleaned: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    serde_json::from_value(json!({ "kind": "bytes", "data": cleaned, "filename": filename }))
        .map_err(|e| ApiError::input(format!("field 'document' is not valid base64: {e}")))
}

fn build_input(r: &mut ApiRequest) -> Result<DocumentInput, ApiError> {
    let sources = [r.document_url.is_some(), r.document.is_some(), r.file.is_some()].iter().filter(|b| **b).count();
    if sources != 1 {
        return Err(ApiError::input(
            "send exactly one document: 'document_url', base64 'document' (+ 'filename'), or a multipart 'file'",
        ));
    }
    if let Some(url) = r.document_url.take() {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(ApiError::input("document_url must be an http(s) URL"));
        }
        return Ok(DocumentInput::Url { url });
    }
    if let Some(b64) = r.document.take() {
        let filename =
            r.filename.clone().filter(|f| !f.trim().is_empty()).ok_or_else(|| {
                ApiError::input("'filename' is required with base64 'document' (it sets the file type)")
            })?;
        return decode_base64(&b64, &filename);
    }
    let (data, part_name) = r.file.take().unwrap_or_default();
    let filename = r.filename.clone().filter(|f| !f.trim().is_empty()).unwrap_or(part_name);
    if filename.trim().is_empty() {
        return Err(ApiError::input("the multipart 'file' part needs a filename (or send a 'filename' field)"));
    }
    Ok(DocumentInput::Bytes { data, filename })
}

// ---------------------------------------------------------------------------------------------
// Routing plan

/// The ordered deployments to try, with the error kinds that allow moving on.
fn plan(
    state: &AppState,
    who: &Principal,
    mode: Mode,
    model: &str,
    fallbacks: &[String],
) -> Result<(Vec<Deployment>, Vec<ErrorKind>), ApiError> {
    let mut out = Vec::new();
    let mut fallback_on = None;
    for name in std::iter::once(model).chain(fallbacks.iter().map(String::as_str)) {
        check_model_access(state, who, name)?;
        if let Some(alias) = state.aliases.get(name) {
            let n = alias.targets.len();
            let start = match alias.strategy {
                Strategy::Ordered => 0,
                Strategy::RoundRobin => alias.cursor.fetch_add(1, Ordering::Relaxed) % n,
            };
            out.extend((0..n).map(|i| alias.targets[(start + i) % n].clone()));
            fallback_on.get_or_insert_with(|| alias.fallback_on.clone());
        } else if state.server.allow_direct_models {
            check_local_engine(state, who, name)?;
            out.push(Deployment { model: name.to_string(), api_key: None, base_url: None });
        } else {
            return Err(unknown_model(name));
        }
    }
    // Mode errors are raised before any network call (SPEC §6).
    for d in &mut out {
        with_credentials(state, d, mode)?;
    }
    Ok((out, fallback_on.unwrap_or_else(default_fallback_on)))
}

fn unknown_model(name: &str) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "unsupported_model_error",
        format!("unknown model '{name}': this gateway only serves its configured aliases (GET /v1/models)"),
    )
}

/// Check the deployment serves `mode`, qualify its model and fill `[providers.*]` credentials
/// where the target has none of its own.
pub(crate) fn with_credentials(state: &AppState, d: &mut Deployment, mode: Mode) -> Result<(), ApiError> {
    let r = ModelRef::parse_for(&d.model, mode).map_err(ApiError::from)?;
    let (key, base) = state.providers.get(&r.provider).cloned().unwrap_or_default();
    d.model = r.qualified();
    d.api_key = d.api_key.take().or(key);
    d.base_url = d.base_url.take().or(base);
    Ok(())
}

/// The single deployment an async job is submitted to: an alias's first target in its strategy
/// order (jobs do not fall back — a provider failure surfaces later, on retrieve), or the model
/// itself. Also returns the alias and target index, so retrieve reuses the same credentials.
pub(crate) fn plan_job(
    state: &AppState,
    who: &Principal,
    model: &str,
) -> Result<(Deployment, Option<(String, usize)>), ApiError> {
    check_model_access(state, who, model)?;
    let (mut d, origin) = if let Some(alias) = state.aliases.get(model) {
        let n = alias.targets.len();
        let index = match alias.strategy {
            Strategy::Ordered => 0,
            Strategy::RoundRobin => alias.cursor.fetch_add(1, Ordering::Relaxed) % n,
        };
        (alias.targets[index].clone(), Some((model.to_string(), index)))
    } else if state.server.allow_direct_models {
        check_local_engine(state, who, model)?;
        (Deployment { model: model.to_string(), api_key: None, base_url: None }, None)
    } else {
        return Err(unknown_model(model));
    };
    with_credentials(state, &mut d, Mode::Parse)?;
    Ok((d, origin))
}

// ---------------------------------------------------------------------------------------------
// Handlers

pub(crate) struct Served {
    pub(crate) body: Value,
    pub(crate) model: String,
    pub(crate) provider: String,
    /// Pages and cost this request adds to the metrics (and, with `billed`, to the key's usage).
    pub(crate) pages: u32,
    pub(crate) cost_usd: Option<f64>,
    pub(crate) fallback_index: usize,
    pub(crate) status: StatusCode,
    /// Charge `cost_usd` / `pages` to the caller's key in [`finish`]. Jobs charge their owner
    /// through `UsageStore::settle_job` instead.
    pub(crate) billed: bool,
}

impl Served {
    fn synchronous(body: Value, model: String, provider: String, pages: u32, cost_usd: Option<f64>, i: usize) -> Self {
        Self { body, model, provider, pages, cost_usd, fallback_index: i, status: StatusCode::OK, billed: true }
    }
}

/// Per-request context for logging and metrics.
pub(crate) struct Ctx {
    request_id: String,
    started: Instant,
    method: &'static str,
    path: String,
    mode: Mode,
    /// Metrics `mode` label: the mode for synchronous calls, `job_submit` / `job_retrieve` /
    /// `webhook` for the jobs API.
    label: &'static str,
    pub(crate) key_id: String,
    /// The requested model, as sent (logged).
    pub(crate) requested: Option<String>,
    /// The requested model once it resolved to an alias or registry model (metrics label).
    pub(crate) model: Option<String>,
    /// Gateway job id and the status observed (jobs API only).
    pub(crate) job_id: Option<String>,
    pub(crate) job_status: Option<&'static str>,
}

impl Ctx {
    pub(crate) fn new(req: &Request, method: &'static str, mode: Mode, label: &'static str) -> Self {
        let request_id = req
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty() && s.len() <= 128 && s.chars().all(|c| c.is_ascii_graphic()))
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        Ctx {
            request_id,
            started: Instant::now(),
            method,
            path: req.uri().path().to_string(),
            mode,
            label,
            key_id: "-".into(),
            requested: None,
            model: None,
            job_id: None,
            job_status: None,
        }
    }

    pub(crate) fn request_id(&self) -> &str {
        &self.request_id
    }
}

async fn run(State(state): State<Arc<AppState>>, mode: Mode, req: Request) -> Response {
    let mut ctx = Ctx::new(&req, "POST", mode, mode.as_str());
    let result = handle(&state, &mut ctx, req).await;
    finish(&state, &ctx, result)
}

/// `provider_options` keys a gateway caller may not set. `cmd` / `pdftoppm_cmd` choose the local
/// executable Tesseract runs (with the uploaded file as its first argument), so accepting them
/// would let any key holder run arbitrary programs on the gateway host. Operators configure these
/// through `TESSERACT_CMD` / `PDFTOPPM_CMD` instead.
const OPERATOR_ONLY_OPTIONS: &[&str] = &["cmd", "pdftoppm_cmd"];

/// The `DocumentRequest` shared by `/v1/{parse,ocr,extract}` and `/v1/jobs` (model and
/// credentials are set per deployment by the caller).
pub(crate) fn build_doc(state: &AppState, body: &mut ApiRequest) -> Result<DocumentRequest, ApiError> {
    if let Some(Value::Object(opts)) = &body.provider_options {
        if let Some(k) = OPERATOR_ONLY_OPTIONS.iter().find(|k| opts.contains_key(**k)) {
            return Err(ApiError::input(format!(
                "provider_options.{k} is not accepted by the gateway (it selects a program on the server)"
            )));
        }
    }
    let input = build_input(body)?;
    let mut doc = DocumentRequest::new(input);
    doc.pages = body.pages.take();
    doc.language = body.language.take();
    doc.output = body.output.unwrap_or_default();
    doc.provider_options = body.provider_options.take();
    doc.include_raw = body.include_raw.unwrap_or(false);
    doc.metadata = body.metadata.take().unwrap_or_default();
    doc.max_retries = body.max_retries.unwrap_or(state.server.max_retries).min(10);
    let max_t = state.server.max_timeout_secs;
    doc.timeout_secs = match body.timeout {
        Some(t) if t.is_finite() && t > 0.0 => t.min(max_t),
        Some(_) => return Err(ApiError::input("'timeout' must be a positive number of seconds")),
        None => max_t,
    };
    Ok(doc)
}

/// Parse an `output_format` value (`None` → the unified response).
pub(crate) fn output_shape(format: Option<&str>) -> Result<puffinparse_core::OutputShape, ApiError> {
    match format {
        None => Ok(puffinparse_core::OutputShape::Puffinparse),
        Some(f) => f.parse().map_err(ApiError::from),
    }
}

async fn handle(state: &AppState, ctx: &mut Ctx, req: Request) -> Result<Served, ApiError> {
    let who = authenticate(state, req.headers())?;
    ctx.key_id = who.id(state).to_string();
    let mut body = decode_body(req).await?;
    let model =
        body.model.clone().filter(|m| !m.trim().is_empty()).ok_or_else(|| ApiError::input("'model' is required"))?;
    ctx.requested = Some(model.clone());
    let fallbacks = body.fallbacks.clone().unwrap_or_default();
    let (deployments, fallback_on) = plan(state, &who, ctx.mode, &model, &fallbacks)?;
    ctx.model = Some(model);
    check_limits(state, &who)?;

    if body.webhook_url.is_some() {
        return Err(ApiError::input("'webhook_url' belongs to POST /v1/jobs (asynchronous parse)"));
    }
    let doc = build_doc(state, &mut body)?;
    check_document_url(state, &doc, ctx.mode, &deployments)?;
    let format = output_shape(body.output_format.as_deref())?;
    if ctx.mode == Mode::Ocr && format != puffinparse_core::OutputShape::Puffinparse {
        return Err(ApiError::input("output_format applies to /v1/parse and /v1/extract only"));
    }
    let extract = if ctx.mode == Mode::Extract {
        let schema =
            body.schema.take().ok_or_else(|| ApiError::input("'schema' (a JSON Schema object) is required"))?;
        Some((schema, body.instructions.take(), body.citations.unwrap_or(false)))
    } else {
        if body.schema.is_some() || body.instructions.is_some() || body.citations.is_some() {
            return Err(ApiError::input("'schema', 'instructions' and 'citations' belong to /v1/extract"));
        }
        None
    };

    let mut last_err: Option<Error> = None;
    let n = deployments.len();
    for (i, d) in deployments.into_iter().enumerate() {
        let mut req = doc.clone();
        req.model = d.model.clone();
        req.api_key = d.api_key;
        req.base_url = d.base_url;
        let outcome = call(ctx.mode, req, extract.clone(), format, i, last_err.as_ref()).await;
        match outcome {
            Ok(served) => return Ok(served),
            Err(e) => {
                let can_fallback = fallback_on.contains(&e.kind) && i + 1 < n;
                tracing::warn!(request_id = %ctx.request_id, model = %d.model, kind = %e.kind, can_fallback, "gateway: target failed");
                if !can_fallback {
                    return Err(e.into());
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.map(ApiError::from).unwrap_or_else(|| ApiError::input("no model to try")))
}

async fn call(
    mode: Mode,
    req: DocumentRequest,
    extract: Option<(Value, Option<String>, bool)>,
    format: puffinparse_core::OutputShape,
    index: usize,
    prior: Option<&Error>,
) -> Result<Served, Error> {
    let note = |meta: &mut BTreeMap<String, Value>| {
        if index > 0 {
            meta.insert("puffinparse_fallback_index".into(), json!(index));
            if let Some(e) = prior {
                meta.insert("puffinparse_fallback_from_error".into(), json!(e.to_string()));
            }
        }
    };
    let to_value = |v: Result<Value, serde_json::Error>| v.map_err(|e| Error::provider(format!("serialise: {e}")));
    match (mode, extract) {
        (Mode::Extract, Some((schema, instructions, citations))) => {
            let mut x = ExtractRequest::new(req, schema).citations(citations);
            x.instructions = instructions;
            let mut r = puffinparse_core::extract(x).await?;
            note(&mut r.metadata);
            let body = match format {
                puffinparse_core::OutputShape::Puffinparse => to_value(serde_json::to_value(&r))?,
                f => puffinparse_core::render_extract(&r, f),
            };
            Ok(Served::synchronous(body, r.model, r.provider, r.usage.pages, r.cost_usd, index))
        }
        (Mode::Ocr, _) => {
            let mut r = puffinparse_core::ocr(req).await?;
            note(&mut r.metadata);
            let body = to_value(serde_json::to_value(&r))?;
            Ok(Served::synchronous(body, r.model, r.provider, r.usage.pages, r.cost_usd, index))
        }
        _ => {
            let mut r = puffinparse_core::parse(req).await?;
            note(&mut r.metadata);
            let body = match format {
                puffinparse_core::OutputShape::Puffinparse => to_value(serde_json::to_value(&r))?,
                f => puffinparse_core::render_parse(&r, f),
            };
            Ok(Served::synchronous(body, r.model, r.provider, r.usage.pages, r.cost_usd, index))
        }
    }
}

/// Account, log, observe, and turn the outcome into an HTTP response.
pub(crate) fn finish(state: &AppState, ctx: &Ctx, result: Result<Served, ApiError>) -> Response {
    let latency = ctx.started.elapsed();
    let mut log = RequestLog {
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        request_id: ctx.request_id.clone(),
        key_id: ctx.key_id.clone(),
        method: ctx.method.into(),
        path: ctx.path.clone(),
        mode: Some(ctx.mode.as_str().to_string()),
        model: ctx.requested.clone(),
        served_model: None,
        provider: None,
        fallback_index: None,
        pages: None,
        cost_usd: None,
        latency_ms: latency.as_millis() as u64,
        status: 200,
        error_type: None,
        provider_status: None,
        job_id: ctx.job_id.clone(),
        job_status: ctx.job_status.map(str::to_string),
    };
    let mut resp = match result {
        Ok(s) => {
            if s.billed && ctx.key_id != "-" {
                state.usage.record(&ctx.key_id, s.cost_usd.unwrap_or(0.0), s.pages);
            }
            state.metrics.observe(&Observation {
                mode: ctx.label,
                model: &s.model,
                status: s.status.as_u16(),
                error_type: None,
                latency_secs: latency.as_secs_f64(),
                pages: s.pages,
                cost_usd: s.cost_usd,
                fell_back: s.fallback_index > 0,
            });
            log.served_model = Some(s.model.clone());
            log.provider = Some(s.provider);
            log.fallback_index = Some(s.fallback_index);
            log.pages = Some(s.pages);
            log.cost_usd = s.cost_usd;
            log.status = s.status.as_u16();
            let mut resp = (s.status, Json(s.body)).into_response();
            if let Ok(v) = HeaderValue::from_str(&s.model) {
                resp.headers_mut().insert("x-puffinparse-model", v);
            }
            if let Some(c) = s.cost_usd {
                if let Ok(v) = HeaderValue::from_str(&format!("{c:.6}")) {
                    resp.headers_mut().insert("x-puffinparse-cost-usd", v);
                }
            }
            resp
        }
        Err(e) => {
            let e = e.with_request_id(&ctx.request_id);
            state.metrics.observe(&Observation {
                mode: ctx.label,
                // Only label with a model name that passed validation, to bound cardinality.
                model: ctx.model.as_deref().unwrap_or("-"),
                status: e.status.as_u16(),
                error_type: Some(&e.error_type),
                latency_secs: latency.as_secs_f64(),
                pages: 0,
                cost_usd: None,
                fell_back: false,
            });
            log.status = e.status.as_u16();
            log.error_type = Some(e.error_type.clone());
            log.provider = e.provider.clone();
            log.provider_status = e.provider_status;
            e.into_response()
        }
    };
    if let Ok(v) = HeaderValue::from_str(&ctx.request_id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    state.logger.write(&log);
    resp
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "version": puffinparse_core::VERSION }))
}

async fn metrics(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !state.server.public_metrics {
        if let Err(e) = authenticate(&state, &headers) {
            return e.into_response();
        }
    }
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], state.metrics.render()).into_response()
}

fn key_configured(state: &AppState, provider: &str, env_var: &str) -> bool {
    state.providers.get(provider).is_some_and(|(k, _)| k.is_some())
        || std::env::var(env_var).map(|v| !v.trim().is_empty()).unwrap_or(false)
}

async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let who = match authenticate(&state, &headers) {
        Ok(w) => w,
        Err(e) => return e.into_response(),
    };
    let patterns: &[String] = match &who {
        Principal::Key(i) => &state.keys[*i].models,
        _ => &[],
    };
    let prices = puffinparse_core::pricing::all_prices();
    let mut data = Vec::new();
    for (name, a) in &state.aliases {
        if !allowed(patterns, name) {
            continue;
        }
        let targets: Vec<&str> = a.targets.iter().map(|t| t.model.as_str()).collect();
        let modes: Vec<&str> = [Mode::Parse, Mode::Ocr, Mode::Extract]
            .into_iter()
            .filter(|m| targets.iter().all(|t| ModelRef::parse_for(t, *m).is_ok()))
            .map(|m| m.as_str())
            .collect();
        data.push(json!({
            "id": name, "type": "alias", "targets": targets, "modes": modes,
            "strategy": a.strategy, "fallback_on": a.fallback_on,
        }));
    }
    if state.server.allow_direct_models {
        for p in puffinparse_core::PROVIDERS {
            for m in p.models {
                let q = m.qualified();
                if !allowed(patterns, &q) || !may_use_local_engine(&state, &who, &q) {
                    continue;
                }
                let per_page: serde_json::Map<String, Value> = m
                    .modes
                    .iter()
                    .filter_map(|md| {
                        prices.get(&q).and_then(|e| e.for_mode(*md)).map(|v| (md.as_str().to_string(), json!(v)))
                    })
                    .collect();
                data.push(json!({
                    "id": q, "type": "model", "provider": p.name, "modes": m.modes, "default": m.default,
                    "description": m.description, "per_page_usd": per_page,
                    "key_configured": key_configured(&state, p.name, p.env_var),
                }));
            }
        }
    }
    Json(json!({ "object": "list", "data": data })).into_response()
}

async fn usage(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let who = match authenticate(&state, &headers) {
        Ok(w) => w,
        Err(e) => return e.into_response(),
    };
    let row = |k: &crate::VirtualKey| {
        let u = state.usage.get(&k.id);
        json!({
            "id": k.id, "month": u.month, "spend_usd": u.spend_usd, "requests": u.requests, "pages": u.pages,
            "monthly_budget_usd": k.monthly_budget_usd,
            "remaining_usd": k.monthly_budget_usd.map(|b| (b - u.spend_usd).max(0.0)),
            "rpm": k.rpm, "models": k.models,
        })
    };
    let keys: Vec<Value> = match who {
        Principal::Key(i) => vec![row(&state.keys[i])],
        _ => state.keys.iter().map(row).collect(),
    };
    Json(json!({ "keys": keys })).into_response()
}

#[cfg(test)]
mod tests {
    use super::allowed;

    #[test]
    fn allow_list_prefixes_work_for_nested_model_ids() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(allowed(&p(&["reducto/*"]), "reducto"));
        assert!(allowed(&p(&["reducto/*"]), "reducto/r-1"));
        assert!(!allowed(&p(&["reducto/*"]), "reductox/r-1"));
        assert!(allowed(&p(&["opendocrouter/*"]), "opendocrouter/google/gemini-3-flash"));
        assert!(allowed(&p(&["opendocrouter/google/*"]), "opendocrouter/google/gemini-3-flash"));
        assert!(!allowed(&p(&["opendocrouter/google/*"]), "opendocrouter/openai/gpt-6-luna"));
        assert!(allowed(&p(&["opendocrouter/openai/gpt-6-luna"]), "OpenDocRouter/OpenAI/GPT-6-Luna"));
        assert!(allowed(&p(&[]), "anything"));
    }
}
