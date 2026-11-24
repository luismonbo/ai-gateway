use super::*;
use actix_web::body::to_bytes;
use actix_web::http::header::{CONTENT_TYPE, RETRY_AFTER};
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const GATEWAY_KEY: &str = "gateway-key-0123456789abcdef0123456789";
const UPSTREAM_KEY: &str = "sk-upstream-test-0123456789";
const LEAKY_BODY: &[u8] =
    br#"{"error":{"message":"Incorrect API key provided: sk-proj-****abcd"}}"#;

/// A config whose only upstream, `mock`, lives at `base_url`.
fn config(base_url: &str, api_key: Option<&str>) -> Config {
    let vars: Vec<(&str, &str)> = [
        ("GATEWAY_API_KEYS", GATEWAY_KEY),
        ("UPSTREAMS", "mock"),
        ("MOCK_BASE_URL", base_url),
    ]
    .into_iter()
    .chain(api_key.map(|key| ("MOCK_API_KEY", key)))
    .collect();
    let lookup = |name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    };
    let (config, _) = Config::from_lookup(lookup).expect("the test config is valid");
    config
}

fn mock(config: &Config) -> &Upstream {
    config.upstream("mock").expect("configured")
}

fn failure(code: u16, retry_after: Option<&'static str>) -> UpstreamFailure {
    UpstreamFailure::Status {
        status: StatusCode::from_u16(code).unwrap(),
        retry_after: retry_after.map(HeaderValue::from_static),
        content_type: Some(HeaderValue::from_static("application/json")),
        body: Bytes::from_static(LEAKY_BODY),
    }
}

fn status(code: u16) -> UpstreamFailure {
    failure(code, None)
}

/// A URL where nothing listens: the port was free a moment ago.
fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}/v1")
}

#[test]
fn fallback_triggers_match_the_spec() {
    assert!(should_fallback(&UpstreamFailure::Unreachable(
        "refused".into()
    )));
    assert!(should_fallback(&UpstreamFailure::Timeout));
    for s in [500, 502, 503, 429] {
        assert!(should_fallback(&status(s)), "{s}");
    }
    for s in [400, 401, 403, 404, 413, 422, 302] {
        assert!(!should_fallback(&status(s)), "{s}");
    }
}

#[actix_web::test]
async fn failures_map_to_the_error_table() {
    let unreachable = config("http://127.0.0.1:9/v1", None);
    let cases = [
        (
            UpstreamFailure::Unreachable("refused".into()),
            502,
            "upstream_unavailable",
        ),
        (UpstreamFailure::Timeout, 504, "upstream_timeout"),
        (status(500), 502, "upstream_error"),
        (status(401), 502, "upstream_auth_failed"),
        (status(403), 502, "upstream_auth_failed"),
        (status(302), 502, "upstream_error"),
        (failure(429, Some("20")), 429, "upstream_rate_limited"),
    ];
    for (failure, expected_status, expected_code) in cases {
        let response = failure_response(mock(&unreachable), &failure);
        assert_eq!(response.status().as_u16(), expected_status, "{failure}");
        let retry_after = response.headers().get(RETRY_AFTER).cloned();

        let body = to_bytes(response.into_body()).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], expected_code, "{failure}");
        assert!(json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("'mock'"));
        let text = String::from_utf8_lossy(&body);
        assert!(!text.contains("sk-proj"), "{failure}: {text}");

        if expected_status == 429 {
            assert_eq!(retry_after.unwrap(), "20");
        }
    }
}

#[actix_web::test]
async fn other_4xx_bodies_pass_through() {
    let unreachable = config("http://127.0.0.1:9/v1", None);
    let body = Bytes::from_static(br#"{"error":{"message":"model 'nope' not found"}}"#);
    let failure = UpstreamFailure::Status {
        status: StatusCode::NOT_FOUND,
        retry_after: None,
        content_type: Some(HeaderValue::from_static("application/json")),
        body: body.clone(),
    };
    let response = failure_response(mock(&unreachable), &failure);
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/json"
    );
    assert_eq!(to_bytes(response.into_body()).await.unwrap(), body);
}

#[test]
fn described_failures_are_truncated_and_redacted() {
    let config = config("http://127.0.0.1:9/v1", Some(UPSTREAM_KEY));
    let body = format!("{UPSTREAM_KEY}{}", "x".repeat(2000));
    let failure = UpstreamFailure::Status {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        retry_after: None,
        content_type: None,
        body: Bytes::from(body),
    };
    let description = describe_failure(mock(&config), &failure);
    assert!(description.contains("[redacted]"), "{description}");
    assert!(!description.contains(UPSTREAM_KEY), "{description}");
    assert!(description.len() < 700, "{} chars", description.len());
    assert!(!format!("{failure:?}").contains(UPSTREAM_KEY));
}

