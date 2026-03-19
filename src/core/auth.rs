use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

use rand::Rng;
use subtle::ConstantTimeEq;
use tracing::warn;

use super::config::AuthConfig;
use super::error::IngestError;
use super::security;
use crate::core::types::StreamId;

/// Bcrypt cost factor for hashing stream keys and admin tokens (security.md §2.1).
const BCRYPT_COST: u32 = 10;

/// Characters used for random key/token generation.
const ALPHANUMERIC: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// Authentication provider for ingest and admin API requests.
///
/// Thread-safe: all mutable state is behind `Mutex` so methods take `&self`.
/// This allows `AuthProvider` to be shared via `Arc<AuthProvider>` without
/// requiring `Arc<Mutex<AuthProvider>>`.
///
/// Stream keys are stored as bcrypt hashes.
/// Admin bearer tokens are kept as plaintext in memory and compared with
/// constant-time equality to avoid expensive per-request bcrypt verification.
#[derive(Debug)]
pub struct AuthProvider {
    /// Valid RTMP stream keys stored as bcrypt hashes.
    ingest_keys: Mutex<Vec<IngestKeyRecord>>,
    /// Valid admin bearer tokens in plaintext for fast constant-time checks.
    admin_tokens: Mutex<Vec<String>>,
    /// IP-based brute-force tracker (from security.md §2.1).
    brute_force_tracker: Mutex<HashMap<IpAddr, BruteForceEntry>>,
}

#[derive(Debug, Clone)]
struct IngestKeyRecord {
    hash: String,
    stream_id: Option<StreamId>,
}

/// Tracks failed auth attempts from a single IP.
#[derive(Debug, Clone)]
struct BruteForceEntry {
    /// Number of failed attempts in the current window.
    attempts: u32,
    /// When the first attempt in the current window occurred.
    window_start: Instant,
    /// If blocked, when the block expires.
    blocked_until: Option<Instant>,
}

impl AuthProvider {
    pub fn new(config: &AuthConfig) -> Self {
        // Hash all configured ingest keys at startup (security.md §2.1).
        let key_hashes: Vec<IngestKeyRecord> = config
            .ingest_stream_keys
            .iter()
            .filter_map(|k| {
                bcrypt::hash(k, BCRYPT_COST)
                    .ok()
                    .map(|hash| IngestKeyRecord {
                        hash,
                        stream_id: None,
                    })
            })
            .collect();
        Self {
            ingest_keys: Mutex::new(key_hashes),
            admin_tokens: Mutex::new(config.admin_bearer_tokens.clone()),
            brute_force_tracker: Mutex::new(HashMap::new()),
        }
    }

    /// Generate a new stream key with `sk_` prefix + 24 random alphanumeric chars.
    /// Returns (plaintext_key, bcrypt_hash). The plaintext is returned to the
    /// caller once; only the hash is stored (security.md §2.1).
    pub fn generate_stream_key(&self) -> (String, String) {
        self.generate_stream_key_for_stream(Option::<StreamId>::None)
    }

    /// Generate a new stream key bound to a specific stream ID.
    pub fn generate_stream_key_for_stream(
        &self,
        stream_id: impl Into<Option<StreamId>>,
    ) -> (String, String) {
        let plaintext = generate_random_token(
            security::STREAM_KEY_PREFIX,
            security::STREAM_KEY_RANDOM_LENGTH,
        );
        let hash = bcrypt::hash(&plaintext, BCRYPT_COST)
            .expect("bcrypt hash should not fail for valid input");
        {
            let mut keys = self.ingest_keys.lock().unwrap_or_else(|e| e.into_inner());
            keys.push(IngestKeyRecord {
                hash: hash.clone(),
                stream_id: stream_id.into(),
            });
        }
        (plaintext, hash)
    }

    /// Generate a new admin bearer token with `at_` prefix + 32 random alphanumeric chars.
    /// Returns (plaintext_token, bcrypt_hash) for audit/compatibility.
    pub fn generate_admin_token(&self) -> (String, String) {
        let plaintext = generate_random_token("at_", 32);
        let hash = bcrypt::hash(&plaintext, BCRYPT_COST)
            .expect("bcrypt hash should not fail for valid input");
        {
            let mut tokens = self.admin_tokens.lock().unwrap_or_else(|e| e.into_inner());
            tokens.push(plaintext.clone());
        }
        (plaintext, hash)
    }

    /// Rotate the stream key for a given stream. Invalidates the old key hash
    /// and returns a new (plaintext, hash) pair (security.md §2.1).
    pub fn rotate_stream_key(&self, old_hash: &str) -> (String, String) {
        {
            let mut keys = self.ingest_keys.lock().unwrap_or_else(|e| e.into_inner());
            keys.retain(|k| k.hash != old_hash);
        }
        self.generate_stream_key()
    }

