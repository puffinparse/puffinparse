//! Supabase Auth: verify the access token (a JWT) a signed-in browser sends as
//! `Authorization: Bearer <jwt>` and return its `sub` (the playground owns jobs by a hash of it).
//!
//! Keys come from the project's JWKS endpoint (asymmetric signing keys: ES256 / RS256 / EdDSA),
//! cached for `jwks_cache_secs` and refetched early when a token names a `kid` the cache does not
//! know (key rotation), at most once per [`MISS_REFETCH_SECS`]. Projects still on the legacy
//! shared secret can set `jwt_secret` to accept HS256 as well. `exp`, `aud` and `iss` are always
//! checked; anonymous Supabase sessions (`is_anonymous: true`) are refused, because the playground
//! is for signed-in users only.
//!
//! Supabase JWTs only ever authorise the playground (`/v1/playground/*`) and reading the user's
//! own jobs (`GET /v1/jobs/{id}`); every other endpoint still needs the master key or a virtual key.

use crate::config::SupabaseConfig;
use crate::error::ApiError;
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Minimum spacing of JWKS refetches triggered by an unknown `kid` (protects the JWKS endpoint
/// from a flood of tokens with made-up key ids).
pub const MISS_REFETCH_SECS: u64 = 30;

/// Asymmetric algorithms accepted from the JWKS. `none` and anything symmetric are never taken
/// from a token header; HS256 is only accepted with a configured `jwt_secret`.
const ASYMMETRIC: &[Algorithm] =
    &[Algorithm::ES256, Algorithm::ES384, Algorithm::RS256, Algorithm::RS384, Algorithm::RS512, Algorithm::EdDSA];

/// The claims the gateway reads. Everything else in the token is ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(default)]
    pub is_anonymous: Option<bool>,
}

#[derive(Debug, Default)]
struct Cache {
    keys: Vec<(Option<String>, Jwk)>,
    fetched: Option<Instant>,
    last_attempt: Option<Instant>,
}

#[derive(Debug)]
pub struct SupabaseAuth {
    jwks_url: Option<String>,
    issuer: String,
    audience: Vec<String>,
    hs_secret: Option<String>,
    leeway_secs: u64,
    cache_ttl: Duration,
    http: reqwest::Client,
    cache: Mutex<Cache>,
}

/// `true` for a compact JWS (`header.payload.signature`, base64url header starting `eyJ`).
/// The 401 message for a token that is malformed or fails a check the user cannot act on.
const INVALID_TOKEN: &str = "invalid access token";

pub fn looks_like_jwt(token: &str) -> bool {
    token.starts_with("eyJ") && token.split('.').count() == 3
}

impl SupabaseAuth {
    pub fn new(cfg: &SupabaseConfig, hs_secret: Option<String>) -> Result<Self, String> {
        let base = cfg.url.as_deref().map(|u| u.trim_end_matches('/').to_string());
        let jwks_url =
            cfg.jwks_url.clone().or_else(|| base.as_ref().map(|b| format!("{b}/auth/v1/.well-known/jwks.json")));
        let issuer = cfg
            .issuer
            .clone()
            .or_else(|| base.as_ref().map(|b| format!("{b}/auth/v1")))
            .ok_or("auth.supabase: set 'url' (or 'issuer')")?;
        if jwks_url.is_none() && hs_secret.is_none() {
            return Err("auth.supabase: set 'url' / 'jwks_url' (JWKS) or 'jwt_secret' (HS256)".into());
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("auth.supabase: cannot build HTTP client: {e}"))?;
        Ok(Self {
            jwks_url,
            issuer,
            audience: cfg.audience.clone(),
            hs_secret,
            leeway_secs: cfg.leeway_secs,
            cache_ttl: Duration::from_secs(cfg.jwks_cache_secs),
            http,
            cache: Mutex::new(Cache::default()),
        })
    }

