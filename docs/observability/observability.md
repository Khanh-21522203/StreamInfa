# StreamInfa — Observability

> **Purpose:** Define the complete observability stack: metrics, structured logging, distributed tracing, SLIs/SLOs, dashboards, and alerting.
> **Audience:** Engineers instrumenting code; SREs building dashboards and alerts; operators responding to incidents.

---

## 1. Observability Architecture

```
┌──────────────────────────────────────────────────────────────────────────┐
│                          StreamInfa Process                               │
│                                                                           │
│  ┌─────────────┐  ┌──────────────┐  ┌──────────────────────────────────┐ │
│  │  Metrics    │  │  Structured  │  │  Distributed Tracing             │ │
│  │ (prometheus │  │  Logging     │  │  (OpenTelemetry)                 │ │
│  │  crate)     │  │ (tracing +   │  │                                  │ │
│  │             │  │  tracing-    │  │  Spans: ingest, transcode,       │ │
│  │  Counters,  │  │  subscriber) │  │  package, store, deliver         │ │
│  │  Gauges,    │  │              │  │                                  │ │
│  │  Histograms │  │  JSON to     │  │  Export to Jaeger/OTLP           │ │
│  │             │  │  stdout      │  │  (optional, off by default)      │ │
│  └──────┬──────┘  └──────┬───────┘  └────────────┬─────────────────────┘ │
│         │                │                        │                       │
└─────────┼────────────────┼────────────────────────┼───────────────────────┘
          │                │                        │
          ▼                ▼                        ▼
   ┌────────────┐   ┌───────────┐          ┌──────────────┐
   │ Prometheus │   │  Log      │          │   Jaeger /   │
   │   Server   │   │ Collector │          │   Tempo      │
   │            │   │ (Loki,    │          │              │
   │            │   │  ELK)     │          │              │
   └─────┬──────┘   └─────┬─────┘          └──────┬───────┘
         │                │                        │
         ▼                ▼                        ▼
   ┌───────────────────────────────────────────────────────┐
   │                    Grafana                             │
   │    Dashboards, Alerts, Log Exploration, Trace View     │
   └───────────────────────────────────────────────────────┘
```

**[ASSUMPTION]** Operators maintain their own Prometheus + Grafana + log collector stack. StreamInfa exposes metrics and logs but does not bundle these tools.

---

## 2. Metrics

### 2.1 Metrics Implementation

StreamInfa uses the `prometheus` Rust crate to expose metrics in Prometheus text exposition format at `GET /metrics`.

**Module:** `src/observability/metrics.rs`
**Registry:** A global `prometheus::Registry` is created at startup. All metrics are registered once. Metric handles are cloned into components that need them.

### 2.2 Complete Metrics Catalog

#### Ingest Metrics

| Metric Name | Type | Labels | Description |
|-------------|------|--------|-------------|
| `streaminfa_ingest_active_streams` | Gauge | `protocol` (rtmp, http) | Currently active ingest streams |
| `streaminfa_ingest_connections_total` | Counter | `protocol` | Total ingest connections (including failed auth) |
| `streaminfa_ingest_bytes_total` | Counter | `protocol`, `stream_id` | Total bytes ingested |
| `streaminfa_ingest_frames_total` | Counter | `stream_id`, `track` (video, audio) | Total frames demuxed |
| `streaminfa_ingest_errors_total` | Counter | `protocol`, `error_type` | Ingest errors by type (auth_failed, invalid_codec, timeout, connection_reset) |
| `streaminfa_ingest_bitrate_bps` | Gauge | `stream_id` | Current ingest bitrate (rolling 5s window) |
| `streaminfa_ingest_channel_utilization` | Gauge | `stream_id` | Ingest→transcode channel fullness (0.0–1.0) |
| `streaminfa_upload_duration_seconds` | Histogram | | VOD upload duration |
| `streaminfa_upload_size_bytes` | Histogram | | VOD upload file size |

#### Transcode Metrics

