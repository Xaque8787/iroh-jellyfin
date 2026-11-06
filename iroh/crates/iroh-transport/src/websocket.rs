//! WebSocket upgrades: host settings, the upgrade handshake with the origin, and the byte relay
//! shared by host and client. Messages are never parsed; the relay copies bytes in both directions.

use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, Method, Request, Response, StatusCode, header};
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use iroh::endpoint::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::protocol::{BODY_CHUNK_BYTES, Header, HttpRequest, error_code};
use crate::proxy::{self, Origin, ProxyError, UPSTREAM_RESPONSE_TIMEOUT};

/// Open WebSockets allowed per device session.
pub const MAX_WEBSOCKETS_PER_SESSION: u64 = 16;
/// After one side of a WebSocket closes, how long the other side has to finish.
const CLOSE_GRACE: Duration = Duration::from_secs(10);

/// What the host sends as the `Origin` header when forwarding a WebSocket upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginHeader {
    /// Replace a present `Origin` with the origin's own address, as a same-site reverse proxy would.
    #[default]
    Rewrite,
    /// Forward the client's `Origin` unchanged.
    PassThrough,
}

impl OriginHeader {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rewrite => "rewrite",
            Self::PassThrough => "pass_through",
        }
    }
}

impl std::str::FromStr for OriginHeader {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "rewrite" => Ok(Self::Rewrite),
            "pass_through" => Ok(Self::PassThrough),
            other => Err(format!(
                "unknown origin header mode '{other}'; expected rewrite or pass_through"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSocketSettings {
    pub enabled: bool,
    pub origin_header: OriginHeader,
}

impl Default for WebSocketSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            origin_header: OriginHeader::Rewrite,
        }
    }
}

/// Parses an on/off switch as used by `IROH_WEBSOCKETS`.
pub fn parse_switch(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        other => Err(format!("'{other}' is not on or off")),
    }
}

/// Whether an incoming HTTP/1.1 request asks to be upgraded to a WebSocket.
pub fn is_upgrade_request(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::GET
        && has_token(headers, header::CONNECTION, "upgrade")
        && has_token(headers, header::UPGRADE, "websocket")
}

fn has_token(headers: &HeaderMap, name: header::HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|item| item.trim().eq_ignore_ascii_case(token))
}

pub enum Upstream {
    /// The origin switched protocols. `headers` are its 101 response headers, without hop-by-hop ones.
    Switched {
        headers: Vec<Header>,
        io: TokioIo<Upgraded>,
    },
    /// The origin answered with an ordinary response, which is passed back unchanged.
    Refused(Response<Incoming>),
}

/// Sends the upgrade request on a dedicated origin connection and waits for the origin's answer.
pub async fn connect_upstream(
    origin: &Origin,
    request: &HttpRequest,
    origin_header: OriginHeader,
) -> Result<Upstream, ProxyError> {
    match timeout(
        UPSTREAM_RESPONSE_TIMEOUT,
        handshake_upstream(origin, request, origin_header),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(ProxyError {
            code: error_code::UPSTREAM_TIMEOUT,
            message: "origin did not answer the WebSocket upgrade in time".into(),
        }),
    }
}

async fn handshake_upstream(
    origin: &Origin,
    request: &HttpRequest,
    origin_header: OriginHeader,
) -> Result<Upstream, ProxyError> {
    let upgrade_request = build_upstream_request(origin, request, origin_header)?;
    let (host, port) = origin.host_port();
    let tcp = TcpStream::connect((host.as_str(), port))
        .await
        .map_err(unavailable)?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
        .await
        .map_err(unavailable)?;
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let mut response = sender
        .send_request(upgrade_request)
        .await
        .map_err(unavailable)?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Ok(Upstream::Refused(response));
    }
    let headers = proxy::response_headers(&response);
    let upgraded = hyper::upgrade::on(&mut response)
        .await
        .map_err(unavailable)?;
    Ok(Upstream::Switched {
        headers,
        io: TokioIo::new(upgraded),
    })
}

