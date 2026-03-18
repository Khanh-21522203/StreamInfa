# StreamInfa — Control Plane vs Data Plane

> **Purpose:** Define the separation between control plane (stream lifecycle, admin APIs, orchestration) and data plane (media flow, ingest, transcode, package, deliver). Document all API surfaces and internal coordination.
> **Audience:** Engineers implementing the control module; API consumers; operators managing the system.

---

## 1. Plane Separation Principle

StreamInfa separates concerns into two logical planes within the same process:

```
┌──────────────────────────────────────────────────────────────────────────┐
│                          StreamInfa Process                               │
│                                                                           │
│  ┌─────────────────────────────────────────────────────────────────────┐  │
│  │                       CONTROL PLANE                                 │  │
│  │                                                                     │  │
│  │  ┌──────────┐  ┌──────────────┐  ┌────────────┐  ┌──────────────┐ │  │
│  │  │  REST    │  │   Stream     │  │   Health   │  │   Metrics    │ │  │
│  │  │  API     │  │   State      │  │   Check    │  │   Endpoint   │ │  │
│  │  │ (Axum)   │  │   Machine    │  │            │  │ (Prometheus) │ │  │
│  │  └──────────┘  └──────────────┘  └────────────┘  └──────────────┘ │  │
│  └───────────────────────────┬─────────────────────────────────────────┘  │
│                              │ coordinates                                │
│  ┌───────────────────────────▼─────────────────────────────────────────┐  │
│  │                        DATA PLANE                                   │  │
│  │                                                                     │  │
│  │  ┌──────────┐  ┌──────────────┐  ┌────────────┐  ┌──────────────┐ │  │
│  │  │  Ingest  │  │  Transcode   │  │  Packager  │  │  Delivery    │ │  │
│  │  │  Service │──│  Pipeline    │──│   (HLS)    │──│  (Origin)    │ │  │
│  │  └──────────┘  └──────────────┘  └────────────┘  └──────────────┘ │  │
│  │                                                                     │  │
│  │  ┌──────────────────────────────────────────────────────────────┐  │  │
│  │  │                    Storage Layer                              │  │  │
│  │  └──────────────────────────────────────────────────────────────┘  │  │
│  └─────────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────────────┘
```

### Why Separate Planes?

| Concern | Control Plane | Data Plane |
|---------|--------------|------------|
| **Traffic pattern** | Low-frequency API calls (tens/sec) | High-throughput media flow (thousands of frames/sec) |
| **Latency sensitivity** | Tolerant (100ms+ acceptable) | Critical (microseconds matter in pipeline) |
| **Failure impact** | API unavailable; streams continue | Stream interrupted; media lost |
| **Scaling need** | Minimal (one API server) | Intensive (CPU for transcode, I/O for storage) |
| **Authentication** | Required (admin tokens) | Per-path (stream keys for ingest, open for delivery in MVP) |

**Key rule:** A control plane failure (e.g., REST API crashes) must NOT interrupt active data plane operations (ongoing live streams, transcode jobs, segment delivery). This is achieved by:
- Data plane components hold their own state (channel handles, FFmpeg contexts) independently of the control plane.
- Control plane writes stream state to an in-memory store. Data plane reads state but does not depend on the control plane being responsive for ongoing operations.
- The control plane and data plane share a `CancellationToken` only for graceful shutdown.

---

## 2. Control Plane Components

### 2.1 Stream State Machine

The control plane owns the authoritative stream state. State is stored in-memory in a `DashMap<StreamId, StreamRecord>` (concurrent hash map).

```
StreamRecord {
    id: StreamId,                          // UUIDv7
    status: StreamStatus,                  // Pending, Live, Processing, Ready, Error, Deleted
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    media_info: Option<MediaInfo>,         // Populated after ingest validation
    ingest_type: IngestType,               // Rtmp | HttpUpload
    renditions: Vec<RenditionStatus>,      // Per-rendition progress
    error: Option<StreamError>,            // If status == Error
    metadata: HashMap<String, String>,     // User-provided metadata (title, tags)
}

RenditionStatus {
    name: String,                          // "high", "medium", "low"
    segments_produced: u64,
    last_segment_time: Option<DateTime<Utc>>,
    status: RenditionState,                // Active, Complete, Error
}
```

**Persistence:** In MVP, stream state is in-memory only. Process restart loses state. This is acceptable because:
- Live streams are inherently ephemeral (restart = reconnect).
- VOD streams can be re-uploaded.
- Phase 2 adds SQLite or PostgreSQL for durable state.

