//! The `UPSTREAMS` list and each upstream's `<NAME>_*` variables.
//!
//! Error messages never repeat a base URL: a mistyped one may hold a secret.

use super::{ConfigError, Env};
use reqwest::Url;
use std::fmt;

const UPSTREAMS: &str = "UPSTREAMS";

/// An OpenAI-compatible backend the gateway forwards requests to.
#[derive(Debug, Clone)]
pub struct Upstream {
    pub name: String,
    /// Includes the version path, e.g. `http://localhost:11434/v1`, with no
    /// trailing `/`.
    pub base_url: String,
    pub api_key: Option<UpstreamKey>,
    pub fallback: Option<FallbackTarget>,
}

impl Upstream {
    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    pub fn models_url(&self) -> String {
        format!("{}/models", self.base_url)
    }
}

/// The key sent upstream as `Authorization: Bearer …`. Its `Debug` output is
/// redacted; the value is only reachable through [`UpstreamKey::expose`].
#[derive(Clone)]
pub struct UpstreamKey(String);

impl UpstreamKey {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for UpstreamKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UpstreamKey(<redacted>)")
    }
}

/// Where a request goes when its upstream fails, from `<NAME>_FALLBACK`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackTarget {
    pub upstream: String,
    pub model: String,
}

impl fmt::Display for FallbackTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.upstream, self.model)
    }
}

/// Reads `UPSTREAMS` and every upstream's variables, in `UPSTREAMS` order.
/// Returns the upstreams and their warnings.
pub(super) fn upstreams(env: &Env) -> Result<(Vec<Upstream>, Vec<String>), ConfigError> {
    let names = upstream_names(env)?;
    let parsed = names
        .iter()
        .map(|name| upstream(env, name, &names))
        .collect::<Result<Vec<_>, _>>()?;
    let (upstreams, warnings): (Vec<Upstream>, Vec<Option<String>>) = parsed.into_iter().unzip();
    Ok((upstreams, warnings.into_iter().flatten().collect()))
}

/// `DEFAULT_UPSTREAM`, which must be one of the configured names.
pub(super) fn default_upstream(
    env: &Env,
    upstreams: &[Upstream],
) -> Result<Option<String>, ConfigError> {
    let Some(name) = env.get("DEFAULT_UPSTREAM") else {
        return Ok(None);
    };
    if upstreams.iter().any(|upstream| upstream.name == name) {
        Ok(Some(name))
    } else {
        Err(ConfigError::new(
            "DEFAULT_UPSTREAM",
            format!("{name:?} is not in UPSTREAMS ({})", joined(upstreams)),
        ))
    }
}

fn upstream_names(env: &Env) -> Result<Vec<String>, ConfigError> {
    let names = env.list(UPSTREAMS);
    if names.is_empty() {
        return Err(ConfigError::new(
            UPSTREAMS,
            "at least one upstream is required, e.g. UPSTREAMS=ollama,openai, \
             each with a <NAME>_BASE_URL such as OLLAMA_BASE_URL=http://localhost:11434/v1",
        ));
    }
    if let Some(bad) = names.iter().find(|name| !is_valid_name(name)) {
        return Err(ConfigError::new(
            UPSTREAMS,
            format!(
                "{bad:?} is not a valid upstream name; use only lowercase letters, digits and _"
            ),
        ));
    }
    // A name is repeated when it already appears earlier in the list.
    let repeated = names
        .iter()
        .enumerate()
        .find(|(i, name)| names[..*i].contains(name));
    if let Some((_, repeated)) = repeated {
        return Err(ConfigError::new(
            UPSTREAMS,
            format!("{repeated:?} is listed more than once"),
        ));
    }
    Ok(names)
}

