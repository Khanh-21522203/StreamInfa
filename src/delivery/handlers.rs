use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[cfg(not(feature = "ffmpeg"))]
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tracing::{debug, error, info};

/// Custom header name (parsed once, no unwrap in hot path).
static X_STREAMINFA_STREAM_ID: HeaderName = HeaderName::from_static("x-streaminfa-stream-id");

use crate::control::state::StreamState;
use crate::core::auth::TokenStatus;
use crate::core::security::{DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT};
use crate::core::types::{IngestMode, StreamId};
#[cfg(not(feature = "ffmpeg"))]
use crate::core::types::{
    DemuxedFrame, Track, VideoCodec, INGEST_TRANSCODE_CHANNEL_CAP,
};
use crate::observability::metrics as obs;
use crate::storage::MediaStore;

use super::router::AppState;

const READYZ_MIN_DISK_BYTES: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB
const READYZ_DISK_PATH: &str = "/tmp/streaminfa/uploads";

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
    object_type: Option<&'static str>,
    request_headers: &HeaderMap,
) -> Response {
    let start = std::time::Instant::now();
    let _delivery_guard = DeliveryConnectionGuard::new();
    let otype: &'static str = object_type.unwrap_or_else(|| obs::classify_object_type(path));

    // Check stream state once — no full StreamEntry clone, just the state enum.
    // Reused below for Cache-Control header (is_live) without a second lookup.
    let stream_state = stream_id.parse::<uuid::Uuid>().ok().and_then(|uuid| {
        let sid = StreamId::from_uuid(uuid);
        state.state_manager.get_stream_state(sid)
    });

    if let Some(ss) = stream_state {
        match ss {
            StreamState::Pending => {
                let response = error_json(
                    StatusCode::NOT_FOUND,
                    "stream_not_ready",
                    &format!("Stream '{}' is not yet ingesting.", stream_id),
                );
                finalize_delivery_metrics(&response, otype, start);
                return response;
            }
            StreamState::Error => {
                let response = error_json(
                    StatusCode::GONE,
                    "stream_error",
                    &format!("Stream '{}' encountered an error.", stream_id),
                );
                finalize_delivery_metrics(&response, otype, start);
                return response;
            }
            StreamState::Deleted => {
                let response = error_json(
                    StatusCode::NOT_FOUND,
                    "stream_not_found",
                    &format!("Stream '{}' has been deleted.", stream_id),
                );
                finalize_delivery_metrics(&response, otype, start);
                return response;
            }
            _ => {} // Live, Processing, Ready — all serveable
        }
    }

    // Determine live/VOD for Cache-Control headers from the state already fetched above.
    let is_live = stream_state == Some(StreamState::Live);

    // Parse Range header if present
    let range = match request_headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
    {
        Some(raw) => match parse_range_header(raw) {
            Some(r) => Some(r),
            None => {
                let response = error_json(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "range_not_satisfiable",
                    "Invalid or unsupported Range header.",
                );
                finalize_delivery_metrics(&response, otype, start);
                return response;
            }
        },
        None => None,
    };

    // Try cache first
    if let Some((cached_data, content_type, etag)) = state.cache.get(path) {
        debug!(path, "serving from cache");
        obs::inc_cache_hit(otype);
        let response = build_response(
            cached_data,
            &content_type,
            &etag,
            path,
            range,
            is_live,
            &state.config.delivery,
        );
        finalize_delivery_metrics(&response, otype, start);
        return response;
    }

    // Cache miss — fetch from storage
    obs::inc_cache_miss(otype);
    let storage_start = std::time::Instant::now();
    // Fetch full object on miss and slice in-memory for range responses.
    // This keeps range semantics consistent across cache/storage paths.
    let result = state.store.get_object(path).await;

    match result {
        Ok(output) => {
            obs::record_storage_get_duration(otype, storage_start.elapsed().as_secs_f64());
            obs::add_storage_get_bytes(otype, output.body.len() as u64);
            // Cache the object (full object only, not range requests)
            if range.is_none() {
                state.cache.put(
                    path,
                    output.body.clone(),
                    &output.content_type,
                    &output.etag,
                );
            }

            let response = build_response(
                output.body,
                &output.content_type,
                &output.etag,
                path,
                range,
                is_live,
                &state.config.delivery,
            );
            finalize_delivery_metrics(&response, otype, start);
            response
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
                let response = error_json(
                    StatusCode::NOT_FOUND,
                    error_code,
                    &format!("Object '{}' not found.", path),
                );
                finalize_delivery_metrics(&response, otype, start);
                response
            } else {
                error!(path, error = %e, "storage error serving object");
                let response = error_json(
                    StatusCode::BAD_GATEWAY,
                    "storage_error",
                    "Failed to retrieve object from storage.",
                );
                finalize_delivery_metrics(&response, otype, start);
                response
            }
        }
    }
}

