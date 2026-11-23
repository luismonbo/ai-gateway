//! Gateway API keys: hashing, key IDs, roles and the `Authorization` header.
//!
//! Raw keys are hashed once at startup and then dropped. Everything after
//! that (logs, stats, `/v1/stats`) only ever sees a [`KeyId`].

use crate::app::AppState;
use crate::error::ApiError;
use actix_web::body::{EitherBody, MessageBody};
use actix_web::dev::{Payload, ServiceRequest, ServiceResponse};
use actix_web::http::header::AUTHORIZATION;
use actix_web::middleware::Next;
use actix_web::{web, FromRequest, HttpMessage, HttpRequest, ResponseError};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::future::{ready, Ready};
use tracing::warn;

const KEY_ID_PREFIX: &str = "key_";
/// Hex characters of the hash kept in a key ID (6 bytes).
const KEY_ID_HEX_LEN: usize = 12;

type KeyHash = [u8; 32];

fn sha256(raw: &str) -> KeyHash {
    Sha256::digest(raw.as_bytes()).into()
}

/// A stable, non-secret name for a key: `key_` plus the first 12 hex
/// characters of its SHA-256 hash, e.g. `key_ba7816bf8f01`.
///
/// It can only be built from a raw key or by parsing a well-formed ID, so a
/// `KeyId` is never a raw key in disguise.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, serde::Serialize)]
#[serde(transparent)]
pub struct KeyId(String);

impl KeyId {
    pub fn from_raw_key(raw: &str) -> KeyId {
        KeyId::from_hash(&sha256(raw))
    }

    fn from_hash(hash: &KeyHash) -> KeyId {
        let hex: String = hash[..KEY_ID_HEX_LEN / 2]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        KeyId(format!("{KEY_ID_PREFIX}{hex}"))
    }

