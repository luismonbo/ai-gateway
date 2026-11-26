use super::*;
use crate::sign_in::fake::ScriptedCredential;
use azure_core::time::{Duration, OffsetDateTime};

const GATEWAY_KEY: &str = "0123456789abcdef0123456789abcdef";
const UPSTREAM_KEY: &str = "sk-upstream-key-0123456789abcdef";
const LOOPBACK: &str = "http://127.0.0.1:9/v1";

/// The config from exactly `vars`.
fn config(vars: &[(&str, &str)]) -> Config {
    let lookup = |name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    };
    Config::from_lookup(lookup)
        .expect("the test config is valid")
        .0
}

/// `keyed` sends a key, `azure` signs in with the Azure CLI, `open` sends
/// nothing; `extra` is added to the variables.
fn three_upstreams(extra: &[(&'static str, &'static str)]) -> Config {
    let base = [
        ("GATEWAY_API_KEYS", GATEWAY_KEY),
        ("UPSTREAMS", "keyed,azure,open"),
        ("KEYED_BASE_URL", LOOPBACK),
        ("KEYED_API_KEY", UPSTREAM_KEY),
        ("AZURE_BASE_URL", LOOPBACK),
        ("AZURE_AUTH", "azure_cli"),
        ("OPEN_BASE_URL", LOOPBACK),
    ];
    let vars: Vec<_> = extra.iter().copied().chain(base).collect();
    config(&vars)
}

fn in_an_hour() -> OffsetDateTime {
    OffsetDateTime::now_utc() + Duration::hours(1)
}

fn with_azure(config: &Config, credential: &Arc<ScriptedCredential>) -> SignIn {
    let credential: Arc<dyn TokenCredential> = credential.clone();
    SignIn::with_credentials(config, vec![("azure".to_string(), credential)])
}

/// The secret `upstream`'s request would send.
async fn exposed(sign_in: &SignIn, config: &Config, upstream: &str) -> Option<String> {
    sign_in
        .bearer(config.upstream(upstream).unwrap())
        .await
        .expect("a bearer")
        .map(|bearer| bearer.expose().to_string())
}

#[actix_web::test]
async fn bearer_is_the_key_a_token_or_nothing() {
    let config = three_upstreams(&[]);
    let credential = ScriptedCredential::new(vec![Ok(("tok-1", in_an_hour()))], Duration::ZERO);
    let sign_in = with_azure(&config, &credential);

    assert_eq!(
        exposed(&sign_in, &config, "keyed").await.as_deref(),
        Some(UPSTREAM_KEY)
    );
    assert_eq!(
        exposed(&sign_in, &config, "azure").await.as_deref(),
        Some("tok-1")
    );
    assert_eq!(exposed(&sign_in, &config, "open").await, None);
    assert_eq!(credential.calls(), 1);
}

#[actix_web::test]
async fn bearer_reports_sign_in_failures() {
    let config = three_upstreams(&[]);
    let credential =
        ScriptedCredential::new(vec![Err("az: please run 'az login'")], Duration::ZERO);
    let sign_in = with_azure(&config, &credential);

    let err = sign_in
        .bearer(config.upstream("azure").unwrap())
        .await
        .unwrap_err();
    let SignInError::Token(message) = err else {
        panic!("{err:?}");
    };
    assert!(message.contains("please run 'az login'"), "{message}");
}

#[actix_web::test]
async fn an_azure_upstream_without_a_credential_is_an_error() {
    let config = three_upstreams(&[]);
    let sign_in = SignIn::with_credentials(&config, vec![]);

    let azure = sign_in.bearer(config.upstream("azure").unwrap()).await;
    assert!(matches!(azure, Err(SignInError::Token(_))), "{azure:?}");
    // The others never needed one.
    assert!(sign_in
        .bearer(config.upstream("keyed").unwrap())
        .await
        .is_ok());
}

#[actix_web::test]
async fn caches_ask_for_their_upstreams_scope() {
    let older = "https://cognitiveservices.azure.com/.default";
    let config = three_upstreams(&[("AZURE_TOKEN_SCOPE", older)]);
    let credential = ScriptedCredential::new(vec![Ok(("tok-1", in_an_hour()))], Duration::ZERO);
    let sign_in = with_azure(&config, &credential);

    exposed(&sign_in, &config, "azure").await;
    assert_eq!(credential.scopes(), [older]);
}

#[actix_web::test]
async fn fetches_are_bounded_by_the_connect_timeout() {
    let config = three_upstreams(&[("UPSTREAM_CONNECT_TIMEOUT_SECS", "1")]);
    let credential =
        ScriptedCredential::new(vec![Ok(("late", in_an_hour()))], Duration::seconds(3));
    let sign_in = with_azure(&config, &credential);

    let err = sign_in
        .bearer(config.upstream("azure").unwrap())
        .await
        .unwrap_err();
    assert_eq!(err, SignInError::Timeout(std::time::Duration::from_secs(1)));
}

#[test]
fn bearer_debug_is_redacted() {
    let token = format!("{:?}", Bearer::Token(Secret::new("tok-secret-1")));
    assert!(!token.contains("tok-secret-1"), "{token}");

    let config = three_upstreams(&[]);
    let key = config.upstream("keyed").unwrap().api_key.clone().unwrap();
    let key = format!("{:?}", Bearer::Key(key));
    assert!(!key.contains(UPSTREAM_KEY), "{key}");
}
