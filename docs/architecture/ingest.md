# StreamInfa — Ingest Service

> **Purpose:** Detailed design of the ingest layer: RTMP live ingest and HTTP VOD upload, including protocol handling, authentication, validation, demuxing, and error management.
> **Audience:** Engineers implementing the ingest module; anyone debugging ingest-related issues.

---

## 1. Ingest Architecture

The ingest service is the entry point for all media into StreamInfa. It handles two distinct paths:

```
                         ┌─────────────────────────────────┐
                         │         Ingest Service           │
                         │      (src/ingest/)        │
                         │                                  │
  RTMP Encoder ─────────►│  ┌───────────┐                  │
  (live, port 1935)      │  │   RTMP    │   ┌───────────┐  │
                         │  │  Server   │──►│ Validator  │  │     ┌──────────────┐
                         │  └───────────┘   │ & Demuxer  │──────► │  Transcode   │
                         │                  │            │  │     │  Pipeline    │
  HTTP Client ──────────►│  ┌───────────┐   │            │  │     │  (mpsc ch)   │
  (VOD, port 8080)       │  │   HTTP    │──►│            │  │     └──────────────┘
                         │  │  Upload   │   └───────────┘  │
                         │  └───────────┘                  │
                         │         │                        │
                         │         ▼                        │
                         │  ┌───────────┐                  │
                         │  │   Auth    │                  │
                         │  │  Module   │                  │
                         │  └───────────┘                  │
                         └─────────────────────────────────┘
```

Both paths converge at the validator/demuxer, which produces a unified `DemuxedFrame` stream sent to the transcode pipeline.

---

## 2. RTMP Ingest (Live)

### 2.1 Protocol Choice Justification

**RTMP is the MVP choice for live ingest because:**
- Every major encoder (OBS, FFmpeg, Wirecast, vMix) supports RTMP natively.
- RTMP is a simple, well-understood protocol with decades of production use.
- RTMP's FLV container is trivial to demux (tag-based, sequential).
- No additional protocol negotiation complexity (unlike SRT which requires connection bonding, encryption negotiation, etc.).

**Trade-offs accepted:**
- RTMP is TCP-based, so it suffers from head-of-line blocking on lossy networks. SRT (UDP-based) handles this better. SRT is a Phase 2 addition.
- RTMP is not actively developed (Adobe deprecated it), but the wire protocol is stable and all encoder implementations remain maintained.
- RTMP does not natively support HEVC/AV1. Since MVP is H.264-only, this is not a limitation.

### 2.2 RTMP Handshake

StreamInfa implements the RTMP handshake as per the RTMP specification (Adobe, 2012):

```
Client                         Server
  │                              │
  │──── C0 (1 byte: version) ──►│
  │──── C1 (1536 bytes) ───────►│
  │                              │
  │◄─── S0 (1 byte: version) ──│
  │◄─── S1 (1536 bytes) ───────│
  │◄─── S2 (1536 bytes) ───────│
  │                              │
  │──── C2 (1536 bytes) ───────►│
  │                              │
  │     [Handshake complete]     │
```

**Implementation notes:**
- We accept RTMP version 3 only.
- S1/S2 are generated with random bytes (no complex handshake; we do not implement RTMPE encryption).
- Handshake timeout: 5 seconds. If C0/C1 does not arrive within 5s, drop the connection.

### 2.3 RTMP Command Processing

After handshake, the client sends AMF-encoded commands:

| Command | StreamInfa Action |
|---------|------------------|
| `connect` | Parse `app` field from connection URL. Respond with `_result` + server bandwidth settings. |
| `releaseStream` | Acknowledge. No-op for our implementation. |
| `FCPublish` | Acknowledge. No-op. |
| `createStream` | Respond with stream ID (always 1 for simplicity). |
| `publish` | Extract stream key from the publish name. Authenticate. If valid, begin accepting media data. If invalid, send `onStatus` with `NetStream.Publish.Denied` and disconnect. |
| `deleteStream` | Graceful stream termination. Flush buffers. |

**Stream key extraction:**
- URL format: `rtmp://{host}:1935/live/{stream_key}`
- `app` = `live` (from `connect`)
- `stream_key` = publish name (from `publish`)
- The stream key is looked up in the configured key set.

### 2.4 FLV Demuxing

RTMP carries media as FLV tags within RTMP chunk stream messages:

