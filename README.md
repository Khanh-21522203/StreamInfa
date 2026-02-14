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
- Storage path format: `{stream_id}/{rendition}/{segment_number}.m4s`

### Delivery
- **Axum HTTP origin server** with full HLS delivery
- LRU object cache (512 MB default) with per-type TTLs
- HTTP Range request support for seeking
- Cache-Control headers (no-cache for live, 24h for VOD)
- CORS configured for cross-origin playback

### Control Plane REST API
- Stream lifecycle management (create, list, get, delete)
- Bearer token authentication for admin endpoints
- Stream state machine: `Pending → Live → Processing → Ready` (with `Error` and `Deleted`)
- Concurrent state management via DashMap

### Observability
- Prometheus metrics at `/metrics` (ingest, transcode, packaging, storage, delivery, system)
- Structured JSON logging via `tracing`
- Health endpoints: `/healthz` (liveness), `/readyz` (readiness with FFmpeg check)

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

- **Rust** 1.77+ (2021 edition)
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
max_concurrent_live_streams = 5
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

[auth]
ingest_stream_keys = []
admin_bearer_tokens = []

[cache]
enabled = true
max_size_bytes = 536870912
ttl_secs = 300
segment_ttl_secs = 3600

[security]
max_json_body_bytes = 1048576
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
curl -X POST http://localhost:8080/api/v1/upload \
  -H "Authorization: Bearer <admin_token>" \
  -F "file=@video.mp4" \
  -F "stream_id=<stream_id>"
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

#### Health Checks

```bash
curl http://localhost:8080/healthz    # Liveness
curl http://localhost:8080/readyz     # Readiness
curl http://localhost:8080/metrics    # Prometheus metrics
```

## Tests

```bash
# Run all tests
cargo test

# Run tests with output
cargo test -- --nocapture

# Run specific module tests
cargo test package::manifest
cargo test transcode::profile
cargo test storage::memory
```

## Stress Testing

StreamInfa includes stress test scenarios in `tests/stress/` for benchmarking key components under load. These tests use Tokio's async runtime and measure throughput, latency, and correctness under concurrent access.

### Running Stress Tests

```bash
# Run all stress tests (release mode recommended for realistic numbers)
cargo test --release --test stress -- --nocapture

# Run a specific scenario
cargo test --release --test stress concurrent_stream_creation -- --nocapture
cargo test --release --test stress storage_write_throughput -- --nocapture
cargo test --release --test stress hls_manifest_generation -- --nocapture
cargo test --release --test stress state_manager_contention -- --nocapture
cargo test --release --test stress segment_index_window -- --nocapture
```

### Stress Test Scenarios

| Scenario | What it measures | Parameters |
|----------|-----------------|------------|
| `concurrent_stream_creation` | Control plane throughput for creating streams | 100 concurrent streams |
| `storage_write_throughput` | In-memory storage write throughput (segments/sec) | 1,000 segments × 200 KB |
| `hls_manifest_generation` | Playlist generation throughput under load | 10,000 playlists with 30 segments each |
| `state_manager_contention` | DashMap read/write contention with mixed workload | 50 writers + 200 readers |
| `segment_index_window` | Sliding window performance with eviction | 100,000 segments through a 5-segment window |
| `concurrent_storage_reads` | Read throughput with concurrent readers | 100 readers × 1,000 reads each |
| `pipeline_event_throughput` | Event channel throughput (events/sec) | 50,000 events |

### Interpreting Results

Each test prints:
- **Total time** — wall-clock duration
- **Throughput** — operations per second
- **Per-operation latency** — average time per operation

Example output:
```
[stress::storage_write_throughput] 1000 segments written in 12.3ms (81,300 segments/sec, 1.23μs/op)
[stress::state_manager_contention] 50 writers + 200 readers completed in 45.2ms (0 conflicts)
```
