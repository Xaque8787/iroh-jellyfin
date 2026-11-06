//! Command line and environment configuration. Every flag has an `IROH_*` environment equivalent.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use iroh::RelayUrl;
use iroh_transport::net::RelayChoice;
use iroh_transport::policy::TransportPolicy;
use iroh_transport::proxy::Origin;
use iroh_transport::websocket::{OriginHeader, parse_switch};

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
    /// Control API listen address (see protocol/control-v1.md), or `off`.
    #[arg(long, env = "IROH_CONTROL_LISTEN", default_value = DEFAULT_CONTROL_LISTEN)]
    pub control_listen: String,
    /// Allow the control API to listen on a non-loopback address.
    #[arg(long, env = "IROH_CONTROL_ALLOW_REMOTE")]
    pub control_allow_remote: bool,
    /// Control API token. When unset, the token in <data-dir>/control.token is used or created.
    #[arg(long, env = "IROH_CONTROL_TOKEN", hide_env_values = true)]
    pub control_token: Option<String>,
    /// Forward WebSocket connections: on or off. When omitted, the setting in state.json is used.
    #[arg(long, env = "IROH_WEBSOCKETS")]
    pub websockets: Option<String>,
    /// Origin header sent to the origin on WebSocket upgrades: rewrite or pass_through.
    /// When omitted, the setting in state.json is used.
    #[arg(long, env = "IROH_WEBSOCKET_ORIGIN")]
    pub websocket_origin: Option<String>,
}

pub const DEFAULT_CONTROL_LISTEN: &str = "127.0.0.1:47950";
const MIN_CONTROL_TOKEN_CHARS: usize = 32;

#[derive(Clone)]
pub struct ControlConfig {
    pub listen: SocketAddr,
    pub token: Option<String>,
}

impl std::fmt::Debug for ControlConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlConfig")
            .field("listen", &self.listen)
            .field("token", &self.token.as_ref().map(|_| "<set>"))
            .finish()
    }
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
    pub control: Option<ControlConfig>,
    pub websockets_enabled: Option<bool>,
    pub websocket_origin: Option<OriginHeader>,
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
        let control = self
            .control_config()
            .map_err(|error| problems.push(error))
            .ok()
            .flatten();
        let websockets_enabled = optional(&self.websockets)
            .map(parse_switch)
            .transpose()
            .map_err(|error| problems.push(format!("IROH_WEBSOCKETS: {error}")))
            .ok()
            .flatten();
        let websocket_origin = optional(&self.websocket_origin)
            .map(str::parse::<OriginHeader>)
            .transpose()
            .map_err(|error| problems.push(format!("IROH_WEBSOCKET_ORIGIN: {error}")))
            .ok()
            .flatten();
        match origin {
            Some(origin) if problems.is_empty() => Ok(HostConfig {
                data_dir: self.data.data_dir.clone(),
                origin,
                policy,
                relay,
                control,
                websockets_enabled,
                websocket_origin,
            }),
            _ => Err(problems),
        }
    }

    fn control_config(&self) -> Result<Option<ControlConfig>, String> {
        let listen = self.control_listen.trim();
        if listen.eq_ignore_ascii_case("off") {
            return Ok(None);
        }
        let listen: SocketAddr = listen.parse().map_err(|_| {
            format!("IROH_CONTROL_LISTEN: '{listen}' is not an ip:port address or 'off'")
        })?;
        if !listen.ip().is_loopback() && !self.control_allow_remote {
            return Err(format!(
                "IROH_CONTROL_LISTEN: {listen} is not a loopback address; set IROH_CONTROL_ALLOW_REMOTE=true to allow it"
            ));
        }
        let token = match self.control_token.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(token) if token.chars().count() < MIN_CONTROL_TOKEN_CHARS => {
                return Err(format!(
                    "IROH_CONTROL_TOKEN must be at least {MIN_CONTROL_TOKEN_CHARS} characters"
                ));
            }
            Some(token) => Some(token.to_owned()),
        };
        Ok(Some(ControlConfig { listen, token }))
    }
}

/// Empty values count as unset, so a container can pass the variable through without a value.
fn optional(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
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
            control_listen: DEFAULT_CONTROL_LISTEN.into(),
            control_allow_remote: false,
            control_token: None,
            websockets: None,
            websocket_origin: None,
        }
    }

    #[test]
    fn websocket_settings_are_optional_and_validated() {
        let mut run = args("http://127.0.0.1:8096", None, None);
        let config = run.validate().unwrap();
        assert_eq!(config.websockets_enabled, None);
        assert_eq!(config.websocket_origin, None);

        run.websockets = Some("".into());
        run.websocket_origin = Some(" ".into());
        assert_eq!(run.validate().unwrap().websockets_enabled, None);

        run.websockets = Some("OFF".into());
        run.websocket_origin = Some("pass_through".into());
        let config = run.validate().unwrap();
        assert_eq!(config.websockets_enabled, Some(false));
        assert_eq!(config.websocket_origin, Some(OriginHeader::PassThrough));

        run.websockets = Some("maybe".into());
        run.websocket_origin = Some("keep".into());
        assert_eq!(run.validate().unwrap_err().len(), 2);
    }

    #[test]
    fn control_api_is_loopback_only_unless_allowed() {
        let mut run = args("http://127.0.0.1:8096", None, None);
        let control = run.validate().unwrap().control.unwrap();
        assert_eq!(control.listen.to_string(), DEFAULT_CONTROL_LISTEN);
        run.control_listen = "0.0.0.0:47950".into();
        assert!(run.validate().is_err());
        run.control_allow_remote = true;
        assert!(run.validate().is_ok());
        run.control_listen = "OFF".into();
        assert!(run.validate().unwrap().control.is_none());
    }

    #[test]
    fn short_control_tokens_are_rejected() {
        let mut run = args("http://127.0.0.1:8096", None, None);
        run.control_token = Some("too-short".into());
        assert!(run.validate().is_err());
        run.control_token = Some("x".repeat(32));
        let control = run.validate().unwrap().control.unwrap();
        assert_eq!(control.token.as_deref(), Some("x".repeat(32).as_str()));
        assert!(!format!("{control:?}").contains("xxxx"));
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
