//! The long-running host: binds the Iroh endpoint and serves paired devices until a stop signal.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iroh::{Endpoint, EndpointAddr, SecretKey};
use iroh_transport::host::{HostService, serve};
use iroh_transport::net::{EndpointOptions, RelayChoice, bind_endpoint};
use iroh_transport::policy::TransportPolicy;
use iroh_transport::protocol::ALPN;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::{ControlConfig, HostConfig};
use crate::control::{ControlContext, TOKEN_FILE, load_or_create_token, serve_control};
use crate::fsutil::write_private_atomic;
use crate::registry::StateRegistry;
use crate::state::HostState;

/// The running host's current Iroh address, so `invite` can build links with relay and direct addresses.
pub const ADDR_FILE: &str = "endpoint-addr.json";
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(2);
const ONLINE_WAIT: Duration = Duration::from_secs(20);

pub async fn run(
    config: HostConfig,
    secret_key: SecretKey,
    state: Arc<HostState>,
) -> Result<(), String> {
    let policy = state.policy();
    if policy == TransportPolicy::RelayOnly && config.relay == RelayChoice::Disabled {
        return Err("relay_only needs a relay; IROH_RELAY_URL=none disables relays".into());
    }
    let options = EndpointOptions {
        relay: Some(config.relay.clone()),
        relay_transport_only: policy == TransportPolicy::RelayOnly,
    };
    let endpoint = bind_endpoint(secret_key, vec![ALPN.to_vec()], options)
        .await
        .map_err(|error| format!("could not start the Iroh endpoint: {error}"))?;
    info!(
        endpoint_id = %endpoint.id(),
        origin = %config.origin.display(),
        policy = policy.as_str(),
        enforcement = policy.enforcement(),
        relay = ?config.relay,
        "host started"
    );
    if policy == TransportPolicy::RelayOnly {
        info!(
            "relay_only: IP transports are disabled on this endpoint, so Iroh can only use relay paths; changing away from relay_only requires a restart"
        );
    }

    let shutdown = CancellationToken::new();
    if config.relay != RelayChoice::Disabled {
        tokio::spawn(report_online(endpoint.clone()));
    }
    tokio::spawn(maintain(
        endpoint.clone(),
        state.clone(),
        config.data_dir.join(ADDR_FILE),
        shutdown.clone(),
    ));
    let service = HostService::new(
        config.origin.clone(),
        Arc::new(StateRegistry::new(state.clone())),
    );
    if let Some(control) = &config.control {
        let context = ControlContext::new(
            state,
            service.sessions(),
            Box::new({
                let endpoint = endpoint.clone();
                move || endpoint.addr()
            }),
            config.origin.display(),
            config.relay.clone(),
            policy == TransportPolicy::RelayOnly,
            &load_or_create_token(&config.data_dir, control.token.as_deref())?,
        );
        start_control(control, &config.data_dir, context, shutdown.clone()).await?;
    }
    tokio::spawn(stop_on_signal(shutdown.clone()));

    serve(endpoint, service, shutdown).await;
    info!("host stopped");
    Ok(())
}

async fn start_control(
    control: &ControlConfig,
    data_dir: &Path,
    context: ControlContext,
    shutdown: CancellationToken,
) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind(control.listen)
        .await
        .map_err(|error| {
            format!(
                "could not listen for the control API on {}: {error}",
                control.listen
            )
        })?;
    let token_source = if control.token.is_some() {
        "IROH_CONTROL_TOKEN".to_owned()
    } else {
        data_dir.join(TOKEN_FILE).display().to_string()
    };
    info!(listen = %control.listen, token = %token_source, "control API v1 listening");
    tokio::spawn(serve_control(listener, Arc::new(context), shutdown));
    Ok(())
}

async fn report_online(endpoint: Endpoint) {
    match tokio::time::timeout(ONLINE_WAIT, endpoint.online()).await {
        Ok(_) => info!(addr = ?endpoint.addr(), "connected to relay"),
        Err(_) => warn!(
            "no relay connection after {}s; only direct connections will work until it succeeds",
            ONLINE_WAIT.as_secs()
        ),
    }
}

/// Reloads state written by admin commands and publishes the endpoint address for `invite`.
async fn maintain(
    endpoint: Endpoint,
    state: Arc<HostState>,
    addr_path: PathBuf,
    shutdown: CancellationToken,
) {
    let mut interval = tokio::time::interval(MAINTENANCE_INTERVAL);
    let mut published: Option<EndpointAddr> = None;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = interval.tick() => {}
        }
        let reload_state = state.clone();
        match tokio::task::spawn_blocking(move || reload_state.reload()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!(%error, "could not reload host state; keeping the last good copy")
            }
            Err(_) => warn!("state reload task failed"),
        }
        let addr = endpoint.addr();
        if published.as_ref() != Some(&addr) {
            match publish_addr(&addr_path, &addr) {
                Ok(()) => {
                    debug!(?addr, "published endpoint address");
                    published = Some(addr);
                }
                Err(error) => warn!(%error, "could not write {}", addr_path.display()),
            }
        }
    }
}

fn publish_addr(path: &Path, addr: &EndpointAddr) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(addr).map_err(std::io::Error::other)?;
    write_private_atomic(path, &bytes)
}

/// Reads the address last published by a running host, if it belongs to `secret_key`'s identity.
pub fn published_addr(data_dir: &Path, secret_key: &SecretKey) -> Option<EndpointAddr> {
    let bytes = std::fs::read(data_dir.join(ADDR_FILE)).ok()?;
    let addr: EndpointAddr = serde_json::from_slice(&bytes).ok()?;
    (addr.id == secret_key.public()).then_some(addr)
}

async fn stop_on_signal(shutdown: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                warn!(%error, "cannot listen for SIGTERM; only Ctrl-C will stop the host");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    info!("stop requested, closing sessions");
    shutdown.cancel();
}