**[ASSUMPTION]** Losing in-memory state on restart is acceptable for MVP. S3 data (segments, manifests) persists independently.

### 2.2 State Transition Rules

| Current State | Event | New State | Side Effects |
|--------------|-------|-----------|-------------|
| (none) | `POST /api/v1/streams` | `PENDING` | Generate stream_id, allocate stream key |
| `PENDING` | RTMP publish received | `LIVE` | Start ingest + transcode pipeline |
| `PENDING` | HTTP upload starts | `PROCESSING` | Stream body to temp file |
| `LIVE` | RTMP disconnect | `PROCESSING` | Flush transcode buffers |
| `LIVE` | Unrecoverable error | `ERROR` | Clean up pipeline, log error |
| `PROCESSING` | All renditions complete | `READY` | Mark all renditions as complete |
| `PROCESSING` | Unrecoverable error | `ERROR` | Clean up, log error |
| `READY` | `DELETE /api/v1/streams/{id}` | `DELETED` | Delete S3 objects, retain metadata |
| `ERROR` | `DELETE /api/v1/streams/{id}` | `DELETED` | Delete S3 objects if any |
| Any | `DELETE /api/v1/streams/{id}` | `DELETED` | Abort active pipeline if running |

**Invalid transitions** are rejected with `409 Conflict` and a descriptive error message.

---

## 3. REST API Surface

All control plane APIs are served on the same HTTP port as delivery (default `:8080`), under the `/api/v1/` prefix. Delivery endpoints are under `/streams/`.

### 3.1 Stream Management

#### Create Stream

```
POST /api/v1/streams
Authorization: Bearer {admin_token}
Content-Type: application/json

{
  "ingest_type": "rtmp",           // "rtmp" or "upload"
  "metadata": {
    "title": "My Live Stream",
    "tags": ["event", "keynote"]
  }
}
```

**Response (201 Created):**
```json
{
  "stream_id": "01906b3a-7f1e-7000-8000-000000000001",
  "status": "pending",
  "stream_key": "sk_a1b2c3d4e5f6g7h8",
  "rtmp_url": "rtmp://streaminfa:1935/live/sk_a1b2c3d4e5f6g7h8",
  "created_at": "2025-06-15T14:30:00Z"
}
```

**Notes:**
- `stream_key` is auto-generated (cryptographically random, 24 chars, prefixed with `sk_`).
- `rtmp_url` is populated only when `ingest_type == "rtmp"`.
- For `ingest_type == "upload"`, the client subsequently calls the upload endpoint.

#### List Streams

```
GET /api/v1/streams?status=live&limit=50&offset=0
Authorization: Bearer {admin_token}
```

**Response (200 OK):**
```json
{
  "streams": [
    {
      "stream_id": "01906b3a-7f1e-7000-8000-000000000001",
      "status": "live",
      "ingest_type": "rtmp",
      "created_at": "2025-06-15T14:30:00Z",
      "media_info": {
        "video_codec": "h264",
        "resolution": "1920x1080",
        "audio_codec": "aac"
      }
    }
  ],
  "total": 1,
  "limit": 50,
  "offset": 0
}
```

#### Get Stream

```
GET /api/v1/streams/{stream_id}
Authorization: Bearer {admin_token}
```

**Response (200 OK):**
```json
{
  "stream_id": "01906b3a-7f1e-7000-8000-000000000001",
  "status": "live",
  "ingest_type": "rtmp",
  "created_at": "2025-06-15T14:30:00Z",
  "updated_at": "2025-06-15T14:35:00Z",
  "media_info": {
    "video_codec": "h264",
    "resolution": "1920x1080",
    "video_bitrate_kbps": 4500,
    "audio_codec": "aac",
    "audio_sample_rate": 48000,
    "audio_channels": 2
  },
  "renditions": [
    { "name": "high",   "segments_produced": 50, "status": "active" },
    { "name": "medium", "segments_produced": 50, "status": "active" },
    { "name": "low",    "segments_produced": 50, "status": "active" }
  ],
  "playback_url": "http://streaminfa:8080/streams/01906b3a-.../master.m3u8",
  "metadata": {
    "title": "My Live Stream",
    "tags": ["event", "keynote"]
  }
}
```

#### Delete Stream

```
DELETE /api/v1/streams/{stream_id}
Authorization: Bearer {admin_token}
```

