//! End-to-end tests over real Iroh connections on the local machine, with relays disabled.
//!
//! A small HTTP origin stands in for Jellyfin. Covers pairing, reconnecting, Range, concurrency,
//! request bodies, cancellation, revocation, the transport policy gate, and WebSockets.

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

async fn new_client() -> Endpoint {
    bind_endpoint(SecretKey::generate(), vec![], local_options())
        .await
        .unwrap()
}

async fn connect(
    harness: &Harness,
    client: &Endpoint,
    token: Option<&str>,
) -> Result<ClientSession, ClientError> {
    ClientSession::connect(client, harness.host_addr.clone(), "test device", token).await
}

async fn read_all(
    session: &ClientSession,
    path: &str,
    range: Option<&str>,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let headers = range
        .map(|r| vec![("range".to_owned(), r.to_owned())])
        .unwrap_or_default();
    let mut response = session.request("GET", path, headers, None).await.unwrap();
    let mut body = Vec::new();
    AsyncReadExt::read_to_end(&mut response.body, &mut body)
        .await
        .unwrap();
    (response.head.status, response.head.headers, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn pairs_reconnects_and_serves_ranges() {
    let harness = start_host().await;
    let client = new_client().await;

    let session = connect(&harness, &client, Some(TOKEN)).await.unwrap();
    assert_eq!(session.outcome(), AuthOutcome::Paired);
    assert_eq!(session.transport_policy(), TransportPolicy::Auto);
    let (status, headers, body) = read_all(&session, "/file", Some("bytes=100-199")).await;
    assert_eq!(status, 206);
    assert_eq!(body, file_bytes()[100..=199]);
    assert!(
        headers.iter().any(|(name, value)| name == "content-range"
            && value == &format!("bytes 100-199/{FILE_LEN}"))
    );
    assert!(session.ping().await.unwrap() < Duration::from_secs(5));
    session.close().await;

    let session = connect(&harness, &client, None).await.unwrap();
    assert_eq!(session.outcome(), AuthOutcome::Authorized);
    let (status, _, body) = read_all(&session, "/file", None).await;
    assert_eq!(status, 200);
    assert_eq!(body, file_bytes());
}

#[tokio::test(flavor = "multi_thread")]
async fn denies_unknown_devices() {
    let harness = start_host().await;
    let client = new_client().await;
    match connect(&harness, &client, Some("wrong")).await {
        Err(ClientError::Denied(code)) => assert_eq!(code, error_code::PAIRING_TOKEN_INVALID),
        other => panic!("expected denial, got {:?}", other.err()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn serves_concurrent_requests() {
    let harness = start_host().await;
    let client = new_client().await;
    let session = Arc::new(connect(&harness, &client, Some(TOKEN)).await.unwrap());
    let mut tasks = Vec::new();
    for i in 0..8usize {
        let session = session.clone();
        tasks.push(tokio::spawn(async move {
            let start = i * 1000;
            let range = format!("bytes={start}-{}", start + 65_535);
            let (status, _, body) = read_all(&session, "/file", Some(&range)).await;
            assert_eq!(status, 206);
            assert_eq!(body, file_bytes()[start..=start + 65_535]);
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn forwards_request_bodies() {
    let harness = start_host().await;
    let client = new_client().await;
    let session = connect(&harness, &client, Some(TOKEN)).await.unwrap();
    let payload = Bytes::from_static(br#"{"Username":"a","Pw":"b"}"#);
    let mut response = session
        .request("POST", "/echo", vec![], Some(payload.clone()))
        .await
        .unwrap();
    let mut body = Vec::new();
    AsyncReadExt::read_to_end(&mut response.body, &mut body)
        .await
        .unwrap();
    assert_eq!(response.head.status, 200);
    assert_eq!(body, payload);
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_a_response_cancels_the_upstream_request() {
    let mut harness = start_host().await;
    let client = new_client().await;
    let session = connect(&harness, &client, Some(TOKEN)).await.unwrap();
    let mut response = session.request("GET", "/slow", vec![], None).await.unwrap();
    let mut chunk = [0u8; 2048];
    response.body.read_exact(&mut chunk).await.unwrap();
    drop(response);
    tokio::time::timeout(Duration::from_secs(10), harness.origin_dropped.recv())
        .await
        .expect("origin body was not dropped after the client cancelled")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_device_ends_its_session() {
    let harness = start_host().await;
    let client = new_client().await;
    let session = connect(&harness, &client, Some(TOKEN)).await.unwrap();
    harness.registry.revoke_all();
    tokio::time::timeout(Duration::from_secs(10), session.connection().closed())
        .await
        .expect("session stayed open after revocation");
    assert!(connect(&harness, &client, None).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_gate_follows_the_selected_path() {
    let harness = start_host().await;
    let client = new_client().await;
    let session = connect(&harness, &client, Some(TOKEN)).await.unwrap();

    // With relays disabled, the only possible path is direct.
    harness.registry.set_policy(TransportPolicy::P2pOnly);
    let (status, _, _) = read_all(&session, "/file", Some("bytes=0-9")).await;
    assert_eq!(status, 206);

    harness.registry.set_policy(TransportPolicy::RelayOnly);
    match session.request("GET", "/file", vec![], None).await {
        Err(ClientError::Request { code, .. }) => {
            assert_eq!(code, error_code::TRANSPORT_POLICY_DENIED)
        }
        Ok(response) => panic!(
            "relay_only served a direct request: {}",
            response.head.status
        ),
        Err(other) => panic!("unexpected error: {other}"),
    }
}

const FOREIGN_ORIGIN: &str = "http://127.0.0.1:8097";

async fn open_websocket(session: &ClientSession) -> Result<ClientWebSocket, String> {
    let headers = vec![
        ("origin".to_owned(), FOREIGN_ORIGIN.to_owned()),
        (
            "sec-websocket-key".to_owned(),
            "dGhlIHNhbXBsZSBub25jZQ==".to_owned(),
        ),
    ];
    match session.open_websocket("/ws", headers).await {
        Ok(WebSocketReply::Open(socket)) => Ok(socket),
        Ok(WebSocketReply::Refused(response)) => Err(format!("refused {}", response.head.status)),
        Err(error) => Err(error.to_string()),
    }
}

async fn echo(socket: &mut ClientWebSocket, message: &'static [u8]) {
    socket
        .send
        .write_chunk(Bytes::from_static(message))
        .await
        .unwrap();
    let mut reply = vec![0u8; message.len()];
    tokio::time::timeout(Duration::from_secs(10), socket.recv.read_exact(&mut reply))
        .await
        .expect("no echo from the origin")
        .unwrap();
    assert_eq!(reply, message);
}

async fn expect_closed(socket: &mut ClientWebSocket) {
    let mut rest = Vec::new();
    // Either a clean end or a reset counts as closed.
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        AsyncReadExt::read_to_end(&mut socket.recv, &mut rest),
    )
    .await
    .expect("WebSocket stayed open");
}

#[tokio::test(flavor = "multi_thread")]
async fn websockets_relay_bytes_and_rewrite_the_origin_header() {
    let harness = start_host().await;
    let client = new_client().await;
    let session = connect(&harness, &client, Some(TOKEN)).await.unwrap();
    assert!(session.supports_websocket());

    let mut socket = open_websocket(&session).await.unwrap();
    assert_eq!(socket.head.status, 101);
    let own_origin = format!("http://{}", harness.origin_addr);
    assert!(
        socket
            .head
            .headers
            .iter()
            .any(|(name, value)| name == "x-seen-origin" && value == &own_origin),
        "{:?}",
        socket.head.headers
    );
    echo(&mut socket, b"hello").await;
    // Plain HTTP keeps working while a WebSocket is open.
    let (status, _, body) = read_all(&session, "/file", Some("bytes=0-9")).await;
    assert_eq!(status, 206);
    assert_eq!(body, file_bytes()[..10]);
    echo(&mut socket, b"again").await;

    socket.send.finish().unwrap();
    expect_closed(&mut socket).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pass_through_forwards_the_original_origin_header() {
    let harness = start_host().await;
    harness.registry.set_websocket(WebSocketSettings {
        enabled: true,
        origin_header: OriginHeader::PassThrough,
    });
    let client = new_client().await;
    let session = connect(&harness, &client, Some(TOKEN)).await.unwrap();
    assert_eq!(
        open_websocket(&session).await.err().as_deref(),
        Some("refused 403")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_websockets_off_closes_them_and_stops_new_ones() {
    let harness = start_host().await;
    let client = new_client().await;
    let session = connect(&harness, &client, Some(TOKEN)).await.unwrap();
    let mut socket = open_websocket(&session).await.unwrap();
    echo(&mut socket, b"hello").await;

    harness.registry.set_websocket(WebSocketSettings {
        enabled: false,
        origin_header: OriginHeader::Rewrite,
    });
    expect_closed(&mut socket).await;
    assert!(open_websocket(&session).await.is_err());
    let (status, _, _) = read_all(&session, "/file", Some("bytes=0-9")).await;
    assert_eq!(status, 206);
    session.close().await;

    let session = connect(&harness, &client, None).await.unwrap();
    assert!(!session.supports_websocket());
    match session.open_websocket("/ws", vec![]).await {
        Err(ClientError::Request { code, .. }) => assert_eq!(code, error_code::UNSUPPORTED),
        Ok(_) => panic!("a WebSocket opened while they are switched off"),
        Err(other) => panic!("unexpected error: {other}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn websockets_per_session_are_limited() {
    let harness = start_host().await;
    let client = new_client().await;
    let session = connect(&harness, &client, Some(TOKEN)).await.unwrap();
    let mut open = Vec::new();
    for _ in 0..MAX_WEBSOCKETS_PER_SESSION {
        open.push(open_websocket(&session).await.unwrap());
    }
    match session.open_websocket("/ws", vec![]).await {
        Err(ClientError::Request { code, .. }) => {
            assert_eq!(code, error_code::TOO_MANY_WEBSOCKETS)
        }
        Ok(_) => panic!("opened more WebSockets than the limit"),
        Err(other) => panic!("unexpected error: {other}"),
    }

    let mut first = open.remove(0);
    first.send.finish().unwrap();
    expect_closed(&mut first).await;
    let mut socket = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(socket) = open_websocket(&session).await {
                return socket;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("a closed WebSocket did not free its slot");
    echo(&mut socket, b"slot").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_device_closes_its_websockets() {
    let harness = start_host().await;
    let client = new_client().await;
    let session = connect(&harness, &client, Some(TOKEN)).await.unwrap();
    let mut socket = open_websocket(&session).await.unwrap();
    echo(&mut socket, b"hello").await;
    harness.registry.revoke_all();
    expect_closed(&mut socket).await;
    tokio::time::timeout(Duration::from_secs(10), session.connection().closed())
        .await
        .expect("session stayed open after revocation");
}
