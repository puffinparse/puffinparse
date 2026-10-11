//! The public playground API (docs/SERVER.md, "Playground API"): `GET /v1/playground/config` and
//! `POST /v1/playground/runs`, plus access to the jobs a run creates through `GET /v1/jobs/{id}`.
//!
//! A run is one upload fanned out to one async job per model (core `submit_parse`), stored like
//! any gateway job but owned by a playground principal instead of a virtual key:
//!
//! - **Free tier** (`Authorization: Bearer <Supabase access token>` + a Turnstile token): the
//!   gateway's own provider keys pay, so the run reserves `pages x models` of the user's daily
//!   model-pages first (released on failure, trued up to the provider's page count on success), the
//!   day's free spend must be under `free_daily_budget_usd`, and only models priced at or under
//!   `free_tier_max_price_per_page` are offered. Owner: `playground:free:<sha256(user id)>`.
//! - **Your own keys** (`x-provider-key-<provider>`, no `Authorization`): the key is used for this
//!   request and its polls only and never stored or logged. Owner: `playground:byok:<hmac>`, an
//!   HMAC of the key under a random per-process secret, so only the same key can read the job and
//!   nothing about the key outlives the process.
//!
//! Both are limited per client IP (`ip_rpm`) and per principal for status checks (`poll_rpm`).

use crate::api::{self, Ctx, Served};
use crate::config::{resolve_secret, PlaygroundConfig, SupabaseConfig, PLAYGROUND_PROVIDERS};
use crate::error::ApiError;
use crate::pages::{self, Kind};
use crate::usage::{JobOutcome, PlaygroundJob, StoredJob};
use crate::{AppState, Deployment};
use axum::extract::{FromRequest, Multipart, Request, State};
use axum::http::{header, HeaderMap, HeaderName, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use puffinparse_core::model::ModelRef;
use puffinparse_core::{DocumentInput, DocumentRequest, Mode};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

/// The connection's peer address, inserted by the accept loop (absent under `oneshot` tests).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PeerAddr(pub SocketAddr);

/// Status checks of one provider job take at most this long (seconds).
const SUBMIT_TIMEOUT_SECS: f64 = 120.0;
const MAX_PROVIDER_KEY_LEN: usize = 512;

#[derive(Debug, Clone)]
struct PgModel {
    id: String,
    provider: String,
    provider_name: &'static str,
    model: String,
    price: Option<f64>,
    free: bool,
}

pub(crate) struct Playground {
    cfg: PlaygroundConfig,
    models: Vec<PgModel>,
    auth: Option<crate::auth::SupabaseAuth>,
    turnstile_secret: Option<String>,
    quota: Quota,
    http: reqwest::Client,
    /// HMAC key for own-key job owners. Random per process, never stored.
    owner_secret: [u8; 32],
    /// PDF page scans running at once (each holds up to 16 MiB of inflated data).
    scans: tokio::sync::Semaphore,
}

impl std::fmt::Debug for Playground {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Playground")
            .field("cfg", &self.cfg)
            .field("models", &self.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>())
            .field("free_tier_configured", &self.free_configured())
            .field("quota", &self.quota)
            .finish_non_exhaustive()
    }
}

