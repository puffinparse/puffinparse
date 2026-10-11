//! The async jobs API (SPEC §14–15): `POST /v1/jobs`, `GET /v1/jobs/{id}`, and the optional
//! provider webhook receiver `POST /v1/webhooks/{provider}`.
//!
//! A job is `puffinparse_core::submit_parse` on one deployment (an alias's first target in its
//! strategy order: jobs never fall back). The core's `JobHandle` is stored under an opaque
//! gateway id bound to the key that submitted it; only that key (and the master key) can read
//! it, everyone else gets 404. Status checks re-resolve the provider credentials from the config,
//! so nothing secret is stored. The job's cost is charged to its owner the first time it is
//! observed succeeded — by a `GET` or a webhook — and never again.

use crate::api::{self, Ctx, Principal, Served};
use crate::error::ApiError;
use crate::usage::{JobOutcome, StoredJob};
use crate::{AppState, Deployment};
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use puffinparse_core::model::ModelRef;
use puffinparse_core::{JobHandle, JobStatus, Mode, OutputShape, RetrieveOptions, WebhookStatus};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// Deadline of one status check (a finished job's result download included).
const RETRIEVE_TIMEOUT_SECS: f64 = 120.0;

// ---- POST /v1/jobs -------------------------------------------------------------------------------

pub(crate) async fn submit(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let mut ctx = Ctx::new(&req, "POST", Mode::Parse, "job_submit");
    let result = handle_submit(&state, &mut ctx, req).await;
    api::finish(&state, &ctx, result)
}

async fn handle_submit(state: &AppState, ctx: &mut Ctx, req: Request) -> Result<Served, ApiError> {
    let who = api::authenticate(state, req.headers())?;
    ctx.key_id = who.id(state).to_string();
    let mut body = api::decode_body(req).await?;
    let model =
        body.model.clone().filter(|m| !m.trim().is_empty()).ok_or_else(|| ApiError::input("'model' is required"))?;
    ctx.requested = Some(model.clone());
    if body.fallbacks.as_ref().is_some_and(|f| !f.is_empty()) {
        return Err(ApiError::input(
            "'fallbacks' does not apply to /v1/jobs: a job is submitted to one model (an alias's first target) \
             and never falls back",
        ));
    }
    if body.schema.is_some() || body.instructions.is_some() || body.citations.is_some() {
        return Err(ApiError::input("'schema', 'instructions' and 'citations' belong to /v1/extract; jobs are parse"));
    }
    let (deployment, origin) = api::plan_job(state, &who, &model)?;
    ctx.model = Some(model);
    api::check_limits(state, &who)?;

    let mut doc = api::build_doc(state, &mut body)?;
    api::check_document_url(state, &doc, Mode::Parse, std::slice::from_ref(&deployment))?;
    let output_format = body.output_format.take();
    api::output_shape(output_format.as_deref())?;
    doc.webhook_url = body.webhook_url.take();
    doc.model = deployment.model;
    doc.api_key = deployment.api_key;
    doc.base_url = deployment.base_url;
    let handle = puffinparse_core::submit_parse(doc).await?;

    let id = format!("job_{}", uuid::Uuid::new_v4().simple());
    let (alias, target_index) = origin.map_or((None, 0), |(a, i)| (Some(a), i));
    let stored = StoredJob {
        owner: ctx.key_id.clone(),
        handle: handle.clone(),
        alias,
        target_index,
        output_format,
        outcome: None,
        created_unix: chrono::Utc::now().timestamp(),
        playground: None,
    };
    let retention = i64::try_from(state.server.job_retention_hours.saturating_mul(3600)).unwrap_or(i64::MAX);
    state.usage.insert_job(&id, stored, retention);
    state.metrics.job_event("submitted");
    ctx.job_id = Some(id.clone());
    ctx.job_status = Some("pending");
    Ok(Served {
        body: json!({ "id": id, "object": "job", "status": "pending", "job": public_handle(&handle) }),
        model: handle.model,
        provider: handle.provider,
        pages: 0,
        cost_usd: None,
        fallback_index: 0,
        status: StatusCode::ACCEPTED,
        billed: false,
    })
}

/// The handle as clients see it: the operator's `base_url` is not theirs to know.
fn public_handle(handle: &JobHandle) -> Value {
    let mut v = serde_json::to_value(handle).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("base_url");
    }
    v
}

// ---- GET /v1/jobs/{id} ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetrieveQuery {
    /// Overrides the `output_format` sent at submit time.
    output_format: Option<String>,
}

pub(crate) async fn retrieve(State(state): State<Arc<AppState>>, Path(id): Path<String>, req: Request) -> Response {
    let mut ctx = Ctx::new(&req, "GET", Mode::Parse, "job_retrieve");
    let result = handle_retrieve(&state, &mut ctx, &id, req).await;
    api::finish(&state, &ctx, result)
}

