# StreamInfa — Architecture Overview

> **Purpose:** System-level architecture, Rust module structure, component interactions, and key design decisions.
> **Audience:** All engineers working on StreamInfa; technical leads evaluating the design.

---

## 1. Architecture Style

**Choice: Modular Monolith**

StreamInfa MVP is a single deployable binary with well-defined internal module boundaries. Each major subsystem (ingest, transcode, package, storage, delivery, control plane) is a separate Rust module within a single crate.

**Why modular monolith over microservices:**

| Concern | Modular Monolith | Microservices |
|---------|------------------|---------------|
| **Deployment complexity** | Single binary, single process | Multiple services, orchestration, service discovery |
| **Inter-component latency** | Function calls, zero network overhead | Network hops, serialization overhead |
| **Debugging** | Single process, single log stream, easy to trace | Distributed tracing required from day one |
| **Data sharing** | In-process channels (`tokio::sync::mpsc`) | Message bus or RPC, schema versioning |
| **Operational burden** | One deployment target | N deployment targets, N health checks, N scaling policies |
| **Team size** | 1–5 engineers (MVP) | Justified at 10+ engineers with domain ownership |

**Trade-off acknowledged:** A monolith can become a "big ball of mud" if module boundaries are not enforced. We mitigate this with:
- Separate Rust modules with explicit `pub` API surfaces — each module exposes only its public interface via `mod.rs`.
- `pub(crate)` is used for types shared between modules that should not be part of the external API.
- Integration tests exercise cross-module boundaries explicitly.

**Evolution path:** If StreamInfa needs to scale specific components independently (e.g., transcoding workers), we can extract modules into separate crates within a Cargo workspace, then into standalone services connected via a message queue (NATS, Redis Streams). The module structure is designed to make this extraction low-cost.

---

## 2. High-Level Component Diagram

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                          StreamInfa Binary                                   │
│                                                                              │
│  ┌─────────────┐    ┌──────────────┐    ┌────────────┐    ┌──────────────┐  │
│  │   Ingest     │    │  Transcode   │    │  Packager  │    │   Storage    │  │
│  │   Service    │───►│  Pipeline    │───►│   (HLS)    │───►│   Client     │  │
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
│                                                                              │
│  ┌──────────────────────────────────────────────────────────────────────┐    │
│  │                        Common / Core                                 │    │
│  │   config │ error │ metrics │ tracing │ types │ health │ auth        │    │
│  └──────────────────────────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Data Flow Summary

1. **Ingest** receives media (RTMP or HTTP upload), validates, authenticates, and demuxes into elementary streams (H.264 NALUs + AAC frames).
2. **Transcode Pipeline** consumes elementary streams, runs FFmpeg to produce multiple renditions, and emits encoded segments.
3. **Packager** wraps encoded segments into fMP4 containers, generates HLS playlists.
4. **Storage Client** writes fMP4 segments and m3u8 manifests to S3-compatible object storage.
5. **Delivery (Origin)** serves stored segments and manifests to players/CDNs via HTTP.
6. **Control Plane** orchestrates stream lifecycle, exposes admin APIs, and aggregates health/metrics.

---

## 3. Rust Module Structure

StreamInfa is organized as a **single crate** with well-separated modules. This keeps the project simple (one `Cargo.toml`, no workspace overhead) while maintaining clear internal boundaries through Rust's module visibility system.

