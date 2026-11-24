//! Talking to one upstream: the shared HTTP client, the chat request, and
//! what each kind of failure means for the client and for fallback.
//!
//! actix-web uses the `http` 0.2 types and reqwest the 1.x ones, so status
//! codes and header values are converted by their numbers and bytes.

use crate::config::{Config, Upstream, UpstreamKey};
use crate::error::ApiError;
use actix_web::http::header::{HeaderValue, CONTENT_TYPE};
use actix_web::http::StatusCode;
use actix_web::{HttpResponse, ResponseError};
use bytes::Bytes;
use std::error::Error;
use std::fmt;
use std::time::Duration;
use tracing::warn;

/// Bytes of an upstream error body kept in a log line.
const LOGGED_BODY_BYTES: usize = 512;
const REDACTED: &str = "[redacted]";

/// The one client every upstream request goes through, so connections are
/// reused. There is no total timeout: `read_timeout` limits each silence,
/// so a long stream that keeps sending is never cut off.
pub fn build_http_client(config: &Config) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(config.connect_timeout)
        .read_timeout(config.upstream_timeout)
        // A redirect could carry the upstream key to another host.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("ai-gateway/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Why an upstream request didn't produce a successful response.
pub enum UpstreamFailure {
    /// The connection failed; the text is the error chain, without the URL.
    Unreachable(String),
    Timeout,
    /// The upstream answered with a status other than 2xx.
    Status {
        status: StatusCode,
        retry_after: Option<HeaderValue>,
        content_type: Option<HeaderValue>,
        body: Bytes,
    },
}

/// Never shows the body: it may hold fragments of the upstream key.
impl fmt::Display for UpstreamFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpstreamFailure::Unreachable(reason) => write!(f, "could not connect: {reason}"),
            UpstreamFailure::Timeout => f.write_str("timed out"),
            UpstreamFailure::Status { status, .. } => write!(f, "returned HTTP {status}"),
        }
    }
}

/// Like `Display`, the body appears only as its length.
impl fmt::Debug for UpstreamFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpstreamFailure::Unreachable(reason) => {
                f.debug_tuple("Unreachable").field(reason).finish()
            }
            UpstreamFailure::Timeout => f.write_str("Timeout"),
            UpstreamFailure::Status {
                status,
                retry_after,
                content_type,
                body,
            } => f
                .debug_struct("Status")
                .field("status", status)
                .field("retry_after", retry_after)
                .field("content_type", content_type)
                .field("body", &format_args!("<{} bytes>", body.len()))
                .finish(),
        }
    }
}

/// `POST {base_url}/chat/completions` with `body`, sending the upstream's
/// own key if it has one. No client header is forwarded.
pub async fn send_chat(
    client: &reqwest::Client,
    upstream: &Upstream,
    body: Vec<u8>,
) -> Result<reqwest::Response, UpstreamFailure> {
    let request = client
        .post(upstream.chat_url())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    send(authorized(request, upstream), upstream).await
}

/// `GET {base_url}/models`, read in full. `timeout` bounds the whole
/// request, so one slow upstream can't hold up the combined list.
pub async fn send_models(
    client: &reqwest::Client,
    upstream: &Upstream,
    timeout: Duration,
) -> Result<Bytes, UpstreamFailure> {
    let request = authorized(client.get(upstream.models_url()), upstream).timeout(timeout);
    read_body(send(request, upstream).await?).await
}

/// Adds the upstream's own key, if it has one. `bearer_auth` also marks
/// the header as sensitive.
fn authorized(request: reqwest::RequestBuilder, upstream: &Upstream) -> reqwest::RequestBuilder {
    match &upstream.api_key {
        Some(key) => request.bearer_auth(key.expose()),
        None => request,
    }
}

/// Sends `request`; anything but a 2xx answer is a failure.
async fn send(
    request: reqwest::RequestBuilder,
    upstream: &Upstream,
) -> Result<reqwest::Response, UpstreamFailure> {
    let response = request.send().await.map_err(classify)?;
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(status_failure(&upstream.name, response).await)
    }
}

