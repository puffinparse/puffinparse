//! # puffinparse-core
//!
//! One API for every OCR / document-parsing provider.
//!
//! ```no_run
//! use puffinparse_core::{parse, DocumentRequest};
//!
//! # async fn run() -> puffinparse_core::Result<()> {
//! let resp = parse(DocumentRequest::from_path("invoice.pdf").model("reducto/standard")).await?;
//! println!("{}", resp.markdown);
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod bench;
pub mod compat;
pub mod error;
pub mod fetch;
pub mod http;
pub mod jobs;
pub mod model;
pub mod pricing;
pub mod provider;
pub mod providers;
pub mod router;
#[cfg(test)]
pub(crate) mod testutil;
pub mod types;
pub mod util;

pub use compat::{render_extract, render_parse, Format as OutputShape};
pub use error::{Error, ErrorKind, Result};
pub use jobs::{JobHandle, JobStatus, RetrieveOptions, WebhookEvent, WebhookStatus};
pub use model::{list_models, list_models_for, model_info, ModelInfo, ModelRef, ProviderInfo, PROVIDERS};
pub use provider::Provider;
pub use router::{Router, RouterConfig, Strategy};
pub use types::{
    BBox, Block, BlockType, Citation, DocumentInput, DocumentRequest, ExtractRequest, ExtractResponse, FieldInfo, Line,
    Mode, OutputFormat, Page, ParseResponse, TextPage, TextResponse, Usage, Word,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run a `parse`-mode request (layout-aware markdown + blocks) with the model in `request.model`.
///
/// This is the entry point used by the Python SDK, the CLI and the [`Router`].
pub async fn parse(request: DocumentRequest) -> Result<ParseResponse> {
    let (provider, model_ref) = resolve(&request.model, Mode::Parse)?;
    let started = std::time::Instant::now();
    let qualified = model_ref.qualified();
    tracing::info!(mode = "parse", model = %qualified, input = %request.input.describe(), "puffinparse: starting");
    let result = provider.parse(&request, &model_ref.model).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(resp) => {
            let resp = finish_parse(resp, &model_ref, latency_ms, &request.metadata, request.include_raw);
            log_done("parse", &qualified, resp.usage.pages, latency_ms, resp.cost_usd);
            Ok(resp)
        }
        Err(e) => Err(log_failed("parse", &qualified, latency_ms, e, &model_ref.provider)),
    }
}

/// The unified post-processing every `parse` result gets, synchronous or retrieved from a job.
fn finish_parse(
    mut resp: ParseResponse,
    model_ref: &ModelRef,
    latency_ms: u64,
    metadata: &std::collections::BTreeMap<String, serde_json::Value>,
    include_raw: bool,
) -> ParseResponse {
    let qualified = model_ref.qualified();
    resp.model = qualified.clone();
    resp.provider = model_ref.provider.clone();
    resp.latency_ms = latency_ms;
    resp.metadata.extend(metadata.clone());
    if resp.cost_usd.is_none() {
        resp.cost_usd =
            resp.usage.provider_cost_usd.or_else(|| pricing::estimate_cost(&qualified, Mode::Parse, resp.usage.pages));
    }
    if !include_raw {
        resp.raw = None;
    }
    resp
}

// ---- asynchronous jobs ---------------------------------------------------------------------------