impl Playground {
    pub(crate) fn new(cfg: &PlaygroundConfig) -> Result<Option<Arc<Self>>, String> {
        if !cfg.enabled {
            return Ok(None);
        }
        let secret = |field: &str, v: &Option<String>| -> Result<Option<String>, String> {
            match v {
                None => Ok(None),
                Some(v) => resolve_secret(v).map_err(|e| format!("playground.{field}: {e}")),
            }
        };
        let turnstile_secret = secret("turnstile_secret", &cfg.turnstile_secret)?;
        let (auth, quota) = match &cfg.supabase {
            Some(s) => {
                let hs = secret("supabase.jwt_secret", &s.jwt_secret)?;
                let auth = crate::auth::SupabaseAuth::new(s, hs)?;
                let service = secret("supabase.service_key", &s.service_key)?;
                if s.service_key.is_some() && service.is_none() {
                    return Err("playground.supabase.service_key is set but resolved to an empty value".into());
                }
                (Some(auth), Quota::new(s, service))
            }
            None => (None, Quota::memory()),
        };
        if cfg.turnstile_secret.is_some() && turnstile_secret.is_none() {
            return Err("playground.turnstile_secret is set but resolved to an empty value".into());
        }
        let models = cfg
            .models
            .iter()
            .map(|m| {
                let r = ModelRef::parse_for(m, Mode::Parse).map_err(|e| e.message)?;
                let id = r.qualified();
                let price = puffinparse_core::pricing::price_per_page(&id, Mode::Parse);
                let provider_name = puffinparse_core::model::provider_info(&r.provider).map_or("", |p| p.display_name);
                Ok(PgModel {
                    free: price.is_some_and(|p| p <= cfg.free_tier_max_price_per_page),
                    provider: r.provider,
                    provider_name,
                    model: r.model,
                    price,
                    id,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("playground: cannot build HTTP client: {e}"))?;
        let mut owner_secret = [0u8; 32];
        owner_secret[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        owner_secret[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        let scans = tokio::sync::Semaphore::new(pages::MAX_CONCURRENT_SCANS);
        let pg = Self { cfg: cfg.clone(), models, auth, turnstile_secret, quota, http, owner_secret, scans };
        if !pg.free_configured() {
            eprintln!(
                "playground: the free tier is off (it needs playground.supabase, playground.turnstile_secret and \
                 free_daily_budget_usd > 0); own-key runs are on"
            );
        }
        if matches!(pg.quota, Quota::Memory(_)) && pg.free_configured() {
            eprintln!(
                "playground: free-tier counters are in memory (no playground.supabase.service_key): run one \
                 instance only; they reset on restart"
            );
        }
        Ok(Some(Arc::new(pg)))
    }

    fn free_configured(&self) -> bool {
        self.auth.is_some() && self.turnstile_secret.is_some() && self.cfg.free_daily_budget_usd > 0.0
    }

    fn model(&self, id: &str) -> Option<&PgModel> {
        let q = ModelRef::parse_for(id, Mode::Parse).ok()?.qualified();
        self.models.iter().find(|m| m.id == q)
    }

    /// CORS for the browser-facing routes: the configured origins only, no credentials.
    pub(crate) fn cors(&self) -> tower_http::cors::CorsLayer {
        use tower_http::cors::{AllowOrigin, CorsLayer};
        let origins: Vec<_> = self.cfg.allowed_origins.iter().filter_map(|o| o.parse().ok()).collect();
        let mut headers = vec![header::AUTHORIZATION, header::CONTENT_TYPE, HeaderName::from_static("x-request-id")];
        for p in PLAYGROUND_PROVIDERS {
            if let Ok(h) = HeaderName::try_from(format!("x-provider-key-{p}")) {
                headers.push(h);
            }
        }
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([Method::GET, Method::POST])
            .allow_headers(headers)
            .expose_headers([header::RETRY_AFTER, HeaderName::from_static("x-request-id")])
            .max_age(std::time::Duration::from_secs(600))
    }

    pub(crate) fn body_limit(&self) -> usize {
        self.cfg.max_file_mb.saturating_mul(1024 * 1024).saturating_add(64 * 1024)
    }

    fn free_owner(user: &str) -> String {
        use sha2::{Digest, Sha256};
        format!("playground:free:{}", &hex(&Sha256::digest(user.as_bytes()))[..32])
    }

    fn byok_owner(&self, provider: &str, key: &str) -> String {
        use hmac::{KeyInit, Mac, SimpleHmac};
        let mut mac =
            <SimpleHmac<sha2::Sha256> as KeyInit>::new_from_slice(&self.owner_secret).expect("any key length");
        mac.update(provider.as_bytes());
        mac.update(&[0]);
        mac.update(key.as_bytes());
        format!("playground:byok:{}", &hex(&mac.finalize().into_bytes())[..32])
    }

    async fn verify_user(&self, token: &str) -> Result<String, ApiError> {
        let auth = self.auth.as_ref().ok_or_else(|| {
            ApiError::unauthorized("this gateway does not accept sign-in tokens (no playground.supabase)")
        })?;
        Ok(auth.verify(token).await?.sub)
    }

    async fn turnstile_ok(&self, token: &str, ip: Option<IpAddr>) -> Result<bool, ApiError> {
        let Some(secret) = self.turnstile_secret.as_deref() else { return Ok(false) };
        let mut body = json!({ "secret": secret, "response": token });
        if let Some(ip) = ip {
            body["remoteip"] = json!(ip.to_string());
        }
        let resp = self.http.post(&self.cfg.turnstile_verify_url).json(&body).send().await.map_err(|e| {
            tracing::warn!(error = %e, "playground: Turnstile verification failed to run");
            unavailable("the human check cannot be verified right now; try again shortly")
        })?;
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        Ok(v.get("success").and_then(Value::as_bool).unwrap_or(false))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unavailable(message: &str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "free_tier_unavailable", message)
        .with_details(json!({ "reason": "unavailable" }))
}

// ---- free-tier counters ----------------------------------------------------------------------

#[derive(Debug, Default)]
struct MemCounters {
    day: String,
    users: HashMap<String, u32>,
    spent_usd: f64,
}

/// Where the free tier's daily counters live: Supabase (`infra/supabase/`, shared by every
/// instance) or this process (one instance only).
enum Quota {
    Memory(Mutex<MemCounters>),
    Supabase { rpc: String, key: String, http: reqwest::Client },
}

impl std::fmt::Debug for Quota {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Quota::Memory(_) => f.write_str("Quota::Memory"),
            Quota::Supabase { rpc, .. } => f.debug_struct("Quota::Supabase").field("rpc", rpc).finish_non_exhaustive(),
        }
    }
}

/// Why a reservation was refused.
#[derive(Debug, Clone, PartialEq)]
enum Refusal {
    Quota { remaining: u32 },
    Budget,
    Paused,
}

fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

impl Quota {
    fn memory() -> Self {
        Quota::Memory(Mutex::new(MemCounters::default()))
    }

    fn new(cfg: &SupabaseConfig, service_key: Option<String>) -> Self {
        match (cfg.url.as_deref(), service_key) {
            (Some(url), Some(key)) => Quota::Supabase {
                rpc: format!("{}/rest/v1/rpc", url.trim_end_matches('/')),
                key,
                http: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new()),
            },
            _ => Quota::memory(),
        }
    }

    fn mem(m: &Mutex<MemCounters>) -> std::sync::MutexGuard<'_, MemCounters> {
        let mut g = m.lock().unwrap_or_else(|p| p.into_inner());
        let day = today();
        if g.day != day {
            *g = MemCounters { day, ..MemCounters::default() };
        }
        g
    }

    async fn rpc(&self, name: &str, args: Value) -> Result<Value, ApiError> {
        let Quota::Supabase { rpc, key, http } = self else { return Ok(Value::Null) };
        let mut req = http.post(format!("{rpc}/{name}")).header("apikey", key.as_str()).json(&args);
        // Legacy service_role keys are JWTs and go in Authorization too; `sb_secret_...` keys must not.
        if crate::auth::looks_like_jwt(key) {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.map_err(|e| {
            tracing::warn!(error = %e, function = name, "playground: Supabase counter call failed");
            unavailable("the free tier's counters are unreachable right now; try again shortly")
        })?;
        if !resp.status().is_success() {
            tracing::warn!(status = %resp.status(), function = name, "playground: Supabase counter call failed");
            return Err(unavailable("the free tier's counters are unavailable right now; try again shortly"));
        }
        let text = resp.text().await.unwrap_or_default();
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Reserve `pages` model-pages for `user` today.
    async fn reserve(&self, user: &str, pages: u32, limit: u32, budget: f64) -> Result<Result<u32, Refusal>, ApiError> {
        if budget <= 0.0 {
            return Ok(Err(Refusal::Paused));
        }
        match self {
            Quota::Memory(m) => {
                let mut g = Self::mem(m);
                if g.spent_usd >= budget {
                    return Ok(Err(Refusal::Budget));
                }
                let used = g.users.get(user).copied().unwrap_or(0);
                if used.saturating_add(pages) > limit {
                    return Ok(Err(Refusal::Quota { remaining: limit.saturating_sub(used) }));
                }
                g.users.insert(user.to_string(), used + pages);
                Ok(Ok(limit - used - pages))
            }
            Quota::Supabase { .. } => {
                let v = self
                    .rpc(
                        "playground_reserve",
                        json!({ "p_user": user, "p_pages": pages, "p_day_limit": limit, "p_budget_usd": budget }),
                    )
                    .await?;
                let row = v.get(0).cloned().unwrap_or(v);
                let remaining = row.get("remaining").and_then(Value::as_u64).unwrap_or(0) as u32;
                Ok(match (row.get("ok").and_then(Value::as_bool), row.get("reason").and_then(Value::as_str)) {
                    (Some(true), _) => Ok(remaining),
                    (_, Some("budget_exhausted")) => Err(Refusal::Budget),
                    (_, Some("paused")) => Err(Refusal::Paused),
                    (Some(false), _) => Err(Refusal::Quota { remaining }),
                    _ => return Err(unavailable("the free tier's counters answered unexpectedly")),
                })
            }
        }
    }

    async fn release(&self, user: &str, pages: u32) {
        if pages == 0 {
            return;
        }
        match self {
            Quota::Memory(m) => {
                let mut g = Self::mem(m);
                if let Some(u) = g.users.get_mut(user) {
                    *u = u.saturating_sub(pages);
                }
            }
            Quota::Supabase { .. } => {
                let _ = self.rpc("playground_release", json!({ "p_user": user, "p_pages": pages })).await;
            }
        }
    }

    async fn charge(&self, usd: f64) {
        if usd.is_nan() || usd <= 0.0 {
            return;
        }
        match self {
            Quota::Memory(m) => Self::mem(m).spent_usd += usd,
            Quota::Supabase { .. } => {
                let _ = self.rpc("playground_charge", json!({ "p_usd": usd })).await;
            }
        }
    }

    /// `(model-pages left for user, free spend today)`.
    async fn status(&self, user: Option<&str>, limit: u32) -> Result<(u32, f64), ApiError> {
        match self {
            Quota::Memory(m) => {
                let g = Self::mem(m);
                let used = user.and_then(|u| g.users.get(u).copied()).unwrap_or(0);
                Ok((limit.saturating_sub(used), g.spent_usd))
            }
            Quota::Supabase { .. } => {
                let nil = uuid::Uuid::nil().to_string();
                let v = self
                    .rpc("playground_status", json!({ "p_user": user.unwrap_or(&nil), "p_day_limit": limit }))
                    .await?;
                let row = v.get(0).cloned().unwrap_or(v);
                let remaining = row.get("remaining").and_then(Value::as_u64).unwrap_or(0) as u32;
                let spent = row.get("spent_usd").and_then(|s| s.as_f64().or_else(|| s.as_str()?.parse().ok()));
                Ok((remaining, spent.unwrap_or(0.0)))
            }
        }
    }
}

// ---- client identity -------------------------------------------------------------------------

/// The client's address: the `trust_forwarded_for`-th entry from the right of `X-Forwarded-For`
/// (each trusted proxy appends the address it saw), else the connection's peer.
fn client_ip(headers: &HeaderMap, peer: Option<PeerAddr>, trusted: usize) -> Option<IpAddr> {
    if trusted > 0 {
        let hops: Vec<&str> = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if hops.len() >= trusted {
            if let Ok(ip) = hops[hops.len() - trusted].parse::<IpAddr>() {
                return Some(ip);
            }
        }
    }
    peer.map(|p| p.0.ip())
}

/// Rate-limit bucket for an address: IPv4 as is, IPv6 by /64 (one subscriber's prefix).
fn ip_bucket(ip: Option<IpAddr>) -> String {
    match ip {
        Some(IpAddr::V4(v4)) => format!("pg-ip:{v4}"),
        Some(IpAddr::V6(v6)) => {
            let s = v6.segments();
            format!("pg-ip:{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        None => "pg-ip:unknown".into(),
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let t = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))?.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// `x-provider-key-<provider>` headers, validated.
fn provider_keys(headers: &HeaderMap) -> Result<HashMap<String, String>, ApiError> {
    let mut out = HashMap::new();
    for (name, value) in headers {
        let Some(provider) = name.as_str().strip_prefix("x-provider-key-") else { continue };
        if !PLAYGROUND_PROVIDERS.contains(&provider) {
            return Err(ApiError::input(format!("'{name}': the playground has no provider '{provider}'")));
        }
        let key = value.to_str().map(str::trim).unwrap_or("");
        if key.is_empty() || key.len() > MAX_PROVIDER_KEY_LEN || !key.chars().all(|c| c.is_ascii_graphic()) {
            return Err(ApiError::input(format!("'{name}' is empty or not a plausible API key")));
        }
        out.insert(provider.to_string(), key.to_string());
    }
    Ok(out)
}

// ---- GET /v1/playground/config ---------------------------------------------------------------

pub(crate) async fn config(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(pg) = state.playground.clone() else { return disabled().into_response() };
    let user = match bearer(&headers) {
        Some(t) if pg.auth.is_some() => match pg.verify_user(&t).await {
            Ok(sub) => Some(sub),
            Err(e) => return e.into_response(),
        },
        _ => None,
    };
    let limit = pg.cfg.free_model_pages_per_day;
    let (free, user_json) = if pg.free_configured() {
        match pg.quota.status(user.as_deref(), limit).await {
            Ok((remaining, spent)) => {
                let reason = (spent >= pg.cfg.free_daily_budget_usd).then_some("budget_exhausted");
                let user_json = user.as_ref().map(|_| json!({ "model_pages_remaining": remaining }));
                (json!({ "available": reason.is_none(), "reason": reason }), user_json)
            }
            Err(_) => (json!({ "available": false, "reason": "paused" }), None),
        }
    } else {
        (json!({ "available": false, "reason": "paused" }), None)
    };
    let mut free = free;
    free["user"] = user_json.unwrap_or(Value::Null);
    let models: Vec<Value> = pg
        .models
        .iter()
        .map(|m| {
            json!({
                "id": m.id, "provider": m.provider, "provider_name": m.provider_name, "model": m.model,
                "price_per_page_usd": m.price, "free_tier": m.free, "byok": true,
            })
        })
        .collect();
    Json(json!({
        "limits": {
            "max_file_bytes": pg.cfg.max_file_mb * 1024 * 1024,
            "accepted_types": ["application/pdf", "image/png", "image/jpeg"],
            "models_per_run": pg.cfg.max_models,
            "pages_per_run": pg.cfg.max_pages,
            "model_pages_per_day": limit,
        },
        "free_tier": free,
        "byok": { "available": true },
        "models": models,
    }))
    .into_response()
}

fn disabled() -> ApiError {
    ApiError::not_found("the playground is not enabled on this gateway ([playground] enabled = true)")
}

// ---- POST /v1/playground/runs ----------------------------------------------------------------

pub(crate) async fn run(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let mut ctx = Ctx::new(&req, "POST", Mode::Parse, "playground_run");
    let result = handle_run(&state, &mut ctx, req).await;
    api::finish(&state, &ctx, result)
}

enum Payer {
    Free { user: String },
    Byok { keys: HashMap<String, String> },
}

struct Upload {
    data: bytes::Bytes,
    models: Vec<String>,
    turnstile: Option<String>,
}

async fn read_form(req: Request) -> Result<Upload, ApiError> {
    let ct = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ct.to_ascii_lowercase().starts_with("multipart/form-data") {
        return Err(ApiError::input("send multipart/form-data with 'file' and 'models'"));
    }
    let mut mp = Multipart::from_request(req, &()).await.map_err(|e| too_large_or(e.status(), e.body_text()))?;
    let bad = |e: axum::extract::multipart::MultipartError| too_large_or(e.status(), e.body_text());
    let mut up = Upload { data: bytes::Bytes::new(), models: Vec::new(), turnstile: None };
    let mut has_file = false;
    while let Some(field) = mp.next_field().await.map_err(bad)? {
        match field.name().unwrap_or("") {
            "file" if !has_file => {
                up.data = field.bytes().await.map_err(bad)?;
                has_file = true;
            }
            "models" => up.models.push(field.text().await.map_err(bad)?.trim().to_string()),
            "turnstile_token" => up.turnstile = Some(field.text().await.map_err(bad)?),
            other => return Err(ApiError::input(format!("unexpected form field '{other}'"))),
        }
    }
    if !has_file || up.data.is_empty() {
        return Err(ApiError::input("the form needs a non-empty 'file'"));
    }
    Ok(up)
}

fn too_large_or(status: StatusCode, text: String) -> ApiError {
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::new(status, "payload_too_large", "the file is larger than the playground accepts")
    } else {
        ApiError::input(text)
    }
}

async fn handle_run(state: &AppState, ctx: &mut Ctx, req: Request) -> Result<Served, ApiError> {
    let pg = state.playground.clone().ok_or_else(disabled)?;
    let peer = req.extensions().get::<PeerAddr>().copied();
    let ip = client_ip(req.headers(), peer, pg.cfg.trust_forwarded_for);
    let token = bearer(req.headers());
    let keys = provider_keys(req.headers())?;
    let payer = match (token, keys.is_empty()) {
        (Some(_), false) => {
            return Err(ApiError::input(
                "send either a sign-in token (free tier) or x-provider-key-* headers (your own keys), not both",
            ))
        }
        (Some(t), true) => Payer::Free { user: pg.verify_user(&t).await? },
        (None, false) => Payer::Byok { keys },
        (None, true) => {
            return Err(ApiError::unauthorized(
                "sign in for the free tier, or send your own provider keys as x-provider-key-<provider>",
            ))
        }
    };
    ctx.key_id = match &payer {
        Payer::Free { user } => Playground::free_owner(user),
        Payer::Byok { .. } => "playground:byok".into(),
    };
    state.usage.check_rate(&ip_bucket(ip), pg.cfg.ip_rpm).map_err(|secs| {
        ApiError::new(StatusCode::TOO_MANY_REQUESTS, "ip_rate_limited", "too many runs from this network")
            .with_retry_after(secs)
    })?;

    let up = read_form(req).await?;
    ctx.requested = Some(up.models.join(","));
    // Models: 1..=max_models, distinct, offered, and on the free tier when it pays.
    if up.models.is_empty() {
        return Err(ApiError::input("pick at least one model ('models')"));
    }
    if up.models.len() > pg.cfg.max_models {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "too_many_models",
            format!("pick at most {} models", pg.cfg.max_models),
        )
        .with_details(json!({ "limit": pg.cfg.max_models })));
    }
    let mut chosen: Vec<PgModel> = Vec::new();
    for m in &up.models {
        let model = pg.model(m).cloned().ok_or_else(|| {
            ApiError::forbidden(format!("'{m}' is not offered on the playground (GET /v1/playground/config)"))
        })?;
        if chosen.iter().any(|c| c.id == model.id) {
            return Err(ApiError::input(format!("'{}' is listed twice", model.id)));
        }
        match &payer {
            Payer::Free { .. } if !model.free => {
                return Err(ApiError::forbidden(format!(
                    "'{}' is not on the free tier (list price above ${}/page); use your own key",
                    model.id, pg.cfg.free_tier_max_price_per_page
                )))
            }
            Payer::Byok { keys } if !keys.contains_key(&model.provider) => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "missing_provider_key",
                    format!("no x-provider-key-{} header for '{}'", model.provider, model.id),
                )
                .with_details(json!({ "provider": model.provider })))
            }
            _ => {}
        }
        chosen.push(model);
    }
    ctx.model = Some("playground".into());