```
streaminfa/
├── Cargo.toml                    # Single crate
├── Cargo.lock
├── src/
│   ├── main.rs                   # Tokio runtime init, signal handling, component wiring
│   │
│   ├── core/                     # Shared types, error types, config, traits
│   │   ├── mod.rs
│   │   ├── config.rs             # TOML + env var configuration
│   │   ├── error.rs              # Unified error types (thiserror)
│   │   ├── types.rs              # StreamId, RenditionId, SegmentId, MediaInfo
│   │   ├── auth.rs               # AuthN trait + token validation
│   │   └── shutdown.rs           # Graceful shutdown coordination (CancellationToken)
│   │
│   ├── ingest/                   # RTMP server + HTTP upload handler
│   │   ├── mod.rs
│   │   ├── rtmp.rs               # RTMP handshake, command parsing, FLV demux
│   │   ├── http_upload.rs        # Multipart upload handler
│   │   ├── validator.rs          # Codec detection, format validation
│   │   └── demuxer.rs            # Container demux → elementary stream extraction
│   │
│   ├── transcode/                # FFmpeg FFI wrapper, transcoding pipeline
│   │   ├── mod.rs
│   │   ├── pipeline.rs           # Transcode orchestration (input → decode → encode → output)
│   │   ├── profile.rs            # Profile ladder definition
│   │   ├── ffmpeg.rs             # FFI wrapper around ffmpeg-next
│   │   └── segment.rs            # Segment boundary detection, IDR alignment
│   │
│   ├── package/                  # HLS packaging, manifest generation
│   │   ├── mod.rs
│   │   ├── hls.rs                # fMP4 segment wrapping, init segment generation
│   │   ├── manifest.rs           # Media playlist + multivariant playlist generation
│   │   └── segment_index.rs      # Segment tracking for live window management
│   │
│   ├── storage/                  # S3 client abstraction
│   │   ├── mod.rs
│   │   ├── s3.rs                 # S3 put/get/delete/list via aws-sdk-s3
│   │   ├── path.rs               # Deterministic path scheme generation
│   │   └── cache.rs              # In-memory LRU cache for hot segments
│   │
│   ├── delivery/                 # HTTP origin server
│   │   ├── mod.rs
│   │   ├── server.rs             # Axum HTTP server for segment/manifest serving
│   │   ├── handlers.rs           # Request handlers with cache-control, CORS, range
│   │   └── middleware.rs         # Request logging, auth, rate limiting
│   │
│   ├── control/                  # Control plane REST API
│   │   ├── mod.rs
│   │   ├── api.rs                # Stream CRUD, status, health, readiness
│   │   ├── state.rs              # In-memory stream state machine
│   │   └── metrics.rs            # Prometheus metrics endpoint
│   │
│   └── observability/            # Metrics, logging, tracing setup
│       ├── mod.rs
│       ├── metrics.rs            # prometheus crate integration
│       ├── logging.rs            # tracing-subscriber setup, JSON structured logging
│       └── tracing.rs            # OpenTelemetry span propagation
│
├── tests/                        # Integration tests
│   ├── ingest_rtmp_test.rs
│   ├── ingest_upload_test.rs
│   ├── transcode_pipeline_test.rs
│   ├── hls_packaging_test.rs
│   ├── e2e_live_test.rs
│   └── e2e_vod_test.rs
│
├── fixtures/                     # Test media files
│   ├── test_h264_aac.mp4
│   ├── test_h264_aac.flv
│   ├── test_invalid_codec.mp4
│   └── test_corrupt.mp4
│
├── docker/
│   ├── Dockerfile
│   ├── docker-compose.yml
│   └── ffmpeg/                   # FFmpeg build scripts if needed
│
├── config/
│   ├── default.toml              # Default configuration
│   ├── development.toml          # Dev overrides
│   └── production.toml           # Prod overrides
│
└── docs/                         # This documentation
    └── ...
```

### Module Dependency Graph

```
main.rs
  ├── core               (shared types, config, errors — no internal deps)
  ├── ingest             → uses: core
  ├── transcode          → uses: core
  ├── package            → uses: core
  ├── storage            → uses: core
  ├── delivery           → uses: core, storage
  ├── control            → uses: core, ingest, transcode, package, storage
  └── observability      → uses: core
```

**Rule:** No circular dependencies. `core` has zero internal dependencies. Every other module depends on `core` and optionally on peer modules, but never in cycles.

**Visibility enforcement:** Each module's `mod.rs` re-exports only its public API. Internal types use `pub(super)` or remain private. This achieves the same API boundary enforcement as separate crates, with less boilerplate.