/// The whole body of a successful response.
pub async fn read_body(response: reqwest::Response) -> Result<Bytes, UpstreamFailure> {
    response.bytes().await.map_err(classify)
}

/// Connection errors, timeouts, 5xx and 429 are worth retrying elsewhere.
/// Other statuses are the client's or the operator's mistake.
pub fn should_fallback(failure: &UpstreamFailure) -> bool {
    match failure {
        UpstreamFailure::Unreachable(_) | UpstreamFailure::Timeout => true,
        UpstreamFailure::Status { status, .. } => {
            status.is_server_error() || *status == StatusCode::TOO_MANY_REQUESTS
        }
    }
}

/// What the client gets. Bodies of 401, 403, 429 and 5xx responses are
/// never returned; other 4xx bodies explain the client's mistake and pass
/// through unchanged.
pub fn failure_response(upstream: &str, failure: &UpstreamFailure) -> HttpResponse {
    let error = match failure {
        UpstreamFailure::Unreachable(_) => ApiError::upstream_unavailable(upstream),
        UpstreamFailure::Timeout => ApiError::upstream_timeout(upstream),
        UpstreamFailure::Status {
            status,
            retry_after,
            content_type,
            body,
        } => match status.as_u16() {
            401 | 403 => ApiError::upstream_auth_failed(upstream),
            429 => ApiError::upstream_rate_limited(upstream, retry_after.clone()),
            400..=499 => return passed_through(*status, content_type.as_ref(), body),
            _ => ApiError::upstream_error(upstream),
        },
    };
    error.error_response()
}

fn passed_through(
    status: StatusCode,
    content_type: Option<&HeaderValue>,
    body: &Bytes,
) -> HttpResponse {
    match content_type {
        Some(content_type) => HttpResponse::build(status)
            .insert_header((CONTENT_TYPE, content_type.clone()))
            .body(body.clone()),
        None => HttpResponse::build(status).body(body.clone()),
    }
}

/// For logs: the failure plus the start of any body, with the upstream's
/// key replaced by `[redacted]`.
pub fn describe_failure(upstream: &Upstream, failure: &UpstreamFailure) -> String {
    match failure {
        UpstreamFailure::Status { body, .. } if !body.is_empty() => {
            let key = upstream.api_key.as_ref().map(UpstreamKey::expose);
            format!("{failure}; body: {}", body_excerpt(body, key))
        }
        _ => failure.to_string(),
    }
}

/// The first [`LOGGED_BODY_BYTES`] of `body` as lossy UTF-8, key redacted.
fn body_excerpt(body: &[u8], key: Option<&str>) -> String {
    let key = key.filter(|key| !key.is_empty());
    // Decode a key's length past the limit, so a key that starts before the
    // cut is still found and replaced whole.
    let window = LOGGED_BODY_BYTES + key.map_or(0, str::len);
    let text = String::from_utf8_lossy(&body[..body.len().min(window)]);
    let redacted = match key {
        Some(key) => text.replace(key, REDACTED),
        None => text.into_owned(),
    };
    truncate(&redacted, LOGGED_BODY_BYTES).to_string()
}

/// At most `max` bytes, cut at a character boundary.
fn truncate(text: &str, max: usize) -> &str {
    let end = (0..=max.min(text.len()))
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    &text[..end]
}

fn classify(error: reqwest::Error) -> UpstreamFailure {
    if error.is_timeout() {
        UpstreamFailure::Timeout
    } else {
        UpstreamFailure::Unreachable(error_chain(&error.without_url()))
    }
}

