use bytes::{Bytes, BytesMut};

use crate::core::types::{EncodedSegment, RenditionId, StreamId};

// ---------------------------------------------------------------------------
// Segment boundary detection (from transcoding-and-packaging.md §4.2)
// ---------------------------------------------------------------------------

/// An encoded packet from the encoder, before segmentation.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub pts: i64,
    pub _dts: i64,
    pub keyframe: bool,
    pub data: Bytes,
    pub is_audio: bool,
}

/// Accumulates encoded packets and emits segments at IDR boundaries.
///
/// Algorithm (from transcoding-and-packaging.md §4.2):
/// ```text
/// for each encoded_packet from encoder:
///     if segment_start_pts is None:
///         segment_start_pts = packet.pts
///     segment_accumulator.push(packet)
///     current_duration = (packet.pts - segment_start_pts) / 90000.0
///     if current_duration >= target_duration AND packet.is_keyframe:
///         emit segment
///         reset accumulator
/// ```
///
/// Why cut at IDR *after* reaching target duration:
/// - Cutting before would produce short segments if IDRs don't align.
/// - Cutting at/after ensures segments are at least the target duration.
/// - With 2s keyframe interval, max overshoot is ~2s (6-8s segments).
pub struct SegmentAccumulator {
    stream_id: StreamId,
    rendition: RenditionId,
    target_duration_secs: f64,
    /// Current segment's video data.
    video_buffer: BytesMut,
    /// Current segment's audio data.
    audio_buffer: BytesMut,
    /// PTS of the first frame in the current segment.
    segment_start_pts: Option<i64>,
    /// PTS of the IDR that starts the current segment.
    segment_keyframe_pts: Option<i64>,
    /// PTS of the most recent packet.
    last_pts: i64,
    /// Next segment sequence number (0-indexed, monotonically increasing).
    next_sequence: u64,
    /// Whether we have any audio data.
    has_audio: bool,
    /// Rendition output width.
    width: u32,
    /// Rendition output height.
    height: u32,
    /// Video bitrate in kbps.
    video_bitrate_kbps: u32,
    /// Audio bitrate in kbps.
    audio_bitrate_kbps: u32,
    /// H.264 profile name.
    profile: String,
    /// H.264 level.
    level: String,
    /// Source frame rate.
    frame_rate: f64,
}

impl SegmentAccumulator {
    pub fn new(
        stream_id: StreamId,
        rendition: RenditionId,
        target_duration_secs: f64,
        width: u32,
        height: u32,
        video_bitrate_kbps: u32,
        audio_bitrate_kbps: u32,
        profile: String,
        level: String,
        frame_rate: f64,
    ) -> Self {
        Self {
            stream_id,
            rendition,
            target_duration_secs,
            video_buffer: BytesMut::new(),
            audio_buffer: BytesMut::new(),
            segment_start_pts: None,
            segment_keyframe_pts: None,
            last_pts: 0,
            next_sequence: 0,
            has_audio: false,
            width,
            height,
            video_bitrate_kbps,
            audio_bitrate_kbps,
            profile,
            level,
            frame_rate,
        }
    }

    /// Feed an encoded packet into the accumulator.
    /// Returns `Some(EncodedSegment)` if a segment boundary was reached.
    pub fn push(&mut self, packet: EncodedPacket) -> Option<EncodedSegment> {
        // Initialize segment start PTS on first packet
        if self.segment_start_pts.is_none() {
            self.segment_start_pts = Some(packet.pts);
            if packet.keyframe && !packet.is_audio {
                self.segment_keyframe_pts = Some(packet.pts);
            }
        }

        self.last_pts = packet.pts;

        // Accumulate data
        if packet.is_audio {
            self.has_audio = true;
            self.audio_buffer.extend_from_slice(&packet.data);
        } else {
            self.video_buffer.extend_from_slice(&packet.data);
        }

        // Check segment boundary: duration >= target AND packet is a video keyframe
        if !packet.is_audio && packet.keyframe {
            let start = self.segment_start_pts.unwrap_or(0);
            let duration_secs = (packet.pts - start) as f64 / 90000.0;

            if duration_secs >= self.target_duration_secs {
                return Some(self.emit_segment(false));
            }
        }

        None
    }

    /// Flush any remaining data as the final segment.
    pub fn flush(mut self) -> Option<EncodedSegment> {
        if self.video_buffer.is_empty() {
            return None;
        }
        Some(self.emit_segment(true))
    }

