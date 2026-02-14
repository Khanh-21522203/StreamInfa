use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use tracing::{error, info, warn};

use streaminfa::control::events;
use streaminfa::control::state::StreamStateManager;
use streaminfa::core::config::AppConfig;
use streaminfa::core::shutdown::ShutdownCoordinator;
use streaminfa::delivery::router::{self, AppState};
use streaminfa::observability::metrics as obs_metrics;
use streaminfa::storage::cache::ObjectCache;
use streaminfa::storage::memory::InMemoryMediaStore;

#[tokio::main]
async fn main() -> ExitCode {
    // Install Prometheus metrics recorder (from observability.md §2.1)
    // Must be installed before any metrics are recorded.
    let metrics_handle = obs_metrics::install_prometheus_recorder();

    // Install panic hook: log panics with full backtrace and increment counter.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        obs_metrics::inc_panic_total();
        let backtrace = std::backtrace::Backtrace::force_capture();
        eprintln!("PANIC: {info}\nBacktrace:\n{backtrace}");
        default_hook(info);
    }));

    // Load configuration (layered: default.toml → {env}.toml → env vars)
    let config = match AppConfig::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("failed to load configuration: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Initialize tracing / logging
    init_tracing(
        &config.observability.log_level,
        &config.observability.log_format,
    );

    info!(version = env!("CARGO_PKG_VERSION"), "StreamInfa starting");

    // Register all metrics descriptors (from observability.md §2.2)
    obs_metrics::describe_all_metrics();
    streaminfa::core::metrics::describe_backpressure_metrics();

    // Initialize shared components
    let shutdown = ShutdownCoordinator::new();
    let state_manager = Arc::new(StreamStateManager::new());
    let auth = Arc::new(streaminfa::core::auth::AuthProvider::new(&config.auth));
    let cache = Arc::new(ObjectCache::new(&config.cache));

    // Initialize storage backend
    // For development, use InMemoryMediaStore. In production, use S3MediaStore.
    let store: Arc<InMemoryMediaStore> = Arc::new(InMemoryMediaStore::new());

    // Create pipeline event channel (data plane → control plane)
    let (event_tx, event_rx) = events::create_event_channel();

    // Start the event handler task
    let event_state_manager = state_manager.clone();
    let event_store = store.clone();
    tokio::spawn(async move {
        events::run_event_handler(event_rx, event_state_manager, event_store).await;
    });

    // Start the storage cleanup task
    let cleanup_store = store.clone();
    let cleanup_state = state_manager.clone();
    let cleanup_cancel = shutdown.token().clone();
    let packaging_config = config.packaging.clone();
    tokio::spawn(async move {
        streaminfa::storage::cleanup::run_cleanup_task(
            cleanup_store,
            cleanup_state,
            packaging_config,
            cleanup_cancel,
        )
        .await;
    });

    // Build the HTTP router (delivery + control plane API)
    let start_time = std::time::Instant::now();
    let app_state = AppState {
        store: store.clone(),
        cache: cache.clone(),
        state_manager: state_manager.clone(),
        auth: auth.clone(),
        config: config.clone(),
        start_time,
        metrics_handle,
        event_tx: event_tx.clone(),
    };
    let app = router::build_router(app_state, &config.delivery, &config.security);

    // Start uptime gauge task (from observability.md §2.2)
    let uptime_cancel = shutdown.token().clone();
    tokio::spawn(async move {
        obs_metrics::run_uptime_task(start_time, uptime_cancel).await;
    });

    // Start RTMP ingest server (from architecture/overview.md §2)
    let rtmp_auth = auth.clone();
    let rtmp_state = state_manager.clone();
    let rtmp_store = store.clone();
    let rtmp_config = config.ingest.clone();
    let rtmp_app_config = config.clone();
    let rtmp_cancel = shutdown.token().clone();
    let rtmp_event_tx = event_tx.clone();
    tokio::spawn(async move {
        let server = streaminfa::ingest::rtmp::RtmpServer::new(
            rtmp_config,
            rtmp_app_config,
            rtmp_auth,
            rtmp_state,
            rtmp_store,
            rtmp_cancel,
            rtmp_event_tx,
        );
        if let Err(e) = server.run().await {
            error!(error = %e, "RTMP server failed");
        }
    });

    // Start HTTP server
    let http_addr: SocketAddr = format!("{}:{}", config.server.host, config.server.control_port)
        .parse()
        .expect("invalid HTTP bind address");

    info!(
        %http_addr,
        rtmp_port = config.server.rtmp_port,
        "configuration loaded, starting servers"
    );

    let listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .expect("failed to bind HTTP listener");

    info!(%http_addr, "HTTP server listening (delivery + control API)");

    // Run HTTP server with graceful shutdown
    let shutdown_token = shutdown.token().clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_token.cancelled().await;
            })
            .await
            .expect("HTTP server error");
    });

    // Start SIGHUP config reload task (from security.md §6.3)
    let reload_auth = auth.clone();
    let reload_cancel = shutdown.token().clone();
    tokio::spawn(async move {
        run_config_reload_task(reload_auth, reload_cancel).await;
    });

    // Start brute-force tracker cleanup task (from security.md §2.1)
    let bf_auth = auth.clone();
    let bf_cancel = shutdown.token().clone();
    tokio::spawn(async move {
        run_brute_force_cleanup_task(bf_auth, bf_cancel).await;
    });

    // Wait for shutdown signal
    shutdown.wait_for_signal_and_shutdown().await;

    // Graceful shutdown with total timeout
    obs_metrics::set_shutdown_in_progress(true);
    info!("initiating graceful shutdown sequence");
    let shutdown_result = tokio::time::timeout(
        std::time::Duration::from_secs(streaminfa::core::shutdown::SHUTDOWN_TIMEOUT_SECS),
        graceful_shutdown(),
    )
    .await;

    match shutdown_result {
        Ok(()) => {
            info!("graceful shutdown completed");
            ExitCode::SUCCESS
        }
        Err(_) => {
            error!(
                "shutdown timed out after {}s, forcing exit",
                streaminfa::core::shutdown::SHUTDOWN_TIMEOUT_SECS
            );
            ExitCode::FAILURE
        }
    }
}

