//! `puffinparse.toml`: server settings, provider credentials, model aliases and virtual keys.
//!
//! Secrets (provider keys, virtual keys, the master key) may be written as `"env:VAR"` to read
//! them from the environment at startup; literal values work too but should stay out of VCS.

use puffinparse_core::model::ModelRef;
use puffinparse_core::{ErrorKind, Strategy};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    /// Bearer token with full access (all models, no budget or rate limit). `env:VAR` allowed.
    #[serde(default)]
    pub master_key: Option<String>,
    /// Per-provider credentials and base URLs, keyed by provider name (`reducto`, `llamaparse`, ...).
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Named aliases that route to one or more provider models.
    #[serde(default)]
    pub models: Vec<AliasConfig>,
    /// Virtual keys handed to clients.
    #[serde(default)]
    pub keys: Vec<KeyConfig>,
    /// `POST /v1/webhooks/{provider}`: off (404) unless enabled here.
    #[serde(default)]
    pub webhooks: WebhooksConfig,
    /// `/v1/playground/*` for the website's playground page: off (404) unless enabled here.
    #[serde(default)]
    pub playground: PlaygroundConfig,
}

/// `"env:VAR"` references are names, not secrets, and are shown; literal values are not.
pub(crate) fn redact(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(|v| if v.starts_with("env:") { v } else { "<redacted>" })
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("server", &self.server)
            .field("master_key", &redact(&self.master_key))
            .field("providers", &self.providers)
            .field("models", &self.models)
            .field("keys", &self.keys)
            .field("webhooks", &self.webhooks)
            .field("playground", &self.playground)
            .finish()
    }
}

/// Provider webhook receiver for jobs submitted through `POST /v1/jobs`.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhooksConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Shared secret the provider must send as `?token=` or `x-puffinparse-webhook-secret`.
    /// Required when enabled; `env:VAR` allowed.
    #[serde(default)]
    pub secret: Option<String>,
}

impl std::fmt::Debug for WebhooksConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhooksConfig").field("enabled", &self.enabled).field("secret", &redact(&self.secret)).finish()
    }
}

/// Providers the playground can run: they have a job queue, and a browser can send their key in
/// an `x-provider-key-<provider>` header.
pub const PLAYGROUND_PROVIDERS: &[&str] = &["reducto", "extend", "llamaparse", "opendocrouter"];

/// `[playground]`: the public playground API (docs/SERVER.md, "Playground API").
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaygroundConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Exact browser origins allowed by CORS (`https://puffinparse.com`). Required when enabled.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Registry models offered (`provider/model`, providers in [`PLAYGROUND_PROVIDERS`]).
    #[serde(default)]
    pub models: Vec<String>,
    /// List price (USD per page, parse) at or under which a model is on the free tier.
    #[serde(default = "default_free_max_price")]
    pub free_tier_max_price_per_page: f64,
    #[serde(default = "default_pg_file_mb")]
    pub max_file_mb: usize,
    #[serde(default = "default_pg_pages")]
    pub max_pages: u32,
    #[serde(default = "default_pg_models")]
    pub max_models: usize,
    /// Model-pages (pages x models) one signed-in user may run per UTC day on the free tier.
    #[serde(default = "default_pg_user_pages")]
    pub free_model_pages_per_day: u32,
    /// Free-tier spend cap for all users together per UTC day (USD, list price). 0 = paused.
    #[serde(default)]
    pub free_daily_budget_usd: f64,
    /// Runs per minute from one client IP (both tiers).
    #[serde(default = "default_pg_ip_rpm")]
    pub ip_rpm: u32,
    /// Job status checks per minute for one playground user or key.
    #[serde(default = "default_pg_poll_rpm")]
    pub poll_rpm: u32,
    /// Reverse proxies in front of the gateway that append to `X-Forwarded-For` (Cloud Run: 1).
    /// 0 = use the connection's address.
    #[serde(default)]
    pub trust_forwarded_for: usize,
    /// Cloudflare Turnstile secret. The free tier is unavailable without it. `env:VAR` allowed.
    #[serde(default)]
    pub turnstile_secret: Option<String>,
    /// Turnstile's verification endpoint (overridable for tests).
    #[serde(default = "default_turnstile_url")]
    pub turnstile_verify_url: String,
    /// Supabase project: sign-in for the free tier, and its quota counters.
    #[serde(default)]
    pub supabase: Option<SupabaseConfig>,
}

