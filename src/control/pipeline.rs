use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::core::config::AppConfig;
use crate::core::metrics;
use crate::core::types::{
    DemuxedFrame, EncodedSegment, StorageWrite, StreamId, INGEST_TRANSCODE_CHANNEL_CAP,
    PACKAGER_STORAGE_CHANNEL_CAP, TRANSCODE_PACKAGER_CHANNEL_CAP,
};

use super::events::PipelineEvent;

// ---------------------------------------------------------------------------
// Pipeline wiring (from control-plane-vs-data-plane.md §5)
// ---------------------------------------------------------------------------

/// A wired pipeline for a single stream.
///
/// Created by the control plane when a stream starts. Holds all channel
/// endpoints and the per-stream cancellation token.
pub struct Pipeline {
    /// Send demuxed frames into the transcode stage.
    pub ingest_tx: mpsc::Sender<DemuxedFrame>,
    /// Receive demuxed frames in the transcode stage.
    pub ingest_rx: mpsc::Receiver<DemuxedFrame>,
    /// Send encoded segments into the packager stage.
    pub transcode_tx: mpsc::Sender<EncodedSegment>,
    /// Receive encoded segments in the packager stage.
    pub transcode_rx: mpsc::Receiver<EncodedSegment>,
    /// Send storage writes from the packager.
    pub package_tx: mpsc::Sender<StorageWrite>,
    /// Receive storage writes in the storage stage.
    pub package_rx: mpsc::Receiver<StorageWrite>,
    /// Per-stream cancellation token.
    pub cancel: CancellationToken,
    /// The stream this pipeline belongs to.
    pub stream_id: StreamId,
}

/// Create a wired pipeline for a stream.
///
/// Channels (from control-plane-vs-data-plane.md §5):
/// - ingest → transcode: capacity 64
/// - transcode → packager: capacity 32
/// - packager → storage: capacity 32
/// - data → control (events): shared sender (cloned)
///
/// The control plane holds the `CancellationToken` and can cancel the
/// stream's pipeline at any time. The data plane holds the channel
/// endpoints and runs tasks until cancelled or stream ends naturally.
pub fn create_pipeline(
    stream_id: StreamId,
    _event_tx: mpsc::Sender<PipelineEvent>,
    _config: &AppConfig,
) -> Pipeline {
    // Register backpressure metrics (from performance-and-backpressure.md §4)
    metrics::describe_backpressure_metrics();
    metrics::record_channel_metrics("ingest_transcode", INGEST_TRANSCODE_CHANNEL_CAP, 0);
    metrics::record_channel_metrics("transcode_packager", TRANSCODE_PACKAGER_CHANNEL_CAP, 0);
    metrics::record_channel_metrics("packager_storage", PACKAGER_STORAGE_CHANNEL_CAP, 0);

    let (ingest_tx, ingest_rx) = mpsc::channel(INGEST_TRANSCODE_CHANNEL_CAP);
    let (transcode_tx, transcode_rx) = mpsc::channel(TRANSCODE_PACKAGER_CHANNEL_CAP);
    let (package_tx, package_rx) = mpsc::channel(PACKAGER_STORAGE_CHANNEL_CAP);
    let cancel = CancellationToken::new();

    info!(%stream_id, "pipeline created");

    Pipeline {
        ingest_tx,
        ingest_rx,
        transcode_tx,
        transcode_rx,
        package_tx,
        package_rx,
        cancel,
        stream_id,
    }
}