```
RTMP Message (type 8 = audio, type 9 = video)
  │
  ▼
FLV Tag Header
  ├── TagType (1 byte): 0x08 (audio) or 0x09 (video)
  ├── DataSize (3 bytes)
  ├── Timestamp (3 bytes + 1 byte extension = 4 bytes effective)
  │
  ▼
FLV Tag Body
  ├── [Video] CodecID (4 bits) + FrameType (4 bits) + AVC packet type + composition time offset + NALUs
  └── [Audio] SoundFormat (4 bits) + SampleRate + SampleSize + Channels + AAC packet type + raw AAC data
```

**Video demux logic:**
1. Check CodecID = 7 (H.264). If not, reject stream.
2. If AVC packet type = 0 → AVC Sequence Header (contains SPS/PPS). Parse and validate.
3. If AVC packet type = 1 → AVC NALU(s). Each NALU is length-prefixed (4-byte big-endian length + NALU bytes).
4. Extract NAL unit type from the first byte of each NALU:
   - Type 5 (IDR) → keyframe = true
   - Type 1 (non-IDR slice) → keyframe = false
   - Type 7 (SPS), Type 8 (PPS) → parameter sets (store for re-injection if needed)
5. Convert timestamps: FLV timestamp (milliseconds) → PTS/DTS in 90kHz timebase (multiply by 90).

**Audio demux logic:**
1. Check SoundFormat = 10 (AAC) or 2 (MP3).
2. If AAC and packet type = 0 → AudioSpecificConfig. Parse and validate (sample rate, channels, profile).
3. If AAC and packet type = 1 → raw AAC frame. Forward as-is.
4. If MP3 → forward raw frame (will be re-encoded to AAC in transcode stage).

### 2.5 RTMP Connection Lifecycle

```
TCP Accept
    │
    ▼
Handshake (5s timeout)
    │ fail → drop connection
    ▼
connect command
    │
    ▼
publish command
    │
    ▼
Auth check (stream key lookup)
    │ fail → send Publish.Denied, disconnect
    ▼
Receive AVC Sequence Header + AudioSpecificConfig
    │ fail (timeout 10s or invalid) → disconnect
    ▼
Codec validation
    │ fail → disconnect
    ▼
Start demux loop:
    ┌──────────────────────────────────────┐
    │ Read RTMP message                     │
    │ Parse FLV tag                         │
    │ Demux to DemuxedFrame                 │
    │ Send to transcode channel             │
    │   └─ if channel full, await (backpressure) │
    │ Loop until disconnect or error        │
    └──────────────────────────────────────┘
    │
    ▼
Stream end:
    Flush last frames
    Notify control plane
    Close TCP connection
```

### 2.6 RTMP Error Handling

| Error | Detection | Response |
|-------|-----------|----------|
| Handshake timeout | Timer (5s) | Drop TCP connection, log warning |
| Invalid RTMP version | C0 byte ≠ 3 | Drop connection |
| Malformed AMF | AMF parse failure | Drop connection, log error |
| Invalid stream key | Key not in config | Send `NetStream.Publish.Denied`, disconnect |
| Unsupported codec | CodecID ≠ 7 | Disconnect, log with codec info |
| Missing sequence header | No SPS/PPS within 10s | Disconnect, log timeout |
| TCP read error | `io::Error` | Log, flush buffered frames, finalize |
| Corrupt NALU | Length mismatch or invalid NAL type | Skip frame, increment error counter, continue (do not disconnect for transient corruption) |

**Resilience policy:** Transient single-frame corruption is logged and skipped. The decoder in the transcode stage handles missing frames via error concealment. Persistent corruption (>10 consecutive errors) triggers disconnect.

---

## 3. HTTP Upload Ingest (VOD)

### 3.1 Protocol Choice Justification

**HTTP multipart upload is the MVP choice for VOD because:**
- Every HTTP client (curl, browsers, SDKs) supports multipart form upload.
- No custom protocol needed; standard HTTP semantics.
- Easy to add authentication (bearer token in header).
- File size limits are enforced at the application layer.

**Trade-offs accepted:**
- No resumable upload support in MVP. Large files (multi-GB) must be re-uploaded on failure. Acceptable for internal MVP use.
- No chunked/parallel upload. Single sequential upload stream. Acceptable for MVP throughput targets.

### 3.2 Upload API

**Endpoint:** `POST /api/v1/streams/upload`

**Request:**
```
POST /api/v1/streams/upload HTTP/1.1
Host: streaminfa:8080
Authorization: Bearer {admin_token}
Content-Type: multipart/form-data; boundary=----Boundary

------Boundary
Content-Disposition: form-data; name="file"; filename="video.mp4"
Content-Type: video/mp4

<binary file data>
------Boundary
Content-Disposition: form-data; name="metadata"
Content-Type: application/json

{"title": "My Video", "tags": ["test"]}
------Boundary--
```

