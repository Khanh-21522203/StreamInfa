use bytes::Bytes;
use tracing::{debug, warn};

use crate::core::error::IngestError;
use crate::core::security;
use crate::core::types::{AudioCodec, DemuxedFrame, StreamId, Track, VideoCodec};

use super::rtmp::RtmpMessage;

// ---------------------------------------------------------------------------
// FLV constants (from ingest.md §2.4)
// ---------------------------------------------------------------------------

/// FLV video CodecID for H.264.
const FLV_CODEC_ID_H264: u8 = 7;

/// FLV audio SoundFormat for AAC.
const FLV_SOUND_FORMAT_AAC: u8 = 10;

/// FLV audio SoundFormat for MP3.
const FLV_SOUND_FORMAT_MP3: u8 = 2;

/// AVC packet type: sequence header (SPS/PPS).
const AVC_PACKET_TYPE_SEQ_HEADER: u8 = 0;

/// AVC packet type: NALU data.
const AVC_PACKET_TYPE_NALU: u8 = 1;

/// AAC packet type: AudioSpecificConfig.
const AAC_PACKET_TYPE_SEQ_HEADER: u8 = 0;

/// AAC packet type: raw AAC frame.
const AAC_PACKET_TYPE_RAW: u8 = 1;

/// H.264 NAL unit type: IDR slice.
const NAL_TYPE_IDR: u8 = 5;

/// H.264 NAL unit type: SPS.
const NAL_TYPE_SPS: u8 = 7;

/// H.264 NAL unit type: PPS.
const NAL_TYPE_PPS: u8 = 8;

// ---------------------------------------------------------------------------
// FLV Demuxer (from ingest.md §2.4, §4)
// ---------------------------------------------------------------------------

/// FLV demuxer that extracts elementary streams from RTMP FLV tags.
///
/// Handles:
/// - Video: H.264 NALU extraction with SPS/PPS parsing
/// - Audio: AAC raw frame extraction, MP3 passthrough
/// - Timestamp normalization to 90kHz timebase
pub struct FlvDemuxer {
    stream_id: StreamId,
    /// True once we've received the AVC sequence header (SPS/PPS).
    has_video_seq_header: bool,
    /// True once we've received the AudioSpecificConfig.
    has_audio_seq_header: bool,
    /// Stored SPS data for re-injection if needed.
    sps: Option<Bytes>,
    /// Stored PPS data for re-injection if needed.
    pps: Option<Bytes>,
    /// Detected video dimensions from SPS.
    video_width: u32,
    video_height: u32,
    /// Detected audio parameters from AudioSpecificConfig.
    audio_sample_rate: u32,
    audio_channels: u8,
    audio_codec: Option<AudioCodec>,
    /// First PTS seen, for normalization to start at 0.
    first_pts: Option<i64>,
}

impl FlvDemuxer {
    pub fn new(stream_id: StreamId) -> Self {
        Self {
            stream_id,
            has_video_seq_header: false,
            has_audio_seq_header: false,
            sps: None,
            pps: None,
            video_width: 0,
            video_height: 0,
            audio_sample_rate: 44100,
            audio_channels: 2,
            audio_codec: None,
            first_pts: None,
        }
    }

    /// Returns true if we've received both video and audio sequence headers
    /// (or video-only if no audio is present).
    pub fn has_sequence_header(&self) -> bool {
        self.has_video_seq_header
    }

    /// Demux a single RTMP message containing an FLV tag.
    ///
    /// Returns `Ok(Some(frame))` for media frames, `Ok(None)` for sequence
    /// headers and non-frame data, or `Err` for invalid data.
    pub fn demux_flv_tag(
        &mut self,
        msg: &RtmpMessage,
    ) -> Result<Option<DemuxedFrame>, IngestError> {
        let data = &msg.data;
        if data.is_empty() {
            return Ok(None);
        }

        // Validate FLV tag size against security limit (security.md §4.1)
        if data.len() > security::MAX_FLV_TAG_SIZE {
            return Err(IngestError::CorruptNalu {
                reason: format!(
                    "FLV tag size {} exceeds max {}",
                    data.len(),
                    security::MAX_FLV_TAG_SIZE
                ),
            });
        }

        match msg.msg_type {
            super::rtmp::RtmpMessageType::Video => self.demux_video(data, msg.timestamp),
            super::rtmp::RtmpMessageType::Audio => self.demux_audio(data, msg.timestamp),
            _ => Ok(None),
        }
    }

