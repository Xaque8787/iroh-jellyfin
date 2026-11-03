//! Command line and environment configuration. Every flag has an `IROH_*` environment equivalent.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use iroh::RelayUrl;
use iroh_transport::net::RelayChoice;
use iroh_transport::policy::TransportPolicy;
use iroh_transport::proxy::Origin;

use crate::identity::IDENTITY_FILE;
use crate::state::STATE_FILE;

#[derive(Debug, Parser)]
#[command(
    name = "iroh-host",
    version,
    about = "Standalone Iroh host for a local HTTP media server"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print this host's endpoint id, creating the identity on first run.
    Identity(DataArgs),
    /// Check configuration, data directory, identity, state, and origin reachability.
    Doctor(RunArgs),
    /// Run the host: accept paired devices and forward their requests to the origin.
    Serve(RunArgs),
    /// Create a one-time invitation link for a new device.
    Invite(InviteArgs),
    /// List paired devices.
    Devices(DataArgs),
    /// Revoke a paired device. Its open sessions end within a few seconds.
    Revoke(RevokeArgs),
}

#[derive(Debug, Clone, Args)]
pub struct InviteArgs {
    #[command(flatten)]
    pub data: DataArgs,
    /// Name shown in the device list, for example "Living room TV".
    #[arg(long, default_value = "New device")]
    pub label: String,
    /// How long the link stays valid, in seconds (60 to 86400).
    #[arg(long)]
    pub ttl_secs: Option<u64>,
    /// Relay to put in the link when the running host's address is not known. Same values as serve.
    #[arg(long, env = "IROH_RELAY_URL")]
    pub relay_url: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct RevokeArgs {
    #[command(flatten)]
    pub data: DataArgs,
    /// Endpoint id of the device, as shown by `devices`.
    pub endpoint_id: String,
}

#[derive(Debug, Clone, Args)]
pub struct DataArgs {
    /// Directory holding identity.key, state.json, and host.lock.
    #[arg(long, env = "IROH_DATA_DIR", default_value = "./iroh-data")]
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub data: DataArgs,
    /// Local HTTP service to forward requests to. Plain http:// only.
    #[arg(long, env = "IROH_ORIGIN", default_value = "http://127.0.0.1:8096")]
    pub origin: String,
    /// auto, relay_only, or p2p_only. When omitted, the policy stored in state.json is used.
    #[arg(long, env = "IROH_TRANSPORT_POLICY")]
    pub policy: Option<String>,
    /// Iroh relay URL. Empty uses Iroh's default public relays; `none` disables relays entirely.
    #[arg(long, env = "IROH_RELAY_URL")]
    pub relay_url: Option<String>,
}

/// Parses the IROH_RELAY_URL convention shared by the host and the test client.
pub fn parse_relay(value: Option<&str>) -> Result<RelayChoice, String> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(RelayChoice::Default),
        Some("none") => Ok(RelayChoice::Disabled),
        Some(url) => url
            .parse::<RelayUrl>()
            .map(RelayChoice::Custom)
            .map_err(|error| format!("IROH_RELAY_URL: {error}")),
    }
}

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub data_dir: PathBuf,
    pub origin: Origin,
    pub policy: Option<TransportPolicy>,
    pub relay: RelayChoice,
}

impl HostConfig {
    pub fn identity_path(&self) -> PathBuf {
        self.data_dir.join(IDENTITY_FILE)
    }

    pub fn state_path(&self) -> PathBuf {
        self.data_dir.join(STATE_FILE)
    }
}

impl RunArgs {
    /// Validates every setting, reporting all problems at once.
    pub fn validate(&self) -> Result<HostConfig, Vec<String>> {
        let mut problems = Vec::new();
        let origin = Origin::parse(&self.origin)
            .map_err(|error| problems.push(format!("IROH_ORIGIN: {error}")))
            .ok();
        let policy = match self.policy.as_deref().map(str::parse::<TransportPolicy>) {
            None => None,
            Some(Ok(policy)) => Some(policy),
            Some(Err(error)) => {
                problems.push(format!("IROH_TRANSPORT_POLICY: {error}"));
                None
            }
        };
        let relay = parse_relay(self.relay_url.as_deref())
            .map_err(|error| problems.push(error))
            .unwrap_or(RelayChoice::Default);
        match origin {
            Some(origin) if problems.is_empty() => Ok(HostConfig {
                data_dir: self.data.data_dir.clone(),
                origin,
                policy,
                relay,
            }),
            _ => Err(problems),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(origin: &str, policy: Option<&str>, relay: Option<&str>) -> RunArgs {
        RunArgs {
            data: DataArgs {
                data_dir: "./data".into(),
            },
            origin: origin.into(),
            policy: policy.map(Into::into),
            relay_url: relay.map(Into::into),
        }
    }

    #[test]
    fn accepts_valid_configuration() {
        let config = args(
            "http://jellyfin:8096",
            Some("p2p_only"),
            Some("https://relay.example.com"),
        )
        .validate()
        .unwrap();
        assert_eq!(config.policy, Some(TransportPolicy::P2pOnly));
        assert!(matches!(config.relay, RelayChoice::Custom(_)));
        assert!(config.state_path().ends_with("state.json"));
    }

    #[test]
    fn empty_relay_url_means_default_relays() {
        let config = args("http://127.0.0.1:8096", None, Some(""))
            .validate()
            .unwrap();
        assert_eq!(config.relay, RelayChoice::Default);
        assert!(config.policy.is_none());
    }

    #[test]
    fn none_disables_relays() {
        assert_eq!(parse_relay(Some(" none ")), Ok(RelayChoice::Disabled));
        assert_eq!(parse_relay(None), Ok(RelayChoice::Default));
    }

    #[test]
    fn reports_every_problem() {
        let problems = args("https://x", Some("direct"), Some("not a url"))
            .validate()
            .unwrap_err();
        assert_eq!(problems.len(), 3, "{problems:?}");
    }

    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