| Metric Name | Type | Labels | Description |
|-------------|------|--------|-------------|
| `streaminfa_transcode_active_jobs` | Gauge | | Currently active transcode jobs |
| `streaminfa_transcode_segments_total` | Counter | `stream_id`, `rendition` | Total segments produced |
| `streaminfa_transcode_segment_duration_seconds` | Histogram | `rendition` | Actual segment duration distribution |
| `streaminfa_transcode_latency_seconds` | Histogram | `rendition` | Time from first frame input to segment output |
| `streaminfa_transcode_fps` | Gauge | `stream_id`, `rendition` | Current encoding frames per second |
| `streaminfa_transcode_queue_depth` | Gauge | `stream_id` | Ingest→transcode channel current depth |
| `streaminfa_transcode_errors_total` | Counter | `stream_id`, `error_type` | Transcode errors (decode_failed, encode_failed, ffmpeg_init) |
| `streaminfa_vod_progress_percent` | Gauge | `stream_id` | VOD transcode progress (0.0–100.0) |

#### Packaging Metrics

| Metric Name | Type | Labels | Description |
|-------------|------|--------|-------------|
| `streaminfa_package_segments_written_total` | Counter | `stream_id`, `rendition` | fMP4 segments written to storage |
| `streaminfa_package_manifest_updates_total` | Counter | `stream_id`, `rendition` | Manifest file updates |
| `streaminfa_package_mux_duration_seconds` | Histogram | `rendition` | Time to mux one fMP4 segment |

#### Storage Metrics

| Metric Name | Type | Labels | Description |
|-------------|------|--------|-------------|
| `streaminfa_storage_put_duration_seconds` | Histogram | `object_type` (segment, manifest, init, metadata) | S3 PUT latency |
| `streaminfa_storage_get_duration_seconds` | Histogram | `object_type` | S3 GET latency |
| `streaminfa_storage_put_bytes_total` | Counter | `object_type` | Total bytes written to S3 |
| `streaminfa_storage_get_bytes_total` | Counter | `object_type` | Total bytes read from S3 |
| `streaminfa_storage_errors_total` | Counter | `operation` (put, get, delete), `error_type` | Storage operation errors |
| `streaminfa_storage_retries_total` | Counter | `operation` | Storage operation retries |
| `streaminfa_storage_segments_deleted_total` | Counter | | Segments cleaned up by retention policy |
| `streaminfa_cache_hits_total` | Counter | `object_type` | LRU cache hits |
| `streaminfa_cache_misses_total` | Counter | `object_type` | LRU cache misses |
| `streaminfa_cache_size_bytes` | Gauge | | Current cache memory usage |
| `streaminfa_cache_entries` | Gauge | | Current number of cached objects |

#### Delivery Metrics

| Metric Name | Type | Labels | Description |
|-------------|------|--------|-------------|
| `streaminfa_delivery_requests_total` | Counter | `status` (2xx, 4xx, 5xx), `object_type` | HTTP requests to origin |
| `streaminfa_delivery_request_duration_seconds` | Histogram | `object_type` | Origin response latency |
| `streaminfa_delivery_bytes_sent_total` | Counter | | Total bytes served to players/CDN |
| `streaminfa_delivery_active_connections` | Gauge | | Current active HTTP connections for delivery |

#### System Metrics

| Metric Name | Type | Labels | Description |
|-------------|------|--------|-------------|
| `streaminfa_uptime_seconds` | Gauge | | Process uptime |
| `streaminfa_streams_total` | Gauge | `status` (pending, live, processing, ready, error) | Stream count by state |
| `streaminfa_panic_total` | Counter | | Total panics caught (should always be 0) |
| `streaminfa_config_reload_total` | Counter | `result` (success, failure) | Config reload attempts |
| `streaminfa_shutdown_in_progress` | Gauge | | 1 if graceful shutdown is in progress, 0 otherwise |

#### Backpressure Metrics

| Metric Name | Type | Labels | Description |
|-------------|------|--------|-------------|
| `streaminfa_channel_utilization` | Gauge | `channel_name`, `stream_id` | Channel fullness ratio (0.0–1.0) |
| `streaminfa_channel_send_wait_seconds` | Histogram | `channel_name` | Time blocked on channel send |
| `streaminfa_backpressure_events_total` | Counter | `channel_name` | Sends that blocked > 100ms |

