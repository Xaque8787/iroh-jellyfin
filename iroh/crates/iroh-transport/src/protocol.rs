//! Transport protocol v1 message definitions (see protocol/transport-v1.md).

use serde::{Deserialize, Serialize};

use crate::policy::TransportPolicy;

/// Application protocol identifier negotiated by Iroh (ALPN).
pub const ALPN: &[u8] = b"jellyfin-iroh/1";

pub const PROTOCOL_VERSION: u32 = 1;

/// Capability announced in HELLO and AUTH_RESULT, and the only accepted `upgrade` value.
pub const WEBSOCKET: &str = "websocket";

/// Upper bound for control-stream frames (HELLO, AUTH, PING).
pub const MAX_CONTROL_FRAME_BYTES: usize = 16 * 1024;

/// Upper bound for HTTP request and response header frames.
pub const MAX_HTTP_FRAME_BYTES: usize = 64 * 1024;

pub const MAX_HEADER_COUNT: usize = 128;
pub const MAX_REQUEST_ID_BYTES: usize = 128;
pub const MAX_PATH_BYTES: usize = 8 * 1024;

/// Largest request body the host forwards. Media playback sends small JSON bodies only.
pub const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Largest chunk read from an Iroh stream at once when relaying bodies.
pub const BODY_CHUNK_BYTES: usize = 64 * 1024;

pub mod error_code {
    pub const PROTOCOL_VERSION_UNSUPPORTED: &str = "protocol_version_unsupported";
    pub const PROTOCOL_VIOLATION: &str = "protocol_violation";
    pub const NOT_AUTHORIZED: &str = "not_authorized";
    pub const PAIRING_TOKEN_INVALID: &str = "pairing_token_invalid";
    pub const PAIRING_RATE_LIMITED: &str = "pairing_rate_limited";
    pub const TRANSPORT_POLICY_DENIED: &str = "transport_policy_denied";
    pub const BAD_REQUEST: &str = "bad_request";
    pub const UNSUPPORTED: &str = "unsupported";
    pub const PAYLOAD_TOO_LARGE: &str = "payload_too_large";
    pub const UPSTREAM_UNAVAILABLE: &str = "upstream_unavailable";
    pub const UPSTREAM_TIMEOUT: &str = "upstream_timeout";
    pub const REVOKED: &str = "revoked";
    pub const SHUTTING_DOWN: &str = "shutting_down";
    pub const TOO_MANY_WEBSOCKETS: &str = "too_many_websockets";
}

/// Application-level QUIC close codes. Stream resets use these values too.
pub mod close_code {
    pub const NORMAL: u32 = 0;
    pub const PROTOCOL_VIOLATION: u32 = 1;
    pub const NOT_AUTHORIZED: u32 = 2;
    pub const REVOKED: u32 = 3;
    pub const TRANSPORT_POLICY: u32 = 4;
    pub const SHUTTING_DOWN: u32 = 5;
    pub const IDLE_TIMEOUT: u32 = 6;
    /// Stream reset only: the origin failed while the response body was being sent.
    pub const UPSTREAM_FAILED: u32 = 7;
}

