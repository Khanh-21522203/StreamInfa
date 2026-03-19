# StreamInfa

A backend media infrastructure platform built in Rust for live and VOD video streaming. StreamInfa handles the full pipeline from ingest to delivery: **RTMP ingest → transcoding → HLS fMP4 packaging → S3-compatible storage → HTTP origin delivery**.

## Features

### Ingest
- **RTMP live ingest** on port 1935 (configurable) with stream key authentication
- **HTTP VOD upload** with file validation (codec, container, duration, resolution)
- Per-stream pipeline isolation — each connection gets its own transcode → package → storage pipeline

### Transcoding
- **3-rung H.264 adaptive bitrate ladder** (no upscaling):
  | Rendition | Resolution | Video Bitrate | Audio | Profile | Level |
  |-----------|-----------|---------------|-------|---------|-------|
  | high      | 1920×1080 | 3,500 kbps    | 128k AAC | High | 4.1 |
  | medium    | 1280×720  | 2,000 kbps    | 128k AAC | Main | 3.1 |
  | low       | 854×480   | 1,000 kbps    | 96k AAC  | Main | 3.0 |
- Automatic rendition selection based on source resolution (skip rungs above source)
- Falls back to source resolution with lowest-rung settings if input < 480p

### Packaging
- **HLS v7 with fMP4** (fragmented MP4) segments
- 6-second target segment duration, 2-second keyframe interval
- Multivariant (master) playlist with `#EXT-X-INDEPENDENT-SEGMENTS`
- Live sliding window (5 segments) with automatic eviction
- VOD playlists with `#EXT-X-PLAYLIST-TYPE:VOD` and `#EXT-X-ENDLIST`

### Storage
- **S3-compatible object storage** — works with both AWS S3 and MinIO (same `aws-sdk-s3` SDK)
- In-memory store for development/testing (default)
- Retry logic with exponential backoff (3 retries, 100ms initial backoff)
- Path-style addressing support for MinIO
- Automatic segment cleanup for expired live segments and error-state streams
- Storage path format: `{stream_id}/{rendition}/{sequence:06}.m4s`

### Delivery
- **Axum HTTP origin server** with full HLS delivery
- LRU object cache (512 MB default) with per-type TTLs
- HTTP Range request support for seeking
- Cache-Control headers (no-cache for live, 24h for VOD)
- CORS configured for cross-origin playback

### Control Plane REST API
- Stream lifecycle management (create, list, get, delete, rotate key)
- Bearer token authentication for admin endpoints
- Stream state machine: `Pending → Live → Processing → Ready` (with `Error` and `Deleted`)
- Concurrent state management via DashMap

### Observability
- Prometheus metrics at `/metrics` (ingest, transcode, packaging, storage, delivery, system)
- Structured JSON logging via `tracing`
- Health endpoints:
  - `/healthz` (liveness)
  - `/readyz` (readiness checks: storage, ffmpeg, RTMP listener, disk space)

### Security
- Stream key authentication for RTMP ingest
- Bearer token authentication for admin API
- Brute-force protection (5 attempts / 60s window, 5-minute block)
- Input validation (max body size, RTMP chunk limits, metadata validation)
- Constant-time token comparison (timing attack prevention)

## Architecture

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                          StreamInfa Binary                                   │
│                                                                              │
│  ┌─────────────┐    ┌──────────────┐    ┌────────────┐    ┌──────────────┐  │
│  │   Ingest    │    │  Transcode   │    │  Packager  │    │   Storage    │  │
│  │   Service   │───►│  Pipeline    │───►│   (HLS)    │───►│   Client     │  │
│  │             │    │              │    │            │    │  (S3/MinIO)  │  │
│  │  ┌────────┐ │    │  ┌─────────┐ │    │            │    │              │  │
│  │  │ RTMP   │ │    │  │ FFmpeg  │ │    │            │    │              │  │
│  │  │ Server │ │    │  │  FFI    │ │    │            │    │              │  │
│  │  ├────────┤ │    │  └─────────┘ │    │            │    │              │  │
│  │  │ HTTP   │ │    │              │    │            │    │              │  │
│  │  │ Upload │ │    │              │    │            │    │              │  │
│  │  └────────┘ │    │              │    │            │    │              │  │
│  └─────────────┘    └──────────────┘    └────────────┘    └──────┬───────┘  │
│         │                                                        │          │
│         │              ┌──────────────┐                          │          │
│         │              │   Control    │                          │          │
│         └─────────────►│    Plane     │◄─────────────────────────┘          │
│                        │   (REST)     │                                     │
│                        └──────────────┘                                     │
│                               │                                             │
│                        ┌──────┴───────┐                                     │
│                        │   Delivery   │                                     │
│                        │   (Origin)   │──────────────────────────────► CDN  │
│                        └──────────────┘                                     │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────────────┐   │
│  │                        Common / Core                                 │   │
│  │   config │ error │ metrics │ tracing │ types │ health │ auth         │   │
│  └──────────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Module Structure

