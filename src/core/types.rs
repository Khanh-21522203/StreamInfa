use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Unique identifier for a stream (UUIDv7 for time-sortability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamId(Uuid);

impl StreamId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl Default for StreamId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifies a rendition in the transcoding profile ladder.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RenditionId {
    High,
    Medium,
    Low,
    /// Custom rendition at source resolution (for inputs below 480p).
    Source,
}

impl RenditionId {
    pub fn as_str(&self) -> &'static str {
        match self {
            RenditionId::High => "high",
            RenditionId::Medium => "medium",
            RenditionId::Low => "low",
            RenditionId::Source => "source",
        }
    }
}

impl fmt::Display for RenditionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Codec and container enums (from ingest.md §4.1)
// ---------------------------------------------------------------------------

/// Supported video codecs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
}

impl fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoCodec::H264 => write!(f, "h264"),
        }
    }
}

/// Supported audio codecs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioCodec {
    Aac,
    Mp3,
}

impl fmt::Display for AudioCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioCodec::Aac => write!(f, "aac"),
            AudioCodec::Mp3 => write!(f, "mp3"),
        }
    }
}

/// Container format detected at ingest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Container {
    Flv,
    Mp4,
    Mkv,
}

impl fmt::Display for Container {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Container::Flv => write!(f, "flv"),
            Container::Mp4 => write!(f, "mp4"),
            Container::Mkv => write!(f, "mkv"),
        }
    }
}

// ---------------------------------------------------------------------------
// Track types (from ingest.md §4.1 — richer variant with codec metadata)
// ---------------------------------------------------------------------------

/// Track type within a media stream, carrying codec-specific metadata.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Track {
    Video {
        codec: VideoCodec,
        width: u32,
        height: u32,
    },
    Audio {
        codec: AudioCodec,
        sample_rate: u32,
        channels: u8,
    },
}

impl Track {
    pub fn is_audio(&self) -> bool {
        matches!(self, Track::Audio { .. })
    }
}

impl fmt::Display for Track {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Track::Video {
                codec,
                width,
                height,
            } => {
                write!(f, "video({}, {}x{})", codec, width, height)
            }
            Track::Audio {
                codec,
                sample_rate,
                channels,
            } => {
                write!(f, "audio({}, {}Hz, {}ch)", codec, sample_rate, channels)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Media info (from ingest.md §4.3 — richer version)
// ---------------------------------------------------------------------------

/// Detected media information from the ingest source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInfo {
    pub stream_id: StreamId,
    pub video_codec: VideoCodec,
    pub video_width: u32,
    pub video_height: u32,
    pub video_bitrate_kbps: Option<u32>,
    pub frame_rate: f64,
    pub audio_codec: Option<AudioCodec>,
    pub audio_sample_rate: Option<u32>,
    pub audio_channels: Option<u8>,
    pub audio_bitrate_kbps: Option<u32>,
    pub duration_secs: Option<f64>,
    pub container: Container,
}

// ---------------------------------------------------------------------------
// Ingest mode and stream metadata
// ---------------------------------------------------------------------------

/// The mode of ingestion for a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IngestMode {
    Live,
    Vod,
}

impl fmt::Display for IngestMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IngestMode::Live => write!(f, "live"),
            IngestMode::Vod => write!(f, "vod"),
        }
    }
}

/// Metadata associated with a stream throughout its lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamMetadata {
    pub stream_id: StreamId,
    pub ingest_mode: IngestMode,
    pub media_info: Option<MediaInfo>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl StreamMetadata {
    pub fn new(stream_id: StreamId, ingest_mode: IngestMode) -> Self {
        Self {
            stream_id,
            ingest_mode,
            media_info: None,
            created_at: Utc::now(),
            started_at: None,
            ended_at: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline message types
// ---------------------------------------------------------------------------

/// A demuxed frame extracted from the ingest source.
/// Sent from Ingest → Transcode via bounded mpsc channel (capacity: 64).
#[derive(Debug, Clone)]
pub struct DemuxedFrame {
    pub stream_id: StreamId,
    pub track: Track,
    /// Presentation timestamp in 90 kHz timebase.
    pub pts: i64,
    /// Decode timestamp in 90 kHz timebase.
    pub dts: i64,
    /// True if this is an IDR keyframe (video only).
    pub keyframe: bool,
    /// Raw NALU data (video) or raw AAC/MP3 frame (audio).
    pub data: Bytes,
}

/// An encoded segment produced by the transcode pipeline.
/// Sent from Transcode → Packager via bounded mpsc channel (capacity: 32).
#[derive(Debug, Clone)]
pub struct EncodedSegment {
    pub _stream_id: StreamId,
    pub rendition: RenditionId,
    /// 0-indexed, monotonically increasing per rendition.
    pub sequence: u64,
    /// Actual segment duration in seconds.
    pub duration_secs: f64,
    /// PTS of the first frame in the segment (90 kHz timebase).
    pub pts_start: i64,
    /// Encoded H.264 NALUs for this segment.
    pub video_data: Bytes,
    /// Encoded AAC frames for this segment (None if no audio).
    pub audio_data: Option<Bytes>,
    /// True if this is the final segment (stream ended).
    pub is_last: bool,
    /// Rendition output width (from profile ladder selection).
    pub width: u32,
    /// Rendition output height (from profile ladder selection).
    pub height: u32,
    /// Video bitrate in kbps for this rendition.
    pub video_bitrate_kbps: u32,
    /// Audio bitrate in kbps for this rendition.
    pub audio_bitrate_kbps: u32,
    /// H.264 profile name (e.g. "high", "main").
    pub profile: String,
    /// H.264 level (e.g. "4.1", "3.1").
    pub level: String,
    /// Source frame rate.
    pub frame_rate: f64,
}

/// A storage write request.
/// Sent from Packager → Storage via bounded mpsc channel (capacity: 32).
#[derive(Debug, Clone)]
pub struct StorageWrite {
    pub path: String,
    pub data: Bytes,
    pub content_type: String,
}

// ---------------------------------------------------------------------------
// Channel capacity constants (from architecture overview)
// ---------------------------------------------------------------------------

/// Bounded channel capacity: Ingest → Transcode.
pub const INGEST_TRANSCODE_CHANNEL_CAP: usize = 64;

/// Bounded channel capacity: Transcode → Packager.
pub const TRANSCODE_PACKAGER_CHANNEL_CAP: usize = 32;

/// Bounded channel capacity: Packager → Storage.
pub const PACKAGER_STORAGE_CHANNEL_CAP: usize = 32;
