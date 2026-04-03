use std::collections::HashMap;
use std::path::{Path, PathBuf};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// A single authenticated session.
#[derive(Serialize, Deserialize)]
pub struct Session {
    pub username: String,
    pub created: DateTime<Utc>,
    pub expires: DateTime<Utc>,
    pub csrf_token: String,
}

/// Session store with optional file-system persistence.
///
/// When `persist_dir` is `Some`, each session is written as a JSON file
/// at `{persist_dir}/{session_id}.json` and removed on logout / cleanup.
pub struct SessionStore {
    sessions: HashMap<String, Session>,
    persist_dir: Option<PathBuf>,
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
            persist_dir: None,
        }
    }

    /// Create a session store that persists sessions to `dir`.
    /// Loads any existing session files from the directory.
    pub fn with_persistence(dir: &Path) -> Self {
        if let Err(e) = std::fs::create_dir_all(dir) {
            warn!(path = %dir.display(), error = %e, "Failed to create session directory");
        }

        let mut sessions = HashMap::new();
        let now = Utc::now();

        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }

                let session_id = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(id) => id.to_string(),
                    None => continue,
                };

                let content = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(path = %path.display(), error = %e, "Failed to read session file");
                        continue;
                    }
                };

                match serde_json::from_str::<Session>(&content) {
                    Ok(session) => {
                        if session.expires > now {
                            debug!(session_id = %session_id, username = %session.username, "Loaded persisted session");
                            sessions.insert(session_id, session);
                        } else {
                            // Expired — remove the file
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                    Err(e) => {
                        warn!(path = %path.display(), error = %e, "Failed to parse session file");
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }

        debug!(count = sessions.len(), "Loaded persisted sessions");

        Self {
            sessions,
            persist_dir: Some(dir.to_path_buf()),
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

        self.persist_session(&session_id, &session);

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
        self.remove_session_file(session_id);
    }

    /// Remove all expired sessions.
    pub fn cleanup_expired(&mut self) {
        let now = Utc::now();
        let expired_ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.expires <= now)
            .map(|(id, _)| id.clone())
            .collect();

        for id in &expired_ids {
            self.remove_session_file(id);
        }

        self.sessions.retain(|_, s| s.expires > now);
    }

    fn persist_session(&self, session_id: &str, session: &Session) {
        if let Some(ref dir) = self.persist_dir {
            let path = dir.join(format!("{}.json", session_id));
            match serde_json::to_string_pretty(session) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&path, json) {
                        warn!(path = %path.display(), error = %e, "Failed to persist session");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to serialize session");
                }
            }
        }
    }

    fn remove_session_file(&self, session_id: &str) {
        if let Some(ref dir) = self.persist_dir {
            let path = dir.join(format!("{}.json", session_id));
            let _ = std::fs::remove_file(&path);
        }
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

    #[test]
    fn test_persistence_survives_reload() {
        let dir = tempfile::TempDir::new().unwrap();
        let session_id;

        // Create a session in a persistent store
        {
            let mut store = SessionStore::with_persistence(dir.path());
            session_id = store.create_session("bob", 3600);
            assert!(store.get_session(&session_id).is_some());
        }

        // Reload from disk — session should still exist
        {
            let store = SessionStore::with_persistence(dir.path());
            let session = store
                .get_session(&session_id)
                .expect("session should survive reload");
            assert_eq!(session.username, "bob");
        }
    }

    #[test]
    fn test_persistence_removes_on_logout() {
        let dir = tempfile::TempDir::new().unwrap();
        let session_id;

        {
            let mut store = SessionStore::with_persistence(dir.path());
            session_id = store.create_session("bob", 3600);
            store.remove_session(&session_id);
        }

        // Reload — session file should be gone
        {
            let store = SessionStore::with_persistence(dir.path());
            assert!(
                store.get_session(&session_id).is_none(),
                "removed session should not survive reload"
            );
        }
    }

    #[test]
    fn test_persistence_cleanup_removes_files() {
        let dir = tempfile::TempDir::new().unwrap();

        let expired_id;
        let valid_id;

        {
            let mut store = SessionStore::with_persistence(dir.path());
            expired_id = store.create_session("expired", 0);
            valid_id = store.create_session("valid", 3600);
            store.cleanup_expired();
        }

        // Reload — only the valid session should exist
        {
            let store = SessionStore::with_persistence(dir.path());
            assert!(
                store.get_session(&expired_id).is_none(),
                "expired session should not survive"
            );
            assert!(
                store.get_session(&valid_id).is_some(),
                "valid session should survive"
            );
        }
    }
}
