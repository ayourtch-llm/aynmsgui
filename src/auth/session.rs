use std::collections::HashMap;
use chrono::{DateTime, Utc};
use rand::RngCore;
use tracing::debug;

/// A single authenticated session.
pub struct Session {
    pub username: String,
    pub created: DateTime<Utc>,
    pub expires: DateTime<Utc>,
    pub csrf_token: String,
}

/// In-memory session store.
pub struct SessionStore {
    sessions: HashMap<String, Session>,
}

/// Generate a hex-encoded string of `N` random bytes.
fn random_hex<const N: usize>() -> String {
    let mut bytes = [0u8; N];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().fold(String::with_capacity(N * 2), |mut s: String, b| {
        s.push_str(&format!("{b:02x}"));
        s
    })
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    /// Create a new session for `username` that expires after `ttl_secs` seconds.
    /// Returns the session ID (64-character hex string).
    pub fn create_session(&mut self, username: &str, ttl_secs: u64) -> String {
        let session_id = random_hex::<32>();
        let csrf_token = random_hex::<16>();
        let now = Utc::now();
        let expires = now + chrono::Duration::seconds(ttl_secs as i64);

        let session = Session {
            username: username.to_string(),
            created: now,
            expires,
            csrf_token,
        };

        debug!(session_id = %session_id, username = %username, "Created session");
        self.sessions.insert(session_id.clone(), session);
        session_id
    }

    /// Look up a session. Returns `None` if the session does not exist or has expired.
    pub fn get_session(&self, session_id: &str) -> Option<&Session> {
        match self.sessions.get(session_id) {
            None => None,
            Some(session) if session.expires <= Utc::now() => {
                debug!(session_id = %session_id, "Session expired");
                None
            }
            Some(session) => Some(session),
        }
    }

    /// Remove a specific session (e.g. on logout).
    pub fn remove_session(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
    }

    /// Remove all expired sessions.
    pub fn cleanup_expired(&mut self) {
        let now = Utc::now();
        self.sessions.retain(|_, s| s.expires > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_session_returns_id() {
        let mut store = SessionStore::new();
        let id = store.create_session("alice", 3600);
        assert!(!id.is_empty(), "session ID should be non-empty");
    }

    #[test]
    fn test_get_valid_session() {
        let mut store = SessionStore::new();
        let id = store.create_session("alice", 3600);
        let session = store.get_session(&id).expect("session should exist");
        assert_eq!(session.username, "alice");
    }

    #[test]
    fn test_get_expired_session_returns_none() {
        let mut store = SessionStore::new();
        // ttl_secs = 0 means it expires immediately (at or before now)
        let id = store.create_session("alice", 0);
        // A tiny sleep is needed only if the clock resolution is coarse; in practice
        // expires == now so the check `expires <= now` fires immediately.
        assert!(store.get_session(&id).is_none(), "expired session should return None");
    }

    #[test]
    fn test_remove_session() {
        let mut store = SessionStore::new();
        let id = store.create_session("alice", 3600);
        store.remove_session(&id);
        assert!(store.get_session(&id).is_none(), "removed session should return None");
    }

    #[test]
    fn test_cleanup_expired() {
        let mut store = SessionStore::new();
        let expired_id = store.create_session("expired_user", 0);
        let valid_id = store.create_session("valid_user", 3600);

        store.cleanup_expired();

        // Expired session should be gone from the backing map entirely
        assert!(
            store.sessions.get(&expired_id).is_none(),
            "expired session should be removed by cleanup"
        );
        // Valid session should remain
        assert!(
            store.sessions.get(&valid_id).is_some(),
            "valid session should survive cleanup"
        );
    }

    #[test]
    fn test_session_has_csrf_token() {
        let mut store = SessionStore::new();
        let id = store.create_session("alice", 3600);
        let session = store.get_session(&id).expect("session should exist");
        assert!(!session.csrf_token.is_empty(), "csrf_token should be non-empty");
    }
}
