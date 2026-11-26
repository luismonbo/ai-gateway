//! Talking to one upstream: the shared HTTP client, the chat request, and
//! what each kind of failure means for the client and for fallback.
//!
//! actix-web uses the `http` 0.2 types and reqwest the 1.x ones, so status
//! codes and header values are converted by their numbers and bytes.

use crate::config::{Config, Upstream};
use crate::error::ApiError;
use crate::sign_in::Bearer;
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
        /// Already scrubbed of the secret the request carried.
        body: Bytes,
    },
    /// No Azure token could be had, so nothing was sent; the text is why.
    SignIn(String),
}

/// Never shows the body: it may hold fragments of the upstream key.
impl fmt::Display for UpstreamFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpstreamFailure::Unreachable(reason) => write!(f, "could not connect: {reason}"),
            UpstreamFailure::Timeout => f.write_str("timed out"),
            UpstreamFailure::Status { status, .. } => write!(f, "returned HTTP {status}"),
            UpstreamFailure::SignIn(reason) => write!(f, "could not sign in: {reason}"),
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
            UpstreamFailure::SignIn(reason) => f.debug_tuple("SignIn").field(reason).finish(),
        }
    }
}

/// `POST {base_url}/chat/completions` with `body`, sending `bearer`: the
/// upstream's own key or its Azure token. No client header is forwarded.
pub async fn send_chat(
    client: &reqwest::Client,
    upstream: &Upstream,
    bearer: Option<&Bearer>,
    body: Vec<u8>,
) -> Result<reqwest::Response, UpstreamFailure> {
    let request = client
        .post(upstream.chat_url())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    send(authorized(request, bearer), upstream, bearer).await
}

/// `GET {base_url}/models`, read in full, sending `bearer`. `timeout`
/// bounds the whole request, so one slow upstream can't hold up the
/// combined list.
pub async fn send_models(
    client: &reqwest::Client,
    upstream: &Upstream,
    bearer: Option<&Bearer>,
    timeout: Duration,
) -> Result<Bytes, UpstreamFailure> {
    let request = authorized(client.get(upstream.models_url()), bearer).timeout(timeout);
    read_body(send(request, upstream, bearer).await?).await
}

/// Adds the bearer, if there is one. `bearer_auth` also marks the header
/// as sensitive.
fn authorized(
    request: reqwest::RequestBuilder,
    bearer: Option<&Bearer>,
) -> reqwest::RequestBuilder {
    match bearer {
        Some(bearer) => request.bearer_auth(bearer.expose()),
        None => request,
    }
}

/// Sends `request`; anything but a 2xx answer is a failure, whose body is
/// scrubbed of `bearer` before anything else sees it.
async fn send(
    request: reqwest::RequestBuilder,
    upstream: &Upstream,
    bearer: Option<&Bearer>,
) -> Result<reqwest::Response, UpstreamFailure> {
    let response = request.send().await.map_err(classify)?;
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(status_failure(&upstream.name, response, bearer.map(Bearer::expose)).await)
    }
}

/// The whole body of a successful response.
pub async fn read_body(response: reqwest::Response) -> Result<Bytes, UpstreamFailure> {
    response.bytes().await.map_err(classify)
}

/// Connection errors, timeouts, 5xx and 429 are worth retrying elsewhere.
/// Other statuses, and failing to sign in, are the client's or the
/// operator's mistake, which a fallback would hide.
pub fn should_fallback(failure: &UpstreamFailure) -> bool {
    match failure {
        UpstreamFailure::Unreachable(_) | UpstreamFailure::Timeout => true,
        UpstreamFailure::Status { status, .. } => {
            status.is_server_error() || *status == StatusCode::TOO_MANY_REQUESTS
        }
        UpstreamFailure::SignIn(_) => false,
    }
}

/// What the client gets. Bodies of 401, 403, 429 and 5xx responses are
/// never returned; other 4xx bodies explain the client's mistake and pass
/// through, already scrubbed of the request's secret.
pub fn failure_response(upstream: &Upstream, failure: &UpstreamFailure) -> HttpResponse {
    let upstream = upstream.name.as_str();
    let error = match failure {
        UpstreamFailure::Unreachable(_) => ApiError::upstream_unavailable(upstream),
        UpstreamFailure::Timeout => ApiError::upstream_timeout(upstream),
        UpstreamFailure::SignIn(_) => ApiError::upstream_auth_failed(upstream),
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

/// The upstream's own 4xx answer.
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

/// For logs: the failure, a hint when an Azure upstream rejected its token,
/// and the start of any body.
pub fn describe_failure(upstream: &Upstream, failure: &UpstreamFailure) -> String {
    let hint = sign_in_hint(upstream, failure);
    match failure {
        UpstreamFailure::Status { body, .. } if !body.is_empty() => {
            format!("{failure}{hint}; body: {}", body_excerpt(body))
        }
        _ => format!("{failure}{hint}"),
    }
}

/// What to check when an upstream that signs in to Azure answers 401 or 403.
fn sign_in_hint(upstream: &Upstream, failure: &UpstreamFailure) -> String {
    let UpstreamFailure::Status { status, .. } = failure else {
        return String::new();
    };
    if upstream.sign_in.is_none() {
        return String::new();
    }
    match status.as_u16() {
        401 => format!(
            "; check that the base URL uses the resource's custom subdomain, or try \
             {}_TOKEN_SCOPE=https://cognitiveservices.azure.com/.default",
            upstream.name.to_ascii_uppercase()
        ),
        403 => "; give the identity the Cognitive Services OpenAI User role on the resource \
                (it can take 5 minutes to apply) and check the tenant"
            .to_string(),
        _ => String::new(),
    }
}

/// The first [`LOGGED_BODY_BYTES`] of `body`, as lossy UTF-8 with control
/// characters escaped, so the excerpt stays on one log line and can't fake
/// another.
fn body_excerpt(body: &[u8]) -> String {
    let start = &body[..body.len().min(LOGGED_BODY_BYTES)];
    escape_controls(&String::from_utf8_lossy(start))
}

/// `body` with every copy of `secret` replaced by `[redacted]`.
fn redacted(body: &[u8], secret: Option<&str>) -> Vec<u8> {
    let Some(secret) = secret
        .map(str::as_bytes)
        .filter(|secret| !secret.is_empty())
    else {
        return body.to_vec();
    };
    let mut out = Vec::with_capacity(body.len());
    let mut rest = body;
    while let Some((&byte, after)) = rest.split_first() {
        if rest.starts_with(secret) {
            out.extend_from_slice(REDACTED.as_bytes());
            rest = &rest[secret.len()..];
        } else {
            out.push(byte);
            rest = after;
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

/// A non-2xx answer. Its body is scrubbed of `secret`, the key or token the
/// request carried, here, so no log line or client response can repeat it.
async fn status_failure(
    upstream: &str,
    response: reqwest::Response,
    secret: Option<&str>,
) -> UpstreamFailure {
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
        body: Bytes::from(redacted(&body, secret)),
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