### 2.3 Histogram Buckets

| Metric Category | Bucket Values (seconds) | Rationale |
|-----------------|------------------------|-----------|
| **S3 latency** | 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0 | S3 latency is typically 50–500ms; need granularity in that range |
| **Delivery latency** | 0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0 | Cached responses should be < 1ms; uncached < 500ms |
| **Transcode latency** | 0.5, 1.0, 2.0, 4.0, 6.0, 8.0, 10.0, 15.0, 30.0 | Segment transcode takes 2–10s depending on rendition |
| **Segment duration** | 4.0, 5.0, 5.5, 6.0, 6.5, 7.0, 8.0, 10.0 | Target is 6s; buckets focus on ±2s around target |
| **Upload duration** | 1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0 | Uploads range from seconds to minutes |
| **Upload size** | 1e6, 1e7, 1e8, 5e8, 1e9, 5e9, 1e10 | 1 MB to 10 GB |

### 2.4 Label Cardinality Rules

**Critical rule:** Labels must have bounded cardinality. Unbounded labels (for example, `stream_id`) require explicit lifecycle management to avoid a cardinality explosion in Prometheus.

| Label | Bounded? | Management |
|-------|----------|------------|
| `protocol` | Yes (2 values: rtmp, http) | Safe |
| `rendition` | Yes (3 values: high, medium, low) | Safe |
| `object_type` | Yes (4 values: segment, manifest, init, metadata) | Safe |
| `status` | Yes (5 values: 2xx, 3xx, 4xx, 5xx, timeout) | Safe |
| `error_type` | Yes (enum, ~10 variants) | Safe |
| `stream_id` | **Unbounded** (new UUID per stream) | Allowed only for per-stream operational metrics in MVP scale. Metric series must be removed when streams are deleted/expired, and `stream_id` must not be used on global high-rate request counters. |
| `channel_name` | Yes (4 values: ingest_to_transcode, transcode_to_packager, packager_to_storage, events) | Safe |

**Cleanup:** When a stream transitions to `DELETED` (or ages out), remove all `stream_id`-labeled metric series (gauges and per-stream counters) using `MetricVec::remove_label_values(...)` and stream-lifecycle hooks in the control plane.

---

## 3. Structured Logging

### 3.1 Implementation

StreamInfa uses the `tracing` ecosystem:
- `tracing` — instrumentation API (spans, events)
- `tracing-subscriber` — log formatting and filtering
- `tracing-appender` — output to stdout (non-blocking writer)

### 3.2 Log Format

**JSON format (default for production):**
```json
{
  "timestamp": "2025-06-15T14:30:12.345678Z",
  "level": "INFO",
  "target": "streaminfa_ingest::rtmp",
  "message": "RTMP stream started",
  "stream_id": "01906b3a-7f1e-7000-8000-000000000001",
  "protocol": "rtmp",
  "remote_addr": "10.0.1.50:54321",
  "video_codec": "h264",
  "resolution": "1920x1080",
  "span": {
    "name": "rtmp_connection",
    "stream_id": "01906b3a-7f1e-7000-8000-000000000001"
  }
}
```

**Pretty format (for local development):**
```
2025-06-15T14:30:12.345Z  INFO streaminfa_ingest::rtmp: RTMP stream started stream_id=01906b3a... protocol=rtmp resolution=1920x1080
```

**Configuration:**
```toml
[observability]
log_level = "info"       # trace, debug, info, warn, error
log_format = "json"      # "json" or "pretty"
```

### 3.3 Log Level Guidelines

| Level | Usage | Example |
|-------|-------|---------|
| `ERROR` | Unrecoverable failures that affect stream processing | S3 write failed after all retries; FFmpeg encoder init failure |
| `WARN` | Recoverable issues or degraded operation | Single frame decode error (skipped); S3 retry succeeded after failure; channel utilization > 80% |
| `INFO` | Significant operational events | Stream started, stream ended, segment produced, upload completed, config reloaded |
| `DEBUG` | Detailed operational information | Frame timestamps, segment boundaries, cache hits/misses, channel send/receive |
| `TRACE` | Very detailed internals | Individual RTMP packet parsing, FFmpeg API calls, fMP4 box construction |

