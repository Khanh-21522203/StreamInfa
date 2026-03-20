# StreamInfa

StreamInfa is a Rust backend for live and VOD video streaming. It handles the full pipeline from ingest to delivery: RTMP ingest → FFmpeg transcoding → HLS fMP4 packaging → S3-compatible storage → HTTP origin delivery.

## What Is Implemented

- Ingest:
  - RTMP live ingest on port 1935 with stream key authentication
  - HTTP VOD file upload with codec, container, duration, and resolution validation
  - Per-stream pipeline isolation (each connection gets its own transcode → package → store pipeline)
- Transcoding:
  - 3-rung H.264 ABR ladder (high 1080p/3500k, medium 720p/2000k, low 480p/1000k)
  - Automatic rung selection based on source resolution (no upscaling)
  - FFmpeg FFI via `ffmpeg-sys-next` (compile-time feature flag)
- Packaging:
  - HLS v7 with fMP4 segments (6s target duration, 2s keyframe interval)
  - Multivariant (master) playlist with `#EXT-X-INDEPENDENT-SEGMENTS`
  - Live sliding window (5 segments) with automatic eviction
  - VOD playlists with `#EXT-X-PLAYLIST-TYPE:VOD` and `#EXT-X-ENDLIST`
- Storage:
  - S3-compatible backend via `aws-sdk-s3` (AWS S3, MinIO, DigitalOcean Spaces, etc.)
  - In-memory store for development and testing (default, no feature flag needed)
  - Retry with exponential backoff (3 retries, 100ms initial)
  - Background cleanup for expired live segments and error-state streams
- Delivery:
  - Axum HTTP origin server with full HLS delivery
  - LRU object cache (512 MB default) with per-type TTLs
  - HTTP Range request support
  - Cache-Control headers (no-cache for live, 24h for VOD)
- Control plane REST API:
  - Stream lifecycle: create, list, get, delete, rotate key
  - Bearer token authentication for all admin endpoints
  - State machine: `Pending → Live → Processing → Ready` (+ `Error`, `Deleted`)
- Observability:
  - Prometheus metrics at `/metrics` (ingest, transcode, packaging, storage, delivery, system)
  - Structured JSON logging via `tracing`
  - `/healthz` (liveness) and `/readyz` (readiness: storage, FFmpeg, RTMP, disk)
- Security:
  - Stream key auth for RTMP, bearer token auth for admin API
  - Brute-force protection (5 attempts / 60s window, 5-minute block)
  - Constant-time token comparison

## Architecture At A Glance

Request flow:

`RTMP/HTTP upload → Ingest → Transcode pipeline (FFmpeg) → Packager (fMP4/HLS) → Storage writer (S3) → ready`

Delivery flow:

`Client → Axum HTTP handler → LRU cache → S3 GetObject (on miss) → response`

Control flow:

`REST API → StreamStateManager (DashMap) → pipeline event channel → state transitions`

## Build

Requirements:

- Rust 1.91+
- FFmpeg dev libraries (for `--features ffmpeg`)

Commands:

```bash
# Default build (in-memory storage, placeholder transcoding)
cargo build --release

# Full production build (S3 storage + FFmpeg transcoding)
cargo build --release --features s3,ffmpeg
```

Docker (full stack with MinIO, Prometheus, Grafana):

```bash
docker compose -f docker/docker-compose.yml up -d
```

## Run

With default config (in-memory storage):

```bash
cargo run --release
```

With MinIO storage:

```bash
STREAMINFA_STORAGE_ENDPOINT=http://localhost:9000 \
STREAMINFA_STORAGE_ACCESS_KEY_ID=minioadmin \
STREAMINFA_STORAGE_SECRET_ACCESS_KEY=minioadmin \
STREAMINFA_STORAGE_BUCKET=streaminfa-media \
cargo run --release --features s3
```

Key environment variables:

