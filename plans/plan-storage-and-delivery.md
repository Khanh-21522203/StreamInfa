# Feature: Object Storage, Caching, and Origin Delivery

## 1. Purpose

Persist HLS artifacts in S3-compatible storage and serve them through HTTP origin endpoints optimized for CDN and direct playback access.

## 2. Responsibilities

- Implement object-storage read/write/delete operations
- Enforce storage write ordering rules (segment before manifest reference)
- Define and apply canonical object path scheme
- Implement origin endpoints under `/streams/{stream_id}/...`
- Support byte-range requests for `.m4s` and `.mp4`
- Provide in-memory cache for hot manifests/segments

## 3. Non-Responsibilities

- No built-in CDN implementation
- No multi-region replication in MVP
- No policy-driven storage tiering in MVP

## 4. Architecture Design

```
Packager output
  -> storage client (S3-compatible)
  -> object namespace per stream/rendition
  -> origin HTTP handlers
  -> optional in-memory LRU cache
  -> client/CDN
```

## 5. Core Data Structures (Rust)

- `StorageClient` abstraction (`put`, `get`, `delete_prefix`, `head`)
- `ObjectKey` path builder helpers
- `CacheEntry` and LRU index structures
- `RangeSpec` parser for partial-content responses

## 6. Public Interfaces

- Delivery endpoints:
  - `GET /streams/{id}/master.m3u8`
  - `GET /streams/{id}/{rendition}/media.m3u8`
  - `GET /streams/{id}/{rendition}/init.mp4`
  - `GET /streams/{id}/{rendition}/seg_{n}.m4s`
- Internal storage API:
  - `put_object`, `get_object`, `delete_prefix`, `list_prefix`

## 7. Internal Algorithms

1. Build object keys from stream id + rendition + artifact kind.
2. On write, use retry policy for transient backend errors.
3. On read, check LRU cache first for eligible object classes.
4. On cache miss, fetch from storage and decide cache admission.
5. Parse and honor valid `Range` headers for media objects.
6. Return typed HTTP errors for not found, invalid range, and backend failures.
7. On delete stream, recursively remove stream prefix objects.

## 8. Persistence Model

- Durable source of truth is object storage backend.
- Cache is transient and safely disposable.

## 9. Concurrency Model

- Async storage client with bounded in-flight operations
- Thread-safe LRU cache with controlled memory budget
- Background cleanup task for retention policies

## 10. Configuration

- S3 endpoint/bucket/credentials/region/path-style mode
- Retry counts/backoff settings
- Cache capacity/eviction policy and object eligibility
- Response cache-control policy for live vs VOD

## 11. Observability

- Storage operation latency and error counters by operation
- Cache hit/miss/eviction metrics
- Origin request metrics by route/status/content type
- Range-request usage metrics and 416 counters

## 12. Testing Strategy

- Unit: object-key scheme, range parser, cache behavior
- Integration: MinIO put/get/delete and retry paths
- E2E: playback endpoints for live and VOD artifacts
- Failure injection: storage outage and high latency scenarios

## 13. Implementation Plan

1. Implement `StorageClient` abstraction and concrete S3 client wrapper.
2. Implement object key builder with strict path contract.
3. Implement retry/backoff and typed backend error classification.
4. Implement LRU cache and cache policy for manifests/segments.
5. Implement origin handlers and response-header policy.
6. Add range support and content-type handling.
7. Add cleanup worker and integration coverage with MinIO.

## 14. Open Questions

- Whether live segments should be cacheable in-process by default or cache manifests only to reduce memory pressure.