    // The document: type by magic bytes, pages counted here (the quota depends on it).
    let kind = pages::sniff(&up.data).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "only PDF, PNG and JPEG files are accepted",
        )
    })?;
    let page_count = match kind {
        Kind::Pdf => {
            // CPU-bound and allocating: a few at a time, off the async workers.
            let _permit = pg.scans.acquire().await.map_err(|_| scan_failed())?;
            let data = up.data.clone();
            tokio::task::spawn_blocking(move || pages::pdf_pages(&data)).await.map_err(|_| scan_failed())?
        }
        Kind::Png | Kind::Jpeg => Some(1),
    };
    if let Some(n) = page_count.filter(|n| *n > pg.cfg.max_pages) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "too_many_pages",
            format!("the document has {n} pages; the playground takes up to {}", pg.cfg.max_pages),
        )
        .with_details(json!({ "pages": n, "limit": pg.cfg.max_pages })));
    }

    // Free tier: human check, then the user's daily model-pages and the global budget.
    let mut usage = Value::Null;
    let reserved_per_model = page_count.unwrap_or(pg.cfg.max_pages);
    if let Payer::Free { user } = &payer {
        if !pg.free_configured() {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "free_tier_unavailable",
                "the free tier is paused",
            )
            .with_details(json!({ "reason": "paused" })));
        }
        if page_count.is_none() {
            return Err(ApiError::input(
                "the gateway cannot count this PDF's pages, so the free tier cannot take it; use your own keys",
            ));
        }
        let token = up.turnstile.as_deref().map(str::trim).filter(|t| !t.is_empty() && t.len() <= 2048);
        let passed = match token {
            Some(t) => pg.turnstile_ok(t, ip).await?,
            None => false,
        };
        if !passed {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "turnstile_failed",
                "the human check is missing or failed",
            ));
        }
        let need = reserved_per_model.saturating_mul(chosen.len() as u32);
        let limit = pg.cfg.free_model_pages_per_day;
        match pg.quota.reserve(user, need, limit, pg.cfg.free_daily_budget_usd).await? {
            Ok(remaining) => usage = json!({ "model_pages_remaining": remaining }),
            Err(Refusal::Quota { remaining }) => {
                return Err(ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "quota_exceeded",
                    format!("this run needs {need} model-pages and {remaining} are left today"),
                )
                .with_details(json!({ "requested": need, "remaining": remaining, "limit": limit })))
            }
            Err(r) => {
                let reason = if r == Refusal::Budget { "budget_exhausted" } else { "paused" };
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "free_tier_unavailable",
                    "the free tier is unavailable today; samples and your own keys still work",
                )
                .with_details(json!({ "reason": reason })));
            }
        }
    }

    // One job per model, submitted together.
    let submits = chosen.iter().map(|m| {
        let mut d = Deployment { model: m.id.clone(), api_key: None, base_url: None };
        let creds = api::with_credentials(state, &mut d, Mode::Parse);
        if let Payer::Byok { keys } = &payer {
            // Never fall back to the gateway's own key for an own-key run.
            d.api_key = keys.get(&m.provider).cloned();
        }
        let mut doc =
            DocumentRequest::new(DocumentInput::Bytes { data: up.data.clone(), filename: kind.filename().into() });
        doc.model = d.model;
        doc.api_key = d.api_key;
        doc.base_url = d.base_url;
        doc.timeout_secs = state.server.max_timeout_secs.min(SUBMIT_TIMEOUT_SECS);
        doc.max_retries = state.server.max_retries.min(2);
        async move {
            creds?;
            puffinparse_core::submit_parse(doc).await.map_err(ApiError::from)
        }
    });
    let outcomes = futures::future::join_all(submits).await;

    let now = chrono::Utc::now().timestamp();
    let retention = i64::try_from(state.server.job_retention_hours.saturating_mul(3600)).unwrap_or(i64::MAX);
    let mut jobs = Vec::with_capacity(chosen.len());
    let mut failed_pages = 0u32;
    for (m, outcome) in chosen.iter().zip(outcomes) {
        match outcome {
            Ok(handle) => {
                let id = format!("job_{}", uuid::Uuid::new_v4().simple());
                let (owner, user) = match &payer {
                    Payer::Free { user } => (Playground::free_owner(user), Some(user.clone())),
                    Payer::Byok { keys } => (pg.byok_owner(&m.provider, &keys[&m.provider]), None),
                };
                let stored = StoredJob {
                    owner,
                    handle,
                    alias: None,
                    target_index: 0,
                    output_format: None,
                    outcome: None,
                    created_unix: now,
                    playground: Some(PlaygroundJob {
                        user,
                        reserved_pages: if matches!(payer, Payer::Free { .. }) { reserved_per_model } else { 0 },
                    }),
                };
                state.usage.insert_job(&id, stored, retention);
                state.metrics.job_event("submitted");
                jobs.push(json!({ "model": m.id, "id": id, "status": "pending", "error": null }));
            }
            Err(e) => {
                failed_pages += reserved_per_model;
                let e = e.with_request_id(ctx.request_id());
                jobs.push(json!({ "model": m.id, "id": null, "status": "failed", "error": e.error_object() }));
            }
        }
    }
    if let Payer::Free { user } = &payer {
        if failed_pages > 0 {
            pg.quota.release(user, failed_pages).await;
            if let Some(r) = usage.get("model_pages_remaining").and_then(Value::as_u64) {
                usage["model_pages_remaining"] = json!(r + u64::from(failed_pages));
            }
        }
    }
    let mode = if matches!(payer, Payer::Free { .. }) { "free" } else { "byok" };
    Ok(Served {
        body: json!({
            "id": format!("pgrun_{}", uuid::Uuid::new_v4().simple()), "object": "playground_run", "mode": mode,
            "pages": page_count, "jobs": jobs, "usage": usage,
        }),
        model: "playground".into(),
        provider: "playground".into(),
        pages: 0,
        cost_usd: None,
        fallback_index: 0,
        status: StatusCode::ACCEPTED,
        billed: false,
    })
}

