//! Configuration, read once at startup from environment variables.
//!
//! Every rule is checked before the server starts; the first broken rule
//! stops startup with a message naming the variable. Error and warning texts
//! never contain a key, only its key ID.

mod azure;
mod upstreams;

pub use azure::{AzureSignIn, SignInMethod, DEFAULT_TOKEN_SCOPE};
pub use upstreams::{FallbackTarget, Upstream, UpstreamKey};

use crate::auth::{KeyId, KeyStore, KeyStoreError, Role};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;
/// 10 MiB.
const DEFAULT_MAX_REQUEST_BYTES: usize = 10 * 1024 * 1024;
const DEFAULT_RATE_LIMIT_RPM: u32 = 60;
const DEFAULT_STATS_PATH: &str = "stats.json";
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
    /// In `UPSTREAMS` order.
    pub upstreams: Vec<Upstream>,
    pub default_upstream: Option<String>,
    pub connect_timeout: Duration,
    /// Longest silence allowed while waiting for upstream data.
    pub upstream_timeout: Duration,
    pub max_request_bytes: usize,
    /// Requests per minute per key; also the burst size.
    pub rate_limit_rpm: u32,
    pub stats_path: PathBuf,
    /// How often stats are saved, besides at shutdown.
    pub stats_flush_interval: Duration,
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
        let (upstreams, upstream_warnings) = upstreams::upstreams(&env)?;
        let default_upstream = upstreams::default_upstream(&env, &upstreams)?;
        let connect_timeout = seconds(&env, "UPSTREAM_CONNECT_TIMEOUT_SECS", 10)?;
        let upstream_timeout = seconds(&env, "UPSTREAM_TIMEOUT_SECS", 300)?;
        let max_request_bytes = positive(&env, "MAX_REQUEST_BYTES", DEFAULT_MAX_REQUEST_BYTES)?;
        let rate_limit_rpm = positive(&env, "RATE_LIMIT_RPM", DEFAULT_RATE_LIMIT_RPM)?;
        let stats_path = PathBuf::from(
            env.get("STATS_PATH")
                .unwrap_or_else(|| DEFAULT_STATS_PATH.to_string()),
        );
        let stats_flush_interval = seconds(&env, "STATS_FLUSH_SECS", 60)?;

        let config = Config {
            host,
            port,
            log_format,
            keys,
            upstreams,
            default_upstream,
            connect_timeout,
            upstream_timeout,
            max_request_bytes,
            rate_limit_rpm,
            stats_path,
            stats_flush_interval,
        };
        let warnings = [key_warnings, upstream_warnings, legacy_warnings(&env)].concat();
        Ok((config, warnings))
    }

    pub fn upstream(&self, name: &str) -> Option<&Upstream> {
        self.upstreams.iter().find(|upstream| upstream.name == name)
    }

    pub fn upstream_names(&self) -> Vec<&str> {
        self.upstreams
            .iter()
            .map(|upstream| upstream.name.as_str())
            .collect()
    }

    /// What the gateway will do, for the startup log. Never contains a key.
    pub fn summary_lines(&self) -> Vec<String> {
        let address = format!("bind address: {}:{}", self.host, self.port);
        let upstreams = self.upstreams.iter().map(|upstream| {
            format!(
                "upstream {}: {} (auth: {}, fallback: {})",
                upstream.name,
                upstream.base_url,
                auth_label(upstream),
                upstream
                    .fallback
                    .as_ref()
                    .map_or_else(|| "none".to_string(), ToString::to_string),
            )
        });
        let default_upstream = format!(
            "default upstream: {}",
            self.default_upstream
                .as_deref()
                .unwrap_or("none (models need an upstream prefix)")
        );
        let limits = format!(
            "upstream timeouts: connect {} s, idle {} s; max request body: {} bytes",
            self.connect_timeout.as_secs(),
            self.upstream_timeout.as_secs(),
            self.max_request_bytes
        );
        let rate_limit = format!(
            "rate limit: {} requests per minute per key",
            self.rate_limit_rpm
        );
        let stats = format!(
            "stats: saved to {} every {} s and at shutdown",
            self.stats_path.display(),
            self.stats_flush_interval.as_secs()
        );
        let keys = format!(
            "API keys: {} user, {} admin",
            self.keys.count(Role::User),
            self.keys.count(Role::Admin)
        );
        std::iter::once(address)
            .chain(upstreams)
            .chain([default_upstream, limits, rate_limit, stats, keys])
            .collect()
    }
}

/// How an upstream authenticates, for the summary. Never contains a key.
fn auth_label(upstream: &Upstream) -> String {
    match (&upstream.sign_in, &upstream.api_key) {
        (Some(sign_in), _) => sign_in.method.to_string(),
        (None, Some(_)) => "api key".to_string(),
        (None, None) => "none".to_string(),
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

/// A whole number of seconds greater than 0.
fn seconds(env: &Env, name: &str, default: u64) -> Result<Duration, ConfigError> {
    positive(env, name, default).map(Duration::from_secs)
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
mod tests;
