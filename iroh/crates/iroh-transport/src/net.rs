//! Iroh endpoint construction and connection path diagnostics.

use std::time::Duration;

use iroh::endpoint::{BindError, Connection, RelayMode, presets};
use iroh::{Endpoint, RelayUrl, SecretKey};
use serde::Serialize;

use crate::policy::PathKind;

/// Which relay servers an endpoint uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayChoice {
    /// The public n0 relays, with n0 DNS address lookup.
    Default,
    /// Only this relay, with n0 DNS address lookup.
    Custom(RelayUrl),
    /// No relays and no address lookup. Peers must be reachable on a direct IP address.
    Disabled,
}

#[derive(Debug, Clone, Default)]
pub struct EndpointOptions {
    pub relay: Option<RelayChoice>,
    /// Removes the IP transports so every path must go through a relay.
    pub relay_transport_only: bool,
}

pub async fn bind_endpoint(
    secret_key: SecretKey,
    alpns: Vec<Vec<u8>>,
    options: EndpointOptions,
) -> Result<Endpoint, BindError> {
    let relay = options.relay.unwrap_or(RelayChoice::Default);
    let mut builder = match &relay {
        RelayChoice::Disabled => {
            Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled)
        }
        RelayChoice::Default => Endpoint::builder(presets::N0),
        RelayChoice::Custom(url) => {
            Endpoint::builder(presets::N0).relay_mode(RelayMode::custom([url.clone()]))
        }
    };
    builder = builder.secret_key(secret_key).alpns(alpns);
    if options.relay_transport_only {
        builder = builder.clear_ip_transports();
    }
    builder.bind().await
}

/// What Iroh currently reports about a connection's network paths.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PathReport {
    pub kind: PathKind,
    pub rtt_ms: Option<f64>,
    pub remote_addr: Option<String>,
    pub open_paths: usize,
}

/// Reads the selected path from Iroh's path list. `Unknown` when no path is marked selected.
pub fn current_path(connection: &Connection) -> PathReport {
    let paths = connection.paths();
    let mut report = PathReport {
        kind: PathKind::Unknown,
        rtt_ms: None,
        remote_addr: None,
        open_paths: paths.len(),
    };
    for path in &paths {
        if !path.is_selected() {
            continue;
        }
        report.kind = if path.is_relay() {
            PathKind::Relay
        } else if path.is_ip() {
            PathKind::Direct
        } else {
            PathKind::Unknown
        };
        report.rtt_ms = Some(duration_ms(path.rtt()));
        report.remote_addr = Some(format!("{:?}", path.remote_addr()));
        break;
    }
    report
}

fn duration_ms(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 1000.0 * 10.0).round() / 10.0
}