**Evolution path to multi-crate workspace:** If compile times grow or the team needs stronger isolation, each `src/{module}/` directory can be extracted into `crates/streaminfa-{module}/` with its own `Cargo.toml`. The `use crate::{module}::` paths become `use streaminfa_{module}::` — a mechanical refactor.

---

## 4. Async Runtime and Concurrency Model

### 4.1 Tokio Configuration

```
Runtime: tokio::runtime::Builder::new_multi_thread()
  - worker_threads: num_cpus (default) or configured
  - max_blocking_threads: 64 (for FFmpeg FFI calls)
  - enable_all() (io + time)
```

### 4.2 Task Architecture

| Task Type | Executor | Reason |
|-----------|----------|--------|
| RTMP connection handling | `tokio::spawn` (async) | I/O-bound: reading TCP packets |
| HTTP upload handling | `tokio::spawn` (async) | I/O-bound: reading multipart body |
| FFmpeg transcoding | `tokio::task::spawn_blocking` | CPU-bound: video encoding is pure computation |
| S3 put/get | `tokio::spawn` (async) | I/O-bound: HTTP to S3 |
| HLS manifest generation | `tokio::spawn` (async) | Lightweight string assembly |
| HTTP origin serving | `tokio::spawn` (async) | I/O-bound: serving HTTP responses |

### 4.3 Inter-Component Channels

Components communicate via bounded `tokio::sync::mpsc` channels. Each channel is explicitly sized to enforce backpressure.

```
Ingest ──(mpsc, cap=64)──► Transcode ──(mpsc, cap=32)──► Packager ──(mpsc, cap=32)──► Storage
```

| Channel | Capacity | Message Type | Backpressure Behavior |
|---------|----------|-------------|----------------------|
| Ingest → Transcode | 64 frames | `DemuxedFrame { stream_id, track, pts, dts, data: Bytes }` | If full, ingest task awaits (TCP backpressure propagates to encoder) |
| Transcode → Packager | 32 segments | `EncodedSegment { rendition, sequence, duration, data: Bytes }` | If full, transcode blocks on send (backpressure to ingest) |
| Packager → Storage | 32 items | `StorageWrite { path, data: Bytes, content_type }` | If full, packager blocks on send |

**Why bounded channels:** Unbounded channels hide memory leaks. A slow consumer (e.g., storage writes stall) would cause unbounded memory growth. Bounded channels make backpressure explicit and measurable.

---

## 5. Buffer and Memory Management

### 5.1 Strategy: `bytes::Bytes` as the Media Data Primitive

All media data flowing through the pipeline uses `bytes::Bytes` (from the `bytes` crate). This provides:

- **Reference-counted sharing:** Multiple consumers can hold a reference to the same buffer without copying.
- **Cheap slicing:** Subslicing a `Bytes` does not copy; it creates a new reference with adjusted offset/length.
- **Deterministic deallocation:** When the last reference is dropped, memory is freed. No GC pauses.

### 5.2 Zero-Copy vs. Copy Trade-offs

| Operation | Strategy | Justification |
|-----------|----------|---------------|
| **RTMP packet → demux buffer** | Copy into `BytesMut`, freeze to `Bytes` | RTMP packets arrive in small TCP reads; we assemble them into NALUs which requires buffering |
| **HTTP upload → disk temp file → demux** | No full copy; stream to temp file, then memory-map or read in chunks | Upload bodies can be multi-GB; we cannot hold them entirely in memory |
| **Demuxed frames → FFmpeg input** | Copy into FFmpeg `AVPacket` | FFI boundary requires copying into FFmpeg-allocated memory; unavoidable |
| **FFmpeg output → `Bytes`** | Copy out of FFmpeg `AVPacket` into `Bytes` | FFmpeg owns its output buffers; we must copy to Rust-owned memory |
| **Encoded segment → fMP4 packaging** | Zero-copy (reference) | fMP4 muxing wraps existing encoded data with headers; no re-encoding |
| **fMP4 segment → S3 upload** | Zero-copy (Bytes passed to HTTP body) | `aws-sdk-s3` accepts `Bytes` directly as the upload body |
| **S3 → HTTP origin response** | Streaming (no full buffering) | Segments are streamed from S3 to the HTTP response body using Tokio's `Body` streaming |

