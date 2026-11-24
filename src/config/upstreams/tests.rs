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

#[test]
fn fallback_errors_name_no_model() {
    // Model names go stale; the owner keeps them out of the code.
    let err = load(&[
        ("UPSTREAMS", "ollama,openai"),
        ("OLLAMA_BASE_URL", OLLAMA_URL),
        ("OPENAI_BASE_URL", OPENAI_URL),
        ("OLLAMA_FALLBACK", "openai"),
    ])
    .unwrap_err();
    assert!(err.message.contains("<upstream>/<model>"), "{err}");
    assert!(!err.message.contains("gpt"), "names a model: {err}");
}