pub type Header = (String, String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Message {
    Hello(Hello),
    AuthRequest(AuthRequest),
    AuthResult(AuthResult),
    HttpRequest(HttpRequest),
    HttpResponse(HttpResponse),
    StreamError(StreamError),
    Ping(Ping),
    Pong(Ping),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthRequest {
    pub protocol: u32,
    /// Present only when redeeming an invitation. Never used as a session credential.
    #[serde(default)]
    pub pairing_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthOutcome {
    /// The connecting endpoint was already an authorized device.
    Authorized,
    /// The pairing token was redeemed and the endpoint is now an authorized device.
    Paired,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthResult {
    pub outcome: AuthOutcome,
    #[serde(default)]
    pub reason: Option<String>,
    pub transport_policy: TransportPolicy,
    /// Optional features this host serves on the connection, such as `websocket`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyMode {
    /// No request body. The request stream is finished immediately after the header.
    None,
    /// The request body follows the header and ends when the stream is finished.
    Stream,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    pub request_id: String,
    pub method: String,
    /// Origin-form request target: path plus optional query, exactly as the client sent it.
    pub path: String,
    #[serde(default)]
    pub headers: Vec<Header>,
    pub body: BodyMode,
    #[serde(default)]
    pub content_length: Option<u64>,
    /// `websocket` when the request opens a WebSocket. After a 101 response the stream carries raw
    /// WebSocket bytes in both directions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpResponse {
    pub request_id: String,
    pub status: u16,
    #[serde(default)]
    pub headers: Vec<Header>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamError {
    #[serde(default)]
    pub request_id: Option<String>,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub nonce: u64,
}

/// Returns why an HTTP request header is unacceptable, if it is.
pub fn validate_http_request(request: &HttpRequest) -> Result<(), &'static str> {
    if request.request_id.is_empty() || request.request_id.len() > MAX_REQUEST_ID_BYTES {
        return Err("request_id must be 1 to 128 bytes");
    }
    if request.method.is_empty()
        || !request
            .method
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'-')
    {
        return Err("method must be an uppercase HTTP token");
    }
    if request.path.len() > MAX_PATH_BYTES {
        return Err("path is too long");
    }
    if !request.path.starts_with('/') || request.path.starts_with("//") {
        return Err("path must be origin-form and start with a single slash");
    }
    if request
        .path
        .bytes()
        .any(|byte| byte <= b' ' || byte == 0x7f)
    {
        return Err("path contains control characters or spaces");
    }
    if request.headers.len() > MAX_HEADER_COUNT {
        return Err("too many headers");
    }
    match (request.body, request.content_length) {
        (BodyMode::None, Some(length)) if length > 0 => {
            return Err("content_length requires body stream");
        }
        (BodyMode::Stream, Some(length)) if length > MAX_REQUEST_BODY_BYTES as u64 => {
            return Err("request body is too large");
        }
        _ => {}
    }
    if let Some(upgrade) = &request.upgrade {
        if upgrade != WEBSOCKET {
            return Err("only websocket upgrades are supported");
        }
        if request.method != "GET" || request.body != BodyMode::None {
            return Err("a websocket upgrade must be a GET without a body");
        }
    }
    Ok(())
}

pub fn validate_http_response(response: &HttpResponse) -> Result<(), &'static str> {
    if response.headers.len() > MAX_HEADER_COUNT {
        return Err("too many headers");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(path: &str) -> HttpRequest {
        HttpRequest {
            request_id: "r1".into(),
            method: "GET".into(),
            path: path.into(),
            headers: vec![("range".into(), "bytes=0-99".into())],
            body: BodyMode::None,
            content_length: None,
            upgrade: None,
        }
    }

    #[test]
    fn http_request_wire_format_is_stable() {
        let value =
            serde_json::to_value(Message::HttpRequest(request("/Videos/1/stream"))).unwrap();
        assert_eq!(
            value,
            json!({
                "type": "HTTP_REQUEST",
                "request_id": "r1",
                "method": "GET",
                "path": "/Videos/1/stream",
                "headers": [["range", "bytes=0-99"]],
                "body": "none",
                "content_length": null
            })
        );
    }

    #[test]
    fn auth_result_wire_format_is_stable() {
        let message = Message::AuthResult(AuthResult {
            outcome: AuthOutcome::Paired,
            reason: None,
            transport_policy: TransportPolicy::P2pOnly,
            capabilities: vec![],
        });
        assert_eq!(
            serde_json::to_value(message).unwrap(),
            json!({"type": "AUTH_RESULT", "outcome": "paired", "reason": null, "transport_policy": "p2p_only"})
        );
    }

    #[test]
    fn websocket_fields_are_optional_on_the_wire() {
        let message: Message = serde_json::from_value(json!({
            "type": "AUTH_RESULT", "outcome": "authorized", "transport_policy": "auto",
            "capabilities": ["websocket"]
        }))
        .unwrap();
        let Message::AuthResult(result) = message else {
            panic!("expected AUTH_RESULT");
        };
        assert_eq!(result.capabilities, vec![WEBSOCKET.to_owned()]);

        let mut upgrade = request("/socket");
        upgrade.upgrade = Some(WEBSOCKET.into());
        let value = serde_json::to_value(Message::HttpRequest(upgrade)).unwrap();
        assert_eq!(value["upgrade"], "websocket");
        let old = serde_json::to_value(Message::HttpRequest(request("/socket"))).unwrap();
        assert!(old.get("upgrade").is_none());
    }

    #[test]
    fn checks_websocket_upgrades() {
        let mut upgrade = request("/socket");
        upgrade.upgrade = Some(WEBSOCKET.into());
        assert!(validate_http_request(&upgrade).is_ok());
        let mut other = upgrade.clone();
        other.upgrade = Some("h2c".into());
        assert!(validate_http_request(&other).is_err());
        let mut post = upgrade.clone();
        post.method = "POST".into();
        assert!(validate_http_request(&post).is_err());
        let mut with_body = upgrade;
        with_body.body = BodyMode::Stream;
        assert!(validate_http_request(&with_body).is_err());
    }

    #[test]
    fn hello_tolerates_missing_optional_fields() {
        let message: Message =
            serde_json::from_value(json!({"type": "HELLO", "protocol": 1, "name": "TV"})).unwrap();
        assert_eq!(
            message,
            Message::Hello(Hello {
                protocol: 1,
                name: "TV".into(),
                capabilities: vec![]
            })
        );
    }

    #[test]
    fn accepts_origin_form_paths() {
        assert!(validate_http_request(&request("/Items?userId=1&limit=5")).is_ok());
    }

    #[test]
    fn rejects_unsafe_request_targets() {
        for path in [
            "Items",
            "//evil.example/x",
            "http://evil.example/",
            "/a b",
            "/a\r\nX: y",
        ] {
            assert!(
                validate_http_request(&request(path)).is_err(),
                "{path:?} should be rejected"
            );
        }
        let mut long = request("/");
        long.path = format!("/{}", "a".repeat(MAX_PATH_BYTES));
        assert!(validate_http_request(&long).is_err());
    }

    #[test]
    fn rejects_bad_methods_ids_and_header_counts() {
        let mut bad = request("/");
        bad.method = "get".into();
        assert!(validate_http_request(&bad).is_err());

        let mut bad = request("/");
        bad.request_id = String::new();
        assert!(validate_http_request(&bad).is_err());

        let mut bad = request("/");
        bad.headers = vec![("x".into(), "y".into()); MAX_HEADER_COUNT + 1];
        assert!(validate_http_request(&bad).is_err());
    }

    #[test]
    fn checks_declared_body_lengths() {
        let mut req = request("/Users/AuthenticateByName");
        req.content_length = Some(10);
        assert!(validate_http_request(&req).is_err());
        req.body = BodyMode::Stream;
        assert!(validate_http_request(&req).is_ok());
        req.content_length = Some(MAX_REQUEST_BODY_BYTES as u64 + 1);
        assert!(validate_http_request(&req).is_err());
    }
}
