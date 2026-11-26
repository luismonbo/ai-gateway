//! The `UPSTREAMS` list and each upstream's `<NAME>_*` variables.
//!
//! Error messages never repeat a base URL: a mistyped one may hold a secret.

use super::azure::{self, AzureSignIn};
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
    /// From `<NAME>_AUTH`: the upstream gets Azure tokens instead of a key.
    pub sign_in: Option<AzureSignIn>,
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
    let (upstreams, warnings): (Vec<Upstream>, Vec<Vec<String>>) = parsed.into_iter().unzip();
    Ok((upstreams, warnings.concat()))
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

/// One upstream's variables, and their warnings.
fn upstream(
    env: &Env,
    name: &str,
    names: &[String],
) -> Result<(Upstream, Vec<String>), ConfigError> {
    let prefix = name.to_ascii_uppercase();
    let url_variable = format!("{prefix}_BASE_URL");
    let key_variable = format!("{prefix}_API_KEY");

    let url = base_url(env, &url_variable, &key_variable)?;
    let path_warning = (url.path() == "/").then(|| {
        format!(
            "{url_variable}: the URL has no path; OpenAI-compatible APIs usually need \
             the version path, e.g. https://api.openai.com/v1 or http://localhost:11434/v1"
        )
    });
    let api_key = env.get(&key_variable).map(UpstreamKey);
    let (sign_in, sign_in_warnings) = azure::sign_in(env, &prefix, &url, api_key.is_some())?;
    let upstream = Upstream {
        name: name.to_string(),
        base_url: url.as_str().trim_end_matches('/').to_string(),
        api_key,
        sign_in,
        fallback: fallback(env, &format!("{prefix}_FALLBACK"), names)?,
    };
    let warnings = path_warning.into_iter().chain(sign_in_warnings).collect();
    Ok((upstream, warnings))
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
            format!("{raw:?} must be <upstream>/<model>, e.g. openai/<model>"),
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
mod tests;