impl Default for PlaygroundConfig {
    fn default() -> Self {
        toml::from_str("").expect("defaults")
    }
}

impl std::fmt::Debug for PlaygroundConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaygroundConfig")
            .field("enabled", &self.enabled)
            .field("allowed_origins", &self.allowed_origins)
            .field("models", &self.models)
            .field("free_tier_max_price_per_page", &self.free_tier_max_price_per_page)
            .field("max_file_mb", &self.max_file_mb)
            .field("max_pages", &self.max_pages)
            .field("max_models", &self.max_models)
            .field("free_model_pages_per_day", &self.free_model_pages_per_day)
            .field("free_daily_budget_usd", &self.free_daily_budget_usd)
            .field("ip_rpm", &self.ip_rpm)
            .field("poll_rpm", &self.poll_rpm)
            .field("trust_forwarded_for", &self.trust_forwarded_for)
            .field("turnstile_secret", &redact(&self.turnstile_secret))
            .field("turnstile_verify_url", &self.turnstile_verify_url)
            .field("supabase", &self.supabase)
            .finish()
    }
}

fn default_free_max_price() -> f64 {
    0.025
}
fn default_pg_file_mb() -> usize {
    4
}
fn default_pg_pages() -> u32 {
    10
}
fn default_pg_models() -> usize {
    3
}
fn default_pg_user_pages() -> u32 {
    30
}
fn default_pg_ip_rpm() -> u32 {
    20
}
fn default_pg_poll_rpm() -> u32 {
    120
}
fn default_turnstile_url() -> String {
    "https://challenges.cloudflare.com/turnstile/v0/siteverify".into()
}

/// `[playground.supabase]`: verifying the browser's access token, and the free-tier counters.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupabaseConfig {
    /// `https://<project>.supabase.co`: JWKS, issuer and the counters' RPC endpoint derive from it.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub jwks_url: Option<String>,
    #[serde(default)]
    pub issuer: Option<String>,
    #[serde(default = "default_audience")]
    pub audience: Vec<String>,
    #[serde(default = "default_leeway")]
    pub leeway_secs: u64,
    #[serde(default = "default_jwks_cache")]
    pub jwks_cache_secs: u64,
    /// Legacy HS256 projects only. `env:VAR` allowed.
    #[serde(default)]
    pub jwt_secret: Option<String>,
    /// Service-role (or `sb_secret_...`) key for the `playground_*` functions. Unset: the counters
    /// live in this process's memory (one instance only; they reset on restart). `env:VAR` allowed.
    #[serde(default)]
    pub service_key: Option<String>,
}

impl Default for SupabaseConfig {
    fn default() -> Self {
        toml::from_str("").expect("defaults")
    }
}

impl std::fmt::Debug for SupabaseConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupabaseConfig")
            .field("url", &self.url)
            .field("jwks_url", &self.jwks_url)
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .field("leeway_secs", &self.leeway_secs)
            .field("jwks_cache_secs", &self.jwks_cache_secs)
            .field("jwt_secret", &redact(&self.jwt_secret))
            .field("service_key", &redact(&self.service_key))
            .finish()
    }
}

