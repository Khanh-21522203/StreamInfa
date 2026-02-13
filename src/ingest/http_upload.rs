use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tracing::{info, warn};

use crate::control::state::{StreamState, StreamStateManager};
use crate::core::auth::AuthProvider;
use crate::core::config::IngestConfig;
use crate::core::error::IngestError;
use crate::core::security;
use crate::core::types::{IngestMode, StreamId};

use super::validator::{self, ProbeResult};

// ---------------------------------------------------------------------------
// Upload API types (from ingest.md §3.2)
// ---------------------------------------------------------------------------

/// Successful upload response (from ingest.md §3.2).
#[derive(Debug, Clone, Serialize)]
pub struct UploadResponse {
    pub stream_id: String,
    pub status: String,
    pub upload_size_bytes: u64,
    pub detected_codec: String,
    pub detected_audio: Option<String>,
    pub duration_secs: Option<f64>,
    pub resolution: String,
}

/// Error response for upload failures.
#[derive(Debug, Clone, Serialize)]
pub struct UploadErrorResponse {
    pub error: String,
    pub message: String,
    pub stream_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Upload handler (from ingest.md §3.3)
// ---------------------------------------------------------------------------

/// HTTP upload handler for VOD ingest.
///
/// Processing flow (from ingest.md §3.3):
/// 1. Auth (Bearer token) → 401 on failure
/// 2. Generate stream_id (UUIDv7)
/// 3. Stream body to temp file: /tmp/streaminfa/uploads/{stream_id}.tmp
/// 4. Probe temp file with FFmpeg
/// 5. Validate (container, codecs, duration, resolution)
/// 6. Create stream record (state: PROCESSING)
/// 7. Submit transcode job (async)
/// 8. Return 201 with stream metadata
pub struct HttpUploadHandler {
    config: IngestConfig,
    state_manager: Arc<StreamStateManager>,
    upload_dir: PathBuf,
}

impl HttpUploadHandler {
    pub fn new(
        config: IngestConfig,
        _auth: Arc<AuthProvider>,
        state_manager: Arc<StreamStateManager>,
    ) -> Self {
        let upload_dir = PathBuf::from("/tmp/streaminfa/uploads");
        Self {
            config,
            state_manager,
            upload_dir,
        }
    }

    /// Ensure the upload temp directory exists.
    pub async fn ensure_upload_dir(&self) -> Result<(), IngestError> {
        tokio::fs::create_dir_all(&self.upload_dir).await?;
        Ok(())
    }

    /// Get the temp file path for a given stream ID.
    pub fn temp_file_path(&self, stream_id: StreamId) -> PathBuf {
        self.upload_dir.join(format!("{}.tmp", stream_id))
    }

    /// Process a completed upload file.
    ///
    /// This is called after the file has been fully written to disk.
    /// It probes, validates, creates the stream record, and returns
    /// the response to send to the client.
    pub async fn process_upload(
        &self,
        stream_id: StreamId,
        file_path: &Path,
        file_size: u64,
    ) -> Result<UploadResponse, IngestError> {
        // Check file size
        if file_size > self.config.max_upload_size_bytes {
            self.cleanup_temp_file(file_path).await;
            return Err(IngestError::UploadTooLarge {
                size_bytes: file_size,
                max_bytes: self.config.max_upload_size_bytes,
            });
        }

        // Probe the file (placeholder — real impl would use FFmpeg FFI)
        let probe = self.probe_file(file_path).await?;

        // Validate against config rules
        if let Err(e) = validator::validate_upload(&probe, &self.config) {
            self.cleanup_temp_file(file_path).await;
            return Err(e);
        }

        // Create stream record
        let _media_info = probe.to_media_info(stream_id);
        let mut _entry = self
            .state_manager
            .create_stream(stream_id, IngestMode::Vod)
            .await;

        // Transition to PROCESSING
        _entry = self
            .state_manager
            .transition(stream_id, StreamState::Processing)
            .await
            .map_err(|e| IngestError::ValidationFailed {
                reason: e.to_string(),
            })?;

        info!(
            %stream_id,
            file_size,
            codec = %probe.video_codec,
            resolution = format!("{}x{}", probe.video_width, probe.video_height),
            "upload accepted, processing started"
        );

        Ok(UploadResponse {
            stream_id: stream_id.to_string(),
            status: "processing".to_string(),
            upload_size_bytes: file_size,
            detected_codec: probe.video_codec.to_string(),
            detected_audio: probe.audio_codec.map(|c| c.to_string()),
            duration_secs: probe.duration_secs,
            resolution: format!("{}x{}", probe.video_width, probe.video_height),
        })
    }

