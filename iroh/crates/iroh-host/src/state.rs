//! Authorized devices, invitations, and policy, persisted as one JSON file in the data directory.
//!
//! Every mutation is written to disk (atomically, 0600) before it becomes visible in memory.
//! Mutations hold an exclusive lock on `state.lock` and start from the file on disk, so the running
//! host and admin commands (invite, revoke) can change state at the same time without losing writes.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use iroh_transport::pairing::{
    self, DEFAULT_INVITATION_TTL_SECS, FailureLimiter, Invitation, RedeemError,
};
use iroh_transport::policy::TransportPolicy;
use serde::{Deserialize, Serialize};

use crate::fsutil::{restrict_permissions, write_private_atomic};

pub const STATE_FILE: &str = "state.json";
const STATE_LOCK_FILE: &str = "state.lock";
pub const STATE_VERSION: u32 = 1;
pub const MIN_INVITATION_TTL_SECS: u64 = 60;
pub const MAX_INVITATION_TTL_SECS: u64 = 24 * 60 * 60;
const MAX_LABEL_CHARS: usize = 80;
const MAX_DEVICE_NAME_CHARS: usize = 64;
/// Spent or expired invitations are kept this long for the admin UI, then pruned.
const INVITATION_RETENTION_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// The device's Iroh endpoint id. This is the credential checked on every connection.
    pub endpoint_id: String,
    pub name: String,
    pub paired_at: u64,
    pub invitation_id: String,
    #[serde(default)]
    pub revoked_at: Option<u64>,
}