```
src/
├── main.rs              # Entry point, component wiring, graceful shutdown
├── core/
│   ├── config.rs        # Layered TOML + env var configuration
│   ├── types.rs         # Core data types (StreamId, DemuxedFrame, EncodedSegment, etc.)
│   ├── error.rs         # Per-module error types (thiserror)
│   ├── security.rs      # Security constants and validation
│   ├── shutdown.rs      # Graceful shutdown coordinator (CancellationToken)
│   └── auth.rs          # Authentication provider
├── ingest/
│   ├── rtmp.rs          # RTMP server with per-stream pipeline creation
│   ├── http_upload.rs   # VOD file upload handler
│   ├── demuxer.rs       # FLV/MP4 demuxing
│   └── validator.rs     # Upload validation (codec, resolution, duration)
├── transcode/
│   ├── pipeline.rs      # Transcode orchestrator (fan-out to renditions)
│   ├── profile.rs       # Rendition selection, codec string generation
│   └── segment.rs       # Segment accumulator (keyframe-aligned splitting)
├── package/
│   ├── hls.rs           # fMP4 box generation (init.mp4, .m4s segments)
│   ├── manifest.rs      # HLS playlist generation + storage path helpers
│   ├── runner.rs        # Packager task (segment → fMP4 → storage)
│   └── segment_index.rs # Per-rendition segment window management
├── storage/
│   ├── mod.rs           # MediaStore trait (abstraction)
│   ├── memory.rs        # In-memory store (dev/test)
│   ├── s3.rs            # S3-compatible store (AWS S3 / MinIO)
│   ├── writer.rs        # Storage writer task
│   ├── cache.rs         # LRU object cache
│   └── cleanup.rs       # Background segment cleanup
├── delivery/
│   ├── router.rs        # Axum router setup
│   ├── handlers.rs      # HTTP handlers (API + delivery)
│   └── middleware.rs     # Request ID middleware
├── control/
│   ├── state.rs         # StreamStateManager (DashMap-based)
│   ├── events.rs        # Pipeline event handler
│   └── pipeline.rs      # Per-stream pipeline channel wiring
└── observability/
    └── metrics.rs       # Prometheus metric definitions
```

## Why `aws-sdk-s3` Instead of a MinIO SDK?

MinIO implements the **S3 API** — it's fully S3-compatible by design. The `aws-sdk-s3` crate works with any S3-compatible store (AWS S3, MinIO, DigitalOcean Spaces, Backblaze B2, etc.) by simply changing the `endpoint` and enabling `path_style` addressing. There's no need for a separate MinIO SDK.

The `S3MediaStore` in `src/storage/s3.rs` already supports:
- Custom endpoint URL (e.g., `http://localhost:9000` for MinIO)
- Path-style addressing (`path_style: true`, required by MinIO)
- Configurable region, credentials, and timeouts

## Getting Started

### Prerequisites

- **Rust** 1.82+ (2021 edition)
- **MinIO** or any S3-compatible storage (optional — in-memory store used by default)
- **FFmpeg** (optional — required for production transcoding, enable with `--features ffmpeg`)

### Build

```bash
# Default build (in-memory storage, placeholder transcoding)
cargo build --release

# With S3 storage support
cargo build --release --features s3

# With FFmpeg transcoding
cargo build --release --features ffmpeg

# Full production build
cargo build --release --features s3,ffmpeg
```

### Configuration

StreamInfa uses layered configuration:

1. **`config/default.toml`** — base configuration
2. **`config/{env}.toml`** — environment-specific overrides (set `STREAMINFA_ENV`)
3. **Environment variables** — highest priority overrides

#### Example `config/default.toml`

