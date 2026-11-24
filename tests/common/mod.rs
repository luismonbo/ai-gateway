//! Shared helpers for the integration tests.
#![allow(dead_code)] // each test binary uses a different subset of these

use actix_web::body::MessageBody;
use actix_web::dev::{Service, ServiceResponse};
use actix_web::{test, web};
use ai_gateway::app::{build_app, AppState};
use ai_gateway::config::Config;
use ai_gateway::middleware::RateLimiter;
use ai_gateway::providers::ollama::OllamaProvider;
use ai_gateway::tracking::RequestTracker;
use serde_json::Value;
use std::sync::{Arc, RwLock};

pub const USER_KEY: &str = "test-user-key-0123456789abcdef01234567";
pub const ADMIN_KEY: &str = "test-admin-key-0123456789abcdef0123456";

/// Nothing listens on port 9, so a request that reaches it fails to connect.
pub const UNREACHABLE_URL: &str = "http://127.0.0.1:9/v1";

/// A valid config: the two test keys and two upstreams, `primary` and
/// `backup`, plus `overrides`. An override wins over a base value, and
/// `("X", "")` unsets X (blank values count as unset).
pub fn config(overrides: &[(&str, &str)]) -> Config {
    let base = [
        ("GATEWAY_API_KEYS", USER_KEY),
        ("ADMIN_API_KEYS", ADMIN_KEY),
        ("UPSTREAMS", "primary,backup"),
        ("PRIMARY_BASE_URL", UNREACHABLE_URL),
        ("BACKUP_BASE_URL", UNREACHABLE_URL),
    ];
    let lookup = |name: &str| {
        overrides
            .iter()
            .chain(base.iter())
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    };
    let (config, _warnings) = Config::from_lookup(lookup).expect("the test config is valid");
    config
}

pub fn state(config: Config) -> AppState {
    AppState {
        config,
        stats: Arc::new(RwLock::new(RequestTracker::new())),
        limiter: Arc::new(RateLimiter::new(60)),
        // Nothing listens on port 9; no test reaches the legacy provider.
        provider: Arc::new(OllamaProvider::new("http://127.0.0.1:9".to_string())),
    }
}

/// The whole app, in-process, ready for `test::call_service`.
pub async fn init(
    state: AppState,
) -> impl Service<
    actix_http::Request,
    Response = ServiceResponse<impl MessageBody>,
    Error = actix_web::Error,
> {
    test::init_service(build_app(web::Data::new(state))).await
}

pub fn bearer(key: &str) -> (&'static str, String) {
    ("Authorization", format!("Bearer {key}"))
}

pub async fn json_body<B: MessageBody>(response: ServiceResponse<B>) -> Value {
    let bytes = test::read_body(response).await;
    serde_json::from_slice(&bytes).expect("the response body is JSON")
}
