mod common;

use actix_web::test::{self, TestRequest};
use common::{
    bearer, chat_body, config, init, json_body, mock_url, sse_stream, state, UPSTREAM_KEY,
    USAGE_EVENT, USER_KEY,
};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHAT: &str = "/v1/chat/completions";

fn post_chat(body: &Value) -> TestRequest {
    TestRequest::post()
        .uri(CHAT)
        .insert_header(bearer(USER_KEY))
        .set_json(body)
}

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

/// The JSON bodies the mock received, in order.
async fn received_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("request recording is on")
        .iter()
        .map(|request| serde_json::from_slice(&request.body).expect("a JSON body"))
        .collect()
}

fn ok_json(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn sse_response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(sse_stream(), "text/event-stream")
}

#[actix_web::test]
async fn forwards_extra_fields_and_strips_the_prefix() {
    let server = upstream(ok_json(json!({"model": "m1", "choices": []}))).await;
    let url = mock_url(&server);
    let app = init(state(config(&[("PRIMARY_BASE_URL", &url)]))).await;
    let body = json!({
        "model": "primary/m1",
        "temperature": 0.2,
        "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}],
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let response = test::call_service(&app, post_chat(&body).to_request()).await;
    assert_eq!(response.status(), 200);

    let received = received_bodies(&server).await;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["model"], "m1");
    assert_eq!(received[0]["temperature"], 0.2);
    assert_eq!(received[0]["tools"], body["tools"]);
    assert_eq!(received[0]["messages"], body["messages"]);
    assert!(received[0].get("stream").is_none(), "{}", received[0]);
}

#[actix_web::test]
async fn returns_the_upstream_body_byte_for_byte() {
    let raw = "{ \"model\" :\"m1\",  \"id\":\"chatcmpl-1\" ,\"choices\":[] }";
    let server = upstream(ResponseTemplate::new(200).set_body_raw(raw, "application/json")).await;
    let url = mock_url(&server);
    let app = init(state(config(&[("PRIMARY_BASE_URL", &url)]))).await;

    let response = test::call_service(&app, post_chat(&chat_body("primary/m1")).to_request()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("x-gateway-upstream").unwrap(),
        "primary"
    );
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(test::read_body(response).await, raw.as_bytes());
}

#[actix_web::test]
async fn sends_the_upstream_key_not_the_gateway_key() {
    let server = upstream(ok_json(json!({"model": "m1", "choices": []}))).await;
    let url = mock_url(&server);
    let app = init(state(config(&[
        ("PRIMARY_BASE_URL", &url),
        ("PRIMARY_API_KEY", UPSTREAM_KEY),
    ])))
    .await;

    let response = test::call_service(&app, post_chat(&chat_body("primary/m1")).to_request()).await;
    assert_eq!(response.status(), 200);

    let requests = server.received_requests().await.unwrap();
    let headers = &requests[0].headers;
    assert_eq!(
        headers["authorization"],
        format!("Bearer {UPSTREAM_KEY}").as_str()
    );
    for (name, value) in headers {
        let value = String::from_utf8_lossy(value.as_bytes());
        assert!(!value.contains(USER_KEY), "{name} carries the gateway key");
    }
}

#[actix_web::test]
async fn records_usage_from_the_response() {
    let server = upstream(ok_json(json!({
        "model": "m1-2026",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hi"}}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
    })))
    .await;
    let url = mock_url(&server);
    let app = init(state(config(&[("PRIMARY_BASE_URL", &url)]))).await;

    let response = test::call_service(&app, post_chat(&chat_body("primary/m1")).to_request()).await;
    assert_eq!(response.status(), 200);

    let stats_request = TestRequest::get()
        .uri("/v1/stats")
        .insert_header(bearer(USER_KEY))
        .to_request();
    let stats = json_body(test::call_service(&app, stats_request).await).await;
    assert_eq!(stats["total_prompt_tokens"], 5);
    assert_eq!(stats["total_completion_tokens"], 2);
    assert_eq!(stats["models_used"], json!({"primary/m1-2026": 1}));
}

#[actix_web::test]
async fn streams_events_through_unchanged() {
    let server = upstream(sse_response()).await;
    let url = mock_url(&server);
    let app = init(state(config(&[("PRIMARY_BASE_URL", &url)]))).await;
    let body = json!({
        "model": "primary/m1",
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let response = test::call_service(&app, post_chat(&body).to_request()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-cache");
    assert_eq!(
        response.headers().get("x-gateway-upstream").unwrap(),
        "primary"
    );
    let streamed = test::read_body(response).await;
    assert_eq!(String::from_utf8_lossy(&streamed), sse_stream());
}

#[actix_web::test]
async fn injects_include_usage_and_strips_the_chunk() {
    let server = upstream(sse_response()).await;
    let url = mock_url(&server);
    let app = init(state(config(&[("PRIMARY_BASE_URL", &url)]))).await;
    let body = json!({
        "model": "primary/m1",
        "stream": true,
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let response = test::call_service(&app, post_chat(&body).to_request()).await;
    assert_eq!(response.status(), 200);
    let streamed = test::read_body(response).await;
    assert_eq!(
        String::from_utf8_lossy(&streamed),
        sse_stream().replace(USAGE_EVENT, "")
    );

    let received = received_bodies(&server).await;
    assert_eq!(received[0]["stream_options"]["include_usage"], true);
    assert_eq!(received[0]["stream"], true);

    let stats_request = TestRequest::get()
        .uri("/v1/stats")
        .insert_header(bearer(USER_KEY))
        .to_request();
    let stats = json_body(test::call_service(&app, stats_request).await).await;
    assert_eq!(stats["total_prompt_tokens"], 5);
    assert_eq!(stats["total_completion_tokens"], 2);
}

#[actix_web::test]
async fn upstream_401_becomes_502_without_its_body() {
    let leaky = json!({"error": {"message": "Incorrect API key provided: sk-proj-****abcd"}});
    let server = upstream(ResponseTemplate::new(401).set_body_json(leaky)).await;
    let url = mock_url(&server);
    let app = init(state(config(&[("PRIMARY_BASE_URL", &url)]))).await;

    let response = test::call_service(&app, post_chat(&chat_body("primary/m1")).to_request()).await;
    assert_eq!(response.status(), 502);
    assert_eq!(
        response.headers().get("x-gateway-upstream").unwrap(),
        "primary"
    );
    let text = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
    assert!(!text.contains("sk-proj"), "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["error"]["code"], "upstream_auth_failed");
}

#[actix_web::test]
async fn upstream_404_is_passed_through() {
    let raw = r#"{"error":{"message":"model 'm1' not found","type":"invalid_request_error"}}"#;
    let server = upstream(ResponseTemplate::new(404).set_body_raw(raw, "application/json")).await;
    let url = mock_url(&server);
    let app = init(state(config(&[("PRIMARY_BASE_URL", &url)]))).await;

    let response = test::call_service(&app, post_chat(&chat_body("primary/m1")).to_request()).await;
    assert_eq!(response.status(), 404);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(test::read_body(response).await, raw.as_bytes());
}

#[actix_web::test]
async fn upstream_500_without_fallback_becomes_502() {
    let server =
        upstream(ResponseTemplate::new(500).set_body_string("database password is hunter2")).await;
    let url = mock_url(&server);
    let app = init(state(config(&[("PRIMARY_BASE_URL", &url)]))).await;

    let response = test::call_service(&app, post_chat(&chat_body("primary/m1")).to_request()).await;
    assert_eq!(response.status(), 502);
    let text = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
    assert!(!text.contains("hunter2"), "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["error"]["code"], "upstream_error");
}

#[actix_web::test]
async fn oversized_body_returns_413() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let url = mock_url(&server);
    let app = init(state(config(&[
        ("PRIMARY_BASE_URL", &url),
        ("MAX_REQUEST_BYTES", "1024"),
    ])))
    .await;
    let body =
        json!({"model": "primary/m1", "messages": [{"role": "user", "content": "x".repeat(2000)}]});

    let response = test::call_service(&app, post_chat(&body).to_request()).await;
    assert_eq!(response.status(), 413);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "payload_too_large"
    );
}

#[actix_web::test]
async fn accepts_body_larger_than_actix_default() {
    let server = upstream(ok_json(json!({"model": "m1", "choices": []}))).await;
    let url = mock_url(&server);
    let app = init(state(config(&[("PRIMARY_BASE_URL", &url)]))).await;
    let long = "x".repeat(300 * 1024);
    let body = json!({"model": "primary/m1", "messages": [{"role": "user", "content": long}]});

    let response = test::call_service(&app, post_chat(&body).to_request()).await;
    assert_eq!(response.status(), 200);
    let received = received_bodies(&server).await;
    assert_eq!(received[0]["messages"][0]["content"], long);
}

#[actix_web::test]
async fn bad_bodies_return_400() {
    let app = init(state(config(&[]))).await;
    let bodies = [
        "not json".to_string(),
        json!({"messages": []}).to_string(),
        json!({"model": 5}).to_string(),
    ];
    for body in bodies {
        let request = TestRequest::post()
            .uri(CHAT)
            .insert_header(bearer(USER_KEY))
            .insert_header(("content-type", "application/json"))
            .set_payload(body.clone())
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), 400, "{body}");
        assert_eq!(
            json_body(response).await["error"]["type"],
            "invalid_request_error",
            "{body}"
        );
    }
}

#[actix_web::test]
async fn unknown_prefix_without_default_returns_400() {
    let app = init(state(config(&[]))).await;
    let response = test::call_service(&app, post_chat(&chat_body("gemma4:e2b")).to_request()).await;
    assert_eq!(response.status(), 400);
    let body = json_body(response).await;
    assert_eq!(body["error"]["type"], "invalid_request_error");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("primary/, backup/"), "{message}");
}

#[actix_web::test]
async fn unprefixed_model_goes_to_the_default() {
    let server = upstream(ok_json(json!({"model": "m1", "choices": []}))).await;
    let url = mock_url(&server);
    let app = init(state(config(&[
        ("PRIMARY_BASE_URL", &url),
        ("DEFAULT_UPSTREAM", "primary"),
    ])))
    .await;

    let response = test::call_service(&app, post_chat(&chat_body("m1")).to_request()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(received_bodies(&server).await[0]["model"], "m1");
}