```toml
[server]
host = "0.0.0.0"
control_port = 8080
rtmp_port = 1935

[ingest]
max_bitrate_kbps = 20000
max_upload_size_bytes = 10737418240
allowed_codecs = ["h264"]
allowed_audio_codecs = ["aac", "mp3"]
max_duration_secs = 21600.0
upload_timeout_secs = 60
max_concurrent_rtmp_connections = 50
max_concurrent_live_streams = 10
max_concurrent_uploads = 10
max_pending_connections = 100

[transcode]
keyframe_interval_secs = 2
max_blocking_threads = 64

[[transcode.profile_ladder]]
name = "high"
width = 1920
height = 1080
bitrate_kbps = 3500
audio_bitrate_kbps = 128
profile = "high"
level = "4.1"
preset = "medium"

[[transcode.profile_ladder]]
name = "medium"
width = 1280
height = 720
bitrate_kbps = 2000
audio_bitrate_kbps = 128
profile = "main"
level = "3.1"
preset = "medium"

[[transcode.profile_ladder]]
name = "low"
width = 854
height = 480
bitrate_kbps = 1000
audio_bitrate_kbps = 96
profile = "main"
level = "3.0"
preset = "medium"

[packaging]
segment_duration_secs = 6
live_window_segments = 5
hls_version = 7
segment_retention_minutes = 60

[storage]
backend = "s3"
endpoint = "http://localhost:9000"
bucket = "streaminfa-media"
access_key_id = ""
secret_access_key = ""
region = "us-east-1"
path_style = true
max_concurrent_requests = 50
request_timeout_secs = 30

[delivery]
cache_control_live = "no-cache, no-store"
cache_control_vod = "public, max-age=86400"
cors_allowed_origins = ["*"]

[observability]
log_level = "info"
log_format = "json"
metrics_enabled = true
tracing_enabled = false
tracing_endpoint = "http://localhost:4317"
tracing_sample_rate = 0.1

[auth]
ingest_stream_keys = []
admin_bearer_tokens = []

[cache]
enabled = true
max_size_bytes = 536870912
ttl_secs = 300
segment_ttl_secs = 3600

[security]
max_rtmp_chunk_size = 65536
max_amf_string_length = 4096
max_flv_tag_size_bytes = 16777216
max_json_body_bytes = 1048576
brute_force_max_attempts = 5
brute_force_window_secs = 60
brute_force_block_secs = 300
```

#### Environment Variable Overrides

| Variable | Description |
|----------|-------------|
| `STREAMINFA_ENV` | Config environment (`development`, `production`) |
| `STREAMINFA_SERVER_HOST` | Bind address |
| `STREAMINFA_SERVER_CONTROL_PORT` | HTTP API port |
| `STREAMINFA_SERVER_RTMP_PORT` | RTMP ingest port |
| `STREAMINFA_STORAGE_ENDPOINT` | S3/MinIO endpoint URL |
| `STREAMINFA_STORAGE_BUCKET` | Storage bucket name |
| `STREAMINFA_STORAGE_ACCESS_KEY_ID` | S3 access key |
| `STREAMINFA_STORAGE_SECRET_ACCESS_KEY` | S3 secret key |
| `STREAMINFA_STORAGE_REGION` | S3 region |
| `STREAMINFA_AUTH_INGEST_STREAM_KEYS` | Comma-separated stream keys |
| `STREAMINFA_AUTH_ADMIN_BEARER_TOKENS` | Comma-separated admin tokens |
| `STREAMINFA_OBSERVABILITY_LOG_LEVEL` | Log level (trace/debug/info/warn/error) |

### Run

```bash
# Run with default config
cargo run --release

# Run with MinIO storage
STREAMINFA_STORAGE_ENDPOINT=http://localhost:9000 \
STREAMINFA_STORAGE_ACCESS_KEY_ID=minioadmin \
STREAMINFA_STORAGE_SECRET_ACCESS_KEY=minioadmin \
STREAMINFA_STORAGE_BUCKET=streaminfa-media \
cargo run --release --features s3
```

### Using MinIO

```bash
# Start MinIO with Docker
docker run -d --name minio \
  -p 9000:9000 -p 9001:9001 \
  -e MINIO_ROOT_USER=minioadmin \
  -e MINIO_ROOT_PASSWORD=minioadmin \
  minio/minio server /data --console-address ":9001"

# Create the bucket
docker exec minio mc alias set local http://localhost:9000 minioadmin minioadmin
docker exec minio mc mb local/streaminfa-media
```

### API Usage

#### Create a Live Stream

```bash
curl -X POST http://localhost:8080/api/v1/streams \
  -H "Authorization: Bearer <admin_token>" \
  -H "Content-Type: application/json" \
  -d '{"ingest_type": "rtmp"}'
```

Response:
```json
{
  "stream_id": "01234567-89ab-cdef-0123-456789abcdef",
  "status": "pending",
  "stream_key": "sk_abc123...",
  "rtmp_url": "rtmp://0.0.0.0:1935/live/sk_abc123...",
  "created_at": "2025-01-01T00:00:00Z"
}
```

#### Upload VOD Content