    /// Verify `token` and return its claims. Every failure is a 401. Cases a user can act on
    /// (expired, wrong audience or project, unknown key) say so; anything else is a plain
    /// "invalid access token", with the library's detail logged at debug level only.
    pub async fn verify(&self, token: &str) -> Result<Claims, ApiError> {
        let header = jsonwebtoken::decode_header(token).map_err(|e| {
            tracing::debug!(error = %e, "puffinparse-server: undecodable access token header");
            ApiError::unauthorized(INVALID_TOKEN)
        })?;
        let key = if header.alg == Algorithm::HS256 {
            let secret = self.hs_secret.as_deref().ok_or_else(|| {
                ApiError::unauthorized(
                    "HS256 access tokens are not accepted by this gateway (no auth.supabase.jwt_secret)",
                )
            })?;
            DecodingKey::from_secret(secret.as_bytes())
        } else if ASYMMETRIC.contains(&header.alg) {
            self.key_for(header.kid.as_deref(), header.alg).await?
        } else {
            tracing::debug!(alg = ?header.alg, "puffinparse-server: access token algorithm not accepted");
            return Err(ApiError::unauthorized(INVALID_TOKEN));
        };
        let mut v = Validation::new(header.alg);
        v.set_audience(&self.audience);
        v.set_issuer(&[&self.issuer]);
        v.set_required_spec_claims(&["exp", "sub", "aud", "iss"]);
        v.leeway = self.leeway_secs;
        let data = jsonwebtoken::decode::<Claims>(token, &key, &v).map_err(|e| {
            use jsonwebtoken::errors::ErrorKind as K;
            let why = match e.kind() {
                K::ExpiredSignature => "access token expired; sign in again".to_string(),
                K::InvalidAudience => "access token has the wrong audience".to_string(),
                K::InvalidIssuer => "access token was issued by a different Supabase project".to_string(),
                K::InvalidSignature => "access token signature is invalid".to_string(),
                _ => {
                    tracing::debug!(error = %e, "puffinparse-server: access token rejected");
                    INVALID_TOKEN.to_string()
                }
            };
            ApiError::unauthorized(why)
        })?;
        let claims = data.claims;
        if claims.sub.trim().is_empty() {
            return Err(ApiError::unauthorized("access token has an empty 'sub'"));
        }
        if claims.is_anonymous == Some(true) {
            return Err(ApiError::unauthorized("anonymous sessions cannot use the playground; sign in first"));
        }
        Ok(claims)
    }

    async fn key_for(&self, kid: Option<&str>, alg: Algorithm) -> Result<DecodingKey, ApiError> {
        let Some(url) = self.jwks_url.as_deref() else {
            return Err(ApiError::unauthorized("asymmetric access tokens need auth.supabase.url or jwks_url"));
        };
        let mut cache = self.cache.lock().await;
        let fresh = cache.fetched.is_some_and(|t| t.elapsed() < self.cache_ttl);
        let known = find(&cache.keys, kid, alg).is_some();
        // Refetch when stale or when the kid is unknown (rotation), but at most once per
        // MISS_REFETCH_SECS: during a JWKS outage the stale keys keep serving instead of every
        // request queueing behind a fetch that times out.
        let may_retry = cache.last_attempt.is_none_or(|t| t.elapsed() >= Duration::from_secs(MISS_REFETCH_SECS));
        if (!fresh || !known) && may_retry {
            cache.last_attempt = Some(Instant::now());
            match self.fetch(url).await {
                Ok(keys) => {
                    cache.keys = keys;
                    cache.fetched = Some(Instant::now());
                }
                // Keep serving the cached keys if the JWKS endpoint has a blip.
                Err(e) => tracing::warn!(error = %e, "puffinparse-server: cannot refresh the Supabase JWKS"),
            }
        }
        let jwk = find(&cache.keys, kid, alg).ok_or_else(|| {
            if cache.fetched.is_none() {
                ApiError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "auth_unavailable",
                    "cannot load the Supabase signing keys right now; try again shortly",
                )
            } else {
                ApiError::unauthorized("access token is signed with an unknown key")
            }
        })?;
        DecodingKey::from_jwk(jwk).map_err(|e| {
            tracing::warn!(error = %e, "puffinparse-server: a Supabase signing key cannot be used");
            ApiError::unauthorized(INVALID_TOKEN)
        })
    }

    async fn fetch(&self, url: &str) -> Result<Vec<(Option<String>, Jwk)>, String> {
        let resp = self.http.get(url).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("JWKS endpoint answered HTTP {}", resp.status()));
        }
        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        // Parse key by key so one unsupported entry does not hide the others.
        let keys = body
            .get("keys")
            .and_then(Value::as_array)
            .ok_or("JWKS has no 'keys' array")?
            .iter()
            .filter_map(|k| serde_json::from_value::<Jwk>(k.clone()).ok())
            .map(|k| (k.common.key_id.clone(), k))
            .collect();
        Ok(keys)
    }
}

