use std::collections::VecDeque;

use chrono::{DateTime, Utc};

use crate::core::types::{RenditionId, StreamId};

use super::manifest::{self, PlaylistSegment};

// ---------------------------------------------------------------------------
// Segment index (from transcoding-and-packaging.md §8)
// ---------------------------------------------------------------------------

/// A single segment entry in the index.
#[derive(Debug, Clone)]
pub struct SegmentEntry {
    pub sequence: u64,
    pub duration_secs: f64,
    pub _storage_path: String,
    pub program_date_time: DateTime<Utc>,
    pub _size_bytes: u64,
}

/// Per-rendition segment index for live window management.
///
/// Structure (from transcoding-and-packaging.md §8):
/// ```text
/// SegmentIndex {
///     stream_id: StreamId,
///     rendition: RenditionId,
///     segments: VecDeque<SegmentEntry>,  // bounded by live_window_segments
///     next_sequence: u64,
///     total_segments_produced: u64,
/// }
/// ```
///
/// When a new segment is added:
/// 1. Push to back of VecDeque.
/// 2. If len > live_window_segments, pop from front.
/// 3. Regenerate the media playlist from current segments.
///
/// Popped segments are NOT immediately deleted from S3.
/// A background cleanup task handles deletion after retention period.
#[derive(Debug)]
pub struct SegmentIndex {
    pub _stream_id: StreamId,
    pub _rendition: RenditionId,
    segments: VecDeque<SegmentEntry>,
    live_window_segments: usize,
    next_sequence: u64,
    total_segments_produced: u64,
    /// Maximum segment duration seen (for TARGETDURATION calculation).
    max_duration: f64,
}

impl SegmentIndex {
    pub fn new(stream_id: StreamId, rendition: RenditionId, live_window_segments: usize) -> Self {
        Self {
            _stream_id: stream_id,
            _rendition: rendition,
            segments: VecDeque::with_capacity(live_window_segments + 1),
            live_window_segments,
            next_sequence: 0,
            total_segments_produced: 0,
            max_duration: 0.0,
        }
    }

    /// Add a new segment to the index.
    ///
    /// Returns the evicted segment (if the window overflowed) so the caller
    /// can schedule it for eventual cleanup.
    pub fn add_segment(
        &mut self,
        duration_secs: f64,
        storage_path: String,
        size_bytes: u64,
    ) -> Option<SegmentEntry> {
        let entry = SegmentEntry {
            sequence: self.next_sequence,
            duration_secs,
            _storage_path: storage_path,
            program_date_time: Utc::now(),
            _size_bytes: size_bytes,
        };

        self.segments.push_back(entry);
        self.next_sequence += 1;
        self.total_segments_produced += 1;

        if duration_secs > self.max_duration {
            self.max_duration = duration_secs;
        }

        // Evict oldest if window exceeded
        if self.segments.len() > self.live_window_segments {
            self.segments.pop_front()
        } else {
            None
        }
    }

    /// Generate a media playlist from the current segment window.
    pub fn generate_playlist(&self, hls_version: u32, is_vod: bool, is_finished: bool) -> String {
        let playlist_segments: Vec<PlaylistSegment> = self
            .segments
            .iter()
            .map(|entry| PlaylistSegment {
                sequence: entry.sequence,
                duration_secs: entry.duration_secs,
                filename: manifest::segment_filename(entry.sequence),
                program_date_time: Some(entry.program_date_time),
            })
            .collect();

        manifest::generate_media_playlist(&playlist_segments, hls_version, is_vod, is_finished)
    }
}

#[cfg(test)]
impl SegmentIndex {
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    pub fn total_segments_produced(&self) -> u64 {
        self.total_segments_produced
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn media_sequence(&self) -> u64 {
        self.segments.front().map(|s| s.sequence).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_segments_within_window() {
        let mut index = SegmentIndex::new(StreamId::new(), RenditionId::High, 5);

        for i in 0..5 {
            let evicted = index.add_segment(6.0, format!("{:06}.m4s", i), 200000);
            assert!(evicted.is_none());
        }

        assert_eq!(index.len(), 5);
        assert_eq!(index.next_sequence(), 5);
        assert_eq!(index.total_segments_produced(), 5);
    }

    #[test]
    fn test_eviction_on_window_overflow() {
        let mut index = SegmentIndex::new(StreamId::new(), RenditionId::High, 3);

        // Fill window
        for i in 0..3 {
            index.add_segment(6.0, format!("{:06}.m4s", i), 200000);
        }
        assert_eq!(index.len(), 3);

        // Add one more — should evict the oldest
        let evicted = index.add_segment(6.0, "000003.m4s".to_string(), 200000);
        assert!(evicted.is_some());
        let evicted = evicted.unwrap();
        assert_eq!(evicted.sequence, 0);
        assert_eq!(index.len(), 3);
        assert_eq!(index.media_sequence(), 1);
    }

    #[test]
    fn test_media_sequence_tracks_window() {
        let mut index = SegmentIndex::new(StreamId::new(), RenditionId::Low, 2);

        index.add_segment(6.0, "000000.m4s".to_string(), 100);
        assert_eq!(index.media_sequence(), 0);

        index.add_segment(6.0, "000001.m4s".to_string(), 100);
        assert_eq!(index.media_sequence(), 0);

        index.add_segment(6.0, "000002.m4s".to_string(), 100);
        assert_eq!(index.media_sequence(), 1); // 000000 evicted

        index.add_segment(6.0, "000003.m4s".to_string(), 100);
        assert_eq!(index.media_sequence(), 2); // 000001 evicted
    }

    #[test]
    fn test_generate_live_playlist() {
        let mut index = SegmentIndex::new(StreamId::new(), RenditionId::High, 5);

        for i in 0..3 {
            index.add_segment(6.006, format!("{:06}.m4s", i), 200000);
        }

        let playlist = index.generate_playlist(7, false, false);
        assert!(playlist.contains("#EXTM3U"));
        assert!(playlist.contains("#EXT-X-VERSION:7"));
        assert!(playlist.contains("#EXT-X-TARGETDURATION:7"));
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(playlist.contains("#EXTINF:6.006,"));
        assert!(playlist.contains("000000.m4s"));
        assert!(playlist.contains("000001.m4s"));
        assert!(playlist.contains("000002.m4s"));
        assert!(!playlist.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn test_generate_vod_playlist() {
        let mut index = SegmentIndex::new(StreamId::new(), RenditionId::Medium, 100);

        index.add_segment(6.006, "000000.m4s".to_string(), 200000);
        index.add_segment(4.238, "000001.m4s".to_string(), 150000);

        let playlist = index.generate_playlist(7, true, true);
        assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(playlist.contains("#EXT-X-ENDLIST"));
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    }

    #[test]
    fn test_generate_finished_live_playlist() {
        let mut index = SegmentIndex::new(StreamId::new(), RenditionId::High, 5);
        index.add_segment(6.0, "000000.m4s".to_string(), 200000);

        let playlist = index.generate_playlist(7, false, true);
        assert!(playlist.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn test_total_segments_vs_window() {
        let mut index = SegmentIndex::new(StreamId::new(), RenditionId::Low, 2);

        for i in 0..10 {
            index.add_segment(6.0, format!("{:06}.m4s", i), 100);
        }

        assert_eq!(index.len(), 2); // window size
        assert_eq!(index.total_segments_produced(), 10); // total ever produced
        assert_eq!(index.next_sequence(), 10);
    }
}