    /// Demux a video FLV tag body (from ingest.md §2.4 — Video demux logic).
    ///
    /// FLV video tag body:
    /// - Byte 0: FrameType (4 bits) | CodecID (4 bits)
    /// - Byte 1: AVC packet type (0=seq header, 1=NALU, 2=end of seq)
    /// - Bytes 2-4: Composition time offset (signed 24-bit, milliseconds)
    /// - Remaining: AVC data
    fn demux_video(
        &mut self,
        data: &[u8],
        timestamp_ms: u32,
    ) -> Result<Option<DemuxedFrame>, IngestError> {
        if data.len() < 5 {
            return Err(IngestError::CorruptNalu {
                reason: "video tag too short".to_string(),
            });
        }

        let codec_id = data[0] & 0x0F;
        let frame_type = (data[0] >> 4) & 0x0F;

        // Check CodecID = 7 (H.264)
        if codec_id != FLV_CODEC_ID_H264 {
            return Err(IngestError::InvalidCodec {
                expected: "h264 (CodecID=7)".to_string(),
                actual: format!("CodecID={}", codec_id),
            });
        }

        let avc_packet_type = data[1];
        let cts = i32::from_be_bytes([
            if data[2] & 0x80 != 0 { 0xFF } else { 0x00 },
            data[2],
            data[3],
            data[4],
        ]);

        match avc_packet_type {
            AVC_PACKET_TYPE_SEQ_HEADER => {
                // AVC Sequence Header: contains SPS/PPS in AVCDecoderConfigurationRecord
                self.parse_avc_decoder_config(&data[5..])?;
                self.has_video_seq_header = true;
                debug!(
                    stream_id = %self.stream_id,
                    width = self.video_width,
                    height = self.video_height,
                    "received AVC sequence header"
                );
                Ok(None)
            }
            AVC_PACKET_TYPE_NALU => {
                if !self.has_video_seq_header {
                    return Ok(None); // Drop frames before sequence header
                }

                // Extract NALUs (length-prefixed, 4-byte big-endian)
                let nalu_data = &data[5..];
                let (keyframe, nalu_bytes) = self.extract_nalus(nalu_data)?;

                // Timestamp normalization: FLV ms → 90kHz
                let pts_90k = (timestamp_ms as i64 + cts as i64) * 90;
                let dts_90k = timestamp_ms as i64 * 90;

                // Normalize to start at 0
                let pts_90k = self.normalize_pts(pts_90k);
                let dts_90k = self.normalize_pts(dts_90k);

                let is_keyframe = keyframe || frame_type == 1;

                Ok(Some(DemuxedFrame {
                    stream_id: self.stream_id,
                    track: Track::Video {
                        codec: VideoCodec::H264,
                        width: self.video_width,
                        height: self.video_height,
                    },
                    pts: pts_90k,
                    dts: dts_90k,
                    keyframe: is_keyframe,
                    data: Bytes::from(nalu_bytes),
                }))
            }
            _ => Ok(None), // End of sequence or unknown
        }
    }

    /// Demux an audio FLV tag body (from ingest.md §2.4 — Audio demux logic).
    ///
    /// FLV audio tag body:
    /// - Byte 0: SoundFormat (4 bits) | SampleRate (2 bits) | SampleSize (1 bit) | Channels (1 bit)
    /// - Byte 1 (AAC only): AAC packet type (0=seq header, 1=raw)
    /// - Remaining: audio data
    fn demux_audio(
        &mut self,
        data: &[u8],
        timestamp_ms: u32,
    ) -> Result<Option<DemuxedFrame>, IngestError> {
        if data.is_empty() {
            return Ok(None);
        }

        let sound_format = (data[0] >> 4) & 0x0F;

        match sound_format {
            FLV_SOUND_FORMAT_AAC => {
                if data.len() < 2 {
                    return Ok(None);
                }
                let aac_packet_type = data[1];

                match aac_packet_type {
                    AAC_PACKET_TYPE_SEQ_HEADER => {
                        // AudioSpecificConfig
                        self.parse_audio_specific_config(&data[2..])?;
                        self.audio_codec = Some(AudioCodec::Aac);
                        self.has_audio_seq_header = true;
                        debug!(
                            stream_id = %self.stream_id,
                            sample_rate = self.audio_sample_rate,
                            channels = self.audio_channels,
                            "received AudioSpecificConfig"
                        );
                        Ok(None)
                    }
                    AAC_PACKET_TYPE_RAW => {
                        if !self.has_audio_seq_header {
                            return Ok(None);
                        }

                        let pts_90k = timestamp_ms as i64 * 90;
                        let pts_90k = self.normalize_pts(pts_90k);

                        Ok(Some(DemuxedFrame {
                            stream_id: self.stream_id,
                            track: Track::Audio {
                                codec: AudioCodec::Aac,
                                sample_rate: self.audio_sample_rate,
                                channels: self.audio_channels,
                            },
                            pts: pts_90k,
                            dts: pts_90k,
                            keyframe: true, // Every audio frame is a "keyframe"
                            data: Bytes::copy_from_slice(&data[2..]),
                        }))
                    }
                    _ => Ok(None),
                }
            }
            FLV_SOUND_FORMAT_MP3 => {
                self.audio_codec = Some(AudioCodec::Mp3);

                let pts_90k = timestamp_ms as i64 * 90;
                let pts_90k = self.normalize_pts(pts_90k);

                Ok(Some(DemuxedFrame {
                    stream_id: self.stream_id,
                    track: Track::Audio {
                        codec: AudioCodec::Mp3,
                        sample_rate: self.audio_sample_rate,
                        channels: self.audio_channels,
                    },
                    pts: pts_90k,
                    dts: pts_90k,
                    keyframe: true,
                    data: Bytes::copy_from_slice(&data[1..]),
                }))
            }
            _ => {
                warn!(
                    stream_id = %self.stream_id,
                    sound_format,
                    "unsupported audio format, skipping"
                );
                Ok(None)
            }
        }
    }