fn build_upstream_request(
    origin: &Origin,
    request: &HttpRequest,
    origin_header: OriginHeader,
) -> Result<Request<Empty<Bytes>>, ProxyError> {
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(request.path.as_str())
        .header(header::HOST, origin.authority())
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket");
    for (name, value) in &request.headers {
        if proxy::is_hop_by_hop(name) {
            continue;
        }
        if name.eq_ignore_ascii_case("origin") && origin_header == OriginHeader::Rewrite {
            builder = builder.header(header::ORIGIN, origin.web_origin());
            continue;
        }
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
        .body(Empty::new())
        .map_err(|_| ProxyError::bad_request("a header name or value is not valid"))
}

fn unavailable(error: impl std::fmt::Display) -> ProxyError {
    ProxyError {
        code: error_code::UPSTREAM_UNAVAILABLE,
        message: format!("origin is unavailable: {error}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayEnd {
    /// Both sides closed their direction cleanly.
    Closed,
    /// A side failed or did not finish within the close grace period.
    Failed,
    /// `stop` resolved with this close code before the WebSocket ended.
    Stopped(u32),
}

/// Copies bytes between an Iroh stream pair and a local connection until both directions end or
/// `stop` resolves. `on_sent` is called with the byte count of each chunk written to `send`.
pub async fn relay<T>(
    send: &mut SendStream,
    recv: &mut RecvStream,
    io: T,
    mut on_sent: impl FnMut(u64),
    stop: impl Future<Output = u32>,
) -> RelayEnd
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let (mut io_read, mut io_write) = tokio::io::split(io);
    let inbound = async {
        let copied = tokio::io::copy(recv, &mut io_write).await;
        let _ = io_write.shutdown().await;
        copied.is_ok()
    };
    let outbound = async {
        let mut buffer = vec![0u8; BODY_CHUNK_BYTES];
        loop {
            match io_read.read(&mut buffer).await {
                Ok(0) => return send.finish().is_ok(),
                Ok(read) => {
                    if send
                        .write_chunk(Bytes::copy_from_slice(&buffer[..read]))
                        .await
                        .is_err()
                    {
                        return false;
                    }
                    on_sent(read as u64);
                }
                Err(_) => return false,
            }
        }
    };
    tokio::pin!(inbound, outbound, stop);
    let (first_ok, inbound_finished) = tokio::select! {
        ok = &mut inbound => (ok, true),
        ok = &mut outbound => (ok, false),
        code = &mut stop => return RelayEnd::Stopped(code),
    };
    if !first_ok {
        return RelayEnd::Failed;
    }
    let remaining = async {
        if inbound_finished {
            outbound.await
        } else {
            inbound.await
        }
    };
    tokio::select! {
        result = timeout(CLOSE_GRACE, remaining) => match result {
            Ok(true) => RelayEnd::Closed,
            _ => RelayEnd::Failed,
        },
        code = &mut stop => RelayEnd::Stopped(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_static(value));
        }
        map
    }

    #[test]
    fn recognizes_websocket_upgrades() {
        let upgrade = headers(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "websocket"),
        ]);
        assert!(is_upgrade_request(&Method::GET, &upgrade));
        assert!(!is_upgrade_request(&Method::POST, &upgrade));
        assert!(!is_upgrade_request(
            &Method::GET,
            &headers(&[("connection", "upgrade"), ("upgrade", "h2c")])
        ));
        assert!(!is_upgrade_request(
            &Method::GET,
            &headers(&[("upgrade", "websocket")])
        ));
    }

    #[test]
    fn parses_settings() {
        assert_eq!(parse_switch(" OFF "), Ok(false));
        assert_eq!(parse_switch("on"), Ok(true));
        assert!(parse_switch("maybe").is_err());
        for mode in [OriginHeader::Rewrite, OriginHeader::PassThrough] {
            assert_eq!(mode.as_str().parse::<OriginHeader>().unwrap(), mode);
        }
        let settings: WebSocketSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, WebSocketSettings::default());
        assert!(settings.enabled);
    }

    fn upgrade_request(headers: Vec<Header>) -> HttpRequest {
        HttpRequest {
            request_id: "w1".into(),
            method: "GET".into(),
            path: "/socket?id=1".into(),
            headers,
            body: crate::protocol::BodyMode::None,
            content_length: None,
            upgrade: Some(crate::protocol::WEBSOCKET.into()),
        }
    }

    #[test]
    fn rewrites_or_passes_the_origin_header() {
        let origin = Origin::parse("http://127.0.0.1:9014").unwrap();
        let request = upgrade_request(vec![
            ("origin".into(), "http://127.0.0.1:8097".into()),
            (
                "sec-websocket-key".into(),
                "dGhlIHNhbXBsZSBub25jZQ==".into(),
            ),
            ("connection".into(), "Upgrade".into()),
        ]);
        let rewritten = build_upstream_request(&origin, &request, OriginHeader::Rewrite).unwrap();
        assert_eq!(rewritten.headers()["origin"], "http://127.0.0.1:9014");
        assert_eq!(rewritten.headers()["host"], "127.0.0.1:9014");
        assert_eq!(rewritten.headers()["connection"], "upgrade");
        assert_eq!(rewritten.headers().get_all("connection").iter().count(), 1);
        assert_eq!(rewritten.uri(), "/socket?id=1");
        let passed = build_upstream_request(&origin, &request, OriginHeader::PassThrough).unwrap();
        assert_eq!(passed.headers()["origin"], "http://127.0.0.1:8097");

        let no_origin = upgrade_request(vec![]);
        let request = build_upstream_request(&origin, &no_origin, OriginHeader::Rewrite).unwrap();
        assert!(request.headers().get("origin").is_none());
    }
}
