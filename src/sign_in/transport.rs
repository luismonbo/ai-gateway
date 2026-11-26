//! The gateway's reqwest client as the Azure crates' HTTP transport, so
//! sign-in requests use its settings (no redirects, its timeouts) and the
//! crates need no HTTP stack of their own.
//!
//! Ported from Microsoft's glue for reqwest 0.13 in `typespec_client_core`
//! 1.1 (`src/http/clients/reqwest.rs`) to the gateway's reqwest 0.12.

use async_trait::async_trait;
use azure_core::error::{Error, ErrorKind};
use azure_core::http::headers::{HeaderName, HeaderValue, Headers};
use azure_core::http::{AsyncRawResponse, Body, HttpClient, Request};
use std::collections::HashMap;
use tracing::warn;

/// azure_core's HTTP client trait over the gateway's reqwest 0.12 client.
#[derive(Debug, Clone)]
pub struct ReqwestTransport(reqwest::Client);

impl ReqwestTransport {
    pub fn new(client: reqwest::Client) -> Self {
        Self(client)
    }
}

#[async_trait]
impl HttpClient for ReqwestTransport {
    async fn execute_request(&self, request: &Request) -> azure_core::Result<AsyncRawResponse> {
        let Body::Bytes(body) = request.body() else {
            return Err(Error::with_message(
                ErrorKind::Other,
                "sign-in requests never stream their body",
            ));
        };
        let method = reqwest::Method::from_bytes(request.method().as_str().as_bytes())
            .map_err(|e| Error::with_error(ErrorKind::Other, e, "unsupported HTTP method"))?;
        let builder = request.headers().iter().fold(
            self.0.request(method, request.url().clone()),
            |builder, (name, value)| builder.header(name.as_str(), value.as_str()),
        );
        let response = builder.body(body.clone()).send().await.map_err(failed)?;
        let status = response.status().as_u16();
        let headers = headers(response.headers());
        // Token responses are small, so they're read whole.
        let body = response.bytes().await.map_err(failed)?;
        Ok(AsyncRawResponse::from_bytes(status.into(), headers, body))
    }
}

/// A connection failure is a `Connection` error, anything else `Io`; the
/// Azure crates retry both. The error never carries the URL.
fn failed(error: reqwest::Error) -> Error {
    let kind = if error.is_connect() {
        ErrorKind::Connection
    } else {
        ErrorKind::Io
    };
    Error::with_error(kind, error.without_url(), "the sign-in request failed")
}

/// The response's headers, for the Azure crates. A value that isn't text is
/// left out, as Microsoft's glue does.
fn headers(map: &reqwest::header::HeaderMap) -> Headers {
    let pairs: HashMap<HeaderName, HeaderValue> = map
        .iter()
        .filter_map(|(name, value)| match value.to_str() {
            Ok(value) => Some((
                HeaderName::from(name.as_str().to_owned()),
                HeaderValue::from(value.to_owned()),
            )),
            Err(_) => {
                warn!(header = %name, "a sign-in response header isn't text; leaving it out");
                None
            }
        })
        .collect();
    Headers::from(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use azure_core::error::ErrorKind;
    use azure_core::http::{HttpClient, Method, Request, StatusCode, Url};
    use azure_core::Bytes;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn transport() -> ReqwestTransport {
        ReqwestTransport::new(reqwest::Client::new())
    }

    #[actix_web::test]
    async fn sends_the_request_as_given() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let url = Url::parse(&format!("{}/token?resource=r", server.uri())).unwrap();
        let mut request = Request::new(url, Method::Post);
        request.insert_header("metadata", "true");
        request.set_body(Bytes::from_static(b"grant_type=x"));

        let response = transport().execute_request(&request).await.unwrap();
        assert_eq!(response.status(), StatusCode::Ok);

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let sent = &received[0];
        assert_eq!(sent.method.as_str(), "POST");
        assert_eq!(sent.url.path(), "/token");
        assert_eq!(sent.url.query(), Some("resource=r"));
        assert_eq!(sent.headers["metadata"], "true");
        assert_eq!(sent.body, b"grant_type=x");
    }

    #[actix_web::test]
    async fn returns_status_headers_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(404)
                    .insert_header("x-test", "yes")
                    .set_body_string("nope"),
            )
            .mount(&server)
            .await;
        let request = Request::new(Url::parse(&server.uri()).unwrap(), Method::Get);

        let response = transport().execute_request(&request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NotFound);
        assert!(response
            .headers()
            .iter()
            .any(|(name, value)| name.as_str() == "x-test" && value.as_str() == "yes"));
        assert_eq!(response.into_body().collect().await.unwrap(), "nope");
    }

    #[actix_web::test]
    async fn connection_failures_are_connection_errors() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/token")).unwrap();

        let err = transport()
            .execute_request(&Request::new(url, Method::Get))
            .await
            .unwrap_err();
        assert_eq!(*err.kind(), ErrorKind::Connection);
        // Neither the error nor its sources repeat the URL.
        let chain = std::iter::successors(Some(&err as &dyn std::error::Error), |&e| e.source())
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(": ");
        assert!(!chain.contains(&port.to_string()), "{chain}");
    }
}
