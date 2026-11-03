//! Control API v1: a local HTTP+JSON interface for the media server plugin. See
//! protocol/control-v1.md. Every route is answered by `route`, which is synchronous and does the
//! blocking state I/O, so the HTTP layer just runs it on a blocking thread.

use std::collections::HashSet;
use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use iroh::EndpointAddr;
use iroh_transport::invite::InviteLink;
use iroh_transport::net::RelayChoice;
use iroh_transport::pairing::{generate_token, hash_token};
use iroh_transport::policy::TransportPolicy;
use iroh_transport::protocol::PROTOCOL_VERSION;
use iroh_transport::sessions::SessionTable;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::fsutil::write_private_atomic;
use crate::state::{HostState, StateError, now_secs};

pub const CONTROL_API_VERSION: u32 = 1;
pub const TOKEN_FILE: &str = "control.token";
const MAX_BODY_BYTES: usize = 16 * 1024;
const MIN_TOKEN_CHARS: usize = 32;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Returns the configured token, else the one in `control.token`, creating that file if missing.
pub fn load_or_create_token(data_dir: &Path, configured: Option<&str>) -> Result<String, String> {
    if let Some(token) = configured {
        return Ok(token.to_owned());
    }
    let path = data_dir.join(TOKEN_FILE);
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let token = contents.trim();
            if token.chars().count() < MIN_TOKEN_CHARS {
                return Err(format!(
                    "{} must hold at least {MIN_TOKEN_CHARS} characters; delete it to generate a new token",
                    path.display()
                ));
            }
            crate::fsutil::restrict_permissions(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            Ok(token.to_owned())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let token = generate_token();
            write_private_atomic(&path, format!("{token}\n").as_bytes())
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            Ok(token)
        }
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

pub struct ControlContext {
    pub state: Arc<HostState>,
    pub sessions: Arc<SessionTable>,
    /// The endpoint's current address. A closure so tests need no live endpoint.
    pub addr: Box<dyn Fn() -> EndpointAddr + Send + Sync>,
    pub origin: String,
    pub relay: RelayChoice,
    /// Whether this endpoint was started without IP transports (relay_only at startup).
    pub relay_transport_only: bool,
    pub started: Instant,
    token_digest: String,
}

impl ControlContext {
    pub fn new(
        state: Arc<HostState>,
        sessions: Arc<SessionTable>,
        addr: Box<dyn Fn() -> EndpointAddr + Send + Sync>,
        origin: String,
        relay: RelayChoice,
        relay_transport_only: bool,
        token: &str,
    ) -> Self {
        Self {
            state,
            sessions,
            addr,
            origin,
            relay,
            relay_transport_only,
            started: Instant::now(),
            token_digest: hash_token(token),
        }
    }

    /// Compares digests, so the comparison time does not depend on how much of the token matched.
    fn authorized(&self, header: Option<&str>) -> bool {
        let Some(presented) = header.and_then(|value| value.strip_prefix("Bearer ")) else {
            return false;
        };
        hash_token(presented.trim()) == self.token_digest
    }
}

pub async fn serve_control(
    listener: TcpListener,
    context: Arc<ControlContext>,
    shutdown: CancellationToken,
) {
    loop {
        let socket = tokio::select! {
            _ = shutdown.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((socket, _)) => socket,
                Err(error) => {
                    warn!(%error, "control API accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let context = context.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| handle(context.clone(), request));
            let connection = http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .serve_connection(TokioIo::new(socket), service);
            if let Err(error) = connection.await {
                debug!(%error, "control API connection ended");
            }
        });
    }
}

async fn handle(
    context: Arc<ControlContext>,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let authorization = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = match Limited::new(request.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(error) if error.downcast_ref::<LengthLimitError>().is_some() => {
            return Ok(respond(Reply::error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "request body is larger than 16 KiB",
            )));
        }
        Err(_) => {
            return Ok(respond(Reply::error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "could not read the request body",
            )));
        }
    };
    let reply = tokio::task::spawn_blocking(move || {
        route(&context, &method, &path, authorization.as_deref(), &body)
    })
    .await
    .unwrap_or_else(|_| {
        Reply::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "handler failed",
        )
    });
    Ok(respond(reply))
}

fn respond(reply: Reply) -> Response<Full<Bytes>> {
    let builder = Response::builder()
        .status(reply.status)
        .header(CACHE_CONTROL, "no-store");
    let response = match reply.body {
        Some(body) => builder
            .header(CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Full::new(Bytes::from(body.to_string()))),
        None => builder.body(Full::new(Bytes::new())),
    };
    response.unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

#[derive(Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub body: Option<Value>,
}

impl Reply {
    fn json(status: StatusCode, body: Value) -> Self {
        Self {
            status,
            body: Some(body),
        }
    }

