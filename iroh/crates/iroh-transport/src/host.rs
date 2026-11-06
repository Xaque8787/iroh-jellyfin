//! Host side of transport v1: authenticates devices and forwards their HTTP requests to the origin.
//!
//! One task per connection runs the control stream and accepts request streams; one task per
//! request stream forwards the request and streams the response body back.

use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;
use http_body_util::{BodyExt, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use iroh::Endpoint;
use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
use tokio::time::{Instant, timeout};
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, info, warn};

use crate::framing::{FrameError, read_frame, write_frame};
use crate::net::{PathReport, current_path};
use crate::policy::{PathKind, TransportPolicy};
use crate::protocol::{
    ALPN, AuthOutcome, AuthResult, BODY_CHUNK_BYTES, BodyMode, HttpRequest, HttpResponse,
    MAX_CONTROL_FRAME_BYTES, MAX_HTTP_FRAME_BYTES, MAX_REQUEST_BODY_BYTES, Message,
    PROTOCOL_VERSION, StreamError, WEBSOCKET, close_code, error_code, validate_http_request,
};
use crate::proxy::{self, Origin, ProxyBody, UpstreamClient};
use crate::sessions::{SessionGuard, SessionStats, SessionTable};
use crate::websocket::{self, MAX_WEBSOCKETS_PER_SESSION, RelayEnd, Upstream, WebSocketSettings};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// How often an open session re-checks that its device is still authorized.
pub const AUTHORIZATION_RECHECK: Duration = Duration::from_secs(2);
/// Under `p2p_only`, how long a request waits for Iroh to select a direct path before it is refused.
pub const DIRECT_PATH_WAIT: Duration = Duration::from_secs(5);
const DENIAL_LINGER: Duration = Duration::from_secs(3);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthDecision {
    Authorized,
    Paired,
    /// `code` is one of the protocol error codes and is sent to the client.
    Denied {
        code: &'static str,
        reason: String,
    },
}

/// The host's record of paired devices. Implemented by the embedding application.
///
/// `authorize` may block on disk I/O; it is called from a blocking thread.
pub trait DeviceRegistry: Send + Sync + 'static {
    fn authorize(
        &self,
        endpoint_id: &str,
        device_name: &str,
        pairing_token: Option<&str>,
    ) -> AuthDecision;
    fn is_authorized(&self, endpoint_id: &str) -> bool;
    fn policy(&self) -> TransportPolicy;
    /// Read on every WebSocket upgrade and while WebSockets are open, so changes apply live.
    fn websocket(&self) -> WebSocketSettings {
        WebSocketSettings::default()
    }
}

pub struct HostService {
    origin: Origin,
    client: UpstreamClient,
    registry: Arc<dyn DeviceRegistry>,
    sessions: Arc<SessionTable>,
}

impl HostService {
    pub fn new(origin: Origin, registry: Arc<dyn DeviceRegistry>) -> Arc<Self> {
        Arc::new(Self {
            origin,
            client: proxy::new_upstream_client(),
            registry,
            sessions: Arc::default(),
        })
    }

    /// Sessions currently admitted, for status reporting.
    pub fn sessions(&self) -> Arc<SessionTable> {
        self.sessions.clone()
    }
}

/// Accepts connections until `shutdown` is cancelled, then closes sessions and the endpoint.
pub async fn serve(endpoint: Endpoint, service: Arc<HostService>, shutdown: CancellationToken) {
    let tracker = TaskTracker::new();
    loop {
        let incoming = tokio::select! {
            _ = shutdown.cancelled() => break,
            incoming = endpoint.accept() => incoming,
        };
        let Some(incoming) = incoming else {
            break;
        };
        let service = service.clone();
        let shutdown = shutdown.clone();
        tracker.spawn(async move {
            match incoming.await {
                Ok(connection) => run_connection(service, connection, shutdown).await,
                Err(error) => debug!(?error, "incoming connection failed"),
            }
        });
    }
    tracker.close();
    if timeout(SHUTDOWN_GRACE, tracker.wait()).await.is_err() {
        warn!("sessions did not end within the shutdown grace period");
    }
    endpoint.close().await;
}