**Production default: `INFO`.** Debug and trace logs should never be enabled in production for sustained periods (volume is too high).

### 3.4 Structured Fields

Every log event includes structured fields relevant to the context:

| Context | Required Fields |
|---------|----------------|
| RTMP connection | `stream_id`, `remote_addr`, `protocol` |
| HTTP upload | `stream_id`, `remote_addr`, `content_length` |
| Transcode | `stream_id`, `rendition`, `segment_sequence` |
| Packaging | `stream_id`, `rendition`, `segment_sequence` |
| Storage | `stream_id`, `object_path`, `operation` (put/get/delete) |
| Delivery | `request_id`, `path`, `status_code`, `latency_ms` |
| API | `request_id`, `method`, `path`, `status_code`, `remote_addr` |

### 3.5 Sensitive Field Redaction

The `tracing-subscriber` layer includes a custom layer that redacts sensitive fields:

| Field Name Pattern | Redaction |
|-------------------|-----------|
| `token`, `key`, `secret`, `password`, `authorization` | Value replaced with `[REDACTED]` |
| `stream_key` | Show only first 6 chars: `sk_a1b***` |
| `access_key_id` | Show only last 4 chars: `***WXYZ` |

---

## 4. Distributed Tracing

### 4.1 Implementation

StreamInfa uses OpenTelemetry for distributed tracing, exported via OTLP (OpenTelemetry Protocol) to a collector (Jaeger, Tempo, or any OTLP-compatible backend).

**Dependencies:**
- `opentelemetry` — API
- `opentelemetry-otlp` — OTLP exporter
- `tracing-opentelemetry` — bridge between `tracing` spans and OpenTelemetry spans

**Configuration:**
```toml
[observability]
tracing_enabled = false                    # Disabled by default (adds overhead)
tracing_endpoint = "http://localhost:4317" # OTLP gRPC endpoint
tracing_sample_rate = 0.1                  # Sample 10% of traces in production
```

### 4.2 Span Hierarchy

Each media pipeline operation is wrapped in a tracing span, forming a hierarchy:

```
[stream_lifecycle] stream_id=abc-123 (root span, covers entire stream)
  │
  ├── [rtmp_connection] remote_addr=10.0.1.50:54321
  │     ├── [rtmp_handshake] duration=15ms
  │     ├── [rtmp_auth] stream_key=sk_a1b*** result=success
  │     └── [rtmp_demux] frames=1500 duration=30s
  │
  ├── [transcode_pipeline] renditions=3
  │     ├── [transcode_rendition] rendition=high
  │     │     ├── [decode_gop] gop_num=1 frames=60
  │     │     ├── [scale_frames] from=1920x1080 to=1920x1080
  │     │     ├── [encode_gop] gop_num=1 frames=60 bitrate=3500kbps
  │     │     └── [segment_complete] sequence=0 duration=6.006s
  │     ├── [transcode_rendition] rendition=medium
  │     │     └── ...
  │     └── [transcode_rendition] rendition=low
  │           └── ...
  │
  ├── [hls_packaging] stream_id=abc-123
  │     ├── [mux_init_segment] rendition=high size=1234
  │     ├── [mux_media_segment] rendition=high sequence=0 duration=6.006s
  │     └── [generate_manifest] rendition=high segments=1
  │
  └── [storage_write] stream_id=abc-123
        ├── [s3_put] path=abc-123/high/init.mp4 size=1234 latency=45ms
        ├── [s3_put] path=abc-123/high/seg_000000.m4s size=2097152 latency=120ms
        └── [s3_put] path=abc-123/high/media.m3u8 size=456 latency=30ms
```

### 4.3 Trace Context Propagation

Within the StreamInfa process, trace context propagates naturally through `tracing` spans (no network hops in the modular monolith). Each component enters a child span:

```
ingest task:    span = tracing::info_span!("rtmp_demux", stream_id = %id);
                // sends DemuxedFrame to channel
                
transcode task: // receives DemuxedFrame from channel
                span = tracing::info_span!("transcode_rendition", stream_id = %id, rendition = %r);
```

