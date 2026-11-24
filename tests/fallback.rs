mod common;

use actix_web::test::{self, TestRequest};
use ai_gateway::config::Config;
use common::{
    bearer, chat_body, closed_port_url, config, init, json_body, mock_url, sse_stream, state,
    USER_KEY,
};
use serde_json::{json, Value};
use std::time::Duration;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const FALLBACK: &str = "backup/fb-model";

/// A mock upstream that answers every request with `response`, and checks
/// on drop that it was called `times` times.
async fn upstream(response: ResponseTemplate, times: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(response)
        .expect(times)
        .mount(&server)
        .await;
    server
}

fn ok_json(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

/// `primary` falls back to `backup/fb-model`.
fn fallback_config(primary_url: &str, backup_url: &str, extra: &[(&str, &str)]) -> Config {
    let vars: Vec<(&str, &str)> = extra
        .iter()
        .copied()
        .chain([
            ("PRIMARY_BASE_URL", primary_url),
            ("BACKUP_BASE_URL", backup_url),
            ("PRIMARY_FALLBACK", FALLBACK),
        ])
        .collect();
    config(&vars)
}

async fn received_models(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("request recording is on")
        .iter()
        .map(|request| {
            let body: Value = serde_json::from_slice(&request.body).expect("a JSON body");
            body["model"].clone()
        })
        .collect()
}

fn post_chat(body: &Value) -> actix_http::Request {
    TestRequest::post()
        .uri("/v1/chat/completions")
        .insert_header(bearer(USER_KEY))
        .set_json(body)
        .to_request()
}

fn header<B>(response: &actix_web::dev::ServiceResponse<B>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().unwrap().to_string())
}

/// Sends a chat request for `primary/m1` and checks that `backup` answered.
async fn assert_backup_answered(config: Config, backup: &MockServer) {
    let app = init(state(config)).await;
    let response = test::call_service(&app, post_chat(&chat_body("primary/m1"))).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        header(&response, "x-gateway-fallback").as_deref(),
        Some(FALLBACK)
    );
    assert_eq!(
        header(&response, "x-gateway-upstream").as_deref(),
        Some("backup")
    );
    assert_eq!(json_body(response).await["model"], "fb-model");
    assert_eq!(received_models(backup).await, [json!("fb-model")]);
}

#[actix_web::test]
async fn falls_back_on_500() {
    let primary = upstream(ResponseTemplate::new(500), 1).await;
    let backup = upstream(ok_json(json!({"model": "fb-model", "choices": []})), 1).await;
    let config = fallback_config(&mock_url(&primary), &mock_url(&backup), &[]);
    assert_backup_answered(config, &backup).await;
    assert_eq!(received_models(&primary).await, [json!("m1")]);
}

#[actix_web::test]
async fn falls_back_on_429() {
    let primary = upstream(
        ResponseTemplate::new(429).insert_header("retry-after", "30"),
        1,
    )
    .await;
    let backup = upstream(ok_json(json!({"model": "fb-model", "choices": []})), 1).await;
    let config = fallback_config(&mock_url(&primary), &mock_url(&backup), &[]);
    assert_backup_answered(config, &backup).await;
}

#[actix_web::test]
async fn falls_back_when_primary_refuses_connections() {
    let backup = upstream(ok_json(json!({"model": "fb-model", "choices": []})), 1).await;
    let config = fallback_config(&closed_port_url(), &mock_url(&backup), &[]);
    assert_backup_answered(config, &backup).await;
}

#[actix_web::test]
async fn falls_back_on_timeout() {
    let primary = upstream(
        ResponseTemplate::new(200).set_delay(Duration::from_secs(2)),
        1,
    )
    .await;
    let backup = upstream(ok_json(json!({"model": "fb-model", "choices": []})), 1).await;
    let config = Config {
        upstream_timeout: Duration::from_millis(300),
        ..fallback_config(&mock_url(&primary), &mock_url(&backup), &[])
    };
    assert_backup_answered(config, &backup).await;
}

