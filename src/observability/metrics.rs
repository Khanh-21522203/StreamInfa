use std::time::Instant;

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};

// ---------------------------------------------------------------------------
// Metrics catalog (from observability.md §2.2)
// ---------------------------------------------------------------------------

/// Register all metric descriptors at startup.
///
/// This must be called once before any metrics are recorded.
/// Descriptors provide human-readable descriptions for Prometheus.
pub fn describe_all_metrics() {
    // -- Ingest metrics (§2.2) --
    describe_gauge!(
        "streaminfa_ingest_active_streams",
        "Currently active ingest streams"
    );
    describe_counter!(
        "streaminfa_ingest_connections_total",
        "Total ingest connections (including failed auth)"
    );
    describe_counter!("streaminfa_ingest_bytes_total", "Total bytes ingested");
    describe_counter!("streaminfa_ingest_frames_total", "Total frames demuxed");
    describe_counter!("streaminfa_ingest_errors_total", "Ingest errors by type");
    describe_gauge!(
        "streaminfa_ingest_bitrate_bps",
        "Current ingest bitrate (rolling 5s window)"
    );
    describe_gauge!(
        "streaminfa_ingest_channel_utilization",
        "Ingest->transcode channel fullness (0.0-1.0)"
    );
    describe_histogram!("streaminfa_upload_duration_seconds", "VOD upload duration");
    describe_histogram!("streaminfa_upload_size_bytes", "VOD upload file size");

    // -- Transcode metrics (§2.2) --
    describe_gauge!(
        "streaminfa_transcode_active_jobs",
        "Currently active transcode jobs"
    );
    describe_counter!(
        "streaminfa_transcode_segments_total",
        "Total segments produced"
    );
    describe_histogram!(
        "streaminfa_transcode_segment_duration_seconds",
        "Actual segment duration distribution"
    );
    describe_histogram!(
        "streaminfa_transcode_latency_seconds",
        "Time from first frame input to segment output"
    );
    describe_gauge!(
        "streaminfa_transcode_fps",
        "Current encoding frames per second"
    );
    describe_gauge!(
        "streaminfa_transcode_queue_depth",
        "Ingest->transcode channel current depth"
    );
    describe_counter!(
        "streaminfa_transcode_errors_total",
        "Transcode errors by type"
    );
    describe_gauge!(
        "streaminfa_vod_progress_percent",
        "VOD transcode progress (0.0-100.0)"
    );

    // -- Packaging metrics (§2.2) --
    describe_counter!(
        "streaminfa_package_segments_written_total",
        "fMP4 segments written to storage"
    );
    describe_counter!(
        "streaminfa_package_manifest_updates_total",
        "Manifest file updates"
    );
    describe_histogram!(
        "streaminfa_package_mux_duration_seconds",
        "Time to mux one fMP4 segment"
    );

    // -- Storage metrics (§2.2) --
    describe_histogram!("streaminfa_storage_put_duration_seconds", "S3 PUT latency");
    describe_histogram!("streaminfa_storage_get_duration_seconds", "S3 GET latency");
    describe_counter!(
        "streaminfa_storage_put_bytes_total",
        "Total bytes written to S3"
    );
    describe_counter!(
        "streaminfa_storage_get_bytes_total",
        "Total bytes read from S3"
    );
    describe_counter!(
        "streaminfa_storage_errors_total",
        "Storage operation errors"
    );
    describe_counter!(
        "streaminfa_storage_retries_total",
        "Storage operation retries"
    );
    describe_counter!(
        "streaminfa_storage_segments_deleted_total",
        "Segments cleaned up by retention policy"
    );
    describe_counter!("streaminfa_cache_hits_total", "LRU cache hits");
    describe_counter!("streaminfa_cache_misses_total", "LRU cache misses");
    describe_gauge!("streaminfa_cache_size_bytes", "Current cache memory usage");
    describe_gauge!(
        "streaminfa_cache_entries",
        "Current number of cached objects"
    );

    // -- Delivery metrics (§2.2) --
    describe_counter!(
        "streaminfa_delivery_requests_total",
        "HTTP requests to origin"
    );
    describe_histogram!(
        "streaminfa_delivery_request_duration_seconds",
        "Origin response latency"
    );
    describe_counter!(
        "streaminfa_delivery_bytes_sent_total",
        "Total bytes served to players/CDN"
    );
    describe_gauge!(
        "streaminfa_delivery_active_connections",
        "Current active HTTP connections for delivery"
    );

    // -- System metrics (§2.2) --
    describe_gauge!("streaminfa_uptime_seconds", "Process uptime");
    describe_gauge!("streaminfa_streams_total", "Stream count by state");
    describe_counter!(
        "streaminfa_panic_total",
        "Total panics caught (should always be 0)"
    );
    describe_counter!("streaminfa_config_reload_total", "Config reload attempts");
    describe_gauge!(
        "streaminfa_shutdown_in_progress",
        "1 if graceful shutdown is in progress, 0 otherwise"
    );

    // -- Backpressure metrics (§2.2) --
    // These are also described in core::metrics, but we re-describe here for completeness
    describe_gauge!(
        "streaminfa_channel_utilization",
        "Channel fullness ratio (0.0-1.0)"
    );
    describe_histogram!(
        "streaminfa_channel_send_wait_seconds",
        "Time blocked on channel send"
    );
    describe_counter!(
        "streaminfa_backpressure_events_total",
        "Sends that blocked > 100ms"
    );
}

