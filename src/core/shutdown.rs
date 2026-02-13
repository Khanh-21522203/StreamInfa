use tokio_util::sync::CancellationToken;
use tracing::info;

/// Graceful shutdown coordinator.
///
/// Uses `CancellationToken` to broadcast shutdown signals to all tasks.
/// Shutdown sequence (from architecture/overview.md §9):
/// 1. Set CancellationToken (broadcast to all tasks)
/// 2. Stop accepting new RTMP connections and HTTP uploads
/// 3. Wait for active ingest streams to finish current segment (timeout: 15s)
/// 4. Wait for in-flight transcode jobs to complete (timeout: 10s)
/// 5. Flush remaining segments to storage (timeout: 5s)
/// 6. Close HTTP server (drain in-flight requests, timeout: 5s)
/// 7. Exit with code 0
/// Total shutdown timeout: 30 seconds. After 30s, force exit with code 1.
#[derive(Clone)]
pub struct ShutdownCoordinator {
    token: CancellationToken,
}

impl ShutdownCoordinator {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    /// Returns a clone of the cancellation token for use by tasks.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Triggers shutdown for all tasks listening on this token.
    pub fn trigger_shutdown(&self) {
        info!("shutdown signal received, broadcasting to all tasks");
        self.token.cancel();
    }

    /// Wait for a shutdown signal (SIGTERM or SIGINT) and trigger coordinated shutdown.
    pub async fn wait_for_signal_and_shutdown(&self) {
        let ctrl_c = tokio::signal::ctrl_c();
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");

        tokio::select! {
            _ = ctrl_c => {
                info!("received SIGINT (Ctrl+C)");
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM");
            }
        }

        self.trigger_shutdown();
    }
}

impl Default for ShutdownCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// Total shutdown timeout in seconds.
pub const SHUTDOWN_TIMEOUT_SECS: u64 = 30;

/// Per-phase shutdown timeouts.
pub const INGEST_DRAIN_TIMEOUT_SECS: u64 = 15;
pub const TRANSCODE_DRAIN_TIMEOUT_SECS: u64 = 10;
pub const STORAGE_FLUSH_TIMEOUT_SECS: u64 = 5;
pub const HTTP_DRAIN_TIMEOUT_SECS: u64 = 5;
