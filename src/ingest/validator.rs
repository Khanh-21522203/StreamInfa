use crate::core::config::IngestConfig;
use crate::core::error::IngestError;
use crate::core::types::{AudioCodec, Container, MediaInfo, StreamId, VideoCodec};

// ---------------------------------------------------------------------------
// Validation constants (from ingest.md §3.4)
// ---------------------------------------------------------------------------

const MAX_RESOLUTION_WIDTH: u32 = 3840;
const MAX_RESOLUTION_HEIGHT: u32 = 2160;

// ---------------------------------------------------------------------------
// Upload validation (from ingest.md §3.4)
// ---------------------------------------------------------------------------

/// Result of probing an uploaded file.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub container: Container,
    pub video_codec: VideoCodec,
    pub video_width: u32,
    pub video_height: u32,
    pub frame_rate: f64,
    pub audio_codec: Option<AudioCodec>,
    pub audio_sample_rate: Option<u32>,
    pub audio_channels: Option<u8>,
    pub duration_secs: Option<f64>,
    pub video_bitrate_kbps: Option<u32>,
    pub audio_bitrate_kbps: Option<u32>,
}

impl ProbeResult {
    /// Convert probe result into MediaInfo.
    pub fn to_media_info(&self, stream_id: StreamId) -> MediaInfo {
        MediaInfo {
            stream_id,
            video_codec: self.video_codec,
            video_width: self.video_width,
            video_height: self.video_height,
            video_bitrate_kbps: self.video_bitrate_kbps,
            frame_rate: self.frame_rate,
            audio_codec: self.audio_codec,
            audio_sample_rate: self.audio_sample_rate,
            audio_channels: self.audio_channels,
            audio_bitrate_kbps: self.audio_bitrate_kbps,
            duration_secs: self.duration_secs,
            container: self.container,
        }
    }
}

/// Validate an uploaded file against the configured rules.
///
/// Checks (from ingest.md §3.4):
/// - Container format: MP4 or MKV
/// - Video codec: H.264
/// - Audio codec: AAC or MP3 (or no audio)
/// - Duration: ≤ max_duration_secs
/// - Resolution: ≤ 3840×2160 and > 0×0
/// - File integrity: must be probeable
pub fn validate_upload(probe: &ProbeResult, config: &IngestConfig) -> Result<(), IngestError> {
    // Container format check
    match probe.container {
        Container::Mp4 | Container::Mkv => {}
        other => {
            return Err(IngestError::UnsupportedFormat {
                format: other.to_string(),
            });
        }
    }

    // Video codec check
    let codec_name = probe.video_codec.to_string();
    if !config.allowed_codecs.iter().any(|c| c == &codec_name) {
        return Err(IngestError::UnsupportedCodec { codec: codec_name });
    }

    // Audio codec check (if audio present)
    if let Some(audio_codec) = &probe.audio_codec {
        let audio_name = audio_codec.to_string();
        if !config.allowed_audio_codecs.iter().any(|c| c == &audio_name) {
            return Err(IngestError::UnsupportedCodec { codec: audio_name });
        }
    }

    // Duration check
    if let Some(duration) = probe.duration_secs {
        if duration > config.max_duration_secs {
            return Err(IngestError::DurationExceeded {
                duration_secs: duration,
                max_secs: config.max_duration_secs,
            });
        }
    }

    // Resolution check
    if probe.video_width == 0 || probe.video_height == 0 {
        return Err(IngestError::InvalidResolution {
            width: probe.video_width,
            height: probe.video_height,
        });
    }
    if probe.video_width > MAX_RESOLUTION_WIDTH || probe.video_height > MAX_RESOLUTION_HEIGHT {
        return Err(IngestError::InvalidResolution {
            width: probe.video_width,
            height: probe.video_height,
        });
    }

    Ok(())
}

/// Validate RTMP stream codec parameters.
pub fn validate_rtmp_codecs(
    video_codec_id: u8,
    audio_sound_format: Option<u8>,
) -> Result<(), IngestError> {
    // Video: must be H.264 (FLV CodecID = 7)
    if video_codec_id != 7 {
        return Err(IngestError::InvalidCodec {
            expected: "h264 (CodecID=7)".to_string(),
            actual: format!("CodecID={}", video_codec_id),
        });
    }

    // Audio: must be AAC (10) or MP3 (2)
    if let Some(fmt) = audio_sound_format {
        if fmt != 10 && fmt != 2 {
            return Err(IngestError::UnsupportedCodec {
                codec: format!("SoundFormat={}", fmt),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> IngestConfig {
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

    fn valid_probe() -> ProbeResult {
        ProbeResult {
            container: Container::Mp4,
            video_codec: VideoCodec::H264,
            video_width: 1920,
            video_height: 1080,
            frame_rate: 30.0,
            audio_codec: Some(AudioCodec::Aac),
            audio_sample_rate: Some(44100),
            audio_channels: Some(2),
            duration_secs: Some(600.0),
            video_bitrate_kbps: Some(3500),
            audio_bitrate_kbps: Some(128),
        }
    }

    #[test]
    fn test_valid_upload() {
        let config = default_config();
        let probe = valid_probe();
        assert!(validate_upload(&probe, &config).is_ok());
    }

    #[test]
    fn test_unsupported_container() {
        let config = default_config();
        let mut probe = valid_probe();
        probe.container = Container::Flv;
        assert!(matches!(
            validate_upload(&probe, &config),
            Err(IngestError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn test_duration_exceeded() {
        let config = default_config();
        let mut probe = valid_probe();
        probe.duration_secs = Some(30000.0);
        assert!(matches!(
            validate_upload(&probe, &config),
            Err(IngestError::DurationExceeded { .. })
        ));
    }

    #[test]
    fn test_invalid_resolution_zero() {
        let config = default_config();
        let mut probe = valid_probe();
        probe.video_width = 0;
        assert!(matches!(
            validate_upload(&probe, &config),
            Err(IngestError::InvalidResolution { .. })
        ));
    }

    #[test]
    fn test_invalid_resolution_too_large() {
        let config = default_config();
        let mut probe = valid_probe();
        probe.video_width = 7680;
        probe.video_height = 4320;
        assert!(matches!(
            validate_upload(&probe, &config),
            Err(IngestError::InvalidResolution { .. })
        ));
    }

    #[test]
    fn test_no_audio_is_valid() {
        let config = default_config();
        let mut probe = valid_probe();
        probe.audio_codec = None;
        assert!(validate_upload(&probe, &config).is_ok());
    }

    #[test]
    fn test_mkv_container_valid() {
        let config = default_config();
        let mut probe = valid_probe();
        probe.container = Container::Mkv;
        assert!(validate_upload(&probe, &config).is_ok());
    }

    #[test]
    fn test_rtmp_codec_validation() {
        assert!(validate_rtmp_codecs(7, Some(10)).is_ok());
        assert!(validate_rtmp_codecs(7, Some(2)).is_ok());
        assert!(validate_rtmp_codecs(7, None).is_ok());
        assert!(validate_rtmp_codecs(4, Some(10)).is_err());
        assert!(validate_rtmp_codecs(7, Some(5)).is_err());
    }
}
