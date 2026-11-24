//! The stats middleware and `GET /v1/stats`.

use super::store::KeyStats;
use crate::app::AppState;
use crate::auth::{Caller, KeyId, Role};
use crate::error::ApiError;
use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::middleware::Next;
use actix_web::{web, HttpMessage, HttpResponse};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Instant;

/// Innermost middleware in `/v1`: counts every request that got past auth
/// and the rate limit, with its latency to the response headers. A 5xx
/// response counts as an error.
pub async fn track_request<B: MessageBody>(
    state: web::Data<AppState>,
    req: ServiceRequest,
    next: Next<B>,
) -> Result<ServiceResponse<B>, actix_web::Error> {
    let key_id = req
        .extensions()
        .get::<Caller>()
        .map(|caller| caller.key_id.clone());
    let started = Instant::now();
    let response = next.call(req).await?;
    if let Some(key_id) = key_id {
        let is_error = response.status().is_server_error();
        state
            .stats
            .record_request(&key_id, started.elapsed(), is_error);
    }
    Ok(response)
}

#[derive(Deserialize)]
pub struct StatsQuery {
    pub key_id: Option<String>,
}

#[derive(Serialize)]
struct KeyStatsView<'a> {
    key_id: &'a KeyId,
    request_count: u64,
    error_count: u64,
    total_latency_ms: u64,
    avg_latency_ms: f64,
    total_prompt_tokens: u64,
    total_completion_tokens: u64,
    last_request_timestamp: u64,
    models_used: &'a BTreeMap<String, u64>,
}

fn view<'a>(key_id: &'a KeyId, stats: &'a KeyStats) -> KeyStatsView<'a> {
    let avg_latency_ms = if stats.request_count == 0 {
        0.0
    } else {
        stats.total_latency_ms as f64 / stats.request_count as f64
    };
    KeyStatsView {
        key_id,
        request_count: stats.request_count,
        error_count: stats.error_count,
        total_latency_ms: stats.total_latency_ms,
        avg_latency_ms,
        total_prompt_tokens: stats.total_prompt_tokens,
        total_completion_tokens: stats.total_completion_tokens,
        last_request_timestamp: stats.last_request_timestamp,
        models_used: &stats.models_used,
    }
}

/// Users get their own stats (zeros before their first request) and
/// `?key_id=` is ignored. Admins get everyone's as an array, or one key's
/// with `?key_id=`.
pub async fn get_stats(
    state: web::Data<AppState>,
    caller: Caller,
    query: web::Query<StatsQuery>,
) -> Result<HttpResponse, ApiError> {
    match (caller.role, query.key_id.as_deref()) {
        (Role::User, _) => {
            let stats = state.stats.get(&caller.key_id).unwrap_or_default();
            Ok(HttpResponse::Ok().json(view(&caller.key_id, &stats)))
        }
        (Role::Admin, None) => {
            let all = state.stats.all();
            let views: Vec<KeyStatsView> = all.iter().map(|(id, stats)| view(id, stats)).collect();
            Ok(HttpResponse::Ok().json(views))
        }
        (Role::Admin, Some(raw)) => {
            // Never echo the value: it may be a raw key pasted by mistake.
            let key_id = KeyId::parse(raw).ok_or_else(|| {
                ApiError::bad_request(
                    "`key_id` must be a key ID such as key_0123456789ab, not an API key.",
                )
            })?;
            let stats = state
                .stats
                .get(&key_id)
                .ok_or_else(|| ApiError::not_found(format!("No stats for {key_id}.")))?;
            Ok(HttpResponse::Ok().json(view(&key_id, &stats)))
        }
    }
}
