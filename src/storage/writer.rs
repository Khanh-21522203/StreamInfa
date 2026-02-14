use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::core::types::{StorageWrite, StreamId};
use crate::observability::metrics as obs;

use super::memory::InMemoryMediaStore;
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

        // Retry loop with exponential backoff (storage-and-delivery.md §4.3)
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
                    segments_written += 1;
                    bytes_written += size;
                    obs::add_storage_put_bytes(object_type, size);
                    obs::record_storage_put_duration(object_type, start.elapsed().as_secs_f64());
                    if attempt > 0 {
                        debug!(%stream_id, path, attempt, "storage write succeeded after retry");
                    } else {
                        debug!(%stream_id, path, size, object_type, "storage write completed");
                    }
                    last_error = None;
                    break;
                }
                Err(e) => {
                    last_error = Some(e);
                    if attempt < MAX_RETRIES {
                        let delay = Duration::from_millis(RETRY_BASE_DELAY_MS * (1 << attempt));
                        obs::inc_storage_retries("put");
                        warn!(
                            %stream_id, path, attempt = attempt + 1,
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
                %stream_id, path,
                error = %e,
                retries = MAX_RETRIES,
                "storage write failed after all retries"
            );
        }
    }

    info!(
        %stream_id,
        segments_written,
        bytes_written,
        "storage writer task finished"
    );
}
