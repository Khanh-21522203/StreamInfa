use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

use tracing::warn;

use super::config::AuthConfig;
use super::error::IngestError;
use super::security;

/// Authentication provider for ingest and admin API requests.
///
/// Thread-safe: all mutable state is behind `Mutex` so methods take `&self`.
/// This allows `AuthProvider` to be shared via `Arc<AuthProvider>` without
/// requiring `Arc<Mutex<AuthProvider>>`.
#[derive(Debug)]
pub struct AuthProvider {
    /// Valid RTMP stream keys.
    ingest_stream_keys: Mutex<Vec<String>>,
    /// Valid admin bearer tokens.
    admin_bearer_tokens: Mutex<Vec<String>>,
    /// Whether open mode is active (no tokens configured).
    admin_tokens_open_mode: bool,
    /// IP-based brute-force tracker (from security.md §2.1).
    brute_force_tracker: Mutex<HashMap<IpAddr, BruteForceEntry>>,
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
        let open_mode = config.admin_bearer_tokens.is_empty();
        Self {
            ingest_stream_keys: Mutex::new(config.ingest_stream_keys.clone()),
            admin_bearer_tokens: Mutex::new(config.admin_bearer_tokens.clone()),
            admin_tokens_open_mode: open_mode,
            brute_force_tracker: Mutex::new(HashMap::new()),
        }
    }

    /// Validate an RTMP stream key.
    /// Returns Ok(()) if the key is valid, or an IngestError::AuthFailed if not.
    /// If no keys are configured, all keys are accepted (open mode).
    pub fn validate_stream_key(&self, stream_key: &str) -> Result<(), IngestError> {
        let keys = self.ingest_stream_keys.lock().unwrap();
        if keys.is_empty() {
            return Ok(());
        }

        if keys.iter().any(|k| k == stream_key) {
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
        let tracker = self.brute_force_tracker.lock().unwrap();
        if let Some(entry) = tracker.get(&ip) {
            if let Some(blocked_until) = entry.blocked_until {
                return Instant::now() < blocked_until;
            }
        }
        false
    }

    /// Record a failed auth attempt from an IP.
    fn record_failed_attempt(&self, ip: IpAddr) {
        let mut tracker = self.brute_force_tracker.lock().unwrap();
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
        let mut tracker = self.brute_force_tracker.lock().unwrap();
        tracker.remove(&ip);
    }

    /// Validate an admin bearer token (for HTTP upload and control plane).
    /// Returns true if the token is valid.
    /// If no tokens are configured, all requests are accepted (open mode).
    pub fn validate_bearer_token(&self, token: &str) -> bool {
        let tokens = self.admin_bearer_tokens.lock().unwrap();
        if tokens.is_empty() {
            return true;
        }

        tokens.iter().any(|t| t == token)
    }

    /// Check a bearer token from an Authorization header.
    ///
    /// From security.md §2.2:
    /// - Missing → 401
    /// - Invalid format → 401
    /// - Not in valid set → 403
    /// - Valid → proceed
    pub fn check_bearer_token(&self, token: Option<&str>) -> TokenStatus {
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
    pub fn update_stream_keys(&self, keys: Vec<String>) {
        let mut current = self.ingest_stream_keys.lock().unwrap();
        *current = keys;
    }

    /// Update admin bearer tokens at runtime.
    pub fn update_admin_tokens(&self, tokens: Vec<String>) {
        let mut current = self.admin_bearer_tokens.lock().unwrap();
        *current = tokens;
    }

    /// Check if running in open mode (no admin tokens configured).
    pub fn is_open_mode(&self) -> bool {
        self.admin_tokens_open_mode
    }

    /// Clean up expired brute-force tracker entries.
    /// Called periodically by the cleanup task.
    pub fn cleanup_brute_force_tracker(&self) {
        let mut tracker = self.brute_force_tracker.lock().unwrap();
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
