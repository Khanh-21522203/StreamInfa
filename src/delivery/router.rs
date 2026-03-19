use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};

use crate::core::config::{DeliveryConfig, SecurityConfig};
use crate::storage::cache::ObjectCache;
use crate::storage::AppMediaStore;

use super::handlers;
use super::middleware::RequestIdLayer;

// ---------------------------------------------------------------------------
// Delivery + API router (from control-plane-vs-data-plane.md §3.3)
// ---------------------------------------------------------------------------

/// Application state shared across all handlers.
///
/// Uses the configured application storage backend.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<AppMediaStore>,
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
    let cors_base = CorsLayer::new()
        .allow_methods([http::Method::GET, http::Method::HEAD, http::Method::OPTIONS])
        .allow_headers([http::header::RANGE])
        .expose_headers([
            http::header::CONTENT_LENGTH,
            http::header::CONTENT_RANGE,
            http::header::ACCEPT_RANGES,
        ])
        .max_age(std::time::Duration::from_secs(86400));
    let cors = if delivery_config
        .cors_allowed_origins
        .iter()
        .any(|o| o == "*")
    {
        cors_base.allow_origin(Any)
    } else {
        let mut parsed = Vec::new();
        for origin in &delivery_config.cors_allowed_origins {
            match origin.parse::<http::HeaderValue>() {
                Ok(v) => parsed.push(v),
                Err(_) => tracing::warn!(origin = %origin, "ignoring invalid CORS origin"),
            }
        }
        if parsed.is_empty() {
            tracing::warn!("no valid CORS origins configured; falling back to wildcard");
            cors_base.allow_origin(Any)
        } else {
            cors_base.allow_origin(parsed)
        }
    };

    // JSON body size limit for control + delivery routes (security.md §4.3: ≤ 1 MB).
    // The upload route is in a separate inner router so it never passes through this limit —
    // DefaultBodyLimit::max() wraps the body immediately and cannot be undone by inner layers.
    // The upload handler enforces max_upload_size_bytes itself.
    let body_limit = DefaultBodyLimit::max(security_config.max_json_body_bytes);

    let limited_routes = Router::new()
        // Control plane API
        .route(
            "/api/v1/streams",
            get(handlers::list_streams).post(handlers::create_stream),
        )
        .route(
            "/api/v1/streams/{stream_id}",
            get(handlers::get_stream).delete(handlers::delete_stream),
        )
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
        .layer(body_limit);

    Router::new()
        .merge(limited_routes)
        // Explicitly disable the body limit for upload — without this, Axum's
        // with_limited_body() falls back to a 2 MB hard default even with no layer applied.
        // The handler enforces max_upload_size_bytes instead.
        .route(
            "/api/v1/streams/upload",
            post(handlers::upload_vod).layer(DefaultBodyLimit::disable()),
        )
        .layer(cors)
        // Request ID middleware (from observability.md §9.1)
        .layer(RequestIdLayer)
        .with_state(state)
}
