//! Signing in to Azure for upstreams with `<NAME>_AUTH`, and the secret
//! every upstream request carries: its key, an Azure token, or nothing.

pub mod cache;
pub mod credentials;
pub mod transport;

#[cfg(test)]
pub(crate) mod fake;

use crate::config::{Config, Upstream, UpstreamKey};
use azure_core::credentials::{Secret, TokenCredential};
use azure_core::http::HttpClient;
use azure_identity::Executor;
use cache::TokenCache;
use std::error::Error;
use std::sync::Arc;

/// Why an upstream couldn't sign in. `Clone`, so every request that waited
/// for one failed fetch gets its error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignInError {
    /// The upstream's credential couldn't be built, at startup.
    #[error("upstream {upstream}: can't sign in with {method}: {reason}")]
    Setup {
        upstream: String,
        method: String,
        reason: String,
    },
    #[error("could not get a token: {0}")]
    Token(String),
    #[error("no token within {0:?}; raise UPSTREAM_CONNECT_TIMEOUT_SECS if signing in is slow")]
    Timeout(std::time::Duration),
}

/// What an upstream request sends as `Authorization: Bearer …`. Both kinds
/// of secret redact their `Debug` output.
#[derive(Debug, Clone)]
pub enum Bearer {
    /// The upstream's own key, from `<NAME>_API_KEY`.
    Key(UpstreamKey),
    /// An Azure token, for an upstream with `<NAME>_AUTH`.
    Token(Secret),
}

impl Bearer {
    /// The secret itself, for the `Authorization` header and for redaction.
    pub fn expose(&self) -> &str {
        match self {
            Bearer::Key(key) => key.expose(),
            Bearer::Token(token) => token.secret(),
        }
    }
}

/// A token cache for each upstream with `<NAME>_AUTH`, in `UPSTREAMS` order.
#[derive(Debug)]
pub struct SignIn {
    caches: Vec<(String, TokenCache)>,
}

impl SignIn {
    /// One credential per upstream with `<NAME>_AUTH`, built at startup.
    /// Fails, naming the upstream, when this environment can't support one.
    pub fn from_config(
        config: &Config,
        transport: Arc<dyn HttpClient>,
        executor: Arc<dyn Executor>,
    ) -> Result<SignIn, SignInError> {
        let credentials = config
            .upstreams
            .iter()
            .filter_map(|upstream| Some((upstream, upstream.sign_in.as_ref()?)))
            .map(|(upstream, sign_in)| {
                credentials::credential(
                    &sign_in.method,
                    Arc::clone(&transport),
                    Arc::clone(&executor),
                )
                .map(|credential| (upstream.name.clone(), credential))
                .map_err(|e| SignInError::Setup {
                    upstream: upstream.name.clone(),
                    method: sign_in.method.to_string(),
                    reason: error_chain(&e),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(SignIn::with_credentials(config, credentials))
    }

    /// Caches over the given credentials, for the upstreams with
    /// `<NAME>_AUTH` they name. Tests pass fakes here.
    pub fn with_credentials(
        config: &Config,
        credentials: Vec<(String, Arc<dyn TokenCredential>)>,
    ) -> SignIn {
        let caches = config
            .upstreams
            .iter()
            .filter_map(|upstream| {
                let sign_in = upstream.sign_in.as_ref()?;
                let (_, credential) = credentials
                    .iter()
                    .find(|(name, _)| *name == upstream.name)?;
                let cache = TokenCache::new(
                    upstream.name.clone(),
                    Arc::clone(credential),
                    sign_in.scope.clone(),
                    config.connect_timeout,
                );
                Some((upstream.name.clone(), cache))
            })
            .collect();
        SignIn { caches }
    }

    /// The secret `upstream`'s request carries: a token when it signs in,
    /// otherwise its key, if it has one.
    pub async fn bearer(&self, upstream: &Upstream) -> Result<Option<Bearer>, SignInError> {
        if upstream.sign_in.is_none() {
            return Ok(upstream.api_key.clone().map(Bearer::Key));
        }
        let cache = self
            .cache(&upstream.name)
            .ok_or_else(|| SignInError::Token("no credential for this upstream".to_string()))?;
        let token = cache.token().await?;
        Ok(Some(Bearer::Token(token.token)))
    }

    fn cache(&self, upstream: &str) -> Option<&TokenCache> {
        self.caches
            .iter()
            .find(|(name, _)| name == upstream)
            .map(|(_, cache)| cache)
    }
}

/// `error` and its sources, joined with ": ". azure_core's errors also show
/// their source as their own text, so a repeat of the line before is dropped.
pub(crate) fn error_chain(error: &(dyn Error + 'static)) -> String {
    // `|&e|` copies the inner reference out, so each source borrows from the
    // original error rather than from the closure's argument.
    let mut parts: Vec<String> = std::iter::successors(Some(error), |&e| e.source())
        .map(ToString::to_string)
        .collect();
    parts.dedup();
    parts.join(": ")
}

#[cfg(test)]
mod tests;