**Response (success):**
```json
{
  "stream_id": "01906b3a-7f1e-7000-8000-000000000001",
  "status": "processing",
  "upload_size_bytes": 524288000,
  "detected_codec": "h264",
  "detected_audio": "aac",
  "duration_secs": 600.0,
  "resolution": "1920x1080"
}
```

**Response (error):**
```json
{
  "error": "unsupported_codec",
  "message": "Video codec 'hevc' is not supported. Supported codecs: h264.",
  "stream_id": "01906b3a-7f1e-7000-8000-000000000001"
}
```

**Status codes:**

| Code | Condition |
|------|-----------|
| `201 Created` | Upload accepted, processing started |
| `400 Bad Request` | Missing file field, invalid multipart |
| `401 Unauthorized` | Missing or invalid bearer token |
| `413 Payload Too Large` | File exceeds `max_upload_size_bytes` (default 10 GB) |
| `415 Unsupported Media Type` | Not MP4/MKV container |
| `422 Unprocessable Entity` | Unsupported codec, corrupt file, duration exceeds limit |
| `507 Insufficient Storage` | Temp disk space insufficient |

### 3.3 Upload Processing Flow

```
HTTP Request
    │
    ▼
Auth (Bearer token)
    │ fail → 401
    ▼
Generate stream_id (UUIDv7)
    │
    ▼
Stream body to temp file:
    /tmp/streaminfa/uploads/{stream_id}.tmp
    │
    │  Monitor: file size vs max_upload_size_bytes
    │  If exceeded → abort, delete temp, 413
    │
    ▼
Upload complete
    │
    ▼
Probe temp file with FFmpeg:
    ffmpeg::format::input(&path)
    Extract: container, codecs, duration, resolution, bitrate
    │
    │  Validation checks (see §3.4)
    │  If failed → delete temp, return error
    │
    ▼
Create stream record (state: PROCESSING)
    │
    ▼
Submit transcode job (async)
    │
    ▼
Return 201 with stream metadata
```

### 3.4 Upload Validation Rules

| Check | Condition | Error |
|-------|-----------|-------|
| Container format | Must be MP4 (ftyp box present) or MKV (EBML header) | 415 |
| Video codec | Must be H.264 (AVC) | 422 `unsupported_codec` |
| Audio codec | Must be AAC or MP3 (or no audio) | 422 `unsupported_codec` |
| Duration | Must be ≤ 6 hours (21600s) | 422 `duration_exceeded` |
| Resolution | Must be ≤ 3840×2160 and > 0×0 | 422 `invalid_resolution` |
| File integrity | FFmpeg must be able to open and read the moov atom | 422 `corrupt_file` |
| File size | Must be ≤ `max_upload_size_bytes` | 413 |

---

## 4. Shared Validation and Demuxing

Both ingest paths (RTMP and HTTP upload) converge at a shared validation and demuxing layer. The output is a stream of `DemuxedFrame` messages.

### 4.1 DemuxedFrame Structure

```
DemuxedFrame {
    stream_id: StreamId,        // UUIDv7
    track: Track,               // Video | Audio
    pts: i64,                   // Presentation timestamp (90kHz timebase)
    dts: i64,                   // Decode timestamp (90kHz timebase)
    keyframe: bool,             // true for IDR (video) or every audio frame
    data: Bytes,                // NALU data (video) or raw AAC/MP3 frame (audio)
}

enum Track {
    Video {
        codec: VideoCodec,      // H264
        width: u32,
        height: u32,
    },
    Audio {
        codec: AudioCodec,      // Aac, Mp3
        sample_rate: u32,       // 44100, 48000
        channels: u8,           // 1 (mono), 2 (stereo)
    },
}
```

### 4.2 Timestamp Normalization

- RTMP FLV timestamps are in milliseconds. Converted to 90kHz: `pts_90k = pts_ms * 90`.
- MP4 timestamps use a per-track timescale (e.g., 48000 for 48kHz audio). Converted to 90kHz: `pts_90k = pts * 90000 / timescale`.
- All downstream components (transcode, packager) operate in 90kHz timebase exclusively.
- PTS/DTS values are normalized to start at 0 for each stream (subtract the first PTS from all subsequent values).

### 4.3 Media Info Extraction

On the first keyframe (or the AVC Sequence Header for RTMP), the ingest service extracts and stores `MediaInfo`:

```
MediaInfo {
    stream_id: StreamId,
    video_codec: VideoCodec,
    video_width: u32,
    video_height: u32,
    video_bitrate_kbps: Option<u32>,  // Estimated from first few seconds
    audio_codec: AudioCodec,
    audio_sample_rate: u32,
    audio_channels: u8,
    audio_bitrate_kbps: Option<u32>,
    duration_secs: Option<f64>,       // Known for VOD, None for live
    container: Container,             // Flv, Mp4, Mkv
}
```