#[actix_web::test]
async fn no_fallback_on_400_or_404() {
    for status in [400, 404] {
        let raw = r#"{"error":{"message":"the client's mistake"}}"#;
        let primary = upstream(
            ResponseTemplate::new(status).set_body_raw(raw, "application/json"),
            1,
        )
        .await;
        let backup = upstream(ResponseTemplate::new(200), 0).await;
        let app = init(state(fallback_config(
            &mock_url(&primary),
            &mock_url(&backup),
            &[],
        )))
        .await;

        let response = test::call_service(&app, post_chat(&chat_body("primary/m1"))).await;
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(header(&response, "x-gateway-fallback"), None);
        assert_eq!(
            header(&response, "x-gateway-upstream").as_deref(),
            Some("primary")
        );
        assert_eq!(test::read_body(response).await, raw.as_bytes());
    }
}

#[actix_web::test]
async fn no_fallback_on_401() {
    let primary = upstream(ResponseTemplate::new(401), 1).await;
    let backup = upstream(ResponseTemplate::new(200), 0).await;
    let app = init(state(fallback_config(
        &mock_url(&primary),
        &mock_url(&backup),
        &[],
    )))
    .await;

    let response = test::call_service(&app, post_chat(&chat_body("primary/m1"))).await;
    assert_eq!(response.status(), 502);
    assert_eq!(header(&response, "x-gateway-fallback"), None);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "upstream_auth_failed"
    );
}

#[actix_web::test]
async fn streaming_falls_back_before_the_first_byte() {
    let primary = upstream(ResponseTemplate::new(500), 1).await;
    let backup = upstream(
        ResponseTemplate::new(200).set_body_raw(sse_stream(), "text/event-stream"),
        1,
    )
    .await;
    let app = init(state(fallback_config(
        &mock_url(&primary),
        &mock_url(&backup),
        &[],
    )))
    .await;
    let body = json!({
        "model": "primary/m1",
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let response = test::call_service(&app, post_chat(&body)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        header(&response, "x-gateway-fallback").as_deref(),
        Some(FALLBACK)
    );
    assert_eq!(
        header(&response, "content-type").as_deref(),
        Some("text/event-stream")
    );
    let streamed = test::read_body(response).await;
    assert_eq!(String::from_utf8_lossy(&streamed), sse_stream());
    assert_eq!(received_models(&backup).await, [json!("fb-model")]);
}

#[actix_web::test]
async fn fallback_is_one_hop() {
    let primary = upstream(ResponseTemplate::new(500), 1).await;
    let backup = upstream(ResponseTemplate::new(500), 1).await;
    let third = upstream(ResponseTemplate::new(200), 0).await;
    let third_url = mock_url(&third);
    let app = init(state(fallback_config(
        &mock_url(&primary),
        &mock_url(&backup),
        &[
            ("UPSTREAMS", "primary,backup,third"),
            ("THIRD_BASE_URL", &third_url),
            ("BACKUP_FALLBACK", "third/x"),
        ],
    )))
    .await;

    let response = test::call_service(&app, post_chat(&chat_body("primary/m1"))).await;
    assert_eq!(response.status(), 502);
    assert_eq!(
        header(&response, "x-gateway-fallback").as_deref(),
        Some(FALLBACK)
    );
    assert_eq!(
        header(&response, "x-gateway-upstream").as_deref(),
        Some("backup")
    );
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], "upstream_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("'backup'"),
        "{body}"
    );
}

#[actix_web::test]
async fn stats_record_the_fallback_model() {
    let primary = upstream(ResponseTemplate::new(503), 1).await;
    // No `model` in the answer: the label falls back to the model sent.
    let backup = upstream(
        ok_json(json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 4}})),
        1,
    )
    .await;
    let app = init(state(fallback_config(
        &mock_url(&primary),
        &mock_url(&backup),
        &[],
    )))
    .await;

    let response = test::call_service(&app, post_chat(&chat_body("primary/m1"))).await;
    assert_eq!(response.status(), 200);

    let stats_request = TestRequest::get()
        .uri("/v1/stats")
        .insert_header(bearer(USER_KEY))
        .to_request();
    let stats = json_body(test::call_service(&app, stats_request).await).await;
    assert_eq!(stats["models_used"], json!({"backup/fb-model": 1}));
    assert_eq!(stats["total_prompt_tokens"], 3);
}
