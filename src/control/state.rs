use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tracing::{debug, info, warn};

use crate::core::error::StateError;
use crate::core::types::{IngestMode, MediaInfo, StreamId, StreamMetadata};
use crate::observability::metrics as obs;

/// Stream lifecycle states as defined in the media lifecycle state machine.
///
/// ```text
/// PENDING → LIVE → PROCESSING → READY → DELETED
///                                          ↑
///            (any state) ──────────────► ERROR
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum StreamState {
    /// Stream key created, awaiting ingest.
    Pending,
    /// Actively receiving and processing media in real time (live only).
    Live,
    /// Transcoding and packaging in progress.
    Processing,
    /// All renditions available for playback.
    Ready,
    /// Unrecoverable failure occurred.
    Error,
    /// Assets removed, metadata retained for audit.
    Deleted,
}

impl std::fmt::Display for StreamState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamState::Pending => write!(f, "pending"),
            StreamState::Live => write!(f, "live"),
            StreamState::Processing => write!(f, "processing"),
            StreamState::Ready => write!(f, "ready"),
            StreamState::Error => write!(f, "error"),
            StreamState::Deleted => write!(f, "deleted"),
        }
    }
}

/// Per-rendition progress tracking (from control-plane-vs-data-plane.md §2.1).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RenditionStatus {
    pub rendition: String,
    pub segments_produced: u64,
    pub last_segment_time: Option<chrono::DateTime<chrono::Utc>>,
    pub status: RenditionState,
}

/// State of a single rendition within a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RenditionState {
    /// Rendition is actively being produced.
    Active,
    /// Rendition is complete (all segments written).
    Complete,
}

/// A tracked stream with its current state and metadata.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StreamEntry {
    pub state: StreamState,
    pub metadata: StreamMetadata,
    pub error_message: Option<String>,
    /// Per-rendition progress (from control-plane-vs-data-plane.md §2.1).
    pub renditions: HashMap<String, RenditionStatus>,
    /// Expected number of renditions for this stream (set by transcode pipeline).
    pub expected_renditions: Option<usize>,
}

/// In-memory stream state manager backed by `DashMap` for lock-free concurrent access.
///
/// Manages the lifecycle of all streams, enforcing valid state transitions
/// as defined in the media lifecycle document.
#[derive(Clone)]
pub struct StreamStateManager {
    streams: DashMap<StreamId, StreamEntry>,
    /// Per-state counters for O(1) gauge updates. Indexed by `state_index()`.
    /// Arc so that `Clone` shares the same counters (production always uses Arc<Self>).
    state_counts: Arc<[AtomicI64; 6]>,
}

impl StreamStateManager {
    pub fn new() -> Self {
        Self {
            streams: DashMap::new(),
            state_counts: Arc::new([
                AtomicI64::new(0),
                AtomicI64::new(0),
                AtomicI64::new(0),
                AtomicI64::new(0),
                AtomicI64::new(0),
                AtomicI64::new(0),
            ]),
        }
    }

    /// Map a `StreamState` to its index in `state_counts`.
    #[inline]
    fn state_index(state: StreamState) -> usize {
        match state {
            StreamState::Pending => 0,
            StreamState::Live => 1,
            StreamState::Processing => 2,
            StreamState::Ready => 3,
            StreamState::Error => 4,
            StreamState::Deleted => 5,
        }
    }

    /// Create a new stream in PENDING state.
    pub async fn create_stream(&self, stream_id: StreamId, ingest_mode: IngestMode) -> StreamEntry {
        let metadata = StreamMetadata::new(stream_id, ingest_mode);
        let entry = StreamEntry {
            state: StreamState::Pending,
            metadata,
            error_message: None,
            renditions: HashMap::new(),
            expected_renditions: None,
        };

        self.streams.insert(stream_id, entry.clone());
        self.state_counts[Self::state_index(StreamState::Pending)]
            .fetch_add(1, Ordering::Relaxed);
        self.update_stream_gauges();

        info!(%stream_id, mode = %ingest_mode, "stream created in PENDING state");
        entry
    }

