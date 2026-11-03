//! End-to-end tests over real Iroh connections on the local machine, with relays disabled.
//!
//! A small HTTP origin stands in for Jellyfin. Covers pairing, reconnecting, Range, concurrency,
//! request bodies, cancellation, revocation, and the transport policy gate.

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
use iroh_transport::client::{ClientError, ClientSession};
use iroh_transport::host::{AuthDecision, DeviceRegistry, HostService, serve};
use iroh_transport::net::{EndpointOptions, RelayChoice, bind_endpoint};
use iroh_transport::policy::TransportPolicy;
use iroh_transport::protocol::{ALPN, AuthOutcome, error_code};
use iroh_transport::proxy::Origin;
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
}

impl MemoryRegistry {
    fn revoke_all(&self) {
        self.devices.lock().unwrap().clear();
    }

    fn set_policy(&self, policy: TransportPolicy) {
        *self.policy.lock().unwrap() = policy;
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
}

struct Harness {
    host_addr: EndpointAddr,
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