- `STREAMINFA_SERVER_CONTROL_PORT` — HTTP port (default: 8080)
- `STREAMINFA_SERVER_RTMP_PORT` — RTMP port (default: 1935)
- `STREAMINFA_STORAGE_ENDPOINT` — S3/MinIO endpoint URL
- `STREAMINFA_STORAGE_BUCKET` — bucket name
- `STREAMINFA_AUTH_ADMIN_BEARER_TOKENS` — comma-separated admin tokens
- `STREAMINFA_AUTH_INGEST_STREAM_KEYS` — comma-separated stream keys
- `STREAMINFA_OBSERVABILITY_LOG_LEVEL` — `trace`, `debug`, `info`, `warn`, `error`
- `STREAMINFA_OBSERVABILITY_LOG_FORMAT` — `json` (structured) or any other value (plain text)

Full config reference: `config/default.toml`.

## API

Create a live stream:

```bash
curl -X POST http://localhost:8080/api/v1/streams \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{"ingest_type": "rtmp"}'
```

Upload VOD content:

```bash
curl -X POST http://localhost:8080/api/v1/streams/upload \
  -H "Authorization: Bearer <token>" \
  -F "file=@video.mp4"
```

Play HLS stream:

```
http://localhost:8080/streams/<stream_id>/master.m3u8
```

Other endpoints:

```bash
curl http://localhost:8080/api/v1/streams                          # list
curl -X DELETE http://localhost:8080/api/v1/streams/<id>           # delete
curl -X POST http://localhost:8080/api/v1/streams/<id>/rotate-key  # rotate key
curl http://localhost:8080/healthz                                 # liveness
curl http://localhost:8080/readyz                                  # readiness
curl http://localhost:8080/metrics                                 # Prometheus
```

## Test

```bash
# All tests
cargo test --all

# Integration tests (control plane API)
cargo test --test integration_control_plane

# End-to-end playback flow
cargo test --test e2e_playback_flow
```

## Benchmark

Run the benchmark harness (requires `docker` for k6):

```bash
# Individual scenarios
ADMIN_TOKEN=<token> BASE_URL=http://localhost:8080 ./scripts/benchmark.sh control
ADMIN_TOKEN=<token> BASE_URL=http://localhost:8080 ./scripts/benchmark.sh ingest
ADMIN_TOKEN=<token> BASE_URL=http://localhost:8080 ./scripts/benchmark.sh delivery
ADMIN_TOKEN=<token> BASE_URL=http://localhost:8080 ./scripts/benchmark.sh soak
ADMIN_TOKEN=<token> BASE_URL=http://localhost:8080 ./scripts/benchmark.sh overload

# Capacity search (steps 20→200 VUs)
ADMIN_TOKEN=<token> BASE_URL=http://localhost:8080 ./scripts/benchmark-capacity.sh

# Ops path (startup/readiness/reload/shutdown timings)
./scripts/benchmark-ops-path.sh

# Pipeline microbenchmarks (Criterion)
./scripts/benchmark-micro.sh

# Baseline suite (control + ingest + delivery)
ADMIN_TOKEN=<token> ./scripts/benchmark.sh all
```

Artifacts written to `.tmp/benchmarks/<YYYYMMDD-HHMMSS>/`. Detailed guide: `docs/testing/benchmarking.md`.

Terms:

- **VUs (Virtual Users)** — concurrent simulated clients sending requests in a tight loop. 20 VUs ≈ 20 parallel connections hammering the server simultaneously.
- **Capacity search** — the benchmark steps up VUs from a starting point (e.g. 20, 40, 60 …) and checks whether p95 latency stays under a threshold (400ms). The highest VU count that passes is the reported capacity.
- **Manifest generation** — building the HLS playlist text file (`.m3u8`) that tells the player which segments to download and in what order. Done on every new segment for live streams.
- **Segment window** — for live streams, only the last N segments are kept in the playlist. The window slides forward as new segments arrive; old ones are evicted from storage.
- **Segment index** — the in-memory data structure that tracks which segments exist for a stream. Adding a segment updates the index and triggers a playlist regeneration.
- **p50 / p95** — percentiles. p50 (median) is the midpoint latency; p95 means 95% of requests finished faster than this value. p95 is the primary indicator for tail latency and SLO health.

