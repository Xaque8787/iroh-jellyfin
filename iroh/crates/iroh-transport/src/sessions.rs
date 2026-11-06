//! Live table of admitted sessions, for status reporting by the embedding application.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use iroh::endpoint::Connection;
use serde::Serialize;

use crate::net::{PathReport, current_path};

#[derive(Default)]
pub struct SessionTable {
    next_id: AtomicU64,
    entries: Mutex<HashMap<u64, Arc<SessionEntry>>>,
}

struct SessionEntry {
    endpoint_id: String,
    name: String,
    connected_at: u64,
    connection: Connection,
    stats: SessionStats,
}

#[derive(Default)]
pub(crate) struct SessionStats {
    requests: AtomicU64,
    active_requests: AtomicU64,
    active_websockets: AtomicU64,
    bytes_sent: AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub endpoint_id: String,
    pub name: String,
    pub connected_at: u64,
    pub path: PathReport,
    pub requests: u64,
    pub active_requests: u64,
    pub active_websockets: u64,
    pub bytes_sent: u64,
}

/// Removes its session from the table when dropped.
pub(crate) struct SessionGuard {
    table: Arc<SessionTable>,
    id: u64,
    entry: Arc<SessionEntry>,
}

impl SessionGuard {
    pub(crate) fn stats(&self) -> &SessionStats {
        &self.entry.stats
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.table.lock().remove(&self.id);
    }
}

/// Counts one in-flight request until dropped.
pub(crate) struct RequestGuard<'a>(&'a SessionStats);

impl SessionStats {
    pub(crate) fn begin_request(&self) -> RequestGuard<'_> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.active_requests.fetch_add(1, Ordering::Relaxed);
        RequestGuard(self)
    }

    pub(crate) fn add_bytes(&self, bytes: u64) {
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Counts one open WebSocket until dropped, or returns `None` when `limit` are already open.
    pub(crate) fn begin_websocket(&self, limit: u64) -> Option<WebSocketGuard<'_>> {
        self.active_websockets
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |open| {
                (open < limit).then_some(open + 1)
            })
            .ok()?;
        Some(WebSocketGuard(self))
    }
}

/// Counts one open WebSocket until dropped.
pub(crate) struct WebSocketGuard<'a>(&'a SessionStats);

impl Drop for WebSocketGuard<'_> {
    fn drop(&mut self) {
        self.0.active_websockets.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.0.active_requests.fetch_sub(1, Ordering::Relaxed);
    }
}

impl SessionTable {
    pub(crate) fn register(
        self: &Arc<Self>,
        endpoint_id: &str,
        name: &str,
        connection: &Connection,
    ) -> SessionGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let connected_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let entry = Arc::new(SessionEntry {
            endpoint_id: endpoint_id.to_owned(),
            name: name.to_owned(),
            connected_at,
            connection: connection.clone(),
            stats: SessionStats::default(),
        });
        self.lock().insert(id, entry.clone());
        SessionGuard {
            table: self.clone(),
            id,
            entry,
        }
    }

    /// Current sessions, oldest first, with the path Iroh has selected right now.
    pub fn snapshot(&self) -> Vec<SessionInfo> {
        let entries: Vec<Arc<SessionEntry>> = self.lock().values().cloned().collect();
        let mut sessions: Vec<SessionInfo> = entries
            .iter()
            .map(|entry| SessionInfo {
                endpoint_id: entry.endpoint_id.clone(),
                name: entry.name.clone(),
                connected_at: entry.connected_at,
                path: current_path(&entry.connection),
                requests: entry.stats.requests.load(Ordering::Relaxed),
                active_requests: entry.stats.active_requests.load(Ordering::Relaxed),
                active_websockets: entry.stats.active_websockets.load(Ordering::Relaxed),
                bytes_sent: entry.stats.bytes_sent.load(Ordering::Relaxed),
            })
            .collect();
        sessions.sort_by_key(|session| session.connected_at);
        sessions
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<SessionEntry>>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
