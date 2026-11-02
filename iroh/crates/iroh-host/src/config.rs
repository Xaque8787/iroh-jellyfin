//! Command line and environment configuration. Every flag has an `IROH_*` environment equivalent.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use iroh::RelayUrl;
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
    /// Iroh relay to use. When omitted, Iroh's default public relays are used.
    #[arg(long, env = "IROH_RELAY_URL")]
    pub relay_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub data_dir: PathBuf,
    pub origin: Origin,
    pub policy: Option<TransportPolicy>,
    pub relay_url: Option<RelayUrl>,
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
        let relay_url = match self
            .relay_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
        {
            None => None,
            Some(url) => match url.parse::<RelayUrl>() {
                Ok(url) => Some(url),
                Err(error) => {
                    problems.push(format!("IROH_RELAY_URL: {error}"));
                    None
                }
            },
        };
        match origin {
            Some(origin) if problems.is_empty() => Ok(HostConfig {
                data_dir: self.data.data_dir.clone(),
                origin,
                policy,
                relay_url,
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
        assert!(config.relay_url.is_some());
        assert!(config.state_path().ends_with("state.json"));
    }

    #[test]
    fn empty_relay_url_means_default_relays() {
        let config = args("http://127.0.0.1:8096", None, Some(""))
            .validate()
            .unwrap();
        assert!(config.relay_url.is_none());
        assert!(config.policy.is_none());
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
