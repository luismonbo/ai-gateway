//! Configuration, read once at startup from environment variables.
//!
//! Every rule is checked before the server starts; the first broken rule
//! stops startup with a message naming the variable. Error and warning texts
//! never contain a key, only its key ID.

use crate::auth::{KeyId, KeyStore, KeyStoreError};
use std::fmt;
use std::str::FromStr;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;
const USER_KEYS: &str = "GATEWAY_API_KEYS";
const ADMIN_KEYS: &str = "ADMIN_API_KEYS";
/// Keys shorter than this get a startup warning.
const MIN_KEY_CHARS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub log_format: LogFormat,
    pub keys: KeyStore,
}

/// A configuration rule that was broken, and the variable that broke it.
#[derive(Debug, PartialEq, thiserror::Error)]
#[error("{variable}: {message}")]
pub struct ConfigError {
    pub variable: String,
    pub message: String,
}

impl ConfigError {
    fn new(variable: &str, message: impl Into<String>) -> Self {
        Self {
            variable: variable.to_string(),
            message: message.into(),
        }
    }
}

impl Config {
    /// Reads the process environment. Returns the config and its warnings.
    pub fn from_env() -> Result<(Config, Vec<String>), ConfigError> {
        Config::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads variables through `lookup`, so tests can pass a fixed set.
    pub fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<(Config, Vec<String>), ConfigError> {
        let env = Env { lookup: &lookup };

        let host = env.get("HOST").unwrap_or_else(|| DEFAULT_HOST.to_string());
        let port = positive(&env, "PORT", DEFAULT_PORT)?;
        let log_format = log_format(&env)?;
        let (keys, key_warnings) = key_store(&env)?;

        let config = Config {
            host,
            port,
            log_format,
            keys,
        };
        let warnings = [key_warnings, legacy_warnings(&env)].concat();
        Ok((config, warnings))
    }
}

/// Variable lookup where a blank value counts as unset.
struct Env<'a> {
    lookup: &'a dyn Fn(&str) -> Option<String>,
}

