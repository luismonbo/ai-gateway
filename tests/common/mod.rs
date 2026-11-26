//! Shared helpers for the integration tests.
#![allow(dead_code)] // each test binary uses a different subset of these

use actix_web::body::MessageBody;
use actix_web::dev::{Service, ServiceResponse};
use actix_web::{test, web};
use ai_gateway::app::{build_app, AppState};
use ai_gateway::config::Config;
use ai_gateway::stats::StatsStore;
use azure_core::credentials::{AccessToken, TokenCredential, TokenRequestOptions};
use azure_core::error::ErrorKind;
use azure_core::time::{Duration, OffsetDateTime};
use serde_json::{json, Value};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
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
    AppState::new(config, Arc::new(StatsStore::default())).expect("the app state builds")
}

/// The app state, with `credentials` signing in the upstreams they name.
pub fn state_with_credentials(
    config: Config,
    credentials: &[(&str, Arc<FakeCredential>)],
) -> AppState {
    let credentials = credentials
        .iter()
        .map(|(name, credential)| {
            let credential: Arc<dyn TokenCredential> = credential.clone();
            (name.to_string(), credential)
        })
        .collect();
    AppState::with_credentials(config, Arc::new(StatsStore::default()), credentials)
        .expect("the app state builds")
}

/// What a failing [`FakeCredential`] reports.
pub const SIGN_IN_FAILURE: &str = "fake sign-in failure: please run 'az login'";

/// A token source for tests: always the same token, valid for an hour, or
/// always [`SIGN_IN_FAILURE`].
#[derive(Debug)]
pub struct FakeCredential {
    token: Option<String>,
    /// How long each call takes.
    delay: std::time::Duration,
    calls: AtomicUsize,
}

impl FakeCredential {
    pub fn ok(token: &str) -> Arc<Self> {
        Self::slow(token, std::time::Duration::ZERO)
    }

    /// Like [`FakeCredential::ok`], taking `delay` to answer.
    pub fn slow(token: &str, delay: std::time::Duration) -> Arc<Self> {
        Arc::new(Self {
            token: Some(token.to_string()),
            delay,
            calls: AtomicUsize::new(0),
        })
    }

    pub fn failing() -> Arc<Self> {
        Arc::new(Self {
            token: None,
            delay: std::time::Duration::ZERO,
            calls: AtomicUsize::new(0),
        })
    }

    /// How many tokens were asked for.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl TokenCredential for FakeCredential {
    async fn get_token(
        &self,
        _scopes: &[&str],
        _options: Option<TokenRequestOptions<'_>>,
    ) -> azure_core::Result<AccessToken> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.delay.is_zero() {
            actix_web::rt::time::sleep(self.delay).await;
        }
        match &self.token {
            Some(token) => Ok(AccessToken::new(
                token.clone(),
                OffsetDateTime::now_utc() + Duration::hours(1),
            )),
            None => Err(azure_core::Error::with_message(
                ErrorKind::Credential,
                SIGN_IN_FAILURE,
            )),
        }
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

/// Everything logged on this thread from now until the guard drops, at
/// DEBUG and above, as plain text. `actix_web::test` runs a test on one
/// thread, so the gateway's own log lines all land here.
pub fn capture_logs() -> (tracing::subscriber::DefaultGuard, Arc<Mutex<Vec<u8>>>) {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = SharedBuffer(Arc::clone(&buffer));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    (tracing::subscriber::set_default(subscriber), buffer)
}

#[derive(Clone)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl io::Write for SharedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