Snapshot (2026-03-20, docker-compose + MinIO, `--features s3,ffmpeg`, k6 0.50.0):

**Control** (20 list + 5 create/delete VUs, 30s):

| Case | Description | avg_ms | p50 | p95 | qps |
|------|-------------|-------:|----:|----:|----:|
| `list_streams` | GET /api/v1/streams — list all streams | 10.17 | 0.523 | 45.39 | — |
| `create_delete_stream` | POST + DELETE /api/v1/streams — full create/delete round trip | 27.15 | 45.54 | 53.14 | — |
| overall | all control requests combined | 14.63 | 0.648 | 48.01 | 244.9 |

**Ingest** (2 VUs, 30s, 2.6 MB fixture — 10s 1280x720 H.264, real FFmpeg transcode, 2 renditions, veryfast preset):

Each VU uploads a file, polls until the stream reaches READY, then deletes it. The qps column is **upload cycles/s**, not raw HTTP requests/s. `upload_to_ready` reflects real H.264 decode + libx264 encode to 720p and 480p renditions + fMP4 packaging + S3 write — approximately 300× realtime for a 10-second clip.

| Case | Description | avg_ms | p50 | p95 | cycles/s |
|------|-------------|-------:|----:|----:|---------:|
| `upload_request_duration_ms` | Time for the POST /api/v1/streams/upload HTTP response only — server accepted the file but pipeline has not started yet | 4.72 | 4.21 | 8.37 | 0.6 |
| `upload_to_ready_duration_ms` | Time from upload accepted → stream state reaches READY (full pipeline: FFmpeg transcode + package + S3 write) | 3,015 | 3,015 | 3,018 | 0.6 |

**Delivery** (20 VUs, 30s):

| Case | Description | avg_ms | p50 | p95 | qps |
|------|-------------|-------:|----:|----:|----:|
| `http_req_duration` | Each iteration fetches master playlist + media playlist + init segment + one segment | 1.11 | 0.285 | 4.30 | 758.9 |

**Soak** (20 delivery + 2 control VUs, 30s):

| Case | Description | avg_ms | p50 | p95 | qps |
|------|-------------|-------:|----:|----:|----:|
| `http_req_duration` | Mixed delivery reads and control list/create/delete under sustained load | 1.13 | 0.286 | 4.33 | 764.7 |

**Overload** (80 delivery + 8 control churn VUs, 2m):

| Case | Description | avg_ms | p50 | p95 | qps |
|------|-------------|-------:|----:|----:|----:|
| `http_req_duration` | Delivery reads at extreme concurrency; control churn continuously creates/deletes streams | 11.53 | 5.41 | 38.77 | 7,545.8 |

**Capacity search** (p95 threshold = 400ms, delivery read path):

Read-only workload: each VU repeatedly fetches master playlist + media playlist + init segment + one media segment (no uploads, no transcoding). Steps VUs from 10 → 200 in increments of 10, each step runs 20s. A step passes if p95 latency stays under 400ms and error rate is 0%. All steps passed — the server scales linearly and has not hit saturation at 200 VUs.

