# StreamInfa - Roadmap

> **Purpose:** Describe post-MVP evolution priorities.
> **Audience:** Engineers and product/technical stakeholders planning upcoming phases.

---

## Phase 1 - MVP (Current Target)

1. RTMP live ingest + HTTP VOD upload.
2. H.264/AAC-only pipeline.
3. 3-rung adaptive ladder and HLS fMP4 output.
4. S3-compatible storage and origin delivery.
5. Auth for ingest/control plane.
6. Baseline observability and CI quality gates.

---

## Phase 2 - Reliability and Scale

1. Durable control-plane state (SQLite/PostgreSQL).
2. Playback auth (signed URLs / token validation).
3. LL-HLS support with partial segments.
4. Resumable VOD uploads (tus or multipart resume).
5. Better autoscaling story for transcoding workload.
6. Runtime configuration management and safer secret rotation.

---

## Phase 3 - Enterprise Features

1. RBAC and tenant isolation.
2. Multi-region architecture and failover strategy.
3. Advanced analytics and QoE dashboards.
4. DRM integration and policy-driven delivery.
5. Cost-aware storage tiering and lifecycle automation.

---

## Deferred / Explicitly Out of Near-Term Scope

1. WebRTC ingest path.
2. AV1/HEVC transcoding in default pipeline.
3. Built-in CDN functionality.
4. In-product player UI.