/// Start a `parse` job and return at once with a [`JobHandle`] (see [`jobs`]).
///
/// Supported by the providers with a job queue: `reducto` (`/parse_async`), `extend`
/// (`/parse_runs`) and `llamaparse` (`/parsing/upload`). `request.webhook_url` registers a
/// provider webhook where the provider supports one per job.
pub async fn submit_parse(request: DocumentRequest) -> Result<JobHandle> {
    let (provider, model_ref) = resolve(&request.model, Mode::Parse)?;
    let qualified = model_ref.qualified();
    provider::webhook_url(&request)?;
    tracing::info!(mode = "parse", model = %qualified, input = %request.input.describe(), "puffinparse: submitting job");
    let mut job = provider.submit_parse(&request, &model_ref.model).await.map_err(|e| {
        if e.provider.is_none() {
            e.with_provider(&model_ref.provider)
        } else {
            e
        }
    })?;
    job.provider = model_ref.provider.clone();
    job.model = qualified;
    job.output = request.output;
    job.include_raw = request.include_raw;
    job.base_url = request.base_url.clone();
    job.metadata = request.metadata.clone();
    tracing::info!(model = %job.model, job_id = %job.job_id, "puffinparse: job submitted");
    Ok(job)
}

/// Check a submitted job once, with credentials from the environment.
pub async fn retrieve_parse(job: &JobHandle) -> Result<JobStatus> {
    retrieve_parse_with(job, &RetrieveOptions::default()).await
}

/// Check a submitted job once. A finished job's response is normalised exactly like [`parse`]'s;
/// its `latency_ms` is the time since submission. A provider-side failure is returned as
/// `Ok(JobStatus::Failed(_))`; `Err` means the status check itself failed (auth, network, …).
pub async fn retrieve_parse_with(job: &JobHandle, opts: &RetrieveOptions) -> Result<JobStatus> {
    let (provider, model_ref) = resolve(&job.model, Mode::Parse)?;
    let request = job.request(opts);
    let status = provider.retrieve_parse(job, &request, &model_ref.model).await.map_err(|e| {
        if e.provider.is_none() {
            e.with_provider(&model_ref.provider)
        } else {
            e
        }
    })?;
    Ok(match status {
        JobStatus::Succeeded(resp) => {
            let latency_ms = job.elapsed_ms();
            let resp = finish_parse(*resp, &model_ref, latency_ms, &job.metadata, job.include_raw);
            log_done("parse", &model_ref.qualified(), resp.usage.pages, latency_ms, resp.cost_usd);
            JobStatus::Succeeded(Box::new(resp))
        }
        JobStatus::Failed(e) => {
            let e = if e.provider.is_none() { e.with_provider(&model_ref.provider) } else { e };
            let e = if e.job_id.is_none() { e.with_job_id(job.job_id.clone()) } else { e };
            JobStatus::Failed(e)
        }
        JobStatus::Pending => JobStatus::Pending,
    })
}

/// Interpret the JSON body a provider POSTed to your webhook, without any network call.
///
/// `model` names the provider (or a model of it, which the returned handle then carries), e.g.
/// `"reducto"` or `"llamaparse/agentic"`.
pub fn parse_webhook(model: &str, payload: &serde_json::Value) -> Result<WebhookEvent> {
    let (provider, model_ref) = resolve(model, Mode::Parse)?;
    let mut event = provider.parse_webhook(&model_ref.model, payload)?;
    if let Some(job) = event.job.as_mut() {
        job.provider = model_ref.provider.clone();
        job.model = model_ref.qualified();
    }
    if let WebhookStatus::Succeeded(resp) = event.status {
        let resp = finish_parse(*resp, &model_ref, 0, &Default::default(), false);
        event.status = WebhookStatus::Succeeded(Box::new(resp));
    }
    Ok(event)
}

/// [`parse_webhook`], then — when the body only says the job finished — one [`retrieve_parse_with`]
/// to fetch the result or the provider's failure message.
pub async fn resolve_webhook(model: &str, payload: &serde_json::Value, opts: &RetrieveOptions) -> Result<JobStatus> {
    let event = parse_webhook(model, payload)?;
    match event.status {
        WebhookStatus::Pending => Ok(JobStatus::Pending),
        WebhookStatus::Succeeded(resp) => Ok(JobStatus::Succeeded(resp)),
        WebhookStatus::Failed(e) => Ok(JobStatus::Failed(e)),
        WebhookStatus::Finished => {
            let job = event.job.ok_or_else(|| Error::input("webhook payload names no job id to retrieve"))?;
            retrieve_parse_with(&job, opts).await
        }
    }
}