**Response (200 OK):**
```json
{
  "stream_id": "01906b3a-7f1e-7000-8000-000000000001",
  "status": "deleted",
  "segments_deleted": 150,
  "storage_freed_bytes": 314572800
}
```

**Side effects:**
- If stream is `LIVE`: disconnect RTMP, abort transcode, then delete.
- If stream is `PROCESSING`: abort transcode, then delete.
- Delete all S3 objects under the stream prefix.
- Retain `StreamRecord` in state (with `DELETED` status) for 24 hours for audit, then remove.

#### Rotate Stream Key

```
POST /api/v1/streams/{id}/rotate-key
Authorization: Bearer {admin_token}
```

**Response (200 OK):**
```json
{
  "stream_id": "01906b3a-7f1e-7000-8000-000000000001",
  "stream_key": "sk_n3w4x5y6z7a8b9c0d1e2f3g4",
  "rotated_at": "2025-06-15T14:36:00Z"
}
```

**Behavior:**
- Generates a new stream key and invalidates the old key for new publish attempts.
- Existing active RTMP sessions are not force-disconnected.
- Does not change stream lifecycle state.

#### Upload VOD

```
POST /api/v1/streams/upload
Authorization: Bearer {admin_token}
Content-Type: multipart/form-data
```

(See `docs/architecture/ingest.md` §3.2 for full specification.)

### 3.2 Health and Readiness

#### Health Check

```
GET /healthz
```

**Response (200 OK):**
```json
{
  "status": "healthy",
  "uptime_secs": 86400,
  "version": "0.1.0"
}
```

No authentication required. Used by load balancers and container orchestrators. Returns 200 if the process is running and can accept requests. Does NOT check downstream dependencies.

#### Readiness Check

```
GET /readyz
```

**Response (200 OK):**
```json
{
  "status": "ready",
  "checks": {
    "storage": { "status": "ok", "latency_ms": 15 },
    "ffmpeg": { "status": "ok", "version": "6.1.1" },
    "rtmp_listener": { "status": "ok", "port": 1935 },
    "disk_space": { "status": "ok", "available_gb": 120.5 }
  }
}
```

**Response (503 Service Unavailable):**
```json
{
  "status": "not_ready",
  "checks": {
    "storage": { "status": "error", "error": "connection refused" },
    "ffmpeg": { "status": "ok", "version": "6.1.1" },
    "rtmp_listener": { "status": "ok", "port": 1935 },
    "disk_space": { "status": "ok", "available_gb": 120.5 }
  }
}
```

**Readiness checks:**
- **Storage:** HEAD request to the S3 bucket (latency measured).
- **FFmpeg:** Check that `ffmpeg-next` can initialize an encoder context.
- **RTMP listener:** Check that the TCP listener is bound.
- **Disk space:** Check that temp directory has >10 GB free.

#### Metrics

```
GET /metrics
```

Returns Prometheus text exposition format. No authentication required (metrics endpoint is typically not exposed externally; it is scraped by Prometheus from within the network).

### 3.3 API Summary Table

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `POST` | `/api/v1/streams` | Bearer | Create a new stream |
| `GET` | `/api/v1/streams` | Bearer | List streams (with filters) |
| `GET` | `/api/v1/streams/{id}` | Bearer | Get stream details |
| `DELETE` | `/api/v1/streams/{id}` | Bearer | Delete stream and assets |
| `POST` | `/api/v1/streams/{id}/rotate-key` | Bearer | Rotate ingest key for a stream |
| `POST` | `/api/v1/streams/upload` | Bearer | Upload VOD file |
| `GET` | `/healthz` | None | Liveness probe |
| `GET` | `/readyz` | None | Readiness probe |
| `GET` | `/metrics` | None | Prometheus metrics |
| `GET` | `/streams/{id}/master.m3u8` | None | HLS master playlist |
| `GET` | `/streams/{id}/{rendition}/media.m3u8` | None | HLS media playlist |
| `GET` | `/streams/{id}/{rendition}/init.mp4` | None | HLS init segment |
| `GET` | `/streams/{id}/{rendition}/seg_{n}.m4s` | None | HLS media segment |

---

## 4. Internal Coordination (Control ↔ Data)

### 4.1 Control → Data Signals

| Signal | Mechanism | Purpose |
|--------|-----------|---------|
| **Start pipeline** | Direct function call (`Pipeline::start(stream_id, config)`) | Control plane creates pipeline components and wires channels |
| **Stop pipeline** | Per-stream `CancellationToken` | Control plane cancels a specific stream's token; all tasks observe and shut down |
| **Config update** | `tokio::sync::watch` channel | Control plane sends updated config (e.g., new stream keys) to data plane listeners |