async fn run_connection(
    service: Arc<HostService>,
    connection: Connection,
    shutdown: CancellationToken,
) {
    let remote = connection.remote_id().to_string();
    if connection.alpn() != ALPN {
        close(
            &connection,
            close_code::PROTOCOL_VIOLATION,
            "unsupported alpn",
        );
        return;
    }
    match handshake(&service, &connection, &remote).await {
        Ok(Some((send, recv, name))) => {
            let guard = Arc::new(service.sessions.register(&remote, &name, &connection));
            serve_session(service, connection, remote, guard, send, recv, shutdown).await;
        }
        Ok(None) => {}
        Err(problem) => {
            debug!(device = %remote, problem, "handshake failed");
            close(
                &connection,
                close_code::PROTOCOL_VIOLATION,
                "handshake failed",
            );
        }
    }
}

/// Runs HELLO / AUTH_REQUEST / AUTH_RESULT. Returns the control stream and the device's name when
/// the device is admitted.
async fn handshake(
    service: &HostService,
    connection: &Connection,
    remote: &str,
) -> Result<Option<(SendStream, RecvStream, String)>, String> {
    let (mut send, mut recv) = timeout(HANDSHAKE_TIMEOUT, connection.accept_bi())
        .await
        .map_err(|_| "no control stream opened".to_owned())?
        .map_err(|error| format!("connection ended: {error}"))?;
    let hello = match read_control(&mut recv).await? {
        Message::Hello(hello) => hello,
        _ => return Err("expected HELLO".into()),
    };
    let policy = service.registry.policy();
    if hello.protocol != PROTOCOL_VERSION {
        deny(
            connection,
            &mut send,
            policy,
            error_code::PROTOCOL_VERSION_UNSUPPORTED,
            close_code::PROTOCOL_VIOLATION,
        )
        .await;
        return Ok(None);
    }
    let auth = match read_control(&mut recv).await? {
        Message::AuthRequest(auth) => auth,
        _ => return Err("expected AUTH_REQUEST".into()),
    };

    let registry = service.registry.clone();
    let endpoint_id = remote.to_owned();
    let device_name = hello.name.clone();
    let decision = tokio::task::spawn_blocking(move || {
        registry.authorize(&endpoint_id, &device_name, auth.pairing_token.as_deref())
    })
    .await
    .map_err(|_| "authorization task failed".to_owned())?;

    let outcome = match decision {
        AuthDecision::Authorized => AuthOutcome::Authorized,
        AuthDecision::Paired => AuthOutcome::Paired,
        AuthDecision::Denied { code, reason } => {
            info!(device = %remote, code, reason, "device denied");
            deny(
                connection,
                &mut send,
                policy,
                code,
                close_code::NOT_AUTHORIZED,
            )
            .await;
            return Ok(None);
        }
    };
    let capabilities = if service.registry.websocket().enabled {
        vec![WEBSOCKET.to_owned()]
    } else {
        Vec::new()
    };
    let result = Message::AuthResult(AuthResult {
        outcome,
        reason: None,
        transport_policy: policy,
        capabilities,
    });
    write_frame(&mut send, &result, MAX_CONTROL_FRAME_BYTES)
        .await
        .map_err(|error| error.to_string())?;
    info!(device = %remote, name = %hello.name, ?outcome, "device connected");
    Ok(Some((send, recv, hello.name)))
}

async fn read_control(recv: &mut RecvStream) -> Result<Message, String> {
    match timeout(HANDSHAKE_TIMEOUT, read_frame(recv, MAX_CONTROL_FRAME_BYTES)).await {
        Ok(Ok(message)) => Ok(message),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("handshake timed out".into()),
    }
}

/// Sends a denied AUTH_RESULT, gives the client a moment to read it, then closes the connection.
async fn deny(
    connection: &Connection,
    send: &mut SendStream,
    policy: TransportPolicy,
    code: &'static str,
    close_with: u32,
) {
    let result = Message::AuthResult(AuthResult {
        outcome: AuthOutcome::Denied,
        reason: Some(code.to_owned()),
        transport_policy: policy,
        capabilities: Vec::new(),
    });
    if write_frame(send, &result, MAX_CONTROL_FRAME_BYTES)
        .await
        .is_ok()
    {
        let _ = send.finish();
        let _ = timeout(DENIAL_LINGER, connection.closed()).await;
    }
    close(connection, close_with, code);
}

