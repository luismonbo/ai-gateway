use super::*;
use crate::config::tests::{lookup, LONG};
use crate::config::{Config, ConfigError};

const GUID: &str = "0f7c3a52-8d4e-4b9a-9c1d-2e5f6a7b8c9d";
const TOKEN_FILE: &str = "/var/run/secrets/azure/tokens/azure-identity-token";

/// One upstream, `azure`, on an Azure OpenAI URL.
const BASE: [(&str, &str); 3] = [
    ("GATEWAY_API_KEYS", LONG),
    ("UPSTREAMS", "azure"),
    ("AZURE_BASE_URL", "https://res.openai.azure.com/openai/v1"),
];

/// [`BASE`] plus `vars`; an entry in `vars` wins over the base.
fn azure(vars: &[(&str, &str)]) -> Result<(Config, Vec<String>), ConfigError> {
    let all: Vec<(&str, &str)> = vars.iter().copied().chain(BASE).collect();
    Config::from_lookup(lookup(&all))
}

/// The `azure` upstream's sign-in settings, which `vars` must produce.
fn sign_in_of(vars: &[(&str, &str)]) -> AzureSignIn {
    let (config, _) = azure(vars).unwrap();
    config.upstream("azure").unwrap().sign_in.clone().unwrap()
}

#[test]
fn auth_modes_parse_in_any_case() {
    let (config, warnings) = azure(&[("AZURE_AUTH", "Managed_Identity")]).unwrap();
    let sign_in = config.upstream("azure").unwrap().sign_in.clone().unwrap();
    assert_eq!(
        sign_in.method,
        SignInMethod::ManagedIdentity { client_id: None }
    );
    assert_eq!(sign_in.scope, DEFAULT_TOKEN_SCOPE);
    assert!(warnings.is_empty(), "{warnings:?}");
    let (config, _) = azure(&[("AZURE_AUTH", "azure_cli")]).unwrap();
    assert_eq!(
        config
            .upstream("azure")
            .unwrap()
            .sign_in
            .as_ref()
            .unwrap()
            .method,
        SignInMethod::AzureCli
    );
}

#[test]
fn unknown_auth_mode_is_an_error() {
    let err = azure(&[("AZURE_AUTH", "api_key")]).unwrap_err();
    assert_eq!(err.variable, "AZURE_AUTH");
    assert!(err.message.contains("managed_identity"), "{err}");
    assert!(err.message.contains("azure_cli"), "{err}");
}

#[test]
fn auth_and_api_key_are_exclusive() {
    let err = azure(&[
        ("AZURE_AUTH", "azure_cli"),
        ("AZURE_API_KEY", "sk-secret-x"),
    ])
    .unwrap_err();
    assert_eq!(err.variable, "AZURE_AUTH");
    assert!(err.message.contains("AZURE_API_KEY"), "{err}");
    assert!(!err.to_string().contains("sk-secret-x"), "{err}");
}

#[test]
fn client_id_picks_a_user_assigned_identity() {
    let sign_in = sign_in_of(&[
        ("AZURE_AUTH", "managed_identity"),
        ("AZURE_CLIENT_ID", GUID),
    ]);
    assert_eq!(
        sign_in.method,
        SignInMethod::ManagedIdentity {
            client_id: Some(GUID.to_string())
        }
    );
}

#[test]
fn client_id_accepts_uppercase_and_spaces() {
    // Copied from the portal: uppercase, with stray spaces. It's kept in
    // the canonical lowercase form, which every Azure endpoint accepts.
    let pasted = format!("  {}  ", GUID.to_uppercase());
    let sign_in = sign_in_of(&[
        ("AZURE_AUTH", "managed_identity"),
        ("AZURE_CLIENT_ID", &pasted),
    ]);
    assert_eq!(
        sign_in.method,
        SignInMethod::ManagedIdentity {
            client_id: Some("0f7c3a52-8d4e-4b9a-9c1d-2e5f6a7b8c9d".to_string())
        }
    );
}

#[test]
fn client_id_must_be_a_guid() {
    let with_client_id = |client_id| {
        azure(&[
            ("AZURE_AUTH", "managed_identity"),
            ("AZURE_CLIENT_ID", client_id),
        ])
        .unwrap_err()
    };
    for bad in [
        "not-a-guid",
        "0f7c3a52-8d4e-4b9a-9c1d-2e5f6a7b8c9",
        "0f7c3a52x8d4e-4b9a-9c1d-2e5f6a7b8c9d",
    ] {
        assert_eq!(with_client_id(bad).variable, "AZURE_CLIENT_ID", "{bad}");
    }

    let err = with_client_id(
        "/subscriptions/s/resourceGroups/g/providers/Microsoft.ManagedIdentity/userAssignedIdentities/u",
    );
    assert_eq!(err.variable, "AZURE_CLIENT_ID");
    assert!(err.message.contains("resource ID"), "{err}");
}

#[test]
fn client_id_without_managed_identity_only_warns() {
    // On AKS the webhook exports AZURE_CLIENT_ID, which an upstream named
    // `azure` reads as its own.
    let (config, warnings) =
        azure(&[("AZURE_AUTH", "azure_cli"), ("AZURE_CLIENT_ID", GUID)]).unwrap();
    let sign_in = config.upstream("azure").unwrap().sign_in.as_ref().unwrap();
    assert_eq!(sign_in.method, SignInMethod::AzureCli);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("AZURE_CLIENT_ID"), "{warnings:?}");
    assert!(warnings[0].contains("ignored"), "{warnings:?}");

    let (config, warnings) = azure(&[("AZURE_CLIENT_ID", GUID)]).unwrap();
    assert!(config.upstream("azure").unwrap().sign_in.is_none());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("AZURE_CLIENT_ID"), "{warnings:?}");
    assert!(warnings[0].contains("ignored"), "{warnings:?}");
}

