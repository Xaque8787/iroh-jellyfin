use super::*;
use bytes::Bytes;
use futures_util::TryStreamExt;
use http::{Request, Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use iroh_transport::client::{ClientResponse, WebSocketReply};
use iroh_transport::protocol::{BODY_CHUNK_BYTES, MAX_REQUEST_BODY_BYTES};
use iroh_transport::websocket;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;

type ProxyBody = UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

pub(super) struct ProxyState {
    common: ClientConfig,
    endpoint: Endpoint,
    pub(super) session: Mutex<Option<Arc<ClientSession>>>,
    pub(super) last_error: Mutex<Option<String>>,
    cancel: CancellationToken,
    prefix: Option<String>,
    upgrades: tokio_util::task::TaskTracker,
}

impl ProxyState {
    /// Returns the open session, reconnecting if the previous one has closed.
    async fn session(&self) -> Result<Arc<ClientSession>, Error> {
        let mut slot = self.session.lock().await;
        if let Some(session) = slot.as_ref()
            && session.connection().close_reason().is_none()
        {
            return Ok(session.clone());
        }
        let session = match connect(&self.common, &self.endpoint).await {
            Ok(session) => Arc::new(session),
            Err(error) => {
                *self.last_error.lock().await = Some(error.to_string());
                return Err(error);
            }
        };
        *self.last_error.lock().await = None;
        *slot = Some(session.clone());
        Ok(session)
    }
}

async fn proxy_request(
    state: Arc<ProxyState>,
    mut request: Request<Incoming>,
) -> Result<Response<ProxyBody>, std::convert::Infallible> {
    if let Some(prefix) = &state.prefix {
        let uri = request
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        let Some(rest) = uri.strip_prefix(prefix) else {
            return Ok(text_response(
                StatusCode::FORBIDDEN,
                "proxy capability required".into(),
            ));
        };
        if !rest.starts_with('/') {
            return Ok(text_response(
                StatusCode::FORBIDDEN,
                "proxy capability required".into(),
            ));
        }
        match rest.parse() {
            Ok(uri) => *request.uri_mut() = uri,
            Err(_) => return Ok(text_response(StatusCode::BAD_REQUEST, "invalid URI".into())),
        }
    }
    Ok(match forward(&state, request).await {
        Ok(response) => response,
        Err((status, message)) => text_response(status, message),
    })
}

async fn forward(
    state: &ProxyState,
    request: Request<Incoming>,
) -> Result<Response<ProxyBody>, (StatusCode, String)> {
    let method = request.method().as_str().to_owned();
    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_owned())
        .unwrap_or_else(|| "/".into());
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    if websocket::is_upgrade_request(request.method(), request.headers()) {
        return forward_websocket(state, request, &path, headers).await;
    }
    let body = Limited::new(request.into_body(), MAX_REQUEST_BODY_BYTES)
        .collect()
        .await
        .map_err(|_| {
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body is too large".into(),
            )
        })?
        .to_bytes();
    let body = (!body.is_empty()).then_some(body);

    let session = state
        .session()
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;
    let response = session
        .request(&method, &path, headers, body)
        .await
        .map_err(gateway_error)?;
    stream_response(response)
}

async fn forward_websocket(
    state: &ProxyState,
    request: Request<Incoming>,
    path: &str,
    headers: Vec<(String, String)>,
) -> Result<Response<ProxyBody>, (StatusCode, String)> {
    let session = state
        .session()
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;
    let socket = match session
        .open_websocket(path, headers)
        .await
        .map_err(gateway_error)?
    {
        WebSocketReply::Open(socket) => socket,
        WebSocketReply::Refused(response) => return stream_response(response),
    };
    let mut builder = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(http::header::CONNECTION, "upgrade")
        .header(http::header::UPGRADE, "websocket");
    for (name, value) in &socket.head.headers {
        builder = builder.header(name, value);
    }
    let response = builder
        .body(empty_body())
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;
    let cancel = state.cancel.clone();
    state.upgrades.spawn(async move {
        tokio::select! {
        _ = cancel.cancelled() => {},
        _ = async move {
        let mut socket = socket;
        if let Ok(upgraded) = hyper::upgrade::on(request).await {
                let end = websocket::relay(
                    &mut socket.send,
                    &mut socket.recv,
                    TokioIo::new(upgraded),
                    |_| {},
                    std::future::pending::<u32>(),
                )
                .await;
                let _ = end;
        }
        } => {}
        }
    });
    Ok(response)
}

