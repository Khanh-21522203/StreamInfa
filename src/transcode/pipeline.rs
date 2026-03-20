use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
#[cfg(not(feature = "ffmpeg"))]
use tracing::warn;
use tracing::{error, info};

use crate::control::state::StreamStateManager;
use crate::core::config::TranscodeConfig;
use crate::core::error::TranscodeError;
use crate::core::types::{DemuxedFrame, EncodedSegment, StreamId};
use crate::observability::metrics as obs;

use super::profile::select_renditions;
#[cfg(not(feature = "ffmpeg"))]
use super::profile::SelectedRendition;
#[cfg(not(feature = "ffmpeg"))]
use super::segment::{EncodedPacket, SegmentAccumulator};

// ---------------------------------------------------------------------------
// Constants (from transcoding-and-packaging.md §3.2, §10)
// ---------------------------------------------------------------------------

/// Max consecutive decode errors before aborting (from transcoding-and-packaging.md §3.2).
#[cfg(not(feature = "ffmpeg"))]
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
    state_manager: Arc<StreamStateManager>,
    cancel: CancellationToken,
}

impl TranscodePipeline {
    pub fn new(
        config: TranscodeConfig,
        state_manager: Arc<StreamStateManager>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            state_manager,
            cancel,
        }
    }

    // -----------------------------------------------------------------------
    // FFmpeg implementation (real H.264 decode + libx264 encode)
    // -----------------------------------------------------------------------

    /// Start a live transcode pipeline using real FFmpeg transcoding.
    ///
    /// Architecture:
    /// 1. Select renditions based on source resolution.
    /// 2. Bridge the async ingest channel to a blocking FFmpeg thread via a
    ///    tokio mpsc channel — `blocking_recv()` on the blocking side avoids
    ///    blocking the async runtime.
    /// 3. The blocking thread decodes each frame once and encodes N renditions.
    /// 4. Cancellation drops the bridge sender, which causes the blocking
    ///    thread's `blocking_recv()` to return `None` and exit cleanly.
    #[cfg(feature = "ffmpeg")]
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

        obs::inc_transcode_active_jobs();
        let _active_job_guard = TranscodeActiveJobGuard;

        self.state_manager
            .set_expected_renditions(stream_id, renditions.len())
            .await;

        info!(
            %stream_id,
            rendition_count = renditions.len(),
            source = format!("{}x{}@{:.1}fps", source_width, source_height, source_fps),
            "starting FFmpeg transcode pipeline"
        );

        // Bridge: async frame_rx → blocking FFmpeg thread.
        let (bridge_tx, bridge_rx) = mpsc::channel::<DemuxedFrame>(64);
        let target_duration = self.config.keyframe_interval_secs as f64 * 3.0; // ~6s
        let cancel = self.cancel.clone();
        let stream_id_str = stream_id.to_string();

        // Spawn blocking FFmpeg pipeline.
        let ffmpeg_handle = tokio::task::spawn_blocking(move || {
            super::ffmpeg::transcode_blocking(
                stream_id,
                bridge_rx,
                segment_tx,
                renditions,
                source_width,
                source_height,
                source_fps,
                target_duration,
            )
        });

        // Feed frames from the ingest channel into the bridge.
        'feed: loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(%stream_id, "transcode cancelled");
                    break 'feed;
                }
                frame = frame_rx.recv() => {
                    match frame {
                        Some(f) => {
                            obs::set_transcode_queue_depth(
                                &stream_id_str,
                                frame_rx.len() as f64,
                            );
                            if bridge_tx.send(f).await.is_err() {
                                // FFmpeg thread exited early (error or done).
                                break 'feed;
                            }
                        }
                        None => {
                            info!(%stream_id, "ingest channel closed, flushing FFmpeg pipeline");
                            break 'feed;
                        }
                    }
                }
            }
        }

        // Drop bridge_tx to signal EOF to the blocking thread.
        drop(bridge_tx);

        // Wait for the FFmpeg thread to finish and propagate any error.
        let result = ffmpeg_handle
            .await
            .map_err(|e| TranscodeError::FfmpegInit {
                reason: format!("spawn_blocking join: {e}"),
            })?;

        if let Err(ref e) = result {
            error!(%stream_id, error = %e, "FFmpeg transcode pipeline error");
        } else {
            info!(%stream_id, "FFmpeg transcode pipeline finished");
        }

        result
    }

    // -----------------------------------------------------------------------
    // FFmpeg VOD from file
    // -----------------------------------------------------------------------

    /// Transcode a VOD file directly using FFmpeg's container demuxer.
    ///
    /// Unlike `run_live`, this does not use an ingest channel — FFmpeg opens the
    /// file itself via `ffmpeg::format::input`, demuxes it, and feeds packets
    /// directly to the decoder. This correctly handles codec extradata (SPS/PPS)
    /// embedded in MP4/MKV containers.
    #[cfg(feature = "ffmpeg")]
    pub async fn run_vod_from_file(
        &self,
        stream_id: StreamId,
        file_path: std::path::PathBuf,
        source_width: u32,
        source_height: u32,
        source_fps: f64,
        segment_tx: mpsc::Sender<EncodedSegment>,
    ) -> Result<(), TranscodeError> {
        let renditions =
            select_renditions(source_width, source_height, &self.config.profile_ladder);

        if renditions.is_empty() {
            return Err(TranscodeError::FfmpegInit {
                reason: "no renditions selected for source resolution".to_string(),
            });
        }

        obs::inc_transcode_active_jobs();
        let _active_job_guard = TranscodeActiveJobGuard;

        self.state_manager
            .set_expected_renditions(stream_id, renditions.len())
            .await;

        info!(
            %stream_id,
            rendition_count = renditions.len(),
            source = format!("{}x{}@{:.1}fps", source_width, source_height, source_fps),
            "starting FFmpeg VOD transcode from file"
        );

        let target_duration = self.config.keyframe_interval_secs as f64 * 3.0;
        let cancel = self.cancel.clone();

        let result = tokio::task::spawn_blocking(move || {
            super::ffmpeg::transcode_vod_from_file_blocking(
                stream_id,
                file_path,
                segment_tx,
                renditions,
                source_fps,
                target_duration,
                cancel,
            )
        })
        .await
        .map_err(|e| TranscodeError::FfmpegInit {
            reason: format!("spawn_blocking join: {e}"),
        })?;

        if let Err(ref e) = result {
            error!(%stream_id, error = %e, "FFmpeg VOD transcode error");
        } else {
            info!(%stream_id, "FFmpeg VOD transcode finished");
        }

        result
    }

    // -----------------------------------------------------------------------
    // Placeholder implementation (passthrough — no real transcoding)
    // -----------------------------------------------------------------------

    /// Start a live transcode pipeline for a stream.
    ///
    /// Task structure (from transcoding-and-packaging.md §5.2):
    /// - spawn_blocking: Decoder task (reads from ingest channel)
    /// - spawn_blocking: Encoder task per rendition (receives decoded frames, encodes, segments)
    ///
    /// In a full implementation, the decoder and encoders communicate via
    /// crossbeam::channel (bounded, capacity 8) since both run on blocking threads.
    /// For now, this is structured to show the architecture with placeholder FFmpeg calls.
    #[cfg(not(feature = "ffmpeg"))]
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

        obs::inc_transcode_active_jobs();
        let _active_job_guard = TranscodeActiveJobGuard;

        // Register expected rendition count for PROCESSING → READY auto-transition
        // (media-lifecycle.md §3.2 Step 8)
        self.state_manager
            .set_expected_renditions(stream_id, renditions.len())
            .await;

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
        let stream_id_str = stream_id.to_string();

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
            obs::set_transcode_queue_depth(&stream_id_str, frame_rx.len() as f64);
            match self
                .process_frame(&frame, &stream_id_str, &mut accumulators, &segment_tx)
                .await
            {
                Ok(()) => {
                    consecutive_decode_errors = 0;
                    let frame_latency = frame_start.elapsed().as_secs_f64();
                    let fps = 1.0 / frame_latency.max(0.001);
                    for (rendition, _) in accumulators.iter() {
                        obs::record_transcode_latency(rendition.id.as_str(), frame_latency);
                        obs::set_transcode_fps(&stream_id_str, rendition.id.as_str(), fps);
                    }
                }
                Err(e) => {
                    consecutive_decode_errors += 1;
                    obs::inc_transcode_error(&stream_id_str, transcode_error_type(&e));
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
                if crate::core::metrics::send_with_backpressure(
                    &segment_tx,
                    segment,
                    "transcode_packager",
                    crate::core::types::TRANSCODE_PACKAGER_CHANNEL_CAP,
                )
                .await
                .is_err()
                {
                    warn!(%stream_id, rendition = %rendition.id, "packager channel closed during flush");
                }
            }
        }

        info!(%stream_id, "live transcode pipeline finished");
        Ok(())
    }

    /// Process a single demuxed frame through all rendition encoders.
    ///
    /// In a real implementation with FFmpeg FFI:
    /// 1. Feed NALU to decoder via avcodec_send_packet
    /// 2. Receive decoded YUV frame via avcodec_receive_frame
    /// 3. For each rendition: scale + encode + push to accumulator
    #[cfg(not(feature = "ffmpeg"))]
    async fn process_frame(
        &self,
        frame: &DemuxedFrame,
        stream_id_str: &str,
        accumulators: &mut [(SelectedRendition, SegmentAccumulator)],
        segment_tx: &mpsc::Sender<EncodedSegment>,
    ) -> Result<(), TranscodeError> {
        let is_audio = frame.track.is_audio();

        // Capture audio parameters from the track info so the packager can
        // derive the correct AudioSpecificConfig in the init segment.
        if is_audio {
            if let crate::core::types::Track::Audio {
                sample_rate,
                channels,
                ..
            } = frame.track
            {
                for (_, acc) in accumulators.iter_mut() {
                    acc.set_audio_params(sample_rate, channels);
                }
            }
        }

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
                obs::inc_transcode_segments(stream_id_str, rendition.id.as_str());
                obs::record_transcode_segment_duration(
                    rendition.id.as_str(),
                    segment.duration_secs,
                );
                if crate::core::metrics::send_with_backpressure(
                    segment_tx,
                    segment,
                    "transcode_packager",
                    crate::core::types::TRANSCODE_PACKAGER_CHANNEL_CAP,
                )
                .await
                .is_err()
                {
                    return Err(TranscodeError::Cancelled {
                        stream_id: frame.stream_id,
                    });
                }
            }
        }

        Ok(())
    }
}

#[cfg(not(feature = "ffmpeg"))]
fn transcode_error_type(err: &TranscodeError) -> &'static str {
    match err {
        TranscodeError::FfmpegInit { .. } => "ffmpeg_init",
        TranscodeError::Cancelled { .. } => "cancelled",
        TranscodeError::ConsecutiveDecodeErrors { .. } => "decode_failed",
    }
}

struct TranscodeActiveJobGuard;

impl Drop for TranscodeActiveJobGuard {
    fn drop(&mut self) {
        obs::dec_transcode_active_jobs();
    }
}
