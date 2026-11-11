//! Platform-independent client engine. No terminal signals or process-global configuration.
use iroh::{Endpoint, EndpointAddr};
use iroh_transport::{
    client::{ClientError, ClientSession},
    invite::InviteLink,
    net::{EndpointOptions, RelayChoice, bind_endpoint},
};
use serde::Serialize;
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Mutex;

pub mod fsutil;
pub mod identity;
mod proxy;

#[derive(Clone)]
pub struct ClientConfig {
    pub data_dir: PathBuf,
    pub name: String,
    pub relay: RelayChoice,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid invitation")]
    InvalidInvitation,
    #[error("host rejected authorization ({0})")]
    Denied(String),
    #[error("no paired host")]
    NotPaired,
    #[error("client is shut down")]
    Closed,
    #[error("invalid configuration: {0}")]
    Configuration(String),
    #[error("storage failure: {0}")]
    Storage(String),
    #[error("transport failure: {0}")]
    Transport(String),
}
impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidInvitation => "invalid_invitation",
            Self::Denied(_) => "authorization_denied",
            Self::NotPaired => "not_paired",
            Self::Closed => "closed",
            Self::Configuration(_) => "configuration",
            Self::Storage(_) => "storage",
            Self::Transport(_) => "transport",
        }
    }
}
impl From<ClientError> for Error {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Denied(code) => Self::Denied(code),
            other => Self::Transport(other.to_string()),
        }
    }
}
fn storage(e: impl std::fmt::Display) -> Error {
    Error::Storage(e.to_string())
}