```bash
curl -X POST http://localhost:8080/api/v1/streams/upload \
  -H "Authorization: Bearer <admin_token>" \
  -F "file=@video.mp4"
```

#### Play HLS Stream

```
http://localhost:8080/streams/<stream_id>/master.m3u8
```

#### List Streams

```bash
curl http://localhost:8080/api/v1/streams \
  -H "Authorization: Bearer <admin_token>"
```

#### Delete a Stream

```bash
curl -X DELETE http://localhost:8080/api/v1/streams/<stream_id> \
  -H "Authorization: Bearer <admin_token>"
```

#### Rotate Stream Key

```bash
curl -X POST http://localhost:8080/api/v1/streams/<stream_id>/rotate-key \
  -H "Authorization: Bearer <admin_token>"
```

#### Health Checks

```bash
curl http://localhost:8080/healthz    # Liveness
curl http://localhost:8080/readyz     # Readiness
curl http://localhost:8080/metrics    # Prometheus metrics
```

## Tests

```bash
# Run all tests
cargo test --all -- --nocapture

# Run integration API tests
cargo test --test integration_control_plane -- --nocapture

# Run in-process e2e playback flow
cargo test --test e2e_playback_flow -- --nocapture
```

## Benchmarking

Run the benchmark harness:

```bash
# Control-plane benchmark (list + create/delete)
ADMIN_TOKEN=<admin_token> ./scripts/benchmark.sh control

# Ingest benchmark (VOD upload + ready latency)
ADMIN_TOKEN=<admin_token> ./scripts/benchmark.sh ingest

# Delivery benchmark (auto-prepares stream when DELIVERY_STREAM_ID is omitted)
DELIVERY_STREAM_ID=<uuidv7> ./scripts/benchmark.sh delivery

# Soak/endurance benchmark (mixed delivery + control)
ADMIN_TOKEN=<admin_token> SOAK_DURATION=30m ./scripts/benchmark.sh soak

# Overload/backpressure benchmark
ADMIN_TOKEN=<admin_token> OVERLOAD_VUS=120 OVERLOAD_DURATION=120s ./scripts/benchmark.sh overload

# Live RTMP end-to-end benchmark (requires ffmpeg)
ADMIN_TOKEN=<admin_token> LIVE_RTMP_PUBLISH_SECS=45 ./scripts/benchmark.sh live-rtmp

# Fault-injection benchmark (example: storage outage and recovery)
ADMIN_TOKEN=<admin_token> FAULT_INJECT_CMD='docker compose stop minio' FAULT_RECOVER_CMD='docker compose start minio' ./scripts/benchmark.sh fault

# Capacity search benchmark (max passing VUs before threshold breach)
ADMIN_TOKEN=<admin_token> CAPACITY_START_VUS=10 CAPACITY_STEP_VUS=10 CAPACITY_MAX_VUS=200 ./scripts/benchmark.sh capacity

# Operational-path benchmark (startup/readiness/reload/shutdown timings)
OPS_START_CMD='cargo run --release' OPS_RELOAD_CMD='pkill -HUP streaminfa' ./scripts/benchmark.sh ops
# If readiness is intentionally degraded, allow probe-only mode:
OPS_REQUIRE_READY=0 ./scripts/benchmark.sh ops

# Pipeline microbenchmarks (criterion)
./scripts/benchmark.sh micro

# Baseline suite (control + ingest + delivery)
ADMIN_TOKEN=<admin_token> ./scripts/benchmark.sh all

# Extended suite (all + soak + overload + capacity + ops + micro)
ADMIN_TOKEN=<admin_token> ./scripts/benchmark.sh all-plus
```

Artifacts are written to:
1. Default directory: `.tmp/benchmarks/<YYYYMMDD-HHMMSS>` (override with `BENCH_OUT_DIR`).
2. Per-scenario k6 summary: `<scenario>-<timestamp>-k6-summary.json`.
3. Optional metrics snapshots (`METRICS_SNAPSHOT=1`): `...-metrics-before.prom` and `...-metrics-after.prom`.
4. Optional metrics delta (when both snapshots exist): `...-metrics-delta.txt`.
5. Helper summaries for non-k6 workflows (`live-rtmp`, `capacity`, `ops`, `micro`).

Detailed guide: `docs/testing/benchmarking.md`

Tip (Docker k6 against local service):
1. Use `BASE_URL=http://host.docker.internal:8080`.
2. Keep metrics scrape URL on host side: `METRICS_URL=http://localhost:8080/metrics`.
3. For shell-side API calls in helper scenarios, keep `CONTROL_BASE_URL=http://localhost:8080`.

