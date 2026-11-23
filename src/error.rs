//! Errors the gateway itself produces, in OpenAI's error format:
//! `{"error": {"message": …, "type": …, "param": null, "code": …}}`.
//!
//! Stock OpenAI SDKs parse this shape, so clients see a normal API error.

use actix_web::http::header::{HeaderValue, RETRY_AFTER};
use actix_web::http::StatusCode;
use actix_web::{HttpResponse, ResponseError};
use serde_json::json;

const INVALID_REQUEST: &str = "invalid_request_error";
const RATE_LIMIT: &str = "rate_limit_error";
const SERVER_ERROR: &str = "server_error";

/// An error response. Build one with the constructors; return it from a
/// handler and actix renders it through [`ResponseError`].
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    status: StatusCode,
    error_type: &'static str,
    code: Option<&'static str>,
    message: String,
    retry_after: Option<HeaderValue>,
}

impl ApiError {
    fn new(
        status: StatusCode,
        error_type: &'static str,
        code: Option<&'static str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            error_type,
            code,
            message: message.into(),
            retry_after: None,
        }
    }

    fn with_retry_after(self, retry_after: Option<HeaderValue>) -> Self {
        Self {
            retry_after,
            ..self
        }
    }

    pub fn invalid_api_key() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            INVALID_REQUEST,
            Some("invalid_api_key"),
            "Missing or invalid API key. Send it as `Authorization: Bearer <key>`.",
        )
    }

    pub fn rate_limited(retry_after_secs: u64) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            RATE_LIMIT,
            Some("rate_limit_exceeded"),
            format!("Rate limit exceeded. Try again in {retry_after_secs} seconds."),
        )
        .with_retry_after(Some(HeaderValue::from(retry_after_secs)))
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, INVALID_REQUEST, None, message)
    }

    pub fn payload_too_large(limit_bytes: usize) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            INVALID_REQUEST,
            Some("payload_too_large"),
            format!("The request body is larger than the gateway's limit of {limit_bytes} bytes."),
        )
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            INVALID_REQUEST,
            Some("not_found"),
            message,
        )
    }

    pub fn upstream_unavailable(upstream: &str) -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            SERVER_ERROR,
            Some("upstream_unavailable"),
            format!("Upstream '{upstream}' could not be reached."),
        )
    }

    pub fn upstream_timeout(upstream: &str) -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            SERVER_ERROR,
            Some("upstream_timeout"),
            format!("Upstream '{upstream}' did not respond in time."),
        )
    }

    pub fn upstream_error(upstream: &str) -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            SERVER_ERROR,
            Some("upstream_error"),
            format!("Upstream '{upstream}' returned an error."),
        )
    }

    /// Keeps the upstream's `Retry-After`, if it sent one, so clients back off
    /// for as long as the upstream asked.
    pub fn upstream_rate_limited(upstream: &str, retry_after: Option<HeaderValue>) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            RATE_LIMIT,
            Some("upstream_rate_limited"),
            format!("Upstream '{upstream}' is rate limiting requests."),
        )
        .with_retry_after(retry_after)
    }

    pub fn upstream_auth_failed(upstream: &str) -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            SERVER_ERROR,
            Some("upstream_auth_failed"),
            format!("The gateway could not authenticate with upstream '{upstream}'."),
        )
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        self.status
    }

    fn error_response(&self) -> HttpResponse {
        let mut response = HttpResponse::build(self.status);
        if let Some(retry_after) = &self.retry_after {
            response.insert_header((RETRY_AFTER, retry_after.clone()));
        }
        response.json(json!({
            "error": {
                "message": self.message,
                "type": self.error_type,
                "param": null,
                "code": self.code,
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::body::to_bytes;
    use actix_web::http::header::HeaderMap;
    use serde_json::Value;

    async fn render(error: ApiError) -> (u16, HeaderMap, Value) {
        let response = error.error_response();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body())
            .await
            .expect("an in-memory body can be read");
        let body = serde_json::from_slice(&bytes).expect("the body is JSON");
        (status, headers, body)
    }

    #[actix_web::test]
    async fn bad_request_uses_the_openai_envelope() {
        let (status, headers, body) = render(ApiError::bad_request("`model` is required")).await;
        assert_eq!(status, 400);
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(
            body,
            json!({"error": {"message": "`model` is required",
                "type": "invalid_request_error", "param": null, "code": null}})
        );
    }

    #[actix_web::test]
    async fn rate_limited_sets_retry_after() {
        let (status, headers, body) = render(ApiError::rate_limited(7)).await;
        assert_eq!(status, 429);
        assert_eq!(headers.get("retry-after").unwrap(), "7");
        assert_eq!(body["error"]["code"], "rate_limit_exceeded");
    }

    #[actix_web::test]
    async fn constructors_match_the_table() {
        let upstream_retry = Some(HeaderValue::from_static("12"));
        // (error, status, type, code, Retry-After)
        let table = [
            (
                ApiError::invalid_api_key(),
                401,
                "invalid_request_error",
                json!("invalid_api_key"),
                None,
            ),
            (
                ApiError::rate_limited(1),
                429,
                "rate_limit_error",
                json!("rate_limit_exceeded"),
                Some("1"),
            ),
            (
                ApiError::bad_request("bad"),
                400,
                "invalid_request_error",
                Value::Null,
                None,
            ),
            (
                ApiError::payload_too_large(1024),
                413,
                "invalid_request_error",
                json!("payload_too_large"),
                None,
            ),
            (
                ApiError::not_found("gone"),
                404,
                "invalid_request_error",
                json!("not_found"),
                None,
            ),
            (
                ApiError::upstream_unavailable("ollama"),
                502,
                "server_error",
                json!("upstream_unavailable"),
                None,
            ),
            (
                ApiError::upstream_timeout("ollama"),
                504,
                "server_error",
                json!("upstream_timeout"),
                None,
            ),
            (
                ApiError::upstream_error("ollama"),
                502,
                "server_error",
                json!("upstream_error"),
                None,
            ),
            (
                ApiError::upstream_rate_limited("ollama", upstream_retry),
                429,
                "rate_limit_error",
                json!("upstream_rate_limited"),
                Some("12"),
            ),
            (
                ApiError::upstream_rate_limited("ollama", None),
                429,
                "rate_limit_error",
                json!("upstream_rate_limited"),
                None,
            ),
            (
                ApiError::upstream_auth_failed("ollama"),
                502,
                "server_error",
                json!("upstream_auth_failed"),
                None,
            ),
        ];

        for (error, want_status, want_type, want_code, want_retry) in table {
            let (status, headers, body) = render(error).await;
            let row = format!("{want_code} ({want_status})");
            assert_eq!(status, want_status, "{row}");
            assert_eq!(body["error"]["type"], want_type, "{row}");
            assert_eq!(body["error"]["code"], want_code, "{row}");
            assert_eq!(body["error"]["param"], Value::Null, "{row}");
            assert_eq!(
                headers.get("retry-after").map(|v| v.to_str().unwrap()),
                want_retry,
                "{row}"
            );
            let message = body["error"]["message"].as_str().unwrap();
            assert!(!message.is_empty(), "{row}");
            if want_code
                .as_str()
                .is_some_and(|c| c.starts_with("upstream_"))
            {
                assert!(message.contains("ollama"), "{row}: {message}");
            }
        }
    }
}
