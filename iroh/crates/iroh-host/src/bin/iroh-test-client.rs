//! Test client for transport v1: pairs with a host, fetches paths, and runs a local HTTP proxy so a
//! browser or media app on this machine can reach the host's origin over Iroh.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use clap::{Args, Parser, Subcommand};
use futures_util::TryStreamExt;
use http::{Request, Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use iroh::{Endpoint, EndpointAddr, SecretKey};
use iroh_host::config::parse_relay;
use iroh_host::fsutil::{ensure_private_dir, write_private_atomic};
use iroh_host::identity;
use iroh_transport::client::{ClientError, ClientResponse, ClientSession, WebSocketReply};
use iroh_transport::invite::InviteLink;
use iroh_transport::net::{EndpointOptions, bind_endpoint};
use iroh_transport::protocol::{BODY_CHUNK_BYTES, MAX_REQUEST_BODY_BYTES};
use iroh_transport::websocket;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::io::ReaderStream;

const HOST_FILE: &str = "host.json";
const KEY_FILE: &str = "client.key";

#[derive(Debug, Parser)]
#[command(
    name = "iroh-test-client",
    version,
    about = "Test client for jellyfin-iroh hosts"
)]
struct Cli {
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args)]
struct Common {
    /// Directory holding this client's identity and the paired host's address.
    #[arg(
        long,
        global = true,
        env = "IROH_CLIENT_DATA_DIR",
        default_value = "./iroh-client-data"
    )]
    data_dir: PathBuf,
    /// Relay URL. Empty uses Iroh's default relays; `none` disables relays.
    #[arg(long, global = true, env = "IROH_RELAY_URL")]
    relay_url: Option<String>,
    /// Name the host shows for this device.
    #[arg(long, global = true, default_value = "iroh-test-client")]
    name: String,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Redeem an invitation link and remember the host.
    Pair { link: String },
    /// Connect and report authorization, policy, path, and round-trip times.
    Status,
    /// Fetch one path and write the body to stdout or a file.
    Fetch {
        /// Origin-form path, for example /System/Info/Public
        path: String,
        /// Range header value, for example bytes=0-1048575
        #[arg(long)]
        range: Option<String>,
        /// Extra header as name:value. Repeatable.
        #[arg(long = "header")]
        headers: Vec<String>,
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Serve the host's origin on a local port, for example to open Jellyfin in a browser.
    Proxy {
        #[arg(long, default_value = "127.0.0.1:8097")]
        listen: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let common = cli.common;
    ensure_private_dir(&common.data_dir).map_err(|error| error.to_string())?;
    let secret_key = identity::load_or_create(&common.data_dir.join(KEY_FILE))
        .map_err(|error| error.to_string())?;
    let endpoint = bind(&common, secret_key).await?;
    let result = match cli.command {
        Command::Pair { link } => pair(&common, &endpoint, &link).await,
        Command::Status => status(&common, &endpoint).await,
        Command::Fetch {
            path,
            range,
            headers,
            output,
        } => fetch(&common, &endpoint, &path, range, headers, output).await,
        Command::Proxy { listen } => proxy(common, endpoint.clone(), listen).await,
    };
    endpoint.close().await;
    result
}

async fn bind(common: &Common, secret_key: SecretKey) -> Result<Endpoint, String> {
    let relay = parse_relay(common.relay_url.as_deref())?;
    let options = EndpointOptions {
        relay: Some(relay),
        relay_transport_only: false,
    };
    bind_endpoint(secret_key, vec![], options)
        .await
        .map_err(|error| format!("could not start the Iroh endpoint: {error}"))
}

fn saved_host(data_dir: &Path) -> Result<EndpointAddr, String> {
    let bytes = std::fs::read(data_dir.join(HOST_FILE))
        .map_err(|_| "no paired host yet; run `pair <link>` first".to_owned())?;
    serde_json::from_slice(&bytes).map_err(|error| format!("{HOST_FILE} is damaged: {error}"))
}

async fn connect(common: &Common, endpoint: &Endpoint) -> Result<ClientSession, String> {
    let addr = saved_host(&common.data_dir)?;
    ClientSession::connect(endpoint, addr, &common.name, None)
        .await
        .map_err(|error| error.to_string())
}

async fn pair(common: &Common, endpoint: &Endpoint, link: &str) -> Result<(), String> {
    let invite = InviteLink::decode(link).map_err(|error| error.to_string())?;
    let session = ClientSession::connect(
        endpoint,
        invite.addr.clone(),
        &common.name,
        Some(&invite.token),
    )
    .await
    .map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec_pretty(&invite.addr).map_err(|error| error.to_string())?;
    write_private_atomic(&common.data_dir.join(HOST_FILE), &bytes)
        .map_err(|error| error.to_string())?;
    println!(
        "{:?}: host {} (transport policy {})",
        session.outcome(),
        invite.addr.id,
        session.transport_policy().as_str()
    );
    session.close().await;
    Ok(())
}

async fn status(common: &Common, endpoint: &Endpoint) -> Result<(), String> {
    let session = connect(common, endpoint).await?;
    println!("authorization: {:?}", session.outcome());
    println!(
        "transport policy: {} ({})",
        session.transport_policy().as_str(),
        session.transport_policy().enforcement()
    );
    for attempt in 1..=5 {
        let rtt = session.ping().await.map_err(|error| error.to_string())?;
        let path = session.path();
        println!(
            "ping {attempt}: {:.1} ms over {:?} path (Iroh rtt {:?} ms, {} open path(s), remote {})",
            rtt.as_secs_f64() * 1000.0,
            path.kind,
            path.rtt_ms,
            path.open_paths,
            path.remote_addr.unwrap_or_else(|| "-".into())
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    session.close().await;
    Ok(())
}

async fn fetch(
    common: &Common,
    endpoint: &Endpoint,
    path: &str,
    range: Option<String>,
    extra: Vec<String>,
    output: Option<PathBuf>,
) -> Result<(), String> {
    let mut headers = Vec::new();
    for header in extra {
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| format!("header '{header}' must be name:value"))?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    if let Some(range) = range {
        headers.push(("range".into(), range));
    }
    let session = connect(common, endpoint).await?;
    let mut response = session
        .request("GET", path, headers, None)
        .await
        .map_err(|error| error.to_string())?;
    eprintln!("status {}", response.head.status);
    for (name, value) in &response.head.headers {
        eprintln!("{name}: {value}");
    }
    let copied = match output {
        Some(file) => {
            let mut file = tokio::fs::File::create(&file)
                .await
                .map_err(|error| format!("{}: {error}", file.display()))?;
            let copied = tokio::io::copy(&mut response.body, &mut file).await;
            file.flush().await.map_err(|error| error.to_string())?;
            copied
        }
        None => tokio::io::copy(&mut response.body, &mut tokio::io::stdout()).await,
    }
    .map_err(|error| format!("body transfer failed: {error}"))?;
    eprintln!(
        "received {copied} bytes over {:?} path",
        session.path().kind
    );
    session.close().await;
    Ok(())
}

type ProxyBody = UnsyncBoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

struct ProxyState {
    common: Common,
    endpoint: Endpoint,
    session: Mutex<Option<Arc<ClientSession>>>,
}

impl ProxyState {
    /// Returns the open session, reconnecting if the previous one has closed.
    async fn session(&self) -> Result<Arc<ClientSession>, String> {
        let mut slot = self.session.lock().await;
        if let Some(session) = slot.as_ref()
            && session.connection().close_reason().is_none()
        {
            return Ok(session.clone());
        }
        let session = Arc::new(connect(&self.common, &self.endpoint).await?);
        eprintln!(
            "connected: {:?}, policy {}, path {:?}",
            session.outcome(),
            session.transport_policy().as_str(),
            session.path().kind
        );
        *slot = Some(session.clone());
        Ok(session)
    }
}

async fn proxy(common: Common, endpoint: Endpoint, listen: SocketAddr) -> Result<(), String> {
    let state = Arc::new(ProxyState {
        common,
        endpoint,
        session: Mutex::new(None),
    });
    state.session().await?;
    let listener = TcpListener::bind(listen)
        .await
        .map_err(|error| format!("cannot listen on {listen}: {error}"))?;
    eprintln!("proxying http://{listen} to the host; press Ctrl-C to stop");
    loop {
        let (socket, _) = tokio::select! {
            accepted = listener.accept() => accepted.map_err(|error| error.to_string())?,
            _ = tokio::signal::ctrl_c() => return Ok(()),
        };
        let state = state.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| proxy_request(state.clone(), request));
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(socket), service)
                .with_upgrades()
                .await;
        });
    }
}

async fn proxy_request(
    state: Arc<ProxyState>,
    request: Request<Incoming>,
) -> Result<Response<ProxyBody>, std::convert::Infallible> {
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
        .map_err(|error| (StatusCode::BAD_GATEWAY, error))?;
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
        .map_err(|error| (StatusCode::BAD_GATEWAY, error))?;
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
    tokio::spawn(async move {
        let mut socket = socket;
        match hyper::upgrade::on(request).await {
            Ok(upgraded) => {
                let end = websocket::relay(
                    &mut socket.send,
                    &mut socket.recv,
                    TokioIo::new(upgraded),
                    |_| {},
                    std::future::pending::<u32>(),
                )
                .await;
                eprintln!("websocket ended: {end:?}");
            }
            Err(error) => eprintln!("websocket upgrade failed: {error}"),
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
