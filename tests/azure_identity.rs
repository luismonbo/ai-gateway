//! Microsoft's sign-in code, as the gateway builds it, against simulated
//! Azure replies: a fake transport stands in for the network and a fake
//! `az` for the Azure CLI. These credentials read process-wide environment
//! variables, so the tests here hold one lock and live in a binary of their
//! own.
#![cfg(unix)]

mod common;

use ai_gateway::config::SignInMethod;
use ai_gateway::sign_in::{credentials, SignIn, SignInError};
use async_trait::async_trait;
use azure_core::http::headers::Headers;
use azure_core::http::{AsyncRawResponse, Body, HttpClient, Request, StatusCode};
use azure_core::time::{Duration, OffsetDateTime};
use azure_identity::Executor;
use std::ffi::OsStr;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Output};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

const SCOPE: &str = "https://ai.azure.com/.default";
const CLIENT_ID: &str = "0f7c3a52-8d4e-4b9a-9c1d-2e5f6a7b8c9d";
const OTHER_CLIENT_ID: &str = "9e8d7c6b-5a49-4382-a1b0-c9d8e7f6a5b4";
const TENANT_ID: &str = "11111111-2222-4333-8444-555555555555";

/// Every variable the Azure crates read to choose how to sign in.
const IDENTITY_VARIABLES: [&str; 10] = [
    "IDENTITY_ENDPOINT",
    "IDENTITY_HEADER",
    "IDENTITY_SERVER_THUMBPRINT",
    "IMDS_ENDPOINT",
    "MSI_ENDPOINT",
    "MSI_SECRET",
    "AZURE_CLIENT_ID",
    "AZURE_TENANT_ID",
    "AZURE_FEDERATED_TOKEN_FILE",
    "AZURE_AUTHORITY_HOST",
];

static ENV: Mutex<()> = Mutex::new(());

/// The process environment for one test: the identity variables cleared,
/// then `vars` set. Dropping it clears them again, even after a panic.
struct TestEnv {
    _lock: MutexGuard<'static, ()>,
}

impl TestEnv {
    fn set(vars: &[(&str, &str)]) -> Self {
        let lock = ENV.lock().unwrap_or_else(PoisonError::into_inner);
        clear_identity_variables();
        for (name, value) in vars {
            std::env::set_var(name, value);
        }
        Self { _lock: lock }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // Runs before the lock is released.
        clear_identity_variables();
    }
}

fn clear_identity_variables() {
    for name in IDENTITY_VARIABLES {
        std::env::remove_var(name);
    }
}

/// A request as the fake transport saw it.
#[derive(Debug, Clone)]
struct Sent {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Sent {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(sent_name, _)| sent_name == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Answers every request with 200 and `reply`, and keeps what was sent.
#[derive(Debug)]
struct FakeTransport {
    reply: String,
    sent: Mutex<Vec<Sent>>,
}

impl FakeTransport {
    fn new(reply: &str) -> Arc<Self> {
        Arc::new(Self {
            reply: reply.to_string(),
            sent: Mutex::new(Vec::new()),
        })
    }

    /// The one request sent; fails the test if there were more or none.
    fn only_request(&self) -> Sent {
        let sent = self.sent.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(sent.len(), 1, "{sent:?}");
        sent[0].clone()
    }
}

#[async_trait]
impl HttpClient for FakeTransport {
    async fn execute_request(&self, request: &Request) -> azure_core::Result<AsyncRawResponse> {
        let body = match request.body() {
            Body::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            Body::SeekableStream(_) => String::from("<stream>"),
        };
        let sent = Sent {
            method: request.method().as_str().to_string(),
            url: request.url().to_string(),
            headers: request
                .headers()
                .iter()
                .map(|(name, value)| (name.as_str().to_string(), value.as_str().to_string()))
                .collect(),
            body,
        };
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(sent);
        Ok(AsyncRawResponse::from_bytes(
            StatusCode::Ok,
            Headers::new(),
            self.reply.clone(),
        ))
    }
}

/// Stands in for `az`: keeps each command line and prints `stdout`.
#[derive(Debug)]
struct FakeAz {
    stdout: String,
    commands: Mutex<Vec<String>>,
}

impl FakeAz {
    fn new(stdout: &str) -> Arc<Self> {
        Arc::new(Self {
            stdout: stdout.to_string(),
            commands: Mutex::new(Vec::new()),
        })
    }