### 5.3 Memory Budget (per live stream, 3 renditions)

| Component | Estimated Memory | Calculation |
|-----------|-----------------|-------------|
| Ingest buffers | ~4 MB | 64 frames × ~64 KB average |
| Transcode input/output | ~24 MB | 3 renditions × 2 buffers × ~4 MB per GOP |
| Packager buffers | ~6 MB | 3 renditions × 1 segment × ~2 MB |
| Manifest state | ~0.1 MB | String data for playlists |
| **Total per stream** | **~34 MB** | |
| **5 concurrent streams** | **~170 MB** | Leaves ~28 GB headroom on 32 GB machine |

---

## 6. Error Handling Philosophy

### 6.1 Error Types

StreamInfa uses `thiserror` for structured error types in each module, and `anyhow` at the top-level `main.rs` boundary for ergonomic context attachment.

Each module defines its own error enum:

```
IngestError { ConnectionReset, InvalidCodec, AuthFailed, Timeout, ... }
TranscodeError { FfmpegInit, DecodeFailed, EncodeFailed, SegmentTooLarge, ... }
PackageError { ManifestGenFailed, InvalidSegment, ... }
StorageError { S3PutFailed, S3GetFailed, BucketNotFound, ... }
DeliveryError { StreamNotFound, RangeNotSatisfiable, ... }
```

### 6.2 Error Propagation Rules

1. **Ingest errors** → log, emit metric, disconnect the stream, notify control plane. Do NOT crash the process.
2. **Transcode errors** → log, emit metric, mark the stream as `error` state, clean up intermediate artifacts.
3. **Storage errors** → retry up to 3 times with exponential backoff (100ms, 200ms, 400ms). If all retries fail, mark stream as `error`.
4. **Delivery errors** → return appropriate HTTP status (404, 500, 503) with JSON error body. Never panic.
5. **No `unwrap()` on fallible operations in production paths.** `unwrap()` is only acceptable in tests and in provably infallible contexts (e.g., static regex compilation in `lazy_static`).

### 6.3 Panic Policy

- Panics are treated as bugs.
- The binary installs a `std::panic::set_hook` that logs the panic with full backtrace and increments a `streaminfa_panic_total` counter.
- If a spawned task panics, the `JoinHandle` captures it and logs the error. The affected stream is marked as `error`.

---

## 7. FFI Assumptions (FFmpeg)

### 7.1 Binding Crate

