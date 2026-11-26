use super::*;
use crate::sign_in::fake::ScriptedCredential;
use azure_core::time::{Duration, OffsetDateTime};
use std::io;
use std::sync::{Mutex, PoisonError};

const SCOPE: &str = "https://ai.azure.com/.default";
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A fixed "now", so expiry times are exact.
fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
}

fn minutes(n: i64) -> Duration {
    Duration::minutes(n)
}

fn cache(credential: &Arc<ScriptedCredential>) -> TokenCache {
    TokenCache::new(
        "azure".to_string(),
        credential.clone(),
        SCOPE.to_string(),
        TIMEOUT,
    )
}

/// The token text of a successful result.
fn secret(result: Result<AccessToken, SignInError>) -> String {
    result.expect("a token").token.secret().to_string()
}

#[actix_web::test]
async fn reuses_a_token_with_more_than_five_minutes_left() {
    let credential = ScriptedCredential::new(
        vec![
            Ok(("tok-1", now() + minutes(60))),
            Ok(("tok-2", now() + minutes(120))),
        ],
        Duration::ZERO,
    );
    let cache = cache(&credential);

    assert_eq!(secret(cache.token_at(now()).await), "tok-1");
    assert_eq!(secret(cache.token_at(now() + minutes(54)).await), "tok-1");
    assert_eq!(credential.calls(), 1);
}

#[actix_web::test]
async fn fetches_again_within_five_minutes_of_expiry() {
    let credential = ScriptedCredential::new(
        vec![
            Ok(("tok-1", now() + minutes(60))),
            Ok(("tok-2", now() + minutes(120))),
        ],
        Duration::ZERO,
    );
    let cache = cache(&credential);
    cache.token_at(now()).await.unwrap();

    // Exactly 5 minutes left is no longer "more than 5".
    assert_eq!(secret(cache.token_at(now() + minutes(55)).await), "tok-2");
    assert_eq!(credential.calls(), 2);
}

#[actix_web::test]
async fn uses_a_short_lived_token_once() {
    let credential = ScriptedCredential::new(
        vec![
            Ok(("short", now() + minutes(3))),
            Ok(("tok-2", now() + minutes(60))),
        ],
        Duration::ZERO,
    );
    let cache = cache(&credential);

    assert_eq!(secret(cache.token_at(now()).await), "short");
    assert_eq!(secret(cache.token_at(now()).await), "tok-2");
    assert_eq!(credential.calls(), 2);
}

#[actix_web::test]
async fn asks_for_its_scope() {
    let credential =
        ScriptedCredential::new(vec![Ok(("tok-1", now() + minutes(60)))], Duration::ZERO);
    let older_scope = "https://cognitiveservices.azure.com/.default";
    let cache = TokenCache::new(
        "azure".to_string(),
        credential.clone(),
        older_scope.to_string(),
        TIMEOUT,
    );

    cache.token_at(now()).await.unwrap();
    assert_eq!(credential.scopes(), [older_scope]);
}

#[actix_web::test]
async fn concurrent_requests_share_one_fetch() {
    // One answer only: a second fetch would find the script empty.
    let credential = ScriptedCredential::new(
        vec![Ok(("tok-1", now() + minutes(60)))],
        Duration::milliseconds(20),
    );
    let cache = cache(&credential);

    let (first, second) = futures::join!(cache.token_at(now()), cache.token_at(now()));
    assert_eq!(secret(first), "tok-1");
    assert_eq!(secret(second), "tok-1");
    assert_eq!(credential.calls(), 1);
}

#[actix_web::test]
async fn concurrent_requests_share_a_failed_fetch() {
    let credential =
        ScriptedCredential::new(vec![Err("az: not logged in")], Duration::milliseconds(20));
    let cache = cache(&credential);

    let (first, second) = futures::join!(cache.token_at(now()), cache.token_at(now()));
    assert!(matches!(first, Err(SignInError::Token(_))), "{first:?}");
    assert!(matches!(second, Err(SignInError::Token(_))), "{second:?}");
    assert_eq!(credential.calls(), 1);
}

#[actix_web::test]
async fn keeps_the_old_token_when_a_renewal_fails() {
    let credential = ScriptedCredential::new(
        vec![Ok(("tok-1", now() + minutes(60))), Err("IMDS is down")],
        Duration::ZERO,
    );
    let cache = cache(&credential);
    cache.token_at(now()).await.unwrap();

    let (_guard, logs) = capture_logs();
    assert_eq!(secret(cache.token_at(now() + minutes(57)).await), "tok-1");
    assert_eq!(credential.calls(), 2);
    let logs = text(&logs);
    assert!(logs.contains("IMDS is down"), "{logs}");
    assert!(logs.contains("azure"), "names the upstream: {logs}");
    assert!(!logs.contains("tok-1"), "{logs}");
}

#[actix_web::test]
async fn waiters_keep_the_old_token_when_a_renewal_fails() {
    // Both requests wait on one renewal, which fails while the old token
    // still has 3 minutes left: both get the old token.
    let credential = ScriptedCredential::new(
        vec![Ok(("tok-1", now() + minutes(60))), Err("IMDS is down")],
        Duration::milliseconds(20),
    );
    let cache = cache(&credential);
    cache.token_at(now()).await.unwrap();

    let later = now() + minutes(57);
    let (first, second) = futures::join!(cache.token_at(later), cache.token_at(later));
    assert_eq!(secret(first), "tok-1");
    assert_eq!(secret(second), "tok-1");
    assert_eq!(credential.calls(), 2);
}

#[actix_web::test]
async fn a_failed_fetch_without_a_token_is_an_error() {
    let credential =
        ScriptedCredential::new(vec![Err("az: please run 'az login'")], Duration::ZERO);

    let err = cache(&credential).token_at(now()).await.unwrap_err();
    let SignInError::Token(message) = err else {
        panic!("{err:?}");
    };
    assert!(message.contains("please run 'az login'"), "{message}");
}

#[actix_web::test]
async fn a_slow_fetch_times_out() {
    let credential = ScriptedCredential::new(
        vec![Ok(("late", now() + minutes(60)))],
        Duration::seconds(1),
    );
    let timeout = std::time::Duration::from_millis(50);
    let cache = TokenCache::new(
        "azure".to_string(),
        credential.clone(),
        SCOPE.to_string(),
        timeout,
    );

    let err = cache.token_at(now()).await.unwrap_err();
    assert_eq!(err, SignInError::Timeout(timeout));
    assert!(
        err.to_string().contains("UPSTREAM_CONNECT_TIMEOUT_SECS"),
        "{err}"
    );
}

#[actix_web::test]
async fn debug_shows_no_token() {
    let credential = ScriptedCredential::new(
        vec![Ok(("tok-secret-1", now() + minutes(60)))],
        Duration::ZERO,
    );
    let cache = cache(&credential);
    cache.token_at(now()).await.unwrap();

    let debug = format!("{cache:?}");
    assert!(!debug.contains("tok-secret-1"), "{debug}");
    assert!(debug.contains("expires_on"), "{debug}");
}

/// Everything logged on this thread until the guard drops, as plain text.
/// `actix_web::test` runs the test, and so the cache, on one thread.
fn capture_logs() -> (tracing::subscriber::DefaultGuard, Arc<Mutex<Vec<u8>>>) {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = SharedBuffer(Arc::clone(&buffer));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    (tracing::subscriber::set_default(subscriber), buffer)
}

fn text(buffer: &Mutex<Vec<u8>>) -> String {
    String::from_utf8_lossy(&buffer.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
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