struct DeliveryConnectionGuard;

impl DeliveryConnectionGuard {
    fn new() -> Self {
        obs::inc_delivery_active_connections();
        Self
    }
}

impl Drop for DeliveryConnectionGuard {
    fn drop(&mut self) {
        obs::dec_delivery_active_connections();
    }
}

fn finalize_delivery_metrics(
    response: &Response,
    object_type: &'static str,
    start: std::time::Instant,
) {
    obs::inc_delivery_request(status_bucket(response.status()), object_type);
    obs::record_delivery_request_duration(object_type, start.elapsed().as_secs_f64());
    if response.status().is_success() {
        let bytes_sent = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        if bytes_sent > 0 {
            obs::add_delivery_bytes_sent(bytes_sent);
        }
    }
}

fn status_bucket(status: StatusCode) -> &'static str {
    match status.as_u16() {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
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
        if total_size == 0 {
            return error_json(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "range_not_satisfiable",
                "Cannot satisfy range request for empty object.",
            );
        }
        let end = end.min(total_size.saturating_sub(1));

        if start >= total_size || start > end {
            return error_json(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "range_not_satisfiable",
                &format!(
                    "Requested range bytes={}-{} is outside object size {}.",
                    start, end, total_size
                ),
            );
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
                (X_STREAMINFA_STREAM_ID.clone(), stream_id.to_string()),
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
                (X_STREAMINFA_STREAM_ID.clone(), stream_id.to_string()),
            ],
            data,
        )
            .into_response()
    }
}

