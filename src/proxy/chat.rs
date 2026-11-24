//! `POST /v1/chat/completions`: pick the upstream from the model prefix,
//! forward the body with only `model` rewritten, and relay the answer.

use super::routing::resolve_route;
use super::sse::{relay_sse, with_include_usage, StreamInspector, StreamSummary};
use super::upstream::{
    self, describe_failure, failure_response, read_body, send_chat, UpstreamFailure,
};
use super::usage::{probe, Usage};
use crate::app::AppState;
use crate::auth::{Caller, KeyId};
use crate::config::Upstream;
use crate::error::ApiError;
use crate::tracking::RequestTracker;
use actix_web::error::PayloadError;
use actix_web::http::header::{HeaderName, HeaderValue, CACHE_CONTROL, CONTENT_TYPE};
use actix_web::http::StatusCode;
use actix_web::{web, HttpResponse};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::{Arc, PoisonError, RwLock};
use tracing::{debug, warn};

/// Names the upstream that handled the request, on every response that
/// reached one.
const UPSTREAM_HEADER: &str = "x-gateway-upstream";

/// A chat request as the client sent it. Only `model` and `stream` are read;
/// every other field is kept as-is in `rest` and forwarded unchanged.
#[derive(Serialize, Deserialize, Debug)]
pub struct ChatRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// The upstream's copy of a request, borrowing from the client's.
#[derive(Serialize)]
struct Outgoing<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(flatten)]
    rest: &'a Map<String, Value>,
}

/// The JSON body for the upstream: the client's request with `model`
/// replaced.
pub fn upstream_body(request: &ChatRequest, model: &str) -> Vec<u8> {
    let outgoing = Outgoing {
        model,
        stream: request.stream,
        rest: &request.rest,
    };
    serde_json::to_vec(&outgoing).expect("serializing a JSON object cannot fail")
}

pub async fn chat_completions(
    state: web::Data<AppState>,
    caller: Caller,
    body: Result<web::Bytes, actix_web::Error>,
) -> Result<HttpResponse, ApiError> {
    let body = body.map_err(|e| body_error(&e, state.config.max_request_bytes))?;
    let request: ChatRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?;
    let (request, strip_usage) = ask_for_usage(request);

    let names = state.config.upstream_names();
    let route = resolve_route(
        &request.model,
        &names,
        state.config.default_upstream.as_deref(),
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let upstream = state
        .config
        .upstream(route.upstream)
        .expect("routes only name configured upstreams");

    let stream = request.stream == Some(true);
    let body = upstream_body(&request, route.model);
    let response = match attempt(&state, upstream, body, stream).await {
        Ok(attempt) => {
            success_response(&state, &caller, upstream, route.model, attempt, strip_usage)
        }
        Err(failure) => {
            warn!(
                key_id = %caller.key_id,
                upstream = %upstream.name,
                error = %describe_failure(upstream, &failure),
                "upstream request failed"
            );
            failure_response(&upstream.name, &failure)
        }
    };
    Ok(with_upstream_header(response, &upstream.name))
}

fn body_error(error: &actix_web::Error, limit: usize) -> ApiError {
    match error.as_error::<PayloadError>() {
        Some(PayloadError::Overflow) => ApiError::payload_too_large(limit),
        _ => {
            debug!(error = %error, "could not read the request body");
            ApiError::bad_request("could not read the request body")
        }
    }
}

/// Streaming requests ask the upstream to report usage. Returns the request
/// and whether the gateway added that itself (its usage chunk is then
/// dropped before the client sees it).
fn ask_for_usage(request: ChatRequest) -> (ChatRequest, bool) {
    if request.stream != Some(true) {
        return (request, false);
    }
    let ChatRequest {
        model,
        stream,
        rest,
    } = request;
    let (rest, injected) = with_include_usage(rest);
    (
        ChatRequest {
            model,
            stream,
            rest,
        },
        injected,
    )
}

/// A successful upstream answer: read in full, or still streaming.
enum Attempt {
    Complete {
        status: StatusCode,
        content_type: Option<HeaderValue>,
        body: Bytes,
    },
    Stream(reqwest::Response),
}

/// Sends `body` to `upstream`. A non-streaming answer is read in full here,
/// so a failure while reading it is still a failure (and can fall back).
async fn attempt(
    state: &AppState,
    upstream: &Upstream,
    body: Vec<u8>,
    stream: bool,
) -> Result<Attempt, UpstreamFailure> {
    let response = send_chat(&state.http, upstream, body).await?;
    if stream {
        return Ok(Attempt::Stream(response));
    }
    let status = upstream::status(&response);
    let content_type = upstream::header(&response, reqwest::header::CONTENT_TYPE);
    let body = read_body(response).await?;
    Ok(Attempt::Complete {
        status,
        content_type,
        body,
    })
}

/// Relays a successful answer and records its token usage under
/// `"{upstream}/{model}"`, preferring the model the upstream reported.
fn success_response(
    state: &web::Data<AppState>,
    caller: &Caller,
    upstream: &Upstream,
    model_sent: &str,
    attempt: Attempt,
    strip_usage: bool,
) -> HttpResponse {
    match attempt {
        Attempt::Complete {
            status,
            content_type,
            body,
        } => {
            if let Some(found) = probe(&body) {
                let model = found.model.as_deref().unwrap_or(model_sent);
                if let Some(usage) = found.usage {
                    let label = model_label(&upstream.name, model);
                    record_usage(&state.stats, &caller.key_id, &label, usage);
                }
            }
            let content_type =
                content_type.unwrap_or_else(|| HeaderValue::from_static("application/json"));
            HttpResponse::build(status)
                .insert_header((CONTENT_TYPE, content_type))
                .body(body)
        }
        Attempt::Stream(response) => {
            let stats = Arc::clone(&state.stats);
            let key_id = caller.key_id.clone();
            let upstream_name = upstream.name.clone();
            let model_sent = model_sent.to_string();
            let on_finish = move |summary: StreamSummary| {
                if let Some(usage) = summary.usage {
                    let model = summary.model.unwrap_or(model_sent);
                    let label = model_label(&upstream_name, &model);
                    record_usage(&stats, &key_id, &label, usage);
                }
            };
            let events = relay_sse(
                response.bytes_stream(),
                StreamInspector::new(strip_usage),
                on_finish,
            );
            HttpResponse::Ok()
                .insert_header((CONTENT_TYPE, "text/event-stream"))
                .insert_header((CACHE_CONTROL, "no-cache"))
                .streaming(events)
        }
    }
}

fn model_label(upstream: &str, model: &str) -> String {
    format!("{upstream}/{model}")
}

fn record_usage(stats: &RwLock<RequestTracker>, key_id: &KeyId, model: &str, usage: Usage) {
    stats
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .record_tokens(
            key_id.as_str(),
            usage.prompt_tokens,
            usage.completion_tokens,
            model,
        );
}

fn with_upstream_header(response: HttpResponse, upstream: &str) -> HttpResponse {
    let Ok(value) = HeaderValue::from_str(upstream) else {
        return response;
    };
    let mut response = response;
    response
        .headers_mut()
        .insert(HeaderName::from_static(UPSTREAM_HEADER), value);
    response
}
