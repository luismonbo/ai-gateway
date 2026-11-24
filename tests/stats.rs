mod common;

use actix_web::test::{self, TestRequest};
use ai_gateway::auth::KeyId;
use common::{bearer, chat_body, config, init, json_body, mock_url, state, ADMIN_KEY, USER_KEY};
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn stats_as(key: &str, query: &str) -> actix_http::Request {
    TestRequest::get()
        .uri(&format!("/v1/stats{query}"))
        .insert_header(bearer(key))
        .to_request()
}

fn id(raw_key: &str) -> String {
    KeyId::from_raw_key(raw_key).to_string()
}

async fn upstream(response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

#[actix_web::test]
async fn users_see_only_their_own_stats() {
    let app = init(state(config(&[]))).await;
    let query = format!("?key_id={}", id(ADMIN_KEY));

    let response = test::call_service(&app, stats_as(USER_KEY, &query)).await;
    assert_eq!(response.status(), 200);
    let body = json_body(response).await;
    assert_eq!(body["key_id"], id(USER_KEY));
    assert_eq!(body["request_count"], 0);
    assert_eq!(body["models_used"], json!({}));
}

#[actix_web::test]
async fn admins_see_everyone_and_can_filter() {
    let app = init(state(config(&[]))).await;
    // Each request is counted once its response is ready, so the admin's
    // first call shows up from the second one on.
    for key in [USER_KEY, ADMIN_KEY] {
        assert_eq!(
            test::call_service(&app, stats_as(key, "")).await.status(),
            200
        );
    }

    let all = json_body(test::call_service(&app, stats_as(ADMIN_KEY, "")).await).await;
    let ids: Vec<&str> = all
        .as_array()
        .expect("an array")
        .iter()
        .map(|entry| entry["key_id"].as_str().unwrap())
        .collect();
    let mut expected = [id(ADMIN_KEY), id(USER_KEY)];
    expected.sort();
    assert_eq!(ids, expected);

    let query = format!("?key_id={}", id(USER_KEY));
    let one = json_body(test::call_service(&app, stats_as(ADMIN_KEY, &query)).await).await;
    assert_eq!(one["key_id"], id(USER_KEY));
    assert_eq!(one["request_count"], 1);
    for field in [
        "error_count",
        "total_latency_ms",
        "avg_latency_ms",
        "total_prompt_tokens",
        "total_completion_tokens",
        "last_request_timestamp",
        "models_used",
    ] {
        assert!(one.get(field).is_some(), "missing {field}: {one}");
    }
}

#[actix_web::test]
async fn unknown_key_id_is_404_and_malformed_is_400() {
    let app = init(state(config(&[]))).await;

    let response = test::call_service(&app, stats_as(ADMIN_KEY, "?key_id=key_000000000000")).await;
    assert_eq!(response.status(), 404);
    assert_eq!(json_body(response).await["error"]["code"], "not_found");

    let response = test::call_service(&app, stats_as(ADMIN_KEY, "?key_id=sk-raw-key-123")).await;
    assert_eq!(response.status(), 400);
    let text = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
    assert!(!text.contains("sk-raw-key-123"), "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

#[actix_web::test]
async fn requests_and_server_errors_are_counted() {
    let primary = upstream(ResponseTemplate::new(200).set_body_json(json!({
        "model": "m1",
        "choices": [],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2}
    })))
    .await;
    let backup = upstream(ResponseTemplate::new(500)).await;
    let (primary_url, backup_url) = (mock_url(&primary), mock_url(&backup));
    let app = init(state(config(&[
        ("PRIMARY_BASE_URL", &primary_url),
        ("BACKUP_BASE_URL", &backup_url),
    ])))
    .await;

    for (model, status) in [("primary/m1", 200), ("backup/m1", 502)] {
        let request = TestRequest::post()
            .uri("/v1/chat/completions")
            .insert_header(bearer(USER_KEY))
            .set_json(chat_body(model))
            .to_request();
        assert_eq!(test::call_service(&app, request).await.status(), status);
    }

    let stats = json_body(test::call_service(&app, stats_as(USER_KEY, "")).await).await;
    assert_eq!(stats["request_count"], 2);
    assert_eq!(stats["error_count"], 1);
    assert_eq!(stats["total_prompt_tokens"], 5);
    assert_eq!(stats["models_used"], json!({"primary/m1": 1}));
}

#[actix_web::test]
async fn rejected_requests_are_not_counted() {
    let app = init(state(config(&[("RATE_LIMIT_RPM", "1")]))).await;
    assert_eq!(
        test::call_service(&app, stats_as(USER_KEY, ""))
            .await
            .status(),
        200
    );
    assert_eq!(
        test::call_service(&app, stats_as(USER_KEY, ""))
            .await
            .status(),
        429
    );
    let unauthenticated = TestRequest::get().uri("/v1/stats").to_request();
    assert_eq!(
        test::call_service(&app, unauthenticated).await.status(),
        401
    );

    let query = format!("?key_id={}", id(USER_KEY));
    let stats = json_body(test::call_service(&app, stats_as(ADMIN_KEY, &query)).await).await;
    assert_eq!(stats["request_count"], 1);
}
