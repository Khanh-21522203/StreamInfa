use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use crate::control::state::StreamState;
use crate::core::auth::TokenStatus;
use crate::core::security::{DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT};
use crate::core::types::{IngestMode, StreamId};
use crate::observability::metrics as obs;
use crate::storage::MediaStore;

use super::router::AppState;

// ---------------------------------------------------------------------------
// Error response (from storage-and-delivery.md §6.5)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
    message: String,
    status: u16,
}

fn error_json(status: StatusCode, error: &str, message: &str) -> Response {
    let body = ErrorResponse {
        error: error.to_string(),
        message: message.to_string(),
        status: status.as_u16(),
    };
    (status, Json(body)).into_response()
}

// ---------------------------------------------------------------------------
// Delivery handlers (from storage-and-delivery.md §6.2)
// ---------------------------------------------------------------------------

/// `GET /streams/{stream_id}/master.m3u8`
pub async fn serve_master_playlist(
    State(state): State<AppState>,
    Path(stream_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("{}/master.m3u8", stream_id);
    serve_object(&state, &path, &stream_id, Some("master_playlist"), &headers).await
}

/// `GET /streams/{stream_id}/{rendition}/media.m3u8`
pub async fn serve_media_playlist(
    State(state): State<AppState>,
    Path((stream_id, rendition)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("{}/{}/media.m3u8", stream_id, rendition);
    serve_object(&state, &path, &stream_id, Some("media_playlist"), &headers).await
}

/// `GET /streams/{stream_id}/{rendition}/init.mp4`
pub async fn serve_init_segment(
    State(state): State<AppState>,
    Path((stream_id, rendition)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("{}/{}/init.mp4", stream_id, rendition);
    serve_object(&state, &path, &stream_id, Some("init_segment"), &headers).await
}

/// `GET /streams/{stream_id}/{rendition}/{segment}`
pub async fn serve_segment(
    State(state): State<AppState>,
    Path((stream_id, rendition, segment)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("{}/{}/{}", stream_id, rendition, segment);
    serve_object(&state, &path, &stream_id, Some("segment"), &headers).await
}

/// Core object serving logic with cache, range requests, and proper headers.
///
/// Flow (from storage-and-delivery.md §5.4):
/// 1. Check LRU cache → HIT: return cached bytes
/// 2. MISS: fetch from S3
/// 3. Insert into cache (if size allows)
/// 4. Return with proper HTTP headers
async fn serve_object(
    state: &AppState,
    path: &str,
    stream_id: &str,
    object_type: Option<&str>,
    request_headers: &HeaderMap,
) -> Response {
    let start = std::time::Instant::now();
    obs::set_delivery_active_connections(1.0); // Track active request
    let otype = object_type.unwrap_or_else(|| obs::classify_object_type(path));
    // Check if stream exists and is in a serveable state
    if let Ok(sid) = stream_id.parse::<uuid::Uuid>() {
        let sid = StreamId::from_uuid(sid);
        if let Some(entry) = state.state_manager.get_stream(sid).await {
            match entry.state {
                StreamState::Pending => {
                    return error_json(
                        StatusCode::NOT_FOUND,
                        "stream_not_ready",
                        &format!("Stream '{}' is not yet ingesting.", stream_id),
                    );
                }
                StreamState::Error => {
                    return error_json(
                        StatusCode::GONE,
                        "stream_error",
                        &format!("Stream '{}' encountered an error.", stream_id),
                    );
                }
                StreamState::Deleted => {
                    return error_json(
                        StatusCode::NOT_FOUND,
                        "stream_not_found",
                        &format!("Stream '{}' has been deleted.", stream_id),
                    );
                }
                _ => {} // Live, Processing, Ready — all serveable
            }
        }
    }

    // Parse Range header if present
    let range = request_headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range_header);

    // Record delivery request metric
    obs::inc_delivery_request("2xx", otype);

    // Try cache first
    if let Some((cached_data, content_type, etag)) = state.cache.get(path) {
        debug!(path, "serving from cache");
        obs::inc_cache_hit(otype);
        obs::add_delivery_bytes_sent(cached_data.len() as u64);
        obs::record_delivery_request_duration(otype, start.elapsed().as_secs_f64());
        return build_response(
            cached_data,
            &content_type,
            &etag,
            path,
            range,
            is_live_manifest(path, state).await,
            &state.config.delivery,
        );
    }

    // Cache miss — fetch from storage
    obs::inc_cache_miss(otype);
    let storage_start = std::time::Instant::now();
    let result = if let Some((start, end)) = range {
        state.store.get_object_range(path, start, end).await
    } else {
        state.store.get_object(path).await
    };

    match result {
        Ok(output) => {
            obs::record_storage_get_duration(otype, storage_start.elapsed().as_secs_f64());
            obs::add_storage_get_bytes(otype, output.body.len() as u64);
            obs::add_delivery_bytes_sent(output.body.len() as u64);
            obs::record_delivery_request_duration(otype, start.elapsed().as_secs_f64());
            // Cache the object (full object only, not range requests)
            if range.is_none() {
                state.cache.put(
                    path,
                    output.body.clone(),
                    &output.content_type,
                    &output.etag,
                );
            }

            build_response(
                output.body,
                &output.content_type,
                &output.etag,
                path,
                range,
                is_live_manifest(path, state).await,
                &state.config.delivery,
            )
        }
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("not found") {
                let error_code = if path.ends_with(".m4s") {
                    "segment_not_found"
                } else if path.contains("media.m3u8") || path.contains("init.mp4") {
                    "rendition_not_found"
                } else {
                    "stream_not_found"
                };
                error_json(
                    StatusCode::NOT_FOUND,
                    error_code,
                    &format!("Object '{}' not found.", path),
                )
            } else {
                error!(path, error = %e, "storage error serving object");
                error_json(
                    StatusCode::BAD_GATEWAY,
                    "storage_error",
                    "Failed to retrieve object from storage.",
                )
            }
        }
    }
}

/// Build an HTTP response with proper headers.
///
/// Headers (from storage-and-delivery.md §6.3):
/// - Content-Type
/// - Cache-Control (live vs VOD)
/// - ETag
/// - Accept-Ranges: bytes
/// - X-StreamInfa-Stream-Id
/// - Range support (206 Partial Content)
fn build_response(
    data: Bytes,
    content_type: &str,
    etag: &str,
    path: &str,
    range: Option<(u64, u64)>,
    is_live: bool,
    delivery_config: &crate::core::config::DeliveryConfig,
) -> Response {
    let cache_control = if is_live && path.ends_with(".m3u8") {
        &delivery_config.cache_control_live
    } else {
        &delivery_config.cache_control_vod
    };

    // Extract stream_id from path (first component)
    let stream_id = path.split('/').next().unwrap_or("");

    if let Some((start, end)) = range {
        let total_size = data.len() as u64;
        let end = end.min(total_size.saturating_sub(1));

        if start >= total_size {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{}", total_size))],
            )
                .into_response();
        }

        let slice = data.slice(start as usize..=(end as usize));
        let content_range = format!("bytes {}-{}/{}", start, end, total_size);

        (
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, content_type.to_string()),
                (header::CONTENT_LENGTH, slice.len().to_string()),
                (header::CONTENT_RANGE, content_range),
                (header::CACHE_CONTROL, cache_control.to_string()),
                (header::ETAG, etag.to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (
                    "X-StreamInfa-Stream-Id".parse().unwrap(),
                    stream_id.to_string(),
                ),
            ],
            slice,
        )
            .into_response()
    } else {
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, content_type.to_string()),
                (header::CONTENT_LENGTH, data.len().to_string()),
                (header::CACHE_CONTROL, cache_control.to_string()),
                (header::ETAG, etag.to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (
                    "X-StreamInfa-Stream-Id".parse().unwrap(),
                    stream_id.to_string(),
                ),
            ],
            data,
        )
            .into_response()
    }
}

