# StreamInfa — Media Lifecycle

> **Purpose:** Trace the end-to-end journey of media through StreamInfa, from ingest to playback, for both live and VOD paths.
> **Audience:** Engineers implementing pipeline stages; architects validating data flow.

---

## 1. Lifecycle Overview

Every piece of media that enters StreamInfa follows a deterministic pipeline. The pipeline is the same conceptual flow for live and VOD, but the timing, buffering, and manifest behavior differ.

```
┌──────────────────────────────────────────────────────────────────────────┐
│                        MEDIA LIFECYCLE                                   │
│                                                                          │
│  ┌─────────┐   ┌──────────┐   ┌───────────┐   ┌─────────┐   ┌───────┐  │
│  │ INGEST  │──►│ VALIDATE │──►│ TRANSCODE │──►│ PACKAGE │──►│ STORE │  │
│  │         │   │ & DEMUX  │   │           │   │  (HLS)  │   │ (S3)  │  │
│  └─────────┘   └──────────┘   └───────────┘   └─────────┘   └───┬───┘  │
│                                                                   │      │
│                                                              ┌────┴───┐  │
│                                                              │DELIVER │  │
│                                                              │(Origin)│  │
│                                                              └────────┘  │
└──────────────────────────────────────────────────────────────────────────┘
```

---

## 2. Stream State Machine

Every stream (live or VOD) has a lifecycle managed by the control plane:

```
                    ┌──────────┐
          create    │          │   RTMP connect / upload start
     ──────────────►│ PENDING  │──────────────────────────┐
                    │          │                           │
                    └──────────┘                           ▼
                                                    ┌───────────┐
                                                    │           │
                                                    │   LIVE    │ (live streams only)
                                                    │           │
                                                    └─────┬─────┘
                                                          │ stream ends / upload complete
                                                          ▼
                                                    ┌───────────┐
                                                    │           │
                                                    │PROCESSING │ (transcoding + packaging)
                                                    │           │
                                                    └─────┬─────┘
                                                          │ all renditions packaged
                                                          ▼
                                                    ┌───────────┐
                                                    │           │
                                                    │   READY   │ (playable)
                                                    │           │
                                                    └─────┬─────┘
                                                          │ admin delete
                                                          ▼
                                                    ┌───────────┐
                                                    │  DELETED  │
                                                    └───────────┘

     At any point, an unrecoverable error transitions to:
                                                    ┌───────────┐
                                                    │   ERROR   │
                                                    └───────────┘
```

**State definitions:**

| State | Description | Live | VOD |
|-------|-------------|------|-----|
| `PENDING` | Stream key created, awaiting ingest | Yes | Yes |
| `LIVE` | Actively receiving and processing media in real time | Yes | No (skip) |
| `PROCESSING` | Transcoding and packaging in progress (VOD post-upload, or live post-disconnect finalizing) | Yes (briefly) | Yes |
| `READY` | All renditions available for playback | Yes (after end) | Yes |
| `ERROR` | Unrecoverable failure occurred | Yes | Yes |
| `DELETED` | Assets removed, metadata retained for audit | Yes | Yes |

**Key difference:** Live streams enter `LIVE` state and are simultaneously ingesting, transcoding, packaging, and delivering. VOD streams go `PENDING` → `PROCESSING` → `READY` without a `LIVE` phase.

---

## 3. Live Stream Lifecycle (Detailed)

### 3.1 Timeline

```
Time ──────────────────────────────────────────────────────────────────►

Encoder:   [connect]──[publish]──[frame]──[frame]──...──[disconnect]
               │          │         │                        │
Ingest:        │    [auth+validate] [demux NALUs]           [drain]
               │          │         │                        │
Transcode:     │          │    [decode+encode per GOP]      [flush]
               │          │              │                    │
Packager:      │          │         [segment every 6s]      [finalize m3u8]
               │          │              │                    │
Storage:       │          │         [PUT segment to S3]     [PUT final manifest]
               │          │              │                    │
Delivery:      │          │         [manifest available]    [EXT-X-ENDLIST]
               │          │              │
Player:        │          │         [fetches m3u8, fetches segments, plays]
```