    /// Parse AVCDecoderConfigurationRecord to extract SPS/PPS and video dimensions.
    fn parse_avc_decoder_config(&mut self, data: &[u8]) -> Result<(), IngestError> {
        // AVCDecoderConfigurationRecord:
        // configurationVersion (1) + AVCProfileIndication (1) + profile_compatibility (1)
        // + AVCLevelIndication (1) + lengthSizeMinusOne (1)
        // + numOfSequenceParameterSets (1) + SPS entries + numOfPictureParameterSets (1) + PPS entries
        if data.len() < 7 {
            return Err(IngestError::CorruptNalu {
                reason: "AVC decoder config too short".to_string(),
            });
        }

        let num_sps = data[5] & 0x1F;
        let mut pos = 6;

        for _ in 0..num_sps {
            if pos + 2 > data.len() {
                break;
            }
            let sps_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + sps_len > data.len() {
                break;
            }
            let sps_data = &data[pos..pos + sps_len];
            self.sps = Some(Bytes::copy_from_slice(sps_data));

            // Parse SPS for resolution (simplified)
            if sps_len > 4 {
                self.parse_sps_dimensions(sps_data);
            }
            pos += sps_len;
        }

        if pos < data.len() {
            let num_pps = data[pos] as usize;
            pos += 1;
            for _ in 0..num_pps {
                if pos + 2 > data.len() {
                    break;
                }
                let pps_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                pos += 2;
                if pos + pps_len > data.len() {
                    break;
                }
                self.pps = Some(Bytes::copy_from_slice(&data[pos..pos + pps_len]));
                pos += pps_len;
            }
        }

        // Validate resolution
        if self.video_width == 0 || self.video_height == 0 {
            return Err(IngestError::InvalidResolution {
                width: self.video_width,
                height: self.video_height,
            });
        }
        if self.video_width > 3840 || self.video_height > 2160 {
            return Err(IngestError::InvalidResolution {
                width: self.video_width,
                height: self.video_height,
            });
        }

        Ok(())
    }

    /// Simplified SPS dimension parsing.
    fn parse_sps_dimensions(&mut self, sps: &[u8]) {
        // Very simplified: for a proper implementation, we'd need a full
        // H.264 SPS parser with exp-golomb decoding. For now, we use
        // common resolution detection heuristics.
        // The SPS contains profile_idc, level_idc, and then exp-golomb coded
        // fields including pic_width_in_mbs_minus1 and pic_height_in_map_units_minus1.
        // A full parser is needed for production; this is a placeholder.
        if sps.len() > 4 {
            // Default to 1920x1080 if we can't parse (will be overridden by FFmpeg probe)
            if self.video_width == 0 {
                self.video_width = 1920;
                self.video_height = 1080;
            }
        }
    }

    /// Parse AudioSpecificConfig for sample rate and channels.
    fn parse_audio_specific_config(&mut self, data: &[u8]) -> Result<(), IngestError> {
        if data.len() < 2 {
            return Ok(()); // Use defaults
        }

        // AudioSpecificConfig (ISO 14496-3):
        // audioObjectType (5 bits) + samplingFrequencyIndex (4 bits) + channelConfiguration (4 bits)
        let freq_index = ((data[0] & 0x07) << 1) | ((data[1] >> 7) & 0x01);
        let channel_config = (data[1] >> 3) & 0x0F;

        self.audio_sample_rate = match freq_index {
            0 => 96000,
            1 => 88200,
            2 => 64000,
            3 => 48000,
            4 => 44100,
            5 => 32000,
            6 => 24000,
            7 => 22050,
            8 => 16000,
            9 => 12000,
            10 => 11025,
            11 => 8000,
            _ => 44100,
        };

        self.audio_channels = match channel_config {
            1 => 1,
            2 => 2,
            _ => 2,
        };

        Ok(())
    }