    /// Transition a stream to a new state, enforcing valid transitions.
    pub async fn transition(
        &self,
        stream_id: StreamId,
        new_state: StreamState,
    ) -> Result<StreamEntry, StateError> {
        let mut entry_ref = self
            .streams
            .get_mut(&stream_id)
            .ok_or(StateError::StreamNotFound { stream_id })?;

        let current = entry_ref.state;

        if !Self::is_valid_transition(current, new_state, entry_ref.metadata.ingest_mode) {
            return Err(StateError::InvalidTransition {
                stream_id,
                from: current.to_string(),
                to: new_state.to_string(),
            });
        }

        // Update timestamps on specific transitions
        match new_state {
            StreamState::Live => {
                entry_ref.metadata.started_at = Some(chrono::Utc::now());
            }
            StreamState::Ready | StreamState::Error | StreamState::Deleted => {
                entry_ref.metadata.ended_at = Some(chrono::Utc::now());
            }
            _ => {}
        }

        entry_ref.state = new_state;
        let updated = entry_ref.clone();
        drop(entry_ref);

        if matches!(
            new_state,
            StreamState::Ready | StreamState::Error | StreamState::Deleted
        ) {
            obs::clear_stream_metrics(&stream_id.to_string());
        }

        // Update per-state counters (O(1) instead of O(n) scan)
        self.state_counts[Self::state_index(current)].fetch_sub(1, Ordering::Relaxed);
        self.state_counts[Self::state_index(new_state)].fetch_add(1, Ordering::Relaxed);
        self.update_stream_gauges();

        info!(
            %stream_id,
            from = %current,
            to = %new_state,
            "stream state transition"
        );

        Ok(updated)
    }

    /// Transition a stream to ERROR state with an error message.
    pub async fn transition_to_error(
        &self,
        stream_id: StreamId,
        error_message: String,
    ) -> Result<StreamEntry, StateError> {
        let mut entry_ref = self
            .streams
            .get_mut(&stream_id)
            .ok_or(StateError::StreamNotFound { stream_id })?;

        let current = entry_ref.state;

        // ERROR is reachable from any state except DELETED
        if current == StreamState::Deleted {
            return Err(StateError::InvalidTransition {
                stream_id,
                from: current.to_string(),
                to: StreamState::Error.to_string(),
            });
        }

        warn!(
            %stream_id,
            from = %current,
            error = %error_message,
            "stream transitioning to ERROR"
        );

        entry_ref.metadata.ended_at = Some(chrono::Utc::now());
        entry_ref.state = StreamState::Error;
        entry_ref.error_message = Some(error_message);
        let updated = entry_ref.clone();
        drop(entry_ref);

        obs::clear_stream_metrics(&stream_id.to_string());

        // Update per-state counters (O(1) instead of O(n) scan)
        self.state_counts[Self::state_index(current)].fetch_sub(1, Ordering::Relaxed);
        self.state_counts[Self::state_index(StreamState::Error)].fetch_add(1, Ordering::Relaxed);
        self.update_stream_gauges();

        Ok(updated)
    }

    /// Get only the current state of a stream — no allocation, no clone.
    ///
    /// Prefer this over `get_stream` on hot paths (e.g. delivery handlers) where
    /// only the state enum is needed.
    pub fn get_stream_state(&self, stream_id: StreamId) -> Option<StreamState> {
        self.streams.get(&stream_id).map(|r| r.state)
    }

    /// Get the current state of a stream (full entry with metadata and renditions).
    pub async fn get_stream(&self, stream_id: StreamId) -> Option<StreamEntry> {
        self.streams.get(&stream_id).map(|r| r.clone())
    }

    /// List all streams (optionally filtered by state).
    pub async fn list_streams(&self, filter_state: Option<StreamState>) -> Vec<StreamEntry> {
        self.streams
            .iter()
            .filter(|r| filter_state.map(|s| r.value().state == s).unwrap_or(true))
            .map(|r| r.value().clone())
            .collect()
    }

