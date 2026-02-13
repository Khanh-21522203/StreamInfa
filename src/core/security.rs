// ---------------------------------------------------------------------------
// Security constants (from security.md §4)
// ---------------------------------------------------------------------------

// -- RTMP input validation limits (from security.md §4.1) --

/// Maximum RTMP chunk size in bytes. Prevents memory exhaustion.
pub const MAX_RTMP_CHUNK_SIZE: usize = 65536;

/// Maximum AMF string length in bytes. Prevents oversized AMF parsing.
pub const MAX_AMF_STRING_LENGTH: usize = 4096;

/// Maximum FLV tag size in bytes (16 MB). Configurable via SecurityConfig.
pub const MAX_FLV_TAG_SIZE: usize = 16 * 1024 * 1024;

/// Maximum video resolution (width or height).
pub const MAX_RESOLUTION_WIDTH: u32 = 3840;
pub const MAX_RESOLUTION_HEIGHT: u32 = 2160;

/// Minimum video resolution (width or height). Must be > 0.
pub const MIN_RESOLUTION: u32 = 1;

// -- API input validation limits (from security.md §4.3) --

/// Maximum metadata key length in characters.
pub const MAX_METADATA_KEY_LENGTH: usize = 64;

/// Maximum metadata value length in characters.
pub const MAX_METADATA_VALUE_LENGTH: usize = 1024;

/// Maximum number of metadata key-value pairs per stream.
pub const MAX_METADATA_ENTRIES: usize = 50;

/// Maximum `limit` query parameter value for list endpoints.
pub const MAX_LIST_LIMIT: usize = 100;

/// Default `limit` query parameter value for list endpoints.
pub const DEFAULT_LIST_LIMIT: usize = 10;

// -- Brute-force protection (from security.md §2.1) --

/// Maximum failed auth attempts from one IP before temporary block.
pub const BRUTE_FORCE_MAX_ATTEMPTS: u32 = 5;

/// Time window (seconds) for counting failed auth attempts.
pub const BRUTE_FORCE_WINDOW_SECS: u64 = 60;

/// Duration (seconds) an IP is blocked after exceeding max attempts.
pub const BRUTE_FORCE_BLOCK_SECS: u64 = 300;

// -- FFmpeg probe safety (from security.md §4.4) --

/// Maximum probe size in bytes (10 MB).
pub const FFMPEG_PROBE_MAX_SIZE: usize = 10 * 1024 * 1024;

/// Maximum analyze duration in seconds.
pub const FFMPEG_PROBE_MAX_DURATION_SECS: u64 = 10;

/// Timeout for FFmpeg probe operation in seconds.
pub const FFMPEG_PROBE_TIMEOUT_SECS: u64 = 30;

// -- Stream key format (from security.md §2.1) --

/// Stream key prefix.
pub const STREAM_KEY_PREFIX: &str = "sk_";

/// Stream key random part length (24 alphanumeric characters).
pub const STREAM_KEY_RANDOM_LENGTH: usize = 24;

/// Validate that a resolution is within allowed bounds.
///
/// From security.md §4.1: Width and height must be > 0 and ≤ 3840×2160.
pub fn validate_resolution(width: u32, height: u32) -> Result<(), String> {
    if width < MIN_RESOLUTION || height < MIN_RESOLUTION {
        return Err(format!(
            "resolution {}x{} is below minimum ({}x{})",
            width, height, MIN_RESOLUTION, MIN_RESOLUTION
        ));
    }
    if width > MAX_RESOLUTION_WIDTH || height > MAX_RESOLUTION_HEIGHT {
        return Err(format!(
            "resolution {}x{} exceeds maximum ({}x{})",
            width, height, MAX_RESOLUTION_WIDTH, MAX_RESOLUTION_HEIGHT
        ));
    }
    Ok(())
}

/// Validate metadata key-value pairs.
///
/// From security.md §4.3: String values only, keys ≤ 64 chars, values ≤ 1024 chars.
pub fn validate_metadata(
    metadata: &std::collections::HashMap<String, String>,
) -> Result<(), String> {
    if metadata.len() > MAX_METADATA_ENTRIES {
        return Err(format!(
            "too many metadata entries: {} (max {})",
            metadata.len(),
            MAX_METADATA_ENTRIES
        ));
    }
    for (key, value) in metadata {
        if key.len() > MAX_METADATA_KEY_LENGTH {
            return Err(format!(
                "metadata key '{}...' exceeds max length {} chars",
                &key[..32.min(key.len())],
                MAX_METADATA_KEY_LENGTH
            ));
        }
        if value.len() > MAX_METADATA_VALUE_LENGTH {
            return Err(format!(
                "metadata value for key '{}' exceeds max length {} chars",
                key, MAX_METADATA_VALUE_LENGTH
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_valid_resolution() {
        assert!(validate_resolution(1920, 1080).is_ok());
        assert!(validate_resolution(1, 1).is_ok());
        assert!(validate_resolution(3840, 2160).is_ok());
    }

    #[test]
    fn test_invalid_resolution_zero() {
        assert!(validate_resolution(0, 1080).is_err());
        assert!(validate_resolution(1920, 0).is_err());
    }

    #[test]
    fn test_invalid_resolution_too_large() {
        assert!(validate_resolution(3841, 2160).is_err());
        assert!(validate_resolution(3840, 2161).is_err());
    }

    #[test]
    fn test_valid_metadata() {
        let mut meta = HashMap::new();
        meta.insert("title".to_string(), "My Stream".to_string());
        meta.insert("category".to_string(), "gaming".to_string());
        assert!(validate_metadata(&meta).is_ok());
    }

    #[test]
    fn test_metadata_key_too_long() {
        let mut meta = HashMap::new();
        meta.insert("k".repeat(65), "value".to_string());
        assert!(validate_metadata(&meta).is_err());
    }

    #[test]
    fn test_metadata_value_too_long() {
        let mut meta = HashMap::new();
        meta.insert("key".to_string(), "v".repeat(1025));
        assert!(validate_metadata(&meta).is_err());
    }

    #[test]
    fn test_metadata_too_many_entries() {
        let mut meta = HashMap::new();
        for i in 0..51 {
            meta.insert(format!("key_{}", i), "value".to_string());
        }
        assert!(validate_metadata(&meta).is_err());
    }
}