/// Parse a Range header value like "bytes=0-1048575".
fn parse_range_header(value: &str) -> Option<(u64, u64)> {
    let range_str = value.strip_prefix("bytes=")?;
    let parts: Vec<&str> = range_str.split('-').collect();
    if parts.len() != 2 {
        return None;
    }
    let start: u64 = parts[0].parse().ok()?;
    let end: u64 = if parts[1].is_empty() {
        u64::MAX // Open-ended range
    } else {
        parts[1].parse().ok()?
    };
    Some((start, end))
}

/// Check if a path refers to a live stream's manifest.
async fn is_live_manifest(path: &str, state: &AppState) -> bool {
    if !path.ends_with(".m3u8") {
        return false;
    }
    let stream_id_str = path.split('/').next().unwrap_or("");
    if let Ok(uuid) = stream_id_str.parse::<uuid::Uuid>() {
        let sid = StreamId::from_uuid(uuid);
        if let Some(entry) = state.state_manager.get_stream(sid).await {
            return entry.state == StreamState::Live;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Control plane handlers (from control-plane-vs-data-plane.md §3.1)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateStreamRequest {
    pub ingest_type: String,
    pub metadata: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, Serialize)]
pub struct CreateStreamResponse {
    pub stream_id: String,
    pub status: String,
    pub stream_key: Option<String>,
    pub rtmp_url: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct StreamListResponse {
    pub streams: Vec<StreamSummary>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Serialize)]
pub struct StreamSummary {
    pub stream_id: String,
    pub status: String,
    pub ingest_type: String,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct StreamDetailResponse {
    pub stream_id: String,
    pub status: String,
    pub ingest_type: String,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub playback_url: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DeleteStreamResponse {
    pub stream_id: String,
    pub status: String,
    pub segments_deleted: u64,
    pub storage_freed_bytes: u64,
}

#[derive(Debug, Deserialize)]
pub struct ListStreamsQuery {
    pub status: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// `POST /api/v1/streams` — Create a new stream.
pub async fn create_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateStreamRequest>,
) -> Response {
    // Authenticate
    if let Err(e) = authenticate_admin(&state, &headers) {
        return e;
    }

    let ingest_mode = match body.ingest_type.as_str() {
        "rtmp" => IngestMode::Live,
        "upload" => IngestMode::Vod,
        other => {
            return error_json(
                StatusCode::BAD_REQUEST,
                "invalid_ingest_type",
                &format!(
                    "Invalid ingest_type: '{}'. Must be 'rtmp' or 'upload'.",
                    other
                ),
            );
        }
    };

    // Validate metadata if provided (from security.md §4.3)
    if let Some(ref metadata) = body.metadata {
        if let Err(e) = crate::core::security::validate_metadata(metadata) {
            return error_json(StatusCode::BAD_REQUEST, "invalid_metadata", &e);
        }
    }

    let stream_id = StreamId::new();
    state
        .state_manager
        .create_stream(stream_id, ingest_mode)
        .await;

    // For VOD streams, wire up the data-plane pipeline now.
    // For RTMP (live) streams, the pipeline is created by the RTMP connection
    // when the encoder connects and sends its AVC sequence header, so we do
    // NOT create a pipeline here — it would be immediately orphaned.
    if ingest_mode == IngestMode::Vod {
        let pipeline = crate::control::pipeline::create_pipeline(
            stream_id,
            state.event_tx.clone(),
            &state.config,
        );

        // VOD: resolution comes from the probe result after upload.
        // The upload handler will feed frames into pipeline.ingest_tx.
        // For now, use placeholder resolution; real values are set by the
        // upload handler after probing the file.
        let source_width: u32 = 1920;
        let source_height: u32 = 1080;
        let source_fps: f64 = 30.0;

        let transcode_config = state.config.transcode.clone();
        let transcode_state_mgr = state.state_manager.clone();
        let transcode_cancel = pipeline.cancel.clone();
        let transcode_segment_tx = pipeline.transcode_tx;
        let transcode_frame_rx = pipeline.ingest_rx;
        tokio::spawn(async move {
            let tp = crate::transcode::pipeline::TranscodePipeline::new(
                transcode_config,
                transcode_state_mgr,
                transcode_cancel,
            );
            if let Err(e) = tp
                .run_live(
                    stream_id,
                    source_width,
                    source_height,
                    source_fps,
                    transcode_frame_rx,
                    transcode_segment_tx,
                )
                .await
            {
                tracing::error!(%stream_id, error = %e, "transcode pipeline failed");
            }
        });

        let packaging_config = state.config.packaging.clone();
        let packager_event_tx = state.event_tx.clone();
        let packager_cancel = pipeline.cancel.clone();
        let packager_segment_rx = pipeline.transcode_rx;
        let packager_storage_tx = pipeline.package_tx;
        tokio::spawn(async move {
            crate::package::runner::run_packager(
                stream_id,
                packaging_config,
                packager_segment_rx,
                packager_storage_tx,
                packager_event_tx,
                packager_cancel,
            )
            .await;
        });

        let writer_store = state.store.clone();
        let writer_cancel = pipeline.cancel.clone();
        let writer_rx = pipeline.package_rx;
        tokio::spawn(async move {
            crate::storage::writer::run_storage_writer(
                stream_id,
                writer_store,
                writer_rx,
                writer_cancel,
            )
            .await;
        });

        // Pipeline channels consumed by spawned tasks above;
        // cancel token and ingest_tx available for the upload handler.
        let _cancel = pipeline.cancel;
        let _ingest_tx = pipeline.ingest_tx;
        let _stream_id = pipeline.stream_id;
    }

    // Generate stream key for RTMP
    let (stream_key, rtmp_url) = if ingest_mode == IngestMode::Live {
        let key = generate_stream_key();
        let url = format!(
            "rtmp://{}:{}/live/{}",
            state.config.server.host, state.config.server.rtmp_port, key
        );
        (Some(key), Some(url))
    } else {
        (None, None)
    };

    // Log with redacted stream key (from security.md §7.1)
    if let Some(ref key) = stream_key {
        info!(
            %stream_id,
            ingest_type = %body.ingest_type,
            stream_key = %crate::core::redact::Redacted::new(
                crate::core::redact::redact_stream_key(key)
            ),
            "stream created"
        );
    } else {
        info!(%stream_id, ingest_type = %body.ingest_type, "stream created");
    }

    let response = CreateStreamResponse {
        stream_id: stream_id.to_string(),
        status: "pending".to_string(),
        stream_key,
        rtmp_url,
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    (StatusCode::CREATED, Json(response)).into_response()
}

/// `GET /api/v1/streams` — List streams.
pub async fn list_streams(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListStreamsQuery>,
) -> Response {
    if let Err(e) = authenticate_admin(&state, &headers) {
        return e;
    }

    let filter = query.status.as_deref().and_then(parse_stream_state);
    let all_streams = state.state_manager.list_streams(filter).await;

    let limit = query
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .min(MAX_LIST_LIMIT);
    let offset = query.offset.unwrap_or(0);
    let total = all_streams.len();

    let streams: Vec<StreamSummary> = all_streams
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|entry| StreamSummary {
            stream_id: entry.metadata.stream_id.to_string(),
            status: entry.state.to_string(),
            ingest_type: entry.metadata.ingest_mode.to_string(),
            created_at: entry.metadata.created_at.to_rfc3339(),
        })
        .collect();

    let response = StreamListResponse {
        streams,
        total,
        limit,
        offset,
    };

    Json(response).into_response()
}

/// `GET /api/v1/streams/{stream_id}` — Get stream details.
pub async fn get_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(stream_id): Path<String>,
) -> Response {
    if let Err(e) = authenticate_admin(&state, &headers) {
        return e;
    }

    let uuid = match stream_id.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return error_json(
                StatusCode::BAD_REQUEST,
                "invalid_stream_id",
                "Invalid stream ID format.",
            );
        }
    };

    let sid = StreamId::from_uuid(uuid);
    match state.state_manager.get_stream(sid).await {
        Some(entry) => {
            let playback_url = match entry.state {
                StreamState::Live | StreamState::Ready => Some(format!(
                    "http://{}:{}/streams/{}/master.m3u8",
                    state.config.server.host, state.config.server.control_port, stream_id
                )),
                _ => None,
            };

            let response = StreamDetailResponse {
                stream_id: stream_id.clone(),
                status: entry.state.to_string(),
                ingest_type: entry.metadata.ingest_mode.to_string(),
                created_at: entry.metadata.created_at.to_rfc3339(),
                updated_at: entry.metadata.ended_at.map(|t| t.to_rfc3339()),
                playback_url,
                error: entry.error_message,
            };

            Json(response).into_response()
        }
        None => error_json(
            StatusCode::NOT_FOUND,
            "stream_not_found",
            &format!("Stream '{}' not found.", stream_id),
        ),
    }
}

/// `DELETE /api/v1/streams/{stream_id}` — Delete a stream.
///
/// Side effects (from control-plane-vs-data-plane.md §3.1):
/// - If LIVE: abort pipeline, then delete
/// - If PROCESSING: abort transcode, then delete
/// - Delete all S3 objects under stream prefix
/// - Transition state to DELETED
pub async fn delete_stream(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(stream_id): Path<String>,
) -> Response {
    if let Err(e) = authenticate_admin(&state, &headers) {
        return e;
    }

    let uuid = match stream_id.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return error_json(
                StatusCode::BAD_REQUEST,
                "invalid_stream_id",
                "Invalid stream ID format.",
            );
        }
    };

    let sid = StreamId::from_uuid(uuid);
    match state.state_manager.get_stream(sid).await {
        Some(_entry) => {
            // Delete S3 objects
            let prefix = format!("{}/", stream_id);
            let (segments_deleted, _storage_freed) = match state.store.delete_prefix(&prefix).await
            {
                Ok(count) => (count, 0u64), // We don't track freed bytes in delete_prefix
                Err(e) => {
                    error!(%stream_id, error = %e, "failed to delete stream objects");
                    (0, 0)
                }
            };

            // Transition to DELETED and remove from state
            let _ = state
                .state_manager
                .transition(sid, StreamState::Deleted)
                .await;
            state.state_manager.remove_stream(sid).await;
            for _ in 0..segments_deleted {
                obs::inc_storage_segments_deleted();
            }

            info!(%stream_id, segments_deleted, "stream deleted");

            let response = DeleteStreamResponse {
                stream_id,
                status: "deleted".to_string(),
                segments_deleted,
                storage_freed_bytes: 0,
            };

            Json(response).into_response()
        }
        None => error_json(
            StatusCode::NOT_FOUND,
            "stream_not_found",
            &format!("Stream '{}' not found.", stream_id),
        ),
    }
}

