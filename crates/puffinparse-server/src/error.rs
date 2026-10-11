//! Every failure leaves the gateway as the same JSON body:
//!
//! ```json
//! {"error": {"type": "rate_limit_error", "message": "...", "provider": "reducto",
//!            "provider_status": 429, "job_id": null, "request_id": "..."}}
//! ```

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use puffinparse_core::{Error, ErrorKind};

/// A gateway error. Boxed so `Result<_, ApiError>` stays one pointer wide.
#[derive(Debug, Clone)]
pub struct ApiError(Box<ErrorBody>);

#[derive(Debug, Clone)]
pub struct ErrorBody {
    pub status: StatusCode,
    /// Stable machine-readable type: a core `ErrorKind` string or a gateway type below.
    pub error_type: String,
    pub message: String,
    pub provider: Option<String>,
    pub provider_status: Option<u16>,
    pub job_id: Option<String>,
    pub request_id: Option<String>,
    pub retry_after_secs: Option<u64>,
    /// Machine-readable specifics (`{"limit": 3}`, `{"reason": "paused"}`), when the type has any.
    pub details: Option<serde_json::Value>,
}

impl std::ops::Deref for ApiError {
    type Target = ErrorBody;
    fn deref(&self) -> &ErrorBody {
        &self.0
    }
}

impl std::ops::DerefMut for ApiError {
    fn deref_mut(&mut self) -> &mut ErrorBody {
        &mut self.0
    }
}

impl ApiError {
    pub fn new(status: StatusCode, error_type: &str, message: impl Into<String>) -> Self {
        Self(Box::new(ErrorBody {
            status,
            error_type: error_type.to_string(),
            message: message.into(),
            provider: None,
            provider_status: None,
            job_id: None,
            request_id: None,
            retry_after_secs: None,
            details: None,
        }))
    }

    /// 400 `input_error`: malformed request to the gateway itself.
    pub fn input(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "input_error", message)
    }

    /// 401: missing or unknown bearer token.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    /// 403: the key may not call this model.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "model_not_allowed", message)
    }

    /// 402: the key's monthly budget is spent.
    pub fn budget(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYMENT_REQUIRED, "budget_exceeded", message)
    }

    /// 404: unknown job (or one another key owns), or a disabled endpoint.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    /// 429 from the gateway's own per-key limiter (not the provider's).
    pub fn rate_limited(retry_after_secs: u64) -> Self {
        let mut e = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "key_rate_limited",
            format!("requests-per-minute limit reached for this key; retry in {retry_after_secs}s"),
        );
        e.retry_after_secs = Some(retry_after_secs);
        e
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    pub fn with_request_id(mut self, id: &str) -> Self {
        self.request_id = Some(id.to_string());
        self
    }
}

/// HTTP status for a core error. Provider credential failures are the gateway operator's problem,
/// not the caller's, so they surface as 502 rather than 401 (the body still says
/// `authentication_error` and carries the provider's own status).
pub fn status_for(kind: ErrorKind) -> StatusCode {
    match kind {
        ErrorKind::Input | ErrorKind::BadRequest | ErrorKind::UnsupportedModel => StatusCode::BAD_REQUEST,
        ErrorKind::RateLimit => StatusCode::TOO_MANY_REQUESTS,
        ErrorKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
        ErrorKind::Authentication | ErrorKind::Provider | ErrorKind::Network => StatusCode::BAD_GATEWAY,
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(Box::new(ErrorBody {
            status: status_for(e.kind),
            error_type: e.kind.to_string(),
            message: e.message,
            provider: e.provider,
            provider_status: e.status_code,
            job_id: e.job_id,
            request_id: None,
            retry_after_secs: None,
            details: None,
        }))
    }
}

impl ApiError {
    /// The inner `{type, message, provider, provider_status, job_id, request_id}` object, also
    /// used as the `error` of a failed job in `GET /v1/jobs/{id}`.
    pub fn error_object(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "type": self.error_type,
            "message": self.message,
            "provider": self.provider,
            "provider_status": self.provider_status,
            "job_id": self.job_id,
            "request_id": self.request_id,
        });
        if let Some(d) = &self.details {
            v["details"] = d.clone();
        }
        v
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.error_object() });
        let (status, retry_after_secs) = (self.status, self.retry_after_secs);
        let mut resp = (status, axum::Json(body)).into_response();
        if let Some(secs) = retry_after_secs {
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert(axum::http::header::RETRY_AFTER, v);
            }
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_every_kind() {
        assert_eq!(status_for(ErrorKind::Input), StatusCode::BAD_REQUEST);
        assert_eq!(status_for(ErrorKind::BadRequest), StatusCode::BAD_REQUEST);
        assert_eq!(status_for(ErrorKind::UnsupportedModel), StatusCode::BAD_REQUEST);
        assert_eq!(status_for(ErrorKind::RateLimit), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(status_for(ErrorKind::Timeout), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(status_for(ErrorKind::Authentication), StatusCode::BAD_GATEWAY);
        assert_eq!(status_for(ErrorKind::Provider), StatusCode::BAD_GATEWAY);
        assert_eq!(status_for(ErrorKind::Network), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn keeps_provider_message() {
        let e: ApiError = Error::from_http("reducto", 503, r#"{"detail":"overloaded"}"#).into();
        assert_eq!(e.error_type, "provider_error");
        assert_eq!(e.message, "overloaded");
        assert_eq!(e.provider.as_deref(), Some("reducto"));
        assert_eq!(e.provider_status, Some(503));
    }
}