fn gateway_error(error: ClientError) -> (StatusCode, String) {
    let status = match &error {
        ClientError::Request { code, .. } if code == "transport_policy_denied" => {
            StatusCode::FORBIDDEN
        }
        ClientError::Request { code, .. } if code == "upstream_timeout" => {
            StatusCode::GATEWAY_TIMEOUT
        }
        ClientError::Request { code, .. } if code == "unsupported" => StatusCode::NOT_IMPLEMENTED,
        ClientError::Request { code, .. } if code == "too_many_websockets" => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        _ => StatusCode::BAD_GATEWAY,
    };
    (status, error.to_string())
}

fn stream_response(response: ClientResponse) -> Result<Response<ProxyBody>, (StatusCode, String)> {
    let mut builder = Response::builder().status(response.head.status);
    for (name, value) in &response.head.headers {
        builder = builder.header(name, value);
    }
    let frames = ReaderStream::with_capacity(response.body, BODY_CHUNK_BYTES)
        .map_ok(Frame::data)
        .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>);
    builder
        .body(StreamBody::new(frames).boxed_unsync())
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))
}

fn empty_body() -> ProxyBody {
    Full::new(Bytes::new())
        .map_err(|never| match never {})
        .boxed_unsync()
}

fn text_response(status: StatusCode, message: String) -> Response<ProxyBody> {
    let mut response = Response::new(
        Full::new(Bytes::from(message))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response
}

pub(super) struct Proxy {
    pub connection: ProxyConnection,
    pub state: Arc<ProxyState>,
    task: JoinHandle<()>,
}
impl Proxy {
    pub async fn start(
        common: ClientConfig,
        endpoint: Endpoint,
        listen: SocketAddr,
        protect_url: bool,
    ) -> Result<Self, Error> {
        let cancel = CancellationToken::new();
        let prefix = protect_url.then(|| format!("/{}", iroh::SecretKey::generate().public()));
        let state = Arc::new(ProxyState {
            common,
            endpoint,
            session: Mutex::new(None),
            last_error: Mutex::new(None),
            cancel: cancel.clone(),
            prefix,
            upgrades: tokio_util::task::TaskTracker::new(),
        });
        state.session().await?;
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let address = listener
            .local_addr()
            .map_err(|e| Error::Transport(e.to_string()))?;
        let connection = ProxyConnection {
            proxy_url: format!(
                "http://{}{}/",
                address,
                state.prefix.as_deref().unwrap_or("")
            ),
        };
        let serve_state = state.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    Some(_) = connections.join_next(), if !connections.is_empty() => {},
                    accepted = listener.accept() => {
                        let (socket, _) = match accepted { Ok(s) => s, Err(e) => { *serve_state.last_error.lock().await = Some(e.to_string()); break; } };
                        let state = serve_state.clone();
                        connections.spawn(async move {
                            let service = service_fn(move |request| proxy_request(state.clone(), request));
                            let _ = http1::Builder::new().serve_connection(TokioIo::new(socket), service).with_upgrades().await;
                        });
                    }
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Ok(Self {
            connection,
            state,
            task,
        })
    }
    pub async fn stop(&mut self) {
        self.state.cancel.cancel();
        let _ = (&mut self.task).await;
        self.state.upgrades.close();
        self.state.upgrades.wait().await;
        if let Some(session) = self.state.session.lock().await.take() {
            session.connection().close(0u32.into(), b"proxy stopped");
        }
    }
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.state.cancel.cancel();
        self.task.abort();
    }
}
