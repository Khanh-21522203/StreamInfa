# Feature: Control Plane API and Orchestration

## 1. Purpose

Provide authenticated administrative APIs and orchestration hooks that manage stream lifecycle and coordinate data-plane pipelines.

## 2. Responsibilities

- Implement stream CRUD and key-rotation APIs
- Validate API payloads and return typed HTTP errors
- Maintain authoritative stream state for API consumers
- Expose `healthz`, `readyz`, and `metrics`
- Coordinate start/stop/update signals with data-plane components

## 3. Non-Responsibilities

- No direct media processing (decode/encode/mux)
- No player-facing adaptive logic
- No advanced RBAC in MVP

## 4. Architecture Design

Control plane and data plane run in one process but are logically separated.

```
HTTP API (Axum)
   -> auth + validation
   -> state machine + stream registry
   -> orchestration actions
   -> response model
```

## 5. Core Data Structures (Rust)

- `StreamRecord`, `StreamStatus`, `RenditionStatus`
- `PipelineHandle` (channels + cancellation token per stream)
- `ControlPlaneContext` (state map, event receiver, auth config)
- `ApiError` typed mapping to status codes

## 6. Public Interfaces

- `POST /api/v1/streams`
- `GET /api/v1/streams`
- `GET /api/v1/streams/{id}`
- `DELETE /api/v1/streams/{id}`
- `POST /api/v1/streams/{id}/rotate-key`
- `GET /healthz`
- `GET /readyz`
- `GET /metrics`

## 7. Internal Algorithms

1. Authenticate admin bearer token on `/api/v1/*`.
2. For `create stream`, generate UUIDv7 + stream key and initialize state.
3. For `list/get`, project internal record into stable API schema.
4. For `delete`, cancel active pipeline first, then enqueue storage cleanup.
5. For key rotation, atomically swap key in auth registry and keep active sessions alive.
6. For readiness, run dependency checks and return `503` when any required check fails.

## 8. Persistence Model

- MVP: in-memory stream records.
- Keep tombstoned/deleted records for short audit retention.
- Post-MVP: durable control-plane DB.

## 9. Concurrency Model

- Concurrent read/write through `DashMap`
- Bounded `mpsc` event channel from data plane
- Dedicated event-consumer task updates records
- Watch channels for hot-reloaded config values

## 10. Configuration

- Admin tokens / auth settings
- API/HTTP bind addresses
- Readiness thresholds (disk minimum, storage probe timeout)
- Hot-reload policy (which fields are reloadable)

## 11. Observability

- API request metrics by endpoint/status/method
- Stream lifecycle metrics (active, processing, ready, error counts)
- Readiness check latency and failure counters
- Structured audit logs for create/delete/rotate-key actions

## 12. Testing Strategy

- Unit: request validation, auth guard, error mapping
- Integration: API + state transitions + event ingestion
- E2E: create -> ingest -> playback -> delete flow
- Reliability: readiness behavior under storage/FFmpeg failures

## 13. Implementation Plan

1. Build API route skeleton and request/response schemas.
2. Implement auth middleware and shared error model.
3. Implement stream registry/state with transition service.
4. Wire pipeline orchestration hooks (start/stop) and event listener.
5. Implement rotate-key behavior and key registry reload path.
6. Implement health/readiness probes and readiness payload.
7. Add metrics instrumentation and endpoint-level integration tests.

## 14. Open Questions

- Whether pagination should be offset-based only or support cursor mode before Phase 2.
- Whether control APIs and origin APIs should be split to separate ports in post-MVP.