**Cross-channel span linking:** When a message crosses a channel boundary (ingest → transcode), the receiving task creates a new span that links back to the sending span's trace ID. This is done by including the `tracing::Span::current().id()` in the channel message and using `span.follows_from(parent_id)` on the receiving side.

### 4.4 Sampling Strategy

| Environment | Sample Rate | Rationale |
|-------------|-------------|-----------|
| Development | 1.0 (100%) | Full visibility for debugging |
| Production | 0.1 (10%) | Tracing adds ~5% CPU overhead; 10% sampling provides sufficient visibility for troubleshooting |
| Incident investigation | 1.0 (100%, temporarily) | Increase via config reload during active incidents |

---

## 5. SLIs and SLOs

### 5.1 Service Level Indicators (SLIs)

| SLI | Definition | Measurement |
|-----|-----------|-------------|
| **Availability** | Proportion of time the system can accept ingest and serve delivery | `1 - (minutes_of_downtime / total_minutes_in_month)`. Downtime = `/healthz` returns non-200 or process is not running. |
| **Ingest Success Rate** | Proportion of authenticated ingest attempts that successfully produce at least 1 segment | `streaminfa_transcode_segments_total > 0` for streams that reached `LIVE` state |
| **Segment Freshness (Live)** | Time from segment production to availability at origin | `now() - last_segment_storage_write_time`. Measured per stream. |
| **VOD Processing Time** | Time from upload completion to stream reaching `READY` state | `ready_at - upload_completed_at` |
| **Delivery Latency** | p99 latency of segment serving at the origin | `histogram_quantile(0.99, streaminfa_delivery_request_duration_seconds)` |
| **Delivery Error Rate** | Proportion of delivery requests that return 5xx | `rate(streaminfa_delivery_requests_total{status="5xx"}) / rate(streaminfa_delivery_requests_total)` |

### 5.2 Service Level Objectives (SLOs)

| SLO | Target | Window | Consequence of Breach |
|-----|--------|--------|----------------------|
| **Availability** | 99.5% | Monthly (rolling 30 days) | Allowed downtime: 3.6 hours/month. Breach triggers post-mortem. |
| **Ingest Success Rate** | 99.0% | Weekly | Breach triggers investigation of ingest/transcode failures. |
| **Segment Freshness** | < 10 seconds (p99) | Per-stream, rolling 5 minutes | Breach triggers alert. Indicates pipeline backpressure or storage latency. |
| **VOD Processing Time** | ≤ 2× source duration (p95) | Per-job | Breach is informational. May indicate CPU saturation. |
| **Delivery Latency** | < 50ms (p99) | Rolling 5 minutes | Breach triggers alert. Indicates cache miss storm or storage issue. |
| **Delivery Error Rate** | < 0.1% | Rolling 5 minutes | Breach triggers alert. Indicates storage failure or origin overload. |

### 5.3 Error Budget

For the 99.5% availability SLO over 30 days:
- Total minutes: 43,200
- Error budget: 216 minutes (3.6 hours)
- If error budget is exhausted, freeze feature deployments and focus on reliability improvements.

---

## 6. Dashboards

### 6.1 Dashboard Layout

StreamInfa requires three Grafana dashboards:

#### Dashboard 1: Overview

**Purpose:** High-level system health at a glance.

| Panel | Visualization | Query |
|-------|--------------|-------|
| **Active Streams** | Stat | `streaminfa_streams_total{status="live"}` |
| **Total Streams by State** | Pie chart | `streaminfa_streams_total` by `status` |
| **Ingest Bitrate (all streams)** | Time series | `sum(streaminfa_ingest_bitrate_bps)` |
| **Transcode FPS (all renditions)** | Time series | `streaminfa_transcode_fps` by `rendition` |
| **Delivery Request Rate** | Time series | `rate(streaminfa_delivery_requests_total[1m])` |
| **Delivery Error Rate** | Time series | `rate(streaminfa_delivery_requests_total{status="5xx"}[1m]) / rate(streaminfa_delivery_requests_total[1m])` |
| **S3 PUT Latency p99** | Time series | `histogram_quantile(0.99, rate(streaminfa_storage_put_duration_seconds_bucket[5m]))` |
| **Cache Hit Ratio** | Gauge | `rate(streaminfa_cache_hits_total[5m]) / (rate(streaminfa_cache_hits_total[5m]) + rate(streaminfa_cache_misses_total[5m]))` |
| **System Uptime** | Stat | `streaminfa_uptime_seconds` |
| **Panics** | Stat (alert color) | `streaminfa_panic_total` |

