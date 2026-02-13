use chrono::{DateTime, Utc};

use crate::core::types::RenditionId;
use crate::transcode::profile;

// ---------------------------------------------------------------------------
// HLS manifest generation (from transcoding-and-packaging.md §7)
// ---------------------------------------------------------------------------

/// Information about a rendition for the multivariant playlist.
#[derive(Debug, Clone)]
pub struct RenditionInfo {
    pub id: RenditionId,
    pub width: u32,
    pub height: u32,
    pub video_bitrate_kbps: u32,
    pub audio_bitrate_kbps: u32,
    pub profile: String,
    pub level: String,
    pub frame_rate: f64,
    pub has_audio: bool,
}

/// A segment entry for the media playlist.
#[derive(Debug, Clone)]
pub struct PlaylistSegment {
    pub sequence: u64,
    pub duration_secs: f64,
    pub filename: String,
    pub program_date_time: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Multivariant playlist (from transcoding-and-packaging.md §7.1)
// ---------------------------------------------------------------------------

/// Generate a multivariant (master) playlist.
///
/// Format (from transcoding-and-packaging.md §7.1):
/// ```m3u8
/// #EXTM3U
/// #EXT-X-VERSION:7
///
/// #EXT-X-STREAM-INF:BANDWIDTH=...,RESOLUTION=...,CODECS="...",FRAME-RATE=...
/// {rendition}/media.m3u8
/// ```
///
/// BANDWIDTH = (video_bitrate + audio_bitrate) × 1.1 × 1000
/// CODECS = "avc1.PPCCLL,mp4a.40.2"
pub fn generate_multivariant_playlist(renditions: &[RenditionInfo], hls_version: u32) -> String {
    let mut playlist = String::with_capacity(1024);

    playlist.push_str("#EXTM3U\n");
    playlist.push_str(&format!("#EXT-X-VERSION:{}\n", hls_version));
    playlist.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
    playlist.push('\n');

    for rendition in renditions {
        let bandwidth =
            profile::bandwidth(rendition.video_bitrate_kbps, rendition.audio_bitrate_kbps);
        let codecs =
            profile::codecs_attribute(&rendition.profile, &rendition.level, rendition.has_audio);

        playlist.push_str(&format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={},RESOLUTION={}x{},CODECS=\"{}\",FRAME-RATE={:.3}\n",
            bandwidth, rendition.width, rendition.height, codecs, rendition.frame_rate,
        ));
        playlist.push_str(&format!("{}/media.m3u8\n", rendition.id));
        playlist.push('\n');
    }

    playlist
}

// ---------------------------------------------------------------------------
// Media playlist (from transcoding-and-packaging.md §7.2, §7.3)
// ---------------------------------------------------------------------------

/// Generate a media playlist for a single rendition.
///
/// Rules (from transcoding-and-packaging.md §7.3):
/// - `#EXT-X-TARGETDURATION`: rounded-up maximum segment duration
/// - `#EXT-X-MEDIA-SEQUENCE`: for live, increments as old segments fall off; for VOD, always 0
/// - `#EXT-X-MAP`: points to init.mp4
/// - `#EXTINF`: actual duration, 3 decimal places
/// - Live: sliding window of N segments
/// - VOD: complete playlist with `#EXT-X-ENDLIST`
pub fn generate_media_playlist(
    segments: &[PlaylistSegment],
    hls_version: u32,
    is_vod: bool,
    is_finished: bool,
) -> String {
    let mut playlist = String::with_capacity(2048);

    // Calculate TARGETDURATION (rounded-up max segment duration)
    let target_duration = segments
        .iter()
        .map(|s| s.duration_secs)
        .fold(0.0f64, f64::max)
        .ceil() as u32;
    let target_duration = target_duration.max(1);

    // Media sequence (first segment's sequence number)
    let media_sequence = segments.first().map(|s| s.sequence).unwrap_or(0);

    playlist.push_str("#EXTM3U\n");
    playlist.push_str(&format!("#EXT-X-VERSION:{}\n", hls_version));
    playlist.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", target_duration));

    if is_vod {
        playlist.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    }

    playlist.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", media_sequence));
    playlist.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");
    playlist.push('\n');

    for segment in segments {
        // #EXT-X-PROGRAM-DATE-TIME (for live, P1 feature)
        if let Some(pdt) = &segment.program_date_time {
            playlist.push_str(&format!(
                "#EXT-X-PROGRAM-DATE-TIME:{}\n",
                pdt.format("%Y-%m-%dT%H:%M:%S%.3fZ")
            ));
        }

        playlist.push_str(&format!("#EXTINF:{:.3},\n", segment.duration_secs));
        playlist.push_str(&format!("{}\n", segment.filename));
    }

    // VOD or finished live streams get #EXT-X-ENDLIST
    if is_vod || is_finished {
        playlist.push_str("#EXT-X-ENDLIST\n");
    }

    playlist
}

// ---------------------------------------------------------------------------
// Storage path helpers
// ---------------------------------------------------------------------------

/// Generate the storage path for a segment file.
/// Format: `{stream_id}/{rendition}/{sequence:06}.m4s` (FR-STORAGE-03)
pub fn segment_path(stream_id: &str, rendition: &str, sequence: u64) -> String {
    format!("{}/{}/{:06}.m4s", stream_id, rendition, sequence)
}