impl Env<'_> {
    /// The trimmed value, or `None` when unset or blank.
    fn get(&self, name: &str) -> Option<String> {
        (self.lookup)(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    /// A comma-separated list: entries trimmed, empty entries dropped.
    fn list(&self, name: &str) -> Vec<String> {
        self.get(name)
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A whole number greater than 0, or `default` when the variable is unset.
fn positive<T>(env: &Env, name: &str, default: T) -> Result<T, ConfigError>
where
    T: FromStr + PartialOrd + From<u8>,
    T::Err: fmt::Display,
{
    let Some(raw) = env.get(name) else {
        return Ok(default);
    };
    let value = raw.parse::<T>().map_err(|e| {
        ConfigError::new(
            name,
            format!("{raw:?} is not a whole number greater than 0 ({e})"),
        )
    })?;
    if value > T::from(0) {
        Ok(value)
    } else {
        Err(ConfigError::new(
            name,
            format!("{raw:?} must be greater than 0"),
        ))
    }
}

fn log_format(env: &Env) -> Result<LogFormat, ConfigError> {
    match env
        .get("LOG_FORMAT")
        .map(|v| v.to_ascii_lowercase())
        .as_deref()
    {
        None | Some("text") => Ok(LogFormat::Text),
        Some("json") => Ok(LogFormat::Json),
        Some(other) => Err(ConfigError::new(
            "LOG_FORMAT",
            format!("{other:?} is not supported; use \"text\" or \"json\""),
        )),
    }
}

fn key_store(env: &Env) -> Result<(KeyStore, Vec<String>), ConfigError> {
    let user_keys = env.list(USER_KEYS);
    let admin_keys = env.list(ADMIN_KEYS);
    let warnings = [
        short_key_warnings(USER_KEYS, &user_keys),
        short_key_warnings(ADMIN_KEYS, &admin_keys),
    ]
    .concat();

    let keys = KeyStore::new(&user_keys, &admin_keys).map_err(|e| match e {
        KeyStoreError::NoKeys => ConfigError::new(
            USER_KEYS,
            format!("at least one API key is required in {USER_KEYS} or {ADMIN_KEYS}"),
        ),
        KeyStoreError::InBothLists(key_id) => ConfigError::new(
            ADMIN_KEYS,
            format!("key {key_id} is listed in both {USER_KEYS} and {ADMIN_KEYS}"),
        ),
    })?;
    Ok((keys, warnings))
}

fn short_key_warnings(variable: &str, keys: &[String]) -> Vec<String> {
    keys.iter()
        .filter(|key| key.chars().count() < MIN_KEY_CHARS)
        .map(|key| {
            format!(
                "{variable}: key {} is shorter than {MIN_KEY_CHARS} characters; \
                 generate one with `openssl rand -hex 32`",
                KeyId::from_raw_key(key)
            )
        })
        .collect()
}

fn legacy_warnings(env: &Env) -> Vec<String> {
    env.get("AI_PROVIDER")
        .map(|_| {
            "AI_PROVIDER: no longer used and ignored; clients now pick an upstream \
             with a model prefix such as ollama/<model> (see UPSTREAMS)"
                .to_string()
        })
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;

    const LONG: &str = "0123456789abcdef0123456789abcdef"; // 32 characters
    const LONG2: &str = "fedcba9876543210fedcba9876543210";

    fn lookup<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    fn load(vars: &[(&str, &str)]) -> Result<(Config, Vec<String>), ConfigError> {
        Config::from_lookup(lookup(vars))
    }

    #[test]
    fn defaults_apply_with_only_a_key() {
        let (config, warnings) = load(&[("GATEWAY_API_KEYS", LONG)]).unwrap();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 8080);
        assert_eq!(config.log_format, LogFormat::Text);
        assert_eq!(config.keys.count(Role::User), 1);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn missing_keys_is_an_error() {
        let err = load(&[]).unwrap_err();
        assert_eq!(err.variable, "GATEWAY_API_KEYS");
    }

    #[test]
    fn admin_keys_alone_are_enough() {
        let (config, _) = load(&[("ADMIN_API_KEYS", LONG)]).unwrap();
        assert_eq!(config.keys.count(Role::Admin), 1);
        assert_eq!(config.keys.count(Role::User), 0);
    }

    #[test]
    fn key_in_both_lists_names_the_key_id_only() {
        let err = load(&[("GATEWAY_API_KEYS", LONG), ("ADMIN_API_KEYS", LONG)]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains(KeyId::from_raw_key(LONG).as_str()), "{text}");
        assert!(!text.contains(LONG), "{text}");
    }

    #[test]
    fn short_key_warns_by_key_id() {
        let (_, warnings) = load(&[("GATEWAY_API_KEYS", "short-key")]).unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("GATEWAY_API_KEYS"), "{warnings:?}");
        assert!(warnings[0].contains(KeyId::from_raw_key("short-key").as_str()));
        assert!(!warnings[0].contains("short-key"), "{warnings:?}");

        // 31 characters is one short of the limit.
        let (_, warnings) = load(&[("ADMIN_API_KEYS", &LONG[..31])]).unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("ADMIN_API_KEYS"), "{warnings:?}");
    }

    #[test]
    fn port_must_be_a_nonzero_u16() {
        for bad in ["0", "65536", "http", "-1"] {
            let err = load(&[("GATEWAY_API_KEYS", LONG), ("PORT", bad)]).unwrap_err();
            assert_eq!(err.variable, "PORT", "{bad}");
        }
        let (config, _) = load(&[("GATEWAY_API_KEYS", LONG), ("PORT", "9000")]).unwrap();
        assert_eq!(config.port, 9000);
    }

    #[test]
    fn log_format_is_text_or_json() {
        let (config, _) = load(&[("GATEWAY_API_KEYS", LONG), ("LOG_FORMAT", "JSON")]).unwrap();
        assert_eq!(config.log_format, LogFormat::Json);
        let (config, _) = load(&[("GATEWAY_API_KEYS", LONG), ("LOG_FORMAT", "text")]).unwrap();
        assert_eq!(config.log_format, LogFormat::Text);
        let err = load(&[("GATEWAY_API_KEYS", LONG), ("LOG_FORMAT", "yaml")]).unwrap_err();
        assert_eq!(err.variable, "LOG_FORMAT");
    }

    #[test]
    fn ai_provider_is_only_a_warning() {
        let (_, warnings) = load(&[("GATEWAY_API_KEYS", LONG), ("AI_PROVIDER", "ollama")]).unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("AI_PROVIDER"), "{warnings:?}");
    }

    #[test]
    fn blank_values_count_as_unset() {
        let vars = [
            ("GATEWAY_API_KEYS", LONG),
            ("PORT", "  "),
            ("HOST", ""),
            ("LOG_FORMAT", " "),
            ("AI_PROVIDER", " "),
        ];
        let (config, warnings) = load(&vars).unwrap();
        assert_eq!(config.port, 8080);
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.log_format, LogFormat::Text);
        assert!(warnings.is_empty(), "{warnings:?}");

        let err = load(&[("GATEWAY_API_KEYS", "   ")]).unwrap_err();
        assert_eq!(err.variable, "GATEWAY_API_KEYS");
    }

    #[test]
    fn keys_are_trimmed_and_empty_entries_dropped() {
        let list = format!(" {LONG}, , {LONG2} ,");
        let (config, warnings) = load(&[("GATEWAY_API_KEYS", list.as_str())]).unwrap();
        assert_eq!(config.keys.count(Role::User), 2);
        assert!(config.keys.authenticate(LONG).is_some());
        assert!(config.keys.authenticate(LONG2).is_some());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn debug_never_shows_keys() {
        let (config, _) = load(&[("GATEWAY_API_KEYS", LONG), ("ADMIN_API_KEYS", LONG2)]).unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains(LONG), "{debug}");
        assert!(!debug.contains(LONG2), "{debug}");
    }
}