async fn handle_retrieve(state: &AppState, ctx: &mut Ctx, id: &str, req: Request) -> Result<Served, ApiError> {
    // Playground jobs belong to a signed-in user or a provider key, not a gateway key.
    if let Some(pg) = state.playground.as_ref() {
        if let Some(job) = state.usage.job(id).filter(|j| j.playground.is_some()) {
            if !api::is_master(state, req.headers()) {
                let access = pg.job_access(state, req.headers(), id, &job).await?;
                ctx.key_id = access.log_id;
                ctx.job_id = Some(id.to_string());
                ctx.requested = Some(job.handle.model.clone());
                ctx.model = ctx.requested.clone();
                let query: RetrieveQuery = Query::try_from_uri(req.uri())
                    .map(|q| q.0)
                    .map_err(|e| ApiError::input(format!("invalid query string: {}", e.body_text())))?;
                let format = api::output_shape(query.output_format.as_deref())?;
                let status = retrieve_status(state, &job, access.provider_key).await?;
                return Ok(settle(state, ctx, id, &job, status, format));
            }
        }
    }
    let who = api::authenticate(state, req.headers())?;
    ctx.key_id = who.id(state).to_string();
    let query: RetrieveQuery = Query::try_from_uri(req.uri())
        .map(|q| q.0)
        .map_err(|e| ApiError::input(format!("invalid query string: {}", e.body_text())))?;
    // Another key's job is indistinguishable from a missing one.
    let job = state
        .usage
        .job(id)
        .filter(|j| matches!(who, Principal::Master) || j.owner == ctx.key_id)
        .ok_or_else(|| ApiError::not_found(format!("no job '{id}' for this key")))?;
    ctx.job_id = Some(id.to_string());
    ctx.requested = Some(job.alias.clone().unwrap_or_else(|| job.handle.model.clone()));
    ctx.model = ctx.requested.clone();
    api::check_rate(state, &who)?;
    let format = api::output_shape(query.output_format.as_deref().or(job.output_format.as_deref()))?;
    if job.playground.is_some() && !matches!(who, Principal::Master) {
        return Err(ApiError::not_found(format!("no job '{id}' for this key")));
    }
    let status = retrieve_status(state, &job, None).await?;
    Ok(settle(state, ctx, id, &job, status, format))
}

/// The credentials the job was submitted with: its alias target's own, else `[providers.*]`.
fn deployment(state: &AppState, job: &StoredJob) -> Deployment {
    let target = job
        .alias
        .as_deref()
        .and_then(|a| state.aliases.get(a))
        .and_then(|a| a.targets.get(job.target_index))
        .filter(|t| ModelRef::parse(&t.model).is_ok_and(|m| m.qualified() == job.handle.model))
        .cloned();
    let mut d = target.unwrap_or(Deployment { model: job.handle.model.clone(), api_key: None, base_url: None });
    let (key, base) = state.providers.get(&job.handle.provider).cloned().unwrap_or_default();
    d.api_key = d.api_key.or(key);
    d.base_url = d.base_url.or(base);
    d
}

/// One provider status check. `Err` means the check itself failed (auth, network, ...).
/// `key` replaces the configured credentials (own-key playground jobs).
async fn retrieve_status(state: &AppState, job: &StoredJob, key: Option<String>) -> Result<JobStatus, ApiError> {
    let d = deployment(state, job);
    if job.playground.as_ref().is_some_and(|p| p.user.is_none()) && key.is_none() {
        // An own-key job has no gateway credentials to fall back to (master key, webhook).
        return Err(ApiError::input(
            "this job was submitted with the caller's own provider key; poll it with that key",
        ));
    }
    let opts = RetrieveOptions {
        api_key: key.or(d.api_key),
        base_url: d.base_url,
        timeout_secs: state.server.max_timeout_secs.min(RETRIEVE_TIMEOUT_SECS),
        max_retries: state.server.max_retries.min(10),
    };
    Ok(puffinparse_core::retrieve_parse_with(&job.handle, &opts).await?)
}