### 3.2 Step-by-Step

**Step 1: RTMP Connection & Authentication**
- Encoder connects to `rtmp://{host}:1935/live/{stream_key}`.
- StreamInfa performs RTMP handshake (C0/C1/C2 exchange).
- On `connect` command, StreamInfa extracts the stream key from the URL path.
- Stream key is validated against the configured key set. Invalid key → RTMP error + disconnect.
- On success, state transitions from `PENDING` to `LIVE`.

**Step 2: FLV Demuxing & Validation**
- RTMP carries media in FLV tags (type 0x08 = audio, 0x09 = video).
- StreamInfa reads each FLV tag, extracts:
  - **Video:** H.264 NALUs with AVC sequence header (SPS/PPS) + coded slices.
  - **Audio:** AAC raw frames with AudioSpecificConfig.
- Validation checks:
  - Video codec must be H.264 (FLV CodecID = 7). Other codecs → reject with log + metric.
  - Audio codec must be AAC (FLV SoundFormat = 10) or MP3 (SoundFormat = 2). MP3 is accepted but will be re-encoded to AAC.
  - SPS must indicate a valid resolution (>0×0, ≤3840×2160).
  - If no keyframe arrives within 10 seconds of stream start → timeout error.

**Step 3: Frame Dispatch to Transcode Pipeline**
- Each validated frame (H.264 NALU or AAC frame) is wrapped in a `DemuxedFrame` message:
  ```
  DemuxedFrame {
      stream_id: StreamId,
      track: Video | Audio,
      pts: i64,          // presentation timestamp (90kHz timebase)
      dts: i64,          // decode timestamp
      keyframe: bool,    // IDR for video
      data: Bytes,       // NALU or raw AAC frame
  }
  ```
- Sent to the transcode pipeline via a bounded `mpsc` channel (capacity: 64).
- If the channel is full, the send awaits → TCP read pauses → backpressure propagates to the encoder.

**Step 4: Transcoding (Per Rendition)**
- The transcode pipeline spawns one FFmpeg context per rendition (up to 3 for our ladder).
- Each context runs in a `spawn_blocking` task.
- Input frames are decoded from H.264 to raw YUV420p.
- For each rendition:
  - Scale to target resolution using `libswscale`.
  - Encode with `libx264` at the target bitrate and profile.
  - Audio is passthrough if already AAC-LC at acceptable bitrate, otherwise re-encoded via `libfdk_aac`.
- Output is segmented at IDR boundaries, targeting 6-second segments.
- Each completed segment is wrapped as:
  ```
  EncodedSegment {
      stream_id: StreamId,
      rendition: RenditionId,  // "high", "medium", "low"
      sequence: u64,           // monotonically increasing
      duration: f64,           // actual duration in seconds
      keyframe_pts: i64,       // PTS of the first IDR in the segment
      data: Bytes,             // raw H.264 + AAC elementary stream data
  }
  ```

**Step 5: HLS Packaging**
- The packager receives `EncodedSegment` and wraps it into an fMP4 container:
  - Generates an initialization segment (`init.mp4`) on the first segment of each rendition (contains `ftyp`, `moov` boxes).
  - Each media segment becomes an fMP4 fragment (`styp`, `moof`, `mdat` boxes).
- Manifest generation:
  - **Media playlist** (per rendition): Updated with each new segment. For live, maintains a sliding window of 5 segments. Tags include `#EXTINF`, `#EXT-X-MAP` (pointing to init segment), and `#EXT-X-MEDIA-SEQUENCE`.
  - **Multivariant playlist**: Lists all renditions with bandwidth, resolution, and codecs. Generated once and updated only if renditions change.

**Step 6: Storage Write**
- Segments and manifests are written to S3 (object keys, not HTTP routes):
  ```
  /{stream_id}/high/init.mp4
  /{stream_id}/high/seg_000001.m4s
  /{stream_id}/high/seg_000002.m4s
  /{stream_id}/high/media.m3u8
  /{stream_id}/medium/...
  /{stream_id}/low/...
  /{stream_id}/master.m3u8
  ```
