//! # puffinparse-server
//!
//! The PuffinParse gateway: one HTTP endpoint in front of every OCR provider, with model aliases and
//! fallbacks, virtual keys with per-key model allow-lists, monthly budgets and rate limits,
//! JSON-lines request logs and Prometheus metrics. See `docs/SERVER.md`.
//!
//! ```no_run
//! # async fn run() -> Result<(), String> {
//! let config = puffinparse_server::Config::load(std::path::Path::new("puffinparse.toml"))?;
//! puffinparse_server::serve(config).await
//! # }
//! ```

#![forbid(unsafe_code)]

mod api;
pub mod config;
pub mod error;
mod jobs;
pub mod log;
pub mod metrics;
pub mod usage;

pub use config::Config;

use config::{resolve_secret, AliasConfig, Target};
use puffinparse_core::{ErrorKind, Strategy};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

/// A provider call target with credentials resolved.
#[derive(Clone)]
pub(crate) struct Deployment {
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

/// Shown in `Debug` instead of a secret.
const REDACTED: &str = "<redacted>";

impl std::fmt::Debug for Deployment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deployment")
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| REDACTED))
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct Alias {
    pub targets: Vec<Deployment>,
    pub strategy: Strategy,
    pub fallback_on: Vec<ErrorKind>,
    pub cursor: AtomicUsize,
}

pub(crate) struct VirtualKey {
    pub id: String,
    pub secret: String,
    pub models: Vec<String>,
    pub monthly_budget_usd: Option<f64>,
    pub rpm: Option<u32>,
}

impl std::fmt::Debug for VirtualKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualKey")
            .field("id", &self.id)
            .field("secret", &REDACTED)
            .field("models", &self.models)
            .field("monthly_budget_usd", &self.monthly_budget_usd)
            .field("rpm", &self.rpm)
            .finish()
    }
}

/// Everything a request handler needs. Built once from a [`Config`]. `Debug` redacts every secret.
pub struct AppState {
    pub(crate) server: config::ServerConfig,
    pub(crate) master_key: Option<String>,
    /// Provider name → (api_key, base_url) from `[providers.*]`.
    pub(crate) providers: BTreeMap<String, (Option<String>, Option<String>)>,
    pub(crate) aliases: BTreeMap<String, Alias>,
    pub(crate) keys: Vec<VirtualKey>,
    /// Shared secret of `POST /v1/webhooks/{provider}`; `None` = the endpoint is off (404).
    pub(crate) webhook_secret: Option<String>,
    pub(crate) usage: usage::UsageStore,
    pub(crate) metrics: metrics::Metrics,
    pub(crate) logger: log::Logger,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let providers: BTreeMap<&str, (Option<&str>, Option<&str>)> = self
            .providers
            .iter()
            .map(|(name, (key, base))| (name.as_str(), (key.as_ref().map(|_| REDACTED), base.as_deref())))
            .collect();
        f.debug_struct("AppState")
            .field("server", &self.server)
            .field("master_key", &self.master_key.as_ref().map(|_| REDACTED))
            .field("providers", &providers)
            .field("aliases", &self.aliases)
            .field("keys", &self.keys)
            .field("webhook_secret", &self.webhook_secret.as_ref().map(|_| REDACTED))
            .finish_non_exhaustive()
    }
}

pub(crate) fn default_fallback_on() -> Vec<ErrorKind> {
    vec![ErrorKind::Provider, ErrorKind::RateLimit, ErrorKind::Timeout, ErrorKind::Network]
}

fn secret(field: &str, value: Option<&String>) -> Result<Option<String>, String> {
    match value {
        None => Ok(None),
        Some(v) => {
            let resolved = resolve_secret(v).map_err(|e| format!("{field}: {e}"))?;
            if resolved.is_none() {
                // Name the variable, never a value (a literal that resolved to nothing is empty).
                eprintln!("warning: {field} = \"{v}\" resolved to nothing (unset or empty)");
            }
            Ok(resolved)
        }
    }
}

impl AppState {
    /// Resolve secrets, open the log sink and load persisted usage.
    pub fn from_config(cfg: Config) -> Result<Self, String> {
        Self::build(cfg, None)
    }

    /// Like [`AppState::from_config`] but with request logging switched off (tests).
    pub fn from_config_quiet(cfg: Config) -> Result<Self, String> {
        Self::build(cfg, Some(log::Logger::disabled()))
    }

