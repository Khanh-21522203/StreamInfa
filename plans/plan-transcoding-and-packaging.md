# Feature: Transcoding and HLS Packaging

## 1. Purpose

Convert validated input media into an adaptive bitrate ladder and package outputs as HLS with fMP4 segments suitable for player/CDN consumption.

## 2. Responsibilities

- Build rendition ladder (up to 1080p/720p/480p in MVP)
- Avoid upscaling above source resolution
- Segment on IDR boundaries with 6s target duration
- Generate fMP4 init/media segments
- Generate master and per-rendition media playlists

## 3. Non-Responsibilities

- No HEVC/AV1/VP9 pipeline in MVP
- No LL-HLS partial segments in MVP
- No DRM packaging

## 4. Architecture Design

```
Demuxed input frames
  -> decode
  -> per-rendition encode
  -> segment boundary detection
  -> fMP4 mux
  -> manifest update
  -> storage write
```

## 5. Core Data Structures (Rust)

- `RenditionProfile` (name, resolution, bitrate, codec params)
- `EncodedAccessUnit` and segment buffer structs
- `SegmentDescriptor` (`stream_id`, `rendition`, `sequence`, `duration`)
- `ManifestState` (sliding window/live vs endlist/vod mode)

## 6. Public Interfaces

- Internal worker interfaces:
  - `Transcoder::process_frame(...)`
  - `Packager::push_au(...)`
  - `ManifestWriter::update(...)`
- Output artifact contract:
  - `master.m3u8`
  - `{rendition}/media.m3u8`
  - `{rendition}/init.mp4`
  - `{rendition}/seg_{n}.m4s`

## 7. Internal Algorithms

1. Probe source properties and select supported renditions.
2. Initialize encoder contexts per selected rendition.
3. For each incoming frame, decode then fan out encode pipelines.
4. Detect segment cuts at IDR aligned boundaries near target duration.
5. Build fMP4 boxes for init/media segments.
6. Write segment object first, then update/write playlist to preserve ordering.
7. For live mode maintain sliding window; for VOD append endlist at completion.

## 8. Persistence Model

- Segment/manifests persisted in object storage.
- In-memory segment index tracks live window and sequencing.

## 9. Concurrency Model

- Blocking FFmpeg tasks isolated via Tokio `spawn_blocking`
- Per-stream bounded channels between stages
- Per-rendition parallel encode workers with shared cancellation token

## 10. Configuration

- Profile ladder definitions and defaults
- Segment duration target and keyframe interval hints
- Encoder presets/tuning parameters
- Live window size and VOD playlist behavior

## 11. Observability

- Per-stage latency histograms (decode/encode/mux/manifest)
- Segment production counters by rendition
- Transcode failure counters by category
- Queue depth and backpressure visibility between pipeline stages

## 12. Testing Strategy

- Unit: ladder selection, no-upscale logic, manifest generation snapshots
- Integration: FFmpeg-backed transcode correctness for fixtures
- Media validation: ffprobe checks for segment decodability and timing continuity
- E2E: live and VOD flows produce playable HLS outputs

## 13. Implementation Plan

1. Implement rendition selection and encoder initialization path.
2. Implement decode -> encode fanout with bounded channels.
3. Implement IDR-aware segment boundary detector.
4. Implement fMP4 init/media muxing primitives.
5. Implement master/media playlist writers for live and VOD.
6. Wire object-storage writes with manifest ordering guarantees.
7. Add metrics/logging and end-to-end validation with fixtures.

## 14. Open Questions

- Whether MP3 ingest should always be normalized to AAC before pipeline fanout or allowed passthrough in limited cases.