### 4.2 Data → Control Signals

| Signal | Mechanism | Purpose |
|--------|-----------|---------|
| **Stream started** | `tokio::sync::mpsc` event channel | Ingest notifies control plane that RTMP publish succeeded |
| **Media info detected** | Event channel | Ingest sends `MediaInfo` after validation |
| **Segment produced** | Event channel | Transcode/packager notifies control plane (for progress tracking) |
| **Stream ended** | Event channel | Ingest notifies of disconnection |
| **Stream error** | Event channel | Any data plane component reports an unrecoverable error |
| **VOD progress** | `tokio::sync::watch` channel | Transcode reports percentage progress |

### 4.3 Event Channel Design

```
enum PipelineEvent {
    StreamStarted { stream_id: StreamId, media_info: MediaInfo },
    SegmentProduced { stream_id: StreamId, rendition: String, sequence: u64 },
    StreamEnded { stream_id: StreamId },
    StreamError { stream_id: StreamId, error: String },
    VodProgress { stream_id: StreamId, percent: f32 },
}
```

The event channel is a single `tokio::sync::mpsc` (capacity: 256) from the data plane to the control plane. The control plane has a dedicated task that reads from this channel and updates `StreamRecord` state.

**Why a single channel (not per-stream):** Simplicity. At MVP scale (5–10 concurrent streams), a single channel with 256 capacity is more than sufficient. The control plane event handler is fast (state machine update = microseconds). No risk of blocking.

---

## 5. Pipeline Wiring

When the control plane creates a stream, it wires up the data plane pipeline:

```
fn create_pipeline(stream_id: StreamId, config: &Config) -> Pipeline {
    // Channels
    let (ingest_tx, ingest_rx) = mpsc::channel(64);    // ingest → transcode
    let (transcode_tx, transcode_rx) = mpsc::channel(32); // transcode → packager
    let (package_tx, package_rx) = mpsc::channel(32);   // packager → storage
    let (event_tx) = shared_event_channel.clone();       // data → control

    // Per-stream cancellation
    let cancel = CancellationToken::new();

    Pipeline {
        ingest_tx,
        ingest_rx,
        transcode_tx,
        transcode_rx,
        package_tx,
        package_rx,
        event_tx,
        cancel,
    }
}
```

The control plane holds the `CancellationToken` and the event receiver. The data plane holds the channels and runs tasks until the token is cancelled or the stream ends naturally.

---

## 6. Configuration Reload

### 6.1 Mechanism

Configuration reload is triggered by `SIGHUP` (Unix signal):

```
SIGHUP received
    │
    ▼
Re-read config file (default.toml + {env}.toml)
    │
    ▼
Validate new config (parse, check constraints)
    │ fail → log error, keep old config
    ▼
Diff old config vs new config
    │
    ▼
Apply changes:
    ├── Stream keys changed → update auth module via watch channel
    ├── Admin tokens changed → update auth module via watch channel
    ├── Log level changed → update tracing filter dynamically
    ├── Profile ladder changed → apply to NEW streams only (active streams keep old ladder)
    ├── Segment duration changed → apply to NEW streams only
    └── Storage config changed → NOT supported at runtime (requires restart)
```

### 6.2 Hot-Reloadable vs Restart-Required

| Config | Hot-Reloadable | Restart-Required |
|--------|---------------|-------------------|
| Stream keys | Yes | No |
| Admin tokens | Yes | No |
| Log level | Yes | No |
| CORS origins | Yes | No |
| Profile ladder | Yes (new streams) | No |
| Segment duration | Yes (new streams) | No |
| RTMP port | No | Yes |
| HTTP port | No | Yes |
| S3 endpoint/bucket | No | Yes |
| S3 credentials | No | Yes |
| Max blocking threads | No | Yes |

---

## Definition of Done — Control Plane vs Data Plane

- [x] Plane separation principle explained with clear rationale
- [x] Control plane components (state machine, API, health, metrics) defined
- [x] Data plane components listed with separation from control
- [x] Stream state machine with all transitions and side effects
- [x] Full REST API surface documented (request/response formats, status codes)
- [x] Health check and readiness check specifications
- [x] Internal coordination mechanisms (control↔data) documented
- [x] Event channel design with message types
- [x] Pipeline wiring described
- [x] Configuration reload mechanism with hot-reloadable vs restart-required
- [x] In-memory state limitation acknowledged with evolution path
