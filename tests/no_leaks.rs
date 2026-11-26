mod common;

use actix_web::test::{self, TestRequest};
use ai_gateway::auth::KeyId;
use common::{
    bearer, capture_logs, chat_body, config, init, mock_url, state, state_with_credentials,
    FakeCredential, ADMIN_KEY, UPSTREAM_KEY, USER_KEY,
};
use std::sync::{Arc, PoisonError};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const WRONG_KEY: &str = "sk-wrong-key-0123456789";

/// Everything the gateway sent back for one request, as text.
struct Seen {
    status: u16,
    headers: Vec<String>,
    body: String,
}

#[actix_web::test]
async fn no_key_reaches_logs_stats_or_responses() {
    let (_guard, logs) = capture_logs();

    // The primary leaks its own key in an error body; the fallback fails
    // once with 401, then answers.
    let primary = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string(format!(
            r#"{{"error":{{"message":"Incorrect API key provided: {UPSTREAM_KEY}"}}}}"#
        )))
        .mount(&primary)
        .await;
    let backup = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .mount(&backup)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "fb",
            "choices": [],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })))
        .mount(&backup)
        .await;

    let (primary_url, backup_url) = (mock_url(&primary), mock_url(&backup));
    let config = config(&[
        ("PRIMARY_BASE_URL", &primary_url),
        ("PRIMARY_API_KEY", UPSTREAM_KEY),
        ("PRIMARY_FALLBACK", "backup/fb"),
        ("BACKUP_BASE_URL", &backup_url),
    ]);
    let config_debug = format!("{config:?}");
    let state = state(config);
    let stats = Arc::clone(&state.stats);
    let app = init(state).await;

    let chat = || {
        TestRequest::post()
            .uri("/v1/chat/completions")
            .insert_header(bearer(USER_KEY))
            .set_json(chat_body("primary/m1"))
    };
    let requests = [
        chat().to_request(),
        chat().to_request(),
        TestRequest::get()
            .uri("/v1/stats")
            .insert_header(bearer(WRONG_KEY))
            .to_request(),
        TestRequest::get()
            .uri("/v1/stats")
            .insert_header(bearer(USER_KEY))
            .to_request(),
        TestRequest::get()
            .uri("/v1/stats")
            .insert_header(bearer(ADMIN_KEY))
            .to_request(),
        TestRequest::get()
            .uri("/v1/models")
            .insert_header(bearer(USER_KEY))
            .to_request(),
    ];
    let mut seen = Vec::new();
    for request in requests {
        let response = test::call_service(&app, request).await;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| format!("{name}: {}", String::from_utf8_lossy(value.as_bytes())))
            .collect();
        let body = String::from_utf8_lossy(&test::read_body(response).await).into_owned();
        seen.push(Seen {
            status,
            headers,
            body,
        });
    }
    let statuses: Vec<u16> = seen.iter().map(|s| s.status).collect();
    assert_eq!(statuses, [502, 200, 401, 200, 200, 200]);

    let dir = tempfile::tempdir().unwrap();
    let stats_file = dir.path().join("stats.json");
    stats.save(&stats_file).unwrap();
    let saved = std::fs::read_to_string(&stats_file).unwrap();

    let logged =
        String::from_utf8_lossy(&logs.lock().unwrap_or_else(PoisonError::into_inner)).into_owned();
    let places: Vec<(String, &str)> = [
        ("the logs".to_string(), logged.as_str()),
        ("the stats file".to_string(), saved.as_str()),
        (
            "the config's Debug output".to_string(),
            config_debug.as_str(),
        ),
    ]
    .into_iter()
    .chain(seen.iter().enumerate().flat_map(|(i, s)| {
        std::iter::once((format!("response {i}'s body"), s.body.as_str())).chain(
            s.headers
                .iter()
                .map(move |header| (format!("response {i}'s headers"), header.as_str())),
        )
    }))
    .collect();
    for secret in [USER_KEY, ADMIN_KEY, UPSTREAM_KEY, WRONG_KEY] {
        for (place, text) in &places {
            assert!(
                !text.contains(secret),
                "{secret} leaked into {place}: {text}"
            );
        }
    }
    // The scan above saw real output: the capture works, and the primary's
    // body was logged with its key redacted.
    assert!(logged.contains("using fallback"), "{logged}");
    assert!(logged.contains("[redacted]"), "{logged}");
    assert!(
        saved.contains(&KeyId::from_raw_key(USER_KEY).to_string()),
        "{saved}"
    );
}