    /// List streams with offset/limit pagination while avoiding full cloning.
    ///
    /// Returns `(page, total_matching)` where `total_matching` reflects the full
    /// filtered set size before pagination.
    pub async fn list_streams_paginated(
        &self,
        filter_state: Option<StreamState>,
        offset: usize,
        limit: usize,
    ) -> (Vec<StreamEntry>, usize) {
        let mut total_matching = 0usize;
        let mut page = Vec::with_capacity(limit);

        for entry in self.streams.iter() {
            if !filter_state
                .map(|s| entry.value().state == s)
                .unwrap_or(true)
            {
                continue;
            }

            if total_matching >= offset && page.len() < limit {
                page.push(entry.value().clone());
            }
            total_matching += 1;
        }

        (page, total_matching)
    }

    /// Set media info for a stream (called when ingest detects codecs).
    pub async fn set_media_info(&self, stream_id: StreamId, media_info: MediaInfo) {
        if let Some(mut entry) = self.streams.get_mut(&stream_id) {
            entry.metadata.media_info = Some(media_info);
            debug!(%stream_id, "media info updated");
        }
    }

    /// Set the expected number of renditions for a stream.
    /// Called by the transcode pipeline on startup.
    pub async fn set_expected_renditions(&self, stream_id: StreamId, count: usize) {
        if let Some(mut entry) = self.streams.get_mut(&stream_id) {
            entry.expected_renditions = Some(count);
            debug!(%stream_id, count, "expected renditions set");
        }
    }

    /// Update rendition progress for a stream (called when a segment is produced).
    /// Tracks per-rendition segment counts and timestamps.
    pub async fn update_rendition_progress(
        &self,
        stream_id: StreamId,
        rendition: &str,
        sequence: u64,
    ) {
        if let Some(mut entry) = self.streams.get_mut(&stream_id) {
            let status = entry
                .renditions
                .entry(rendition.to_string())
                .or_insert_with(|| RenditionStatus {
                    rendition: rendition.to_string(),
                    segments_produced: 0,
                    last_segment_time: None,
                    status: RenditionState::Active,
                });
            status.segments_produced = sequence;
            status.last_segment_time = Some(chrono::Utc::now());
            debug!(%stream_id, %rendition, sequence, "rendition progress updated");
        }
    }

    /// Mark a rendition as complete. If all expected renditions are complete,
    /// auto-transition from PROCESSING → READY (media-lifecycle.md §3.2 Step 8).
    pub async fn mark_rendition_complete(
        &self,
        stream_id: StreamId,
        rendition: &str,
    ) -> Option<StreamEntry> {
        let should_transition = {
            let mut entry = self.streams.get_mut(&stream_id)?;
            if let Some(status) = entry.renditions.get_mut(rendition) {
                status.status = RenditionState::Complete;
            }
            // Check if all expected renditions are complete
            if let Some(expected) = entry.expected_renditions {
                let complete_count = entry
                    .renditions
                    .values()
                    .filter(|r| r.status == RenditionState::Complete)
                    .count();
                entry.state == StreamState::Processing && complete_count >= expected
            } else {
                false
            }
        };

        if should_transition {
            match self.transition(stream_id, StreamState::Ready).await {
                Ok(updated) => {
                    info!(%stream_id, "all renditions complete, transitioned to READY");
                    Some(updated)
                }
                Err(e) => {
                    warn!(%stream_id, error = %e, "failed auto-transition to READY");
                    None
                }
            }
        } else {
            self.streams.get(&stream_id).map(|r| r.clone())
        }
    }

    /// Remove a stream record entirely (for audit cleanup after retention).
    pub async fn remove_stream(&self, stream_id: StreamId) -> bool {
        if let Some((_key, removed_entry)) = self.streams.remove(&stream_id) {
            self.state_counts[Self::state_index(removed_entry.state)]
                .fetch_sub(1, Ordering::Relaxed);
            obs::clear_stream_metrics(&stream_id.to_string());
            self.update_stream_gauges();
            true
        } else {
            false
        }
    }