    fn no_content() -> Self {
        Self {
            status: StatusCode::NO_CONTENT,
            body: None,
        }
    }

    fn error(status: StatusCode, code: &str, message: &str) -> Self {
        Self::json(status, json!({"error": {"code": code, "message": message}}))
    }

    fn not_found(message: &str) -> Self {
        Self::error(StatusCode::NOT_FOUND, "not_found", message)
    }

    fn method_not_allowed() -> Self {
        Self::error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "method not allowed for this path",
        )
    }

    fn state_error(error: StateError) -> Self {
        match error {
            StateError::Invalid(message) => {
                Self::error(StatusCode::BAD_REQUEST, "bad_request", &message)
            }
            error => {
                warn!(%error, "control API state operation failed");
                Self::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "could not update host state",
                )
            }
        }
    }
}

#[derive(Deserialize)]
struct NewInvitation {
    label: String,
    #[serde(default)]
    ttl_secs: Option<u64>,
}

#[derive(Deserialize)]
struct PolicyChange {
    policy: TransportPolicy,
}

fn parse_body<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, Reply> {
    serde_json::from_slice(body).map_err(|error| {
        Reply::error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            &format!("invalid JSON body: {error}"),
        )
    })
}

pub fn route(
    context: &ControlContext,
    method: &str,
    path: &str,
    authorization: Option<&str>,
    body: &[u8],
) -> Reply {
    let segments: Vec<&str> = path.trim_end_matches('/').split('/').skip(1).collect();
    let Some((&"v1", rest)) = segments.split_first() else {
        return Reply::not_found("unknown path; this host serves control API v1 under /v1/");
    };
    if rest == ["health"] {
        return match method {
            "GET" => Reply::json(
                StatusCode::OK,
                json!({"ok": true, "control_api": CONTROL_API_VERSION}),
            ),
            _ => Reply::method_not_allowed(),
        };
    }
    if !context.authorized(authorization) {
        return Reply::error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or wrong control token",
        );
    }
    let result = match (rest, method) {
        (["status"], "GET") => Ok(status(context)),
        (["sessions"], "GET") => Ok(Reply::json(
            StatusCode::OK,
            json!({"sessions": context.sessions.snapshot()}),
        )),
        (["devices"], "GET") => Ok(devices(context)),
        (["devices", id], "DELETE") => revoke(context, id),
        (["invitations"], "GET") => Ok(invitations(context)),
        (["invitations"], "POST") => create_invitation(context, body),
        (["invitations", id], "DELETE") => cancel_invitation(context, id),
        (["policy"], "PUT") => set_policy(context, body),
        (["status" | "sessions" | "devices" | "invitations" | "policy"], _)
        | (["devices" | "invitations", _], _) => Err(Reply::method_not_allowed()),
        _ => Err(Reply::not_found("unknown control API path")),
    };
    result.unwrap_or_else(|reply| reply)
}

fn policy_json(context: &ControlContext, configured: TransportPolicy) -> Value {
    json!({
        "configured": configured.as_str(),
        "enforcement": configured.enforcement(),
        "relay_transport_only": context.relay_transport_only,
        "restart_required": context.relay_transport_only != (configured == TransportPolicy::RelayOnly),
    })
}

fn status(context: &ControlContext) -> Reply {
    let snapshot = context.state.snapshot();
    let now = now_secs();
    let addr = (context.addr)();
    let (mode, url) = match &context.relay {
        RelayChoice::Default => ("default", None),
        RelayChoice::Custom(url) => ("custom", Some(url.to_string())),
        RelayChoice::Disabled => ("disabled", None),
    };
    Reply::json(
        StatusCode::OK,
        json!({
            "control_api": CONTROL_API_VERSION,
            "transport_protocol": PROTOCOL_VERSION,
            "host_version": env!("CARGO_PKG_VERSION"),
            "endpoint_id": addr.id.to_string(),
            "origin": context.origin,
            "relay": {"mode": mode, "url": url},
            "relay_connected": addr.addrs.iter().any(|transport| transport.is_relay()),
            "addr": addr,
            "policy": policy_json(context, snapshot.policy),
            "devices": snapshot.devices.iter().filter(|device| device.is_active()).count(),
            "pending_invitations": snapshot
                .invitations
                .iter()
                .filter(|invitation| !invitation.is_redeemed() && invitation.expires_at > now)
                .count(),
            "sessions": context.sessions.snapshot().len(),
            "uptime_secs": context.started.elapsed().as_secs(),
        }),
    )
}