    /// Probe a file to detect its media properties.
    ///
    /// In a real implementation, this would use FFmpeg FFI:
    /// `ffmpeg::format::input(&path)` to extract container, codecs, duration, resolution.
    ///
    /// TODO: Replace with actual FFmpeg probe via ffmpeg-next crate.
    /// For now, this returns a placeholder result based on file header detection.
    /// The container is detected from magic bytes; codec/resolution are assumed defaults.
    async fn probe_file(&self, file_path: &Path) -> Result<ProbeResult, IngestError> {
        use crate::core::types::{AudioCodec, Container, VideoCodec};

        // Verify the file exists and is readable
        let metadata =
            tokio::fs::metadata(file_path)
                .await
                .map_err(|e| IngestError::CorruptFile {
                    reason: format!("cannot read file: {}", e),
                })?;

        if metadata.len() == 0 {
            return Err(IngestError::CorruptFile {
                reason: "file is empty".to_string(),
            });
        }

        // Validate probe size against security limit (security.md §4.4)
        if metadata.len() as usize > security::FFMPEG_PROBE_MAX_SIZE {
            return Err(IngestError::CorruptFile {
                reason: format!(
                    "file size {} exceeds probe limit {}",
                    metadata.len(),
                    security::FFMPEG_PROBE_MAX_SIZE
                ),
            });
        }

        // Read first 12 bytes to detect container format from magic bytes
        let header = tokio::fs::read(file_path)
            .await
            .map_err(|e| IngestError::CorruptFile {
                reason: format!("cannot read file header: {}", e),
            })?;

        let container = if header.len() >= 12 && &header[4..8] == b"ftyp" {
            Container::Mp4
        } else if header.len() >= 4 && &header[0..4] == &[0x1A, 0x45, 0xDF, 0xA3] {
            Container::Mkv
        } else {
            return Err(IngestError::UnsupportedFormat {
                format: "unknown (could not detect container from magic bytes)".to_string(),
            });
        };

        // TODO: Replace these placeholder values with actual FFmpeg probe results.
        // For now, assume H.264 + AAC at 1080p. Real implementation would call:
        //   let input = ffmpeg::format::input(file_path)?;
        //   ... extract streams, codecs, duration, resolution ...
        warn!(
            path = %file_path.display(),
            "using placeholder probe results (FFmpeg FFI not yet integrated)"
        );

        Ok(ProbeResult {
            container,
            video_codec: VideoCodec::H264,
            video_width: 1920,
            video_height: 1080,
            frame_rate: 30.0,
            audio_codec: Some(AudioCodec::Aac),
            audio_sample_rate: Some(48000),
            audio_channels: Some(2),
            duration_secs: None,
            video_bitrate_kbps: None,
            audio_bitrate_kbps: Some(128),
        })
    }

    /// Clean up a temp file after validation failure or processing.
    pub async fn cleanup_temp_file(&self, file_path: &Path) {
        if let Err(e) = tokio::fs::remove_file(file_path).await {
            warn!(path = %file_path.display(), error = %e, "failed to clean up temp file");
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP status code mapping (from ingest.md §3.2)
// ---------------------------------------------------------------------------

/// Map an IngestError to an HTTP status code.
pub fn ingest_error_to_status_code(err: &IngestError) -> u16 {
    match err {
        IngestError::AuthFailed { .. } => 401,
        IngestError::UploadTooLarge { .. } => 413,
        IngestError::UnsupportedFormat { .. } => 415,
        IngestError::UnsupportedCodec { .. }
        | IngestError::InvalidResolution { .. }
        | IngestError::DurationExceeded { .. }
        | IngestError::CorruptFile { .. } => 422,
        _ => 500,
    }
}

/// Build an error response body from an IngestError.
pub fn build_error_response(err: &IngestError, stream_id: Option<StreamId>) -> UploadErrorResponse {
    let error_code = match err {
        IngestError::UnsupportedCodec { .. } => "unsupported_codec",
        IngestError::UnsupportedFormat { .. } => "unsupported_format",
        IngestError::InvalidResolution { .. } => "invalid_resolution",
        IngestError::DurationExceeded { .. } => "duration_exceeded",
        IngestError::CorruptFile { .. } => "corrupt_file",
        IngestError::UploadTooLarge { .. } => "payload_too_large",
        IngestError::AuthFailed { .. } => "unauthorized",
        _ => "internal_error",
    };

    UploadErrorResponse {
        error: error_code.to_string(),
        message: err.to_string(),
        stream_id: stream_id.map(|id| id.to_string()),
    }
}