#### Dashboard 2: Per-Stream Detail

**Purpose:** Drill into a specific stream's pipeline health.

| Panel | Visualization | Query (template variable: `$stream_id`) |
|-------|--------------|-------|
| **Stream State** | Stat | From stream API (or derive from metrics) |
| **Ingest Bitrate** | Time series | `streaminfa_ingest_bitrate_bps{stream_id="$stream_id"}` |
| **Channel Utilization** | Time series (stacked) | `streaminfa_channel_utilization{stream_id="$stream_id"}` by `channel_name` |
| **Segments Produced** | Time series | `rate(streaminfa_transcode_segments_total{stream_id="$stream_id"}[1m])` by `rendition` |
| **Transcode FPS** | Time series | `streaminfa_transcode_fps{stream_id="$stream_id"}` by `rendition` |
| **Transcode Latency** | Heatmap | `streaminfa_transcode_latency_seconds{stream_id="$stream_id"}` |
| **Errors** | Time series | `rate(streaminfa_transcode_errors_total{stream_id="$stream_id"}[1m])` |
| **S3 Write Latency** | Time series | `histogram_quantile(0.99, rate(streaminfa_storage_put_duration_seconds_bucket{stream_id="$stream_id"}[5m]))` |

#### Dashboard 3: Infrastructure

**Purpose:** System resource usage and capacity.

| Panel | Visualization | Query |
|-------|--------------|-------|
| **CPU Usage** | Time series | `process_cpu_seconds_total` (from Prometheus process metrics) |
| **Memory RSS** | Time series | `process_resident_memory_bytes` |
| **Open File Descriptors** | Time series | `process_open_fds` |
| **Cache Size** | Time series | `streaminfa_cache_size_bytes` |
| **Tokio Task Count** | Time series | `tokio_tasks_active` (if using tokio-metrics) |
| **S3 Connection Pool** | Time series | Active connections to S3 (from AWS SDK metrics if exposed) |

---

## 7. Alerting Rules

### 7.1 Critical Alerts (Page)

| Alert | Condition | Duration | Action |
|-------|-----------|----------|--------|
| **StreamInfaDown** | `up{job="streaminfa"} == 0` | 1 min | Process crashed or unreachable. Check container/host. |
| **HighDeliveryErrorRate** | `delivery_error_rate > 0.01` (1%) | 5 min | Storage failure or origin overload. See runbook. |
| **PanicDetected** | `increase(streaminfa_panic_total[5m]) > 0` | 0 (instant) | Bug in code. Investigate immediately. |
| **StorageUnreachable** | `increase(streaminfa_storage_errors_total{error_type="connection_refused"}[5m]) > 10` | 2 min | S3/MinIO is down. Check storage backend. |
| **AllStreamsErrored** | `streaminfa_streams_total{status="live"} == 0 AND streaminfa_streams_total{status="error"} > 0` | 5 min | All live streams have failed. Systemic issue. |

### 7.2 Warning Alerts (Notify)

| Alert | Condition | Duration | Action |
|-------|-----------|----------|--------|
| **HighChannelUtilization** | `streaminfa_channel_utilization > 0.8` | 30s | Pipeline backpressure. Check CPU, storage latency. |
| **TranscodeSlowerThanRealtime** | `streaminfa_transcode_fps < source_fps` (per stream) | 1 min | CPU bottleneck. Consider reducing concurrent streams. |
| **HighS3Latency** | `p99(streaminfa_storage_put_duration_seconds) > 1.0` | 2 min | Storage slowness. Check S3 health. |
| **HighDeliveryLatency** | `p99(streaminfa_delivery_request_duration_seconds) > 0.1` | 2 min | Cache misses or S3 slowness. Check cache hit ratio. |
| **LowCacheHitRatio** | `cache_hit_ratio < 0.5` | 5 min | Cache too small or access pattern changed. Consider increasing cache. |
| **DiskSpaceLow** | Disk free < 10 GB (from node_exporter) | 5 min | Clean up temp files or increase disk. |
| **HighIngestErrorRate** | `rate(streaminfa_ingest_errors_total[5m]) > 1` | 5 min | Encoders sending bad data or auth failures. Check logs. |
| **ConfigReloadFailed** | `increase(streaminfa_config_reload_total{result="failure"}[5m]) > 0` | 0 (instant) | Config file has errors. Fix and retry SIGHUP. |