fn devices(context: &ControlContext) -> Reply {
    let connected: HashSet<String> = context
        .sessions
        .snapshot()
        .into_iter()
        .map(|session| session.endpoint_id)
        .collect();
    let devices: Vec<Value> = context
        .state
        .snapshot()
        .devices
        .iter()
        .map(|device| {
            json!({
                "endpoint_id": device.endpoint_id,
                "name": device.name,
                "paired_at": device.paired_at,
                "invitation_id": device.invitation_id,
                "revoked_at": device.revoked_at,
                "connected": device.is_active() && connected.contains(&device.endpoint_id),
            })
        })
        .collect();
    Reply::json(StatusCode::OK, json!({"devices": devices}))
}

fn revoke(context: &ControlContext, endpoint_id: &str) -> Result<Reply, Reply> {
    match context.state.revoke_device(endpoint_id, now_secs()) {
        Ok(true) => Ok(Reply::no_content()),
        Ok(false) => Err(Reply::not_found("no active device with that endpoint id")),
        Err(error) => Err(Reply::state_error(error)),
    }
}

fn invitations(context: &ControlContext) -> Reply {
    let now = now_secs();
    let pending: Vec<Value> = context
        .state
        .snapshot()
        .invitations
        .iter()
        .filter(|invitation| !invitation.is_redeemed() && invitation.expires_at > now)
        .map(|invitation| {
            json!({
                "id": invitation.id,
                "label": invitation.label,
                "created_at": invitation.created_at,
                "expires_at": invitation.expires_at,
            })
        })
        .collect();
    Reply::json(StatusCode::OK, json!({"invitations": pending}))
}

fn create_invitation(context: &ControlContext, body: &[u8]) -> Result<Reply, Reply> {
    let request: NewInvitation = parse_body(body)?;
    let (invitation, token) = context
        .state
        .create_invitation(&request.label, request.ttl_secs, now_secs())
        .map_err(Reply::state_error)?;
    let link = InviteLink::new((context.addr)(), token).encode();
    Ok(Reply::json(
        StatusCode::CREATED,
        json!({
            "id": invitation.id,
            "label": invitation.label,
            "created_at": invitation.created_at,
            "expires_at": invitation.expires_at,
            "link": link,
        }),
    ))
}

fn cancel_invitation(context: &ControlContext, id: &str) -> Result<Reply, Reply> {
    match context.state.cancel_invitation(id) {
        Ok(true) => Ok(Reply::no_content()),
        Ok(false) => Err(Reply::not_found("no unredeemed invitation with that id")),
        Err(error) => Err(Reply::state_error(error)),
    }
}