fn default_audience() -> Vec<String> {
    vec!["authenticated".into()]
}
fn default_leeway() -> u64 {
    30
}
fn default_jwks_cache() -> u64 {
    600
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Append JSON-lines request logs to this file (in addition to stdout).
    #[serde(default)]
    pub log_file: Option<PathBuf>,
    /// Print JSON-lines request logs to stdout.
    #[serde(default = "yes")]
    pub log_stdout: bool,
    /// Persist per-key spend and request counts here (JSON), reloaded on startup.
    #[serde(default)]
    pub state_file: Option<PathBuf>,
    /// Largest accepted request body (multipart upload or base64 JSON), in MiB.
    #[serde(default = "default_max_body_mb")]
    pub max_body_mb: usize,
    /// Upper bound for a client-supplied `timeout` (seconds); also the default.
    #[serde(default = "default_timeout")]
    pub max_timeout_secs: f64,
    /// Retries per provider call when the client does not send `max_retries`.
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    /// Allow requests naming a registry model (`reducto/standard`) that is not an alias.
    #[serde(default = "yes")]
    pub allow_direct_models: bool,
    /// How long `POST /v1/jobs` handles are kept for `GET /v1/jobs/{id}` (hours).
    #[serde(default = "default_job_retention")]
    pub job_retention_hours: u64,
    /// Start without a `master_key` or `[[keys]]` even when `host` is not a loopback address.
    #[serde(default)]
    pub allow_unauthenticated: bool,
    /// Let any key call the self-hosted engines (Tesseract, Docling, PaddleOCR, vLLM) by model
    /// name. Off: only through an alias, a key whose `models` names them, or the master key.
    #[serde(default)]
    pub allow_local_engines: bool,
    /// Accept `document_url` for models that download it inside the gateway process (see
    /// `puffinparse_core::fetch::fetches_url_in_process`). Off: those requests get 400, while
    /// providers that fetch the URL themselves (Reducto, Extend, ...) still take URLs.
    #[serde(default)]
    pub fetch_document_urls: bool,
    /// With `fetch_document_urls`: also download from private, loopback and link-local addresses.
    /// Only for a gateway whose callers are all trusted.
    #[serde(default)]
    pub allow_private_document_urls: bool,
    /// Largest document the gateway downloads for `document_url` (MiB).
    #[serde(default = "default_max_download_mb")]
    pub max_download_mb: u64,
    /// Serve `GET /metrics` without a key.
    #[serde(default)]
    pub public_metrics: bool,
    /// Requests processed at once on the document endpoints (`/v1/parse|ocr|extract`, jobs,
    /// webhooks); further requests wait for a slot, within `request_timeout_secs`.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_requests: usize,
    /// Hard limit on one HTTP request, waiting for a slot included (seconds). Default:
    /// `max_timeout_secs` + 60. Must be at least `max_timeout_secs`.
    #[serde(default)]
    pub request_timeout_secs: Option<f64>,
    /// Time a client has to send its request headers (seconds).
    #[serde(default = "default_header_timeout")]
    pub header_read_timeout_secs: f64,
}

/// The largest accepted value for any of the server's timeouts (seconds): a week.
const MAX_TIMEOUT_SECS: f64 = 7.0 * 24.0 * 3600.0;

impl ServerConfig {
    /// The whole-request limit: `request_timeout_secs`, or `max_timeout_secs` + 60.
    pub fn request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.request_timeout_secs.unwrap_or(self.max_timeout_secs + 60.0))
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            log_file: None,
            log_stdout: true,
            state_file: None,
            max_body_mb: default_max_body_mb(),
            max_timeout_secs: default_timeout(),
            max_retries: default_retries(),
            allow_direct_models: true,
            job_retention_hours: default_job_retention(),
            allow_unauthenticated: false,
            allow_local_engines: false,
            fetch_document_urls: false,
            allow_private_document_urls: false,
            max_download_mb: default_max_download_mb(),
            public_metrics: false,
            max_concurrent_requests: default_max_concurrent(),
            request_timeout_secs: None,
            header_read_timeout_secs: default_header_timeout(),
        }
    }
}

fn default_job_retention() -> u64 {
    168
}

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    4000
}
fn yes() -> bool {
    true
}
fn default_max_body_mb() -> usize {
    50
}
fn default_timeout() -> f64 {
    300.0
}
fn default_retries() -> u32 {
    2
}
fn default_max_download_mb() -> u64 {
    puffinparse_core::fetch::DEFAULT_MAX_DOWNLOAD_MB
}
fn default_max_concurrent() -> usize {
    64
}
fn default_header_timeout() -> f64 {
    30.0
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// API key, usually `env:REDUCTO_API_KEY`. Unset → the core reads the provider's default env var.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Base URL override (self-hosted / regional endpoints, or a mock in tests).
    #[serde(default)]
    pub base_url: Option<String>,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("api_key", &redact(&self.api_key))
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasConfig {
    /// What clients send as `model`, e.g. `"invoices"`.
    pub name: String,
    /// Provider models behind the alias, tried per `strategy`.
    pub targets: Vec<Target>,
    #[serde(default)]
    pub strategy: Strategy,
    /// Error kinds that move on to the next target (default: provider, rate_limit, timeout, network).
    #[serde(default)]
    pub fallback_on: Option<Vec<ErrorKind>>,
}

/// One deployment behind an alias: a bare model string, or a table with per-target overrides.
#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub enum Target {
    Model(String),
    Detailed {
        model: String,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        base_url: Option<String>,
    },
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Model(m) => f.debug_tuple("Model").field(m).finish(),
            Target::Detailed { model, api_key, base_url } => f
                .debug_struct("Detailed")
                .field("model", model)
                .field("api_key", &redact(api_key))
                .field("base_url", base_url)
                .finish(),
        }
    }
}

