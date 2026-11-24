use super::*;
use crate::auth::Role;

pub(super) const LONG: &str = "0123456789abcdef0123456789abcdef"; // 32 characters
const LONG2: &str = "fedcba9876543210fedcba9876543210";

/// A lookup over fixed pairs; the first pair with the name wins.
pub(super) fn lookup<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    }
}

/// One valid upstream, so tests here can focus on other variables.
const UPSTREAM_BASE: [(&str, &str); 2] = [
    ("UPSTREAMS", "local"),
    ("LOCAL_BASE_URL", "http://127.0.0.1:9/v1"),
];

/// `vars` plus [`UPSTREAM_BASE`]; an entry in `vars` wins over the base.
fn load(vars: &[(&str, &str)]) -> Result<(Config, Vec<String>), ConfigError> {
    let all: Vec<(&str, &str)> = vars.iter().copied().chain(UPSTREAM_BASE).collect();
    Config::from_lookup(lookup(&all))
}

#[test]
fn timeouts_and_limit_have_defaults() {
    let (config, _) = load(&[("GATEWAY_API_KEYS", LONG)]).unwrap();
    assert_eq!(config.connect_timeout, Duration::from_secs(10));
    assert_eq!(config.upstream_timeout, Duration::from_secs(300));
    assert_eq!(config.max_request_bytes, 10_485_760);
    assert_eq!(config.rate_limit_rpm, 60);
    assert_eq!(config.default_upstream, None);

    let (config, _) = load(&[
        ("GATEWAY_API_KEYS", LONG),
        ("UPSTREAM_CONNECT_TIMEOUT_SECS", "3"),
        ("UPSTREAM_TIMEOUT_SECS", "30"),
        ("MAX_REQUEST_BYTES", "1024"),
    ])
    .unwrap();
    assert_eq!(config.connect_timeout, Duration::from_secs(3));
    assert_eq!(config.upstream_timeout, Duration::from_secs(30));
    assert_eq!(config.max_request_bytes, 1024);
}

#[test]
fn zero_timeouts_are_rejected() {
    for name in [
        "UPSTREAM_CONNECT_TIMEOUT_SECS",
        "UPSTREAM_TIMEOUT_SECS",
        "MAX_REQUEST_BYTES",
        "RATE_LIMIT_RPM",
    ] {
        let err = load(&[("GATEWAY_API_KEYS", LONG), (name, "0")]).unwrap_err();
        assert_eq!(err.variable, name);
    }
}

#[test]
fn old_env_file_gets_clear_messages() {
    let old = [
        ("AI_PROVIDER", "ollama"),
        ("OLLAMA_BASE_URL", "http://localhost:11434"),
        ("OPENAI_BASE_URL", "https://api.openai.com"),
        ("OPENAI_API_KEY", "sk-old"),
        ("GATEWAY_API_KEYS", LONG),
    ];
    assert_eq!(
        Config::from_lookup(lookup(&old)).unwrap_err().variable,
        "UPSTREAMS"
    );
    let fixed: Vec<_> = old
        .iter()
        .copied()
        .chain([("UPSTREAMS", "ollama,openai")])
        .collect();
    let (_, warnings) = Config::from_lookup(lookup(&fixed)).unwrap();
    for name in ["AI_PROVIDER", "OLLAMA_BASE_URL", "OPENAI_BASE_URL"] {
        assert!(warnings.iter().any(|w| w.contains(name)), "{name}");
    }
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