impl Device {
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFile {
    pub version: u32,
    #[serde(default)]
    pub policy: TransportPolicy,
    #[serde(default)]
    pub devices: Vec<Device>,
    #[serde(default)]
    pub invitations: Vec<Invitation>,
    #[serde(default)]
    pub pairing_failures: FailureLimiter,
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            policy: TransportPolicy::default(),
            devices: Vec::new(),
            invitations: Vec::new(),
            pairing_failures: FailureLimiter::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("could not access {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path} is not valid host state: {source}")]
    Corrupt {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{path} has state version {found}; this build supports up to {max}", max = STATE_VERSION)]
    UnsupportedVersion { path: PathBuf, found: u32 },
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairOutcome {
    AlreadyAuthorized,
    Paired,
}

pub struct HostState {
    path: PathBuf,
    inner: Mutex<StateFile>,
}

impl HostState {
    /// Opens or creates the state file. `configured_policy`, when set, replaces the stored policy.
    pub fn open(
        path: &Path,
        configured_policy: Option<TransportPolicy>,
    ) -> Result<Self, StateError> {
        let _lock = lock_state(path)?;
        let mut file = load(path)?;
        if let Some(policy) = configured_policy {
            file.policy = policy;
        }
        file.version = STATE_VERSION;
        persist(path, &file)?;
        Ok(Self {
            path: path.to_path_buf(),
            inner: Mutex::new(file),
        })
    }

    /// Picks up changes made by other processes. Readers never see a partial file.
    pub fn reload(&self) -> Result<(), StateError> {
        let file = load(&self.path)?;
        *self.lock() = file;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn snapshot(&self) -> StateFile {
        self.lock().clone()
    }

    pub fn policy(&self) -> TransportPolicy {
        self.lock().policy
    }

    pub fn set_policy(&self, policy: TransportPolicy) -> Result<(), StateError> {
        self.update(|file| file.policy = policy)
    }

    pub fn is_authorized(&self, endpoint_id: &str) -> bool {
        self.lock()
            .devices
            .iter()
            .any(|device| device.endpoint_id == endpoint_id && device.is_active())
    }

    /// Creates an invitation and returns it with the plaintext token, which is never stored.
    pub fn create_invitation(
        &self,
        label: &str,
        ttl_secs: Option<u64>,
        now: u64,
    ) -> Result<(Invitation, String), StateError> {
        let label = label.trim();
        if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS {
            return Err(StateError::Invalid(format!(
                "label must be 1 to {MAX_LABEL_CHARS} characters"
            )));
        }
        let ttl = ttl_secs.unwrap_or(DEFAULT_INVITATION_TTL_SECS);
        if !(MIN_INVITATION_TTL_SECS..=MAX_INVITATION_TTL_SECS).contains(&ttl) {
            return Err(StateError::Invalid(format!(
                "ttl_secs must be between {MIN_INVITATION_TTL_SECS} and {MAX_INVITATION_TTL_SECS}"
            )));
        }
        let token = pairing::generate_token();
        let id = format!("inv-{}", &pairing::hash_token(&token)[..12]);
        let invitation = Invitation::new(id, label.to_owned(), &token, now, ttl);
        let stored = invitation.clone();
        self.update(move |file| file.invitations.push(stored))?;
        Ok((invitation, token))
    }

    /// Authorizes `endpoint_id`, redeeming `token` if the device is not already authorized.
    ///
    /// Failed attempts are persisted so the rate limit survives restarts.
    pub fn pair(
        &self,
        endpoint_id: &str,
        device_name: &str,
        token: Option<&str>,
        now: u64,
    ) -> Result<Result<PairOutcome, RedeemError>, StateError> {
        self.reload()?;
        if self.is_authorized(endpoint_id) {
            return Ok(Ok(PairOutcome::AlreadyAuthorized));
        }
        let Some(token) = token else {
            return Ok(Err(RedeemError::Invalid));
        };
        let name = sanitize_device_name(device_name);
        self.update(|file| -> Result<PairOutcome, RedeemError> {
            let StateFile {
                invitations,
                pairing_failures,
                devices,
                ..
            } = file;
            let index = pairing::redeem(invitations, pairing_failures, token, endpoint_id, now)?;
            let invitation_id = invitations[index].id.clone();
            devices.retain(|device| device.endpoint_id != endpoint_id);
            devices.push(Device {
                endpoint_id: endpoint_id.to_owned(),
                name,
                paired_at: now,
                invitation_id,
                revoked_at: None,
            });
            Ok(PairOutcome::Paired)
        })
    }

    /// Revokes a device. Returns false when no active device has that id.
    pub fn revoke_device(&self, endpoint_id: &str, now: u64) -> Result<bool, StateError> {
        self.update(|file| {
            match file
                .devices
                .iter_mut()
                .find(|device| device.endpoint_id == endpoint_id && device.is_active())
            {
                Some(device) => {
                    device.revoked_at = Some(now);
                    true
                }
                None => false,
            }
        })
    }

    /// Cancels an unredeemed invitation. Returns false when none has that id.
    pub fn cancel_invitation(&self, id: &str) -> Result<bool, StateError> {
        self.update(|file| {
            let before = file.invitations.len();
            file.invitations
                .retain(|invitation| invitation.id != id || invitation.is_redeemed());
            file.invitations.len() != before
        })
    }

    /// Applies `change` to the current file, persists it, then publishes it. On write failure nothing
    /// changes.
    fn update<T>(&self, change: impl FnOnce(&mut StateFile) -> T) -> Result<T, StateError> {
        let _file_lock = lock_state(&self.path)?;
        let mut guard = self.lock();
        let mut next = load(&self.path)?;
        let result = change(&mut next);
        prune_invitations(&mut next, now_secs());
        persist(&self.path, &next)?;
        *guard = next;
        Ok(result)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StateFile> {
        // A panic while holding the lock cannot leave a half-applied change: `update` only
        // publishes fully persisted copies, so the inner value is still consistent.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn prune_invitations(file: &mut StateFile, now: u64) {
    file.invitations
        .retain(|inv| now < inv.expires_at.saturating_add(INVITATION_RETENTION_SECS));
}

fn sanitize_device_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DEVICE_NAME_CHARS)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "Unnamed device".into()
    } else {
        cleaned.to_owned()
    }
}

fn load(path: &Path) -> Result<StateFile, StateError> {
    match fs::read(path) {
        Ok(bytes) => {
            restrict_permissions(path).map_err(|source| io_error(path, source))?;
            let file: StateFile =
                serde_json::from_slice(&bytes).map_err(|source| StateError::Corrupt {
                    path: path.to_path_buf(),
                    source,
                })?;
            if file.version > STATE_VERSION {
                return Err(StateError::UnsupportedVersion {
                    path: path.to_path_buf(),
                    found: file.version,
                });
            }
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(StateFile::default()),
        Err(source) => Err(io_error(path, source)),
    }
}

/// Blocks until this process holds the state write lock. Released when the file is dropped.
fn lock_state(state_path: &Path) -> Result<File, StateError> {
    let lock_path = state_path.with_file_name(STATE_LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|source| io_error(&lock_path, source))?;
    file.lock().map_err(|source| io_error(&lock_path, source))?;
    Ok(file)
}

fn persist(path: &Path, file: &StateFile) -> Result<(), StateError> {
    let bytes = serde_json::to_vec_pretty(file).expect("state is always serializable");
    write_private_atomic(path, &bytes).map_err(|source| io_error(path, source))
}

fn io_error(path: &Path, source: io::Error) -> StateError {
    StateError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::test_support::TempDir;

    fn open(dir: &TempDir) -> HostState {
        HostState::open(&dir.0.join(STATE_FILE), None).unwrap()
    }

    #[test]
    fn pairing_persists_across_restarts() {
        let dir = TempDir::create();
        let now = now_secs();
        let state = open(&dir);
        let (_, token) = state
            .create_invitation("Living room TV", None, now)
            .unwrap();
        assert_eq!(
            state.pair("endpoint-a", "TV", Some(&token), now).unwrap(),
            Ok(PairOutcome::Paired)
        );
        drop(state);

        let state = open(&dir);
        assert!(state.is_authorized("endpoint-a"));
        assert_eq!(
            state.pair("endpoint-a", "TV", None, now).unwrap(),
            Ok(PairOutcome::AlreadyAuthorized)
        );
    }

    #[test]
    fn cancelled_invitations_cannot_be_redeemed() {
        let dir = TempDir::create();
        let now = now_secs();
        let state = open(&dir);
        let (invitation, token) = state.create_invitation("Phone", None, now).unwrap();
        assert!(state.cancel_invitation(&invitation.id).unwrap());
        assert!(!state.cancel_invitation(&invitation.id).unwrap());
        assert_eq!(
            state
                .pair("endpoint-a", "Phone", Some(&token), now)
                .unwrap(),
            Err(RedeemError::Invalid)
        );
    }

    #[test]
    fn plaintext_tokens_are_never_written() {
        let dir = TempDir::create();
        let state = open(&dir);
        let (_, token) = state.create_invitation("Phone", None, now_secs()).unwrap();
        let on_disk = fs::read_to_string(state.path()).unwrap();
        assert!(!on_disk.contains(&token));
        assert!(on_disk.contains(&pairing::hash_token(&token)));
    }

    #[test]
    fn invitations_are_single_use_across_devices() {
        let dir = TempDir::create();
        let now = now_secs();
        let state = open(&dir);
        let (_, token) = state.create_invitation("Phone", None, now).unwrap();
        assert!(
            state
                .pair("endpoint-a", "A", Some(&token), now)
                .unwrap()
                .is_ok()
        );
        assert_eq!(
            state.pair("endpoint-b", "B", Some(&token), now).unwrap(),
            Err(RedeemError::AlreadyUsed)
        );
        assert!(!state.is_authorized("endpoint-b"));
    }

    #[test]
    fn unknown_device_without_token_is_denied() {
        let dir = TempDir::create();
        let state = open(&dir);
        assert_eq!(
            state.pair("stranger", "X", None, now_secs()).unwrap(),
            Err(RedeemError::Invalid)
        );
    }

    #[test]
    fn revoked_devices_lose_access_and_can_repair_with_new_invitation() {
        let dir = TempDir::create();
        let now = now_secs();
        let state = open(&dir);
        let (_, first) = state.create_invitation("TV", None, now).unwrap();
        state
            .pair("endpoint-a", "TV", Some(&first), now)
            .unwrap()
            .unwrap();
        assert!(state.revoke_device("endpoint-a", now).unwrap());
        assert!(!state.is_authorized("endpoint-a"));
        assert!(!state.revoke_device("endpoint-a", now).unwrap());

        let (_, second) = state.create_invitation("TV again", None, now).unwrap();
        assert!(
            state
                .pair("endpoint-a", "TV", Some(&second), now)
                .unwrap()
                .is_ok()
        );
        assert!(state.is_authorized("endpoint-a"));
        assert_eq!(state.snapshot().devices.len(), 1);
    }

    #[test]
    fn rate_limit_survives_restart() {
        let dir = TempDir::create();
        let now = now_secs();
        let state = open(&dir);
        let (_, token) = state.create_invitation("TV", None, now).unwrap();
        for _ in 0..pairing::MAX_FAILURES_PER_WINDOW {
            let _ = state.pair("attacker", "X", Some("wrong"), now).unwrap();
        }
        drop(state);
        let state = open(&dir);
        assert_eq!(
            state.pair("endpoint-a", "TV", Some(&token), now).unwrap(),
            Err(RedeemError::RateLimited)
        );
    }

    #[test]
    fn validates_invitation_parameters() {
        let dir = TempDir::create();
        let state = open(&dir);
        let now = now_secs();
        assert!(state.create_invitation("  ", None, now).is_err());
        assert!(state.create_invitation(&"x".repeat(81), None, now).is_err());
        assert!(state.create_invitation("TV", Some(10), now).is_err());
        assert!(
            state
                .create_invitation("TV", Some(MAX_INVITATION_TTL_SECS + 1), now)
                .is_err()
        );
    }

    #[test]
    fn configured_policy_overrides_stored_policy() {
        let dir = TempDir::create();
        let path = dir.0.join(STATE_FILE);
        HostState::open(&path, None)
            .unwrap()
            .set_policy(TransportPolicy::RelayOnly)
            .unwrap();
        assert_eq!(
            HostState::open(&path, None).unwrap().policy(),
            TransportPolicy::RelayOnly
        );
        assert_eq!(
            HostState::open(&path, Some(TransportPolicy::P2pOnly))
                .unwrap()
                .policy(),
            TransportPolicy::P2pOnly
        );
    }

    #[test]
    fn rejects_corrupt_and_future_state() {
        let dir = TempDir::create();
        let path = dir.0.join(STATE_FILE);
        fs::write(&path, b"{not json").unwrap();
        assert!(matches!(
            HostState::open(&path, None),
            Err(StateError::Corrupt { .. })
        ));
        fs::write(&path, br#"{"version": 99}"#).unwrap();
        assert!(matches!(
            HostState::open(&path, None),
            Err(StateError::UnsupportedVersion { found: 99, .. })
        ));
    }

    #[test]
    fn separate_handles_see_each_others_changes() {
        let dir = TempDir::create();
        let now = now_secs();
        let host = open(&dir);
        let admin = open(&dir);
        let (_, token) = admin.create_invitation("TV", None, now).unwrap();
        assert_eq!(
            host.pair("endpoint-a", "TV", Some(&token), now).unwrap(),
            Ok(PairOutcome::Paired)
        );
        assert!(admin.revoke_device("endpoint-a", now).unwrap());
        assert!(host.is_authorized("endpoint-a"));
        host.reload().unwrap();
        assert!(!host.is_authorized("endpoint-a"));
        let (_, second) = admin.create_invitation("Phone", None, now).unwrap();
        host.create_invitation("Tablet", None, now).unwrap();
        admin.reload().unwrap();
        assert_eq!(admin.snapshot().invitations.len(), 3);
        assert!(
            admin
                .snapshot()
                .invitations
                .iter()
                .any(|inv| inv.token_hash == pairing::hash_token(&second))
        );
    }

    #[test]
    fn device_names_are_sanitized() {
        assert_eq!(sanitize_device_name("  TV\u{0007}\n "), "TV");
        assert_eq!(sanitize_device_name("\n"), "Unnamed device");
        assert_eq!(
            sanitize_device_name(&"a".repeat(100)).len(),
            MAX_DEVICE_NAME_CHARS
        );
    }
}
