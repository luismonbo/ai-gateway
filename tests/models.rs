mod common;

use actix_web::test::{self, TestRequest};
use common::{
    bearer, closed_port_url, config, init, json_body, mock_url, state, state_with_credentials,
    FakeCredential, UPSTREAM_KEY, USER_KEY,
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A mock upstream whose `GET /v1/models` lists `ids`.
async fn upstream_listing(ids: &[&str], delay: Duration) -> MockServer {
    let data: Vec<Value> = ids
        .iter()
        .map(|id| json!({"id": id, "object": "model", "created": 1, "owned_by": "someone"}))
        .collect();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"object": "list", "data": data}))
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    server
}

fn get_models() -> actix_http::Request {
    TestRequest::get()
        .uri("/v1/models")
        .insert_header(bearer(USER_KEY))
        .to_request()
}

fn ids(list: &Value) -> Vec<&str> {
    list["data"]
        .as_array()
        .expect("a data array")
        .iter()
        .map(|model| model["id"].as_str().expect("a string id"))
        .collect()
}

#[actix_web::test]
async fn lists_models_from_every_upstream() {
    // The primary answers last, so the order can't come from arrival time.
    let primary = upstream_listing(&["a", "b"], Duration::from_millis(100)).await;
    let backup = upstream_listing(&["c", "d"], Duration::ZERO).await;
    let (primary_url, backup_url) = (mock_url(&primary), mock_url(&backup));
    let app = init(state(config(&[
        ("PRIMARY_BASE_URL", &primary_url),
        ("BACKUP_BASE_URL", &backup_url),
    ])))
    .await;

    let response = test::call_service(&app, get_models()).await;
    assert_eq!(response.status(), 200);
    let list = json_body(response).await;
    assert_eq!(list["object"], "list");
    assert_eq!(
        ids(&list),
        ["primary/a", "primary/b", "backup/c", "backup/d"]
    );
    assert_eq!(list["data"][0]["owned_by"], "primary");
    assert_eq!(list["data"][2]["owned_by"], "backup");
    assert_eq!(list["data"][0]["object"], "model");
    assert_eq!(list["data"][0]["created"], 1);
}

#[actix_web::test]
async fn skips_a_down_upstream_and_returns_200() {
    let primary = upstream_listing(&["a"], Duration::ZERO).await;
    let (primary_url, backup_url) = (mock_url(&primary), closed_port_url());
    let app = init(state(config(&[
        ("PRIMARY_BASE_URL", &primary_url),
        ("BACKUP_BASE_URL", &backup_url),
    ])))
    .await;

    let response = test::call_service(&app, get_models()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(ids(&json_body(response).await), ["primary/a"]);
}

#[actix_web::test]
async fn sends_the_upstream_key() {
    let primary = upstream_listing(&["a"], Duration::ZERO).await;
    let backup = upstream_listing(&["b"], Duration::ZERO).await;
    let (primary_url, backup_url) = (mock_url(&primary), mock_url(&backup));
    let app = init(state(config(&[
        ("PRIMARY_BASE_URL", &primary_url),
        ("PRIMARY_API_KEY", UPSTREAM_KEY),
        ("BACKUP_BASE_URL", &backup_url),
    ])))
    .await;

    let response = test::call_service(&app, get_models()).await;
    assert_eq!(response.status(), 200);

    let to_primary = primary.received_requests().await.unwrap();
    assert_eq!(
        to_primary[0].headers.get("authorization").unwrap(),
        format!("Bearer {UPSTREAM_KEY}").as_str()
    );
    let to_backup = backup.received_requests().await.unwrap();
    assert!(to_backup[0].headers.get("authorization").is_none());
}

#[actix_web::test]
async fn requires_a_gateway_key() {
    let app = init(state(config(&[]))).await;
    let request = TestRequest::get().uri("/v1/models").to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), 401);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "invalid_api_key"
    );
}

#[actix_web::test]
async fn lists_configured_models_without_asking_the_upstream() {
    // Azure's own list holds models it could deploy, not its deployments.
    let primary = upstream_listing(&["not-a-deployment"], Duration::ZERO).await;
    let backup = upstream_listing(&["c"], Duration::ZERO).await;
    let (primary_url, backup_url) = (mock_url(&primary), mock_url(&backup));
    let credential = FakeCredential::failing();
    let config = config(&[
        ("PRIMARY_BASE_URL", &primary_url),
        ("PRIMARY_AUTH", "azure_cli"),
        ("PRIMARY_MODELS", "dep-a,dep-b"),
        ("BACKUP_BASE_URL", &backup_url),
    ]);
    let app = init(state_with_credentials(
        config,
        &[("primary", Arc::clone(&credential))],
    ))
    .await;

    let response = test::call_service(&app, get_models()).await;
    assert_eq!(response.status(), 200);
    let list = json_body(response).await;
    assert_eq!(ids(&list), ["primary/dep-a", "primary/dep-b", "backup/c"]);
    assert_eq!(list["data"][0]["owned_by"], "primary");
    // The upstream wasn't asked, so its failing sign-in never mattered.
    assert!(primary.received_requests().await.unwrap().is_empty());
    assert_eq!(credential.calls(), 0);
}

#[actix_web::test]
async fn signing_in_counts_toward_an_upstreams_time() {
    // Signing in and listing take 0.7 s each: either fits the 1 s bound,
    // but together they don't, so the slow upstream is left out.
    let primary = upstream_listing(&["a"], Duration::from_millis(700)).await;
    let backup = upstream_listing(&["c"], Duration::ZERO).await;
    let (primary_url, backup_url) = (mock_url(&primary), mock_url(&backup));
    let config = config(&[
        ("PRIMARY_BASE_URL", &primary_url),
        ("PRIMARY_AUTH", "azure_cli"),
        ("BACKUP_BASE_URL", &backup_url),
        ("UPSTREAM_CONNECT_TIMEOUT_SECS", "1"),
    ]);
    let credential = FakeCredential::slow("tok-1", Duration::from_millis(700));
    let app = init(state_with_credentials(config, &[("primary", credential)])).await;

    let response = test::call_service(&app, get_models()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(ids(&json_body(response).await), ["backup/c"]);
}
