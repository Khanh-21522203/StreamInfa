use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};

use crate::core::config::{DeliveryConfig, SecurityConfig};
use crate::storage::cache::ObjectCache;
use crate::storage::memory::InMemoryMediaStore;

use super::handlers;
use super::middleware::RequestIdLayer;

// ---------------------------------------------------------------------------
// Delivery + API router (from control-plane-vs-data-plane.md §3.3)
// ---------------------------------------------------------------------------

/// Application state shared across all handlers.
///
/// Uses `InMemoryMediaStore` for MVP. When the `s3` feature is enabled,
/// this can be swapped to `S3MediaStore` (same `MediaStore` trait).
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<InMemoryMediaStore>,
    pub cache: Arc<ObjectCache>,
    pub state_manager: Arc<crate::control::state::StreamStateManager>,
    pub auth: Arc<crate::core::auth::AuthProvider>,
    pub config: crate::core::config::AppConfig,
    pub start_time: std::time::Instant,
    /// Prometheus metrics handle for rendering /metrics endpoint.
    pub metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
    /// Event channel sender for data plane → control plane events.
    pub event_tx: tokio::sync::mpsc::Sender<crate::control::events::PipelineEvent>,
}

/// Build the full Axum router with all routes.
///
/// Route table (from control-plane-vs-data-plane.md §3.3):
///
/// **Control plane (authenticated):**
/// - `POST   /api/v1/streams`          — Create stream
/// - `GET    /api/v1/streams`          — List streams
/// - `GET    /api/v1/streams/{id}`     — Get stream details
/// - `DELETE /api/v1/streams/{id}`     — Delete stream
///
/// **Health (unauthenticated):**
/// - `GET /healthz`                    — Liveness probe
/// - `GET /readyz`                     — Readiness probe
/// - `GET /metrics`                    — Prometheus metrics
///
/// **Delivery (unauthenticated):**
/// - `GET /streams/{id}/master.m3u8`
/// - `GET /streams/{id}/{rendition}/media.m3u8`
/// - `GET /streams/{id}/{rendition}/init.mp4`
/// - `GET /streams/{id}/{rendition}/{segment}`
pub fn build_router(
    state: AppState,
    delivery_config: &DeliveryConfig,
    security_config: &SecurityConfig,
) -> Router {
    tracing::info!(
        cache_control_live = %delivery_config.cache_control_live,
        cache_control_vod = %delivery_config.cache_control_vod,
        cors_origins = ?delivery_config.cors_allowed_origins,
        "delivery configuration loaded"
    );
    // CORS layer (from storage-and-delivery.md §6.6, security.md §5.3)
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([http::Method::GET, http::Method::HEAD, http::Method::OPTIONS])
        .allow_headers([http::header::RANGE])
        .expose_headers([
            http::header::CONTENT_LENGTH,
            http::header::CONTENT_RANGE,
            http::header::ACCEPT_RANGES,
        ])
        .max_age(std::time::Duration::from_secs(86400));

    // JSON body size limit (from security.md §4.3: ≤ 1 MB)
    let body_limit = DefaultBodyLimit::max(security_config.max_json_body_bytes);

    Router::new()
        // Control plane API
        .route(
            "/api/v1/streams",
            get(handlers::list_streams).post(handlers::create_stream),
        )
        .route(
            "/api/v1/streams/{stream_id}",
            get(handlers::get_stream).delete(handlers::delete_stream),
        )
        .route("/api/v1/streams/upload", post(handlers::upload_vod))
        .route(
            "/api/v1/streams/{stream_id}/rotate-key",
            post(handlers::rotate_stream_key),
        )
        // Health endpoints
        .route("/healthz", get(handlers::healthz))
        .route("/readyz", get(handlers::readyz))
        // Prometheus metrics endpoint (from observability.md §2.1)
        .route("/metrics", get(handlers::metrics_handler))
        // Delivery endpoints
        .route(
            "/streams/{stream_id}/master.m3u8",
            get(handlers::serve_master_playlist),
        )
        .route(
            "/streams/{stream_id}/{rendition}/media.m3u8",
            get(handlers::serve_media_playlist),
        )
        .route(
            "/streams/{stream_id}/{rendition}/init.mp4",
            get(handlers::serve_init_segment),
        )
        .route(
            "/streams/{stream_id}/{rendition}/{segment}",
            get(handlers::serve_segment),
        )
        .layer(cors)
        .layer(body_limit)
        // Request ID middleware (from observability.md §9.1)
        .layer(RequestIdLayer)
        .with_state(state)
}