impl Target {
    pub fn model(&self) -> &str {
        match self {
            Target::Model(m) | Target::Detailed { model: m, .. } => m,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyConfig {
    /// Stable, non-secret identifier used in logs, metrics and the state file.
    pub id: String,
    /// The bearer token clients send. `env:VAR` allowed.
    pub key: String,
    /// Aliases or `provider/model` strings this key may call (`provider/*` wildcards allowed).
    /// Empty or absent → every model.
    #[serde(default)]
    pub models: Vec<String>,
    /// Spend cap per calendar month (UTC), in USD of estimated list-price cost.
    #[serde(default)]
    pub monthly_budget_usd: Option<f64>,
    /// Requests per minute (sliding 60 s window).
    #[serde(default)]
    pub rpm: Option<u32>,
}

impl std::fmt::Debug for KeyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyConfig")
            .field("id", &self.id)
            .field("key", &redact(&Some(self.key.clone())))
            .field("models", &self.models)
            .field("monthly_budget_usd", &self.monthly_budget_usd)
            .field("rpm", &self.rpm)
            .finish()
    }
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let cfg: Config = toml::from_str(text).map_err(|e| format!("invalid config: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::from_toml(&text)
    }

    fn validate(&self) -> Result<(), String> {
        let mut names = BTreeSet::new();
        for a in &self.models {
            if a.name.trim().is_empty() {
                return Err("models: alias name must not be empty".into());
            }
            if !names.insert(a.name.as_str()) {
                return Err(format!("models: duplicate alias '{}'", a.name));
            }
            if a.targets.is_empty() {
                return Err(format!("models: alias '{}' has no targets", a.name));
            }
            for t in &a.targets {
                ModelRef::parse(t.model()).map_err(|e| format!("models: alias '{}': {}", a.name, e.message))?;
            }
        }
        for p in self.providers.keys() {
            if puffinparse_core::model::provider_info(p).is_none() {
                return Err(format!("providers: unknown provider '{p}'"));
            }
        }
        let mut ids = BTreeSet::new();
        for k in &self.keys {
            if !ids.insert(k.id.as_str()) {
                return Err(format!("keys: duplicate id '{}'", k.id));
            }
            if k.key.trim().is_empty() {
                return Err(format!("keys: '{}' has an empty key", k.id));
            }
            if k.monthly_budget_usd.is_some_and(|b| b.is_nan() || b < 0.0) {
                return Err(format!("keys: '{}' monthly_budget_usd must be >= 0", k.id));
            }
        }
        let s = &self.server;
        // Every timeout becomes a `Duration`, which panics on inf / NaN / huge values, so each one
        // is checked here: finite, > 0, and at most a week.
        let in_range = |t: f64| t.is_finite() && t > 0.0 && t <= MAX_TIMEOUT_SECS;
        if !in_range(s.max_timeout_secs) || !in_range(s.max_timeout_secs + 60.0) {
            return Err(format!("server.max_timeout_secs must be > 0 and at most {MAX_TIMEOUT_SECS} (a week)"));
        }
        if let Some(t) = s.request_timeout_secs {
            if !in_range(t) {
                return Err(format!("server.request_timeout_secs must be > 0 and at most {MAX_TIMEOUT_SECS} (a week)"));
            }
            if t < s.max_timeout_secs {
                return Err(format!(
                    "server.request_timeout_secs ({t}) must be at least server.max_timeout_secs ({})",
                    s.max_timeout_secs
                ));
            }
        }
        if s.max_concurrent_requests == 0 {
            return Err("server.max_concurrent_requests must be > 0".into());
        }
        if !in_range(s.header_read_timeout_secs) {
            return Err(format!("server.header_read_timeout_secs must be > 0 and at most {MAX_TIMEOUT_SECS} (a week)"));
        }
        if s.max_download_mb == 0 {
            return Err("server.max_download_mb must be > 0".into());
        }
        if self.webhooks.enabled && !self.webhooks.secret.as_deref().is_some_and(|s| !s.trim().is_empty()) {
            return Err("webhooks.enabled requires webhooks.secret (e.g. \"env:PUFFINPARSE_WEBHOOK_SECRET\")".into());
        }
        self.playground.validate()
    }
}

impl PlaygroundConfig {
    fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.allowed_origins.is_empty() {
            return Err(
                "playground.allowed_origins must list the site's origin(s), e.g. [\"https://puffinparse.com\"]".into(),
            );
        }
        for o in &self.allowed_origins {
            let ok =
                (o.starts_with("https://") || o.starts_with("http://localhost") || o.starts_with("http://127.0.0.1"))
                    && !o.ends_with('/')
                    && !o.contains('*')
                    && o.parse::<axum::http::HeaderValue>().is_ok();
            if !ok {
                return Err(format!(
                    "playground.allowed_origins: '{o}' must be an exact https:// origin (no path, no '*')"
                ));
            }
        }
        if self.models.is_empty() {
            return Err("playground.models must list at least one model".into());
        }
        for m in &self.models {
            let r = ModelRef::parse_for(m, puffinparse_core::Mode::Parse)
                .map_err(|e| format!("playground.models: '{m}': {}", e.message))?;
            if !PLAYGROUND_PROVIDERS.contains(&r.provider.as_str()) {
                return Err(format!(
                    "playground.models: '{m}' has no job queue the playground can use (providers: {})",
                    PLAYGROUND_PROVIDERS.join(", ")
                ));
            }
        }
        let positive = [
            ("max_file_mb", self.max_file_mb as f64),
            ("max_pages", f64::from(self.max_pages)),
            ("max_models", self.max_models as f64),
            ("ip_rpm", f64::from(self.ip_rpm)),
            ("poll_rpm", f64::from(self.poll_rpm)),
        ];
        if let Some((name, _)) = positive.iter().find(|(_, v)| *v <= 0.0) {
            return Err(format!("playground.{name} must be > 0"));
        }
        let non_negative = |v: f64| v.is_finite() && v >= 0.0;
        if !non_negative(self.free_daily_budget_usd) || !non_negative(self.free_tier_max_price_per_page) {
            return Err("playground.free_daily_budget_usd and free_tier_max_price_per_page must be >= 0".into());
        }
        if let Some(s) = &self.supabase {
            if s.url.is_none() && (s.issuer.is_none() || (s.jwks_url.is_none() && s.jwt_secret.is_none())) {
                return Err("playground.supabase needs 'url' (or 'issuer' plus 'jwks_url' / 'jwt_secret')".into());
            }
            if s.service_key.is_some() && s.url.is_none() {
                return Err("playground.supabase.service_key needs playground.supabase.url".into());
            }
        }
        Ok(())
    }
}

/// Resolve `env:VAR` references. Returns `Ok(None)` when the variable is unset or empty.
pub fn resolve_secret(value: &str) -> Result<Option<String>, String> {
    match value.strip_prefix("env:") {
        Some(var) => {
            let var = var.trim();
            if var.is_empty() {
                return Err("'env:' reference without a variable name".into());
            }
            Ok(std::env::var(var).ok().filter(|v| !v.trim().is_empty()))
        }
        None => Ok(Some(value.to_string()).filter(|v| !v.trim().is_empty())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample() {
        let cfg = Config::from_toml(
            r#"
            master_key = "env:PUFFINPARSE_MASTER_KEY"
            [server]
            port = 8080
            [providers.reducto]
            api_key = "env:REDUCTO_API_KEY"
            [[models]]
            name = "invoices"
            targets = ["reducto/standard", { model = "extend/parse_performance", base_url = "http://x" }]
            strategy = "round_robin"
            [[keys]]
            id = "team-a"
            key = "sk-a"
            models = ["invoices", "llamaparse/*"]
            monthly_budget_usd = 10.0
            rpm = 60
            "#,
        )
        .unwrap();
        assert_eq!(cfg.server.port, 8080);
        assert_eq!(cfg.models[0].strategy, Strategy::RoundRobin);
        assert_eq!(cfg.models[0].targets[1].model(), "extend/parse_performance");
        assert_eq!(cfg.keys[0].rpm, Some(60));
    }

    #[test]
    fn example_config_is_valid() {
        let cfg = Config::from_toml(include_str!("../../../examples/server/puffinparse.toml")).unwrap();
        assert_eq!(cfg.models.len(), 3);
        assert_eq!(cfg.keys.len(), 2);
    }

    #[test]
    fn cloud_run_playground_config_is_valid() {
        let cfg = Config::from_toml(include_str!("../../../deploy/cloudrun/puffinparse.toml")).unwrap();
        assert!(cfg.playground.enabled && !cfg.server.allow_direct_models);
        assert!(cfg.server.state_file.is_none());
    }

    #[test]
    fn rejects_bad_configs() {
        assert!(Config::from_toml("[[models]]\nname='a'\ntargets=['nope/x']").is_err());
        assert!(Config::from_toml("[[models]]\nname='a'\ntargets=[]").is_err());
        assert!(Config::from_toml("[providers.nope]\napi_key='x'").is_err());
        assert!(Config::from_toml("[[keys]]\nid='a'\nkey='x'\n[[keys]]\nid='a'\nkey='y'").is_err());
        assert!(Config::from_toml("unknown = 1").is_err());
        assert!(Config::from_toml("[webhooks]\nenabled = true").is_err());
        assert!(Config::from_toml("[webhooks]\nenabled = true\nsecret = 'env:X'").is_ok());
    }

    #[test]
    fn new_server_settings_default_to_safe_values() {
        let s = Config::from_toml("").unwrap().server;
        assert!(!s.allow_unauthenticated && !s.allow_local_engines && !s.fetch_document_urls);
        assert!(!s.allow_private_document_urls && !s.public_metrics);
        assert_eq!(s.max_download_mb, 50);
        assert_eq!(s.max_concurrent_requests, 64);
        assert_eq!(s.request_timeout(), std::time::Duration::from_secs(360));
        assert!(Config::from_toml("[server]\nmax_timeout_secs = 300\nrequest_timeout_secs = 100").is_err());
        assert!(Config::from_toml("[server]\nmax_concurrent_requests = 0").is_err());
        assert!(Config::from_toml("[server]\nrequest_timeout_secs = 900").is_ok());
        // Values a `Duration` cannot hold are startup errors, not panics.
        for bad in [
            "max_timeout_secs = inf",
            "max_timeout_secs = 1e300",
            "request_timeout_secs = inf",
            "request_timeout_secs = 1e300",
            "header_read_timeout_secs = inf",
            "header_read_timeout_secs = nan",
            "header_read_timeout_secs = -1",
        ] {
            assert!(Config::from_toml(&format!("[server]\n{bad}")).is_err(), "{bad}");
        }
        assert!(Config::from_toml("[server]\nmax_timeout_secs = 3600\nrequest_timeout_secs = 604800").is_ok());
    }

    #[test]
    fn config_debug_hides_literal_secrets() {
        let cfg = Config::from_toml(
            r#"
            master_key = "sk-master-LITERAL"
            [providers.reducto]
            api_key = "rd-LITERAL"
            [[models]]
            name = "a"
            targets = [{ model = "reducto/standard", api_key = "tgt-LITERAL" }]
            [[keys]]
            id = "team"
            key = "sk-team-LITERAL"
            [webhooks]
            enabled = true
            secret = "hook-LITERAL"
            "#,
        )
        .unwrap();
        let dbg = format!("{cfg:?} {cfg:#?}");
        assert!(!dbg.contains("LITERAL"), "{dbg}");
        assert!(format!("{:?}", Config::from_toml("master_key = 'env:PP_MASTER'").unwrap()).contains("env:PP_MASTER"));
    }

    #[test]
    fn resolves_env_secrets() {
        std::env::set_var("PUFFINPARSE_TEST_SECRET_XYZ", "s3cret");
        assert_eq!(resolve_secret("env:PUFFINPARSE_TEST_SECRET_XYZ").unwrap().as_deref(), Some("s3cret"));
        assert_eq!(resolve_secret("env:PUFFINPARSE_TEST_UNSET_XYZ").unwrap(), None);
        assert_eq!(resolve_secret("literal").unwrap().as_deref(), Some("literal"));
        assert!(resolve_secret("env:").is_err());
    }
}
