//! One upstream's token, kept in memory until shortly before it expires.

use super::{error_chain, SignInError};
use actix_web::rt::time::timeout;
use azure_core::credentials::{AccessToken, TokenCredential};
use azure_core::time::{Duration, OffsetDateTime};
use futures::lock::Mutex;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;
use tracing::warn;

/// A token is renewed once it has this little time left, so it never
/// expires while a request is on its way to the upstream.
const RENEW_BEFORE_EXPIRY: Duration = Duration::minutes(5);

/// Hands out one upstream's token, fetching a new one only when needed.
pub struct TokenCache {
    /// For log lines.
    upstream: String,
    credential: Arc<dyn TokenCredential>,
    scope: String,
    /// Bounds each fetch.
    timeout: std::time::Duration,
    /// An async mutex: it is held while a fetch is awaited, so only one
    /// fetch runs at a time and requests that arrive meanwhile wait for it.
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    token: Option<AccessToken>,
    /// When the last fetch failed, and why; cleared by a successful one.
    failure: Option<(Instant, SignInError)>,
}

impl TokenCache {
    pub fn new(
        upstream: String,
        credential: Arc<dyn TokenCredential>,
        scope: String,
        timeout: std::time::Duration,
    ) -> Self {
        Self {
            upstream,
            credential,
            scope,
            timeout,
            state: Mutex::new(State::default()),
        }
    }

    /// A token for the upstream, renewed when it has 5 minutes or less left.
    pub async fn token(&self) -> Result<AccessToken, SignInError> {
        self.token_at(OffsetDateTime::now_utc()).await
    }

    /// [`TokenCache::token`] with `now` passed in, so tests control time.
    pub(crate) async fn token_at(&self, now: OffsetDateTime) -> Result<AccessToken, SignInError> {
        let arrived = Instant::now();
        let mut state = self.state.lock().await;
        let cached = state.token.clone();
        if let Some(token) = cached
            .as_ref()
            .filter(|token| token.expires_on - now > RENEW_BEFORE_EXPIRY)
        {
            return Ok(token.clone());
        }
        // A token that's renewing but hasn't expired can still be used.
        let still_valid = cached.filter(|token| token.expires_on > now);
        if let Some((failed_at, error)) = &state.failure {
            if *failed_at >= arrived {
                // The fetch this request waited for failed; don't start another.
                return still_valid.ok_or_else(|| error.clone());
            }
        }
        match self.fetch().await {
            Ok(token) => {
                *state = State {
                    token: Some(token.clone()),
                    failure: None,
                };
                Ok(token)
            }
            Err(error) => {
                state.failure = Some((Instant::now(), error.clone()));
                let Some(token) = still_valid else {
                    return Err(error);
                };
                warn!(
                    upstream = %self.upstream,
                    error = %error,
                    "could not renew the upstream's token; using the current one until it expires"
                );
                Ok(token)
            }
        }
    }

    async fn fetch(&self) -> Result<AccessToken, SignInError> {
        let scopes = [self.scope.as_str()];
        match timeout(self.timeout, self.credential.get_token(&scopes, None)).await {
            Ok(Ok(token)) => Ok(token),
            Ok(Err(error)) => Err(SignInError::Token(error_chain(&error))),
            Err(_elapsed) => Err(SignInError::Timeout(self.timeout)),
        }
    }
}

/// Shows when the token expires, never the token. While a fetch holds the
/// state, the expiry shows as `None`.
impl fmt::Debug for TokenCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let expires_on = self
            .state
            .try_lock()
            .and_then(|state| state.token.as_ref().map(|token| token.expires_on));
        f.debug_struct("TokenCache")
            .field("upstream", &self.upstream)
            .field("scope", &self.scope)
            .field("expires_on", &expires_on)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
