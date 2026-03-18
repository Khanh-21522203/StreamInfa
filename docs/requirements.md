# StreamInfa - MVP Requirements

> **Purpose:** Define what must be built for MVP and what is explicitly out of scope.
> **Audience:** Engineers implementing StreamInfa from scratch, reviewers validating scope.

---

## 1. Product Goal

Build a Rust backend service that ingests live and VOD media, transcodes to an adaptive ladder, packages to HLS (fMP4), stores artifacts in S3-compatible storage, and serves playback assets through an HTTP origin.

---

## 2. MVP Functional Requirements

### 2.1 Ingest

1. Support live ingest over RTMP (`:1935`) with stream-key authentication.
2. Support VOD ingest over HTTP multipart upload (`POST /api/v1/streams/upload`).
3. Accept only H.264 video and AAC/MP3 audio at ingest time.
4. Reject unsupported codecs, malformed media, and oversize uploads with typed errors.

### 2.2 Control Plane API

1. Provide authenticated admin APIs for stream lifecycle:
   - `POST /api/v1/streams`
   - `GET /api/v1/streams`
   - `GET /api/v1/streams/{id}`
   - `DELETE /api/v1/streams/{id}`
   - `POST /api/v1/streams/{id}/rotate-key`
2. Expose health/readiness endpoints:
   - `GET /healthz`
   - `GET /readyz`
3. Expose Prometheus metrics at `GET /metrics`.

### 2.3 Transcoding and Packaging

1. Produce an adaptive HLS ladder (max 3 renditions):
   - 1080p @ ~3.5 Mbps
   - 720p @ ~2.0 Mbps
   - 480p @ ~1.0 Mbps
2. Do not upscale above source resolution.
3. Cut segments at IDR boundaries with target duration 6 seconds.
4. Generate valid fMP4 init/media segments and HLS playlists:
   - `master.m3u8`
   - `{rendition}/media.m3u8`
   - `{rendition}/init.mp4`
   - `{rendition}/seg_*.m4s`

### 2.4 Storage and Delivery

1. Persist media artifacts to an S3-compatible backend.
2. Ensure manifest update ordering: segment write must succeed before playlist references it.
3. Serve playback via origin endpoints under `/streams/{stream_id}/...`.
4. Support byte-range requests for `.m4s`/`.mp4`.

### 2.5 Security

1. Require bearer auth for all `/api/v1/*` control endpoints.
2. Require stream-key auth for RTMP publish.
3. Redact sensitive fields from logs.
4. Enforce input size and codec validation to reduce abuse and crash risk.

### 2.6 Observability

1. Emit structured logs (JSON in production).
2. Emit component metrics for ingest, transcode, package, storage, delivery, and backpressure.
3. Provide enough telemetry to debug per-stream failures and system-wide bottlenecks.

---

## 3. MVP Non-Functional Requirements

### 3.1 Performance Targets

1. Live glass-to-glass latency target: `<= 18s` under nominal load.
2. VOD processing target: `<= 2x` source duration for 1080p inputs (p95).
3. Support at least 5 concurrent live streams (3 renditions each) on the reference machine profile.

### 3.2 Reliability Targets

1. Availability SLO target: `99.5%` monthly (MVP baseline).
2. Process must not crash on malformed input; fail stream/job, not the service.
3. Backpressure must be explicit and bounded (no unbounded queues in media path).

### 3.3 Operational Constraints

1. Single-node deployment for MVP.
2. Linux x86_64 as production target.
3. FFmpeg 6.x dependency is required.

---

## 4. Explicit Non-Goals (MVP)

1. DRM (Widevine/FairPlay/PlayReady).
2. LL-HLS / partial segments.
3. SRT/WebRTC ingest.
4. Playback authorization (signed URLs).
5. Multi-region replication and failover.
6. Horizontal scaling across multiple ingest/transcode workers.

---

## 5. External Dependencies

1. FFmpeg (runtime and dev/test usage).
2. S3-compatible storage (MinIO in local/dev, S3-compatible service in production).
3. Prometheus-compatible metrics scraping.
4. Optional: Grafana and log collector for operations.

---

## 6. Acceptance Criteria (Definition of MVP Complete)

1. Live flow works end-to-end:
   - Create stream -> push RTMP -> playback via `/streams/{id}/master.m3u8`.
2. VOD flow works end-to-end:
   - Upload MP4 -> transcode/package -> playback -> delete stream assets.
3. API contracts match architecture docs and return specified status codes.
4. Security checks are enforced (auth + validation).
5. Observability endpoints/logging/metrics are present and usable.
6. Test strategy gates run in CI (unit + integration + e2e).

---

## 7. Canonical Design References

1. `docs/architecture/overview.md`
2. `docs/architecture/media-lifecycle.md`
3. `docs/architecture/ingest.md`
4. `docs/architecture/transcoding-and-packaging.md`
5. `docs/architecture/storage-and-delivery.md`
6. `docs/architecture/control-plane-vs-data-plane.md`
7. `docs/architecture/performance-and-backpressure.md`
8. `docs/architecture/security.md`
9. `docs/testing/testing-strategy.md`
10. `docs/observability/observability.md`