#[actix_web::test]
async fn no_token_reaches_logs_stats_or_responses() {
    const TOKEN: &str = "tok-secret-0123456789abcdef";
    let (_guard, logs) = capture_logs();

    // Both upstreams sign in; each echoes the token in an error body.
    let primary = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(404).set_body_string(format!(
            r#"{{"error":{{"message":"no deployment 'm' for token {TOKEN}"}}}}"#
        )))
        .mount(&primary)
        .await;
    let backup = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(500).set_body_string(format!("internal error (token {TOKEN})")),
        )
        .mount(&backup)
        .await;

    let (primary_url, backup_url) = (mock_url(&primary), mock_url(&backup));
    let config = config(&[
        ("PRIMARY_BASE_URL", &primary_url),
        ("PRIMARY_AUTH", "azure_cli"),
        ("BACKUP_BASE_URL", &backup_url),
        ("BACKUP_AUTH", "azure_cli"),
    ]);
    let state = state_with_credentials(
        config,
        &[
            ("primary", FakeCredential::ok(TOKEN)),
            ("backup", FakeCredential::ok(TOKEN)),
        ],
    );
    let stats = Arc::clone(&state.stats);
    let app = init(state).await;

    let chat = |model: &str| {
        TestRequest::post()
            .uri("/v1/chat/completions")
            .insert_header(bearer(USER_KEY))
            .set_json(chat_body(model))
            .to_request()
    };
    let get = |uri: &str| {
        TestRequest::get()
            .uri(uri)
            .insert_header(bearer(ADMIN_KEY))
            .to_request()
    };
    let requests = [
        chat("primary/m"),
        chat("backup/m"),
        get("/v1/models"),
        get("/v1/stats"),
    ];
    let mut seen = Vec::new();
    for request in requests {
        let response = test::call_service(&app, request).await;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| format!("{name}: {}", String::from_utf8_lossy(value.as_bytes())))
            .collect();
        let body = String::from_utf8_lossy(&test::read_body(response).await).into_owned();
        seen.push(Seen {
            status,
            headers,
            body,
        });
    }
    let statuses: Vec<u16> = seen.iter().map(|s| s.status).collect();
    assert_eq!(statuses, [404, 502, 200, 200]);

    let dir = tempfile::tempdir().unwrap();
    let stats_file = dir.path().join("stats.json");
    stats.save(&stats_file).unwrap();
    let saved = std::fs::read_to_string(&stats_file).unwrap();

    let logged =
        String::from_utf8_lossy(&logs.lock().unwrap_or_else(PoisonError::into_inner)).into_owned();
    assert!(
        !logged.contains(TOKEN),
        "the token leaked into the logs: {logged}"
    );
    assert!(
        !saved.contains(TOKEN),
        "the token leaked into the stats file: {saved}"
    );
    for (i, s) in seen.iter().enumerate() {
        assert!(!s.body.contains(TOKEN), "response {i}'s body: {}", s.body);
        for header in &s.headers {
            assert!(!header.contains(TOKEN), "response {i}'s headers: {header}");
        }
    }
    // The 404 was passed through and the 500 logged, both with the token
    // replaced, so the scan above saw the bodies that held it.
    assert!(seen[0].body.contains("[redacted]"), "{}", seen[0].body);
    assert!(
        logged.contains("internal error (token [redacted])"),
        "{logged}"
    );
}