fn set_policy(context: &ControlContext, body: &[u8]) -> Result<Reply, Reply> {
    let request: PolicyChange = parse_body(body)?;
    if request.policy == TransportPolicy::RelayOnly && context.relay == RelayChoice::Disabled {
        return Err(Reply::error(
            StatusCode::CONFLICT,
            "conflict",
            "relay_only needs a relay, but this host runs with relays disabled",
        ));
    }
    context
        .state
        .set_policy(request.policy)
        .map_err(Reply::state_error)?;
    Ok(Reply::json(
        StatusCode::OK,
        policy_json(context, request.policy),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::test_support::TempDir;
    use crate::state::STATE_FILE;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const AUTH: Option<&str> = Some("Bearer 0123456789abcdef0123456789abcdef");

    struct Fixture {
        _dir: TempDir,
        context: ControlContext,
    }

    fn fixture(relay: RelayChoice) -> Fixture {
        let dir = TempDir::create();
        let state = Arc::new(HostState::open(&dir.0.join(STATE_FILE), None).unwrap());
        let key = crate::identity::load_or_create(&dir.0.join("identity.key")).unwrap();
        let id = key.public();
        let context = ControlContext::new(
            state,
            Arc::default(),
            Box::new(move || EndpointAddr::new(id)),
            "http://127.0.0.1:8096/".into(),
            relay,
            false,
            TOKEN,
        );
        Fixture { _dir: dir, context }
    }

    fn call(fixture: &Fixture, method: &str, path: &str, body: &str) -> Reply {
        route(&fixture.context, method, path, AUTH, body.as_bytes())
    }

    fn body(reply: &Reply) -> &Value {
        reply.body.as_ref().expect("reply has a body")
    }

    #[test]
    fn health_is_public_and_everything_else_needs_the_token() {
        let fixture = fixture(RelayChoice::Default);
        let health = route(&fixture.context, "GET", "/v1/health", None, b"");
        assert_eq!(health.status, StatusCode::OK);
        assert_eq!(body(&health)["control_api"], 1);
        for authorization in [None, Some("Bearer wrong"), Some(TOKEN)] {
            let reply = route(&fixture.context, "GET", "/v1/status", authorization, b"");
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        }
        assert_eq!(
            call(&fixture, "GET", "/v1/status", "").status,
            StatusCode::OK
        );
    }

    #[test]
    fn reports_status() {
        let fixture = fixture(RelayChoice::Disabled);
        let reply = call(&fixture, "GET", "/v1/status/", "");
        let status = body(&reply);
        assert_eq!(status["transport_protocol"], PROTOCOL_VERSION);
        assert_eq!(status["relay"]["mode"], "disabled");
        assert_eq!(status["policy"]["configured"], "auto");
        assert_eq!(status["policy"]["restart_required"], false);
        assert_eq!(status["sessions"], 0);
        assert_eq!(status["endpoint_id"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn invitations_can_be_created_listed_and_cancelled() {
        let fixture = fixture(RelayChoice::Default);
        let created = call(
            &fixture,
            "POST",
            "/v1/invitations",
            r#"{"label":"Phone","ttl_secs":600}"#,
        );
        assert_eq!(created.status, StatusCode::CREATED);
        let created = body(&created);
        let link = InviteLink::decode(created["link"].as_str().unwrap()).unwrap();
        assert_eq!(link.addr, (fixture.context.addr)());
        let id = created["id"].as_str().unwrap().to_owned();

        let listed = call(&fixture, "GET", "/v1/invitations", "");
        assert_eq!(body(&listed)["invitations"][0]["id"], id.as_str());
        assert!(!body(&listed).to_string().contains(&link.token));

        let path = format!("/v1/invitations/{id}");
        assert_eq!(
            call(&fixture, "DELETE", &path, "").status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&fixture, "DELETE", &path, "").status,
            StatusCode::NOT_FOUND
        );
        let listed = call(&fixture, "GET", "/v1/invitations", "");
        assert_eq!(body(&listed)["invitations"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn rejects_bad_invitation_requests() {
        let fixture = fixture(RelayChoice::Default);
        for request in [
            "not json",
            r#"{"label":""}"#,
            r#"{"label":"x","ttl_secs":5}"#,
        ] {
            let reply = call(&fixture, "POST", "/v1/invitations", request);
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{request}");
        }
    }

    #[test]
    fn revokes_paired_devices() {
        let fixture = fixture(RelayChoice::Default);
        let state = &fixture.context.state;
        let (_, token) = state.create_invitation("TV", None, now_secs()).unwrap();
        state
            .pair("device-a", "TV", Some(&token), now_secs())
            .unwrap()
            .unwrap();
        let listed = call(&fixture, "GET", "/v1/devices", "");
        assert_eq!(body(&listed)["devices"][0]["endpoint_id"], "device-a");
        assert_eq!(body(&listed)["devices"][0]["connected"], false);

        assert_eq!(
            call(&fixture, "DELETE", "/v1/devices/device-a", "").status,
            StatusCode::NO_CONTENT
        );
        assert!(!state.is_authorized("device-a"));
        assert_eq!(
            call(&fixture, "DELETE", "/v1/devices/device-a", "").status,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn policy_changes_are_saved_and_report_restarts() {
        let fixture = fixture(RelayChoice::Default);
        let reply = call(&fixture, "PUT", "/v1/policy", r#"{"policy":"p2p_only"}"#);
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(body(&reply)["restart_required"], false);
        assert_eq!(fixture.context.state.policy(), TransportPolicy::P2pOnly);

        let reply = call(&fixture, "PUT", "/v1/policy", r#"{"policy":"relay_only"}"#);
        assert_eq!(body(&reply)["restart_required"], true);
        assert_eq!(
            call(&fixture, "PUT", "/v1/policy", r#"{"policy":"direct"}"#).status,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn relay_only_is_refused_without_relays() {
        let fixture = fixture(RelayChoice::Disabled);
        let reply = call(&fixture, "PUT", "/v1/policy", r#"{"policy":"relay_only"}"#);
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(fixture.context.state.policy(), TransportPolicy::Auto);
    }

    #[test]
    fn unknown_paths_and_methods() {
        let fixture = fixture(RelayChoice::Default);
        assert_eq!(
            call(&fixture, "GET", "/v2/status", "").status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&fixture, "GET", "/v1/nothing", "").status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&fixture, "POST", "/v1/status", "").status,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            call(&fixture, "GET", "/v1/devices/x", "").status,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[test]
    fn token_file_is_created_once_and_private() {
        let dir = TempDir::create();
        let first = load_or_create_token(&dir.0, None).unwrap();
        let second = load_or_create_token(&dir.0, None).unwrap();
        assert_eq!(first, second);
        assert!(first.len() >= MIN_TOKEN_CHARS);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.0.join(TOKEN_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(load_or_create_token(&dir.0, Some(TOKEN)).unwrap(), TOKEN);
        std::fs::write(dir.0.join(TOKEN_FILE), "short").unwrap();
        assert!(load_or_create_token(&dir.0, None).is_err());
    }
}