    /// Accepts exactly `key_` followed by 12 lowercase hex characters.
    pub fn parse(s: &str) -> Option<KeyId> {
        let hex = s.strip_prefix(KEY_ID_PREFIX)?;
        let well_formed = hex.len() == KEY_ID_HEX_LEN
            && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        well_formed.then(|| KeyId(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Both roles can call every endpoint; admins also see everyone's stats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Admin,
}

/// Who made a request. Stored in the request's extensions by the auth
/// middleware; it never holds the raw key.
#[derive(Clone, Debug, PartialEq)]
pub struct Caller {
    pub key_id: KeyId,
    pub role: Role,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyStoreError {
    #[error("no API keys are configured")]
    NoKeys,
    #[error("key {0} is listed as both a user key and an admin key")]
    InBothLists(KeyId),
}

/// Maps the SHA-256 hash of each configured key to its caller.
pub struct KeyStore {
    callers: HashMap<KeyHash, Caller>,
}

impl KeyStore {
    pub fn new(user_keys: &[String], admin_keys: &[String]) -> Result<KeyStore, KeyStoreError> {
        let users = hash_keys(user_keys, Role::User);
        let admins = hash_keys(admin_keys, Role::Admin);

        if let Some((_, caller)) = admins.iter().find(|(hash, _)| users.contains_key(*hash)) {
            return Err(KeyStoreError::InBothLists(caller.key_id.clone()));
        }

        let callers: HashMap<KeyHash, Caller> = users.into_iter().chain(admins).collect();
        if callers.is_empty() {
            return Err(KeyStoreError::NoKeys);
        }
        Ok(KeyStore { callers })
    }

    /// Hashes the presented token and looks the hash up, so the time taken
    /// doesn't depend on how much of a guess matches a real key.
    pub fn authenticate(&self, token: &str) -> Option<Caller> {
        if token.is_empty() {
            return None;
        }
        self.callers.get(&sha256(token)).cloned()
    }

    pub fn count(&self, role: Role) -> usize {
        self.callers
            .values()
            .filter(|caller| caller.role == role)
            .count()
    }
}

/// Empty entries are skipped; a key repeated in one list counts once.
fn hash_keys(keys: &[String], role: Role) -> HashMap<KeyHash, Caller> {
    keys.iter()
        .filter(|key| !key.is_empty())
        .map(|key| {
            let hash = sha256(key);
            let caller = Caller {
                key_id: KeyId::from_hash(&hash),
                role,
            };
            (hash, caller)
        })
        .collect()
}

/// Shows key IDs and roles only: never a key, never a full hash.
impl fmt::Debug for KeyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut callers: Vec<(&str, Role)> = self
            .callers
            .values()
            .map(|caller| (caller.key_id.as_str(), caller.role))
            .collect();
        callers.sort_by_key(|(id, _)| *id);
        f.debug_struct("KeyStore")
            .field("callers", &callers)
            .finish()
    }
}

/// Extracts the token from an `Authorization: Bearer <token>` header value.
/// The scheme is case-insensitive and surrounding spaces are ignored.
pub fn bearer_token(header: Option<&str>) -> Option<&str> {
    let (scheme, token) = header?.trim().split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// Middleware for the `/v1` scope: lets a request through only with a valid
/// gateway key, and stores the [`Caller`] in the request's extensions.
///
/// A rejection is returned as `Ok(response)`, not `Err`, so the access log
/// (which skips `Err` results) still records the 401.
pub async fn require_api_key<B: MessageBody>(
    state: web::Data<AppState>,
    req: ServiceRequest,
    next: Next<B>,
) -> Result<ServiceResponse<EitherBody<B>>, actix_web::Error> {
    let header = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let caller = bearer_token(header).and_then(|token| state.config.keys.authenticate(token));

    match caller {
        Some(caller) => {
            req.extensions_mut().insert(caller);
            next.call(req)
                .await
                .map(ServiceResponse::map_into_left_body)
        }
        None => {
            // Never log the header or the token: a typo'd key is still a secret.
            warn!(path = %req.path(), "rejected request without a valid API key");
            let response = ApiError::invalid_api_key().error_response();
            Ok(req.into_response(response).map_into_right_body())
        }
    }
}

/// Lets handlers take `caller: Caller` as a parameter. Fails closed: without
/// the auth middleware's extension the request is unauthorized.
impl FromRequest for Caller {
    type Error = ApiError;
    type Future = Ready<Result<Caller, ApiError>>;

    fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        let caller = req.extensions().get::<Caller>().cloned();
        ready(caller.ok_or_else(ApiError::invalid_api_key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // SHA-256("abc") = ba7816bf8f01…, SHA-256("hello") = 2cf24dba5fb0…
    const ABC_ID: &str = "key_ba7816bf8f01";
    const HELLO_ID: &str = "key_2cf24dba5fb0";

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|key| key.to_string()).collect()
    }

    fn id(literal: &str) -> KeyId {
        KeyId::parse(literal).expect("test IDs are well-formed")
    }

    #[test]
    fn key_id_is_stable_and_shaped() {
        assert_eq!(KeyId::from_raw_key("abc").as_str(), ABC_ID);
        assert_eq!(KeyId::from_raw_key("hello").as_str(), HELLO_ID);
    }

    #[test]
    fn parse_accepts_only_key_ids() {
        assert!(KeyId::parse("key_ba7816bf8f01").is_some());
        for bad in [
            "key_BA7816BF8F01",
            "key_ba7816bf8f0",
            "key_ba7816bf8f01a",
            "key_ba7816bf8g01",
            "sk-123",
            "",
        ] {
            assert!(KeyId::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn authenticate_returns_the_callers_role() {
        // "abc" is listed twice: it counts once.
        let store = KeyStore::new(&keys(&["abc", "abc"]), &keys(&["hello"])).unwrap();
        assert_eq!(
            store.authenticate("abc"),
            Some(Caller {
                key_id: id(ABC_ID),
                role: Role::User
            })
        );
        assert_eq!(
            store.authenticate("hello"),
            Some(Caller {
                key_id: id(HELLO_ID),
                role: Role::Admin
            })
        );
        assert_eq!(store.count(Role::User), 1);
        assert_eq!(store.count(Role::Admin), 1);
    }

    #[test]
    fn unknown_or_empty_tokens_are_rejected() {
        let store = KeyStore::new(&keys(&["abc"]), &[]).unwrap();
        assert_eq!(store.authenticate("nope"), None);
        assert_eq!(store.authenticate(""), None);
        assert_eq!(store.authenticate("ABC"), None);
    }

    #[test]
    fn a_key_in_both_lists_is_rejected() {
        let result = KeyStore::new(&keys(&["hello", "abc"]), &keys(&["abc"]));
        assert_eq!(result.err(), Some(KeyStoreError::InBothLists(id(ABC_ID))));
    }

    #[test]
    fn no_keys_is_rejected() {
        assert_eq!(KeyStore::new(&[], &[]).err(), Some(KeyStoreError::NoKeys));
        assert_eq!(
            KeyStore::new(&keys(&[""]), &keys(&[""])).err(),
            Some(KeyStoreError::NoKeys)
        );
    }

    #[test]
    fn debug_shows_ids_not_keys() {
        let user = "user-secret-0123456789-abcdefghijklmnop";
        let admin = "admin-secret-0123456789-abcdefghijklmno";
        let store = KeyStore::new(&keys(&[user]), &keys(&[admin])).unwrap();
        let debug = format!("{store:?}");
        assert!(
            debug.contains(KeyId::from_raw_key(user).as_str()),
            "{debug}"
        );
        assert!(
            debug.contains(KeyId::from_raw_key(admin).as_str()),
            "{debug}"
        );
        assert!(!debug.contains(user), "{debug}");
        assert!(!debug.contains(admin), "{debug}");
    }

    #[test]
    fn bearer_token_parsing() {
        assert_eq!(bearer_token(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer_token(Some("bearer abc")), Some("abc"));
        assert_eq!(bearer_token(Some("Bearer   abc  ")), Some("abc"));
        for bad in [
            None,
            Some("Basic abc"),
            Some("Bearer "),
            Some("Bearer"),
            Some("abc"),
        ] {
            assert_eq!(bearer_token(bad), None, "{bad:?}");
        }
    }
}