/// Run an `ocr`-mode request (plain text + word/line boxes) with the model in `request.model`.
pub async fn ocr(request: DocumentRequest) -> Result<TextResponse> {
    let (provider, model_ref) = resolve(&request.model, Mode::Ocr)?;
    let started = std::time::Instant::now();
    let qualified = model_ref.qualified();
    tracing::info!(mode = "ocr", model = %qualified, input = %request.input.describe(), "puffinparse: starting");
    let result = provider.ocr(&request, &model_ref.model).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(mut resp) => {
            resp.model = qualified.clone();
            resp.provider = model_ref.provider.clone();
            resp.latency_ms = latency_ms;
            resp.metadata.extend(request.metadata.clone());
            if resp.cost_usd.is_none() {
                resp.cost_usd = resp
                    .usage
                    .provider_cost_usd
                    .or_else(|| pricing::estimate_cost(&qualified, Mode::Ocr, resp.usage.pages));
            }
            if !request.include_raw {
                resp.raw = None;
            }
            log_done("ocr", &qualified, resp.usage.pages, latency_ms, resp.cost_usd);
            Ok(resp)
        }
        Err(e) => Err(log_failed("ocr", &qualified, latency_ms, e, &model_ref.provider)),
    }
}

/// Run an `extract`-mode request (JSON schema → structured data) with the model in `request.document.model`.
pub async fn extract(request: ExtractRequest) -> Result<ExtractResponse> {
    if !request.schema.is_object() {
        return Err(Error::input("extract: schema must be a JSON object (JSON Schema)"));
    }
    let (provider, model_ref) = resolve(&request.document.model, Mode::Extract)?;
    let started = std::time::Instant::now();
    let qualified = model_ref.qualified();
    tracing::info!(mode = "extract", model = %qualified, input = %request.document.input.describe(), "puffinparse: starting");
    let result = provider.extract(&request, &model_ref.model).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(mut resp) => {
            resp.model = qualified.clone();
            resp.provider = model_ref.provider.clone();
            resp.latency_ms = latency_ms;
            resp.metadata.extend(request.document.metadata.clone());
            if resp.cost_usd.is_none() {
                resp.cost_usd = resp
                    .usage
                    .provider_cost_usd
                    .or_else(|| pricing::estimate_cost(&qualified, Mode::Extract, resp.usage.pages));
            }
            if !request.document.include_raw {
                resp.raw = None;
            }
            log_done("extract", &qualified, resp.usage.pages, latency_ms, resp.cost_usd);
            Ok(resp)
        }
        Err(e) => Err(log_failed("extract", &qualified, latency_ms, e, &model_ref.provider)),
    }
}

fn resolve(model: &str, mode: Mode) -> Result<(std::sync::Arc<dyn Provider>, ModelRef)> {
    let model_ref = ModelRef::parse_for(model, mode)?;
    let provider = providers::build(&model_ref.provider)?;
    Ok((provider, model_ref))
}

fn log_done(mode: &str, model: &str, pages: u32, latency_ms: u64, cost_usd: Option<f64>) {
    tracing::info!(mode, model, pages, latency_ms, cost_usd = ?cost_usd, "puffinparse: done");
}

fn log_failed(mode: &str, model: &str, latency_ms: u64, e: Error, provider: &str) -> Error {
    let e = if e.provider.is_none() { e.with_provider(provider) } else { e };
    tracing::warn!(mode, model, latency_ms, error = %e, "puffinparse: failed");
    e
}

/// Blocking wrapper around [`parse`] for non-async callers. Builds a small runtime per call.
pub fn parse_blocking(request: DocumentRequest) -> Result<ParseResponse> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::provider(format!("failed to start runtime: {e}")))?;
    rt.block_on(parse(request))
}