fn is_valid_name(name: &str) -> bool {
    name.bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// One upstream's variables, and a warning if its URL has no path.
fn upstream(
    env: &Env,
    name: &str,
    names: &[String],
) -> Result<(Upstream, Option<String>), ConfigError> {
    let prefix = name.to_ascii_uppercase();
    let url_variable = format!("{prefix}_BASE_URL");
    let key_variable = format!("{prefix}_API_KEY");

    let url = base_url(env, &url_variable, &key_variable)?;
    let warning = (url.path() == "/").then(|| {
        format!(
            "{url_variable}: the URL has no path; OpenAI-compatible APIs usually need \
             the version path, e.g. https://api.openai.com/v1 or http://localhost:11434/v1"
        )
    });
    let upstream = Upstream {
        name: name.to_string(),
        base_url: url.as_str().trim_end_matches('/').to_string(),
        api_key: env.get(&key_variable).map(UpstreamKey),
        fallback: fallback(env, &format!("{prefix}_FALLBACK"), names)?,
    };
    Ok((upstream, warning))
}

/// An http(s) URL with no credentials, query string or fragment.
fn base_url(env: &Env, variable: &str, key_variable: &str) -> Result<Url, ConfigError> {
    const EXPECTED: &str = "must be an http:// or https:// URL including the version path, \
                            e.g. http://localhost:11434/v1";
    let raw = env
        .get(variable)
        .ok_or_else(|| ConfigError::new(variable, format!("required; it {EXPECTED}")))?;
    // The parse error describes the problem without repeating the input.
    let url = Url::parse(&raw)
        .map_err(|e| ConfigError::new(variable, format!("not a valid URL ({e}); it {EXPECTED}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError::new(variable, EXPECTED));
    }
    let has_secret_parts = !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some();
    if has_secret_parts {
        return Err(ConfigError::new(
            variable,
            format!(
                "must not contain credentials, a query string or a fragment; \
                 put the upstream's API key in {key_variable}"
            ),
        ));
    }
    Ok(url)
}

/// `upstream/model`, where `upstream` is one of `names`.
fn fallback(
    env: &Env,
    variable: &str,
    names: &[String],
) -> Result<Option<FallbackTarget>, ConfigError> {
    let Some(raw) = env.get(variable) else {
        return Ok(None);
    };
    let (upstream, model) = raw.split_once('/').ok_or_else(|| {
        ConfigError::new(
            variable,
            format!("{raw:?} must be <upstream>/<model>, e.g. openai/gpt-6-terra"),
        )
    })?;
    if !names.iter().any(|name| name == upstream) {
        return Err(ConfigError::new(
            variable,
            format!("{upstream:?} is not in UPSTREAMS ({})", names.join(", ")),
        ));
    }
    if model.is_empty() {
        return Err(ConfigError::new(
            variable,
            format!("{raw:?} has no model after the '/'"),
        ));
    }
    Ok(Some(FallbackTarget {
        upstream: upstream.to_string(),
        model: model.to_string(),
    }))
}

fn joined(upstreams: &[Upstream]) -> String {
    upstreams
        .iter()
        .map(|upstream| upstream.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::{lookup, LONG};
    use crate::config::{Config, ConfigError};

    const OLLAMA_URL: &str = "http://localhost:11434/v1";
    const OPENAI_URL: &str = "https://api.openai.com/v1";

    /// A valid gateway key plus `vars`, which hold every upstream variable.
    fn load(vars: &[(&str, &str)]) -> Result<(Config, Vec<String>), ConfigError> {
        let all: Vec<(&str, &str)> = [("GATEWAY_API_KEYS", LONG)]
            .into_iter()
            .chain(vars.iter().copied())
            .collect();
        Config::from_lookup(lookup(&all))
    }

    fn ollama_with_url(url: &str) -> Result<(Config, Vec<String>), ConfigError> {
        load(&[("UPSTREAMS", "ollama"), ("OLLAMA_BASE_URL", url)])
    }

    #[test]
    fn parses_upstreams_with_fallback() {
        let (config, warnings) = load(&[
            ("UPSTREAMS", "ollama,openai"),
            ("OLLAMA_BASE_URL", OLLAMA_URL),
            ("OPENAI_BASE_URL", OPENAI_URL),
            ("OPENAI_API_KEY", "sk-test"),
            ("OLLAMA_FALLBACK", "openai/gpt-6-terra"),
        ])
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.upstream_names(), ["ollama", "openai"]);

        let ollama = config.upstream("ollama").unwrap();
        let fallback = FallbackTarget {
            upstream: "openai".to_string(),
            model: "gpt-6-terra".to_string(),
        };
        assert_eq!(ollama.fallback.as_ref(), Some(&fallback));
        assert_eq!(fallback.to_string(), "openai/gpt-6-terra");
        assert_eq!(
            ollama.chat_url(),
            "http://localhost:11434/v1/chat/completions"
        );
        assert!(ollama.api_key.is_none());

        let openai = config.upstream("openai").unwrap();
        assert_eq!(openai.models_url(), "https://api.openai.com/v1/models");
        assert_eq!(
            openai.api_key.as_ref().map(UpstreamKey::expose),
            Some("sk-test")
        );
        assert!(openai.fallback.is_none());
        assert!(config.upstream("nope").is_none());
    }

    #[test]
    fn upstreams_is_required() {
        for value in ["", " , ,"] {
            let err = load(&[("UPSTREAMS", value)]).unwrap_err();
            assert_eq!(err.variable, "UPSTREAMS", "{value:?}");
        }
    }

    #[test]
    fn upstream_names_are_lowercase_alnum_underscore() {
        for bad in ["Ollama", "my-vllm", "a b"] {
            let err = load(&[("UPSTREAMS", bad)]).unwrap_err();
            assert_eq!(err.variable, "UPSTREAMS", "{bad}");
        }
        let (config, _) = load(&[
            ("UPSTREAMS", "my_vllm2"),
            ("MY_VLLM2_BASE_URL", "http://127.0.0.1:8000/v1"),
        ])
        .unwrap();
        assert_eq!(config.upstream_names(), ["my_vllm2"]);
    }

    #[test]
    fn duplicate_upstreams_are_rejected() {
        let err = load(&[
            ("UPSTREAMS", "ollama, ollama"),
            ("OLLAMA_BASE_URL", OLLAMA_URL),
        ])
        .unwrap_err();
        assert_eq!(err.variable, "UPSTREAMS");
        assert!(err.message.contains("ollama"), "{err}");
    }

    #[test]
    fn base_url_is_required_and_http() {
        let err = load(&[("UPSTREAMS", "ollama")]).unwrap_err();
        assert_eq!(err.variable, "OLLAMA_BASE_URL");

        for bad in ["ftp://x", "localhost:11434", "not a url"] {
            let err = ollama_with_url(bad).unwrap_err();
            assert_eq!(err.variable, "OLLAMA_BASE_URL", "{bad}");
        }
        // The messages are fixed text (their example URL would match a plain
        // "localhost:11434"), so look for a marker instead of the whole input.
        for bad in ["ftp://secret-host/v1", "secret-host:11434", "secret host"] {
            let err = ollama_with_url(bad).unwrap_err();
            assert!(!err.message.contains("secret"), "echoes the URL: {err}");
        }
    }

    #[test]
    fn base_url_with_credentials_query_or_fragment_is_rejected() {
        for bad in [
            "http://user:secret-pass@h/v1",
            "http://token@h/v1",
            "http://h/v1?api_key=secret",
            "http://h/v1#secret",
        ] {
            let err = ollama_with_url(bad).unwrap_err();
            assert_eq!(err.variable, "OLLAMA_BASE_URL", "{bad}");
            assert!(err.message.contains("OLLAMA_API_KEY"), "{err}");
            assert!(!err.message.contains("secret"), "echoes the URL: {err}");
        }
    }

    #[test]
    fn trailing_slash_is_trimmed() {
        let (config, _) = ollama_with_url("http://h/v1/").unwrap();
        let ollama = config.upstream("ollama").unwrap();
        assert_eq!(ollama.base_url, "http://h/v1");
        assert_eq!(ollama.chat_url(), "http://h/v1/chat/completions");
    }

    #[test]
    fn url_without_a_path_is_only_a_warning() {
        let (config, warnings) = ollama_with_url("http://localhost:11434").unwrap();
        assert_eq!(
            config.upstream("ollama").unwrap().chat_url(),
            "http://localhost:11434/chat/completions"
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("OLLAMA_BASE_URL"), "{warnings:?}");
    }

    #[test]
    fn default_upstream_must_exist() {
        let vars = [("UPSTREAMS", "ollama"), ("OLLAMA_BASE_URL", OLLAMA_URL)];
        let with = |default: &'static str| {
            let all: Vec<_> = vars
                .iter()
                .copied()
                .chain([("DEFAULT_UPSTREAM", default)])
                .collect();
            load(&all)
        };

        let err = with("openai").unwrap_err();
        assert_eq!(err.variable, "DEFAULT_UPSTREAM");

        let (config, _) = with("ollama").unwrap();
        assert_eq!(config.default_upstream.as_deref(), Some("ollama"));

        let (config, _) = with("").unwrap();
        assert_eq!(config.default_upstream, None);
    }

    #[test]
    fn fallback_must_name_upstream_and_model() {
        for bad in ["openai", "nope/x", "openai/", "/gpt-6-terra"] {
            let err = load(&[
                ("UPSTREAMS", "ollama,openai"),
                ("OLLAMA_BASE_URL", OLLAMA_URL),
                ("OPENAI_BASE_URL", OPENAI_URL),
                ("OLLAMA_FALLBACK", bad),
            ])
            .unwrap_err();
            assert_eq!(err.variable, "OLLAMA_FALLBACK", "{bad}");
        }
    }

    #[test]
    fn upstream_key_is_redacted_in_debug_and_summary() {
        let secret = "sk-test-secret-value-0123456789";
        let (config, _) = load(&[
            ("UPSTREAMS", "openai"),
            ("OPENAI_BASE_URL", OPENAI_URL),
            ("OPENAI_API_KEY", secret),
        ])
        .unwrap();

        let debug = format!("{config:?}");
        assert!(!debug.contains(secret), "{debug}");
        assert!(debug.contains("UpstreamKey(<redacted>)"), "{debug}");

        let summary = config.summary_lines().join("\n");
        assert!(!summary.contains(secret), "{summary}");
        assert!(summary.contains(OPENAI_URL), "{summary}");
    }
}
