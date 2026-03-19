use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::core::types::{StorageWrite, StreamId};
use crate::observability::metrics as obs;

use super::AppMediaStore;
use super::MediaStore;

/// Maximum number of retry attempts per storage write (storage-and-delivery.md §4.3).
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff: 100ms, 200ms, 400ms.
const RETRY_BASE_DELAY_MS: u64 = 100;

// ---------------------------------------------------------------------------
// Storage writer task (from architecture/overview.md §4.3)
// ---------------------------------------------------------------------------

/// Run the storage writer task for a single stream.
///
/// This task reads `StorageWrite` messages from the packager and writes them
/// to the storage backend. Writes are batched: after receiving the first item,
/// all immediately-available items are drained and executed concurrently via
/// `JoinSet`. It runs until the channel is closed or cancelled.
pub async fn run_storage_writer(
    stream_id: StreamId,
    store: Arc<AppMediaStore>,
    mut write_rx: mpsc::Receiver<StorageWrite>,
    cancel: CancellationToken,
) {
    info!(%stream_id, "storage writer task started");

    let mut segments_written: u64 = 0;
    let mut bytes_written: u64 = 0;

    loop {
        // Block until the first write or cancellation
        let first = tokio::select! {
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

        // Drain all immediately available items to form a batch
        let mut batch = vec![first];
        while let Ok(w) = write_rx.try_recv() {
            batch.push(w);
        }

        // Execute the entire batch concurrently
        let mut join_set: JoinSet<(u64, bool)> = JoinSet::new();
        for write in batch {
            let store = Arc::clone(&store);
            let sid = stream_id;
            join_set.spawn(async move {
                let path = write.path.clone();
                let size = write.data.len() as u64;
                let content_type = write.content_type.clone();
                let object_type = super::object_type_label(&path);
                let start = std::time::Instant::now();

                let mut last_error = None;
                for attempt in 0..=MAX_RETRIES {
                    let result = if path.ends_with(".m3u8") {
                        let content = String::from_utf8_lossy(&write.data).to_string();
                        store.put_manifest(&path, &content).await
                    } else {
                        store
                            .put_segment(&path, write.data.clone(), &content_type)
                            .await
                    };

                    match result {
                        Ok(()) => {
                            obs::add_storage_put_bytes(object_type, size);
                            obs::record_storage_put_duration(
                                object_type,
                                start.elapsed().as_secs_f64(),
                            );
                            if attempt > 0 {
                                debug!(%sid, path, attempt, "storage write succeeded after retry");
                            } else {
                                debug!(%sid, path, size, object_type, "storage write completed");
                            }
                            last_error = None;
                            break;
                        }
                        Err(e) => {
                            last_error = Some(e);
                            if attempt < MAX_RETRIES {
                                let delay =
                                    Duration::from_millis(RETRY_BASE_DELAY_MS * (1 << attempt));
                                obs::inc_storage_retries("put");
                                warn!(
                                    %sid, path, attempt = attempt + 1,
                                    delay_ms = delay.as_millis() as u64,
                                    "storage write failed, retrying"
                                );
                                tokio::time::sleep(delay).await;
                            }
                        }
                    }
                }

                if let Some(e) = last_error {
                    obs::inc_storage_error("put", object_type);
                    error!(
                        %sid, path,
                        error = %e,
                        retries = MAX_RETRIES,
                        "storage write failed after all retries"
                    );
                    (size, false)
                } else {
                    (size, true)
                }
            });
        }

        // Collect results and update counters
        while let Some(result) = join_set.join_next().await {
            if let Ok((size, success)) = result {
                if success {
                    segments_written += 1;
                    bytes_written += size;
                }
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