// ---------------------------------------------------------------------------
// Metric recording helpers
// ---------------------------------------------------------------------------

// -- Ingest --

pub fn inc_ingest_active_streams(protocol: &str) {
    gauge!("streaminfa_ingest_active_streams", "protocol" => protocol.to_string()).increment(1.0);
}

pub fn dec_ingest_active_streams(protocol: &str) {
    gauge!("streaminfa_ingest_active_streams", "protocol" => protocol.to_string()).decrement(1.0);
}

pub fn inc_ingest_connections(protocol: &str) {
    counter!("streaminfa_ingest_connections_total", "protocol" => protocol.to_string())
        .increment(1);
}

pub fn add_ingest_bytes(protocol: &str, stream_id: &str, bytes: u64) {
    counter!("streaminfa_ingest_bytes_total", "protocol" => protocol.to_string(), "stream_id" => stream_id.to_string()).increment(bytes);
}

pub fn inc_ingest_frames(stream_id: &str, track: &str) {
    counter!("streaminfa_ingest_frames_total", "stream_id" => stream_id.to_string(), "track" => track.to_string()).increment(1);
}

pub fn inc_ingest_error(protocol: &str, error_type: &str) {
    counter!("streaminfa_ingest_errors_total", "protocol" => protocol.to_string(), "error_type" => error_type.to_string()).increment(1);
}

pub fn set_ingest_bitrate(stream_id: &str, bps: f64) {
    gauge!("streaminfa_ingest_bitrate_bps", "stream_id" => stream_id.to_string()).set(bps);
}

pub fn record_upload_duration(seconds: f64) {
    histogram!("streaminfa_upload_duration_seconds").record(seconds);
}

pub fn record_upload_size(bytes: f64) {
    histogram!("streaminfa_upload_size_bytes").record(bytes);
}

// -- Transcode --

pub fn set_transcode_active_jobs(count: f64) {
    gauge!("streaminfa_transcode_active_jobs").set(count);
}

pub fn inc_transcode_segments(stream_id: &str, rendition: &str) {
    counter!("streaminfa_transcode_segments_total", "stream_id" => stream_id.to_string(), "rendition" => rendition.to_string()).increment(1);
}

pub fn record_transcode_segment_duration(rendition: &str, seconds: f64) {
    histogram!("streaminfa_transcode_segment_duration_seconds", "rendition" => rendition.to_string()).record(seconds);
}

pub fn record_transcode_latency(rendition: &str, seconds: f64) {
    histogram!("streaminfa_transcode_latency_seconds", "rendition" => rendition.to_string())
        .record(seconds);
}

pub fn set_transcode_fps(stream_id: &str, rendition: &str, fps: f64) {
    gauge!("streaminfa_transcode_fps", "stream_id" => stream_id.to_string(), "rendition" => rendition.to_string()).set(fps);
}

pub fn set_transcode_queue_depth(stream_id: &str, depth: f64) {
    gauge!("streaminfa_transcode_queue_depth", "stream_id" => stream_id.to_string()).set(depth);
}

pub fn inc_transcode_error(stream_id: &str, error_type: &str) {
    counter!("streaminfa_transcode_errors_total", "stream_id" => stream_id.to_string(), "error_type" => error_type.to_string()).increment(1);
}

// -- Packaging --

pub fn inc_package_segments_written(stream_id: &str, rendition: &str) {
    counter!("streaminfa_package_segments_written_total", "stream_id" => stream_id.to_string(), "rendition" => rendition.to_string()).increment(1);
}

pub fn inc_package_manifest_updates(stream_id: &str, rendition: &str) {
    counter!("streaminfa_package_manifest_updates_total", "stream_id" => stream_id.to_string(), "rendition" => rendition.to_string()).increment(1);
}

pub fn record_package_mux_duration(rendition: &str, seconds: f64) {
    histogram!("streaminfa_package_mux_duration_seconds", "rendition" => rendition.to_string())
        .record(seconds);
}

// -- Storage --

pub fn record_storage_put_duration(object_type: &str, seconds: f64) {
    histogram!("streaminfa_storage_put_duration_seconds", "object_type" => object_type.to_string())
        .record(seconds);
}

pub fn record_storage_get_duration(object_type: &str, seconds: f64) {
    histogram!("streaminfa_storage_get_duration_seconds", "object_type" => object_type.to_string())
        .record(seconds);
}