| VUs | avg_ms | p95_ms | req/s |
|----:|-------:|-------:|------:|
| 10 | 1.19 | 4.65 | 718.9 |
| 20 | 1.28 | 5.04 | 1,428.4 |
| 30 | 1.23 | 4.80 | 2,151.6 |
| 40 | 1.23 | 4.68 | 2,871.9 |
| 50 | 1.25 | 4.84 | 3,587.2 |
| 60 | 1.28 | 4.90 | 4,293.9 |
| 70 | 1.31 | 5.02 | 4,995.4 |
| 80 | 1.38 | 5.22 | 5,681.0 |
| 90 | 1.43 | 5.40 | 6,367.7 |
| 100 | 1.55 | 6.00 | 7,020.8 |
| 110 | 1.73 | 6.69 | 7,620.4 |
| 120 | 2.46 | 9.72 | 7,898.2 |
| 130 | 2.86 | 11.36 | 8,323.9 |
| 140 | 4.06 | 15.89 | 8,298.7 |
| 150 | 5.85 | 22.88 | 8,014.3 |
| 160 | 6.86 | 26.63 | 8,100.1 |
| 170 | 7.94 | 30.42 | 8,129.0 |
| 180 | 9.63 | 36.65 | 7,960.2 |
| 190 | 11.66 | 44.29 | 7,684.7 |
| 200 | 13.00 | 48.73 | 7,663.7 |

Max passing VUs: 200. No failures found up to `CAPACITY_MAX_VUS=200`. Throughput peaks around 120–140 VUs (~8,300 req/s) and p95 starts rising above 10ms beyond 130 VUs as the LRU cache is warmed across segments.

**Ops path** (single node, memory backend):

| Metric | Value |
|--------|------:|
| `/healthz` probe avg | 9 ms |
| `/readyz` probe avg | 76 ms |

**Pipeline microbenchmarks** (Criterion, release, `--features s3,ffmpeg`):

| Benchmark | Description | time (µs) |
|-----------|-------------|----------:|
| `manifest_generation/master_playlist_3_renditions` | Generate HLS multivariant playlist for 3 renditions | 1.023 |
| `manifest_generation/media_playlist_live/6` | Generate live media playlist with 6-segment sliding window | 2.585 |
| `manifest_generation/media_playlist_live/24` | Same, 24-segment window | 10.069 |
| `manifest_generation/media_playlist_live/120` | Same, 120-segment window | 48.971 |
| `init_segment_generation` | Build fMP4 init segment (called once per stream, not on hot path) | 3.866 |
| `segment_index/add_segment_and_generate_playlist/6` | Append segment to index + regenerate playlist, 6 segments total | 6.105 |
| `segment_index/add_segment_and_generate_playlist/12` | Same, 12 segments | 11.372 |
| `segment_index/add_segment_and_generate_playlist/24` | Same, 24 segments | 22.811 |

Numbers are local single-node, all services on one host. Not representative of production (real network, TLS, multi-stream load, larger segments).

## Repository Layout

```text
.
├── src/
│   ├── main.rs              # entry point, wiring, graceful shutdown
│   ├── core/                # config, types, error, auth, shutdown, security
│   ├── ingest/              # RTMP server, HTTP upload, demuxer, validator
│   ├── transcode/           # FFmpeg pipeline, rendition profiles, segment accumulator
│   ├── package/             # fMP4 boxing, HLS manifest, packager task, segment index
│   ├── storage/             # MediaStore trait, S3, in-memory, cache, writer, cleanup
│   ├── delivery/            # Axum router, HTTP handlers, middleware
│   ├── control/             # StreamStateManager, pipeline wiring, event handler
│   └── observability/       # Prometheus metrics
├── benches/
│   └── pipeline_micro.rs    # Criterion microbenchmarks
├── tests/                   # Integration and e2e tests
├── config/
│   └── default.toml         # Base configuration
├── docker/
│   ├── Dockerfile           # Multi-stage build (rust:1.91 + debian:bookworm-slim)
│   └── docker-compose.yml   # streaminfa + MinIO + Prometheus + Grafana
├── scripts/
│   ├── benchmark.sh         # k6 benchmark harness
│   ├── benchmark-capacity.sh
│   ├── benchmark-micro.sh
│   └── benchmark-ops-path.sh
└── docs/                    # Architecture, plans, testing guides
```