async fn serve_session(
    service: Arc<HostService>,
    connection: Connection,
    remote: String,
    guard: Arc<SessionGuard>,
    control_send: SendStream,
    control_recv: RecvStream,
    shutdown: CancellationToken,
) {
    let session = shutdown.child_token();
    let requests = TaskTracker::new();
    let mut control = tokio::spawn(control_loop(control_send, control_recv));
    let mut recheck = tokio::time::interval(AUTHORIZATION_RECHECK);
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_path = PathKind::Unknown;

    let (code, reason) = loop {
        tokio::select! {
            _ = shutdown.cancelled() => break (close_code::SHUTTING_DOWN, error_code::SHUTTING_DOWN),
            _ = recheck.tick() => {
                if !service.registry.is_authorized(&remote) {
                    break (close_code::REVOKED, error_code::REVOKED);
                }
                let path = current_path(&connection);
                if path.kind != last_path {
                    info!(device = %remote, path = ?path.kind, rtt_ms = ?path.rtt_ms, remote_addr = ?path.remote_addr, open_paths = path.open_paths, "connection path");
                    last_path = path.kind;
                }
            }
            result = &mut control => {
                break match result {
                    Ok(Ok(())) => (close_code::NORMAL, "session ended"),
                    Ok(Err(problem)) => {
                        debug!(device = %remote, problem, "control stream ended");
                        (close_code::PROTOCOL_VIOLATION, error_code::PROTOCOL_VIOLATION)
                    }
                    Err(_) => (close_code::PROTOCOL_VIOLATION, error_code::PROTOCOL_VIOLATION),
                };
            }
            stream = connection.accept_bi() => match stream {
                Ok((send, recv)) => {
                    requests.spawn(handle_stream(service.clone(), connection.clone(), guard.clone(), send, recv, session.clone()));
                }
                Err(error) => {
                    debug!(device = %remote, %error, "connection ended");
                    break (close_code::NORMAL, "connection ended");
                }
            },
        }
    };
    session.cancel();
    control.abort();
    close(&connection, code, reason);
    requests.close();
    requests.wait().await;
    info!(device = %remote, reason, "session closed");
}

