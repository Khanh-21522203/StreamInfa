use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::control::state::StreamStateManager;
use crate::core::config::TranscodeConfig;
use crate::core::error::TranscodeError;
use crate::core::types::{DemuxedFrame, EncodedSegment, StreamId};
use crate::observability::metrics as obs;

use super::profile::{select_renditions, SelectedRendition};
use super::segment::{EncodedPacket, SegmentAccumulator};

// ---------------------------------------------------------------------------
// Constants (from transcoding-and-packaging.md §3.2, §10)
// ---------------------------------------------------------------------------

/// Max consecutive decode errors before aborting (from transcoding-and-packaging.md §3.2).
const MAX_CONSECUTIVE_DECODE_ERRORS: u32 = 10;

// ---------------------------------------------------------------------------
// Transcode pipeline (from transcoding-and-packaging.md §5)
// ---------------------------------------------------------------------------

/// Orchestrates the transcode pipeline for a single stream.
///
/// Key design principle (from transcoding-and-packaging.md §1):
/// The decoder runs once. Its raw YUV output is fanned out to N encoders
/// (one per rendition). This avoids decoding the same input N times.
///
/// Live vs VOD differences (from transcoding-and-packaging.md §5.1):
/// - Live: continuous DemuxedFrame stream, real-time, immediate segment emission
/// - VOD: sequential file read, offline, batch segment emission
pub struct TranscodePipeline {
    config: TranscodeConfig,
    cancel: CancellationToken,
}

impl TranscodePipeline {
    pub fn new(
        config: TranscodeConfig,
        _state_manager: Arc<StreamStateManager>,
        cancel: CancellationToken,
    ) -> Self {
        Self { config, cancel }
    }

    /// Start a live transcode pipeline for a stream.
    ///
    /// Task structure (from transcoding-and-packaging.md §5.2):
    /// - spawn_blocking: Decoder task (reads from ingest channel)
    /// - spawn_blocking: Encoder task per rendition (receives decoded frames, encodes, segments)
    ///
    /// In a full implementation, the decoder and encoders communicate via
    /// crossbeam::channel (bounded, capacity 8) since both run on blocking threads.
    /// For now, this is structured to show the architecture with placeholder FFmpeg calls.
    pub async fn run_live(
        &self,
        stream_id: StreamId,
        source_width: u32,
        source_height: u32,
        source_fps: f64,
        mut frame_rx: mpsc::Receiver<DemuxedFrame>,
        segment_tx: mpsc::Sender<EncodedSegment>,
    ) -> Result<(), TranscodeError> {
        let renditions =
            select_renditions(source_width, source_height, &self.config.profile_ladder);

        if renditions.is_empty() {
            return Err(TranscodeError::FfmpegInit {
                reason: "no renditions selected for source resolution".to_string(),
            });
        }

        obs::set_transcode_active_jobs(1.0);
        info!(
            %stream_id,
            rendition_count = renditions.len(),
            source = format!("{}x{}@{:.1}fps", source_width, source_height, source_fps),
            "starting live transcode pipeline"
        );

        // Create a segment accumulator per rendition
        let mut accumulators: Vec<(SelectedRendition, SegmentAccumulator)> = renditions
            .iter()
            .map(|r| {
                let acc = SegmentAccumulator::new(
                    stream_id,
                    r.id.clone(),
                    self.config.keyframe_interval_secs as f64 * 3.0, // target ~6s (3 GOPs of 2s each)
                    r.width,
                    r.height,
                    r.video_bitrate_kbps,
                    r.audio_bitrate_kbps,
                    r.profile.clone(),
                    r.level.clone(),
                    source_fps,
                );
                (r.clone(), acc)
            })
            .collect();

        let mut consecutive_decode_errors: u32 = 0;

        // Main decode loop
        loop {
            let frame = tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!(%stream_id, "transcode pipeline cancelled");
                    break;
                }
                frame = frame_rx.recv() => {
                    match frame {
                        Some(f) => f,
                        None => {
                            info!(%stream_id, "ingest channel closed, flushing");
                            break;
                        }
                    }
                }
            };

            // Process frame through decode → scale → encode per rendition
            // In a real implementation, this would:
            // 1. Decode H.264 NALU to raw YUV via FFmpeg avcodec_send_packet/receive_frame
            // 2. For each rendition: scale via swscale, encode via libx264
            // 3. Feed encoded packets to segment accumulator
            //
            // For now, we simulate by passing through the raw data as "encoded" packets.
            // This placeholder will be replaced with actual FFmpeg FFI calls.

            let frame_start = std::time::Instant::now();
            obs::set_transcode_queue_depth(&stream_id.to_string(), segment_tx.capacity() as f64);
            match self
                .process_frame(&frame, &mut accumulators, &segment_tx)
                .await
            {
                Ok(()) => {
                    consecutive_decode_errors = 0;
                    obs::record_transcode_latency("all", frame_start.elapsed().as_secs_f64());
                    obs::set_transcode_fps(
                        &stream_id.to_string(),
                        "all",
                        1.0 / frame_start.elapsed().as_secs_f64().max(0.001),
                    );
                }
                Err(e) => {
                    consecutive_decode_errors += 1;
                    obs::inc_transcode_error(&stream_id.to_string(), &e.to_string());
                    if consecutive_decode_errors >= MAX_CONSECUTIVE_DECODE_ERRORS {
                        error!(
                            %stream_id,
                            errors = consecutive_decode_errors,
                            "too many consecutive decode errors, aborting"
                        );
                        return Err(TranscodeError::ConsecutiveDecodeErrors {
                            stream_id,
                            count: consecutive_decode_errors,
                        });
                    }
                    warn!(%stream_id, error = %e, "decode error, skipping frame");
                }
            }
        }

        // Flush all accumulators
        for (rendition, acc) in accumulators {
            if let Some(segment) = acc.flush() {
                if segment_tx.send(segment).await.is_err() {
                    warn!(%stream_id, rendition = %rendition.id, "packager channel closed during flush");
                }
            }
        }

        obs::set_transcode_active_jobs(0.0);
        info!(%stream_id, "live transcode pipeline finished");
        Ok(())
    }

    /// Process a single demuxed frame through all rendition encoders.
    ///
    /// In a real implementation with FFmpeg FFI:
    /// 1. Feed NALU to decoder via avcodec_send_packet
    /// 2. Receive decoded YUV frame via avcodec_receive_frame
    /// 3. For each rendition: scale + encode + push to accumulator
    async fn process_frame(
        &self,
        frame: &DemuxedFrame,
        accumulators: &mut [(SelectedRendition, SegmentAccumulator)],
        segment_tx: &mpsc::Sender<EncodedSegment>,
    ) -> Result<(), TranscodeError> {
        let is_audio = frame.track.is_audio();

        // Create an encoded packet (placeholder: pass through raw data)
        let packet = EncodedPacket {
            pts: frame.pts,
            _dts: frame.dts,
            keyframe: frame.keyframe,
            data: frame.data.clone(),
            is_audio,
        };

        // Fan out to all rendition accumulators
        for (rendition, acc) in accumulators.iter_mut() {
            if let Some(segment) = acc.push(packet.clone()) {
                obs::inc_transcode_segments(
                    &frame.stream_id.to_string(),
                    &rendition.id.to_string(),
                );
                obs::record_transcode_segment_duration(
                    &rendition.id.to_string(),
                    segment.duration_secs,
                );
                if segment_tx.send(segment).await.is_err() {
                    return Err(TranscodeError::Cancelled {
                        stream_id: frame.stream_id,
                    });
                }
            }
        }

        Ok(())
    }
}