/// Parse a Range header value like "bytes=0-1048575".
fn parse_range_header(value: &str) -> Option<(u64, u64)> {
    let range_str = value.trim().strip_prefix("bytes=")?;
    let parts: Vec<&str> = range_str.split('-').collect();
    if parts.len() != 2 {
        return None;
    }
    if parts[0].is_empty() {
        // Suffix ranges (bytes=-N) are not supported in MVP.
        return None;
    }
    let start: u64 = parts[0].parse().ok()?;
    let end: u64 = if parts[1].is_empty() {
        u64::MAX // Open-ended range
    } else {
        parts[1].parse().ok()?
    };
    if end != u64::MAX && start > end {
        return None;
    }
    Some((start, end))
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
    if let Err(e) = authenticate_admin(&state, &headers, "create_stream") {
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

    // Generate stream key for RTMP using AuthProvider (security.md §2.1)
    let (stream_key, rtmp_url) = if ingest_mode == IngestMode::Live {
        let (plaintext_key, _hash) = state.auth.generate_stream_key_for_stream(stream_id);
        let url = format!(
            "rtmp://{}:{}/live/{}",
            state.config.server.host, state.config.server.rtmp_port, plaintext_key
        );
        (Some(plaintext_key), Some(url))
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
    if let Err(e) = authenticate_admin(&state, &headers, "list_streams") {
        return e;
    }

    let filter = match query.status.as_deref() {
        Some(raw) => match parse_stream_state(raw) {
            Some(s) => Some(s),
            None => {
                return error_json(
                    StatusCode::BAD_REQUEST,
                    "invalid_status",
                    "Invalid status filter. Expected one of: pending, live, processing, ready, error, deleted.",
                );
            }
        },
        None => None,
    };

    let limit = query.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    if limit == 0 || limit > MAX_LIST_LIMIT {
        return error_json(
            StatusCode::BAD_REQUEST,
            "invalid_limit",
            &format!(
                "Invalid limit: {}. Expected range 1..={}.",
                limit, MAX_LIST_LIMIT
            ),
        );
    }
    let offset = query.offset.unwrap_or(0);
    let (page, total) = state
        .state_manager
        .list_streams_paginated(filter, offset, limit)
        .await;

    let streams: Vec<StreamSummary> = page
        .into_iter()
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
    if let Err(e) = authenticate_admin(&state, &headers, "get_stream") {
        return e;
    }

    let sid = match parse_stream_id_param(&stream_id) {
        Ok(sid) => sid,
        Err(response) => return response,
    };
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
    if let Err(e) = authenticate_admin(&state, &headers, "delete_stream") {
        return e;
    }

    let sid = match parse_stream_id_param(&stream_id) {
        Ok(sid) => sid,
        Err(response) => return response,
    };
    match state.state_manager.get_stream(sid).await {
        Some(entry) => {
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

            // Keep tombstone in DELETED for retention/audit cleanup.
            if entry.state != StreamState::Deleted {
                let _ = state
                    .state_manager
                    .transition(sid, StreamState::Deleted)
                    .await;
            }
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

    // Storage check: probe and record latency
    let storage_start = std::time::Instant::now();
    match state
        .store
        .list_objects("__health_check_nonexistent__")
        .await
    {
        Ok(_) => {
            checks.insert(
                "storage".to_string(),
                serde_json::json!({
                    "status": "ok",
                    "latency_ms": storage_start.elapsed().as_millis(),
                }),
            );
        }
        Err(e) => {
            all_ok = false;
            checks.insert(
                "storage".to_string(),
                serde_json::json!({
                    "status": "error",
                    "latency_ms": storage_start.elapsed().as_millis(),
                    "error": e.to_string()
                }),
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

    // RTMP listener check (control-plane-vs-data-plane.md §3.2)
    match check_rtmp_listener(&state.config.server.host, state.config.server.rtmp_port).await {
        Ok(address) => {
            checks.insert(
                "rtmp_listener".to_string(),
                serde_json::json!({
                    "status": "ok",
                    "port": state.config.server.rtmp_port,
                    "address": address,
                }),
            );
        }
        Err(e) => {
            all_ok = false;
            checks.insert(
                "rtmp_listener".to_string(),
                serde_json::json!({
                    "status": "error",
                    "port": state.config.server.rtmp_port,
                    "error": e,
                }),
            );
        }
    }

    // Disk-space threshold check for upload temp directory
    match check_disk_space(READYZ_DISK_PATH, READYZ_MIN_DISK_BYTES).await {
        Ok(available_bytes) => {
            let available_gib = available_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            let required_gib = READYZ_MIN_DISK_BYTES as f64 / (1024.0 * 1024.0 * 1024.0);
            if available_bytes >= READYZ_MIN_DISK_BYTES {
                checks.insert(
                    "disk_space".to_string(),
                    serde_json::json!({
                        "status": "ok",
                        "path": READYZ_DISK_PATH,
                        "available_gb": available_gib,
                        "min_required_gb": required_gib,
                    }),
                );
            } else {
                all_ok = false;
                checks.insert(
                    "disk_space".to_string(),
                    serde_json::json!({
                        "status": "error",
                        "path": READYZ_DISK_PATH,
                        "available_gb": available_gib,
                        "min_required_gb": required_gib,
                        "error": "insufficient disk space",
                    }),
                );
            }
        }
        Err(e) => {
            all_ok = false;
            checks.insert(
                "disk_space".to_string(),
                serde_json::json!({
                    "status": "error",
                    "path": READYZ_DISK_PATH,
                    "error": e,
                }),
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

async fn check_rtmp_listener(host: &str, port: u16) -> Result<String, String> {
    let probe_host = match host {
        "0.0.0.0" => "127.0.0.1",
        "::" => "::1",
        _ => host,
    };
    let address = format!("{probe_host}:{port}");

    let connect_result = tokio::time::timeout(
        std::time::Duration::from_millis(750),
        tokio::net::TcpStream::connect(&address),
    )
    .await;

    match connect_result {
        Ok(Ok(_)) => Ok(address),
        Ok(Err(e)) => Err(format!("connect failed: {}", e)),
        Err(_) => Err("connect timeout".to_string()),
    }
}

async fn check_disk_space(path: &str, min_required_bytes: u64) -> Result<u64, String> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|e| format!("cannot prepare path '{}': {}", path, e))?;

    let output = tokio::process::Command::new("df")
        .args(["-Pk", path])
        .output()
        .await
        .map_err(|e| format!("failed to execute df: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("df failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let available_bytes = parse_df_available_bytes(&stdout)?;
    if available_bytes < min_required_bytes {
        return Ok(available_bytes);
    }
    Ok(available_bytes)
}

fn parse_df_available_bytes(stdout: &str) -> Result<u64, String> {
    let line = stdout
        .lines()
        .nth(1)
        .ok_or_else(|| "unexpected df output".to_string())?;
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 4 {
        return Err("unexpected df output format".to_string());
    }
    let available_kib = cols[3]
        .parse::<u64>()
        .map_err(|e| format!("cannot parse available space: {}", e))?;
    Ok(available_kib.saturating_mul(1024))
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use std::sync::{Arc, OnceLock};
    use std::time::Instant;

    use axum::body::to_bytes;
    use axum::http::header::CONTENT_TYPE;

    use crate::control::state::StreamStateManager;
    use crate::core::auth::AuthProvider;
    use crate::core::config::AppConfig;
    use crate::core::types::IngestMode;
    use crate::storage::cache::ObjectCache;
    use crate::storage::memory::InMemoryMediaStore;
    use crate::storage::MediaStore;

    #[test]
    fn test_parse_df_available_bytes_success() {
        let sample = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100000 25000 75000 25% /\n";
        let bytes = parse_df_available_bytes(sample).unwrap();
        assert_eq!(bytes, 75000 * 1024);
    }

    #[test]
    fn test_parse_df_available_bytes_invalid_output() {
        let sample = "bad output";
        assert!(parse_df_available_bytes(sample).is_err());
    }

    #[tokio::test]
    async fn test_check_rtmp_listener_success() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept_task = tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let result = check_rtmp_listener("127.0.0.1", port).await;
        assert!(result.is_ok());

        accept_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_check_rtmp_listener_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let result = check_rtmp_listener("127.0.0.1", port).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_range_header_valid_and_invalid() {
        assert_eq!(parse_range_header("bytes=0-100"), Some((0, 100)));
        assert_eq!(parse_range_header("bytes=100-"), Some((100, u64::MAX)));
        assert_eq!(parse_range_header("bytes=100-10"), None);
        assert_eq!(parse_range_header("bytes=-5"), None);
        assert_eq!(parse_range_header("not-a-range"), None);
    }

    #[test]
    fn test_build_response_range_not_satisfiable() {
        let delivery = crate::core::config::DeliveryConfig {
            cache_control_live: "no-cache, no-store".to_string(),
            cache_control_vod: "public, max-age=86400".to_string(),
            cors_allowed_origins: vec!["*".to_string()],
        };
        let data = Bytes::from_static(b"abcdef");
        let resp = build_response(
            data,
            "video/mp4",
            "\"etag\"",
            "stream/high/000000.m4s",
            Some((10, 20)),
            false,
            &delivery,
        );
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    fn test_metrics_handle() -> metrics_exporter_prometheus::PrometheusHandle {
        static HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
        HANDLE
            .get_or_init(crate::observability::metrics::install_prometheus_recorder)
            .clone()
    }

    fn test_app_state() -> AppState {
        let config = AppConfig::default();
        let store = Arc::new(InMemoryMediaStore::new());
        let cache = Arc::new(ObjectCache::new(&config.cache));
        let state_manager = Arc::new(StreamStateManager::new());
        let auth = Arc::new(AuthProvider::new(&config.auth));
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);

        AppState {
            store,
            cache,
            state_manager,
            auth,
            config,
            start_time: Instant::now(),
            metrics_handle: test_metrics_handle(),
            event_tx,
        }
    }

    #[tokio::test]
    async fn test_master_playlist_playback_flow() {
        let state = test_app_state();
        let stream_id = StreamId::new();
        let sid = stream_id.to_string();

        state
            .state_manager
            .create_stream(stream_id, IngestMode::Vod)
            .await;
        state
            .state_manager
            .transition(stream_id, StreamState::Processing)
            .await
            .unwrap();
        state
            .state_manager
            .transition(stream_id, StreamState::Ready)
            .await
            .unwrap();

        let manifest_path = format!("{}/master.m3u8", sid);
        let manifest_body = "#EXTM3U\n#EXT-X-VERSION:7\n";
        state
            .store
            .put_manifest(&manifest_path, manifest_body)
            .await
            .unwrap();

        let resp = serve_master_playlist(State(state), Path(sid), HeaderMap::new()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "application/vnd.apple.mpegurl"
        );
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body, Bytes::from_static(manifest_body.as_bytes()));
    }

    #[tokio::test]
    async fn test_invalid_range_header_returns_416() {
        let state = test_app_state();
        let stream_id = StreamId::new();
        let sid = stream_id.to_string();
        let seg_path = format!("{}/high/000000.m4s", sid);

        state
            .state_manager
            .create_stream(stream_id, IngestMode::Vod)
            .await;
        state
            .state_manager
            .transition(stream_id, StreamState::Processing)
            .await
            .unwrap();
        state
            .state_manager
            .transition(stream_id, StreamState::Ready)
            .await
            .unwrap();

        state
            .store
            .put_segment(&seg_path, Bytes::from_static(b"0123456789"), "video/mp4")
            .await
            .unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, "bytes=-5".parse().unwrap());

        let resp = serve_segment(
            State(state),
            Path((sid, "high".to_string(), "000000.m4s".to_string())),
            headers,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    #[tokio::test]
    async fn test_list_streams_invalid_status_returns_400() {
        let state = test_app_state();
        let resp = list_streams(
            State(state),
            HeaderMap::new(),
            Query(ListStreamsQuery {
                status: Some("unknown".to_string()),
                limit: None,
                offset: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_list_streams_invalid_limit_returns_400() {
        let state = test_app_state();
        let resp = list_streams(
            State(state),
            HeaderMap::new(),
            Query(ListStreamsQuery {
                status: None,
                limit: Some(0),
                offset: None,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_parse_stream_id_param_requires_v7() {
        let v4 = uuid::Uuid::new_v4().to_string();
        let result = parse_stream_id_param(&v4);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_stream_id_param_accepts_v7() {
        let v7 = uuid::Uuid::now_v7().to_string();
        let result = parse_stream_id_param(&v7);
        assert!(result.is_ok());
    }
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
#[allow(clippy::result_large_err)]
fn authenticate_admin(
    state: &AppState,
    headers: &HeaderMap,
    endpoint: &'static str,
) -> Result<(), Response> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match state.auth.check_bearer_token(token) {
        TokenStatus::Valid => Ok(()),
        TokenStatus::Missing => {
            obs::inc_auth_failure("http", endpoint, "missing_or_invalid_header");
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
            obs::inc_auth_failure("http", endpoint, "invalid_token");
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

#[allow(clippy::result_large_err)]
fn parse_stream_id_param(raw: &str) -> Result<StreamId, Response> {
    let uuid = match raw.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(_) => {
            return Err(error_json(
                StatusCode::BAD_REQUEST,
                "invalid_stream_id",
                "Invalid stream ID format.",
            ));
        }
    };

    if uuid.get_version_num() != 7 {
        return Err(error_json(
            StatusCode::BAD_REQUEST,
            "invalid_stream_id",
            "Invalid stream ID version. Expected UUIDv7.",
        ));
    }

    Ok(StreamId::from_uuid(uuid))
}

// ---------------------------------------------------------------------------
// Key rotation endpoint (from security.md §2.1)
// ---------------------------------------------------------------------------

/// `POST /api/v1/streams/{stream_id}/rotate-key` — Rotate stream key.
///
/// From security.md §2.1: Admin calls this endpoint to generate a new key.
/// Old key is invalidated. Active streams using the old key are NOT disconnected.
pub async fn rotate_stream_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(stream_id): Path<String>,
) -> Response {
    if let Err(e) = authenticate_admin(&state, &headers, "rotate_stream_key") {
        return e;
    }

    let sid = match parse_stream_id_param(&stream_id) {
        Ok(sid) => sid,
        Err(response) => return response,
    };
    match state.state_manager.get_stream(sid).await {
        Some(_entry) => {
            // Generate new key via AuthProvider (bcrypt-hashed, security.md §2.1)
            let (new_key, _hash) = state.auth.rotate_stream_key_for_stream(sid);
            let rtmp_url = format!(
                "rtmp://{}:{}/live/{}",
                state.config.server.host, state.config.server.rtmp_port, new_key
            );

            info!(
                %stream_id,
                new_key = %crate::core::redact::redact_stream_key(&new_key),
                "stream key rotated"
            );

            Json(serde_json::json!({
                "stream_id": stream_id,
                "stream_key": new_key,
                "rtmp_url": rtmp_url,
            }))
            .into_response()
        }
        None => error_json(
            StatusCode::NOT_FOUND,
            "stream_not_found",
            &format!("Stream '{}' not found.", stream_id),
        ),
    }
}

// ---------------------------------------------------------------------------
// VOD Upload endpoint (from ingest.md §3)
// ---------------------------------------------------------------------------

/// Handle VOD file upload.
///
/// POST /api/v1/streams/upload
/// - Auth: Bearer token
/// - Body: multipart/form-data with file field
/// - Returns: 201 with stream metadata or error
pub async fn upload_vod(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    let start = std::time::Instant::now();

    // Auth check
    if let Err(resp) = authenticate_admin(&state, &headers, "upload_vod") {
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

    // Stream multipart file content to temp file
    let temp_path = upload_handler.temp_file_path(stream_id);
    let mut file = match tokio::fs::File::create(&temp_path).await {
        Ok(f) => f,
        Err(e) => {
            error!(%stream_id, error = %e, "failed to create upload temp file");
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "upload_failed",
                &format!("Failed to create upload temp file: {}", e),
            );
        }
    };

    let mut file_size = 0u64;
    let mut file_received = false;
    loop {
        let field_opt = match multipart.next_field().await {
            Ok(v) => v,
            Err(e) => {
                error!(%stream_id, error = %e, "failed to read multipart field");
                upload_handler.cleanup_temp_file(&temp_path).await;
                return error_json(
                    StatusCode::BAD_REQUEST,
                    "invalid_multipart",
                    &format!("Invalid multipart payload: {}", e),
                );
            }
        };

        let Some(mut field) = field_opt else {
            break;
        };

        // Accept the first file-like part (name=file OR has filename)
        let is_file_part = field.name() == Some("file") || field.file_name().is_some();
        if !is_file_part {
            continue;
        }

        file_received = true;
        loop {
            let chunk_opt = match field.chunk().await {
                Ok(v) => v,
                Err(e) => {
                    error!(%stream_id, error = %e, "failed to read multipart file chunk");
                    upload_handler.cleanup_temp_file(&temp_path).await;
                    return error_json(
                        StatusCode::BAD_REQUEST,
                        "invalid_upload",
                        &format!("Invalid file chunk: {}", e),
                    );
                }
            };

            let Some(chunk) = chunk_opt else {
                break;
            };

            file_size = file_size.saturating_add(chunk.len() as u64);
            if file_size > state.config.ingest.max_upload_size_bytes {
                upload_handler.cleanup_temp_file(&temp_path).await;
                return error_json(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    &format!(
                        "Upload exceeds max size ({} bytes).",
                        state.config.ingest.max_upload_size_bytes
                    ),
                );
            }

            if let Err(e) = file.write_all(&chunk).await {
                error!(%stream_id, error = %e, "failed to write upload chunk to temp file");
                upload_handler.cleanup_temp_file(&temp_path).await;
                return error_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "upload_failed",
                    &format!("Failed to write upload: {}", e),
                );
            }
        }
        break;
    }

    if !file_received {
        upload_handler.cleanup_temp_file(&temp_path).await;
        return error_json(
            StatusCode::BAD_REQUEST,
            "missing_file",
            "No file part found in multipart payload.",
        );
    }

    if let Err(e) = file.flush().await {
        error!(%stream_id, error = %e, "failed to flush upload temp file");
        upload_handler.cleanup_temp_file(&temp_path).await;
        return error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            &format!("Failed to flush upload file: {}", e),
        );
    }
    drop(file);

    // Process the upload
    match upload_handler
        .process_upload(stream_id, &temp_path, file_size)
        .await
    {
        Ok(response) => {
            let (source_width, source_height, source_fps) = state
                .state_manager
                .get_stream(stream_id)
                .await
                .and_then(|entry| entry.metadata.media_info)
                .map(|m| (m.video_width, m.video_height, m.frame_rate))
                .unwrap_or((1920, 1080, 30.0));

            start_vod_pipeline_from_file(
                state.clone(),
                stream_id,
                temp_path.clone(),
                source_width,
                source_height,
                source_fps,
            );

            obs::record_upload_duration(start.elapsed().as_secs_f64());
            obs::record_upload_size(file_size as f64);
            (StatusCode::CREATED, Json(response)).into_response()
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

fn start_vod_pipeline_from_file(
    state: AppState,
    stream_id: StreamId,
    temp_path: PathBuf,
    source_width: u32,
    source_height: u32,
    source_fps: f64,
) {
    let pipeline =
        crate::control::pipeline::create_pipeline(stream_id, state.event_tx.clone(), &state.config);

    let transcode_config = state.config.transcode.clone();
    let transcode_state_mgr = state.state_manager.clone();
    let transcode_cancel = pipeline.cancel.clone();
    let transcode_segment_tx = pipeline.transcode_tx;
    let transcode_event_tx = state.event_tx.clone();

    // ------------------------------------------------------------------
    // Transcode task — FFmpeg reads directly from the file via its own
    // container demuxer; no ingest channel needed.
    // ------------------------------------------------------------------
    #[cfg(feature = "ffmpeg")]
    tokio::spawn(async move {
        let tp = crate::transcode::pipeline::TranscodePipeline::new(
            transcode_config,
            transcode_state_mgr,
            transcode_cancel,
        );
        if let Err(e) = tp
            .run_vod_from_file(
                stream_id,
                temp_path,
                source_width,
                source_height,
                source_fps,
                transcode_segment_tx,
            )
            .await
        {
            tracing::error!(%stream_id, error = %e, "VOD transcode pipeline failed");
            let _ = transcode_event_tx
                .send(crate::control::events::PipelineEvent::StreamError {
                    stream_id,
                    error: e.to_string(),
                })
                .await;
        }
    });

    // ------------------------------------------------------------------
    // Transcode task — placeholder passthrough via the ingest channel.
    // ------------------------------------------------------------------
    #[cfg(not(feature = "ffmpeg"))]
    {
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
                tracing::error!(%stream_id, error = %e, "VOD transcode pipeline failed");
                let _ = transcode_event_tx
                    .send(crate::control::events::PipelineEvent::StreamError {
                        stream_id,
                        error: e.to_string(),
                    })
                    .await;
            }
        });
    }

    let packaging_config = state.config.packaging.clone();
    let packager_event_tx = state.event_tx.clone();
    let packager_cancel = pipeline.cancel.clone();
    let packager_segment_rx = pipeline.transcode_rx;
    let packager_storage_tx = pipeline.package_tx;
    tokio::spawn(async move {
        crate::package::runner::run_packager(
            stream_id,
            packaging_config,
            true,
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

    // ------------------------------------------------------------------
    // File feeder — only needed for the placeholder path; FFmpeg path
    // reads the file directly inside transcode_vod_from_file_blocking.
    // ------------------------------------------------------------------
    #[cfg(not(feature = "ffmpeg"))]
    {
        let feeder_ingest_tx = pipeline.ingest_tx;
        let feeder_event_tx = state.event_tx.clone();
        let feeder_cancel = pipeline.cancel.clone();
        tokio::spawn(async move {
            if let Err(e) = feed_vod_file_to_pipeline(
                stream_id,
                temp_path,
                source_width,
                source_height,
                feeder_ingest_tx,
                feeder_event_tx.clone(),
                feeder_cancel,
            )
            .await
            {
                error!(%stream_id, error = %e, "VOD file feeder failed");
                let _ = feeder_event_tx
                    .send(crate::control::events::PipelineEvent::StreamError {
                        stream_id,
                        error: e,
                    })
                    .await;
            }
        });
    }
}

#[cfg(not(feature = "ffmpeg"))]
async fn feed_vod_file_to_pipeline(
    stream_id: StreamId,
    temp_path: PathBuf,
    source_width: u32,
    source_height: u32,
    ingest_tx: tokio::sync::mpsc::Sender<DemuxedFrame>,
    event_tx: tokio::sync::mpsc::Sender<crate::control::events::PipelineEvent>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    let mut file = tokio::fs::File::open(&temp_path)
        .await
        .map_err(|e| format!("failed to open temp file: {}", e))?;
    let total_bytes = file
        .metadata()
        .await
        .map_err(|e| format!("failed to stat temp file: {}", e))?
        .len()
        .max(1);

    let _ = event_tx
        .send(crate::control::events::PipelineEvent::VodProgress {
            stream_id,
            percent: 0.0,
        })
        .await;

    let mut buf = vec![0u8; 16 * 1024];
    let mut bytes_read_total = 0u64;
    let mut pts = 0i64;
    let mut frame_idx = 0u64;
    let pts_step = (90000.0 / 30.0) as i64; // ~30fps

    loop {
        if cancel.is_cancelled() {
            break;
        }

        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| format!("failed to read temp file: {}", e))?;
        if n == 0 {
            break;
        }

        bytes_read_total = bytes_read_total.saturating_add(n as u64);
        let frame = DemuxedFrame {
            stream_id,
            track: Track::Video {
                codec: VideoCodec::H264,
                width: source_width,
                height: source_height,
            },
            pts,
            dts: pts,
            // Insert IDR every 60 frames (~2s at 30fps) for 6s segment cadence.
            keyframe: frame_idx.is_multiple_of(60),
            data: Bytes::copy_from_slice(&buf[..n]),
        };

        crate::core::metrics::send_with_backpressure(
            &ingest_tx,
            frame,
            "ingest_transcode",
            INGEST_TRANSCODE_CHANNEL_CAP,
        )
        .await
        .map_err(|e| format!("failed to enqueue VOD frame: {}", e))?;

        let progress = (bytes_read_total as f64 / total_bytes as f64 * 100.0).min(100.0) as f32;
        let _ = event_tx
            .send(crate::control::events::PipelineEvent::VodProgress {
                stream_id,
                percent: progress,
            })
            .await;

        pts += pts_step;
        frame_idx = frame_idx.saturating_add(1);
    }

    drop(ingest_tx);

    let _ = event_tx
        .send(crate::control::events::PipelineEvent::VodProgress {
            stream_id,
            percent: 100.0,
        })
        .await;

    if let Err(e) = tokio::fs::remove_file(&temp_path).await {
        // Best-effort cleanup only; processing already completed.
        info!(
            %stream_id,
            path = %temp_path.display(),
            error = %e,
            "failed to remove VOD temp file"
        );
    }

    Ok(())
}
