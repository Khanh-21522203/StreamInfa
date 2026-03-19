use std::collections::HashSet;
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
/// Retention for DELETED state records before dropping in-memory tombstones.
const DELETED_STATE_RETENTION_HOURS: i64 = 24;

/// Background cleanup task that manages segment lifecycle.
///
/// Runs every 5 minutes and (from storage-and-delivery.md §8.2):
/// 1. For LIVE streams: deletes segments older than `segment_retention_minutes`
///    that are NOT referenced in the current manifest.
/// 2. For ERROR streams older than 24 hours: deletes all S3 objects and
///    transitions state to DELETED.
/// 3. For DELETED streams older than 24 hours: removes in-memory tombstones.
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
                if let Err(e) = run_cleanup_cycle(
                    store.as_ref(),
                    &state_manager,
                    &packaging_config,
                    Utc::now(),
                ).await {
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
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Phase 1: Clean up old segments for LIVE streams
    let live_streams = state_manager.list_streams(Some(StreamState::Live)).await;
    for stream in &live_streams {
        let stream_id = stream.metadata.stream_id;
        let prefix = format!("{}/", stream_id);

        match store.list_objects(&prefix).await {
            Ok(objects) => {
                let retention =
                    chrono::Duration::minutes(packaging_config.segment_retention_minutes as i64);
                let mut referenced_segments: HashSet<String> = HashSet::new();

                // Build the set of segments still referenced by current media playlists.
                for obj in &objects {
                    if !obj.key.ends_with("media.m3u8") {
                        continue;
                    }
                    let playlist = match store.get_object(&obj.key).await {
                        Ok(p) => p,
                        Err(e) => {
                            warn!(playlist = %obj.key, error = %e, "failed to read media playlist during cleanup");
                            continue;
                        }
                    };

                    let parent = obj.key.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
                    for line in String::from_utf8_lossy(&playlist.body).lines() {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') || !line.ends_with(".m4s") {
                            continue;
                        }
                        let full_key = if line.contains('/') || parent.is_empty() {
                            line.to_string()
                        } else {
                            format!("{}/{}", parent, line)
                        };
                        referenced_segments.insert(full_key);
                    }
                }

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
                        if referenced_segments.contains(&obj.key) {
                            continue;
                        }
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

    // Phase 3: Remove DELETED tombstones after retention window
    let deleted_streams = state_manager.list_streams(Some(StreamState::Deleted)).await;
    for stream in &deleted_streams {
        let stream_id = stream.metadata.stream_id;
        let deleted_at = stream
            .metadata
            .ended_at
            .unwrap_or(stream.metadata.created_at);
        let age_hours = now.signed_duration_since(deleted_at).num_hours();

        if age_hours >= DELETED_STATE_RETENTION_HOURS
            && state_manager.remove_stream(stream_id).await
        {
            info!(%stream_id, age_hours, "removed DELETED stream tombstone after retention");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{IngestMode, StreamId};
    use crate::storage::memory::InMemoryMediaStore;
    use crate::storage::MediaStore;

    fn test_packaging_config() -> PackagingConfig {
        PackagingConfig {
            segment_duration_secs: 6,
            live_window_segments: 5,
            hls_version: 7,
            segment_retention_minutes: 60,
        }
    }

    #[tokio::test]
    async fn test_deleted_tombstone_removed_after_retention() {
        let store = InMemoryMediaStore::new();
        let state_manager = StreamStateManager::new();
        let stream_id = StreamId::new();

        state_manager
            .create_stream(stream_id, IngestMode::Live)
            .await;
        state_manager
            .transition(stream_id, StreamState::Live)
            .await
            .unwrap();
        state_manager
            .transition(stream_id, StreamState::Processing)
            .await
            .unwrap();
        state_manager
            .transition(stream_id, StreamState::Ready)
            .await
            .unwrap();
        state_manager
            .transition(stream_id, StreamState::Deleted)
            .await
            .unwrap();

        let future_now = Utc::now() + chrono::Duration::hours(DELETED_STATE_RETENTION_HOURS + 1);
        run_cleanup_cycle(&store, &state_manager, &test_packaging_config(), future_now)
            .await
            .unwrap();

        assert!(state_manager.get_stream(stream_id).await.is_none());
    }

    #[tokio::test]
    async fn test_deleted_tombstone_kept_before_retention() {
        let store = InMemoryMediaStore::new();
        let state_manager = StreamStateManager::new();
        let stream_id = StreamId::new();

        state_manager
            .create_stream(stream_id, IngestMode::Live)
            .await;
        state_manager
            .transition(stream_id, StreamState::Live)
            .await
            .unwrap();
        state_manager
            .transition(stream_id, StreamState::Processing)
            .await
            .unwrap();
        state_manager
            .transition(stream_id, StreamState::Ready)
            .await
            .unwrap();
        state_manager
            .transition(stream_id, StreamState::Deleted)
            .await
            .unwrap();

        let near_now = Utc::now() + chrono::Duration::hours(1);
        run_cleanup_cycle(&store, &state_manager, &test_packaging_config(), near_now)
            .await
            .unwrap();

        assert!(state_manager.get_stream(stream_id).await.is_some());
    }

    #[tokio::test]
    async fn test_live_cleanup_keeps_manifest_referenced_segment() {
        let store = InMemoryMediaStore::new();
        let state_manager = StreamStateManager::new();
        let stream_id = StreamId::new();
        let sid = stream_id.to_string();

        state_manager
            .create_stream(stream_id, IngestMode::Live)
            .await;
        state_manager
            .transition(stream_id, StreamState::Live)
            .await
            .unwrap();

        let seg_key = format!("{}/high/000000.m4s", sid);
        let playlist_key = format!("{}/high/media.m3u8", sid);
        store
            .put_segment(&seg_key, bytes::Bytes::from_static(b"segment"), "video/mp4")
            .await
            .unwrap();
        store
            .put_manifest(&playlist_key, "#EXTM3U\n#EXTINF:6.0,\n000000.m4s\n")
            .await
            .unwrap();

        let mut cfg = test_packaging_config();
        cfg.segment_retention_minutes = 0;
        let now = Utc::now() + chrono::Duration::seconds(1);
        run_cleanup_cycle(&store, &state_manager, &cfg, now)
            .await
            .unwrap();

        assert!(store.exists(&seg_key));
    }

    #[tokio::test]
    async fn test_live_cleanup_deletes_unreferenced_segment() {
        let store = InMemoryMediaStore::new();
        let state_manager = StreamStateManager::new();
        let stream_id = StreamId::new();
        let sid = stream_id.to_string();

        state_manager
            .create_stream(stream_id, IngestMode::Live)
            .await;
        state_manager
            .transition(stream_id, StreamState::Live)
            .await
            .unwrap();

        let seg_key = format!("{}/high/000000.m4s", sid);
        let playlist_key = format!("{}/high/media.m3u8", sid);
        store
            .put_segment(&seg_key, bytes::Bytes::from_static(b"segment"), "video/mp4")
            .await
            .unwrap();
        store
            .put_manifest(&playlist_key, "#EXTM3U\n#EXTINF:6.0,\n000001.m4s\n")
            .await
            .unwrap();

        let mut cfg = test_packaging_config();
        cfg.segment_retention_minutes = 0;
        let now = Utc::now() + chrono::Duration::seconds(1);
        run_cleanup_cycle(&store, &state_manager, &cfg, now)
            .await
            .unwrap();

        assert!(!store.exists(&seg_key));
    }
}
