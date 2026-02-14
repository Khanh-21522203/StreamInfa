use std::fmt;

// ---------------------------------------------------------------------------
// Sensitive field redaction (from security.md §7.1)
// ---------------------------------------------------------------------------

/// A wrapper that redacts its contents when displayed or debug-printed.
///
/// From security.md §7.1:
/// Fields named token, key, secret, password, authorization are replaced
/// with `[REDACTED]` in logs.
///
/// Usage:
/// ```ignore
/// let token = Redacted::new("my_secret_token");
/// tracing::info!(token = %token, "authenticating"); // logs: token=[REDACTED]
/// ```
#[derive(Clone)]
pub struct Redacted<T>(T);

impl<T> Redacted<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[REDACTED]")
    }
}

impl<T> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[REDACTED]")
    }
}

/// Redact a stream key for logging: show only the prefix + first 4 chars.
///
/// From security.md §7.1: `stream_key=sk_a***`
pub fn redact_stream_key(key: &str) -> String {
    if key.len() <= 4 {
        return "****".to_string();
    }
    let visible = &key[..key.len().min(6)];
    format!("{}***", visible)
}

/// Redact a bearer token for logging.
///
/// From security.md §7.1: `authorization=Bearer [REDACTED]`
pub fn redact_bearer_token(header_value: &str) -> String {
    if header_value.starts_with("Bearer ") {
        "Bearer [REDACTED]".to_string()
    } else {
        "[REDACTED]".to_string()
    }
}

/// Check if a field name is sensitive and should be redacted.
///
/// From security.md §7.1: token, key, secret, password, authorization.
pub fn is_sensitive_field(field_name: &str) -> bool {
    let lower = field_name.to_lowercase();
    lower.contains("token")
        || lower.contains("key")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("authorization")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redacted_display() {
        let secret = Redacted::new("super_secret_value");
        assert_eq!(format!("{}", secret), "[REDACTED]");
    }

    #[test]
    fn test_redacted_debug() {
        let secret = Redacted::new("super_secret_value");
        assert_eq!(format!("{:?}", secret), "[REDACTED]");
    }

    #[test]
    fn test_redact_stream_key() {
        assert_eq!(
            redact_stream_key("sk_a1b2c3d4e5f6g7h8i9j0k1l2"),
            "sk_a1b***"
        );
        assert_eq!(redact_stream_key("sk_a"), "****");
        assert_eq!(redact_stream_key("abc"), "****");
    }

    #[test]
    fn test_redact_bearer_token() {
        assert_eq!(redact_bearer_token("Bearer at_abc123"), "Bearer [REDACTED]");
        assert_eq!(redact_bearer_token("some_other"), "[REDACTED]");
    }

    #[test]
    fn test_is_sensitive_field() {
        assert!(is_sensitive_field("auth_token"));
        assert!(is_sensitive_field("stream_key"));
        assert!(is_sensitive_field("secret_access_key"));
        assert!(is_sensitive_field("password"));
        assert!(is_sensitive_field("Authorization"));
        assert!(!is_sensitive_field("stream_id"));
        assert!(!is_sensitive_field("status"));
    }
}