/// Record the observed status (charging the owner on the first success) and build the body
/// `{id, object, status, model, provider, provider_job_id, submitted_at, result? | error?}`.
fn settle(
    state: &AppState,
    ctx: &mut Ctx,
    id: &str,
    job: &StoredJob,
    status: JobStatus,
    format: OutputShape,
) -> Served {
    let h = &job.handle;
    let mut body = json!({
        "id": id, "object": "job", "model": h.model, "provider": h.provider,
        "provider_job_id": h.job_id, "submitted_at": h.submitted_at,
    });
    let (mut pages, mut cost_usd) = (0, None);
    let label = match status {
        JobStatus::Pending => "pending",
        JobStatus::Succeeded(resp) => {
            if state.usage.settle_job(id, JobOutcome::Succeeded, resp.cost_usd.unwrap_or(0.0), resp.usage.pages) {
                state.metrics.job_event("succeeded");
                (pages, cost_usd) = (resp.usage.pages, resp.cost_usd);
                if let Some(pg) = &state.playground {
                    pg.settled(job, JobOutcome::Succeeded, resp.usage.pages, resp.cost_usd);
                }
            }
            body["result"] = match format {
                OutputShape::Puffinparse => serde_json::to_value(&*resp).unwrap_or(Value::Null),
                f => puffinparse_core::render_parse(&resp, f),
            };
            "succeeded"
        }
        JobStatus::Failed(e) => {
            if state.usage.settle_job(id, JobOutcome::Failed, 0.0, 0) {
                state.metrics.job_event("failed");
                if let Some(pg) = &state.playground {
                    pg.settled(job, JobOutcome::Failed, 0, None);
                }
            }
            body["error"] = ApiError::from(e).with_request_id(ctx.request_id()).error_object();
            "failed"
        }
    };
    body["status"] = json!(label);
    ctx.job_status = Some(label);
    Served {
        body,
        model: h.model.clone(),
        provider: h.provider.clone(),
        pages,
        cost_usd,
        fallback_index: 0,
        status: StatusCode::OK,
        billed: false,
    }
}

// ---- POST /v1/webhooks/{provider} ----------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct WebhookQuery {
    token: Option<String>,
}

pub(crate) async fn webhook(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    req: Request,
) -> Response {
    let mut ctx = Ctx::new(&req, "POST", Mode::Parse, "webhook");
    let result = handle_webhook(&state, &mut ctx, &provider, req).await;
    api::finish(&state, &ctx, result)
}

async fn handle_webhook(state: &AppState, ctx: &mut Ctx, provider: &str, req: Request) -> Result<Served, ApiError> {
    let Some(secret) = state.webhook_secret.as_deref() else {
        return Err(ApiError::not_found("webhooks are not enabled on this gateway ([webhooks] enabled = true)"));
    };
    ctx.key_id = "webhook".into();
    let query: WebhookQuery = Query::try_from_uri(req.uri()).map(|q| q.0).unwrap_or_default();
    let token = req
        .headers()
        .get("x-puffinparse-webhook-secret")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or(query.token);
    if !token.is_some_and(|t| api::ct_eq(t.trim().as_bytes(), secret.as_bytes())) {
        return Err(ApiError::unauthorized(
            "missing or wrong webhook secret: send '?token=<secret>' or 'x-puffinparse-webhook-secret'",
        ));
    }
    let bytes = bytes::Bytes::from_request(req, &()).await.map_err(|e| api::body_error(e.status(), e.body_text()))?;
    let payload: Value =
        serde_json::from_slice(&bytes).map_err(|e| ApiError::input(format!("webhook body is not JSON: {e}")))?;
    let event = puffinparse_core::parse_webhook(provider, &payload)?;
    let named = event.job.ok_or_else(|| {
        ApiError::input("the webhook body names no job id, so the gateway cannot attribute it to a job")
    })?;
    // Usually one job; several when the provider reused its id (LlamaParse caches identical uploads).
    let jobs = state.usage.find_jobs(&named.provider, &named.job_id);
    let Some((first_id, first)) = jobs.first().cloned() else {
        return Err(ApiError::not_found("no job submitted through this gateway matches this webhook"));
    };
    ctx.job_id = Some(first_id);
    ctx.requested = Some(first.handle.model.clone());
    ctx.model = ctx.requested.clone();
    // Read the body again with the job's own model, so a result it carries is priced for it.
    let status = match puffinparse_core::parse_webhook(&first.handle.model, &payload)?.status {
        WebhookStatus::Pending => JobStatus::Pending,
        WebhookStatus::Succeeded(resp) => JobStatus::Succeeded(resp),
        WebhookStatus::Failed(e) => JobStatus::Failed(e),
        WebhookStatus::Finished => retrieve_status(state, &first, None).await?,
    };
    let mut served: Option<Served> = None;
    let mut ids = Vec::with_capacity(jobs.len());
    for (id, job) in &jobs {
        let s = settle(state, ctx, id, job, status.clone(), OutputShape::Puffinparse);
        ids.push(id.clone());
        served = Some(match served {
            None => s,
            Some(mut acc) => {
                acc.pages += s.pages;
                acc.cost_usd = match (acc.cost_usd, s.cost_usd) {
                    (None, None) => None,
                    (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
                };
                acc
            }
        });
    }
    let mut served = served.expect("at least one job");
    // The provider only needs an acknowledgement; results stay behind GET /v1/jobs/{id}.
    served.body = json!({ "ids": ids, "status": ctx.job_status });
    Ok(served)
}
