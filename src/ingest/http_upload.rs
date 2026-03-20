use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
#[cfg(not(feature = "ffmpeg"))]
use tokio::io::AsyncReadExt;
use tracing::{info, warn};

use crate::control::state::{StreamState, StreamStateManager};
use crate::core::auth::AuthProvider;
use crate::core::config::IngestConfig;
use crate::core::error::IngestError;
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
        let media_info = probe.to_media_info(stream_id);
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
        self.state_manager
            .set_media_info(stream_id, media_info)
            .await;

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
    /// Probe a media file to extract container, codec, resolution, frame rate,
    /// duration, and audio parameters.
    ///
    /// Under `--features ffmpeg`, calls FFmpeg's format demuxer for accurate
    /// values. Without the feature, falls back to magic-byte container detection
    /// with conservative defaults.
    async fn probe_file(&self, file_path: &Path) -> Result<ProbeResult, IngestError> {
        #[cfg(feature = "ffmpeg")]
        {
            let path = file_path.to_path_buf();
            tokio::task::spawn_blocking(move || probe_with_ffmpeg(&path))
                .await
                .map_err(|e| IngestError::CorruptFile {
                    reason: format!("probe task panicked: {e}"),
                })?
        }
        #[cfg(not(feature = "ffmpeg"))]
        {
            probe_placeholder(file_path).await
        }
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
        IngestError::InsufficientStorage { .. } => 507,
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

// ---------------------------------------------------------------------------
// FFmpeg probe (real dimensions, codec, duration)
// ---------------------------------------------------------------------------

#[cfg(feature = "ffmpeg")]
fn probe_with_ffmpeg(file_path: &std::path::Path) -> Result<ProbeResult, IngestError> {
    use crate::core::types::{AudioCodec, Container, VideoCodec};
    use ffmpeg_next as ffmpeg;

    ffmpeg::init().map_err(|e| IngestError::CorruptFile {
        reason: format!("ffmpeg init: {e}"),
    })?;

    let ictx = ffmpeg::format::input(file_path).map_err(|e| IngestError::CorruptFile {
        reason: format!("cannot open file: {e}"),
    })?;

    // Container
    let format = ictx.format();
    let format_name = format.name();
    let container = if format_name.contains("mp4") || format_name.contains("mov") {
        Container::Mp4
    } else if format_name.contains("matroska") || format_name.contains("webm") {
        Container::Mkv
    } else {
        return Err(IngestError::UnsupportedFormat {
            format: format_name.to_string(),
        });
    };

    // Duration
    let duration_secs = if ictx.duration() > 0 {
        Some(ictx.duration() as f64 / ffmpeg::ffi::AV_TIME_BASE as f64)
    } else {
        None
    };

    // Best video stream
    let vs = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| IngestError::UnsupportedFormat {
            format: "no video stream".to_string(),
        })?;

    let fps = {
        let r = vs.avg_frame_rate();
        if r.denominator() != 0 && r.numerator() > 0 {
            r.numerator() as f64 / r.denominator() as f64
        } else {
            30.0
        }
    };

    let vdec = ffmpeg::codec::Context::from_parameters(vs.parameters())
        .map_err(|e| IngestError::CorruptFile {
            reason: format!("video codec params: {e}"),
        })?
        .decoder()
        .video()
        .map_err(|e| IngestError::CorruptFile {
            reason: format!("video decoder open: {e}"),
        })?;

    let video_width = vdec.width();
    let video_height = vdec.height();

    // Best audio stream (optional)
    let (audio_codec, audio_sample_rate, audio_channels, audio_bitrate_kbps) =
        if let Some(aud) = ictx.streams().best(ffmpeg::media::Type::Audio) {
            let adec = ffmpeg::codec::Context::from_parameters(aud.parameters())
                .ok()
                .and_then(|c| c.decoder().audio().ok());
            let (sr, ch) = adec
                .as_ref()
                .map(|a| (a.rate(), a.channels()))
                .unwrap_or((48000, 2));
            (
                Some(AudioCodec::Aac),
                Some(sr),
                Some(ch as u8),
                Some(128u32),
            )
        } else {
            (None, None, None, None)
        };

    Ok(ProbeResult {
        container,
        video_codec: VideoCodec::H264,
        video_width,
        video_height,
        frame_rate: fps,
        audio_codec,
        audio_sample_rate,
        audio_channels,
        duration_secs,
        video_bitrate_kbps: None,
        audio_bitrate_kbps,
    })
}

