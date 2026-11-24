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
pub fn failure_response(upstream: &Upstream, failure: &UpstreamFailure) -> HttpResponse {
    let key = upstream.api_key.as_ref().map(UpstreamKey::expose);
    let upstream = upstream.name.as_str();
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
            400..=499 => return passed_through(*status, content_type.as_ref(), body, key),
            _ => ApiError::upstream_error(upstream),
        },
    };
    error.error_response()
}

/// The upstream's own 4xx answer, with its key replaced by `[redacted]` in
/// case the upstream echoed it.
fn passed_through(
    status: StatusCode,
    content_type: Option<&HeaderValue>,
    body: &Bytes,
    key: Option<&str>,
) -> HttpResponse {
    let leaks_key = key.is_some_and(|key| {
        !key.is_empty()
            && body
                .windows(key.len())
                .any(|window| window == key.as_bytes())
    });
    let body = if leaks_key {
        Bytes::from(redacted(body, key, usize::MAX))
    } else {
        body.clone()
    };
    match content_type {
        Some(content_type) => HttpResponse::build(status)
            .insert_header((CONTENT_TYPE, content_type.clone()))
            .body(body),
        None => HttpResponse::build(status).body(body),
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

/// The first [`LOGGED_BODY_BYTES`] of `body`, with the upstream's key
/// redacted, as lossy UTF-8 with control characters escaped, so the excerpt
/// stays on one log line and can't fake another.
fn body_excerpt(body: &[u8], key: Option<&str>) -> String {
    let start = redacted(body, key, LOGGED_BODY_BYTES);
    let text = String::from_utf8_lossy(&start[..start.len().min(LOGGED_BODY_BYTES)]);
    escape_controls(&text)
}

/// `body` with every copy of `key` replaced by `[redacted]`, read until the
/// result holds `limit` bytes. Matching while reading means a copy that
/// starts before the limit is always replaced whole, however many came first.
fn redacted(body: &[u8], key: Option<&str>, limit: usize) -> Vec<u8> {
    let key = key.map(str::as_bytes).filter(|key| !key.is_empty());
    let mut out = Vec::with_capacity(body.len().min(limit));
    let mut rest = body;
    while out.len() < limit {
        let Some((&byte, after)) = rest.split_first() else {
            break;
        };
        match key {
            Some(key) if rest.starts_with(key) => {
                out.extend_from_slice(REDACTED.as_bytes());
                rest = &rest[key.len()..];
            }
            _ => {
                out.push(byte);
                rest = after;
            }
        }
    }
    out
}

/// Control characters, line breaks included, written as escapes like `\n`.
fn escape_controls(text: &str) -> String {
    text.chars()
        .fold(String::with_capacity(text.len()), |mut out, c| {
            if c.is_control() {
                out.extend(c.escape_default());
            } else {
                out.push(c);
            }
            out
        })
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
mod tests;
