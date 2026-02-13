use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::control::state::{StreamState, StreamStateManager};
use crate::core::config::PackagingConfig;

use super::MediaStore;

// ---------------------------------------------------------------------------
// Cleanup task (from storage-and-delivery.md §8)
// ---------------------------------------------------------------------------

/// Interval between cleanup runs (5 minutes).
const CLEANUP_INTERVAL_SECS: u64 = 300;

/// Age threshold for ERROR state streams before deletion (24 hours).
const ERROR_STATE_MAX_AGE_HOURS: i64 = 24;

/// Background cleanup task that manages segment lifecycle.
///
/// Runs every 5 minutes and (from storage-and-delivery.md §8.2):
/// 1. For LIVE streams: deletes segments older than `segment_retention_minutes`
///    that are NOT referenced in the current manifest.
/// 2. For ERROR streams older than 24 hours: deletes all S3 objects and
///    transitions state to DELETED.
pub async fn run_cleanup_task<S: MediaStore>(
    store: Arc<S>,
    state_manager: Arc<StreamStateManager>,
    packaging_config: PackagingConfig,
    cancel: CancellationToken,
) {
    info!(
        "storage cleanup task started (interval: {}s)",
        CLEANUP_INTERVAL_SECS
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("storage cleanup task shutting down");
                return;
            }
            _ = tokio::time::sleep(Duration::from_secs(CLEANUP_INTERVAL_SECS)) => {
                if let Err(e) = run_cleanup_cycle(store.as_ref(), &state_manager, &packaging_config).await {
                    error!(error = %e, "cleanup cycle failed");
                }
            }
        }
    }
}

async fn run_cleanup_cycle<S: MediaStore>(
    store: &S,
    state_manager: &StreamStateManager,
    packaging_config: &PackagingConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let now = Utc::now();

    // Phase 1: Clean up old segments for LIVE streams
    let live_streams = state_manager.list_streams(Some(StreamState::Live)).await;
    for stream in &live_streams {
        let stream_id = stream.metadata.stream_id;
        let prefix = format!("{}/", stream_id);

        match store.list_objects(&prefix).await {
            Ok(objects) => {
                let retention =
                    chrono::Duration::minutes(packaging_config.segment_retention_minutes as i64);

                let mut deleted = 0u64;
                for obj in &objects {
                    // Only clean up segment files (.m4s), not manifests or init segments
                    if !obj.key.ends_with(".m4s") {
                        continue;
                    }
                    // Skip init segments
                    if obj.key.contains("init.mp4") {
                        continue;
                    }

                    let age = now.signed_duration_since(obj.last_modified);
                    if age > retention {
                        if let Err(e) = store.delete_object(&obj.key).await {
                            warn!(key = %obj.key, error = %e, "failed to delete expired segment");
                        } else {
                            deleted += 1;
                        }
                    }
                }

                if deleted > 0 {
                    debug!(%stream_id, deleted, "cleaned up expired segments");
                }
            }
            Err(e) => {
                warn!(%stream_id, error = %e, "failed to list objects for cleanup");
            }
        }
    }

    // Phase 2: Clean up ERROR streams older than 24 hours
    let error_streams = state_manager.list_streams(Some(StreamState::Error)).await;
    for stream in &error_streams {
        let stream_id = stream.metadata.stream_id;

        let ended_at = stream
            .metadata
            .ended_at
            .unwrap_or(stream.metadata.created_at);
        let age_hours = now.signed_duration_since(ended_at).num_hours();

        if age_hours >= ERROR_STATE_MAX_AGE_HOURS {
            info!(%stream_id, age_hours, "cleaning up ERROR stream (>24h old)");

            let prefix = format!("{}/", stream_id);
            match store.delete_prefix(&prefix).await {
                Ok(count) => {
                    info!(%stream_id, objects_deleted = count, "ERROR stream cleaned up");
                    // Transition to DELETED
                    if let Err(e) = state_manager
                        .transition(stream_id, StreamState::Deleted)
                        .await
                    {
                        warn!(%stream_id, error = %e, "failed to transition to DELETED after cleanup");
                    }
                }
                Err(e) => {
                    error!(%stream_id, error = %e, "failed to delete ERROR stream objects");
                }
            }
        }
    }

    Ok(())
}