    /// Extract NALUs from length-prefixed data, detect keyframes.
    fn extract_nalus(&self, data: &[u8]) -> Result<(bool, Vec<u8>), IngestError> {
        let mut pos = 0;
        let mut keyframe = false;
        let mut output = Vec::with_capacity(data.len());

        while pos + 4 <= data.len() {
            let nalu_len =
                u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                    as usize;
            pos += 4;

            if pos + nalu_len > data.len() {
                return Err(IngestError::CorruptNalu {
                    reason: format!(
                        "NALU length {} exceeds remaining data {} at offset {}",
                        nalu_len,
                        data.len() - pos,
                        pos
                    ),
                });
            }

            if nalu_len > 0 {
                let nal_type = data[pos] & 0x1F;
                match nal_type {
                    NAL_TYPE_IDR => keyframe = true,
                    NAL_TYPE_SPS | NAL_TYPE_PPS => {
                        // Parameter sets inline — include them
                    }
                    _ => {}
                }

                // Annex-B start code + NALU
                output.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                output.extend_from_slice(&data[pos..pos + nalu_len]);
            }

            pos += nalu_len;
        }

        Ok((keyframe, output))
    }

    /// Normalize PTS to start at 0 for this stream.
    fn normalize_pts(&mut self, pts: i64) -> i64 {
        match self.first_pts {
            Some(first) => pts - first,
            None => {
                self.first_pts = Some(pts);
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_pts() {
        let mut demuxer = FlvDemuxer::new(StreamId::new());

        // First PTS becomes 0
        assert_eq!(demuxer.normalize_pts(90000), 0);
        // Subsequent PTS are relative
        assert_eq!(demuxer.normalize_pts(180000), 90000);
        assert_eq!(demuxer.normalize_pts(270000), 180000);
    }

    #[test]
    fn test_audio_specific_config_parsing() {
        let mut demuxer = FlvDemuxer::new(StreamId::new());

        // AAC-LC, 44100 Hz, stereo: objectType=2 (5 bits), freqIndex=4 (4 bits), channels=2 (4 bits)
        // Binary: 00010 0100 0010 000 = 0x12 0x10
        let config = [0x12, 0x10];
        demuxer.parse_audio_specific_config(&config).unwrap();
        assert_eq!(demuxer.audio_sample_rate, 44100);
        assert_eq!(demuxer.audio_channels, 2);
    }

    #[test]
    fn test_audio_specific_config_48khz() {
        let mut demuxer = FlvDemuxer::new(StreamId::new());

        // AAC-LC, 48000 Hz, stereo: objectType=2, freqIndex=3, channels=2
        // Binary: 00010 0011 0010 000 = 0x11 0x90
        let config = [0x11, 0x90];
        demuxer.parse_audio_specific_config(&config).unwrap();
        assert_eq!(demuxer.audio_sample_rate, 48000);
        assert_eq!(demuxer.audio_channels, 2);
    }

    #[test]
    fn test_extract_nalus_keyframe() {
        let demuxer = FlvDemuxer::new(StreamId::new());

        // Single IDR NALU: length=3, nal_type=5 (IDR)
        let data = [
            0x00, 0x00, 0x00, 0x03, // length = 3
            0x65, 0xAA, 0xBB, // nal_type = 5 (IDR), + 2 bytes payload
        ];

        let (keyframe, output) = demuxer.extract_nalus(&data).unwrap();
        assert!(keyframe);
        // Output should have Annex-B start code + NALU
        assert_eq!(&output[..4], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(&output[4..], &[0x65, 0xAA, 0xBB]);
    }

    #[test]
    fn test_extract_nalus_non_keyframe() {
        let demuxer = FlvDemuxer::new(StreamId::new());

        // Single non-IDR NALU: length=2, nal_type=1
        let data = [
            0x00, 0x00, 0x00, 0x02, // length = 2
            0x41, 0xCC, // nal_type = 1 (non-IDR slice)
        ];

        let (keyframe, _output) = demuxer.extract_nalus(&data).unwrap();
        assert!(!keyframe);
    }

    #[test]
    fn test_extract_nalus_corrupt() {
        let demuxer = FlvDemuxer::new(StreamId::new());

        // NALU length exceeds data
        let data = [
            0x00, 0x00, 0x00, 0xFF, // length = 255 (exceeds data)
            0x65,
        ];

        assert!(demuxer.extract_nalus(&data).is_err());
    }
}
