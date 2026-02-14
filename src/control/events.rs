use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::core::types::{MediaInfo, StreamId};
use crate::storage::MediaStore;

use super::state::{StreamState, StreamStateManager};

// ---------------------------------------------------------------------------
// Pipeline events (from control-plane-vs-data-plane.md §4.3)
// ---------------------------------------------------------------------------

/// Events sent from the data plane to the control plane.
///
/// Single `tokio::sync::mpsc` channel (capacity: 256) from data plane → control plane.
/// The control plane has a dedicated task that reads from this channel and updates
/// `StreamRecord` state.
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// Ingest has validated the stream and started receiving media.
    StreamStarted {
        stream_id: StreamId,
        media_info: MediaInfo,
    },

    /// A segment has been produced and written to storage.
    SegmentProduced {
        stream_id: StreamId,
        rendition: String,
        sequence: u64,
    },

    /// The stream has ended (RTMP disconnect or upload complete).
    StreamEnded { stream_id: StreamId },

    /// An unrecoverable error occurred in the data plane.
    StreamError { stream_id: StreamId, error: String },

    /// VOD transcode progress update (from control-plane-vs-data-plane.md §4.3).
    VodProgress { stream_id: StreamId, percent: f32 },

    /// A rendition has finished producing all segments.
    /// Used to trigger PROCESSING → READY auto-transition when all renditions complete.
    RenditionComplete {
        stream_id: StreamId,
        rendition: String,
    },
}

/// Event channel capacity (from control-plane-vs-data-plane.md §4.3).
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Create the shared event channel for data plane → control plane communication.
pub fn create_event_channel() -> (mpsc::Sender<PipelineEvent>, mpsc::Receiver<PipelineEvent>) {
    mpsc::channel(EVENT_CHANNEL_CAPACITY)
}

/// Run the event handler task that processes data plane events.
///
/// This task reads from the event channel and updates stream state accordingly.
/// It runs until the channel is closed (all senders dropped).
pub async fn run_event_handler<S: MediaStore>(
    mut rx: mpsc::Receiver<PipelineEvent>,
    state_manager: Arc<StreamStateManager>,
    store: Arc<S>,
) {
    info!("event handler task started");

    while let Some(event) = rx.recv().await {
        match event {
            PipelineEvent::StreamStarted {
                stream_id,
                media_info,
            } => {
                debug!(%stream_id, "processing StreamStarted event");
                if let Err(e) = state_manager.transition(stream_id, StreamState::Live).await {
                    error!(%stream_id, error = %e, "failed to transition to Live");
                }
                state_manager
                    .set_media_info(stream_id, media_info.clone())
                    .await;

                // Write stream metadata.json to storage (FR-STORAGE-04)
                write_metadata_json(store.as_ref(), stream_id, &media_info).await;
            }

            PipelineEvent::SegmentProduced {
                stream_id,
                rendition,
                sequence,
            } => {
                debug!(%stream_id, %rendition, sequence, "processing SegmentProduced event");
                state_manager
                    .update_rendition_progress(stream_id, &rendition, sequence)
                    .await;
            }

            PipelineEvent::StreamEnded { stream_id } => {
                info!(%stream_id, "processing StreamEnded event");
                if let Err(e) = state_manager
                    .transition(stream_id, StreamState::Processing)
                    .await
                {
                    error!(%stream_id, error = %e, "failed to transition to Processing");
                }
            }

            PipelineEvent::StreamError { stream_id, error } => {
                error!(%stream_id, %error, "processing StreamError event");
                if let Err(e) = state_manager.transition_to_error(stream_id, error).await {
                    error!(%stream_id, error = %e, "failed to transition to Error");
                }
            }

            PipelineEvent::VodProgress { stream_id, percent } => {
                debug!(%stream_id, percent, "VOD transcode progress");
                crate::observability::metrics::set_vod_progress(
                    &stream_id.to_string(),
                    percent as f64,
                );
            }

            PipelineEvent::RenditionComplete {
                stream_id,
                rendition,
            } => {
                info!(%stream_id, %rendition, "rendition complete");
                // mark_rendition_complete will auto-transition PROCESSING → READY
                // when all expected renditions are done (media-lifecycle.md §3.2)
                state_manager
                    .mark_rendition_complete(stream_id, &rendition)
                    .await;
            }
        }
    }

    info!("event handler task stopped (channel closed)");
}

/// Write stream metadata as JSON to storage alongside media objects (FR-STORAGE-04).
async fn write_metadata_json<S: MediaStore>(
    store: &S,
    stream_id: StreamId,
    media_info: &MediaInfo,
) {
    let metadata = serde_json::json!({
        "stream_id": stream_id.to_string(),
        "video_codec": format!("{}", media_info.video_codec),
        "video_width": media_info.video_width,
        "video_height": media_info.video_height,
        "frame_rate": media_info.frame_rate,
        "audio_codec": media_info.audio_codec.as_ref().map(|c| format!("{}", c)),
        "audio_sample_rate": media_info.audio_sample_rate,
        "audio_channels": media_info.audio_channels,
        "duration_secs": media_info.duration_secs,
        "container": format!("{}", media_info.container),
        "created_at": chrono::Utc::now().to_rfc3339(),
    });

    let path = format!("{}/metadata.json", stream_id);
    let content = serde_json::to_string_pretty(&metadata).unwrap_or_default();
    if let Err(e) = store.put_manifest(&path, &content).await {
        warn!(%stream_id, error = %e, "failed to write metadata.json to storage");
    } else {
        debug!(%stream_id, "metadata.json written to storage");
    }
}
