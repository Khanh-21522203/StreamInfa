# Feature: VOD Ingest via HTTP Upload

## 1. Purpose

Accept VOD media files over authenticated multipart HTTP upload, validate media constraints, and feed valid inputs to the processing pipeline.

## 2. Responsibilities

- Implement upload endpoint `POST /api/v1/streams/upload`
- Authenticate with control-plane bearer token
- Enforce upload size/type constraints
- Run probe/validation (container + codecs + basic sanity)
- Stage accepted files for transcode processing

## 3. Non-Responsibilities

- No resumable upload protocol in MVP
- No client-side upload SDK concerns
- No multi-part chunk resume/retry at application layer

## 4. Architecture Design

```
HTTP multipart request
  -> auth + request validation
  -> stream body to temp file
  -> probe media info
  -> create stream record (PROCESSING)
  -> handoff to transcode pipeline
```

## 5. Core Data Structures (Rust)

- `UploadRequest` metadata and file-part descriptor
- `UploadConstraints` (max bytes, allowed containers/codecs)
- `UploadJob` (stream id, temp path, media info)
- `UploadError` variants (`TooLarge`, `UnsupportedCodec`, `MalformedMedia`, `DiskFull`)

## 6. Public Interfaces

- `POST /api/v1/streams/upload` (multipart/form-data)
- `GET /api/v1/streams/{id}` for progress/status polling

## 7. Internal Algorithms

1. Validate auth and multipart structure.
2. Stream body to temp file without full in-memory buffering.
3. Enforce byte limit while streaming; abort with 413 when exceeded.
4. Run media probe and codec validation.
5. Reject unsupported/corrupt files with typed client errors.
6. Create processing stream record and dispatch upload job.
7. Cleanup temp file on completion or failure path.

## 8. Persistence Model

- Temp file on local disk during ingest/transcode kickoff
- Final artifacts stored in object storage by downstream module

## 9. Concurrency Model

- Bounded concurrent upload workers
- Backpressure through request body streaming and disk I/O limits
- Job handoff to pipeline via queue/event

## 10. Configuration

- Max upload bytes
- Temp directory path and minimum free-space threshold
- Concurrent upload limit
- Probe timeout and allowed container list

## 11. Observability

- Upload request count, accepted/rejected by reason
- Upload bytes total and upload duration histogram
- Temp disk pressure metrics and `507` error counters
- Structured logs with `stream_id`, `content_length`, `reject_reason`

## 12. Testing Strategy

- Unit: multipart parser/validator and limit enforcement
- Integration: upload real fixtures (valid, corrupt, unsupported codec)
- E2E: upload -> processing -> ready -> playback
- Failure: low disk space, interrupted upload, probe timeout

## 13. Implementation Plan

1. Build upload route and request parser with auth middleware.
2. Implement streaming writer to temp storage with byte-limit guard.
3. Implement probe/codec validation and typed error mapping.
4. Create stream record in `PROCESSING` and enqueue transcode job.
5. Implement temp-file lifecycle cleanup hooks.
6. Add upload metrics/logs and API contract tests.
7. Validate VOD E2E flow against requirements acceptance criteria.

## 14. Open Questions

- Whether minimum-free-disk threshold should be hard fail pre-check or continuously enforced while streaming.
