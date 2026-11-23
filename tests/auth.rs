mod common;

use actix_web::test::{self, TestRequest};
use ai_gateway::auth::KeyId;
use common::{bearer, config, init, json_body, state, ADMIN_KEY, USER_KEY};

#[actix_web::test]
async fn health_needs_no_key() {
    let app = init(state(config(&[]))).await;
    let response = test::call_service(&app, TestRequest::get().uri("/health").to_request()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(test::read_body(response).await, "ok");
}

#[actix_web::test]
async fn missing_key_gets_openai_401() {
    let app = init(state(config(&[]))).await;
    let request = TestRequest::get().uri("/v1/stats").to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), 401);
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], "invalid_api_key");
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

#[actix_web::test]
async fn wrong_key_gets_401() {
    let app = init(state(config(&[]))).await;
    let request = TestRequest::get()
        .uri("/v1/stats")
        .insert_header(bearer("sk-wrong-key-0123456789"))
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), 401);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "invalid_api_key"
    );
}

#[actix_web::test]
async fn lowercase_bearer_scheme_is_accepted() {
    let app = init(state(config(&[]))).await;
    let request = TestRequest::get()
        .uri("/v1/stats")
        .insert_header(("Authorization", format!("bearer {USER_KEY}")))
        .to_request();
    assert_eq!(test::call_service(&app, request).await.status(), 200);
}

#[actix_web::test]
async fn user_and_admin_keys_are_accepted() {
    let app = init(state(config(&[]))).await;
    for (role, key) in [("user", USER_KEY), ("admin", ADMIN_KEY)] {
        let request = TestRequest::get()
            .uri("/v1/stats")
            .insert_header(bearer(key))
            .to_request();
        assert_eq!(
            test::call_service(&app, request).await.status(),
            200,
            "{role}"
        );
    }
}

#[actix_web::test]
async fn old_v1_health_route_is_gone() {
    let app = init(state(config(&[]))).await;
    let request = TestRequest::get()
        .uri("/v1/health")
        .insert_header(bearer(USER_KEY))
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), 404);
    assert_eq!(json_body(response).await["error"]["code"], "not_found");
}

#[actix_web::test]
async fn stats_show_key_id_not_key() {
    let app = init(state(config(&[]))).await;
    let request = TestRequest::get()
        .uri("/v1/stats")
        .insert_header(bearer(USER_KEY))
        .to_request();
    let response = test::call_service(&app, request).await;
    assert_eq!(response.status(), 200);
    let text = String::from_utf8(test::read_body(response).await.to_vec()).unwrap();
    assert!(!text.contains(USER_KEY), "{text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["key_id"], KeyId::from_raw_key(USER_KEY).as_str());
}
