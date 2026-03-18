# Feature: Media Lifecycle Orchestration

## 1. Purpose

Define and implement the end-to-end lifecycle for both live and VOD streams so every stream moves predictably from ingest to playback readiness (or error/deleted states).

## 2. Responsibilities

- Define canonical states: `PENDING`, `LIVE`, `PROCESSING`, `READY`, `ERROR`, `DELETED`
- Define valid state transitions and side effects
- Model lifecycle for live RTMP flow and VOD upload flow
- Define cleanup behavior and idempotent delete semantics
- Define recovery behavior for expected failures

## 3. Non-Responsibilities

- Not responsible for codec internals (transcoding module)
- Not responsible for storage implementation details (storage module)
- Not responsible for long-term durable state in MVP (in-memory only)

## 4. Architecture Design

Lifecycle orchestration is owned by control-plane logic and updated from data-plane events.

```
Control API + Ingest Events + Pipeline Events
                |
                v
         Lifecycle State Machine
                |
                v
   Side effects: start/stop pipeline, cleanup, status expose
```

## 5. Core Data Structures (Rust)

- `StreamStatus` enum
- `StreamRecord` with timestamps, ingest type, rendition progress, error info
- `PipelineEvent` (`StreamStarted`, `SegmentProduced`, `StreamEnded`, `StreamError`, `VodProgress`)
- `IngestType` (`Rtmp`, `Upload`)

## 6. Public Interfaces

- Lifecycle is externally visible via:
  - `GET /api/v1/streams`
  - `GET /api/v1/streams/{id}`
- Internal interface:
  - `apply_transition(stream_id, event) -> Result<NewState, TransitionError>`

## 7. Internal Algorithms

1. Initialize new stream in `PENDING`.
2. On RTMP publish success: `PENDING -> LIVE`.
3. On upload start: `PENDING -> PROCESSING`.
4. On live disconnect: `LIVE -> PROCESSING` (flush/finalize).
5. On successful packaging completion: `PROCESSING -> READY`.
6. On unrecoverable failure: `* -> ERROR`.
7. On delete request: `* -> DELETED` with pipeline cancellation and storage cleanup.
8. Reject invalid transitions with `409 Conflict`.

## 8. Persistence Model

- MVP: in-memory stream records only.
- Object storage remains durable even if service restarts.
- Phase 2: move state to SQLite/PostgreSQL.

## 9. Concurrency Model

- Shared stream map: `DashMap<StreamId, StreamRecord>`
- Single event-consumer task applies transitions serially per event arrival
- Per-stream cancellation token controls active data-plane tasks

## 10. Configuration

- Transition timeouts and retention windows:
  - deleted-record retention window (24h in docs)
  - polling intervals for progress/reporting

## 11. Observability

- Counters for transition attempts, invalid transitions, and terminal errors
- Structured logs with fields: `stream_id`, `old_state`, `new_state`, `reason`
- Alert on abnormal growth of streams in `ERROR` or long-lived `PROCESSING`

## 12. Testing Strategy

- Unit tests for valid/invalid transition matrix
- Integration tests verifying live and VOD timelines
- E2E tests verifying full progression and delete behavior
- Chaos tests for disconnect/storage errors and idempotent retries

## 13. Implementation Plan

1. Implement `StreamStatus`, `StreamRecord`, and transition validation table.
2. Build lifecycle service with atomic transition helper and side-effect hooks.
3. Wire lifecycle updates from ingest/transcode/packaging event channel.
4. Add API serialization for lifecycle fields and progress details.
5. Add deletion path with cancel + storage cleanup + retention marker.
6. Add metrics/logging and transition audit coverage.
7. Validate with lifecycle unit/integration/E2E suites.

## 14. Open Questions

- Whether to expose partial progress details for `LIVE` and `PROCESSING` in one unified schema.
- Whether retention of `DELETED` records should be configurable at runtime.