/// Answers PINGs until the client finishes the control stream (Ok) or misbehaves (Err).
async fn control_loop(mut send: SendStream, mut recv: RecvStream) -> Result<(), String> {
    loop {
        match read_frame(&mut recv, MAX_CONTROL_FRAME_BYTES).await {
            Ok(Message::Ping(ping)) => {
                write_frame(&mut send, &Message::Pong(ping), MAX_CONTROL_FRAME_BYTES)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            Ok(_) => return Err("unexpected message on control stream".into()),
            Err(FrameError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

enum BodyOutcome {
    Complete(u64),
    ClientGone(u64),
    UpstreamFailed(u64),
}

async fn handle_stream(
    service: Arc<HostService>,
    connection: Connection,
    guard: Arc<SessionGuard>,
    mut send: SendStream,
    mut recv: RecvStream,
    session: CancellationToken,
) {
    let stats = guard.stats();
    let active = stats.begin_request();
    let started = Instant::now();
    let request = match timeout(
        REQUEST_HEADER_TIMEOUT,
        read_frame(&mut recv, MAX_HTTP_FRAME_BYTES),
    )
    .await
    {
        Ok(Ok(Message::HttpRequest(request))) => request,
        _ => {
            let _ = send.reset(VarInt::from_u32(close_code::PROTOCOL_VIOLATION));
            return;
        }
    };
    let id = request.request_id.clone();
    let log_path = request
        .path
        .split('?')
        .next()
        .unwrap_or_default()
        .to_owned();
    if let Err(problem) = validate_http_request(&request) {
        reply_error(&mut send, &id, error_code::BAD_REQUEST, problem).await;
        return;
    }

    let policy = service.registry.policy();
    let path = match admitted_path(&connection, policy, &session).await {
        Ok(path) => path,
        Err(path) => {
            let message = format!(
                "transport policy {} does not allow the current {:?} path",
                policy.as_str(),
                path.kind
            );
            info!(request = %id, path = %log_path, "{message}");
            reply_error(
                &mut send,
                &id,
                error_code::TRANSPORT_POLICY_DENIED,
                &message,
            )
            .await;
            return;
        }
    };

    if request.upgrade.is_some() {
        drop(active);
        handle_upgrade(
            &service, &connection, stats, send, recv, request, &log_path, &session,
        )
        .await;
        return;
    }

    let body = match request.body {
        BodyMode::None => proxy::empty_body(),
        BodyMode::Stream => request_body(recv),
    };
    let cancel = session.child_token();
    let response = tokio::select! {
        result = proxy::forward(&service.client, &service.origin, &request, body, &cancel) => result,
        _ = send.stopped() => {
            debug!(request = %id, "client cancelled before the origin responded");
            return;
        }
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            info!(request = %id, path = %log_path, code = error.code, "request failed");
            reply_error(&mut send, &id, error.code, &error.message).await;
            return;
        }
    };

    let status = response.status().as_u16();
    let head = Message::HttpResponse(HttpResponse {
        request_id: id.clone(),
        status,
        headers: proxy::response_headers(&response),
    });
    if let Err(error) = write_frame(&mut send, &head, MAX_HTTP_FRAME_BYTES).await {
        if matches!(error, FrameError::TooLarge { .. }) {
            reply_error(
                &mut send,
                &id,
                error_code::UPSTREAM_UNAVAILABLE,
                "origin response headers are too large",
            )
            .await;
        }
        return;
    }

    let outcome = tokio::select! {
        _ = cancel.cancelled() => None,
        outcome = pump_body(&mut send, response.into_body(), stats) => Some(outcome),
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match outcome {
        Some(BodyOutcome::Complete(bytes)) => {
            let _ = send.finish();
            debug!(request = %id, method = %request.method, path = %log_path, status, bytes, elapsed_ms, via = ?path.kind, "request complete");
        }
        Some(BodyOutcome::ClientGone(bytes)) => {
            debug!(request = %id, path = %log_path, status, bytes, elapsed_ms, "client cancelled the response");
        }
        Some(BodyOutcome::UpstreamFailed(bytes)) => {
            let _ = send.reset(VarInt::from_u32(close_code::UPSTREAM_FAILED));
            info!(request = %id, path = %log_path, status, bytes, "origin failed mid-response");
        }
        None => {
            let _ = send.reset(VarInt::from_u32(close_code::SHUTTING_DOWN));
        }
    }
}

/// Opens a WebSocket to the origin and relays it until either side closes, the session ends, or the
/// host's settings or transport policy no longer allow it.
#[allow(clippy::too_many_arguments)]
async fn handle_upgrade(
    service: &HostService,
    connection: &Connection,
    stats: &SessionStats,
    mut send: SendStream,
    mut recv: RecvStream,
    request: HttpRequest,
    log_path: &str,
    session: &CancellationToken,
) {
    let id = request.request_id.clone();
    let settings = service.registry.websocket();
    if !settings.enabled {
        reply_error(
            &mut send,
            &id,
            error_code::UNSUPPORTED,
            "WebSockets are turned off on this host",
        )
        .await;
        return;
    }
    let Some(_open) = stats.begin_websocket(MAX_WEBSOCKETS_PER_SESSION) else {
        info!(request = %id, path = %log_path, "WebSocket refused: too many open");
        reply_error(
            &mut send,
            &id,
            error_code::TOO_MANY_WEBSOCKETS,
            "this device already has the maximum number of open WebSockets",
        )
        .await;
        return;
    };

    let upstream = tokio::select! {
        result = websocket::connect_upstream(&service.origin, &request, settings.origin_header) => result,
        _ = send.stopped() => return,
        _ = session.cancelled() => return,
    };
    let (headers, io) = match upstream {
        Ok(Upstream::Switched { headers, io }) => (headers, io),
        Ok(Upstream::Refused(response)) => {
            let status = response.status().as_u16();
            info!(request = %id, path = %log_path, status, "origin refused the WebSocket");
            send_refusal(&mut send, &id, response, stats, session).await;
            return;
        }
        Err(error) => {
            info!(request = %id, path = %log_path, code = error.code, "WebSocket failed");
            reply_error(&mut send, &id, error.code, &error.message).await;
            return;
        }
    };
    let head = Message::HttpResponse(HttpResponse {
        request_id: id.clone(),
        status: 101,
        headers,
    });
    if write_frame(&mut send, &head, MAX_HTTP_FRAME_BYTES)
        .await
        .is_err()
    {
        return;
    }
    info!(request = %id, path = %log_path, "WebSocket opened");
    let started = Instant::now();
    let stop = websocket_stop(service, connection, session);
    let end = websocket::relay(&mut send, &mut recv, io, |bytes| stats.add_bytes(bytes), stop).await;
    match end {
        RelayEnd::Closed => {}
        RelayEnd::Failed => {
            let _ = send.reset(VarInt::from_u32(close_code::UPSTREAM_FAILED));
        }
        RelayEnd::Stopped(code) => {
            let _ = send.reset(VarInt::from_u32(code));
        }
    }
    info!(request = %id, path = %log_path, ?end, open_secs = started.elapsed().as_secs(), "WebSocket closed");
}

/// Resolves with a close code once an open WebSocket must end.
async fn websocket_stop(
    service: &HostService,
    connection: &Connection,
    session: &CancellationToken,
) -> u32 {
    let mut recheck = tokio::time::interval(AUTHORIZATION_RECHECK);
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = session.cancelled() => return close_code::SHUTTING_DOWN,
            _ = recheck.tick() => {
                if !service.registry.websocket().enabled {
                    return close_code::NORMAL;
                }
                if !service.registry.policy().admits(current_path(connection).kind) {
                    return close_code::TRANSPORT_POLICY;
                }
            }
        }
    }
}

/// Passes an origin's non-101 answer to an upgrade back to the client as an ordinary response.
async fn send_refusal(
    send: &mut SendStream,
    id: &str,
    response: hyper::Response<Incoming>,
    stats: &SessionStats,
    session: &CancellationToken,
) {
    let head = Message::HttpResponse(HttpResponse {
        request_id: id.to_owned(),
        status: response.status().as_u16(),
        headers: proxy::response_headers(&response),
    });
    if write_frame(send, &head, MAX_HTTP_FRAME_BYTES).await.is_err() {
        return;
    }
    let outcome = tokio::select! {
        _ = session.cancelled() => None,
        outcome = pump_body(send, response.into_body(), stats) => Some(outcome),
    };
    match outcome {
        Some(BodyOutcome::Complete(_)) => {
            let _ = send.finish();
        }
        Some(BodyOutcome::ClientGone(_)) => {}
        Some(BodyOutcome::UpstreamFailed(_)) => {
            let _ = send.reset(VarInt::from_u32(close_code::UPSTREAM_FAILED));
        }
        None => {
            let _ = send.reset(VarInt::from_u32(close_code::SHUTTING_DOWN));
        }
    }
}

/// Waits briefly for a path the policy admits. Under `p2p_only`, Iroh often starts on a relay and
/// upgrades to a direct path within a few seconds, so refusing immediately would be premature.
async fn admitted_path(
    connection: &Connection,
    policy: TransportPolicy,
    session: &CancellationToken,
) -> Result<PathReport, PathReport> {
    let wait = match policy {
        TransportPolicy::P2pOnly => DIRECT_PATH_WAIT,
        _ => Duration::ZERO,
    };
    let deadline = Instant::now() + wait;
    loop {
        let path = current_path(connection);
        if policy.admits(path.kind) {
            return Ok(path);
        }
        if Instant::now() >= deadline || session.is_cancelled() {
            return Err(path);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Streams the response body, stopping early if the client stops reading or resets the stream.
async fn pump_body(send: &mut SendStream, mut body: Incoming, stats: &SessionStats) -> BodyOutcome {
    let mut sent = 0u64;
    loop {
        let frame = tokio::select! {
            frame = body.frame() => frame,
            _ = send.stopped() => return BodyOutcome::ClientGone(sent),
        };
        let frame = match frame {
            None => return BodyOutcome::Complete(sent),
            Some(Ok(frame)) => frame,
            Some(Err(error)) => {
                debug!(%error, "origin body error");
                return BodyOutcome::UpstreamFailed(sent);
            }
        };
        if let Ok(data) = frame.into_data() {
            let length = data.len() as u64;
            if send.write_chunk(data).await.is_err() {
                return BodyOutcome::ClientGone(sent);
            }
            sent += length;
            stats.add_bytes(length);
        }
    }
}

fn request_body(recv: RecvStream) -> ProxyBody {
    let frames = ReaderStream::with_capacity(recv, BODY_CHUNK_BYTES).map_ok(Frame::data);
    Limited::new(StreamBody::new(frames), MAX_REQUEST_BODY_BYTES).boxed_unsync()
}

async fn reply_error(send: &mut SendStream, request_id: &str, code: &str, message: &str) {
    let error = Message::StreamError(StreamError {
        request_id: Some(request_id.to_owned()),
        code: code.to_owned(),
        message: message.to_owned(),
    });
    if write_frame(send, &error, MAX_HTTP_FRAME_BYTES)
        .await
        .is_ok()
    {
        let _ = send.finish();
    }
}

fn close(connection: &Connection, code: u32, reason: &str) {
    connection.close(VarInt::from_u32(code), reason.as_bytes());
}
