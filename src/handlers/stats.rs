use crate::auth::{Caller, Role};
use crate::tracking::{KeyStats, RequestTracker};
use actix_web::{web, HttpResponse};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};

#[derive(serde::Deserialize)]
pub struct StatsQuery {
    pub key_id: Option<String>,
}

#[derive(Serialize)]
pub struct KeyStatsResponse {
    pub key_id: String,
    pub request_count: u64,
    pub error_count: u64,
    pub total_latency_ms: u64,
    pub avg_latency_ms: f64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub last_request_timestamp: u64,
    pub models_used: HashMap<String, u64>,
}

pub async fn get_stats(
    caller: Caller,
    query: web::Query<StatsQuery>,
    tracker: web::Data<RwLock<RequestTracker>>,
) -> HttpResponse {
    let tracker = tracker.read().unwrap_or_else(PoisonError::into_inner);

    match caller.role {
        Role::Admin => match &query.key_id {
            // Admin requesting one key's stats
            Some(key_id) => match tracker.get_stats(key_id) {
                Some(stats) => HttpResponse::Ok().json(build_stats_response(key_id, stats)),
                None => HttpResponse::NotFound().body("No stats for that key"),
            },
            // Admin requesting all stats
            None => {
                let all_stats: Vec<KeyStatsResponse> = tracker
                    .get_all_stats()
                    .iter()
                    .map(|(key_id, stats)| build_stats_response(key_id, stats))
                    .collect();
                HttpResponse::Ok().json(all_stats)
            }
        },
        // Users only see their own stats; `?key_id=` is ignored.
        Role::User => {
            let key_id = caller.key_id.as_str();
            let response = match tracker.get_stats(key_id) {
                Some(stats) => build_stats_response(key_id, stats),
                None => empty_stats_response(key_id),
            };
            HttpResponse::Ok().json(response)
        }
    }
}

fn build_stats_response(key_id: &str, stats: &KeyStats) -> KeyStatsResponse {
    let avg_latency = if stats.request_count > 0 {
        stats.total_latency_ms as f64 / stats.request_count as f64
    } else {
        0.0
    };

    let timestamp = stats
        .last_request_timestamp
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    KeyStatsResponse {
        key_id: key_id.to_string(),
        request_count: stats.request_count,
        error_count: stats.error_count,
        total_latency_ms: stats.total_latency_ms,
        avg_latency_ms: avg_latency,
        total_prompt_tokens: stats.total_prompt_tokens,
        total_completion_tokens: stats.total_completion_tokens,
        last_request_timestamp: timestamp,
        models_used: stats.models_used.clone(),
    }
}

/// A key with no recorded requests yet (e.g. its first call to /v1/stats).
fn empty_stats_response(key_id: &str) -> KeyStatsResponse {
    KeyStatsResponse {
        key_id: key_id.to_string(),
        request_count: 0,
        error_count: 0,
        total_latency_ms: 0,
        avg_latency_ms: 0.0,
        total_prompt_tokens: 0,
        total_completion_tokens: 0,
        last_request_timestamp: 0,
        models_used: HashMap::new(),
    }
}
