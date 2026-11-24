//! Shared helpers for the integration tests.
#![allow(dead_code)] // each test binary uses a different subset of these

use actix_web::body::MessageBody;
use actix_web::dev::{Service, ServiceResponse};
use actix_web::{test, web};
use ai_gateway::app::{build_app, AppState};
use ai_gateway::config::Config;
use ai_gateway::stats::StatsStore;
use serde_json::{json, Value};
use std::sync::Arc;
use wiremock::MockServer;

pub const USER_KEY: &str = "test-user-key-0123456789abcdef01234567";
pub const ADMIN_KEY: &str = "test-admin-key-0123456789abcdef0123456";
/// The key the gateway sends to the `primary` mock when a test sets it.
pub const UPSTREAM_KEY: &str = "sk-test-upstream-key-0123456789abcdef";

/// Nothing listens on port 9, so a request that reaches it fails to connect.
pub const UNREACHABLE_URL: &str = "http://127.0.0.1:9/v1";

/// The usage-only chunk an upstream sends last when asked for usage.
pub const USAGE_EVENT: &str = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\
    \"model\":\"m1\",\"choices\":[],\
    \"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n";

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
    AppState::new(config, Arc::new(StatsStore::default())).expect("the HTTP client builds")
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

/// A URL where nothing listens: the port was free a moment ago.
pub fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let port = listener.local_addr().expect("a bound address").port();
    drop(listener);
    format!("http://127.0.0.1:{port}/v1")
}

/// The base URL for an upstream served by `server`.
pub fn mock_url(server: &MockServer) -> String {
    format!("{}/v1", server.uri())
}

/// A minimal chat request for `model`.
pub fn chat_body(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "Hello"}]})
}

/// A whole streamed answer: a content event, a stop event, [`USAGE_EVENT`]
/// and the `[DONE]` marker.
pub fn sse_stream() -> String {
    [
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m1\",\
         \"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"},\
         \"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m1\",\
         \"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        USAGE_EVENT,
        "data: [DONE]\n\n",
    ]
    .concat()
}