Benchmark types covered:
1. Baseline: control, ingest, delivery.
2. Reliability: soak, overload/backpressure, fault-injection.
3. Capacity: progressive VU search.
4. Live pipeline: RTMP publish to first segment.
5. Ops path: startup/readiness/reload/shutdown.
6. Microbench: manifest/init/segment-index hot paths.

### Control Benchmark Snapshot (Latest: 2026-03-19 09:00, Post-Optimization)

Environment:
1. StreamInfa `release` build (`cargo build --release`)
2. Benchmark runner: Docker `grafana/k6:0.50.0`
3. Parameters: `DURATION=8s`, `VUS=10`, `CONTROL_CREATE_VUS=2`, `METRICS_SNAPSHOT=0`
4. Target: `BASE_URL=http://host.docker.internal:8080`

k6 results:

| Case                                        | avg_ms  | p50    | p95    | min   | max     | qps    |
|---------------------------------------------|--------:|-------:|-------:|------:|--------:|-------:|
| `control.list_streams_http_duration`        |  21.708 |  5.949 | 77.715 | 1.110 | 175.542 | 79.741 |
| `control.create_delete_http_duration`       |  53.080 | 67.306 | 140.565| 1.374 | 290.090 |  9.475 |
| `control.create_delete_e2e_duration_ms`     | 108.961 | 89.000 | 199.600|68.000 | 293.000 |  9.475 |

Notes:
1. This snapshot is for local single-node benchmarking only; treat it as a baseline, not an SLO.
2. All control scenario checks passed in this run (`879` passes, `0` fails).

### Ingest Benchmark Snapshot (Latest: 2026-03-19 08:37)

Environment:
1. StreamInfa `release` build (`cargo build --release`)
2. Benchmark runner: Docker `grafana/k6:0.50.0`
3. Parameters: `DURATION=6s`, `INGEST_UPLOAD_VUS=2`, `UPLOAD_SAMPLE_SIZE_BYTES=262144`
4. Target: `BASE_URL=http://host.docker.internal:8080`

k6 results:

| Case                                   | avg_ms  | p50     | p95     | min     | max     | qps     |
|----------------------------------------|--------:|--------:|--------:|--------:|--------:|--------:|
| `ingest.http_req_duration`             | 129.919 | 127.214 | 185.228 |  86.121 | 208.064 |  12.088 |
| `ingest.upload_request_duration_ms`    | 108.726 |  97.797 | 181.698 |  86.121 | 208.064 |   4.029 |
| `ingest.upload_to_ready_duration_ms`   | 240.077 | 224.000 | 318.000 | 198.000 | 332.000 |   4.029 |

Prometheus delta (captured from the latest ingest run with metrics snapshots):

| Metric | Delta |
|---|---|
| `upload_count` | `15` |
| `upload_avg_duration_seconds` | `0.115` |
| `upload_avg_size_bytes` | `262144` |
| `package_segments_written_total` | `45` |
| `package_manifest_updates_total` | `45` |
| `storage_put_bytes_total` | `11859480` |
| `storage_errors_total` | `0` |
| `backpressure_events_total` | `0` |
| `auth_failures_total` | `0` |

### Delivery Benchmark Snapshot (Latest: 2026-03-19 09:20)

Environment:
1. StreamInfa `release` build (`cargo build --release`)
2. Benchmark runner: Docker `grafana/k6:0.50.0`
3. Parameters: `DURATION=8s`, `VUS=10`, `DELIVERY_USE_RANGE=1`, `AUTO_PREPARE_DELIVERY=0`
4. Target: `BASE_URL=http://host.docker.internal:8080`

k6 results:

| Case                                | avg_ms  | p50     | p95     | min     | max     | qps      |
|-------------------------------------|--------:|--------:|--------:|--------:|--------:|---------:|
| `delivery.http_req_duration`        |   4.811 |   2.662 |  15.382 |   0.931 |  50.766 |  323.764 |
| `delivery.iteration_duration`       | 122.461 | 117.456 | 145.668 | 107.712 | 276.193 |   80.941 |
| `delivery.http_req_waiting`         |   3.046 |   2.078 |   8.022 |   0.848 |  35.941 |  323.764 |

## Quality and Ops Workflows

1. CI gates: `.github/workflows/ci.yml`
2. Periodic/manual reliability smoke: `.github/workflows/reliability-smoke.yml`
3. Deploy helper: `scripts/deploy.sh`
4. Post-deploy verifier: `scripts/post_deploy_verify.sh`
5. Rollback helper: `scripts/rollback.sh`
6. Release readiness checklist: `docs/devops/release-readiness.md`