    /// Rotate the stream key set for a specific stream id.
    ///
    /// Removes previously bound keys for that stream and returns one fresh key.
    pub fn rotate_stream_key_for_stream(&self, stream_id: StreamId) -> (String, String) {
        {
            let mut keys = self.ingest_keys.lock().unwrap_or_else(|e| e.into_inner());
            keys.retain(|k| k.stream_id != Some(stream_id));
        }
        self.generate_stream_key_for_stream(stream_id)
    }

    /// Validate an RTMP stream key against stored bcrypt hashes.
    /// Returns Ok(()) if the key is valid, or an IngestError::AuthFailed if not.
    /// If no keys are configured, all keys are accepted (open mode).
    /// Uses bcrypt_verify which is inherently constant-time (security.md §2.1).
    pub fn validate_stream_key(&self, stream_key: &str) -> Result<(), IngestError> {
        let keys = self.ingest_keys.lock().unwrap_or_else(|e| e.into_inner());
        if keys.is_empty() {
            return Ok(());
        }

        if keys
            .iter()
            .any(|k| bcrypt::verify(stream_key, &k.hash).unwrap_or(false))
        {
            Ok(())
        } else {
            Err(IngestError::AuthFailed {
                reason: "invalid stream key".to_string(),
            })
        }
    }

    /// Validate an RTMP stream key with IP-based brute-force protection.
    ///
    /// From security.md §2.1:
    /// - After 5 failed attempts from the same IP within 60 seconds,
    ///   the IP is temporarily blocked for 5 minutes.
    pub fn validate_stream_key_with_ip(
        &self,
        stream_key: &str,
        client_ip: IpAddr,
    ) -> Result<(), IngestError> {
        // Check if IP is blocked
        if self.is_ip_blocked(client_ip) {
            warn!(
                ip = %client_ip,
                "RTMP auth rejected: IP is temporarily blocked"
            );
            return Err(IngestError::AuthFailed {
                reason: "too many failed attempts, IP temporarily blocked".to_string(),
            });
        }

        match self.validate_stream_key(stream_key) {
            Ok(()) => {
                self.reset_ip_attempts(client_ip);
                Ok(())
            }
            Err(e) => {
                self.record_failed_attempt(client_ip);
                Err(e)
            }
        }
    }

    /// Check if an IP is currently blocked.
    fn is_ip_blocked(&self, ip: IpAddr) -> bool {
        let tracker = self
            .brute_force_tracker
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = tracker.get(&ip) {
            if let Some(blocked_until) = entry.blocked_until {
                return Instant::now() < blocked_until;
            }
        }
        false
    }

    /// Record a failed auth attempt from an IP.
    fn record_failed_attempt(&self, ip: IpAddr) {
        let mut tracker = self
            .brute_force_tracker
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let entry = tracker.entry(ip).or_insert(BruteForceEntry {
            attempts: 0,
            window_start: now,
            blocked_until: None,
        });

        // Reset window if expired (from security.md §2.1)
        if now.duration_since(entry.window_start).as_secs() > security::BRUTE_FORCE_WINDOW_SECS {
            entry.attempts = 0;
            entry.window_start = now;
            entry.blocked_until = None;
        }

        entry.attempts += 1;

        // Block after max failed attempts (from security.md §2.1)
        if entry.attempts >= security::BRUTE_FORCE_MAX_ATTEMPTS {
            entry.blocked_until =
                Some(now + std::time::Duration::from_secs(security::BRUTE_FORCE_BLOCK_SECS));
            warn!(ip = %ip, attempts = entry.attempts, "IP blocked for 5 minutes due to brute-force");
        }
    }

    /// Reset failed attempts for an IP (on successful auth).
    fn reset_ip_attempts(&self, ip: IpAddr) {
        let mut tracker = self
            .brute_force_tracker
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        tracker.remove(&ip);
    }

    /// Validate an admin bearer token against stored admin token set.
    /// Returns true if the token is valid.
    /// If no tokens are configured, all requests are accepted (open mode).
    /// Uses constant-time string equality to avoid timing leaks (security.md §2.2).
    pub fn validate_bearer_token(&self, token: &str) -> bool {
        let tokens = self.admin_tokens.lock().unwrap_or_else(|e| e.into_inner());
        if tokens.is_empty() {
            return true;
        }

        tokens
            .iter()
            .any(|t| token.as_bytes().ct_eq(t.as_bytes()).unwrap_u8() == 1)
    }

    /// Check a bearer token from an Authorization header.
    ///
    /// From security.md §2.2:
    /// - Missing → 401
    /// - Invalid format → 401
    /// - Not in valid set → 403
    /// - Valid → proceed
    pub fn check_bearer_token(&self, token: Option<&str>) -> TokenStatus {
        if self.is_open_mode() {
            return TokenStatus::Valid;
        }
        match token {
            None => TokenStatus::Missing,
            Some(t) => {
                if self.validate_bearer_token(t) {
                    TokenStatus::Valid
                } else {
                    TokenStatus::Forbidden
                }
            }
        }
    }