// ---------------------------------------------------------------------------
// Health endpoints (from control-plane-vs-data-plane.md §3.2)
// ---------------------------------------------------------------------------

/// `GET /metrics` — Prometheus metrics endpoint.
///
/// From observability.md §2.1: Expose metrics in Prometheus text exposition format.
pub async fn metrics_handler(State(state): State<AppState>) -> Response {
    let metrics = state.metrics_handle.render();
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        metrics,
    )
        .into_response()
}

/// `GET /healthz` — Liveness probe.
pub async fn healthz(State(state): State<AppState>) -> Json<serde_json::Value> {
    let uptime = state.start_time.elapsed().as_secs();
    Json(serde_json::json!({
        "status": "healthy",
        "uptime_secs": uptime,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// `GET /readyz` — Readiness probe.
///
/// Checks (from control-plane-vs-data-plane.md §3.2, FR-CONTROL-04):
/// - Storage: HEAD request to bucket
/// - FFmpeg: verify ffmpeg binary is available on PATH
pub async fn readyz(State(state): State<AppState>) -> Response {
    let mut checks = serde_json::Map::new();
    let mut all_ok = true;

    // Storage check: try to HEAD the bucket by listing with empty prefix
    match state
        .store
        .list_objects("__health_check_nonexistent__")
        .await
    {
        Ok(_) => {
            checks.insert("storage".to_string(), serde_json::json!({"status": "ok"}));
        }
        Err(e) => {
            all_ok = false;
            checks.insert(
                "storage".to_string(),
                serde_json::json!({"status": "error", "error": e.to_string()}),
            );
        }
    }

    // FFmpeg availability check (FR-CONTROL-04)
    match tokio::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
    {
        Ok(status) if status.success() => {
            checks.insert("ffmpeg".to_string(), serde_json::json!({"status": "ok"}));
        }
        Ok(status) => {
            all_ok = false;
            checks.insert(
                "ffmpeg".to_string(),
                serde_json::json!({"status": "error", "error": format!("ffmpeg exited with {}", status)}),
            );
        }
        Err(e) => {
            all_ok = false;
            checks.insert(
                "ffmpeg".to_string(),
                serde_json::json!({"status": "error", "error": format!("ffmpeg not found: {}", e)}),
            );
        }
    }

    let status = if all_ok { "ready" } else { "not_ready" };
    let http_status = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    // Report auth mode in readiness check (from security.md §2.2)
    let auth_open_mode = state.auth.is_open_mode();

    (
        http_status,
        Json(serde_json::json!({
            "status": status,
            "checks": checks,
            "auth_open_mode": auth_open_mode,
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Auth helper
// ---------------------------------------------------------------------------

/// Authenticate admin requests with 401 vs 403 distinction.
///
/// From security.md §2.2:
/// - Header missing → 401 Unauthorized
/// - Token format invalid → 401 Unauthorized  
/// - Token not in valid set → 403 Forbidden
/// - Token valid → proceed
fn authenticate_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match state.auth.check_bearer_token(token) {
        TokenStatus::Valid => Ok(()),
        TokenStatus::Missing => {
            debug!(
                authorization = %crate::core::redact::redact_bearer_token(
                    headers.get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                ),
                "admin auth failed: missing or invalid header"
            );
            Err(error_json(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Missing or invalid Authorization header.",
            ))
        }
        TokenStatus::Forbidden => {
            debug!(
                authorization = %crate::core::redact::redact_bearer_token(
                    token.unwrap_or("")
                ),
                "admin auth failed: invalid token"
            );
            Err(error_json(
                StatusCode::FORBIDDEN,
                "forbidden",
                "Invalid bearer token.",
            ))
        }
    }
}

fn parse_stream_state(s: &str) -> Option<StreamState> {
    match s {
        "pending" => Some(StreamState::Pending),
        "live" => Some(StreamState::Live),
        "processing" => Some(StreamState::Processing),
        "ready" => Some(StreamState::Ready),
        "error" => Some(StreamState::Error),
        "deleted" => Some(StreamState::Deleted),
        _ => None,
    }
}

/// Generate a cryptographically random stream key.
///
// ---------------------------------------------------------------------------
// VOD Upload endpoint (from ingest.md §3)
// ---------------------------------------------------------------------------

/// Handle VOD file upload.
///
/// POST /api/v1/upload
/// - Auth: Bearer token
/// - Body: multipart/form-data with file field
/// - Returns: 201 with stream metadata or error
pub async fn upload_vod(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let start = std::time::Instant::now();

    // Auth check
    if let Err(resp) = authenticate_admin(&state, &headers) {
        return resp;
    }

    let stream_id = StreamId::new();
    let upload_handler = crate::ingest::http_upload::HttpUploadHandler::new(
        state.config.ingest.clone(),
        state.auth.clone(),
        state.state_manager.clone(),
    );

    // Ensure upload directory exists
    if let Err(e) = upload_handler.ensure_upload_dir().await {
        error!(%stream_id, error = %e, "failed to create upload directory");
        return error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            &format!("Failed to prepare upload: {}", e),
        );
    }

    // Write body to temp file
    let temp_path = upload_handler.temp_file_path(stream_id);
    let file_size = body.len() as u64;
    if let Err(e) = tokio::fs::write(&temp_path, &body).await {
        error!(%stream_id, error = %e, "failed to write upload to temp file");
        return error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            &format!("Failed to write upload: {}", e),
        );
    }

    // Process the upload
    match upload_handler
        .process_upload(stream_id, &temp_path, file_size)
        .await
    {
        Ok(response) => {
            obs::record_upload_duration(start.elapsed().as_secs_f64());
            obs::record_upload_size(file_size as f64);
            Json(response).into_response()
        }
        Err(e) => {
            let status_code = crate::ingest::http_upload::ingest_error_to_status_code(&e);
            let error_resp = crate::ingest::http_upload::build_error_response(&e, Some(stream_id));
            let status =
                StatusCode::from_u16(status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(error_resp)).into_response()
        }
    }
}

/// From security.md §2.1:
/// Format: `sk_` prefix + 24 random alphanumeric characters.
/// Entropy: 24 characters from [a-z0-9] = ~124 bits.
fn generate_stream_key() -> String {
    use crate::core::security::{STREAM_KEY_PREFIX, STREAM_KEY_RANDOM_LENGTH};
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let chars: String = (0..STREAM_KEY_RANDOM_LENGTH)
        .map(|_| {
            let idx = rng.gen_range(0..36u8);
            if idx < 10 {
                (b'0' + idx) as char
            } else {
                (b'a' + idx - 10) as char
            }
        })
        .collect();
    format!("{}{}", STREAM_KEY_PREFIX, chars)
}