- Writes are fire-and-await with retry logic (3 retries, exponential backoff).
- A segment is only referenced in the manifest after a successful storage write.

**Step 7: Delivery**
- Players request `GET /streams/{stream_id}/master.m3u8` from the origin server.
- Origin reads the manifest from S3 (with LRU cache for hot manifests).
- Player then requests segments listed in the media playlist.
- Live manifests have `Cache-Control: no-cache, no-store` (players must re-fetch to discover new segments).

**Step 8: Stream End**
- Encoder disconnects (or StreamInfa detects inactivity timeout of 30 seconds).
- Transcode pipeline flushes any buffered frames.
- Packager writes the final segment and adds `#EXT-X-ENDLIST` to all media playlists.
- State transitions: `LIVE` → `PROCESSING` (briefly, while flushing) → `READY`.

---

## 4. VOD Lifecycle (Detailed)

### 4.1 Timeline

```
Time ──────────────────────────────────────────────────────────────────►

Client:    [POST /api/v1/streams/upload]──[multipart body]──[complete]
               │                              │                  │
Ingest:   [auth+create stream]         [write to temp file]  [validate]
               │                              │                  │
Transcode:     │                              │            [full-file transcode]
               │                              │                  │
Packager:      │                              │            [generate all segments + manifests]
               │                              │                  │
Storage:       │                              │            [PUT all to S3]
               │                              │                  │
Control:       │                              │            [state → READY]
```

### 4.2 Step-by-Step

**Step 1: HTTP Upload**
- Client sends `POST /api/v1/streams/upload` with:
  - `Authorization: Bearer {token}` header.
  - Multipart form body with a file field containing the video.
- StreamInfa authenticates the bearer token.
- A new `stream_id` is generated (UUIDv7 for time-sortability).
- State: `PENDING`.

**Step 2: Temp File Write & Validation**
- The upload body is streamed to a temporary file on local disk (not held in memory).
  - **[ASSUMPTION]** Temp disk has at least 50 GB free for concurrent uploads.
- After upload completes, the temp file is validated:
  - Probe with FFmpeg to detect container format, codecs, duration, resolution.
  - Reject if: not MP4/MKV, not H.264 video, duration >6 hours (configurable), resolution >3840×2160.
  - On rejection: delete temp file, return `422 Unprocessable Entity` with error details.

**Step 3: Transcoding**
- The temp file is passed to the transcode pipeline as a single job.
- Unlike live (which processes frame-by-frame in real time), VOD transcoding reads the entire file sequentially.
- FFmpeg processes the file and outputs IDR-aligned segments for each rendition.
- Progress is tracked as percentage (current PTS / total duration) and exposed via the control API.

**Step 4: Packaging & Storage**
- Identical to live: fMP4 segments + HLS manifests.
- All segments are written to S3 before the manifest is written (ensures no dangling references).
- VOD manifests include `#EXT-X-ENDLIST` (the playlist is complete).

**Step 5: Cleanup & Readiness**
- Temp file is deleted after all renditions are stored.
- State: `PROCESSING` → `READY`.
- The stream is now playable via `GET /streams/{stream_id}/master.m3u8`.

---

## 5. Data Format at Each Stage

```
Stage           │ Data Format           │ Container │ Encoding
────────────────┼───────────────────────┼───────────┼──────────────
RTMP Ingest     │ FLV tags              │ FLV       │ H.264 + AAC
HTTP Upload     │ File bytes            │ MP4/MKV   │ H.264 + AAC
After Demux     │ Elementary streams    │ None      │ H.264 NALUs + AAC frames
After Transcode │ Encoded segments      │ None      │ H.264 (re-encoded) + AAC
After Packaging │ fMP4 segments + m3u8  │ fMP4      │ H.264 + AAC
In Storage      │ Object blobs          │ fMP4      │ H.264 + AAC
Delivery        │ HTTP response bodies  │ fMP4      │ H.264 + AAC
```