This `MediaInfo` is sent to the control plane for storage and is used by the transcode pipeline to determine which renditions to produce (skip renditions above the source resolution).

---

## 5. Backpressure at Ingest

### 5.1 RTMP Backpressure Chain

```
Transcode channel full (cap: 64)
    │
    ▼
mpsc::Sender::send().await blocks
    │
    ▼
Ingest task stops reading from TCP socket
    │
    ▼
TCP receive buffer fills up
    │
    ▼
TCP window size shrinks → encoder's TCP send blocks
    │
    ▼
Encoder buffers frames internally
    │
    ▼
If encoder buffer fills → encoder drops frames or increases latency
```

This is the correct and intended behavior. The encoder is the only component that can make quality/latency trade-off decisions when the pipeline is overloaded.

### 5.2 HTTP Upload Backpressure

HTTP uploads write to disk, not to the transcode channel directly. Therefore backpressure is disk I/O bound:
- If disk write is slow, the HTTP body read slows down (Axum's body streaming is demand-driven).
- Client sees slow upload speed, which is the correct signal.

### 5.3 Metrics for Ingest Backpressure

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `streaminfa_ingest_active_streams` | Gauge | `protocol` | Number of currently active ingest streams |
| `streaminfa_ingest_bytes_total` | Counter | `protocol`, `stream_id` | Total bytes ingested |
| `streaminfa_ingest_frames_total` | Counter | `protocol`, `stream_id`, `track` | Total frames demuxed |
| `streaminfa_ingest_errors_total` | Counter | `protocol`, `error_type` | Ingest errors by type |
| `streaminfa_ingest_channel_utilization` | Gauge | `stream_id` | Current occupancy of the ingest→transcode channel (0.0–1.0) |
| `streaminfa_ingest_bitrate_bps` | Gauge | `stream_id` | Estimated ingest bitrate (rolling 5s window) |
| `streaminfa_upload_duration_seconds` | Histogram | | Time to complete HTTP upload |
| `streaminfa_upload_size_bytes` | Histogram | | Size of uploaded files |

---

## 6. Concurrency Limits

| Resource | Limit | Rationale |
|----------|-------|-----------|
| Concurrent RTMP connections | 50 (configurable) | Each connection consumes ~4 MB + a Tokio task. 50 × 4 MB = 200 MB. |
| Concurrent active live streams (transcoding) | 10 (configurable) | Each stream with 3 renditions uses ~34 MB + 3 FFmpeg contexts. Limited by CPU. |
| Concurrent HTTP uploads | 10 (configurable) | Each upload streams to disk. Limited by disk I/O. |
| Max pending (unauthenticated) connections | 100 | Prevents SYN flood from exhausting file descriptors. |

**Note:** "RTMP connections" ≠ "active live streams". A connection goes through handshake + auth before becoming an active stream. The connection limit is higher to allow headroom for handshake processing.

---

## 7. Security at Ingest

### 7.1 RTMP Authentication

- Stream key is validated at `publish` command time.
- Keys are stored as bcrypt hashes in configuration (not plaintext).
- Failed auth attempts are rate-limited: after 5 failures from the same IP in 60 seconds, the IP is temporarily blocked for 5 minutes.
- Stream keys can be rotated via config reload (`SIGHUP`) without disconnecting active streams.

### 7.2 HTTP Upload Authentication

- Bearer token in `Authorization` header.
- Tokens are validated against the admin token set (same as control plane auth).
- Failed auth returns `401 Unauthorized` with no information leakage about valid tokens.

### 7.3 Input Sanitization

- All binary input (RTMP packets, uploaded files) is treated as untrusted.
- Buffer reads use explicit length checks to prevent buffer overflows.
- FFmpeg probing of uploaded files runs with resource limits (max probe duration: 10s, max probe size: 10 MB).
- Filenames from multipart uploads are ignored (we generate our own `stream_id`-based names).

---

## Definition of Done — Ingest

- [x] Two ingest paths (RTMP + HTTP upload) designed with clear separation
- [x] RTMP handshake, command processing, and FLV demuxing fully specified
- [x] HTTP upload API with request/response formats and status codes
- [x] Shared validation and demuxing layer documented
- [x] DemuxedFrame data structure defined
- [x] Timestamp normalization rules specified
- [x] Backpressure chain documented for both paths
- [x] Ingest metrics defined
- [x] Concurrency limits justified
- [x] Security measures (auth, input sanitization) documented
- [x] Error handling table for all ingest error scenarios
