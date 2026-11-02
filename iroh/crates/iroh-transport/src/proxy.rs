//! Forwards validated HTTP requests to the configured origin and returns streaming responses.

use std::time::Duration;

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Method, Request, Response, Uri};
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tokio_util::sync::CancellationToken;

use crate::protocol::{BodyMode, HttpRequest, error_code};

pub const UPSTREAM_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
];

pub type UpstreamClient = Client<HttpConnector, Empty<Bytes>>;

/// The local HTTP service that media requests are forwarded to. Plain HTTP on a trusted address only.
#[derive(Debug, Clone)]
pub struct Origin {
    base: Uri,
}

impl Origin {
    pub fn parse(value: &str) -> Result<Self, String> {
        let uri: Uri = value
            .parse()
            .map_err(|error| format!("origin is not a valid URL: {error}"))?;
        if uri.scheme_str() != Some("http") {
            return Err("origin must use http:// (https origins are not supported yet)".into());
        }
        if uri.authority().is_none() {
            return Err(
                "origin must include a host and port, for example http://127.0.0.1:8096".into(),
            );
        }
        if uri.path() != "/" && !uri.path().is_empty() {
            return Err("origin must not include a path".into());
        }
        if uri.query().is_some() {
            return Err("origin must not include a query string".into());
        }
        Ok(Self { base: uri })
    }

    fn target(&self, path: &str) -> Result<Uri, ProxyError> {
        let authority = self
            .base
            .authority()
            .map(|a| a.as_str())
            .unwrap_or_default();
        format!("http://{authority}{path}")
            .parse()
            .map_err(|_| ProxyError::bad_request("path is not a valid request target"))
    }

    pub fn display(&self) -> String {
        self.base.to_string()
    }

    /// Host and port to open a TCP connection to. IPv6 brackets are removed.
    pub fn host_port(&self) -> (String, u16) {
        let host = self.base.host().unwrap_or_default();
        let host = host.trim_start_matches('[').trim_end_matches(']');
        (host.to_owned(), self.base.port_u16().unwrap_or(80))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyError {
    pub code: &'static str,
    pub message: String,
}

impl ProxyError {
    fn bad_request(message: &str) -> Self {
        Self {
            code: error_code::BAD_REQUEST,
            message: message.to_owned(),
        }
    }
}

pub fn new_upstream_client() -> UpstreamClient {
    Client::builder(TokioExecutor::new()).build_http()
}

/// Sends the request to the origin and resolves once response headers arrive.
///
/// The returned body streams from the origin; dropping it closes the upstream connection.
pub async fn forward(
    client: &UpstreamClient,
    origin: &Origin,
    request: &HttpRequest,
    cancel: &CancellationToken,
) -> Result<Response<Incoming>, ProxyError> {
    if request.body != BodyMode::None {
        return Err(ProxyError {
            code: error_code::UNSUPPORTED,
            message: "request bodies are not supported by this host".into(),
        });
    }
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|_| ProxyError::bad_request("method is not valid"))?;
    let mut builder = Request::builder()
        .method(method)
        .uri(origin.target(&request.path)?);
    for (name, value) in &request.headers {
        if is_hop_by_hop(name) {
            continue;
        }
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            return Err(ProxyError::bad_request(
                "a header name or value is not valid",
            ));
        };
        builder = builder.header(name, value);
    }
    let upstream_request = builder
        .body(Empty::<Bytes>::new())
        .map_err(|_| ProxyError::bad_request("request could not be built"))?;

    let response = tokio::select! {
        _ = cancel.cancelled() => {
            return Err(ProxyError {
                code: error_code::UPSTREAM_UNAVAILABLE,
                message: "request was cancelled".into(),
            });
        }
        result = tokio::time::timeout(UPSTREAM_RESPONSE_TIMEOUT, client.request(upstream_request)) => {
            match result {
                Err(_) => {
                    return Err(ProxyError {
                        code: error_code::UPSTREAM_TIMEOUT,
                        message: "origin did not respond in time".into(),
                    });
                }
                Ok(Err(error)) => {
                    return Err(ProxyError {
                        code: error_code::UPSTREAM_UNAVAILABLE,
                        message: format!("origin is unavailable: {error}"),
                    });
                }
                Ok(Ok(response)) => response,
            }
        }
    };
    Ok(response)
}

/// Copies response headers that are safe to pass to the client.
pub fn response_headers(response: &Response<Incoming>) -> Vec<(String, String)> {
    response
        .headers()
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name.as_str()))
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP_HEADERS
        .iter()
        .any(|hop| name.eq_ignore_ascii_case(hop))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_http_origins() {
        let origin = Origin::parse("http://127.0.0.1:8096").unwrap();
        assert_eq!(origin.host_port(), ("127.0.0.1".to_owned(), 8096));
        let origin = Origin::parse("http://[::1]:8096/").unwrap();
        assert_eq!(origin.host_port(), ("::1".to_owned(), 8096));
        assert_eq!(Origin::parse("http://jellyfin").unwrap().host_port().1, 80);
    }

    #[test]
    fn rejects_unsupported_origins() {
        for bad in [
            "https://127.0.0.1:8096",
            "127.0.0.1:8096",
            "http://127.0.0.1:8096/jellyfin",
            "http://127.0.0.1:8096/?a=b",
            "not a url",
        ] {
            assert!(Origin::parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn builds_targets_from_origin_form_paths() {
        let origin = Origin::parse("http://127.0.0.1:8096").unwrap();
        assert_eq!(
            origin.target("/Items?limit=1").unwrap().to_string(),
            "http://127.0.0.1:8096/Items?limit=1"
        );
    }

    #[test]
    fn strips_hop_by_hop_headers_case_insensitively() {
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("HOST"));
        assert!(!is_hop_by_hop("range"));
    }

    #[tokio::test]
    async fn refuses_request_bodies() {
        let origin = Origin::parse("http://127.0.0.1:9").unwrap();
        let request = HttpRequest {
            request_id: "r1".into(),
            method: "POST".into(),
            path: "/".into(),
            headers: vec![],
            body: BodyMode::Stream,
            content_length: None,
        };
        let error = forward(
            &new_upstream_client(),
            &origin,
            &request,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, error_code::UNSUPPORTED);
    }
}