---

## 6. Timing Analysis (Live Path)

| Stage | Duration | Cumulative | Notes |
|-------|----------|-----------|-------|
| Encoder → RTMP wire | ~500ms | 500ms | Depends on encoder settings |
| RTMP receive + demux | ~200ms | 700ms | TCP read + FLV parsing |
| Queue: ingest → transcode | 0–500ms | 1.2s | Depends on channel occupancy |
| Transcode (1 GOP = 2s) | ~2s | 3.2s | Must buffer full GOP before encoding |
| Queue: transcode → packager | 0–200ms | 3.4s | |
| fMP4 packaging | ~50ms | 3.5s | Lightweight muxing |
| S3 write | ~200ms | 3.7s | Local MinIO; production S3 may be 50–500ms |
| Manifest update + S3 write | ~100ms | 3.8s | |
| **Segment available** | | **~4s** | Time from frame capture to segment availability |
| Player polls manifest | 0–6s | 4–10s | Players poll at segment interval |
| Player buffer | 2–6s | 6–16s | Depends on player buffer settings |
| **Glass-to-glass estimate** | | **~12–18s** | Encoder-to-screen |

**Key insight:** The dominant contributor to live latency is the segment duration (6s). Reducing segment duration to 2s would cut glass-to-glass to ~6–10s but increases CDN request rate 3×. This is a Phase 2 optimization (LL-HLS with partial segments).

---

## 7. Error Scenarios and Recovery

| Scenario | Detection | Response | State Transition |
|----------|-----------|----------|-----------------|
| RTMP disconnect mid-stream | TCP RST / timeout | Flush buffered frames, finalize segments, add `#EXT-X-ENDLIST` | LIVE → PROCESSING → READY |
| Invalid codec in stream | Demux validation | Disconnect RTMP with error, log, metric | LIVE → ERROR |
| FFmpeg crash (SIGSEGV in FFI) | `JoinHandle` returns `Err` | Log panic, mark stream as error, clean up temp artifacts | LIVE/PROCESSING → ERROR |
| S3 write failure (all retries exhausted) | Storage client returns error | Mark stream as error, retain local segments for manual recovery | LIVE/PROCESSING → ERROR |
| Upload timeout (HTTP body stalls for >60s) | Axum request timeout | Abort upload, delete temp file, return 408 | PENDING → ERROR |
| Duplicate stream key (two encoders) | Second `publish` command | Reject second connection with RTMP error, keep first | No change |
| Out of disk space (temp file) | `std::io::Error(NoSpace)` | Reject upload, return 507, emit alert metric | PENDING → ERROR |

---

## 8. Idempotency and Resumption

### MVP Scope
- **No resumable uploads.** If an HTTP upload fails partway, the client must re-upload. This is acceptable for MVP because uploads are atomic (multipart form, not chunked resumable).
- **No resumable live streams.** If an RTMP connection drops, the encoder reconnects and starts a new stream (new `stream_id`) or reconnects to the same key (which creates a continuation in the same `stream_id` with a gap in segments).
- **Segment writes are idempotent.** Writing the same segment path to S3 twice produces the same result (S3 PUT is idempotent by nature).

### Post-MVP Evolution
- Phase 2: Resumable uploads via tus protocol or S3 multipart upload.
- Phase 2: Live stream reconnect within a grace period (30s) → continuation of the same `stream_id` with segment gap handling.

---

## Definition of Done — Media Lifecycle

- [x] End-to-end pipeline diagram for both live and VOD
- [x] Stream state machine with all states and transitions
- [x] Live lifecycle detailed step-by-step with data formats
- [x] VOD lifecycle detailed step-by-step with data formats
- [x] Data format at each pipeline stage documented
- [x] Timing analysis for live path with cumulative latency
- [x] Error scenarios with detection, response, and state transitions
- [x] Idempotency and resumption behavior documented
- [x] Future evolution paths noted