#[derive(Clone, Debug, Serialize)]
pub struct ProxyConnection {
    pub proxy_url: String,
}
#[derive(Debug)]
pub struct PairingResult {
    pub outcome: iroh_transport::protocol::AuthOutcome,
    pub host_id: String,
    pub transport_policy: String,
}
#[derive(Debug, Serialize)]
pub struct Status {
    pub state: String,
    pub authorization: String,
    pub path: String,
    pub rtt_ms: Option<f64>,
    pub transport_policy: Option<String>,
    pub policy_enforcement: Option<String>,
    pub proxy_url: Option<String>,
    pub last_error: Option<String>,
}
struct Inner {
    proxy: Option<proxy::Proxy>,
    closed: bool,
    last_error: Option<String>,
    denied: bool,
    directory_lock: Option<fsutil::DataDirLock>,
}
/// One owner per data directory. Concurrent operations are serialized. Dropping this object
/// aborts its listener; call shutdown to close sessions and await task termination explicitly.
pub struct Client {
    config: ClientConfig,
    endpoint: Endpoint,
    inner: Mutex<Inner>,
}
impl Client {
    pub async fn new(config: ClientConfig) -> Result<Self, Error> {
        if config.name.is_empty() || config.name.len() > 128 {
            return Err(Error::Configuration(
                "device name must be 1..128 bytes".into(),
            ));
        }
        fsutil::ensure_private_dir(&config.data_dir).map_err(storage)?;
        let lock = fsutil::DataDirLock::acquire(&config.data_dir).map_err(storage)?;
        let key = identity::load_or_create(&config.data_dir.join("client.key")).map_err(storage)?;
        let endpoint = bind_endpoint(
            key,
            vec![],
            EndpointOptions {
                relay: Some(config.relay.clone()),
                relay_transport_only: false,
            },
        )
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(Self {
            config,
            endpoint,
            inner: Mutex::new(Inner {
                proxy: None,
                closed: false,
                last_error: None,
                denied: false,
                directory_lock: Some(lock),
            }),
        })
    }
    pub async fn pair(&self, link: &str) -> Result<PairingResult, Error> {
        let mut inner = self.inner.lock().await;
        if inner.closed {
            return Err(Error::Closed);
        }
        let invite = InviteLink::decode(link).map_err(|_| Error::InvalidInvitation)?;
        if let Some(mut proxy) = inner.proxy.take() {
            proxy.stop().await;
        }
        let session = tokio::time::timeout(
            Duration::from_secs(30),
            ClientSession::connect(
                &self.endpoint,
                invite.addr.clone(),
                &self.config.name,
                Some(&invite.token),
            ),
        )
        .await
        .map_err(|_| Error::Transport("pairing timed out".into()))??;
        let result = serde_json::to_vec_pretty(&invite.addr)
            .map_err(storage)
            .and_then(|bytes| {
                fsutil::write_private_atomic(&self.config.data_dir.join("host.json"), &bytes)
                    .map_err(storage)
            });
        let paired = PairingResult {
            outcome: session.outcome(),
            host_id: invite.addr.id.to_string(),
            transport_policy: session.transport_policy().as_str().into(),
        };
        session.close().await;
        result.map(|()| {
            inner.last_error = None;
            inner.denied = false;
            paired
        })
    }
    /// Obtain a separate session, primarily for CLI fetch/status. The caller closes it.
    pub async fn connect(&self) -> Result<ClientSession, Error> {
        let inner = self.inner.lock().await;
        if inner.closed {
            return Err(Error::Closed);
        }
        connect(&self.config, &self.endpoint).await
    }
    /// Loopback only. SDK callers use port zero and a capability-protected URL. CLI callers
    /// may opt out of URL protection to preserve their historical local URL behavior.
    pub async fn start_proxy(
        &self,
        listen: SocketAddr,
        protect_url: bool,
    ) -> Result<ProxyConnection, Error> {
        if !listen.ip().is_loopback() {
            return Err(Error::Configuration("proxy must bind loopback".into()));
        }
        let mut inner = self.inner.lock().await;
        if inner.closed {
            return Err(Error::Closed);
        }
        if let Some(proxy) = &inner.proxy {
            return Ok(proxy.connection.clone());
        }
        let proxy = proxy::Proxy::start(
            self.config.clone(),
            self.endpoint.clone(),
            listen,
            protect_url,
        )
        .await;
        let proxy = match proxy {
            Ok(proxy) => {
                inner.last_error = None;
                inner.denied = false;
                proxy
            }
            Err(error) => {
                inner.last_error = Some(error.to_string());
                inner.denied = matches!(error, Error::Denied(_));
                return Err(error);
            }
        };
        let connection = proxy.connection.clone();
        inner.proxy = Some(proxy);
        Ok(connection)
    }
    pub async fn stop_proxy(&self) -> Result<(), Error> {
        let mut inner = self.inner.lock().await;
        if let Some(mut proxy) = inner.proxy.take() {
            proxy.stop().await;
        }
        Ok(())
    }
    pub async fn forget_pairing(&self) -> Result<(), Error> {
        let mut inner = self.inner.lock().await;
        if inner.closed {
            return Err(Error::Closed);
        }
        if let Some(mut proxy) = inner.proxy.take() {
            proxy.stop().await;
        }
        inner.last_error = None;
        inner.denied = false;
        match std::fs::remove_file(self.config.data_dir.join("host.json")) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(storage(e)),
        }
    }
    pub async fn notify_network_change(&self) -> Result<(), Error> {
        let inner = self.inner.lock().await;
        if inner.closed {
            return Err(Error::Closed);
        }
        self.endpoint.network_change().await;
        Ok(())
    }
    pub async fn status(&self) -> Status {
        let inner = self.inner.lock().await;
        let mut status = Status {
            state: if inner.closed { "closed" } else { "idle" }.into(),
            authorization: if inner.denied { "denied" } else { "unknown" }.into(),
            path: "unknown".into(),
            rtt_ms: None,
            transport_policy: None,
            policy_enforcement: None,
            proxy_url: None,
            last_error: inner.last_error.clone(),
        };
        if let Some(proxy) = &inner.proxy {
            status.proxy_url = Some(proxy.connection.proxy_url.clone());
            let slot = proxy.state.session.lock().await;
            status.last_error = proxy.state.last_error.lock().await.clone();
            if let Some(session) = slot.as_ref() {
                let connected = session.connection().close_reason().is_none();
                status.state = if connected {
                    "connected"
                } else {
                    "disconnected"
                }
                .into();
                status.authorization = match session.connection().close_reason() {
                    None => "authorized",
                    Some(iroh::endpoint::ConnectionError::ApplicationClosed(close))
                        if close.error_code.into_inner()
                            == u64::from(iroh_transport::protocol::close_code::REVOKED) =>
                    {
                        "revoked"
                    }
                    Some(iroh::endpoint::ConnectionError::ApplicationClosed(close))
                        if close.error_code.into_inner()
                            == u64::from(iroh_transport::protocol::close_code::NOT_AUTHORIZED) =>
                    {
                        "denied"
                    }
                    _ => "unknown",
                }
                .into();
                let path = session.path();
                status.path = format!("{:?}", path.kind).to_lowercase();
                status.rtt_ms = path.rtt_ms;
                status.transport_policy = Some(session.transport_policy().as_str().into());
                status.policy_enforcement = Some(session.transport_policy().enforcement().into());
                if !connected {
                    status.last_error = session.connection().close_reason().map(|e| e.to_string());
                }
            }
        }
        status
    }
    pub async fn shutdown(&self) {
        let mut inner = self.inner.lock().await;
        if let Some(mut proxy) = inner.proxy.take() {
            proxy.stop().await;
        }
        self.endpoint.close().await;
        inner.closed = true;
        inner.directory_lock.take();
    }
}
fn saved_host(config: &ClientConfig) -> Result<EndpointAddr, Error> {
    let bytes = std::fs::read(config.data_dir.join("host.json")).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::NotPaired
        } else {
            storage(e)
        }
    })?;
    serde_json::from_slice(&bytes).map_err(storage)
}
async fn connect(config: &ClientConfig, endpoint: &Endpoint) -> Result<ClientSession, Error> {
    tokio::time::timeout(
        Duration::from_secs(30),
        ClientSession::connect(endpoint, saved_host(config)?, &config.name, None),
    )
    .await
    .map_err(|_| Error::Transport("connection timed out".into()))?
    .map_err(Error::from)
}