// ---- GET /v1/jobs/{id} for playground jobs -------------------------------------------------------

/// Who may read a playground job, and with which provider key.
pub(crate) struct JobAccess {
    /// `key_id` in the request log (never the owner hash of an own-key job).
    pub log_id: String,
    /// Own-key jobs: the caller's key for this job's provider.
    pub provider_key: Option<String>,
}

impl Playground {
    /// The caller must be the job's owner: the same signed-in user (free tier) or the same
    /// provider key (own keys). Anything else is indistinguishable from an unknown job.
    pub(crate) async fn job_access(
        &self,
        state: &AppState,
        headers: &HeaderMap,
        id: &str,
        job: &StoredJob,
    ) -> Result<JobAccess, ApiError> {
        let not_found = || ApiError::not_found(format!("no job '{id}' for this caller"));
        let meta = job.playground.as_ref().ok_or_else(not_found)?;
        let access = if meta.user.is_some() {
            let token = bearer(headers).ok_or_else(not_found)?;
            let owner = Playground::free_owner(&self.verify_user(&token).await?);
            if owner != job.owner {
                return Err(not_found());
            }
            JobAccess { log_id: owner, provider_key: None }
        } else {
            let keys = provider_keys(headers)?;
            let key = keys.get(&job.handle.provider).ok_or_else(not_found)?;
            if self.byok_owner(&job.handle.provider, key) != job.owner {
                return Err(not_found());
            }
            JobAccess { log_id: "playground:byok".into(), provider_key: Some(key.clone()) }
        };
        let bucket = if meta.user.is_some() { access.log_id.clone() } else { job.owner.clone() };
        state.usage.check_rate(&format!("pg-poll:{bucket}"), self.cfg.poll_rpm).map_err(ApiError::rate_limited)?;
        Ok(access)
    }

