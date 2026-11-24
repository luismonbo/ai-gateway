mod common;

use actix_web::test::{self, TestRequest};
use common::{bearer, capture_logs, config, init, state, USER_KEY};
use std::sync::PoisonError;

#[actix_web::test]
async fn access_log_has_the_path_but_not_the_query() {
    let (_guard, logs) = capture_logs();
    let app = init(state(config(&[]))).await;
    let requests = [
        TestRequest::get()
            .uri("/v1/stats?key_id=sk-secret-in-url")
            .insert_header(bearer(USER_KEY))
            .to_request(),
        // Rejected by auth and unknown routes are logged too.
        TestRequest::get()
            .uri("/v1/stats?token=sk-unauthenticated")
            .to_request(),
        TestRequest::get().uri("/nope?x=sk-not-found").to_request(),
    ];
    for request in requests {
        test::call_service(&app, request).await;
    }

    let logged =
        String::from_utf8_lossy(&logs.lock().unwrap_or_else(PoisonError::into_inner)).into_owned();
    for secret in ["sk-secret-in-url", "sk-unauthenticated", "sk-not-found"] {
        assert!(
            !logged.contains(secret),
            "{secret} reached the log: {logged}"
        );
    }
    for (path, status) in [("/v1/stats", 200), ("/v1/stats", 401), ("/nope", 404)] {
        let line = format!("method=GET path={path} status={status}");
        let found = logged
            .lines()
            .any(|entry| entry.contains(" access: ") && entry.contains(&line));
        assert!(found, "no access line with {line:?}: {logged}");
    }
}