    /// Emit the current accumulated data as a segment.
    fn emit_segment(&mut self, is_last: bool) -> EncodedSegment {
        let start_pts = self.segment_start_pts.unwrap_or(0);
        let duration_secs = (self.last_pts - start_pts) as f64 / 90000.0;

        let segment = EncodedSegment {
            _stream_id: self.stream_id,
            rendition: self.rendition.clone(),
            sequence: self.next_sequence,
            duration_secs,
            pts_start: start_pts,
            video_data: self.video_buffer.split().freeze(),
            audio_data: if self.has_audio && !self.audio_buffer.is_empty() {
                Some(self.audio_buffer.split().freeze())
            } else {
                None
            },
            is_last,
            width: self.width,
            height: self.height,
            video_bitrate_kbps: self.video_bitrate_kbps,
            audio_bitrate_kbps: self.audio_bitrate_kbps,
            profile: self.profile.clone(),
            level: self.level.clone(),
            frame_rate: self.frame_rate,
        };

        // Reset for next segment
        self.next_sequence += 1;
        self.segment_start_pts = None;
        self.segment_keyframe_pts = None;

        segment
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_packet(pts: i64, keyframe: bool, is_audio: bool) -> EncodedPacket {
        EncodedPacket {
            pts,
            _dts: pts,
            keyframe,
            data: Bytes::from(vec![0xAA; 100]),
            is_audio,
        }
    }

    #[test]
    fn test_segment_at_idr_after_target_duration() {
        let mut acc = SegmentAccumulator::new(
            StreamId::new(),
            RenditionId::High,
            6.0, // 6 second target
            1920,
            1080,
            3500,
            128,
            "high".to_string(),
            "4.1".to_string(),
            30.0,
        );

        // Feed frames for 6+ seconds at 90kHz timebase
        // 6 seconds = 540000 ticks
        // Keyframes every 2 seconds = 180000 ticks

        // t=0: IDR
        assert!(acc.push(make_packet(0, true, false)).is_none());
        // t=2s: IDR (not yet 6s)
        assert!(acc.push(make_packet(180000, true, false)).is_none());
        // t=4s: IDR (not yet 6s)
        assert!(acc.push(make_packet(360000, true, false)).is_none());
        // t=6s: IDR (>= 6s, should emit)
        let seg = acc.push(make_packet(540000, true, false));
        assert!(seg.is_some());

        let seg = seg.unwrap();
        assert_eq!(seg.sequence, 0);
        assert_eq!(seg.pts_start, 0);
        assert!((seg.duration_secs - 6.0).abs() < 0.01);
        assert!(!seg.is_last);
    }

    #[test]
    fn test_no_segment_before_target_duration() {
        let mut acc = SegmentAccumulator::new(
            StreamId::new(),
            RenditionId::Low,
            6.0,
            854,
            480,
            1000,
            96,
            "main".to_string(),
            "3.0".to_string(),
            30.0,
        );

        // Only 4 seconds of data
        assert!(acc.push(make_packet(0, true, false)).is_none());
        assert!(acc.push(make_packet(180000, true, false)).is_none());
        assert!(acc.push(make_packet(360000, true, false)).is_none());
        // No segment emitted yet
    }

    #[test]
    fn test_flush_emits_final_segment() {
        let mut acc = SegmentAccumulator::new(
            StreamId::new(),
            RenditionId::Medium,
            6.0,
            1280,
            720,
            2000,
            128,
            "main".to_string(),
            "3.1".to_string(),
            30.0,
        );

        acc.push(make_packet(0, true, false));
        acc.push(make_packet(180000, false, false));

        let seg = acc.flush().unwrap();
        assert!(seg.is_last);
        assert_eq!(seg.sequence, 0);
    }

    #[test]
    fn test_sequence_numbers_increment() {
        let mut acc = SegmentAccumulator::new(
            StreamId::new(),
            RenditionId::High,
            2.0, // 2 second target = 180000 ticks at 90kHz
            1920,
            1080,
            3500,
            128,
            "high".to_string(),
            "4.1".to_string(),
            30.0,
        );

        // First segment: starts at 0, IDR at 180000 triggers cut (duration=2s)
        acc.push(make_packet(0, true, false));
        let seg1 = acc.push(make_packet(180000, true, false)).unwrap();
        assert_eq!(seg1.sequence, 0);

        // After emit, segment_start_pts is None.
        // Next packet (non-IDR at 180000) sets segment_start_pts = 180000.
        // Then IDR at 360000 gives duration = (360000-180000)/90000 = 2.0s → emit.
        acc.push(make_packet(180000, false, false));
        let seg2 = acc.push(make_packet(360000, true, false)).unwrap();
        assert_eq!(seg2.sequence, 1);
    }

    #[test]
    fn test_audio_data_included() {
        let mut acc = SegmentAccumulator::new(
            StreamId::new(),
            RenditionId::High,
            6.0,
            1920,
            1080,
            3500,
            128,
            "high".to_string(),
            "4.1".to_string(),
            30.0,
        );

        acc.push(make_packet(0, true, false)); // video IDR
        acc.push(make_packet(0, true, true)); // audio
        acc.push(make_packet(180000, false, false)); // video
        acc.push(make_packet(180000, true, true)); // audio
        let seg = acc.push(make_packet(540000, true, false));
        assert!(seg.is_some());
        let seg = seg.unwrap();
        assert!(seg.audio_data.is_some());
    }

    #[test]
    fn test_no_audio_when_none_provided() {
        let mut acc = SegmentAccumulator::new(
            StreamId::new(),
            RenditionId::High,
            6.0,
            1920,
            1080,
            3500,
            128,
            "high".to_string(),
            "4.1".to_string(),
            30.0,
        );

        acc.push(make_packet(0, true, false));
        let seg = acc.push(make_packet(540000, true, false)).unwrap();
        assert!(seg.audio_data.is_none());
    }
}
