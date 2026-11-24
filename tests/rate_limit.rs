mod common;

use actix_web::test::{self, TestRequest};
use common::{bearer, config, init, json_body, state, ADMIN_KEY, USER_KEY};

fn stats_as(key: &str) -> actix_http::Request {
    TestRequest::get()
        .uri("/v1/stats")
        .insert_header(bearer(key))
        .to_request()
}

#[actix_web::test]
async fn over_the_limit_gets_429_with_retry_after() {
    let app = init(state(config(&[("RATE_LIMIT_RPM", "2")]))).await;
    for _ in 0..2 {
        assert_eq!(
            test::call_service(&app, stats_as(USER_KEY)).await.status(),
            200
        );
    }

    let response = test::call_service(&app, stats_as(USER_KEY)).await;
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers().get("retry-after").unwrap(), "30");
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], "rate_limit_exceeded");
    assert_eq!(body["error"]["type"], "rate_limit_error");
}

#[actix_web::test]
async fn health_is_not_rate_limited() {
    let app = init(state(config(&[("RATE_LIMIT_RPM", "1")]))).await;
    for _ in 0..5 {
        let request = TestRequest::get().uri("/health").to_request();
        assert_eq!(test::call_service(&app, request).await.status(), 200);
    }
}

#[actix_web::test]
async fn admin_keys_are_limited_too() {
    let app = init(state(config(&[("RATE_LIMIT_RPM", "1")]))).await;
    assert_eq!(
        test::call_service(&app, stats_as(ADMIN_KEY)).await.status(),
        200
    );
    let response = test::call_service(&app, stats_as(ADMIN_KEY)).await;
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers().get("retry-after").unwrap(), "60");
    // Each key has its own bucket.
    assert_eq!(
        test::call_service(&app, stats_as(USER_KEY)).await.status(),
        200
    );
}
