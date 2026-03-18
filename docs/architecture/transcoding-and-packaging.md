# StreamInfa — Transcoding and Packaging

> **Purpose:** Detailed design of the transcoding pipeline (FFmpeg FFI, profile ladder, segment production) and HLS packaging (fMP4 muxing, manifest generation).
> **Audience:** Engineers implementing the transcode and package modules; anyone tuning encoding quality or latency.

---

## 1. Transcoding Pipeline Overview

The transcode pipeline sits between ingest and packaging. It receives demuxed elementary streams (H.264 NALUs + AAC frames), decodes them, re-encodes at multiple quality levels, and outputs IDR-aligned segments.

```
                         ┌──────────────────────────────────────────────────┐
                         │            Transcode Pipeline                    │
                         │           (src/transcode/)                  │
                         │                                                  │
  DemuxedFrame ─────────►│  ┌──────────┐   ┌──────────┐   ┌────────────┐  │
  (from ingest,          │  │  Decode   │──►│  Scale   │──►│  Encode    │  │
   mpsc channel)         │  │  (H.264)  │   │ (swscale)│   │  (libx264) │  │──► EncodedSegment
                         │  └──────────┘   └──────────┘   └────────────┘  │    (to packager,
                         │       │              │               │          │     mpsc channel)
                         │       │         ┌────┴────┐    ┌────┴─────┐    │
                         │       │         │  Scale  │    │  Encode  │    │──► EncodedSegment
                         │       │         │  720p   │    │  720p    │    │
                         │       │         └─────────┘    └──────────┘    │
                         │       │              │               │          │
                         │       │         ┌────┴────┐    ┌────┴─────┐    │
                         │       │         │  Scale  │    │  Encode  │    │──► EncodedSegment
                         │       │         │  480p   │    │  480p    │    │
                         │       │         └─────────┘    └──────────┘    │
                         │       │                                        │
                         │  ┌────┴─────┐                                  │
                         │  │  Audio   │  (passthrough or re-encode)      │
                         │  │  Process │──────────────────────────────────►│
                         │  └──────────┘                                  │
                         └──────────────────────────────────────────────────┘
```

**Key design principle:** The decoder runs once. Its raw YUV output is fanned out to N encoders (one per rendition). This avoids decoding the same input N times.

---

## 2. Profile Ladder

### 2.1 Rendition Definitions

| Rendition | Resolution | Video Bitrate | H.264 Profile | H.264 Level | Preset | Audio Bitrate | Audio Codec |
|-----------|-----------|--------------|---------------|-------------|--------|--------------|-------------|
| **high** | 1920×1080 | 3,500 kbps | High | 4.1 | `medium` | 128 kbps | AAC-LC |
| **medium** | 1280×720 | 2,000 kbps | Main | 3.1 | `medium` | 128 kbps | AAC-LC |
| **low** | 854×480 | 1,000 kbps | Main | 3.0 | `medium` | 96 kbps | AAC-LC |

### 2.2 Encoding Parameters (All Renditions)

| Parameter | Value | Justification |
|-----------|-------|---------------|
| **Rate control** | CBR (Constant Bitrate) via `libx264` `-b:v` | Predictable bandwidth for CDN and player buffer calculations |
| **VBV buffer** | 1.5× target bitrate | Allows burst for complex scenes while capping peak rate |
| **VBV max rate** | 1.5× target bitrate | Paired with VBV buffer to bound bitrate spikes |
| **Keyframe interval** | 2 seconds (GOP = FPS × 2) | Ensures IDR every 2s for clean 6s segment boundaries (6s / 2s = 3 GOPs per segment) |
| **B-frames** | 3 | Good compression efficiency. Increases decode latency by 3 frames (~100ms at 30fps). Acceptable for MVP latency targets. |
| **Reference frames** | 4 | Balance between compression and decode complexity |
| **Pixel format** | yuv420p | Universal hardware and software decoder support |
| **x264 preset** | `medium` | Balance between encoding speed and quality. `faster` would save CPU but at noticeable quality loss at our bitrates. `slow` would use 2–3× more CPU for ~5% quality gain. |
| **x264 tune** | (none) | No specific tuning. `zerolatency` removes B-frames and lookahead, hurting quality. Not needed for our latency targets. |
| **Closed GOP** | Yes | Each GOP starts with IDR and is independently decodable. Required for segment-based delivery. |
| **Scene change detection** | Enabled | Inserts IDR on scene cuts for improved quality. Extra IDRs do not break segment alignment because segments are cut at the nearest IDR ≥ 6s from the previous cut. |