/// Generate the storage path for an init segment.
/// Format: `{stream_id}/{rendition}/init.mp4`
pub fn init_segment_path(stream_id: &str, rendition: &str) -> String {
    format!("{}/{}/init.mp4", stream_id, rendition)
}

/// Generate the storage path for a media playlist.
/// Format: `{stream_id}/{rendition}/media.m3u8`
pub fn media_playlist_path(stream_id: &str, rendition: &str) -> String {
    format!("{}/{}/media.m3u8", stream_id, rendition)
}

/// Generate the storage path for the multivariant playlist.
/// Format: `{stream_id}/master.m3u8`
pub fn master_playlist_path(stream_id: &str) -> String {
    format!("{}/master.m3u8", stream_id)
}

/// Segment filename (without path prefix).
/// Format: `{sequence:06}.m4s` (FR-STORAGE-03)
pub fn segment_filename(sequence: u64) -> String {
    format!("{:06}.m4s", sequence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multivariant_playlist() {
        let renditions = vec![
            RenditionInfo {
                id: RenditionId::High,
                width: 1920,
                height: 1080,
                video_bitrate_kbps: 3500,
                audio_bitrate_kbps: 128,
                profile: "high".to_string(),
                level: "4.1".to_string(),
                frame_rate: 30.0,
                has_audio: true,
            },
            RenditionInfo {
                id: RenditionId::Medium,
                width: 1280,
                height: 720,
                video_bitrate_kbps: 2000,
                audio_bitrate_kbps: 128,
                profile: "main".to_string(),
                level: "3.1".to_string(),
                frame_rate: 30.0,
                has_audio: true,
            },
            RenditionInfo {
                id: RenditionId::Low,
                width: 854,
                height: 480,
                video_bitrate_kbps: 1000,
                audio_bitrate_kbps: 96,
                profile: "main".to_string(),
                level: "3.0".to_string(),
                frame_rate: 30.0,
                has_audio: true,
            },
        ];

        let playlist = generate_multivariant_playlist(&renditions, 7);

        assert!(playlist.contains("#EXTM3U"));
        assert!(playlist.contains("#EXT-X-VERSION:7"));
        assert!(playlist.contains("#EXT-X-INDEPENDENT-SEGMENTS"));
        assert!(playlist.contains("BANDWIDTH=3990800"));
        assert!(playlist.contains("RESOLUTION=1920x1080"));
        assert!(playlist.contains("avc1.640029,mp4a.40.2"));
        assert!(playlist.contains("high/media.m3u8"));
        assert!(playlist.contains("medium/media.m3u8"));
        assert!(playlist.contains("low/media.m3u8"));
    }

    #[test]
    fn test_live_media_playlist() {
        let segments = vec![
            PlaylistSegment {
                sequence: 42,
                duration_secs: 6.006,
                filename: "000042.m4s".to_string(),
                program_date_time: None,
            },
            PlaylistSegment {
                sequence: 43,
                duration_secs: 6.006,
                filename: "000043.m4s".to_string(),
                program_date_time: None,
            },
            PlaylistSegment {
                sequence: 44,
                duration_secs: 5.972,
                filename: "000044.m4s".to_string(),
                program_date_time: None,
            },
        ];

        let playlist = generate_media_playlist(&segments, 7, false, false);

        assert!(playlist.contains("#EXTM3U"));
        assert!(playlist.contains("#EXT-X-VERSION:7"));
        assert!(playlist.contains("#EXT-X-TARGETDURATION:7")); // ceil(6.006) = 7
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:42"));
        assert!(playlist.contains("#EXT-X-MAP:URI=\"init.mp4\""));
        assert!(playlist.contains("#EXTINF:6.006,"));
        assert!(playlist.contains("#EXTINF:5.972,"));
        assert!(playlist.contains("000042.m4s"));
        assert!(!playlist.contains("#EXT-X-ENDLIST"));
        assert!(!playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
    }

    #[test]
    fn test_vod_media_playlist() {
        let segments = vec![
            PlaylistSegment {
                sequence: 0,
                duration_secs: 6.006,
                filename: "000000.m4s".to_string(),
                program_date_time: None,
            },
            PlaylistSegment {
                sequence: 1,
                duration_secs: 4.238,
                filename: "000001.m4s".to_string(),
                program_date_time: None,
            },
        ];

        let playlist = generate_media_playlist(&segments, 7, true, true);

        assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(playlist.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn test_finished_live_playlist_has_endlist() {
        let segments = vec![PlaylistSegment {
            sequence: 10,
            duration_secs: 6.0,
            filename: "000010.m4s".to_string(),
            program_date_time: None,
        }];

        let playlist = generate_media_playlist(&segments, 7, false, true);
        assert!(playlist.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn test_storage_paths() {
        let sid = "abc-123";
        assert_eq!(segment_path(sid, "high", 42), "abc-123/high/000042.m4s");
        assert_eq!(init_segment_path(sid, "high"), "abc-123/high/init.mp4");
        assert_eq!(media_playlist_path(sid, "high"), "abc-123/high/media.m3u8");
        assert_eq!(master_playlist_path(sid), "abc-123/master.m3u8");
        assert_eq!(segment_filename(1), "000001.m4s");
    }
}