We use `ffmpeg-next` (Rust bindings for FFmpeg's libav* libraries). This crate provides safe-ish Rust wrappers around:
- `libavcodec` — encoding/decoding
- `libavformat` — container muxing/demuxing
- `libavutil` — pixel formats, math, logging
- `libswscale` — resolution scaling
- `libswresample` — audio resampling

### 7.2 FFI Safety Rules

1. **All FFmpeg calls happen inside `spawn_blocking`.** FFmpeg functions can block for milliseconds to seconds (especially encoding). They must never run on a Tokio async worker.
2. **FFmpeg contexts are `Send` but not `Sync`.** Each transcode pipeline owns its FFmpeg contexts and never shares them across threads.
3. **Memory ownership:** Data passed into FFmpeg (via `AVPacket`) must remain valid for the duration of the call. Data returned by FFmpeg must be copied into `Bytes` before the `AVPacket` is unreffed.
4. **Error mapping:** FFmpeg error codes (negative integers) are mapped to `TranscodeError` variants with the AVERROR string included for diagnostics.
5. **Thread safety:** FFmpeg's `avcodec_open2` is not thread-safe. We protect it with a global `tokio::sync::Mutex` (only used during codec initialization, not during encoding).

### 7.3 FFmpeg Build Configuration

For the Docker image, FFmpeg is compiled with:
```
--enable-gpl --enable-libx264 --enable-libfdk-aac
--disable-programs --disable-doc --disable-network
--enable-shared --disable-static
```
Only the libraries needed for H.264/AAC are included. Network support is disabled (StreamInfa handles all I/O).

---

## 8. Configuration Model

### 8.1 Layering

Configuration is loaded in this order (later overrides earlier):
1. `config/default.toml` — defaults compiled into the binary
2. `config/{STREAMINFA_ENV}.toml` — environment-specific overrides (development, production)
3. Environment variables — `STREAMINFA_*` prefix, mapped to config keys

### 8.2 Key Configuration Sections

```toml
[server]
host = "0.0.0.0"
control_port = 8080        # Control plane + delivery
rtmp_port = 1935           # RTMP ingest

[ingest]
max_bitrate_kbps = 20000
max_upload_size_bytes = 10737418240  # 10 GB
allowed_codecs = ["h264"]
allowed_audio_codecs = ["aac", "mp3"]

[transcode]
profile_ladder = [
    { name = "high",   width = 1920, height = 1080, bitrate_kbps = 3500, profile = "high",  level = "4.1" },
    { name = "medium", width = 1280, height = 720,  bitrate_kbps = 2000, profile = "main",  level = "3.1" },
    { name = "low",    width = 854,  height = 480,  bitrate_kbps = 1000, profile = "main",  level = "3.0" },
]
keyframe_interval_secs = 2
max_blocking_threads = 64

[packaging]
segment_duration_secs = 6
live_window_segments = 5
hls_version = 7

[storage]
backend = "s3"
endpoint = "http://localhost:9000"
bucket = "streaminfa-media"
access_key_id = ""          # Set via env var
secret_access_key = ""      # Set via env var
region = "us-east-1"

[delivery]
cache_control_live = "no-cache, no-store"
cache_control_vod = "public, max-age=86400"
cors_allowed_origins = ["*"]

[observability]
log_level = "info"
log_format = "json"         # "json" or "pretty"
metrics_enabled = true
tracing_enabled = false     # OpenTelemetry, disabled by default
tracing_endpoint = "http://localhost:4317"

[auth]
ingest_stream_keys = []     # Set via env var or config reload
admin_bearer_tokens = []    # Set via env var
```

---

## 9. Graceful Shutdown Sequence

StreamInfa uses `tokio_util::sync::CancellationToken` for coordinated shutdown.

```
Signal (SIGTERM/SIGINT)
    │
    ▼
1. Set CancellationToken (broadcast to all tasks)
    │
    ▼
2. Stop accepting new RTMP connections and HTTP uploads
    │
    ▼
3. Wait for active ingest streams to finish current segment (timeout: 15s)
    │
    ▼
4. Wait for in-flight transcode jobs to complete (timeout: 10s)
    │
    ▼
5. Flush remaining segments to storage (timeout: 5s)
    │
    ▼
6. Close HTTP server (drain in-flight requests, timeout: 5s)
    │
    ▼
7. Exit with code 0
    │
    Total shutdown timeout: 30 seconds. After 30s, force exit with code 1.
```

---

## Definition of Done — Architecture Overview

- [x] Architecture style chosen and justified (modular monolith)
- [x] Component diagram with all major subsystems
- [x] Rust module structure fully documented with file-level detail
- [x] Module dependency graph defined, no circular dependencies
- [x] Tokio runtime configuration specified
- [x] Task architecture (async vs blocking) documented
- [x] Inter-component channel design with capacities and backpressure behavior
- [x] Buffer and memory management strategy with `bytes::Bytes`
- [x] Zero-copy vs copy trade-offs explicitly analyzed
- [x] Memory budget estimated per stream
- [x] Error handling philosophy documented with propagation rules
- [x] Panic policy defined
- [x] FFI assumptions and safety rules for FFmpeg documented
- [x] Configuration model documented with sample TOML
- [x] Graceful shutdown sequence specified with timeouts
