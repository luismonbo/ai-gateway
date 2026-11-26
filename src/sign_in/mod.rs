//! Signing in to Azure for upstreams with `<NAME>_AUTH`.

pub mod cache;
pub mod transport;

#[cfg(test)]
pub(crate) mod fake;

use std::error::Error;

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
