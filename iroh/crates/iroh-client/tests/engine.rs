#![allow(dead_code, unused_imports)]
//! End-to-end tests over real Iroh connections on the local machine, with relays disabled.
//!
//! A small HTTP origin stands in for Jellyfin. Covers pairing, reconnecting, Range, concurrency,
//! request bodies, cancellation, revocation, the transport policy gate, and WebSockets.

use iroh_client::{Client, ClientConfig, Error};
use iroh_transport::invite::InviteLink;
use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use bytes::Bytes;
use futures_util::stream;
use http::{Request, Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use iroh::{Endpoint, EndpointAddr, SecretKey};
use iroh_transport::client::{ClientError, ClientSession, ClientWebSocket, WebSocketReply};
use iroh_transport::host::{AuthDecision, DeviceRegistry, HostService, serve};
use iroh_transport::net::{EndpointOptions, RelayChoice, bind_endpoint};
use iroh_transport::policy::TransportPolicy;
use iroh_transport::protocol::{ALPN, AuthOutcome, error_code};
use iroh_transport::proxy::Origin;
use iroh_transport::websocket::{MAX_WEBSOCKETS_PER_SESSION, OriginHeader, WebSocketSettings};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const FILE_LEN: usize = 1024 * 1024 + 123;
const TOKEN: &str = "test-pairing-token";

fn file_bytes() -> Vec<u8> {
    (0..FILE_LEN).map(|i| (i % 251) as u8).collect()
}

type TestBody = UnsyncBoxBody<Bytes, Infallible>;

/// Signals when the origin's slow response body is dropped, which is how cancellation is observed.
struct DropSignal(mpsc::UnboundedSender<()>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

async fn origin_handler(
    request: Request<Incoming>,
    dropped: mpsc::UnboundedSender<()>,
) -> Result<Response<TestBody>, Infallible> {
    let full = |status: StatusCode, body: Vec<u8>| {
        Response::builder()
            .status(status)
            .body(Full::new(Bytes::from(body)).boxed_unsync())
            .unwrap()
    };
    let path = request.uri().path().to_owned();
    let response = match path.as_str() {
        "/file" => {
            let data = file_bytes();
            match request
                .headers()
                .get("range")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_range)
            {
                Some((start, end)) => {
                    let end = end.min(FILE_LEN - 1);
                    let mut response =
                        full(StatusCode::PARTIAL_CONTENT, data[start..=end].to_vec());
                    response.headers_mut().insert(
                        "content-range",
                        format!("bytes {start}-{end}/{FILE_LEN}").parse().unwrap(),
                    );
                    response
                }
                None => full(StatusCode::OK, data),
            }
        }
        "/headers" => {
            let value = format!(
                "{}|{}",
                request
                    .headers()
                    .get("authorization")
                    .unwrap()
                    .to_str()
                    .unwrap(),
                request.headers().get("cookie").unwrap().to_str().unwrap()
            );
            full(StatusCode::OK, value.into_bytes())
        }
        "/redirect" => {
            let mut response = full(StatusCode::FOUND, Vec::new());
            response
                .headers_mut()
                .insert("location", "/file".parse().unwrap());
            response
        }
        "/playlist.m3u8" => full(
            StatusCode::OK,
            b"#EXTM3U\nfile\n/file\nhttp://origin.invalid/file\n".to_vec(),
        ),
        "/echo" => {
            let body = request.into_body().collect().await.unwrap().to_bytes();
            full(StatusCode::OK, body.to_vec())
        }
        "/slow" => {
            let guard = DropSignal(dropped);
            let chunks = stream::unfold(guard, |guard| async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Some((
                    Ok::<_, Infallible>(Frame::data(Bytes::from(vec![7u8; 1024]))),
                    guard,
                ))
            });
            Response::new(StreamBody::new(chunks).boxed_unsync())
        }
        "/ws" if request.headers().contains_key("upgrade") => {
            let text = |name: &str| {
                request
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            // Like many media servers, accept the upgrade only from the server's own web origin.
            let seen_origin = text("origin");
            let own_origin = text("host").map(|host| format!("http://{host}"));
            if seen_origin.is_some() && seen_origin != own_origin {
                return Ok(full(StatusCode::FORBIDDEN, b"cross-origin".to_vec()));
            }
            let mut response = full(StatusCode::SWITCHING_PROTOCOLS, Vec::new());
            let headers = response.headers_mut();
            headers.insert("connection", "upgrade".parse().unwrap());
            headers.insert("upgrade", "websocket".parse().unwrap());
            if let Some(seen) = seen_origin {
                headers.insert("x-seen-origin", seen.parse().unwrap());
            }
            tokio::spawn(async move {
                if let Ok(upgraded) = hyper::upgrade::on(request).await {
                    let (mut read, mut write) = tokio::io::split(TokioIo::new(upgraded));
                    let _ = tokio::io::copy(&mut read, &mut write).await;
                }
            });
            response
        }
        _ => full(StatusCode::NOT_FOUND, b"not found".to_vec()),
    };
    Ok(response)
}

fn parse_range(value: &str) -> Option<(usize, usize)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

async fn start_origin() -> (SocketAddr, mpsc::UnboundedReceiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| origin_handler(req, tx.clone()));
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    (addr, rx)
}

