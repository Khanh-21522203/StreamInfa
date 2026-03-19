# StreamInfa — Media Infrastructure Platform MVP

> **Purpose:** Project overview, quick-start orientation, and documentation map.
> **Audience:** All engineers, technical leads, and stakeholders.

---

## What Is StreamInfa?

StreamInfa is a **backend media infrastructure platform** built in Rust. It handles the complete lifecycle of video media: ingesting streams, validating and demuxing containers, transcoding to delivery-ready formats, packaging into HLS segments, storing media assets, and serving them to players or CDNs.

StreamInfa is **not** a video player, a frontend application, a CDN, or a recommendation engine. It is the infrastructure layer that sits between media producers (cameras, encoders, uploaders) and media consumers (players, CDNs, edge caches).

---

## MVP Scope Summary

| Dimension | MVP Choice | Justification |
|-----------|-----------|---------------|
| **Ingestion mode** | Hybrid (Live + VOD) | Live via RTMP; VOD via HTTP multipart upload |
| **Ingest protocols** | RTMP + HTTP upload | RTMP is the de facto live ingest standard; HTTP upload covers VOD with no protocol overhead |
| **Container format** | FLV (RTMP ingest), MP4/MKV (VOD upload) | FLV is native to RTMP; MP4 is the most common upload format |
| **Video codec** | H.264 (AVC) only | Universal decoder support, mature FFmpeg tooling, lowest risk |
| **Audio codec** | AAC-LC only | Pairs with H.264 in every player; simple passthrough or re-encode |
| **Transcoding** | 3-rung profile ladder | 1080p/3.5 Mbps, 720p/2 Mbps, 480p/1 Mbps — covers mobile to desktop |
| **Packaging** | HLS (fMP4 segments) | Broadest device reach; fMP4 is forward-compatible with CMAF |
| **Segment duration** | 6 seconds (live), 6 seconds (VOD) | Balances latency (~18s live glass-to-glass) against CDN cache efficiency |
| **Storage** | S3-compatible object storage | Decouples compute from storage; MinIO for local dev |
| **Delivery** | Origin server (HTTP) | Serves HLS manifests and segments; external CDN pulls from origin |
| **Architecture** | Modular monolith | Single deployable binary with clear internal module boundaries |
| **Async runtime** | Tokio (multi-threaded) | Rust ecosystem standard; excellent for I/O-bound media serving |
| **FFI** | FFmpeg via `ffmpeg-next` crate | Wraps libavcodec/libavformat; all transcoding runs in blocking Tokio tasks |

---

## Documentation Map

### Core Documentation

| Document | Path | Description |
|----------|------|-------------|
| **Requirements** | `docs/requirements.md` | Functional & non-functional requirements, MVP scope, non-goals |
| **Architecture Overview** | `docs/architecture/overview.md` | System-level architecture, module structure, component diagram |
| **Media Lifecycle** | `docs/architecture/media-lifecycle.md` | End-to-end media flow through the system |
| **Ingest** | `docs/architecture/ingest.md` | RTMP and HTTP upload ingest design |
| **Transcoding & Packaging** | `docs/architecture/transcoding-and-packaging.md` | Transcoding pipeline, profile ladder, HLS packaging |
| **Storage & Delivery** | `docs/architecture/storage-and-delivery.md` | Object storage layer, caching, origin server |
| **Control vs Data Plane** | `docs/architecture/control-plane-vs-data-plane.md` | Plane separation, job orchestration, admin APIs |
| **Performance & Backpressure** | `docs/architecture/performance-and-backpressure.md` | Latency targets, buffer strategy, throughput |
| **Security** | `docs/architecture/security.md` | AuthN/AuthZ, input validation, network security |

### Operations & Quality

| Document | Path | Description |
|----------|------|-------------|
| **Testing Strategy** | `docs/testing/testing-strategy.md` | Unit, integration, e2e testing; failure injection |
| **Benchmarking** | `docs/testing/benchmarking.md` | Load-testing scripts and latency/throughput benchmark workflow |
| **Observability** | `docs/observability/observability.md` | Metrics, logging, tracing, SLIs/SLOs |
| **Docker** | `docs/devops/docker.md` | Dockerfile, docker-compose, local dev workflow |
| **Deployment** | `docs/devops/deployment.md` | CI/CD, environment config, rollback strategy |
| **Release Readiness** | `docs/devops/release-readiness.md` | Pre-release command checklist and sign-off gates |
| **Operational Runbook** | `docs/runbooks/runbook.md` | Incident playbooks and deploy/rollback command references |

---

## High-Level System Diagram

```
                        ┌─────────────────────────────────────────────────────┐
                        │                   StreamInfa                        │
                        │                (Modular Monolith)                   │
                        │                                                     │
  RTMP Encoder ────────►│  ┌──────────┐   ┌────────────┐   ┌─────────────┐  │
                        │  │  Ingest   │──►│ Transcode  │──►│  Packager   │  │
  HTTP Upload  ────────►│  │  Service  │   │  Pipeline  │   │   (HLS)     │  │
                        │  └──────────┘   └────────────┘   └──────┬──────┘  │
                        │        │                                 │         │
                        │        ▼                                 ▼         │
                        │  ┌──────────┐                     ┌───────────┐   │
                        │  │ Control  │                     │  Storage   │   │
                        │  │  Plane   │                     │  (S3/     │   │
                        │  │  (API)   │                     │   MinIO)   │   │
                        │  └──────────┘                     └─────┬─────┘   │
                        │                                         │         │
                        │                                   ┌─────┴─────┐   │
                        │                                   │  Origin   │   │
                        │                                   │  Server   │──────► CDN / Player
                        │                                   └───────────┘   │
                        └─────────────────────────────────────────────────────┘
```

---

## Quick Orientation for New Engineers

1. **Read `docs/requirements.md`** to understand what the MVP does and does not cover.
2. **Read `docs/architecture/overview.md`** for the system-level view and module structure.
3. **Read `docs/architecture/media-lifecycle.md`** to trace a video from ingest to playback.
4. **Read `docs/devops/docker.md`** to set up the local development environment.
5. **Read `docs/testing/testing-strategy.md`** before writing any code.

---

## Key Assumptions (MVP)

These assumptions are documented in detail in `docs/requirements.md` and architecture documents. Summary:

- **Single-node deployment.** StreamInfa MVP runs on one machine. Horizontal scaling is a post-MVP concern.
- **FFmpeg is the transcoding engine.** We do not build our own codec implementations. FFmpeg is linked via FFI.
- **S3-compatible storage.** MinIO for local dev; AWS S3 or equivalent in production.
- **No DRM, no ad insertion, no multi-region.** These are explicit non-goals for MVP.
- **H.264 + AAC only.** No HEVC, AV1, VP9, or Opus in MVP.
- **Linux only.** macOS for development is supported but production target is Linux (x86_64).

---

## Definition of Done — README

- [x] Project identity and scope clearly stated
- [x] What StreamInfa IS and IS NOT defined
- [x] MVP scope summary table present
- [x] Full documentation map with paths
- [x] High-level system diagram included
- [x] New-engineer onboarding path defined
- [x] Key assumptions listed