    fn build(cfg: Config, logger: Option<log::Logger>) -> Result<Self, String> {
        let master_key = secret("master_key", cfg.master_key.as_ref())?;
        let mut providers = BTreeMap::new();
        for (name, p) in &cfg.providers {
            let key = secret(&format!("providers.{name}.api_key"), p.api_key.as_ref())?;
            providers.insert(name.clone(), (key, p.base_url.clone()));
        }
        let mut aliases = BTreeMap::new();
        for AliasConfig { name, targets, strategy, fallback_on } in &cfg.models {
            let targets = targets
                .iter()
                .map(|t| {
                    Ok(match t {
                        Target::Model(m) => Deployment { model: m.clone(), api_key: None, base_url: None },
                        Target::Detailed { model, api_key, base_url } => Deployment {
                            model: model.clone(),
                            api_key: secret(&format!("models.{name}.api_key"), api_key.as_ref())?,
                            base_url: base_url.clone(),
                        },
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            aliases.insert(
                name.clone(),
                Alias {
                    targets,
                    strategy: *strategy,
                    fallback_on: fallback_on.clone().unwrap_or_else(default_fallback_on),
                    cursor: AtomicUsize::new(0),
                },
            );
        }
        let mut keys = Vec::new();
        for k in &cfg.keys {
            let Some(secret) = secret(&format!("keys.{}.key", k.id), Some(&k.key))? else {
                return Err(format!("keys.{}.key resolved to an empty value", k.id));
            };
            keys.push(VirtualKey {
                id: k.id.clone(),
                secret,
                models: k.models.clone(),
                monthly_budget_usd: k.monthly_budget_usd,
                rpm: k.rpm,
            });
        }
        if cfg.master_key.is_some() && master_key.is_none() {
            return Err("master_key is set but resolved to an empty value".into());
        }
        let webhook_secret = if cfg.webhooks.enabled {
            let s = secret("webhooks.secret", cfg.webhooks.secret.as_ref())?;
            // Fail closed: an enabled receiver must never accept unauthenticated bodies.
            Some(s.ok_or("webhooks.secret resolved to an empty value")?)
        } else {
            None
        };
        let logger = match logger {
            Some(l) => l,
            None => log::Logger::new(cfg.server.log_stdout, cfg.server.log_file.as_deref())?,
        };
        let usage = usage::UsageStore::new(cfg.server.state_file.clone())?;
        Ok(Self {
            server: cfg.server,
            master_key,
            providers,
            aliases,
            keys,
            webhook_secret,
            usage,
            metrics: metrics::Metrics::default(),
            logger,
        })
    }

    /// Whether requests must carry a bearer token.
    pub fn auth_enabled(&self) -> bool {
        self.master_key.is_some() || !self.keys.is_empty()
    }

    /// Refuse an open gateway on a non-loopback address unless the operator said so explicitly.
    pub fn check_exposure(&self) -> Result<(), String> {
        if self.auth_enabled() || self.server.allow_unauthenticated || is_loopback_host(&self.server.host) {
            return Ok(());
        }
        Err(format!(
            "refusing to serve on {} without authentication: set master_key or add [[keys]] (see docs/SERVER.md), \
             bind to 127.0.0.1, or set server.allow_unauthenticated = true if something in front of the gateway \
             already authenticates every caller",
            self.server.host
        ))
    }
}

/// `localhost`, `127.0.0.0/8` and `::1` (with or without brackets).
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The axum application (all routes and layers), for embedding or for tests.
pub fn app(state: Arc<AppState>) -> axum::Router {
    api::router(state)
}

/// Bind `server.host:server.port` and serve until Ctrl-C / SIGTERM.
pub async fn serve(cfg: Config) -> Result<(), String> {
    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    let state = Arc::new(AppState::from_config(cfg)?);
    state.check_exposure()?;
    if !state.auth_enabled() {
        eprintln!("warning: no master_key or [[keys]] configured; the gateway is open to anyone who can reach it");
    }
    // `document_url` downloads in this process follow the gateway's config, not the environment.
    puffinparse_core::fetch::set_process_policy(puffinparse_core::fetch::FetchPolicy {
        allow_private: state.server.allow_private_document_urls,
        max_bytes: state.server.max_download_mb.saturating_mul(1024 * 1024),
    });
    let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| format!("cannot bind {addr}: {e}"))?;
    let local = listener.local_addr().map(|a| a.to_string()).unwrap_or(addr);
    eprintln!(
        "puffinparse gateway listening on http://{local} (auth {}; {} alias(es), {} key(s))",
        if state.auth_enabled() { "on" } else { "OFF" },
        state.aliases.len(),
        state.keys.len()
    );
    let header_timeout = std::time::Duration::from_secs_f64(state.server.header_read_timeout_secs);
    serve_connections(listener, app(state), header_timeout).await
}

/// The accept loop. Hand-written instead of `axum::serve` only to set hyper's header read timeout
/// (a client that trickles its headers would otherwise hold a connection open indefinitely).
async fn serve_connections(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    header_timeout: std::time::Duration,
) -> Result<(), String> {
    use hyper_util::rt::{TokioIo, TokioTimer};
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new()).header_read_timeout(header_timeout);
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut stop = std::pin::pin!(shutdown());
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        // Out of file descriptors and similar: back off instead of spinning.
                        tracing::warn!(error = %e, "gateway: accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let service = hyper_util::service::TowerToHyperService::new(app.clone());
                let conn = graceful.watch(http.serve_connection(TokioIo::new(stream), service));
                tokio::spawn(async move {
                    if let Err(e) = conn.await {
                        tracing::debug!(error = %e, "gateway: connection ended with an error");
                    }
                });
            }
            () = &mut stop => break,
        }
    }
    drop(listener);
    // Let in-flight requests finish, bounded so a stuck connection cannot block shutdown forever.
    tokio::select! {
        () = graceful.shutdown() => {}
        () = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
            eprintln!("puffinparse gateway: connections still open after 30 s; exiting");
        }
    }
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { () = ctrl_c => {}, () = term => {} }
}