    /// Update stream keys at runtime (supports key rotation without restart).
    /// Accepts plaintext keys and hashes them before storing.
    pub fn update_stream_keys(&self, keys: Vec<String>) {
        let records: Vec<IngestKeyRecord> = keys
            .iter()
            .filter_map(|k| {
                bcrypt::hash(k, BCRYPT_COST)
                    .ok()
                    .map(|hash| IngestKeyRecord {
                        hash,
                        stream_id: None,
                    })
            })
            .collect();
        let mut current = self.ingest_keys.lock().unwrap_or_else(|e| e.into_inner());
        *current = records;
    }

    /// Resolve stream id associated with a stream key.
    ///
    /// Returns `Some(stream_id)` only for keys generated by the control API
    /// that were explicitly bound to a stream. Returns `None` for keys loaded
    /// from static config or unknown keys.
    pub fn resolve_stream_id_for_key(&self, stream_key: &str) -> Option<StreamId> {
        let keys = self.ingest_keys.lock().unwrap_or_else(|e| e.into_inner());
        keys.iter().find_map(|k| {
            if bcrypt::verify(stream_key, &k.hash).unwrap_or(false) {
                k.stream_id
            } else {
                None
            }
        })
    }

    /// Update admin bearer tokens at runtime.
    /// Accepts plaintext tokens and stores them for constant-time comparisons.
    pub fn update_admin_tokens(&self, tokens: Vec<String>) {
        let mut current = self.admin_tokens.lock().unwrap_or_else(|e| e.into_inner());
        *current = tokens;
    }

    /// Check if running in open mode (no admin tokens configured).
    pub fn is_open_mode(&self) -> bool {
        self.admin_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Clean up expired brute-force tracker entries.
    /// Called periodically by the cleanup task.
    pub fn cleanup_brute_force_tracker(&self) {
        let mut tracker = self
            .brute_force_tracker
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        tracker.retain(|_ip, entry| {
            // Keep entries that are still blocked or have recent attempts
            if let Some(blocked_until) = entry.blocked_until {
                if now < blocked_until {
                    return true;
                }
            }
            // Remove entries older than the brute-force window
            now.duration_since(entry.window_start).as_secs() < security::BRUTE_FORCE_WINDOW_SECS
        });
    }
}

/// Result of checking a bearer token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenStatus {
    Valid,
    Missing,
    Forbidden,
}

/// Generate a random token with a given prefix and random part length.
/// Characters are drawn from [a-z0-9] (security.md §2.1).
fn generate_random_token(prefix: &str, random_length: usize) -> String {
    let mut rng = rand::thread_rng();
    let random_part: String = (0..random_length)
        .map(|_| {
            let idx = rng.gen_range(0..ALPHANUMERIC.len());
            ALPHANUMERIC[idx] as char
        })
        .collect();
    format!("{}{}", prefix, random_part)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_auth_config() -> AuthConfig {
        AuthConfig {
            ingest_stream_keys: Vec::new(),
            admin_bearer_tokens: Vec::new(),
        }
    }

    #[test]
    fn test_resolve_stream_id_for_bound_key() {
        let provider = AuthProvider::new(&empty_auth_config());
        let stream_id = StreamId::new();
        let (key, _hash) = provider.generate_stream_key_for_stream(stream_id);

        let resolved = provider.resolve_stream_id_for_key(&key);
        assert_eq!(resolved, Some(stream_id));
    }

    #[test]
    fn test_resolve_stream_id_for_unbound_key() {
        let provider = AuthProvider::new(&empty_auth_config());
        let (key, _hash) = provider.generate_stream_key();

        let resolved = provider.resolve_stream_id_for_key(&key);
        assert_eq!(resolved, None);
    }

    #[test]
    fn test_rotate_stream_key_for_stream_invalidates_old_keys() {
        let provider = AuthProvider::new(&empty_auth_config());
        let stream_id = StreamId::new();
        let (old_key, _old_hash) = provider.generate_stream_key_for_stream(stream_id);

        let (new_key, _new_hash) = provider.rotate_stream_key_for_stream(stream_id);

        assert!(provider.validate_stream_key(&old_key).is_err());
        assert_eq!(provider.resolve_stream_id_for_key(&old_key), None);
        assert!(provider.validate_stream_key(&new_key).is_ok());
        assert_eq!(
            provider.resolve_stream_id_for_key(&new_key),
            Some(stream_id)
        );
    }

    #[test]
    fn test_open_mode_tracks_runtime_token_updates() {
        let provider = AuthProvider::new(&empty_auth_config());
        assert!(provider.is_open_mode());

        provider.update_admin_tokens(vec!["at_runtime_token".to_string()]);
        assert!(!provider.is_open_mode());

        provider.update_admin_tokens(Vec::new());
        assert!(provider.is_open_mode());
    }

    #[test]
    fn test_bearer_token_validation_constant_time_path() {
        let provider = AuthProvider::new(&AuthConfig {
            ingest_stream_keys: Vec::new(),
            admin_bearer_tokens: vec!["at_abc123".to_string()],
        });

        assert!(provider.validate_bearer_token("at_abc123"));
        assert!(!provider.validate_bearer_token("at_wrong"));
    }
}