### 7.3 Alert Routing

| Severity | Channel | Response Time |
|----------|---------|---------------|
| **Critical** | PagerDuty / on-call Slack channel | 5 minutes |
| **Warning** | Team Slack channel | 1 hour (business hours) |
| **Informational** | Dashboard only | Next business day |

---

## 8. Logging Best Practices for Engineers

### 8.1 What to Log

| Event | Level | Structured Fields |
|-------|-------|-------------------|
| Stream started | INFO | stream_id, protocol, remote_addr, video_codec, resolution |
| Stream ended | INFO | stream_id, duration_secs, segments_produced, total_bytes |
| Stream error | ERROR | stream_id, error_type, error_message |
| Segment produced | DEBUG | stream_id, rendition, sequence, duration_secs, size_bytes |
| S3 write | DEBUG | stream_id, path, size_bytes, latency_ms |
| S3 write retry | WARN | stream_id, path, attempt, error_message |
| S3 write failed (all retries) | ERROR | stream_id, path, error_message |
| Auth failure | WARN | protocol, remote_addr, reason |
| Config reload | INFO | result (success/failure), changed_keys |
| Graceful shutdown started | INFO | active_streams, reason (SIGTERM/SIGINT) |
| Graceful shutdown complete | INFO | drained_streams, duration_secs |

### 8.2 What NOT to Log

- Individual RTMP packets (TRACE only, never in production)
- Frame-level timestamps (TRACE only)
- Successful cache hits (too noisy; use metrics instead)
- Successful S3 reads for delivery (too noisy; use metrics instead)
- Secrets, tokens, keys (see redaction rules)

---

## 9. Correlation and Debugging

### 9.1 Request ID

Every HTTP request gets a unique request ID (UUIDv4), set in the `X-Request-Id` response header and included in all log lines for that request. If the incoming request has an `X-Request-Id` header, it is reused.

### 9.2 Stream ID as Correlation Key

The `stream_id` is the primary correlation key across all observability signals:
- Metrics: `stream_id` label on per-stream metrics (with lifecycle cleanup)
- Logs: `stream_id` structured field
- Traces: `stream_id` attribute on the root span

To debug a specific stream:
1. **Metrics:** Filter dashboards by `stream_id`.
2. **Logs:** Query `stream_id="01906b3a-..."` in log aggregator.
3. **Traces:** Search by `stream_id` attribute in Jaeger/Tempo.

### 9.3 Log-Metric-Trace Correlation in Grafana

Grafana supports linking between data sources:
- From a log line → click `stream_id` → jump to per-stream dashboard.
- From a metric alert → link to log query with the same `stream_id` and time range.
- From a trace → link to log lines within the trace's time range.

This requires configuring Grafana data source links, documented in the deployment guide.

---

## Definition of Done — Observability

- [x] Observability architecture diagram with all components
- [x] Complete metrics catalog with names, types, labels, and descriptions
- [x] Histogram bucket definitions with rationale
- [x] Label cardinality rules documented
- [x] Structured logging format (JSON + pretty) with examples
- [x] Log level guidelines
- [x] Sensitive field redaction rules
- [x] Distributed tracing span hierarchy
- [x] Trace context propagation across channels
- [x] Sampling strategy per environment
- [x] SLIs defined with measurement methods
- [x] SLOs defined with targets and windows
- [x] Error budget calculated
- [x] Three dashboard layouts with panel queries
- [x] Critical and warning alert rules with conditions and durations
- [x] Alert routing by severity
- [x] Logging best practices for engineers
- [x] Correlation strategy (request ID, stream ID)
