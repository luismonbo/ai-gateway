//! The access log: one line per request, through `tracing` like every
//! other log line, so `LOG_FORMAT=json` and `RUST_LOG` apply to it too.

use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::header::USER_AGENT;
use actix_web::middleware::Next;
use std::time::Instant;
use tracing::info;

/// Outermost middleware: logs every request, including ones that auth or
/// the rate limiter reject. It records the path but never the query string,
/// where a client could have put a secret; the gateway never needs one.
pub async fn access_log<B: MessageBody>(
    req: ServiceRequest,
    next: Next<B>,
) -> Result<ServiceResponse<B>, actix_web::Error> {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.path().to_string();
    let peer = req
        .peer_addr()
        .map_or_else(|| "-".to_string(), |addr| addr.ip().to_string());
    let user_agent = req
        .headers()
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-")
        .to_string();

    let result = next.call(req).await;
    let status = match &result {
        Ok(response) => response.status(),
        Err(error) => error.as_response_error().status_code(),
    };
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    info!(
        target: "access",
        %method,
        %path,
        status = status.as_u16(),
        latency_ms,
        %peer,
        // to_str() above only accepts visible ASCII, so no line breaks.
        %user_agent,
        "request"
    );
    result
}