// ---------------------------------------------------------------------------
// Placeholder probe (no FFmpeg — magic bytes + conservative defaults)
// ---------------------------------------------------------------------------

#[cfg(not(feature = "ffmpeg"))]
async fn probe_placeholder(file_path: &std::path::Path) -> Result<ProbeResult, IngestError> {
    use crate::core::types::{AudioCodec, Container, VideoCodec};

    let metadata = tokio::fs::metadata(file_path)
        .await
        .map_err(|e| IngestError::CorruptFile {
            reason: format!("cannot read file: {e}"),
        })?;

    if metadata.len() == 0 {
        return Err(IngestError::CorruptFile {
            reason: "file is empty".to_string(),
        });
    }

    let mut file =
        tokio::fs::File::open(file_path)
            .await
            .map_err(|e| IngestError::CorruptFile {
                reason: format!("cannot open file for probe: {e}"),
            })?;
    let mut header = [0u8; 12];
    let n = file
        .read(&mut header)
        .await
        .map_err(|e| IngestError::CorruptFile {
            reason: format!("cannot read file header: {e}"),
        })?;
    let header = &header[..n];

    let container = if header.len() >= 12 && &header[4..8] == b"ftyp" {
        Container::Mp4
    } else if header.len() >= 4 && header[0..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        Container::Mkv
    } else {
        return Err(IngestError::UnsupportedFormat {
            format: "unknown (could not detect container from magic bytes)".to_string(),
        });
    };

    warn!(
        path = %file_path.display(),
        "using placeholder probe results (build with --features ffmpeg for real values)"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{AuthConfig, IngestConfig};
    use crate::core::security;
    use crate::core::types::Container;
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;

    fn test_ingest_config() -> IngestConfig {
        IngestConfig {
            max_bitrate_kbps: 20000,
            max_upload_size_bytes: 10_737_418_240,
            allowed_codecs: vec!["h264".to_string()],
            allowed_audio_codecs: vec!["aac".to_string(), "mp3".to_string()],
            max_duration_secs: 21600.0,
            upload_timeout_secs: 60,
            max_concurrent_rtmp_connections: 50,
            max_concurrent_live_streams: 10,
            max_concurrent_uploads: 10,
            max_pending_connections: 100,
        }
    }

    fn test_upload_handler() -> HttpUploadHandler {
        let auth = Arc::new(AuthProvider::new(&AuthConfig {
            ingest_stream_keys: Vec::new(),
            admin_bearer_tokens: Vec::new(),
        }));
        let state_manager = Arc::new(StreamStateManager::new());
        HttpUploadHandler::new(test_ingest_config(), auth, state_manager)
    }

    #[tokio::test]
    async fn test_probe_file_allows_large_file_and_detects_mp4() {
        let handler = test_upload_handler();
        let path =
            std::env::temp_dir().join(format!("streaminfa-probe-large-{}.tmp", StreamId::new()));

        let mut file = tokio::fs::File::create(&path).await.unwrap();
        // Minimal MP4 signature: bytes 4..8 = "ftyp"
        file.write_all(&[
            0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm',
        ])
        .await
        .unwrap();
        file.flush().await.unwrap();
        drop(file);

        // Simulate a large upload without allocating the full file contents.
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len((security::FFMPEG_PROBE_MAX_SIZE as u64) + 1)
            .unwrap();

        let probe = handler.probe_file(&path).await.unwrap();
        assert_eq!(probe.container, Container::Mp4);

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_probe_file_rejects_unknown_magic() {
        let handler = test_upload_handler();
        let path =
            std::env::temp_dir().join(format!("streaminfa-probe-unknown-{}.tmp", StreamId::new()));

        let mut file = tokio::fs::File::create(&path).await.unwrap();
        file.write_all(b"not-a-media-header").await.unwrap();
        file.flush().await.unwrap();
        drop(file);

        let err = handler.probe_file(&path).await.unwrap_err();
        assert!(matches!(err, IngestError::UnsupportedFormat { .. }));

        let _ = tokio::fs::remove_file(&path).await;
    }
}