    /// Update Prometheus stream count gauges per state.
    ///
    /// Reads from pre-maintained atomic counters — O(1) instead of O(n) full scan.
    fn update_stream_gauges(&self) {
        const STATES: [StreamState; 6] = [
            StreamState::Pending,
            StreamState::Live,
            StreamState::Processing,
            StreamState::Ready,
            StreamState::Error,
            StreamState::Deleted,
        ];
        for state in &STATES {
            let count = self.state_counts[Self::state_index(*state)]
                .load(Ordering::Relaxed)
                .max(0);
            obs::set_streams_total(&state.to_string(), count as f64);
        }
    }

    /// Check if a valid state transition exists.
    ///
    /// Valid transitions per the media lifecycle state machine:
    /// - PENDING → LIVE (live only, on RTMP connect)
    /// - PENDING → PROCESSING (VOD, on upload complete)
    /// - LIVE → PROCESSING (live, on stream end / disconnect)
    /// - PROCESSING → READY (all renditions packaged)
    /// - READY → DELETED (admin delete)
    /// - Any (except DELETED) → ERROR (unrecoverable failure)
    fn is_valid_transition(from: StreamState, to: StreamState, ingest_mode: IngestMode) -> bool {
        // ERROR is handled separately via transition_to_error
        if to == StreamState::Error {
            return from != StreamState::Deleted;
        }

        matches!(
            (from, to, ingest_mode),
            // Live path: PENDING → LIVE → PROCESSING → READY → DELETED
            (StreamState::Pending, StreamState::Live, IngestMode::Live)
                | (StreamState::Live, StreamState::Processing, IngestMode::Live)
                // VOD path: PENDING → PROCESSING → READY → DELETED
                | (StreamState::Pending, StreamState::Processing, IngestMode::Vod)
                // Common: PROCESSING → READY, READY → DELETED
                | (StreamState::Processing, StreamState::Ready, _)
                | (StreamState::Ready, StreamState::Deleted, _)
                // Allow ERROR → DELETED for cleanup
                | (StreamState::Error, StreamState::Deleted, _)
        )
    }
}

impl Default for StreamStateManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_live_stream_lifecycle() {
        let manager = StreamStateManager::new();
        let stream_id = StreamId::new();

        // Create → PENDING
        let entry = manager.create_stream(stream_id, IngestMode::Live).await;
        assert_eq!(entry.state, StreamState::Pending);

        // PENDING → LIVE
        let entry = manager
            .transition(stream_id, StreamState::Live)
            .await
            .unwrap();
        assert_eq!(entry.state, StreamState::Live);
        assert!(entry.metadata.started_at.is_some());

        // LIVE → PROCESSING
        let entry = manager
            .transition(stream_id, StreamState::Processing)
            .await
            .unwrap();
        assert_eq!(entry.state, StreamState::Processing);

        // PROCESSING → READY
        let entry = manager
            .transition(stream_id, StreamState::Ready)
            .await
            .unwrap();
        assert_eq!(entry.state, StreamState::Ready);
        assert!(entry.metadata.ended_at.is_some());

