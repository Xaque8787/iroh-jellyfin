//! Invitation links: the host's Iroh address plus a one-time pairing token, in one copyable string.
//!
//! Format: `jellyfin-iroh://invite/<base64url(JSON)>` where the JSON is `{"v":1,"addr":..,"token":..}`.
//! No rendezvous service is involved; the link carries everything needed to dial the host.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

pub const LINK_PREFIX: &str = "jellyfin-iroh://invite/";
pub const LINK_VERSION: u32 = 1;
const MAX_LINK_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InviteLink {
    pub v: u32,
    pub addr: EndpointAddr,
    pub token: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InviteError {
    #[error("not a jellyfin-iroh invitation link")]
    NotALink,
    #[error("invitation link is too long")]
    TooLong,
    #[error("invitation link is damaged or incomplete")]
    Malformed,
    #[error("invitation link version {0} is not supported by this build")]
    UnsupportedVersion(u32),
}

impl InviteLink {
    pub fn new(addr: EndpointAddr, token: String) -> Self {
        Self {
            v: LINK_VERSION,
            addr,
            token,
        }
    }

    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("invite link is always serializable");
        format!("{LINK_PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    pub fn decode(link: &str) -> Result<Self, InviteError> {
        let link = link.trim();
        if link.len() > MAX_LINK_BYTES {
            return Err(InviteError::TooLong);
        }
        let payload = link
            .strip_prefix(LINK_PREFIX)
            .ok_or(InviteError::NotALink)?;
        let json = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| InviteError::Malformed)?;
        let version: VersionOnly =
            serde_json::from_slice(&json).map_err(|_| InviteError::Malformed)?;
        if version.v != LINK_VERSION {
            return Err(InviteError::UnsupportedVersion(version.v));
        }
        let parsed: Self = serde_json::from_slice(&json).map_err(|_| InviteError::Malformed)?;
        if parsed.token.is_empty() || parsed.token.len() > 256 {
            return Err(InviteError::Malformed);
        }
        Ok(parsed)
    }
}

#[derive(Deserialize)]
struct VersionOnly {
    v: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn sample() -> InviteLink {
        let id = SecretKey::generate().public();
        let addr = EndpointAddr::new(id)
            .with_ip_addr("192.0.2.10:4433".parse::<std::net::SocketAddr>().unwrap());
        InviteLink::new(addr, crate::pairing::generate_token())
    }

    #[test]
    fn round_trips() {
        let link = sample();
        let encoded = link.encode();
        assert!(encoded.starts_with(LINK_PREFIX));
        assert_eq!(InviteLink::decode(&encoded).unwrap(), link);
        assert_eq!(InviteLink::decode(&format!("  {encoded}\n")).unwrap(), link);
    }

    #[test]
    fn rejects_bad_links() {
        assert_eq!(
            InviteLink::decode("https://example.com"),
            Err(InviteError::NotALink)
        );
        assert_eq!(
            InviteLink::decode(&format!("{LINK_PREFIX}!!!")),
            Err(InviteError::Malformed)
        );
        let future = URL_SAFE_NO_PAD.encode(br#"{"v":2}"#);
        assert_eq!(
            InviteLink::decode(&format!("{LINK_PREFIX}{future}")),
            Err(InviteError::UnsupportedVersion(2))
        );
        let long = format!("{LINK_PREFIX}{}", "a".repeat(MAX_LINK_BYTES));
        assert_eq!(InviteLink::decode(&long), Err(InviteError::TooLong));
    }
}
