use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::core::types::{StorageWrite, StreamId};
use crate::observability::metrics as obs;

use super::memory::InMemoryMediaStore;
use super::MediaStore;

// ---------------------------------------------------------------------------
// Storage writer task (from architecture/overview.md §4.3)
// ---------------------------------------------------------------------------

/// Run the storage writer task for a single stream.
///
/// This task reads `StorageWrite` messages from the packager and writes them
/// to the storage backend. It runs until the channel is closed or cancelled.
pub async fn run_storage_writer(
    stream_id: StreamId,
    store: Arc<InMemoryMediaStore>,
    mut write_rx: mpsc::Receiver<StorageWrite>,
    cancel: CancellationToken,
) {
    info!(%stream_id, "storage writer task started");

    let mut segments_written: u64 = 0;
    let mut bytes_written: u64 = 0;

    loop {
        let write = tokio::select! {
            _ = cancel.cancelled() => {
                info!(%stream_id, "storage writer cancelled");
                break;
            }
            w = write_rx.recv() => {
                match w {
                    Some(w) => w,
                    None => {
                        info!(%stream_id, "packager channel closed, storage writer finishing");
                        break;
                    }
                }
            }
        };

        let path = write.path.clone();
        let size = write.data.len() as u64;
        let content_type = write.content_type.clone();
        let object_type = super::object_type_label(&path);
        let start = std::time::Instant::now();

        let result = if path.ends_with(".m3u8") {
            // Manifests are written as text
            let content = String::from_utf8_lossy(&write.data).to_string();
            store.put_manifest(&path, &content).await
        } else {
            store.put_segment(&path, write.data, &content_type).await
        };

        match result {
            Ok(()) => {
                segments_written += 1;
                bytes_written += size;
                obs::add_storage_put_bytes(object_type, size);
                obs::record_storage_put_duration(object_type, start.elapsed().as_secs_f64());
                debug!(
                    %stream_id,
                    path,
                    size,
                    object_type,
                    "storage write completed"
                );
            }
            Err(e) => {
                obs::inc_storage_error("put", object_type);
                obs::inc_storage_retries("put");
                error!(
                    %stream_id,
                    path,
                    error = %e,
                    "storage write failed"
                );
            }
        }
    }

    info!(
        %stream_id,
        segments_written,
        bytes_written,
        "storage writer task finished"
    );
}
