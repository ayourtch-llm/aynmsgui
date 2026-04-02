use std::collections::HashMap;
use std::path::Path;
use tracing::warn;

/// Stores parsed htpasswd entries (username -> bcrypt hash).
pub struct HtpasswdStore {
    entries: HashMap<String, String>,
}

impl HtpasswdStore {
    /// Parse htpasswd content from a string. Blank lines and `#` comments are skipped.
    pub fn from_str(content: &str) -> Self {
        let mut entries = HashMap::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match line.split_once(':') {
                Some((username, hash)) => {
                    entries.insert(username.to_string(), hash.to_string());
                }
                None => {
                    warn!("Skipping malformed htpasswd line (no colon)");
                }
            }
        }
        Self { entries }
    }

    /// Load an htpasswd file from disk.
    pub fn from_file(path: &Path) -> Result<Self, std::io::Error> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::from_str(&content))
    }

    /// Verify a username/password pair. Returns false for unknown users or wrong passwords.
    /// Supports bcrypt ($2y$/$2b$/$2a$), Apache APR1 MD5 ($apr1$), SHA1 ({SHA}), and plaintext.
    pub fn verify(&self, username: &str, password: &str) -> bool {
        match self.entries.get(username) {
            None => false,
            Some(hash) if hash.starts_with("$2y$") || hash.starts_with("$2b$") || hash.starts_with("$2a$") => {
                bcrypt::verify(password, hash).unwrap_or(false)
            }
            Some(hash) => {
                // Use htpasswd-verify for all other formats (APR1, SHA1, crypt, plaintext)
                htpasswd_verify::Htpasswd::from(format!("{username}:{hash}").as_str())
                    .check(username, password)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bcrypt_hash(password: &str) -> String {
        bcrypt::hash(password, 4).expect("bcrypt::hash should not fail in tests")
    }

    #[test]
    fn test_parse_bcrypt_entry() {
        let hash = make_bcrypt_hash("password");
        let content = format!("alice:{hash}");
        let store = HtpasswdStore::from_str(&content);
        assert!(store.verify("alice", "password"), "correct password should verify");
    }

    #[test]
    fn test_wrong_password_returns_false() {
        let hash = make_bcrypt_hash("correct");
        let content = format!("bob:{hash}");
        let store = HtpasswdStore::from_str(&content);
        assert!(!store.verify("bob", "wrong"), "wrong password should return false");
    }

    #[test]
    fn test_unknown_user_returns_false() {
        let hash = make_bcrypt_hash("password");
        let content = format!("carol:{hash}");
        let store = HtpasswdStore::from_str(&content);
        assert!(!store.verify("dave", "password"), "unknown user should return false");
    }

    #[test]
    fn test_skip_comments_and_blank_lines() {
        let hash = make_bcrypt_hash("pass");
        let content = format!(
            "# This is a comment\n\nalice:{hash}\n\n# another comment\n"
        );
        let store = HtpasswdStore::from_str(&content);
        // Only alice should be present
        assert!(store.verify("alice", "pass"));
        assert!(!store.verify("#", "anything"));
    }

    #[test]
    fn test_apr1_hash_verifies() {
        // This is an actual APR1 hash for password "cisco"
        let content = "testuser:$apr1$mRma0MQu$ngg4/6wYQB5LxYpc9hLGc.";
        let store = HtpasswdStore::from_str(content);
        assert!(store.verify("testuser", "cisco"), "APR1 hash should verify with correct password");
        assert!(!store.verify("testuser", "wrong"), "APR1 hash should fail with wrong password");
    }

    #[test]
    fn test_multiple_users() {
        let hash_a = make_bcrypt_hash("passA");
        let hash_b = make_bcrypt_hash("passB");
        let content = format!("alice:{hash_a}\nbob:{hash_b}");
        let store = HtpasswdStore::from_str(&content);
        assert!(store.verify("alice", "passA"), "alice should verify with passA");
        assert!(store.verify("bob", "passB"), "bob should verify with passB");
        assert!(!store.verify("alice", "passB"), "alice should not verify with bob's password");
        assert!(!store.verify("bob", "passA"), "bob should not verify with alice's password");
    }
}
