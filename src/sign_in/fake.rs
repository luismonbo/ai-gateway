//! A token source that answers from a script, for unit tests.

use azure_core::credentials::{AccessToken, TokenCredential, TokenRequestOptions};
use azure_core::error::{Error, ErrorKind};
use azure_core::time::{Duration, OffsetDateTime};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// One scripted answer: a token and its expiry, or an error message.
type Answer = Result<(&'static str, OffsetDateTime), &'static str>;

/// Each call waits `delay`, then takes the next answer from the script.
#[derive(Debug)]
pub(crate) struct ScriptedCredential {
    script: Mutex<VecDeque<Answer>>,
    delay: Duration,
    calls: AtomicUsize,
    scopes: Mutex<Vec<String>>,
}

impl ScriptedCredential {
    pub fn new(script: Vec<Answer>, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            delay,
            calls: AtomicUsize::new(0),
            scopes: Mutex::new(Vec::new()),
        })
    }

    /// How many tokens were asked for.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Every scope asked for, in order.
    pub fn scopes(&self) -> Vec<String> {
        self.scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl TokenCredential for ScriptedCredential {
    async fn get_token(
        &self,
        scopes: &[&str],
        _options: Option<TokenRequestOptions<'_>>,
    ) -> azure_core::Result<AccessToken> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.scopes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(scopes.iter().map(ToString::to_string));
        if self.delay.is_positive() {
            actix_web::rt::time::sleep(self.delay.unsigned_abs()).await;
        }
        let answer = self
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .expect("the script has an answer for every call");
        answer
            .map(|(token, expires_on)| AccessToken::new(token, expires_on))
            .map_err(|message| Error::with_message(ErrorKind::Credential, message))
    }
}