        // READY → DELETED
        let entry = manager
            .transition(stream_id, StreamState::Deleted)
            .await
            .unwrap();
        assert_eq!(entry.state, StreamState::Deleted);
    }

    #[tokio::test]
    async fn test_vod_stream_lifecycle() {
        let manager = StreamStateManager::new();
        let stream_id = StreamId::new();

        // Create → PENDING
        manager.create_stream(stream_id, IngestMode::Vod).await;

        // VOD skips LIVE: PENDING → PROCESSING
        let entry = manager
            .transition(stream_id, StreamState::Processing)
            .await
            .unwrap();
        assert_eq!(entry.state, StreamState::Processing);

        // PROCESSING → READY
        let entry = manager
            .transition(stream_id, StreamState::Ready)
            .await
            .unwrap();
        assert_eq!(entry.state, StreamState::Ready);
    }

    #[tokio::test]
    async fn test_invalid_transition_vod_to_live() {
        let manager = StreamStateManager::new();
        let stream_id = StreamId::new();

        manager.create_stream(stream_id, IngestMode::Vod).await;

        // VOD streams cannot go to LIVE
        let result = manager.transition(stream_id, StreamState::Live).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_error_from_any_state() {
        let manager = StreamStateManager::new();
        let stream_id = StreamId::new();

        manager.create_stream(stream_id, IngestMode::Live).await;

        // PENDING → ERROR
        let entry = manager
            .transition_to_error(stream_id, "test error".to_string())
            .await
            .unwrap();
        assert_eq!(entry.state, StreamState::Error);
        assert_eq!(entry.error_message.as_deref(), Some("test error"));
    }

    #[tokio::test]
    async fn test_error_from_live_state() {
        let manager = StreamStateManager::new();
        let stream_id = StreamId::new();

        manager.create_stream(stream_id, IngestMode::Live).await;
        manager
            .transition(stream_id, StreamState::Live)
            .await
            .unwrap();

        // LIVE → ERROR
        let entry = manager
            .transition_to_error(stream_id, "ffmpeg crash".to_string())
            .await
            .unwrap();
        assert_eq!(entry.state, StreamState::Error);
    }

    #[tokio::test]
    async fn test_cannot_error_from_deleted() {
        let manager = StreamStateManager::new();
        let stream_id = StreamId::new();

        manager.create_stream(stream_id, IngestMode::Live).await;
        manager
            .transition(stream_id, StreamState::Live)
            .await
            .unwrap();
        manager
            .transition(stream_id, StreamState::Processing)
            .await
            .unwrap();
        manager
            .transition(stream_id, StreamState::Ready)
            .await
            .unwrap();
        manager
            .transition(stream_id, StreamState::Deleted)
            .await
            .unwrap();

        // DELETED → ERROR should fail
        let result = manager
            .transition_to_error(stream_id, "should fail".to_string())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_stream_not_found() {
        let manager = StreamStateManager::new();
        let stream_id = StreamId::new();

        let result = manager.transition(stream_id, StreamState::Live).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_streams_with_filter() {
        let manager = StreamStateManager::new();
        let id1 = StreamId::new();
        let id2 = StreamId::new();

        manager.create_stream(id1, IngestMode::Live).await;
        manager.create_stream(id2, IngestMode::Vod).await;
        manager.transition(id1, StreamState::Live).await.unwrap();

        let pending = manager.list_streams(Some(StreamState::Pending)).await;
        assert_eq!(pending.len(), 1);

        let live = manager.list_streams(Some(StreamState::Live)).await;
        assert_eq!(live.len(), 1);

        let all = manager.list_streams(None).await;
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_list_streams_paginated_returns_page_and_total() {
        let manager = StreamStateManager::new();
        let id1 = StreamId::new();
        let id2 = StreamId::new();
        let id3 = StreamId::new();

        manager.create_stream(id1, IngestMode::Live).await;
        manager.create_stream(id2, IngestMode::Vod).await;
        manager.create_stream(id3, IngestMode::Vod).await;

        let (page1, total1) = manager.list_streams_paginated(None, 0, 2).await;
        assert_eq!(total1, 3);
        assert_eq!(page1.len(), 2);

        let (page2, total2) = manager.list_streams_paginated(None, 2, 2).await;
        assert_eq!(total2, 3);
        assert_eq!(page2.len(), 1);
    }

    #[tokio::test]
    async fn test_get_stream_state_no_clone() {
        let manager = StreamStateManager::new();
        let stream_id = StreamId::new();

        assert_eq!(manager.get_stream_state(stream_id), None);

        manager.create_stream(stream_id, IngestMode::Live).await;
        assert_eq!(
            manager.get_stream_state(stream_id),
            Some(StreamState::Pending)
        );

        manager
            .transition(stream_id, StreamState::Live)
            .await
            .unwrap();
        assert_eq!(
            manager.get_stream_state(stream_id),
            Some(StreamState::Live)
        );
    }
}