pub fn add_storage_put_bytes(object_type: &str, bytes: u64) {
    counter!("streaminfa_storage_put_bytes_total", "object_type" => object_type.to_string())
        .increment(bytes);
}

pub fn add_storage_get_bytes(object_type: &str, bytes: u64) {
    counter!("streaminfa_storage_get_bytes_total", "object_type" => object_type.to_string())
        .increment(bytes);
}

pub fn inc_storage_error(operation: &str, error_type: &str) {
    counter!("streaminfa_storage_errors_total", "operation" => operation.to_string(), "error_type" => error_type.to_string()).increment(1);
}

pub fn inc_storage_retries(operation: &str) {
    counter!("streaminfa_storage_retries_total", "operation" => operation.to_string()).increment(1);
}

pub fn inc_storage_segments_deleted() {
    counter!("streaminfa_storage_segments_deleted_total").increment(1);
}

pub fn inc_cache_hit(object_type: &str) {
    counter!("streaminfa_cache_hits_total", "object_type" => object_type.to_string()).increment(1);
}

pub fn inc_cache_miss(object_type: &str) {
    counter!("streaminfa_cache_misses_total", "object_type" => object_type.to_string())
        .increment(1);
}

pub fn set_cache_size_bytes(bytes: f64) {
    gauge!("streaminfa_cache_size_bytes").set(bytes);
}

pub fn set_cache_entries(count: f64) {
    gauge!("streaminfa_cache_entries").set(count);
}

// -- Delivery --

pub fn inc_delivery_request(status: &str, object_type: &str) {
    counter!("streaminfa_delivery_requests_total", "status" => status.to_string(), "object_type" => object_type.to_string()).increment(1);
}

pub fn record_delivery_request_duration(object_type: &str, seconds: f64) {
    histogram!("streaminfa_delivery_request_duration_seconds", "object_type" => object_type.to_string()).record(seconds);
}

pub fn add_delivery_bytes_sent(bytes: u64) {
    counter!("streaminfa_delivery_bytes_sent_total").increment(bytes);
}

pub fn set_delivery_active_connections(count: f64) {
    gauge!("streaminfa_delivery_active_connections").set(count);
}

// -- System --

pub fn set_uptime_seconds(seconds: f64) {
    gauge!("streaminfa_uptime_seconds").set(seconds);
}

pub fn set_streams_total(status: &str, count: f64) {
    gauge!("streaminfa_streams_total", "status" => status.to_string()).set(count);
}

pub fn inc_panic_total() {
    counter!("streaminfa_panic_total").increment(1);
}

pub fn inc_config_reload(result: &str) {
    counter!("streaminfa_config_reload_total", "result" => result.to_string()).increment(1);
}

pub fn set_shutdown_in_progress(in_progress: bool) {
    gauge!("streaminfa_shutdown_in_progress").set(if in_progress { 1.0 } else { 0.0 });
}

// ---------------------------------------------------------------------------
// Status code bucket helper
// ---------------------------------------------------------------------------

/// Classify an object path into an object_type label for metrics.
///
/// From observability.md §2.4: object_type has 4 values (segment, manifest, init, metadata).
pub fn classify_object_type(path: &str) -> &'static str {
    if path.ends_with(".m3u8") {
        "manifest"
    } else if path.ends_with("init.mp4") {
        "init"
    } else if path.ends_with(".m4s") || path.ends_with(".mp4") {
        "segment"
    } else {
        "metadata"
    }
}

// ---------------------------------------------------------------------------
// Uptime tracking task
// ---------------------------------------------------------------------------

/// Spawn a background task that updates the uptime gauge every second.
///
/// From observability.md §2.2: `streaminfa_uptime_seconds` gauge.
pub async fn run_uptime_task(start_time: Instant, cancel: tokio_util::sync::CancellationToken) {
    let interval = std::time::Duration::from_secs(1);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(interval) => {
                set_uptime_seconds(start_time.elapsed().as_secs_f64());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Prometheus recorder installation
// ---------------------------------------------------------------------------

/// Install the Prometheus metrics recorder.
///
/// This sets up the global `metrics` recorder backed by `metrics-exporter-prometheus`.
/// Returns a handle that can render the metrics as Prometheus text exposition format.
pub fn install_prometheus_recorder() -> metrics_exporter_prometheus::PrometheusHandle {
    let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
    builder
        .install_recorder()
        .expect("failed to install Prometheus metrics recorder")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_object_type() {
        assert_eq!(classify_object_type("stream/master.m3u8"), "manifest");
        assert_eq!(classify_object_type("stream/high/media.m3u8"), "manifest");
        assert_eq!(classify_object_type("stream/high/init.mp4"), "init");
        assert_eq!(
            classify_object_type("stream/high/seg_000001.m4s"),
            "segment"
        );
        assert_eq!(classify_object_type("stream/metadata.json"), "metadata");
    }
}
