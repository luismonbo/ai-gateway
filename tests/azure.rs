//! Upstreams that sign in to Azure: the token goes out as the bearer, and a
//! failed sign-in is a 502 that never falls back.

mod common;

use actix_web::test::{self, TestRequest};
use ai_gateway::auth::KeyId;
use common::{
    bearer, capture_logs, chat_body, config, init, json_body, mock_url, sse_stream,
    state_with_credentials, FakeCredential, USER_KEY,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, PoisonError};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "tok-1";

/// A mock upstream that answers every chat request with `response`.
async fn upstream(response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

fn ok_chat() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"model": "m1", "choices": []}))
}

fn post_chat(body: &Value) -> actix_http::Request {
    TestRequest::post()
        .uri("/v1/chat/completions")
        .insert_header(bearer(USER_KEY))
        .set_json(body)
        .to_request()
}

/// The `authorization` header of each request the mock received.
async fn authorizations(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .expect("request recording is on")
        .iter()
        .map(|request| {
            request
                .headers
                .get("authorization")
                .map(|value| value.to_str().unwrap().to_string())
                .unwrap_or_default()
        })
        .collect()
}

fn text(logs: &Mutex<Vec<u8>>) -> String {
    String::from_utf8_lossy(&logs.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
}

#[actix_web::test]
async fn chat_sends_the_token_as_bearer() {
    let server = upstream(ok_chat()).await;
    let url = mock_url(&server);
    let credential = FakeCredential::ok(TOKEN);
    let config = config(&[("PRIMARY_BASE_URL", &url), ("PRIMARY_AUTH", "azure_cli")]);
    let app = init(state_with_credentials(
        config,
        &[("primary", Arc::clone(&credential))],
    ))
    .await;

    for _ in 0..2 {
        let response = test::call_service(&app, post_chat(&chat_body("primary/m1"))).await;
        assert_eq!(response.status(), 200);
    }
    assert_eq!(
        authorizations(&server).await,
        ["Bearer tok-1", "Bearer tok-1"]
    );
    // The second request reused the cached token.
    assert_eq!(credential.calls(), 1);
}

#[actix_web::test]
async fn streaming_chat_sends_the_token() {
    let server =
        upstream(ResponseTemplate::new(200).set_body_raw(sse_stream(), "text/event-stream")).await;
    let url = mock_url(&server);
    let config = config(&[("PRIMARY_BASE_URL", &url), ("PRIMARY_AUTH", "azure_cli")]);
    let app = init(state_with_credentials(
        config,
        &[("primary", FakeCredential::ok(TOKEN))],
    ))
    .await;
    let body = json!({
        "model": "primary/m1",
        "stream": true,
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let response = test::call_service(&app, post_chat(&body)).await;
    assert_eq!(response.status(), 200);
    let streamed = test::read_body(response).await;
    assert!(!streamed.is_empty());
    assert_eq!(authorizations(&server).await, ["Bearer tok-1"]);
}

#[actix_web::test]
async fn models_sends_the_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m1"}]})))
        .mount(&server)
        .await;
    let url = mock_url(&server);
    let config = config(&[("PRIMARY_BASE_URL", &url), ("PRIMARY_AUTH", "azure_cli")]);
    let app = init(state_with_credentials(
        config,
        &[("primary", FakeCredential::ok(TOKEN))],
    ))
    .await;

    let request = TestRequest::get()
        .uri("/v1/models")
        .insert_header(bearer(USER_KEY))
        .to_request();
    let models = json_body(test::call_service(&app, request).await).await;
    assert_eq!(models["data"][0]["id"], "primary/m1");
    assert_eq!(authorizations(&server).await, ["Bearer tok-1"]);
}

#[actix_web::test]
async fn sign_in_failure_is_502_without_fallback() {
    let (_guard, logs) = capture_logs();
    let primary = upstream(ok_chat()).await;
    let backup = upstream(ok_chat()).await;
    let (primary_url, backup_url) = (mock_url(&primary), mock_url(&backup));
    let config = config(&[
        ("PRIMARY_BASE_URL", &primary_url),
        ("PRIMARY_AUTH", "azure_cli"),
        ("PRIMARY_FALLBACK", "backup/m"),
        ("BACKUP_BASE_URL", &backup_url),
    ]);
    let app = init(state_with_credentials(
        config,
        &[("primary", FakeCredential::failing())],
    ))
    .await;

    let response = test::call_service(&app, post_chat(&chat_body("primary/m1"))).await;
    assert_eq!(response.status(), 502);
    assert_eq!(
        response.headers().get("x-gateway-upstream").unwrap(),
        "primary"
    );
    assert!(response.headers().get("x-gateway-fallback").is_none());
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], "upstream_auth_failed");

    // Without a token nothing is sent, and the fallback isn't tried.
    assert!(primary.received_requests().await.unwrap().is_empty());
    assert!(backup.received_requests().await.unwrap().is_empty());
    // The reason is logged with the caller's key ID.
    let logs = text(&logs);
    assert!(logs.contains("could not sign in"), "{logs}");
    assert!(logs.contains("please run 'az login'"), "{logs}");
    assert!(
        logs.contains(KeyId::from_raw_key(USER_KEY).as_str()),
        "{logs}"
    );
}

#[actix_web::test]
async fn auth_rejections_log_azure_hints() {
    for (status, hint) in [
        (401, "custom subdomain"),
        (403, "Cognitive Services OpenAI User"),
    ] {
        let (_guard, logs) = capture_logs();
        let server = upstream(ResponseTemplate::new(status)).await;
        let url = mock_url(&server);
        let config = config(&[("PRIMARY_BASE_URL", &url), ("PRIMARY_AUTH", "azure_cli")]);
        let app = init(state_with_credentials(
            config,
            &[("primary", FakeCredential::ok(TOKEN))],
        ))
        .await;

        let response = test::call_service(&app, post_chat(&chat_body("primary/m1"))).await;
        assert_eq!(response.status(), 502, "{status}");
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "upstream_auth_failed", "{status}");
        let logs = text(&logs);
        assert!(logs.contains(hint), "{status}: {logs}");
    }
}