/// `error` and its sources, joined: "error sending request: …: Connection refused".
fn error_chain(error: &(dyn Error + 'static)) -> String {
    // `|&e|` copies the inner reference out, so each source borrows from the
    // original error rather than from the closure's argument.
    std::iter::successors(Some(error), |&e| e.source())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

async fn status_failure(upstream: &str, response: reqwest::Response) -> UpstreamFailure {
    let status = status(&response);
    let retry_after = header(&response, reqwest::header::RETRY_AFTER);
    let content_type = header(&response, reqwest::header::CONTENT_TYPE);
    let body = response.bytes().await.unwrap_or_else(|e| {
        // The status is still the upstream's answer, so keep it.
        warn!(
            upstream,
            status = status.as_u16(),
            error = %error_chain(&e.without_url()),
            "could not read the upstream's error body"
        );
        Bytes::new()
    });
    UpstreamFailure::Status {
        status,
        retry_after,
        content_type,
        body,
    }
}

/// The response's status, converted to actix's `StatusCode`.
pub(super) fn status(response: &reqwest::Response) -> StatusCode {
    // reqwest's StatusCode is always 100..=999, which from_u16 accepts.
    StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY)
}

/// A response header, converted to actix's `HeaderValue`.
pub(super) fn header(
    response: &reqwest::Response,
    name: reqwest::header::HeaderName,
) -> Option<HeaderValue> {
    response
        .headers()
        .get(name)
        .and_then(|value| HeaderValue::from_bytes(value.as_bytes()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::body::to_bytes;
    use actix_web::http::header::{CONTENT_TYPE, RETRY_AFTER};
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const GATEWAY_KEY: &str = "gateway-key-0123456789abcdef0123456789";
    const UPSTREAM_KEY: &str = "sk-upstream-test-0123456789";
    const LEAKY_BODY: &[u8] =
        br#"{"error":{"message":"Incorrect API key provided: sk-proj-****abcd"}}"#;

    /// A config whose only upstream, `mock`, lives at `base_url`.
    fn config(base_url: &str, api_key: Option<&str>) -> Config {
        let vars: Vec<(&str, &str)> = [
            ("GATEWAY_API_KEYS", GATEWAY_KEY),
            ("UPSTREAMS", "mock"),
            ("MOCK_BASE_URL", base_url),
        ]
        .into_iter()
        .chain(api_key.map(|key| ("MOCK_API_KEY", key)))
        .collect();
        let lookup = |name: &str| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        };
        let (config, _) = Config::from_lookup(lookup).expect("the test config is valid");
        config
    }

    fn mock(config: &Config) -> &Upstream {
        config.upstream("mock").expect("configured")
    }

    fn failure(code: u16, retry_after: Option<&'static str>) -> UpstreamFailure {
        UpstreamFailure::Status {
            status: StatusCode::from_u16(code).unwrap(),
            retry_after: retry_after.map(HeaderValue::from_static),
            content_type: Some(HeaderValue::from_static("application/json")),
            body: Bytes::from_static(LEAKY_BODY),
        }
    }

    fn status(code: u16) -> UpstreamFailure {
        failure(code, None)
    }

    /// A URL where nothing listens: the port was free a moment ago.
    fn closed_port_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}/v1")
    }

    #[test]
    fn fallback_triggers_match_the_spec() {
        assert!(should_fallback(&UpstreamFailure::Unreachable(
            "refused".into()
        )));
        assert!(should_fallback(&UpstreamFailure::Timeout));
        for s in [500, 502, 503, 429] {
            assert!(should_fallback(&status(s)), "{s}");
        }
        for s in [400, 401, 403, 404, 413, 422, 302] {
            assert!(!should_fallback(&status(s)), "{s}");
        }
    }

    #[actix_web::test]
    async fn failures_map_to_the_error_table() {
        let cases = [
            (
                UpstreamFailure::Unreachable("refused".into()),
                502,
                "upstream_unavailable",
            ),
            (UpstreamFailure::Timeout, 504, "upstream_timeout"),
            (status(500), 502, "upstream_error"),
            (status(401), 502, "upstream_auth_failed"),
            (status(403), 502, "upstream_auth_failed"),
            (status(302), 502, "upstream_error"),
            (failure(429, Some("20")), 429, "upstream_rate_limited"),
        ];
        for (failure, expected_status, expected_code) in cases {
            let response = failure_response("mock", &failure);
            assert_eq!(response.status().as_u16(), expected_status, "{failure}");
            let retry_after = response.headers().get(RETRY_AFTER).cloned();

            let body = to_bytes(response.into_body()).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["error"]["code"], expected_code, "{failure}");
            assert!(json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("'mock'"));
            let text = String::from_utf8_lossy(&body);
            assert!(!text.contains("sk-proj"), "{failure}: {text}");

            if expected_status == 429 {
                assert_eq!(retry_after.unwrap(), "20");
            }
        }
    }

    #[actix_web::test]
    async fn other_4xx_bodies_pass_through() {
        let body = Bytes::from_static(br#"{"error":{"message":"model 'nope' not found"}}"#);
        let failure = UpstreamFailure::Status {
            status: StatusCode::NOT_FOUND,
            retry_after: None,
            content_type: Some(HeaderValue::from_static("application/json")),
            body: body.clone(),
        };
        let response = failure_response("mock", &failure);
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(to_bytes(response.into_body()).await.unwrap(), body);
    }

    #[test]
    fn described_failures_are_truncated_and_redacted() {
        let config = config("http://127.0.0.1:9/v1", Some(UPSTREAM_KEY));
        let body = format!("{UPSTREAM_KEY}{}", "x".repeat(2000));
        let failure = UpstreamFailure::Status {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            retry_after: None,
            content_type: None,
            body: Bytes::from(body),
        };
        let description = describe_failure(mock(&config), &failure);
        assert!(description.contains("[redacted]"), "{description}");
        assert!(!description.contains(UPSTREAM_KEY), "{description}");
        assert!(description.len() < 700, "{} chars", description.len());
        assert!(!format!("{failure:?}").contains(UPSTREAM_KEY));
    }

    #[actix_web::test]
    async fn send_chat_sends_only_the_upstream_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .mount(&server)
            .await;
        let base_url = format!("{}/v1", server.uri());
        let body = br#"{"model":"m"}"#.to_vec();

        for key in [Some(UPSTREAM_KEY), None] {
            let config = config(&base_url, key);
            let client = build_http_client(&config).unwrap();
            let response = send_chat(&client, mock(&config), body.clone())
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
        }

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].headers["authorization"],
            format!("Bearer {UPSTREAM_KEY}").as_str()
        );
        assert!(requests[1].headers.get("authorization").is_none());
        for request in &requests {
            assert_eq!(request.body, body);
            assert_eq!(request.headers["content-type"], "application/json");
            let agent = request.headers["user-agent"].to_str().unwrap();
            assert!(agent.starts_with("ai-gateway/"), "{agent}");
        }
    }

    #[actix_web::test]
    async fn send_chat_classifies_refused_connections() {
        let config = config(&closed_port_url(), None);
        let client = build_http_client(&config).unwrap();
        let failure = send_chat(&client, mock(&config), b"{}".to_vec())
            .await
            .unwrap_err();
        let UpstreamFailure::Unreachable(message) = &failure else {
            panic!("expected Unreachable, got {failure:?}");
        };
        assert!(message.to_lowercase().contains("refused"), "{message}");
        assert!(!message.contains("http://"), "has the URL: {message}");
    }

    #[actix_web::test]
    async fn send_chat_classifies_timeouts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)))
            .mount(&server)
            .await;
        let config = Config {
            upstream_timeout: Duration::from_millis(200),
            ..config(&format!("{}/v1", server.uri()), None)
        };
        let client = build_http_client(&config).unwrap();
        let failure = send_chat(&client, mock(&config), b"{}".to_vec())
            .await
            .unwrap_err();
        assert!(matches!(failure, UpstreamFailure::Timeout), "{failure:?}");
    }

    #[actix_web::test]
    async fn send_chat_returns_status_failures_with_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(503)
                    .set_body_string("busy")
                    .insert_header("retry-after", "7"),
            )
            .mount(&server)
            .await;
        let config = config(&format!("{}/v1", server.uri()), None);
        let client = build_http_client(&config).unwrap();
        let failure = send_chat(&client, mock(&config), b"{}".to_vec())
            .await
            .unwrap_err();
        let UpstreamFailure::Status {
            status,
            retry_after,
            body,
            ..
        } = &failure
        else {
            panic!("expected Status, got {failure:?}");
        };
        assert_eq!(*status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, "busy");
        assert_eq!(retry_after.as_ref().unwrap(), "7");
    }
}
