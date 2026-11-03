//! Pairing invitations: token generation, hashing at rest, expiry, one-time redemption, and failure limiting.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const TOKEN_BYTES: usize = 32;
pub const DEFAULT_INVITATION_TTL_SECS: u64 = 15 * 60;
pub const FAILURE_WINDOW_SECS: u64 = 10 * 60;
pub const MAX_FAILURES_PER_WINDOW: usize = 10;

/// Generates a fresh invitation token from the operating system's random source.
pub fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// SHA-256 digest of a token, hex encoded. Only this digest is stored.
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn digests_match(expected_hex: &str, presented_token: &str) -> bool {
    let presented = hash_token(presented_token);
    expected_hex.as_bytes().ct_eq(presented.as_bytes()).into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invitation {
    pub id: String,
    pub label: String,
    pub token_hash: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub redeemed_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedeemError {
    Invalid,
    Expired,
    AlreadyUsed,
    RateLimited,
}

impl Invitation {
    pub fn new(id: String, label: String, token: &str, now: u64, ttl_secs: u64) -> Self {
        Self {
            id,
            label,
            token_hash: hash_token(token),
            created_at: now,
            expires_at: now.saturating_add(ttl_secs),
            redeemed_by: None,
        }
    }

    pub fn is_redeemed(&self) -> bool {
        self.redeemed_by.is_some()
    }

    pub fn matches(&self, presented_token: &str) -> bool {
        digests_match(&self.token_hash, presented_token)
    }
}

/// Sliding-window limiter for failed pairing attempts across all callers.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FailureLimiter {
    failures: Vec<u64>,
}

impl FailureLimiter {
    pub fn is_limited(&mut self, now: u64) -> bool {
        self.prune(now);
        self.failures.len() >= MAX_FAILURES_PER_WINDOW
    }

    pub fn record_failure(&mut self, now: u64) {
        self.prune(now);
        self.failures.push(now);
    }

    fn prune(&mut self, now: u64) {
        self.failures
            .retain(|&at| now.saturating_sub(at) < FAILURE_WINDOW_SECS);
    }
}

/// Checks a presented token against stored invitations and marks the match as redeemed.
///
/// On success returns the index of the redeemed invitation. Each invitation can be redeemed once.
pub fn redeem(
    invitations: &mut [Invitation],
    limiter: &mut FailureLimiter,
    presented_token: &str,
    device_name: &str,
    now: u64,
) -> Result<usize, RedeemError> {
    if limiter.is_limited(now) {
        return Err(RedeemError::RateLimited);
    }
    let Some(index) = invitations
        .iter()
        .position(|inv| inv.matches(presented_token))
    else {
        limiter.record_failure(now);
        return Err(RedeemError::Invalid);
    };
    let invitation = &mut invitations[index];
    if invitation.is_redeemed() {
        limiter.record_failure(now);
        return Err(RedeemError::AlreadyUsed);
    }
    if now >= invitation.expires_at {
        return Err(RedeemError::Expired);
    }
    invitation.redeemed_by = Some(device_name.to_owned());
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invitation(token: &str, now: u64) -> Invitation {
        Invitation::new("inv-1".into(), "Living room TV".into(), token, now, 60)
    }

    #[test]
    fn tokens_are_unique_and_url_safe() {
        let first = generate_token();
        let second = generate_token();
        assert_ne!(first, second);
        assert_eq!(first.len(), 43);
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn stores_only_a_digest() {
        let token = "secret-token";
        let inv = invitation(token, 0);
        assert_ne!(inv.token_hash, token);
        assert_eq!(inv.token_hash.len(), 64);
    }

    #[test]
    fn redeems_once() {
        let mut invitations = vec![invitation("abc", 100)];
        let mut limiter = FailureLimiter::default();
        assert_eq!(
            redeem(&mut invitations, &mut limiter, "abc", "TV", 110),
            Ok(0)
        );
        assert_eq!(
            redeem(&mut invitations, &mut limiter, "abc", "TV", 111),
            Err(RedeemError::AlreadyUsed)
        );
    }

    #[test]
    fn rejects_expired_invitations() {
        let mut invitations = vec![invitation("abc", 100)];
        let mut limiter = FailureLimiter::default();
        assert_eq!(
            redeem(&mut invitations, &mut limiter, "abc", "TV", 160),
            Err(RedeemError::Expired)
        );
        assert!(!invitations[0].is_redeemed());
    }

    #[test]
    fn rejects_unknown_tokens() {
        let mut invitations = vec![invitation("abc", 0)];
        let mut limiter = FailureLimiter::default();
        assert_eq!(
            redeem(&mut invitations, &mut limiter, "wrong", "TV", 1),
            Err(RedeemError::Invalid)
        );
    }

    #[test]
    fn limits_repeated_failures_then_recovers_after_window() {
        let ttl = FAILURE_WINDOW_SECS * 2;
        let mut invitations = vec![Invitation::new("inv-1".into(), "TV".into(), "abc", 0, ttl)];
        let mut limiter = FailureLimiter::default();
        for attempt in 0..MAX_FAILURES_PER_WINDOW as u64 {
            assert_eq!(
                redeem(&mut invitations, &mut limiter, "nope", "TV", attempt),
                Err(RedeemError::Invalid)
            );
        }
        assert_eq!(
            redeem(&mut invitations, &mut limiter, "abc", "TV", 20),
            Err(RedeemError::RateLimited)
        );
        assert_eq!(
            redeem(
                &mut invitations,
                &mut limiter,
                "abc",
                "TV",
                FAILURE_WINDOW_SECS + 20
            ),
            Ok(0)
        );
    }
}