    /// After a playground job is first seen terminal: give back the model-pages it did not use and
    /// add its cost to the day's free spend. Runs in the background (it may call Supabase).
    pub(crate) fn settled(self: &Arc<Self>, job: &StoredJob, outcome: JobOutcome, pages: u32, cost_usd: Option<f64>) {
        let Some(PlaygroundJob { user: Some(user), reserved_pages }) = job.playground.clone() else { return };
        let price = puffinparse_core::pricing::price_per_page(&job.handle.model, Mode::Parse);
        let pg = Arc::clone(self);
        tokio::spawn(async move {
            match outcome {
                JobOutcome::Succeeded => {
                    pg.quota.release(&user, reserved_pages.saturating_sub(pages)).await;
                    let cost = cost_usd.or_else(|| price.map(|p| p * f64::from(pages)));
                    pg.quota.charge(cost.unwrap_or(0.0)).await;
                }
                JobOutcome::Failed => pg.quota.release(&user, reserved_pages).await,
            }
        });
    }
}

fn scan_failed() -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", "the document could not be checked; try again")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ip_trusts_only_the_configured_hops() {
        let mut h = HeaderMap::new();
        h.append("x-forwarded-for", "6.6.6.6, 203.0.113.9".parse().unwrap());
        let peer = Some(PeerAddr("10.0.0.1:5555".parse().unwrap()));
        assert_eq!(client_ip(&h, peer, 0), Some("10.0.0.1".parse().unwrap()), "spoofable header ignored");
        assert_eq!(client_ip(&h, peer, 1), Some("203.0.113.9".parse().unwrap()));
        assert_eq!(client_ip(&h, peer, 2), Some("6.6.6.6".parse().unwrap()));
        assert_eq!(client_ip(&h, peer, 3), Some("10.0.0.1".parse().unwrap()), "too few hops: peer");
        assert_eq!(client_ip(&HeaderMap::new(), None, 1), None);
    }

    #[test]
    fn ipv6_clients_share_a_bucket_per_64() {
        let a = ip_bucket(Some("2001:db8:1:2:aaaa::1".parse().unwrap()));
        let b = ip_bucket(Some("2001:db8:1:2:bbbb::9".parse().unwrap()));
        let c = ip_bucket(Some("2001:db8:1:3::1".parse().unwrap()));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn provider_key_headers_are_validated() {
        let mut h = HeaderMap::new();
        h.insert("x-provider-key-reducto", "rk-123".parse().unwrap());
        assert_eq!(provider_keys(&h).unwrap()["reducto"], "rk-123");
        h.insert("x-provider-key-tesseract", "x".parse().unwrap());
        assert!(provider_keys(&h).is_err());
        let mut h = HeaderMap::new();
        h.insert("x-provider-key-extend", "has space".parse().unwrap());
        assert!(provider_keys(&h).is_err());
    }

    #[tokio::test]
    async fn memory_counters_reserve_release_and_budget() {
        let q = Quota::memory();
        assert_eq!(q.reserve("u", 9, 30, 1.0).await.unwrap(), Ok(21));
        assert_eq!(q.reserve("u", 22, 30, 1.0).await.unwrap(), Err(Refusal::Quota { remaining: 21 }));
        q.release("u", 4).await;
        assert_eq!(q.status(Some("u"), 30).await.unwrap().0, 25);
        q.charge(1.0).await;
        assert_eq!(q.reserve("v", 1, 30, 1.0).await.unwrap(), Err(Refusal::Budget));
        assert_eq!(q.reserve("v", 1, 30, 0.0).await.unwrap(), Err(Refusal::Paused));
    }
}
