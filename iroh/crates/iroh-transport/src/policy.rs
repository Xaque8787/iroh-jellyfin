//! Transport policies and how strictly each one is enforced.

use serde::{Deserialize, Serialize};

/// The network path currently selected by Iroh for a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    Direct,
    Relay,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportPolicy {
    /// Iroh decides. Direct paths are preferred and relay fallback is allowed.
    #[default]
    Auto,
    /// Requests are served only while the selected path is a relay path. Best-effort until verified.
    RelayOnly,
    /// Requests are served only while the selected path is direct. Best-effort until verified.
    P2pOnly,
}

impl TransportPolicy {
    /// Whether HTTP traffic may be forwarded while the selected path is `path`.
    pub fn admits(self, path: PathKind) -> bool {
        match self {
            Self::Auto => true,
            Self::RelayOnly => path == PathKind::Relay,
            Self::P2pOnly => path == PathKind::Direct,
        }
    }

    /// How the policy is enforced by this build. Never report a policy as strict until it is.
    ///
    /// `best_effort_path_gate`: requests are refused while Iroh reports a disallowed selected path,
    /// but Iroh itself may still choose or switch to that path. See docs/transport-policies.md.
    pub fn enforcement(self) -> &'static str {
        match self {
            Self::Auto => "not_restricted",
            Self::RelayOnly | Self::P2pOnly => "best_effort_path_gate",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::RelayOnly => "relay_only",
            Self::P2pOnly => "p2p_only",
        }
    }
}

impl std::str::FromStr for TransportPolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "relay_only" => Ok(Self::RelayOnly),
            "p2p_only" => Ok(Self::P2pOnly),
            other => Err(format!(
                "unknown transport policy '{other}'; expected auto, relay_only, or p2p_only"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_admits_everything() {
        for path in [PathKind::Direct, PathKind::Relay, PathKind::Unknown] {
            assert!(TransportPolicy::Auto.admits(path));
        }
    }

    #[test]
    fn relay_only_admits_only_relay_paths() {
        assert!(TransportPolicy::RelayOnly.admits(PathKind::Relay));
        assert!(!TransportPolicy::RelayOnly.admits(PathKind::Direct));
        assert!(!TransportPolicy::RelayOnly.admits(PathKind::Unknown));
    }

    #[test]
    fn p2p_only_refuses_relay_and_unknown_paths() {
        assert!(TransportPolicy::P2pOnly.admits(PathKind::Direct));
        assert!(!TransportPolicy::P2pOnly.admits(PathKind::Relay));
        assert!(!TransportPolicy::P2pOnly.admits(PathKind::Unknown));
    }

    #[test]
    fn parses_and_names_policies() {
        for policy in [
            TransportPolicy::Auto,
            TransportPolicy::RelayOnly,
            TransportPolicy::P2pOnly,
        ] {
            assert_eq!(policy.as_str().parse::<TransportPolicy>().unwrap(), policy);
        }
        assert!("direct".parse::<TransportPolicy>().is_err());
    }

    #[test]
    fn restricted_policies_are_labelled_best_effort() {
        assert_eq!(TransportPolicy::Auto.enforcement(), "not_restricted");
        assert_eq!(
            TransportPolicy::RelayOnly.enforcement(),
            "best_effort_path_gate"
        );
        assert_eq!(
            TransportPolicy::P2pOnly.enforcement(),
            "best_effort_path_gate"
        );
    }

    #[test]
    fn serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&TransportPolicy::P2pOnly).unwrap(),
            "\"p2p_only\""
        );
    }
}
