use thiserror::Error;

use super::types::StreamId;

// ---------------------------------------------------------------------------
// Ingest errors (from ingest.md §2.6, §3.2, §3.4)
// ---------------------------------------------------------------------------

/// Errors originating from the ingest module.
#[derive(Debug, Error)]
pub enum IngestError {
    // -- RTMP-specific errors --
    #[error("RTMP handshake timeout after {timeout_secs}s")]
    HandshakeTimeout { timeout_secs: u64 },

    #[error("invalid RTMP version: expected 3, got {version}")]
    InvalidRtmpVersion { version: u8 },

    #[error("malformed AMF data: {reason}")]
    MalformedAmf { reason: String },

    #[error("RTMP connection reset for stream {stream_id}")]
    ConnectionReset { stream_id: StreamId },

    #[error("missing AVC sequence header within {timeout_secs}s")]
    MissingSequenceHeader { timeout_secs: u64 },

    #[error("corrupt NALU: {reason}")]
    CorruptNalu { reason: String },

    // -- Shared ingest errors --
    #[error("invalid codec detected: expected {expected}, got {actual}")]
    InvalidCodec { expected: String, actual: String },

    #[error("unsupported codec: {codec}")]
    UnsupportedCodec { codec: String },

    #[error("authentication failed: {reason}")]
    AuthFailed { reason: String },

    #[error("unsupported container format: {format}")]
    UnsupportedFormat { format: String },

    // -- HTTP upload errors --
    #[error("upload too large: {size_bytes} bytes exceeds limit {max_bytes} bytes")]
    UploadTooLarge { size_bytes: u64, max_bytes: u64 },

    #[error("invalid resolution: {width}x{height}")]
    InvalidResolution { width: u32, height: u32 },

    #[error("duration exceeded: {duration_secs}s exceeds limit {max_secs}s")]
    DurationExceeded { duration_secs: f64, max_secs: f64 },

    #[error("corrupt file: {reason}")]
    CorruptFile { reason: String },

    // -- General --
    #[error("validation failed: {reason}")]
    ValidationFailed { reason: String },

    #[error("ingest I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Transcode errors (from transcoding-and-packaging.md §3.2, §4, §10)
// ---------------------------------------------------------------------------

/// Errors originating from the transcode module.
#[derive(Debug, Error)]
pub enum TranscodeError {
    #[error("FFmpeg initialization failed: {reason}")]
    FfmpegInit { reason: String },

    #[error("transcode pipeline cancelled for stream {stream_id}")]
    Cancelled { stream_id: StreamId },

    #[error("consecutive decode errors ({count}) for stream {stream_id}")]
    ConsecutiveDecodeErrors { stream_id: StreamId, count: u32 },
}

// ---------------------------------------------------------------------------
// Package errors (from transcoding-and-packaging.md §6, §10)
// ---------------------------------------------------------------------------

/// Errors originating from the packaging module.
#[derive(Debug, Error)]
pub enum PackageError {
    #[error("failed to generate init segment: {reason}")]
    InitSegmentFailed { reason: String },

    #[error("failed to generate media segment: {reason}")]
    MediaSegmentFailed { reason: String },

    #[error("invalid fMP4 box structure: {reason}")]
    InvalidBoxStructure { reason: String },
}

// ---------------------------------------------------------------------------
// Storage errors
// ---------------------------------------------------------------------------

/// Errors originating from the storage module.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("S3 PUT failed for path {path}: {reason}")]
    S3PutFailed { path: String, reason: String },

    #[error("S3 GET failed for path {path}: {reason}")]
    S3GetFailed { path: String, reason: String },

    #[error("S3 DELETE failed for path {path}: {reason}")]
    S3DeleteFailed { path: String, reason: String },

    #[error("retries exhausted for path {path}")]
    RetriesExhausted { path: String },

    #[error("storage I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// State errors
// ---------------------------------------------------------------------------

/// Errors related to stream state transitions.
#[derive(Debug, Error)]
pub enum StateError {
    #[error("invalid state transition from {from} to {to} for stream {stream_id}")]
    InvalidTransition {
        stream_id: StreamId,
        from: String,
        to: String,
    },

    #[error("stream not found: {stream_id}")]
    StreamNotFound { stream_id: StreamId },
}