/// The key with this `kid`; without a `kid`, the only key whose declared algorithm (if any)
/// matches.
fn find<'a>(keys: &'a [(Option<String>, Jwk)], kid: Option<&str>, alg: Algorithm) -> Option<&'a Jwk> {
    match kid {
        Some(kid) => keys.iter().find(|(k, _)| k.as_deref() == Some(kid)).map(|(_, j)| j),
        None => {
            let matching: Vec<&Jwk> = keys
                .iter()
                .map(|(_, j)| j)
                .filter(|j| j.common.key_algorithm.is_none_or(|ka| format!("{ka:?}") == format!("{alg:?}")))
                .collect();
            (matching.len() == 1).then(|| matching[0])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_jwts() {
        assert!(looks_like_jwt("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.sig"));
        assert!(!looks_like_jwt("sk-team-a"));
        assert!(!looks_like_jwt("eyJ.only-two"));
    }

    #[test]
    fn needs_a_key_source() {
        let cfg = SupabaseConfig { url: None, issuer: Some("x".into()), ..SupabaseConfig::default() };
        assert!(SupabaseAuth::new(&cfg, None).is_err());
        assert!(SupabaseAuth::new(&cfg, Some("s".into())).is_ok());
        let cfg = SupabaseConfig { url: Some("https://abc.supabase.co/".into()), ..SupabaseConfig::default() };
        let a = SupabaseAuth::new(&cfg, None).unwrap();
        assert_eq!(a.issuer, "https://abc.supabase.co/auth/v1");
        assert_eq!(a.jwks_url.as_deref(), Some("https://abc.supabase.co/auth/v1/.well-known/jwks.json"));
    }

    #[tokio::test]
    async fn malformed_tokens_get_a_generic_message() {
        let cfg = SupabaseConfig { issuer: Some("x".into()), ..SupabaseConfig::default() };
        let a = SupabaseAuth::new(&cfg, Some("s".into())).unwrap();
        for token in ["eyJnot-json.e30.sig", "eyJhbGciOiJub25lIn0.e30.", "eyJhbGciOiJQUzI1NiJ9.e30.sig"] {
            let e = a.verify(token).await.unwrap_err();
            assert_eq!(e.message, INVALID_TOKEN, "{token}");
        }
    }

    /// While the JWKS endpoint is down, a stale cache is refetched at most once per
    /// MISS_REFETCH_SECS, not on every request.
    #[tokio::test]
    async fn jwks_outage_does_not_refetch_on_every_request() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = axum::Router::new().route(
            "/jwks",
            axum::routing::get(move || {
                h.fetch_add(1, Ordering::SeqCst);
                async { axum::http::StatusCode::SERVICE_UNAVAILABLE }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cfg = SupabaseConfig {
            issuer: Some("x".into()),
            jwks_url: Some(format!("http://{addr}/jwks")),
            jwks_cache_secs: 0,
            ..SupabaseConfig::default()
        };
        let a = SupabaseAuth::new(&cfg, None).unwrap();
        for _ in 0..5 {
            assert!(a.key_for(Some("k1"), Algorithm::ES256).await.is_err());
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