#[actix_web::test]
async fn send_chat_sends_only_the_upstream_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .mount(&server)
        .await;
    let base_url = format!("{}/v1", server.uri());
    let body = br#"{"model":"m"}"#.to_vec();

    for key in [Some(UPSTREAM_KEY), None] {
        let config = config(&base_url, key);
        let client = build_http_client(&config).unwrap();
        let response = send_chat(&client, mock(&config), body.clone())
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
    }

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].headers["authorization"],
        format!("Bearer {UPSTREAM_KEY}").as_str()
    );
    assert!(requests[1].headers.get("authorization").is_none());
    for request in &requests {
        assert_eq!(request.body, body);
        assert_eq!(request.headers["content-type"], "application/json");
        let agent = request.headers["user-agent"].to_str().unwrap();
        assert!(agent.starts_with("ai-gateway/"), "{agent}");
    }
}

#[actix_web::test]
async fn send_chat_classifies_refused_connections() {
    let config = config(&closed_port_url(), None);
    let client = build_http_client(&config).unwrap();
    let failure = send_chat(&client, mock(&config), b"{}".to_vec())
        .await
        .unwrap_err();
    let UpstreamFailure::Unreachable(message) = &failure else {
        panic!("expected Unreachable, got {failure:?}");
    };
    assert!(message.to_lowercase().contains("refused"), "{message}");
    assert!(!message.contains("http://"), "has the URL: {message}");
}

#[actix_web::test]
async fn send_chat_classifies_timeouts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)))
        .mount(&server)
        .await;
    let config = Config {
        upstream_timeout: Duration::from_millis(200),
        ..config(&format!("{}/v1", server.uri()), None)
    };
    let client = build_http_client(&config).unwrap();
    let failure = send_chat(&client, mock(&config), b"{}".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(failure, UpstreamFailure::Timeout), "{failure:?}");
}

#[actix_web::test]
async fn send_chat_returns_status_failures_with_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_string("busy")
                .insert_header("retry-after", "7"),
        )
        .mount(&server)
        .await;
    let config = config(&format!("{}/v1", server.uri()), None);
    let client = build_http_client(&config).unwrap();
    let failure = send_chat(&client, mock(&config), b"{}".to_vec())
        .await
        .unwrap_err();
    let UpstreamFailure::Status {
        status,
        retry_after,
        body,
        ..
    } = &failure
    else {
        panic!("expected Status, got {failure:?}");
    };
    assert_eq!(*status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, "busy");
    assert_eq!(retry_after.as_ref().unwrap(), "7");
}

/// A 500 failure from `mock` whose body is `body`.
fn failure_with_body(body: String) -> UpstreamFailure {
    UpstreamFailure::Status {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        retry_after: None,
        content_type: None,
        body: Bytes::from(body),
    }
}

#[test]
fn repeated_keys_are_redacted_before_the_cut() {
    // 51 characters, like the reviewer's repro: one copy near the start and
    // one that starts past byte 512 of the original body.
    let key = "sk-repeat-0123456789abcdefghijklmnopqrstuvwxyzABCDE";
    assert_eq!(key.len(), 51);
    let config = config("http://127.0.0.1:9/v1", Some(key));
    let head = format!("{}{key}", "a".repeat(20));
    let filler = "b".repeat(530 - head.len());
    let body = format!("{head}{filler}{key}{}", "c".repeat(2000));

    let description = describe_failure(mock(&config), &failure_with_body(body));
    assert!(description.contains("[redacted]"), "{description}");
    // Not even a 10-character piece of the key may survive.
    assert!(!description.contains(&key[..10]), "{description}");
}

#[test]
fn logged_excerpts_escape_line_breaks() {
    let config = config("http://127.0.0.1:9/v1", None);
    let body = "<html>\r\n<h1>502 Bad Gateway</h1>\nINFO forged: entry\t".to_string();

    let description = describe_failure(mock(&config), &failure_with_body(body));
    assert!(
        !description.contains(['\n', '\r', '\t']),
        "raw control characters: {description:?}"
    );
    assert!(
        description.contains(r"\r\n<h1>502 Bad Gateway</h1>\nINFO forged"),
        "{description}"
    );
}

#[actix_web::test]
async fn passed_through_bodies_never_carry_the_upstream_key() {
    let config = config("http://127.0.0.1:9/v1", Some(UPSTREAM_KEY));
    let body = format!(r#"{{"error":{{"message":"no model 'm' (key {UPSTREAM_KEY})"}}}}"#);
    let failure = UpstreamFailure::Status {
        status: StatusCode::NOT_FOUND,
        retry_after: None,
        content_type: Some(HeaderValue::from_static("application/json")),
        body: Bytes::from(body),
    };

    let response = failure_response(mock(&config), &failure);
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let sent = to_bytes(response.into_body()).await.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&sent),
        r#"{"error":{"message":"no model 'm' (key [redacted])"}}"#
    );
}