#[derive(Default)]
struct MemoryRegistry {
    devices: Mutex<HashSet<String>>,
    policy: Mutex<TransportPolicy>,
    websocket: Mutex<WebSocketSettings>,
}

impl MemoryRegistry {
    fn revoke_all(&self) {
        self.devices.lock().unwrap().clear();
    }

    fn set_policy(&self, policy: TransportPolicy) {
        *self.policy.lock().unwrap() = policy;
    }

    fn set_websocket(&self, settings: WebSocketSettings) {
        *self.websocket.lock().unwrap() = settings;
    }
}

impl DeviceRegistry for MemoryRegistry {
    fn authorize(&self, endpoint_id: &str, _name: &str, token: Option<&str>) -> AuthDecision {
        let mut devices = self.devices.lock().unwrap();
        if devices.contains(endpoint_id) {
            AuthDecision::Authorized
        } else if token == Some(TOKEN) {
            devices.insert(endpoint_id.to_owned());
            AuthDecision::Paired
        } else {
            AuthDecision::Denied {
                code: error_code::PAIRING_TOKEN_INVALID,
                reason: "bad token".into(),
            }
        }
    }

    fn is_authorized(&self, endpoint_id: &str) -> bool {
        self.devices.lock().unwrap().contains(endpoint_id)
    }

    fn policy(&self) -> TransportPolicy {
        *self.policy.lock().unwrap()
    }

    fn websocket(&self) -> WebSocketSettings {
        *self.websocket.lock().unwrap()
    }
}