    fn commands(&self) -> Vec<String> {
        self.commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Executor for FakeAz {
    async fn run(&self, program: &OsStr, args: &[&OsStr]) -> io::Result<Output> {
        let command = std::iter::once(program)
            .chain(args.iter().copied())
            .map(|part| part.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        self.commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(command);
        Ok(Output {
            status: ExitStatus::from_raw(0),
            stdout: self.stdout.clone().into_bytes(),
            stderr: Vec::new(),
        })
    }
}

fn in_an_hour() -> i64 {
    (OffsetDateTime::now_utc() + Duration::hours(1)).unix_timestamp()
}

/// What the VM metadata endpoint and App Service answer.
fn managed_identity_reply(token: &str) -> String {
    format!(
        r#"{{"access_token":"{token}","expires_on":"{}","resource":"https://ai.azure.com","token_type":"Bearer"}}"#,
        in_an_hour()
    )
}

/// What Microsoft Entra answers.
fn entra_reply(token: &str) -> String {
    format!(
        r#"{{"token_type":"Bearer","expires_in":3599,"ext_expires_in":3599,"access_token":"{token}"}}"#
    )
}

/// What `az account get-access-token -o json` prints.
fn cli_reply(token: &str) -> String {
    format!(
        r#"{{"accessToken":"{token}","expires_on":{},"tokenType":"Bearer"}}"#,
        in_an_hour()
    )
}

fn query(url: &str) -> Vec<(String, String)> {
    azure_core::http::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

fn pair(name: &str, value: &str) -> (String, String) {
    (name.to_string(), value.to_string())
}

#[actix_web::test]
async fn vm_managed_identity_asks_the_metadata_endpoint() {
    let _env = TestEnv::set(&[]);
    let transport = FakeTransport::new(&managed_identity_reply("vm-token"));
    let method = SignInMethod::ManagedIdentity {
        client_id: Some(CLIENT_ID.to_string()),
    };
    let credential = credentials::credential(&method, transport.clone(), FakeAz::new("")).unwrap();

    let token = credential.get_token(&[SCOPE], None).await.unwrap();
    assert_eq!(token.token.secret(), "vm-token");

    let sent = transport.only_request();
    assert!(
        sent.url
            .starts_with("http://169.254.169.254/metadata/identity/oauth2/token"),
        "{}",
        sent.url
    );
    let query = query(&sent.url);
    assert!(
        query.contains(&pair("resource", "https://ai.azure.com")),
        "{query:?}"
    );
    assert!(query.contains(&pair("client_id", CLIENT_ID)), "{query:?}");
    assert_eq!(sent.header("metadata"), Some("true"));
}

#[actix_web::test]
async fn app_service_managed_identity_sends_the_identity_header() {
    let _env = TestEnv::set(&[
        ("IDENTITY_ENDPOINT", "http://localhost:42356/msi/token"),
        ("IDENTITY_HEADER", "identity-header-secret-123"),
    ]);
    let (_guard, logs) = common::capture_logs();
    let transport = FakeTransport::new(&managed_identity_reply("app-token-secret-456"));
    let method = SignInMethod::ManagedIdentity { client_id: None };
    let credential = credentials::credential(&method, transport.clone(), FakeAz::new("")).unwrap();

    let token = credential.get_token(&[SCOPE], None).await.unwrap();
    assert_eq!(token.token.secret(), "app-token-secret-456");

    let sent = transport.only_request();
    assert!(
        sent.url.starts_with("http://localhost:42356/msi/token"),
        "{}",
        sent.url
    );
    assert_eq!(
        sent.header("x-identity-header"),
        Some("identity-header-secret-123")
    );

    let logs =
        String::from_utf8_lossy(&logs.lock().unwrap_or_else(PoisonError::into_inner)).into_owned();
    // The crates log the request with its header names; only the values go.
    assert!(logs.contains("x-identity-header"), "{logs}");
    assert!(!logs.contains("identity-header-secret-123"), "{logs}");
    assert!(!logs.contains("app-token-secret-456"), "{logs}");
}

#[actix_web::test]
async fn workload_identity_exchanges_the_service_account_token() {
    let token_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(token_file.path(), "k8s-sa-token").unwrap();
    let _env = TestEnv::set(&[
        ("AZURE_TENANT_ID", TENANT_ID),
        ("AZURE_CLIENT_ID", CLIENT_ID),
        (
            "AZURE_FEDERATED_TOKEN_FILE",
            token_file.path().to_str().unwrap(),
        ),
    ]);

    // Without a client ID of its own, it uses the one AKS injected.
    let transport = FakeTransport::new(&entra_reply("wi-token"));
    let method = SignInMethod::WorkloadIdentity { client_id: None };
    let credential = credentials::credential(&method, transport.clone(), FakeAz::new("")).unwrap();

    let token = credential.get_token(&[SCOPE], None).await.unwrap();
    assert_eq!(token.token.secret(), "wi-token");
    let sent = transport.only_request();
    assert_eq!(sent.method, "POST");
    assert_eq!(
        sent.url,
        format!("https://login.microsoftonline.com/{TENANT_ID}/oauth2/v2.0/token")
    );
    assert!(
        sent.body.contains("client_assertion=k8s-sa-token"),
        "{}",
        sent.body
    );
    assert!(
        sent.body.contains(&format!("client_id={CLIENT_ID}")),
        "{}",
        sent.body
    );

    // A client ID of its own wins over the injected one.
    let transport = FakeTransport::new(&entra_reply("wi-token-2"));
    let method = SignInMethod::WorkloadIdentity {
        client_id: Some(OTHER_CLIENT_ID.to_string()),
    };
    let credential = credentials::credential(&method, transport.clone(), FakeAz::new("")).unwrap();
    credential.get_token(&[SCOPE], None).await.unwrap();
    let body = transport.only_request().body;
    assert!(
        body.contains(&format!("client_id={OTHER_CLIENT_ID}")),
        "{body}"
    );
}

#[actix_web::test]
async fn azure_cli_runs_az() {
    let _env = TestEnv::set(&[]);
    let az = FakeAz::new(&cli_reply("cli-token"));
    let credential = credentials::credential(
        &SignInMethod::AzureCli,
        FakeTransport::new("{}"),
        az.clone(),
    )
    .unwrap();

    let token = credential.get_token(&[SCOPE], None).await.unwrap();
    assert_eq!(token.token.secret(), "cli-token");

    let commands = az.commands();
    assert_eq!(commands.len(), 1, "{commands:?}");
    assert!(
        commands[0].contains("az account get-access-token"),
        "{commands:?}"
    );
    assert!(commands[0].contains("https://ai.azure.com"), "{commands:?}");
}

#[actix_web::test]
async fn unsupported_managed_identity_environments_fail_when_built() {
    // Cloud Shell sets MSI_ENDPOINT.
    let _env = TestEnv::set(&[("MSI_ENDPOINT", "http://localhost:1/msi")]);
    let method = SignInMethod::ManagedIdentity { client_id: None };

    let err =
        credentials::credential(&method, FakeTransport::new("{}"), FakeAz::new("")).unwrap_err();
    assert!(err.to_string().contains("isn't supported"), "{err}");
}

#[actix_web::test]
async fn from_config_names_the_upstream_when_a_credential_cannot_be_built() {
    let _env = TestEnv::set(&[("MSI_ENDPOINT", "http://localhost:1/msi")]);
    let config = common::config(&[
        ("UPSTREAMS", "azure"),
        ("AZURE_BASE_URL", "https://res.openai.azure.com/openai/v1"),
        ("AZURE_AUTH", "managed_identity"),
    ]);

    let err = SignIn::from_config(&config, FakeTransport::new("{}"), FakeAz::new("")).unwrap_err();
    let SignInError::Setup {
        upstream,
        method,
        reason,
    } = &err
    else {
        panic!("{err:?}");
    };
    assert_eq!(upstream, "azure");
    assert_eq!(method, "managed identity (system-assigned)");
    assert!(reason.contains("isn't supported"), "{reason}");
}