#[test]
fn workload_identity_is_used_when_aks_sets_it_up() {
    let aks = [
        ("AZURE_AUTH", "managed_identity"),
        ("AZURE_FEDERATED_TOKEN_FILE", TOKEN_FILE),
    ];
    assert_eq!(
        sign_in_of(&aks).method,
        SignInMethod::WorkloadIdentity { client_id: None }
    );

    let with_client_id: Vec<_> = aks.into_iter().chain([("AZURE_CLIENT_ID", GUID)]).collect();
    assert_eq!(
        sign_in_of(&with_client_id).method,
        SignInMethod::WorkloadIdentity {
            client_id: Some(GUID.to_string())
        }
    );
}

#[test]
fn token_scope_defaults_and_overrides() {
    assert_eq!(
        sign_in_of(&[("AZURE_AUTH", "azure_cli")]).scope,
        "https://ai.azure.com/.default"
    );

    let older = "https://cognitiveservices.azure.com/.default";
    assert_eq!(
        sign_in_of(&[("AZURE_AUTH", "azure_cli"), ("AZURE_TOKEN_SCOPE", older)]).scope,
        older
    );

    for bad in ["cognitiveservices.azure.com", "http://x/.default"] {
        let err = azure(&[("AZURE_AUTH", "azure_cli"), ("AZURE_TOKEN_SCOPE", bad)]).unwrap_err();
        assert_eq!(err.variable, "AZURE_TOKEN_SCOPE", "{bad}");
    }
}

#[test]
fn token_scope_without_auth_only_warns() {
    let (config, warnings) = azure(&[(
        "AZURE_TOKEN_SCOPE",
        "https://cognitiveservices.azure.com/.default",
    )])
    .unwrap();
    assert!(config.upstream("azure").unwrap().sign_in.is_none());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("AZURE_TOKEN_SCOPE"), "{warnings:?}");
}

#[test]
fn tokens_never_go_over_plain_http() {
    let err = azure(&[
        ("AZURE_AUTH", "azure_cli"),
        ("AZURE_BASE_URL", "http://res.openai.azure.com/openai/v1"),
    ])
    .unwrap_err();
    assert_eq!(err.variable, "AZURE_BASE_URL");
    assert!(err.message.contains("https://"), "{err}");
    assert!(!err.message.contains("res.openai"), "echoes the URL: {err}");

    for local in [
        "http://localhost:8080/v1",
        "http://127.0.0.1:9/v1",
        "http://127.10.0.1:9/v1",
        "http://[::1]:9/v1",
    ] {
        let (_, warnings) = azure(&[("AZURE_AUTH", "azure_cli"), ("AZURE_BASE_URL", local)])
            .unwrap_or_else(|e| panic!("{local}: {e}"));
        assert!(warnings.is_empty(), "{local}: {warnings:?}");
    }

    // Without AZURE_AUTH no token is sent, so plain http stays allowed.
    let (config, _) =
        azure(&[("AZURE_BASE_URL", "http://res.openai.azure.com/openai/v1")]).unwrap();
    assert!(config.upstream("azure").unwrap().sign_in.is_none());
}

#[test]
fn non_azure_hosts_get_a_warning() {
    let (_, warnings) = azure(&[
        ("AZURE_AUTH", "azure_cli"),
        ("AZURE_BASE_URL", "https://api.openai.com/v1"),
    ])
    .unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("AZURE_BASE_URL"), "{warnings:?}");
    assert!(warnings[0].contains("Azure"), "{warnings:?}");
    assert!(!warnings[0].contains("api.openai.com"), "{warnings:?}");

    for url in [
        "https://res.services.ai.azure.com/openai/v1",
        "https://res.cognitiveservices.azure.com/openai/v1",
    ] {
        let (_, warnings) = azure(&[("AZURE_AUTH", "azure_cli"), ("AZURE_BASE_URL", url)]).unwrap();
        assert!(warnings.is_empty(), "{url}: {warnings:?}");
    }
}

#[test]
fn summary_names_the_sign_in_method() {
    let user_assigned = format!("auth: managed identity (client ID {GUID})");
    let workload_user_assigned = format!("auth: workload identity (client ID {GUID})");
    let cases: [(&[(&str, &str)], &str); 7] = [
        (
            &[("AZURE_AUTH", "managed_identity")],
            "auth: managed identity (system-assigned)",
        ),
        (
            &[
                ("AZURE_AUTH", "managed_identity"),
                ("AZURE_CLIENT_ID", GUID),
            ],
            &user_assigned,
        ),
        (
            &[
                ("AZURE_AUTH", "managed_identity"),
                ("AZURE_FEDERATED_TOKEN_FILE", TOKEN_FILE),
            ],
            "auth: workload identity,",
        ),
        (
            &[
                ("AZURE_AUTH", "managed_identity"),
                ("AZURE_FEDERATED_TOKEN_FILE", TOKEN_FILE),
                ("AZURE_CLIENT_ID", GUID),
            ],
            &workload_user_assigned,
        ),
        (&[("AZURE_AUTH", "azure_cli")], "auth: azure cli"),
        (&[("AZURE_API_KEY", "sk-test")], "auth: api key"),
        (&[], "auth: none"),
    ];
    for (vars, expected) in cases {
        let (config, _) = azure(vars).unwrap();
        let summary = config.summary_lines().join("\n");
        assert!(summary.contains(expected), "{expected}: {summary}");
    }
}
