use std::collections::HashMap;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::control::events::PipelineEvent;
use crate::core::config::PackagingConfig;
use crate::core::types::{EncodedSegment, RenditionId, StorageWrite, StreamId};
use crate::observability::metrics as obs;

use super::hls::{self, InitSegmentParams, MediaSegmentParams, SampleInfo};
use super::manifest::{self, RenditionInfo};
use super::segment_index::SegmentIndex;

// ---------------------------------------------------------------------------
// Packager orchestration task (from transcoding-and-packaging.md §7)
// ---------------------------------------------------------------------------

/// Run the packager task for a single stream.
///
/// This task:
/// 1. Reads `EncodedSegment` from the transcode stage
/// 2. Generates fMP4 init segments and media segments
/// 3. Maintains per-rendition segment indexes (sliding window for live)
/// 4. Generates/updates HLS playlists (media + multivariant)
/// 5. Writes all outputs as `StorageWrite` to the storage channel
/// 6. Emits `SegmentProduced` events to the control plane
pub async fn run_packager(
    stream_id: StreamId,
    config: PackagingConfig,
    mut segment_rx: mpsc::Receiver<EncodedSegment>,
    storage_tx: mpsc::Sender<StorageWrite>,
    event_tx: mpsc::Sender<PipelineEvent>,
    cancel: CancellationToken,
) {
    info!(%stream_id, "packager task started");

    let mut indexes: HashMap<RenditionId, SegmentIndex> = HashMap::new();
    let mut rendition_infos: Vec<RenditionInfo> = Vec::new();
    let mut init_segments_written: HashMap<RenditionId, bool> = HashMap::new();

    loop {
        let segment = tokio::select! {
            _ = cancel.cancelled() => {
                info!(%stream_id, "packager cancelled");
                break;
            }
            seg = segment_rx.recv() => {
                match seg {
                    Some(s) => s,
                    None => {
                        info!(%stream_id, "transcode channel closed, flushing packager");
                        break;
                    }
                }
            }
        };

        let rendition = segment.rendition.clone();
        let sequence = segment.sequence;

        // Ensure we have a segment index for this rendition
        let index = indexes.entry(rendition.clone()).or_insert_with(|| {
            SegmentIndex::new(
                stream_id,
                rendition.clone(),
                config.live_window_segments as usize,
            )
        });

        // Generate init segment on first segment for each rendition
        if !init_segments_written.contains_key(&rendition) {
            let init_params = InitSegmentParams {
                width: segment.width,
                height: segment.height,
                video_timescale: 90000,
                audio_timescale: 48000,
                sps: Bytes::from_static(&[0x67, 0x42, 0x00, 0x1e]), // placeholder SPS
                pps: Bytes::from_static(&[0x68, 0xce, 0x38, 0x80]), // placeholder PPS
                audio_specific_config: segment.audio_data.as_ref().map(|_| {
                    Bytes::from_static(&[0x12, 0x10]) // placeholder AAC config
                }),
                has_audio: segment.audio_data.is_some(),
            };

            match hls::generate_init_segment(&init_params) {
                Ok(init_data) => {
                    let init_path =
                        manifest::init_segment_path(&stream_id.to_string(), &rendition.to_string());
                    let write = StorageWrite {
                        path: init_path,
                        data: init_data,
                        content_type: "video/mp4".to_string(),
                    };
                    if crate::core::metrics::send_with_backpressure(
                        &storage_tx,
                        write,
                        "packager_storage",
                        crate::core::types::PACKAGER_STORAGE_CHANNEL_CAP,
                    )
                    .await
                    .is_err()
                    {
                        warn!(%stream_id, "storage channel closed during init segment write");
                        break;
                    }
                    init_segments_written.insert(rendition.clone(), true);
                    debug!(%stream_id, rendition = %rendition, "init segment written");
                }
                Err(e) => {
                    error!(%stream_id, rendition = %rendition, error = %e, "failed to generate init segment");
                }
            }

            // Track rendition info for multivariant playlist using actual segment metadata
            rendition_infos.push(RenditionInfo {
                id: rendition.clone(),
                width: segment.width,
                height: segment.height,
                video_bitrate_kbps: segment.video_bitrate_kbps,
                audio_bitrate_kbps: segment.audio_bitrate_kbps,
                profile: segment.profile.clone(),
                level: segment.level.clone(),
                frame_rate: segment.frame_rate,
                has_audio: segment.audio_data.is_some(),
            });
        }

        // Generate fMP4 media segment
        let sample = SampleInfo {
            duration: (segment.duration_secs * 90000.0) as u32,
            size: segment.video_data.len() as u32,
            flags: if sequence == 0 {
                0x02000000
            } else {
                0x01010000
            },
            composition_offset: 0,
        };

        let media_params = MediaSegmentParams {
            sequence_number: sequence,
            base_decode_time: segment.pts_start,
            samples: vec![sample],
            media_data: segment.video_data.clone(),
            track_id: 1,
        };

        let mux_start = std::time::Instant::now();
        match hls::generate_media_segment(&media_params) {
            Ok(segment_data) => {
                obs::record_package_mux_duration(
                    &rendition.to_string(),
                    mux_start.elapsed().as_secs_f64(),
                );
                let seg_path = manifest::segment_path(
                    &stream_id.to_string(),
                    &rendition.to_string(),
                    sequence,
                );
                let segment_size = segment_data.len() as u64;
                let content_type = crate::storage::content_type_for_path(&seg_path).to_string();
                let write = StorageWrite {
                    path: seg_path.clone(),
                    data: segment_data,
                    content_type,
                };
                if crate::core::metrics::send_with_backpressure(
                    &storage_tx,
                    write,
                    "packager_storage",
                    crate::core::types::PACKAGER_STORAGE_CHANNEL_CAP,
                )
                .await
                .is_err()
                {
                    warn!(%stream_id, "storage channel closed during segment write");
                    break;
                }

                // Update segment index
                index.add_segment(segment.duration_secs, seg_path, segment_size);

                obs::inc_package_segments_written(&stream_id.to_string(), &rendition.to_string());

                // Generate and write updated media playlist
                let playlist_content = index.generate_playlist(
                    config.hls_version,
                    false, // live
                    segment.is_last,
                );
                let playlist_path =
                    manifest::media_playlist_path(&stream_id.to_string(), &rendition.to_string());
                let playlist_write = StorageWrite {
                    path: playlist_path,
                    data: Bytes::from(playlist_content),
                    content_type: "application/vnd.apple.mpegurl".to_string(),
                };
                if crate::core::metrics::send_with_backpressure(
                    &storage_tx,
                    playlist_write,
                    "packager_storage",
                    crate::core::types::PACKAGER_STORAGE_CHANNEL_CAP,
                )
                .await
                .is_err()
                {
                    warn!(%stream_id, "storage channel closed during playlist write");
                    break;
                }

                obs::inc_package_manifest_updates(&stream_id.to_string(), &rendition.to_string());

                // Emit SegmentProduced event
                let _ = event_tx
                    .send(PipelineEvent::SegmentProduced {
                        stream_id,
                        rendition: rendition.to_string(),
                        sequence,
                    })
                    .await;

                // If this is the last segment for this rendition, emit RenditionComplete
                // to trigger PROCESSING → READY auto-transition (media-lifecycle.md §3.2)
                if segment.is_last {
                    let _ = event_tx
                        .send(PipelineEvent::RenditionComplete {
                            stream_id,
                            rendition: rendition.to_string(),
                        })
                        .await;
                    info!(%stream_id, rendition = %rendition, "rendition complete, last segment emitted");
                }

                debug!(
                    %stream_id,
                    rendition = %rendition,
                    sequence,
                    "segment packaged and written"
                );
            }
            Err(e) => {
                error!(
                    %stream_id,
                    rendition = %rendition,
                    sequence,
                    error = %e,
                    "failed to generate media segment"
                );
            }
        }
    }

    // Write final multivariant playlist
    if !rendition_infos.is_empty() {
        let master_content =
            manifest::generate_multivariant_playlist(&rendition_infos, config.hls_version);
        let master_path = manifest::master_playlist_path(&stream_id.to_string());
        let write = StorageWrite {
            path: master_path,
            data: Bytes::from(master_content),
            content_type: "application/vnd.apple.mpegurl".to_string(),
        };
        let _ = crate::core::metrics::send_with_backpressure(
            &storage_tx,
            write,
            "packager_storage",
            crate::core::types::PACKAGER_STORAGE_CHANNEL_CAP,
        )
        .await;
    }

    info!(%stream_id, "packager task finished");
}