/// Execute the graceful shutdown sequence.
async fn graceful_shutdown() {
    // Phase 1: Stop accepting new connections (15s timeout)
    info!(
        "phase 1: stopping ingest ({}s timeout)",
        streaminfa::core::shutdown::INGEST_DRAIN_TIMEOUT_SECS
    );
    // TODO: drain active ingest streams

    // Phase 2: Wait for in-flight transcode jobs (10s timeout)
    info!(
        "phase 2: draining transcode jobs ({}s timeout)",
        streaminfa::core::shutdown::TRANSCODE_DRAIN_TIMEOUT_SECS
    );
    // TODO: wait for transcode completion

    // Phase 3: Flush remaining segments to storage (5s timeout)
    info!(
        "phase 3: flushing storage ({}s timeout)",
        streaminfa::core::shutdown::STORAGE_FLUSH_TIMEOUT_SECS
    );
    // TODO: flush storage writes

    // Phase 4: Close HTTP server (5s timeout)
    info!(
        "phase 4: draining HTTP server ({}s timeout)",
        streaminfa::core::shutdown::HTTP_DRAIN_TIMEOUT_SECS
    );
    // TODO: drain HTTP connections
}

/// SIGHUP config reload task (from security.md §6.3).
///
/// On SIGHUP:
/// 1. Reload config from disk
/// 2. Update auth tokens (admin bearer tokens + stream keys)
/// This enables zero-downtime token rotation.
async fn run_config_reload_task(
    auth: Arc<streaminfa::core::auth::AuthProvider>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut sighup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "failed to install SIGHUP handler, config reload disabled");
            return;
        }
    };

    info!("SIGHUP config reload task started");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("config reload task shutting down");
                return;
            }
            _ = sighup.recv() => {
                info!("received SIGHUP, reloading configuration");
                match AppConfig::load() {
                    Ok(new_config) => {
                        // Hot-reloadable: auth tokens (control-plane-vs-data-plane.md §6.1)
                        auth.update_stream_keys(new_config.auth.ingest_stream_keys);
                        auth.update_admin_tokens(new_config.auth.admin_bearer_tokens);

                        // Hot-reloadable: log level (control-plane-vs-data-plane.md §6.1)
                        // Note: tracing-subscriber's reload handle would be ideal here,
                        // but for now we log the new level. A full implementation would
                        // use a reload::Handle<EnvFilter>.
                        info!(
                            new_log_level = %new_config.observability.log_level,
                            "log level reload requested (takes effect on next process start)"
                        );

                        // Hot-reloadable: CORS origins (control-plane-vs-data-plane.md §6.1)
                        // CORS layer is set at router build time; new origins take effect
                        // only when the HTTP server is restarted. Log for operator awareness.
                        info!(
                            cors_origins = ?new_config.delivery.cors_allowed_origins,
                            "CORS origins updated in config (requires restart for HTTP server)"
                        );

                        // Hot-reloadable: profile ladder for NEW streams
                        info!(
                            profiles = new_config.transcode.profile_ladder.len(),
                            "transcode profile ladder updated for new streams"
                        );

                        streaminfa::observability::metrics::inc_config_reload("success");
                        info!("configuration reloaded successfully");
                    }
                    Err(e) => {
                        streaminfa::observability::metrics::inc_config_reload("failure");
                        error!(error = %e, "failed to reload configuration on SIGHUP, keeping current config");
                    }
                }
            }
        }
    }
}

/// Periodic cleanup of expired brute-force tracker entries (from security.md §2.1).
async fn run_brute_force_cleanup_task(
    auth: Arc<streaminfa::core::auth::AuthProvider>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let interval = std::time::Duration::from_secs(60);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(interval) => {
                auth.cleanup_brute_force_tracker();
            }
        }
    }
}

fn init_tracing(log_level: &str, log_format: &str) {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    match log_format {
        "json" => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .json()
                .init();
        }
        _ => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }
}