struct Harness {
    host_addr: EndpointAddr,
    origin_addr: SocketAddr,
    registry: Arc<MemoryRegistry>,
    origin_dropped: mpsc::UnboundedReceiver<()>,
    shutdown: CancellationToken,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn local_options() -> EndpointOptions {
    EndpointOptions {
        relay: Some(RelayChoice::Disabled),
        relay_transport_only: false,
    }
}

async fn start_host() -> Harness {
    let (origin_addr, origin_dropped) = start_origin().await;
    let origin = Origin::parse(&format!("http://{origin_addr}")).unwrap();
    let registry = Arc::new(MemoryRegistry::default());
    let endpoint = bind_endpoint(SecretKey::generate(), vec![ALPN.to_vec()], local_options())
        .await
        .unwrap();
    let host_addr = wait_for_direct_addr(&endpoint).await;
    let shutdown = CancellationToken::new();
    let service = HostService::new(origin, registry.clone());
    tokio::spawn(serve(endpoint, service, shutdown.clone()));
    Harness {
        host_addr,
        origin_addr,
        registry,
        origin_dropped,
        shutdown,
    }
}

async fn wait_for_direct_addr(endpoint: &Endpoint) -> EndpointAddr {
    for _ in 0..100 {
        let addr = endpoint.addr();
        if addr.addrs.iter().any(|transport| transport.is_ip()) {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("endpoint never reported a direct address");
}

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("iroh-sdk-{}", SecretKey::generate().public()));
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
async fn engine(dir: &TempDir) -> Client {
    Client::new(ClientConfig {
        data_dir: dir.0.clone(),
        name: "SDK test".into(),
        relay: RelayChoice::Disabled,
    })
    .await
    .unwrap()
}
async fn request(url: &str, suffix: &str, headers: &str) -> Vec<u8> {
    let url = url.strip_prefix("http://").unwrap();
    let (addr, prefix) = url.split_once('/').unwrap();
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket.write_all(format!("GET /{prefix}{suffix} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{headers}\r\n").as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), socket.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    bytes
}
fn body(response: &[u8]) -> &[u8] {
    let start = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    &response[start..]
}
#[tokio::test(flavor = "multi_thread")]
async fn protected_proxy_lifecycle_range_concurrency_and_identity() {
    let harness = start_host().await;
    let dir = TempDir::new();
    let client = Arc::new(engine(&dir).await);
    assert!(matches!(
        client.pair("not-an-invite").await,
        Err(Error::InvalidInvitation)
    ));
    assert!(matches!(
        client
            .start_proxy("127.0.0.1:0".parse().unwrap(), true)
            .await,
        Err(Error::NotPaired)
    ));
    assert!(
        client
            .start_proxy("0.0.0.0:0".parse().unwrap(), true)
            .await
            .is_err()
    );
    assert!(
        Client::new(ClientConfig {
            data_dir: dir.0.clone(),
            name: "duplicate".into(),
            relay: RelayChoice::Disabled
        })
        .await
        .is_err()
    );
    let invite = InviteLink::new(harness.host_addr.clone(), TOKEN.into()).encode();
    client.pair(&invite).await.unwrap();
    let key = std::fs::read(dir.0.join("client.key")).unwrap();
    let (a, b) = tokio::join!(
        client.start_proxy("127.0.0.1:0".parse().unwrap(), true),
        client.start_proxy("127.0.0.1:0".parse().unwrap(), true)
    );
    let url = a.unwrap().proxy_url;
    assert_eq!(url, b.unwrap().proxy_url);
    let raw = format!(
        "http://{}/",
        url.strip_prefix("http://")
            .unwrap()
            .split('/')
            .next()
            .unwrap()
    );
    assert!(request(&raw, "file", "").await.starts_with(b"HTTP/1.1 403"));
    let (a, b) = tokio::join!(
        request(&url, "file", "Range: bytes=10-19\r\n"),
        request(&url, "file", "Range: bytes=100-109\r\n")
    );
    assert!(a.starts_with(b"HTTP/1.1 206"));
    assert_eq!(body(&a), &file_bytes()[10..20]);
    assert_eq!(body(&b), &file_bytes()[100..110]);
    let status = client.status().await;
    assert_eq!(status.authorization, "authorized");
    assert_eq!(status.path, "direct");
    client.notify_network_change().await.unwrap();
    client.stop_proxy().await.unwrap();
    let addr = url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
    let new_url = client
        .start_proxy("127.0.0.1:0".parse().unwrap(), true)
        .await
        .unwrap()
        .proxy_url;
    assert_ne!(url, new_url);
    assert!(
        request(&new_url, "file", "Range: bytes=10-19\r\n")
            .await
            .starts_with(b"HTTP/1.1 206")
    );
    client.forget_pairing().await.unwrap();
    assert_eq!(key, std::fs::read(dir.0.join("client.key")).unwrap());
    assert!(!dir.0.join("host.json").exists());
    assert_eq!(harness.registry.devices.lock().unwrap().len(), 1); // not server revocation
    client.shutdown().await;
    assert!(matches!(
        client
            .start_proxy("127.0.0.1:0".parse().unwrap(), true)
            .await,
        Err(Error::Closed)
    ));
    let restored = engine(&dir).await;
    assert_eq!(key, std::fs::read(dir.0.join("client.key")).unwrap());
    restored.shutdown().await;
}
#[tokio::test(flavor = "multi_thread")]
async fn proxy_websocket_shutdown_and_revocation() {
    let harness = start_host().await;
    let dir = TempDir::new();
    let client = engine(&dir).await;
    client
        .pair(&InviteLink::new(harness.host_addr.clone(), TOKEN.into()).encode())
        .await
        .unwrap();
    let url = client
        .start_proxy("127.0.0.1:0".parse().unwrap(), true)
        .await
        .unwrap()
        .proxy_url;
    let (addr, path) = url
        .strip_prefix("http://")
        .unwrap()
        .split_once('/')
        .unwrap();
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket.write_all(format!("GET /{path}ws HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").as_bytes()).await.unwrap();
    let mut head = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !head.ends_with(b"\r\n\r\n") {
            head.push(socket.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert!(head.starts_with(b"HTTP/1.1 101"));
    let frame = [0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2];
    socket.write_all(&frame).await.unwrap();
    let mut echoed = [0; 8];
    tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut echoed))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame, echoed);
    client.stop_proxy().await.unwrap();
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut rest))
        .await
        .expect("upgrade must stop");
    harness.registry.revoke_all();
    assert!(matches!(
        client
            .start_proxy("127.0.0.1:0".parse().unwrap(), true)
            .await,
        Err(Error::Denied(_))
    ));
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn headers_manifests_redirects_and_stream_cancellation() {
    let mut harness = start_host().await;
    let dir = TempDir::new();
    let client = engine(&dir).await;
    client
        .pair(&InviteLink::new(harness.host_addr.clone(), TOKEN.into()).encode())
        .await
        .unwrap();
    let url = client
        .start_proxy("127.0.0.1:0".parse().unwrap(), true)
        .await
        .unwrap()
        .proxy_url;
    let response = request(
        &url,
        "headers",
        "Authorization: Bearer app-token\r\nCookie: session=abc\r\n",
    )
    .await;
    assert_eq!(body(&response), b"Bearer app-token|session=abc");
    let playlist = request(&url, "playlist.m3u8", "").await;
    assert_eq!(
        body(&playlist),
        b"#EXTM3U\nfile\n/file\nhttp://origin.invalid/file\n"
    );
    let redirect = request(&url, "redirect", "").await;
    assert!(
        String::from_utf8_lossy(&redirect)
            .to_lowercase()
            .contains("location: /file")
    );
    // Root-relative redirects/manifests lose the capability. The SDK deliberately does
    // not rewrite media payloads or claim these URLs work transparently.
    let root = format!(
        "http://{}/",
        url.strip_prefix("http://")
            .unwrap()
            .split('/')
            .next()
            .unwrap()
    );
    assert!(
        request(&root, "file", "")
            .await
            .starts_with(b"HTTP/1.1 403")
    );
    let (addr, path) = url
        .strip_prefix("http://")
        .unwrap()
        .split_once('/')
        .unwrap();
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(format!("GET /{path}slow HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut received = [0; 4096];
    assert!(
        tokio::time::timeout(Duration::from_secs(10), socket.read(&mut received))
            .await
            .unwrap()
            .unwrap()
            > 0
    );
    // Closing the downstream TCP stream propagates cancellation to the upstream body.
    drop(socket);
    tokio::time::timeout(Duration::from_secs(5), harness.origin_dropped.recv())
        .await
        .expect("upstream stream must be dropped")
        .unwrap();
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnects_after_a_closed_session_without_replaying_requests() {
    let harness = start_host().await;
    let dir = TempDir::new();
    let client = engine(&dir).await;
    client
        .pair(&InviteLink::new(harness.host_addr.clone(), TOKEN.into()).encode())
        .await
        .unwrap();
    let url = client
        .start_proxy("127.0.0.1:0".parse().unwrap(), true)
        .await
        .unwrap()
        .proxy_url;
    assert!(
        request(&url, "file", "Range: bytes=0-9\r\n")
            .await
            .starts_with(b"HTTP/1.1 206")
    );
    // Force the host to terminate the current session, then restore authorization.
    // This tests the reconnect path without pretending to simulate Wi-Fi migration.
    let id = harness
        .registry
        .devices
        .lock()
        .unwrap()
        .iter()
        .next()
        .unwrap()
        .clone();
    harness.registry.revoke_all();
    tokio::time::timeout(Duration::from_secs(10), async {
        while client.status().await.state != "disconnected" {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    harness.registry.devices.lock().unwrap().insert(id);
    let response = request(&url, "file", "Range: bytes=10-19\r\n").await;
    assert!(response.starts_with(b"HTTP/1.1 206"));
    assert_eq!(body(&response), &file_bytes()[10..20]);
    client.shutdown().await;
}