### 2.3 Resolution Scaling Rules

- **No upscaling.** If input resolution is 720p, only `medium` (720p) and `low` (480p) are produced. The `high` (1080p) rendition is skipped.
- **Aspect ratio preservation.** Scaling uses `libswscale` with `SWS_LANCZOS` algorithm. If the input aspect ratio differs from the target, we scale to fit within the target dimensions and pad with black bars (pillarbox/letterbox). In MVP, we assume 16:9 input.
  - **[ASSUMPTION]** Input is 16:9. Non-16:9 content is scaled to the target width and height is adjusted to maintain aspect ratio (no black bars). The reported resolution in the manifest reflects the actual output dimensions.
- **Minimum resolution.** Inputs below 480p (width < 854 or height < 480) produce only one rendition at the original resolution. No downscaling below source.

### 2.4 Rendition Selection Logic

```
fn select_renditions(input_width: u32, input_height: u32, ladder: &[Rendition]) -> Vec<Rendition> {
    ladder
        .iter()
        .filter(|r| r.width <= input_width && r.height <= input_height)
        .collect()
    // If empty (input is smaller than all rungs), produce one rendition at input resolution
    // with the lowest rung's bitrate settings
}
```

---

## 3. Decode Stage

### 3.1 Input Processing

The decoder consumes `DemuxedFrame` messages from the ingest→transcode channel:

1. **Initialize FFmpeg decoder** (`avcodec_find_decoder(AV_CODEC_ID_H264)`).
2. **Feed NALUs** via `avcodec_send_packet`. Each `DemuxedFrame` with `track == Video` is wrapped in an `AVPacket`.
3. **Receive decoded frames** via `avcodec_receive_frame`. Output is raw `AVFrame` in yuv420p (or whatever the stream's pixel format is; we convert to yuv420p via swscale if needed).
4. **Fan out** the decoded `AVFrame` reference to all active rendition encoders.

### 3.2 Decoder Error Handling

| Error | Action |
|-------|--------|
| `AVERROR_INVALIDDATA` | Skip frame, log warning, increment `decode_errors_total`. Continue processing. |
| `AVERROR(EAGAIN)` | Normal; decoder needs more input. Continue feeding frames. |
| `AVERROR_EOF` | Stream ended. Flush decoder (`send_packet(NULL)`), then flush encoders. |
| Repeated decode errors (>10 consecutive) | Mark stream as `ERROR`. Likely corrupt or incompatible source. |

### 3.3 Audio Processing

| Input Audio | Action |
|-------------|--------|
| AAC-LC, 128 kbps, stereo | **Passthrough.** No re-encoding. Forward raw AAC frames directly to packager. |
| AAC-LC, other bitrate/channels | **Re-encode** to 128 kbps stereo via `libfdk_aac`. |
| MP3 | **Decode** (MP3 decoder) → **Re-encode** to AAC-LC 128 kbps stereo. |
| No audio track | **Silence.** Do not generate audio. Manifest marks rendition as video-only. |

Audio processing runs in the same `spawn_blocking` task as video transcoding. Audio frames are interleaved with video frames based on PTS ordering.

---

## 4. Encode Stage

### 4.1 Encoder Initialization

For each selected rendition, an FFmpeg encoder context is initialized:

```
Encoder context (per rendition):
  - codec: libx264
  - width: rendition.width
  - height: rendition.height
  - time_base: 1/90000 (90kHz)
  - framerate: source FPS (detected from input, default 30)
  - bit_rate: rendition.bitrate_kbps * 1000
  - rc_max_rate: bit_rate * 1.5
  - rc_buffer_size: bit_rate * 1.5
  - gop_size: framerate * 2
  - max_b_frames: 3
  - pix_fmt: AV_PIX_FMT_YUV420P
  - profile: rendition.profile
  - level: rendition.level
  - flags: AV_CODEC_FLAG_CLOSED_GOP
  - thread_count: 2 (per rendition encoder; 3 renditions × 2 threads = 6 threads per stream)
```

**Thread allocation rationale:** Each x264 encoder instance uses 2 threads for frame-level parallelism. With 3 renditions per stream and 5 concurrent streams, that's 30 encoding threads. On an 8-core machine with hyperthreading (16 logical cores), this saturates CPU. The `max_blocking_threads = 64` Tokio setting provides headroom for task scheduling overhead.

### 4.2 Segment Boundary Detection

Segments are cut at IDR frame boundaries. The logic:

```
segment_accumulator = []
segment_start_pts = None
current_segment_duration = 0.0

for each encoded_packet from encoder:
    if segment_start_pts is None:
        segment_start_pts = packet.pts
    
    segment_accumulator.push(packet)
    current_segment_duration = (packet.pts - segment_start_pts) / 90000.0
    
    if current_segment_duration >= target_segment_duration (6.0s) AND packet.is_keyframe:
        emit EncodedSegment {
            data: concatenate(segment_accumulator),
            duration: current_segment_duration,
            sequence: next_sequence_number(),
            ...
        }
        segment_accumulator = []
        segment_start_pts = None
```

**Why cut at IDR after reaching target duration (not before):**
- Cutting before would produce short segments if IDRs don't align perfectly with the target.
- Cutting at or after ensures segments are at least the target duration.
- With a 2-second keyframe interval, the maximum segment overshoot is ~2 seconds (segment can be 6–8 seconds). This jitter is well within HLS player tolerance.

### 4.3 Encoder Output

Each completed segment is emitted as:

```
EncodedSegment {
    stream_id: StreamId,
    rendition: RenditionId,        // "high", "medium", "low"
    sequence: u64,                  // 0-indexed, monotonically increasing per rendition
    duration_secs: f64,             // Actual segment duration
    pts_start: i64,                 // PTS of first frame (90kHz)
    pts_end: i64,                   // PTS of last frame (90kHz)
    keyframe_pts: i64,              // PTS of the IDR that starts this segment
    video_data: Bytes,              // Encoded H.264 NALUs for this segment
    audio_data: Option<Bytes>,      // Encoded AAC frames for this segment (or None if no audio)
    is_last: bool,                  // True if this is the final segment (stream ended)
}
```

---

## 5. Transcode Pipeline Orchestration

### 5.1 Live vs. VOD Differences

| Aspect | Live | VOD |
|--------|------|-----|
| **Input source** | Continuous `DemuxedFrame` stream from RTMP | Sequential file read from temp file |
| **Processing timing** | Real-time (must keep up with ingest rate) | Offline (can use all available CPU) |
| **Segment emission** | Immediate (each segment sent to packager as soon as ready) | Batch (all segments produced, then sent) |
| **Backpressure concern** | Critical (slow transcode → RTMP backpressure → encoder buffering) | Minimal (file read speed is controlled by transcode) |
| **FFmpeg input** | Frame-by-frame via `send_packet` / `receive_frame` | File-based via `avformat_open_input` + read loop |
| **Thread priority** | Normal (Tokio `spawn_blocking` default) | Normal (same) |

### 5.2 Task Structure (Live)

For each live stream, the transcode pipeline spawns:

```
Stream "abc-123" arrives
    │
    ├── spawn_blocking: Decoder task
    │     Reads from ingest channel
    │     Sends decoded YUV frames to rendition encoders via crossbeam::channel (bounded)
    │
    ├── spawn_blocking: Encoder task (high, 1080p)
    │     Receives YUV frames, scales, encodes, segments
    │     Sends EncodedSegment to packager channel
    │
    ├── spawn_blocking: Encoder task (medium, 720p)
    │     Same as above
    │
    └── spawn_blocking: Encoder task (low, 480p)
          Same as above
```

**Why `crossbeam::channel` between decoder and encoders (not `tokio::sync::mpsc`):**
These tasks all run on blocking threads (not async). Using `crossbeam::channel` avoids the overhead of Tokio's async channel machinery when both sender and receiver are in blocking contexts. The channels are bounded (capacity: 8 frames) to enforce backpressure.

### 5.3 Task Structure (VOD)

For VOD, the entire transcode runs as a single `spawn_blocking` task:

```
spawn_blocking: VOD transcode job
    │
    ├── Open input file with avformat_open_input
    ├── Probe streams (video + audio)
    ├── Initialize decoder
    ├── Initialize N encoders (one per selected rendition)
    ├── Read loop:
    │     Read packet → decode → scale → encode per rendition → segment
    ├── Flush all encoders
    └── Emit all EncodedSegments to packager channel
```

**Progress tracking:** During the read loop, the current PTS is compared against the total duration to compute completion percentage. This is reported to the control plane every second via a `tokio::sync::watch` channel.

---

## 6. HLS Packaging

### 6.1 Package Pipeline

The packager receives `EncodedSegment` messages and produces fMP4 segments + HLS manifests.

```
EncodedSegment ──────► ┌──────────────────────────────────────────┐
(from transcode)       │              Packager                     │
                       │          (src/package/)              │
                       │                                           │
                       │  ┌────────────┐     ┌─────────────────┐  │
                       │  │ fMP4 Muxer │────►│ Storage Writer  │  │──► S3
                       │  │ (segment)  │     │ (segment.m4s)   │  │
                       │  └────────────┘     └─────────────────┘  │
                       │                                           │
                       │  ┌────────────┐     ┌─────────────────┐  │
                       │  │ fMP4 Muxer │────►│ Storage Writer  │  │──► S3
                       │  │ (init seg) │     │ (init.mp4)      │  │
                       │  └────────────┘     └─────────────────┘  │
                       │                                           │
                       │  ┌────────────┐     ┌─────────────────┐  │
                       │  │ Manifest   │────►│ Storage Writer  │  │──► S3
                       │  │ Generator  │     │ (media.m3u8)    │  │
                       │  └────────────┘     └─────────────────┘  │
                       │                                           │
                       │  ┌────────────┐     ┌─────────────────┐  │
                       │  │ Master     │────►│ Storage Writer  │  │──► S3
                       │  │ Playlist   │     │ (master.m3u8)   │  │
                       │  └────────────┘     └─────────────────┘  │
                       └──────────────────────────────────────────┘
```

### 6.2 fMP4 Segment Structure

Each media segment is a valid fMP4 fragment:

```
┌──────────────────────────────────────────┐
│ styp box (segment type)                  │
│   major_brand: "msdh"                    │
│   compatible_brands: ["msdh", "msix"]    │
├──────────────────────────────────────────┤
│ moof box (movie fragment)                │
│   ├── mfhd (sequence_number)             │
│   └── traf (track fragment)              │
│       ├── tfhd (track_id, flags)         │
│       ├── tfdt (baseMediaDecodeTime)     │
│       └── trun (sample_count,            │
│               sample_duration,           │
│               sample_size,               │
│               sample_flags,              │
│               sample_composition_offset) │
├──────────────────────────────────────────┤
│ mdat box (media data)                    │
│   [H.264 NALUs + AAC frames]            │
└──────────────────────────────────────────┘
```

### 6.3 Initialization Segment Structure

Generated once per rendition when the first media segment is ready:

```
┌──────────────────────────────────────────┐
│ ftyp box                                 │
│   major_brand: "isom"                    │
│   compatible_brands: ["isom", "iso6",    │
│                       "mp41"]            │
├──────────────────────────────────────────┤
│ moov box                                 │
│   ├── mvhd (timescale: 90000)            │
│   ├── trak (video)                       │
│   │   ├── tkhd (width, height)           │
│   │   └── mdia                           │
│   │       ├── mdhd (timescale: 90000)    │
│   │       └── stbl (empty sample tables) │
│   │           └── stsd                   │
│   │               └── avc1 (SPS, PPS)    │
│   ├── trak (audio)                       │
│   │   ├── tkhd                           │
│   │   └── mdia                           │
│   │       ├── mdhd (timescale: 48000)    │
│   │       └── stbl                       │
│   │           └── stsd                   │
│   │               └── mp4a (AudioSpecificConfig) │
│   └── mvex                               │
│       ├── trex (video track defaults)    │
│       └── trex (audio track defaults)    │
└──────────────────────────────────────────┘
```

### 6.4 fMP4 Muxing Implementation

**[ASSUMPTION]** We implement fMP4 muxing in pure Rust, not via FFmpeg's `libavformat`. Reason: fMP4 box construction is straightforward byte-level work. Using FFmpeg for muxing would require an additional FFI roundtrip and complex output callback plumbing. A Rust implementation gives us:
- Full control over box layout and metadata.
- No FFI overhead for packaging.
- Easier testing (pure Rust, no C dependency).
- Ability to generate manifests atomically with segments.

The muxer operates on `Bytes` slices containing raw H.264 NALUs and AAC frames. It wraps them in the appropriate MP4 box hierarchy.

**Post-MVP evolution:** If we need DASH or CMAF packaging, the fMP4 muxer is reusable (CMAF segments are identical fMP4 fragments). Only the manifest format changes.

---

## 7. HLS Manifest Generation

### 7.1 Multivariant Playlist (master.m3u8)

Generated once when the first rendition becomes available. Updated if renditions change (rare).

```m3u8
#EXTM3U
#EXT-X-VERSION:7

#EXT-X-STREAM-INF:BANDWIDTH=3628000,RESOLUTION=1920x1080,CODECS="avc1.640029,mp4a.40.2",FRAME-RATE=30.000
high/media.m3u8

#EXT-X-STREAM-INF:BANDWIDTH=2128000,RESOLUTION=1280x720,CODECS="avc1.4d001f,mp4a.40.2",FRAME-RATE=30.000
medium/media.m3u8

#EXT-X-STREAM-INF:BANDWIDTH=1096000,RESOLUTION=854x480,CODECS="avc1.4d001e,mp4a.40.2",FRAME-RATE=30.000
low/media.m3u8
```

**Codec string construction:**
- `avc1.640029` = H.264 High Profile, Level 4.1 (profile_idc=100, constraint_flags=0, level_idc=41)
- `avc1.4d001f` = H.264 Main Profile, Level 3.1
- `avc1.4d001e` = H.264 Main Profile, Level 3.0
- `mp4a.40.2` = AAC-LC

**BANDWIDTH calculation:** Video bitrate + audio bitrate + 10% overhead for container/headers.

### 7.2 Media Playlist (per rendition, media.m3u8)

**Live playlist** (sliding window):

```m3u8
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:8
#EXT-X-MEDIA-SEQUENCE:42
#EXT-X-MAP:URI="init.mp4"

#EXT-X-PROGRAM-DATE-TIME:2025-06-15T14:30:12.000Z
#EXTINF:6.006,
seg_000042.m4s
#EXTINF:6.006,
seg_000043.m4s
#EXTINF:5.972,
seg_000044.m4s
#EXTINF:6.006,
seg_000045.m4s
#EXTINF:6.006,
seg_000046.m4s
```

**VOD playlist** (complete):

```m3u8
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:8
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-MAP:URI="init.mp4"

#EXTINF:6.006,
seg_000000.m4s
#EXTINF:6.006,
seg_000001.m4s
...
#EXTINF:4.238,
seg_000099.m4s
#EXT-X-ENDLIST
```

### 7.3 Manifest Generation Rules

| Rule | Description |
|------|-------------|
| `#EXT-X-TARGETDURATION` | Rounded-up maximum segment duration across all segments. If max segment is 7.2s, TARGETDURATION = 8. |
| `#EXT-X-MEDIA-SEQUENCE` | For live: increments as old segments fall off the sliding window. For VOD: always 0. |
| `#EXT-X-MAP` | Points to the initialization segment. Relative path: `init.mp4`. |
| `#EXTINF` | Actual segment duration as a floating-point number, 3 decimal places. |
| Sliding window (live) | Keep the most recent 5 segments. When segment N+5 is added, segment N is removed from the playlist (but the file remains in storage for a configurable retention period). |
| Manifest write ordering | Always write the segment file to S3 **before** updating the manifest to reference it. This prevents 404s on segment fetch. |
| Atomic manifest update | The manifest is written as a complete file (overwrite). S3 PUT is atomic for objects < 5 GB. |

---

## 8. Segment Index (Live Window Management)

The packager maintains an in-memory segment index per stream per rendition:

```
SegmentIndex {
    stream_id: StreamId,
    rendition: RenditionId,
    segments: VecDeque<SegmentEntry>,  // bounded by live_window_segments
    next_sequence: u64,
    total_segments_produced: u64,
}

SegmentEntry {
    sequence: u64,
    duration_secs: f64,
    storage_path: String,           // e.g., "abc-123/high/seg_000042.m4s"
    program_date_time: DateTime<Utc>,
    size_bytes: u64,
}
```

When a new segment is added:
1. Push to back of `VecDeque`.
2. If `segments.len() > live_window_segments`, pop from front.
3. Regenerate the media playlist from the current `segments`.
4. Write the updated playlist to S3.

Popped segments are **not** immediately deleted from S3. A background cleanup task runs every 5 minutes and deletes segments older than `segment_retention_minutes` (default: 60 minutes).

---

## 9. Transcoding Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `streaminfa_transcode_segments_total` | Counter | `stream_id`, `rendition` | Total segments produced |
| `streaminfa_transcode_segment_duration_seconds` | Histogram | `rendition` | Distribution of actual segment durations |
| `streaminfa_transcode_latency_seconds` | Histogram | `rendition` | Time from first frame input to segment output |
| `streaminfa_transcode_queue_depth` | Gauge | `stream_id` | Current depth of ingest→transcode channel |
| `streaminfa_transcode_fps` | Gauge | `stream_id`, `rendition` | Current encoding frames per second |
| `streaminfa_transcode_active_jobs` | Gauge | | Number of active transcode jobs |
| `streaminfa_transcode_errors_total` | Counter | `stream_id`, `error_type` | Transcode errors by type |
| `streaminfa_package_segments_written_total` | Counter | `stream_id`, `rendition` | Segments written to storage |
| `streaminfa_package_manifest_updates_total` | Counter | `stream_id`, `rendition` | Manifest file updates |
| `streaminfa_vod_progress_percent` | Gauge | `stream_id` | VOD transcode progress (0.0–100.0) |

---

## 10. Error Handling in Transcoding and Packaging

| Error | Stage | Action |
|-------|-------|--------|
| FFmpeg encoder init failure | Encode | Log error with codec params, mark stream as ERROR. Likely a misconfigured profile/level. |
| Encode frame failure | Encode | Skip frame, log warning, increment error counter. If >100 consecutive errors, abort. |
| Resolution mismatch after scale | Scale | Log error, abort rendition. Other renditions continue. |
| fMP4 mux error (invalid NALU) | Package | Skip segment, log error. Player sees a gap (acceptable for transient issues). |
| S3 write failure for segment | Package→Storage | Retry 3× with exponential backoff. If all fail, do not update manifest (segment is lost). Log alert. |
| S3 write failure for manifest | Package→Storage | Retry 3×. If all fail, log critical alert. Live players won't see new segments until next successful write. |
| Disk full (VOD temp file) | Transcode | Abort job, mark stream as ERROR, return 507. |

---

## 11. Quality Trade-offs

| Trade-off | MVP Choice | Alternative | Impact |
|-----------|-----------|-------------|--------|
| **CBR vs CRF** | CBR | CRF (constant quality) | CBR gives predictable bandwidth but wastes bits on simple scenes and under-allocates on complex scenes. CRF gives better perceptual quality but variable bitrate complicates CDN caching. **MVP: CBR for predictability.** |
| **x264 preset** | `medium` | `faster` (2× speed, -10% quality) or `slow` (0.5× speed, +5% quality) | `medium` is the sweet spot for our concurrency target (5 streams × 3 renditions). Going faster frees CPU but drops quality below acceptable for 480p at 1 Mbps. |
| **B-frames** | 3 | 0 (lowest latency) or 5 (best compression) | 3 B-frames add ~100ms decode latency (at 30fps) but improve compression by ~15%. Our latency budget (18s) easily absorbs this. |
| **GOP length** | 2s | 1s (better seek, worse compression) or 4s (better compression, worse seek) | 2s keyframe interval means segments (6s) contain exactly 3 GOPs. 1s would double IDR overhead (~10% bitrate waste). 4s would limit segment boundary options. |
| **Audio passthrough vs re-encode** | Passthrough if AAC-LC | Always re-encode | Passthrough saves CPU and avoids generation loss. Re-encode only when the source is MP3 or non-standard AAC. |

---

## Definition of Done — Transcoding and Packaging

- [x] Transcode pipeline architecture with decode-once, encode-N pattern
- [x] Profile ladder fully specified with all encoding parameters
- [x] Resolution scaling rules (no upscale, aspect ratio handling)
- [x] Rendition selection logic documented
- [x] Decode stage with error handling
- [x] Audio processing logic (passthrough vs re-encode)
- [x] Encode stage with full x264 parameter set
- [x] Segment boundary detection algorithm
- [x] Live vs VOD transcode differences
- [x] Task structure for concurrent rendition encoding
- [x] fMP4 segment and init segment box structures
- [x] fMP4 muxing implementation strategy (pure Rust)
- [x] HLS multivariant and media playlist formats with examples
- [x] Manifest generation rules (TARGETDURATION, sliding window, ordering)
- [x] Segment index for live window management
- [x] All transcode and packaging metrics defined
- [x] Error handling table
- [x] Quality trade-offs explicitly analyzed
