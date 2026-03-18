# StreamInfa — Storage and Delivery

> **Purpose:** Design of the storage layer (S3-compatible object storage, caching, path scheme) and the HTTP origin server (segment serving, manifest delivery, CDN interaction).
> **Audience:** Engineers implementing the storage and delivery modules; ops engineers configuring storage backends.

---

## 1. Storage Architecture

```
                    ┌──────────────────────────────────────────────────┐
                    │             Storage Layer                        │
                    │           (src/storage/)                    │
                    │                                                  │
  Packager ────────►│  ┌──────────────┐     ┌───────────────────────┐ │
  (segments,        │  │  Storage     │────►│  S3-Compatible Store  │ │
   manifests)       │  │  Client      │     │  (MinIO / AWS S3)     │ │
                    │  │  (aws-sdk-s3)│◄────│                       │ │
  Delivery ────────►│  └──────────────┘     └───────────────────────┘ │
  (read requests)   │         │                                       │
                    │         ▼                                       │
                    │  ┌──────────────┐                               │
                    │  │  LRU Cache   │                               │
                    │  │  (in-memory) │                               │
                    │  └──────────────┘                               │
                    └──────────────────────────────────────────────────┘
```

---

## 2. Storage Backend Choice

**Choice: S3-Compatible Object Storage**

**Justification:**

| Factor | Object Storage (S3) | Local Disk |
|--------|---------------------|------------|
| **Capacity scaling** | Virtually unlimited | Limited by disk size |
| **Durability** | 99.999999999% (S3) | Single disk = single point of failure |
| **Compute-storage coupling** | Decoupled (can scale independently) | Tightly coupled |
| **Cost** | Pay per GB stored + requests | Fixed cost per disk |
| **Concurrent access** | Native HTTP API, built for concurrent reads | Filesystem locking concerns |
| **CDN integration** | CDN can pull directly from S3 (or origin fronting S3) | CDN must pull from origin server |
| **Operational simplicity** | Managed service (AWS) or single-binary (MinIO) | Manual disk management, RAID, monitoring |

**Trade-offs accepted:**
- **Latency:** S3 GET latency is 50–200ms (vs. <1ms for local disk). Mitigated by LRU cache for hot segments.
- **Cost:** S3 PUT/GET requests are billed per-request. For live streaming with frequent segment writes, this is a cost factor. At MVP scale (5 streams × 3 renditions × 10 segments/minute = 150 PUTs/minute), cost is negligible.
- **Complexity:** Requires S3 client configuration (credentials, region, endpoint). Mitigated by using MinIO for local dev (identical API).

**[ASSUMPTION]** MinIO is used for local development and integration testing. AWS S3 (or any S3-compatible service: GCS with S3 interop, DigitalOcean Spaces, Backblaze B2) is used in production.

---

## 3. Storage Path Scheme

All media assets follow a deterministic path convention:

```
/{bucket}/
  └── {stream_id}/
      ├── metadata.json                    # Stream metadata (codecs, resolution, duration)
      ├── master.m3u8                      # Multivariant playlist
      ├── high/
      │   ├── init.mp4                     # Initialization segment (ftyp + moov)
      │   ├── media.m3u8                   # Media playlist for this rendition
      │   ├── seg_000000.m4s               # Media segment 0
      │   ├── seg_000001.m4s               # Media segment 1
      │   └── ...
      ├── medium/
      │   ├── init.mp4
      │   ├── media.m3u8
      │   ├── seg_000000.m4s
      │   └── ...
      └── low/
          ├── init.mp4
          ├── media.m3u8
          ├── seg_000000.m4s
          └── ...
```

### Path Construction Rules

| Component | Format | Example |
|-----------|--------|---------|
| `stream_id` | UUIDv7 (time-sortable) | `01906b3a-7f1e-7000-8000-000000000001` |
| `rendition` | Lowercase name from profile ladder | `high`, `medium`, `low` |
| `segment number` | Zero-padded 6-digit sequence | `seg_000042.m4s` |
| `init segment` | Fixed name per rendition | `init.mp4` |
| `media playlist` | Fixed name per rendition | `media.m3u8` |
| `master playlist` | Fixed name per stream | `master.m3u8` |
| `metadata` | Fixed name per stream | `metadata.json` |

**Why UUIDv7:** Time-sortable (first 48 bits are Unix millisecond timestamp), globally unique, no collision risk. Listing streams in S3 by prefix returns them in chronological order.

**Why zero-padded segment numbers:** Enables lexicographic ordering in S3 list operations. 6 digits supports up to 999,999 segments per rendition (at 6s each = ~1,667 hours of continuous streaming).

---

## 4. Storage Client API

The `storage` module exposes a trait-based abstraction:

```
trait MediaStore: Send + Sync {
    async fn put_segment(&self, path: &str, data: Bytes, content_type: &str) -> Result<(), StorageError>;
    async fn put_manifest(&self, path: &str, content: &str) -> Result<(), StorageError>;
    async fn put_metadata(&self, path: &str, metadata: &StreamMetadata) -> Result<(), StorageError>;
    async fn get_object(&self, path: &str) -> Result<GetObjectOutput, StorageError>;
    async fn get_object_range(&self, path: &str, start: u64, end: u64) -> Result<GetObjectOutput, StorageError>;
    async fn delete_object(&self, path: &str) -> Result<(), StorageError>;
    async fn delete_prefix(&self, prefix: &str) -> Result<u64, StorageError>;  // returns count deleted
    async fn list_objects(&self, prefix: &str) -> Result<Vec<ObjectInfo>, StorageError>;
    async fn head_object(&self, path: &str) -> Result<ObjectMeta, StorageError>;
}

struct GetObjectOutput {
    body: ByteStream,           // Streaming body (not fully buffered)
    content_length: u64,
    content_type: String,
    last_modified: DateTime<Utc>,
    etag: String,
}

struct ObjectInfo {
    key: String,
    size: u64,
    last_modified: DateTime<Utc>,
}

struct ObjectMeta {
    content_length: u64,
    content_type: String,
    last_modified: DateTime<Utc>,
    etag: String,
}
```

**Why trait-based:** Enables mocking for unit tests. The production implementation (`S3MediaStore`) wraps `aws-sdk-s3`. Tests can use an `InMemoryMediaStore` without any external dependencies.

### 4.1 S3 Client Configuration

```toml
[storage]
backend = "s3"
endpoint = "http://localhost:9000"       # MinIO for dev; omit for AWS S3
bucket = "streaminfa-media"
region = "us-east-1"
access_key_id = ""                       # Via env: STREAMINFA_STORAGE_ACCESS_KEY_ID
secret_access_key = ""                   # Via env: STREAMINFA_STORAGE_SECRET_ACCESS_KEY
path_style = true                        # true for MinIO; false for AWS S3
max_concurrent_requests = 50             # Connection pool size
request_timeout_secs = 30
```

### 4.2 Write Semantics

| Write Type | Content-Type | Consistency Requirement |
|------------|-------------|------------------------|
| fMP4 segment (`.m4s`) | `video/mp4` | Must be durable before manifest references it |
| Init segment (`init.mp4`) | `video/mp4` | Must be durable before any media segments |
| Media playlist (`.m3u8`) | `application/vnd.apple.mpegurl` | Overwritten atomically on each update |
| Master playlist (`.m3u8`) | `application/vnd.apple.mpegurl` | Written once (or on rendition change) |
| Metadata (`metadata.json`) | `application/json` | Written on stream creation, updated on completion |

**Write ordering guarantee:** The packager writes in this order:
1. Init segment (if not already written)
2. Media segment
3. Media playlist (updated to include new segment)
4. Master playlist (if first rendition becoming available)

Each step only proceeds after the previous step's S3 PUT returns success. This prevents dangling references in manifests.

### 4.3 Retry Strategy

| Failure Type | Retry Behavior |
|-------------|----------------|
| Network error (connection refused, DNS) | Retry 3× with exponential backoff: 100ms, 200ms, 400ms |
| S3 5xx (server error) | Retry 3× with exponential backoff |
| S3 429 (rate limited) | Retry with backoff using `Retry-After` header if present, else 1s |
| S3 403 (forbidden) | Do not retry. Log error. Likely misconfigured credentials. |
| S3 404 (not found, on GET) | Do not retry. Return `StreamNotFound` error. |
| Timeout (no response in 30s) | Retry once. If second attempt also times out, fail. |

---

## 5. Caching Strategy

### 5.1 In-Memory LRU Cache

The storage layer maintains an in-memory LRU cache for frequently accessed objects. This primarily benefits the delivery path (origin server serving segments to players).

```
CacheConfig {
    enabled: true,
    max_size_bytes: 512 * 1024 * 1024,  // 512 MB
    ttl_secs: 300,                       // 5 minutes (for live manifests)
    segment_ttl_secs: 3600,              // 1 hour (for segments, which are immutable)
}
```

**Cache key:** The S3 object path (e.g., `01906b3a.../high/seg_000042.m4s`).

### 5.2 Cache Behavior by Object Type

| Object Type | Cacheable | TTL | Reason |
|-------------|-----------|-----|--------|
| fMP4 segment | Yes | 1 hour | Immutable once written. Long TTL safe. |
| Init segment | Yes | 1 hour | Immutable once written. |
| Media playlist (live) | Yes | 1 second | Changes every segment interval. Short TTL ensures freshness. In practice, the cache is invalidated on every manifest update by the packager. |
| Media playlist (VOD) | Yes | 1 hour | Immutable after `#EXT-X-ENDLIST`. |
| Master playlist | Yes | 5 minutes | Rarely changes. |
| Metadata | No | — | Administrative data, not on the hot path. |

### 5.3 Cache Invalidation

- **Write-through invalidation:** When the packager writes a new manifest to S3, it also invalidates (or updates) the cache entry for that manifest path. This ensures the origin always serves the latest manifest.
- **TTL-based expiry:** Segments and playlists expire from cache after their TTL, regardless of access pattern. This bounds memory usage.
- **Eviction:** When the cache reaches `max_size_bytes`, the least-recently-used entry is evicted. Segments are larger (~2 MB) than manifests (~1 KB), so segments are evicted first under memory pressure.

### 5.4 Cache Miss Path

```
Player requests GET /streams/{stream_id}/high/seg_000042.m4s
    │
    ▼
Check LRU cache
    │
    ├── HIT: Return cached Bytes, set HTTP headers
    │
    └── MISS:
         │
         ▼
    Fetch from S3 (async GET)
         │
         ▼
    Insert into LRU cache (if size < max_size_bytes)
         │
         ▼
    Return to player
```

---

## 6. Delivery — HTTP Origin Server

### 6.1 Origin Architecture

The delivery origin is an HTTP server (built with Axum) that serves HLS manifests and fMP4 segments to players and CDNs.

```
                    ┌──────────────────────────────────────────────────┐
                    │           Delivery Origin                        │
                    │         (src/delivery/)                     │
                    │                                                  │
  Player/CDN ──────►│  ┌─────────┐   ┌──────────┐   ┌─────────────┐  │
  GET /stream/...   │  │  Axum   │──►│ Handler  │──►│  Storage    │  │
                    │  │ Router  │   │ (lookup, │   │  Client     │  │
                    │  │         │   │  cache,  │   │  (cached)   │  │
                    │  │         │   │  serve)  │   │             │  │
                    │  └─────────┘   └──────────┘   └─────────────┘  │
                    │       │                                         │
                    │  ┌────┴────────────────────────────────────┐    │
                    │  │ Middleware: Logging, CORS, Auth (opt)   │    │
                    │  └─────────────────────────────────────────┘    │
                    └──────────────────────────────────────────────────┘
```

### 6.2 URL Scheme

| URL Pattern | Handler | Response |
|-------------|---------|----------|
| `GET /streams/{stream_id}/master.m3u8` | Serve multivariant playlist | `application/vnd.apple.mpegurl` |
| `GET /streams/{stream_id}/{rendition}/media.m3u8` | Serve media playlist | `application/vnd.apple.mpegurl` |
| `GET /streams/{stream_id}/{rendition}/init.mp4` | Serve init segment | `video/mp4` |
| `GET /streams/{stream_id}/{rendition}/seg_{number}.m4s` | Serve media segment | `video/mp4` |

**No authentication on delivery endpoints** (MVP). Streams are accessible to anyone with the `stream_id`. This is acceptable for internal MVP. Phase 2 adds token-based playback auth.

### 6.3 HTTP Response Headers

**For live manifests (`media.m3u8` when stream is `LIVE`):**
```http
HTTP/1.1 200 OK
Content-Type: application/vnd.apple.mpegurl
Cache-Control: no-cache, no-store
X-StreamInfa-Stream-Id: 01906b3a-7f1e-7000-8000-000000000001
X-StreamInfa-Rendition: high
Access-Control-Allow-Origin: *
```

**For VOD manifests and all segments:**
```http
HTTP/1.1 200 OK
Content-Type: video/mp4
Content-Length: 2097152
Cache-Control: public, max-age=86400
ETag: "a1b2c3d4e5f6"
Accept-Ranges: bytes
Access-Control-Allow-Origin: *
```

### 6.4 Range Request Support

Segments support HTTP Range requests for partial content delivery:

```http
GET /streams/{id}/high/seg_000042.m4s HTTP/1.1
Range: bytes=0-1048575
```

```http
HTTP/1.1 206 Partial Content
Content-Range: bytes 0-1048575/2097152
Content-Length: 1048576
Content-Type: video/mp4
```

**Implementation:** The origin reads the full object from S3 (or cache), then slices the `Bytes` to the requested range. This is zero-copy on cached objects (Bytes slicing doesn't copy). For uncached objects, we use S3's native Range GET to avoid transferring the full object.

### 6.5 Error Responses

All errors return JSON bodies:

```json
{
  "error": "stream_not_found",
  "message": "Stream '01906b3a-...' does not exist or has been deleted.",
  "status": 404
}
```

| Scenario | Status | Error Code |
|----------|--------|------------|
| Stream ID not found | 404 | `stream_not_found` |
| Segment not found (not yet produced or deleted) | 404 | `segment_not_found` |
| Rendition not found (input too low res for this rung) | 404 | `rendition_not_found` |
| Stream in `PENDING` state (not yet ingesting) | 404 | `stream_not_ready` |
| Stream in `ERROR` state | 410 | `stream_error` |
| Range not satisfiable | 416 | `range_not_satisfiable` |
| Internal error (S3 failure) | 502 | `storage_error` |
| Origin overloaded | 503 | `service_unavailable` |

### 6.6 CORS Configuration

```
Access-Control-Allow-Origin: * (configurable; default allows all origins)
Access-Control-Allow-Methods: GET, HEAD, OPTIONS
Access-Control-Allow-Headers: Range
Access-Control-Expose-Headers: Content-Length, Content-Range, Accept-Ranges
Access-Control-Max-Age: 86400
```

Browser-based HLS players (hls.js, Video.js) require CORS headers to fetch manifests and segments cross-origin.

---

## 7. CDN Integration

### 7.1 Architecture with CDN

StreamInfa is designed as an **origin server** that sits behind an external CDN:

```
Player ──► CDN Edge ──► CDN Shield ──► StreamInfa Origin ──► S3
                                            │
                                         (cache)
```

**[ASSUMPTION]** The CDN is external to StreamInfa (e.g., CloudFront, Fastly, Cloudflare). StreamInfa does not implement CDN functionality. It sets `Cache-Control` headers that inform CDN caching behavior.

### 7.2 CDN Cache Behavior

| Object | `Cache-Control` | CDN Effect |
|--------|-----------------|------------|
| Live manifest | `no-cache, no-store` | CDN always fetches from origin. Every player request hits origin. |
| VOD manifest | `public, max-age=86400` | CDN caches for 24h. Massively reduces origin load. |
| Init segment | `public, max-age=86400` | Cached long-term. Immutable. |
| Media segment | `public, max-age=86400` | Cached long-term. Immutable once written. |

**Live manifest caching trade-off:** Setting `no-cache` on live manifests means every player poll hits the origin. With 1000 players polling every 6 seconds, that's ~167 requests/second to origin. At MVP scale this is manageable. For scale, Phase 2 can introduce short-TTL caching (1–2 seconds) on live manifests at the CDN layer, trading slight delay in segment discovery for reduced origin load.

### 7.3 CDN Configuration Recommendations

| Setting | Value | Reason |
|---------|-------|--------|
| **Origin shield** | Enabled (single POP closest to origin) | Collapses CDN edge → origin requests through one layer, reducing origin load |
| **Cache key** | Full URL path (no query string stripping) | Segment paths are deterministic and unique |
| **Gzip/Brotli** | Enabled for `.m3u8` (text), disabled for `.m4s`/`.mp4` (already compressed) | Manifests are small text files that compress well; segments are entropy-coded video that doesn't compress further |
| **Connection keep-alive** | Enabled (origin) | Reduces TCP handshake overhead for sequential segment fetches |
| **Origin timeout** | 10 seconds | S3 reads typically complete in <1s; 10s covers slow paths |

---

## 8. Segment Lifecycle and Cleanup

### 8.1 Segment Retention Policy

| Stream Type | Retention | Behavior |
|-------------|-----------|----------|
| **Live (active)** | Keep segments referenced in current manifest + retention buffer | Segments older than `segment_retention_minutes` (default 60) are eligible for deletion |
| **Live (ended, state=READY)** | Keep all segments indefinitely (becomes VOD) | The final manifest has `#EXT-X-ENDLIST`; all segments are needed for VOD playback |
| **VOD** | Keep all segments indefinitely | Deleted only on explicit admin `DELETE /api/v1/streams/{id}` |
| **ERROR state** | Delete after 24 hours | Failed streams' artifacts are kept briefly for debugging, then cleaned up |
| **DELETED state** | Immediate deletion | Admin-initiated delete triggers S3 prefix deletion |

### 8.2 Cleanup Task

A background Tokio task runs every 5 minutes:

```
loop {
    sleep(Duration::from_secs(300));

    for each stream with state == LIVE:
        list segments in S3 for this stream
        for each segment older than retention_minutes:
            if segment is NOT referenced in any current manifest:
                delete from S3
                increment streaminfa_storage_segments_deleted_total

    for each stream with state == ERROR and age > 24 hours:
        delete_prefix(stream_id)
        transition state to DELETED
}
```

### 8.3 Storage Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `streaminfa_storage_put_duration_seconds` | Histogram | `object_type` | S3 PUT latency |
| `streaminfa_storage_get_duration_seconds` | Histogram | `object_type` | S3 GET latency |
| `streaminfa_storage_put_bytes_total` | Counter | `object_type` | Total bytes written |
| `streaminfa_storage_get_bytes_total` | Counter | `object_type` | Total bytes read |
| `streaminfa_storage_errors_total` | Counter | `operation`, `error_type` | Storage operation errors |
| `streaminfa_storage_retry_total` | Counter | `operation` | Storage operation retries |
| `streaminfa_storage_segments_deleted_total` | Counter | | Segments cleaned up |
| `streaminfa_cache_hits_total` | Counter | `object_type` | LRU cache hits |
| `streaminfa_cache_misses_total` | Counter | `object_type` | LRU cache misses |
| `streaminfa_cache_size_bytes` | Gauge | | Current cache memory usage |
| `streaminfa_cache_entries` | Gauge | | Current number of cached objects |
| `streaminfa_delivery_requests_total` | Counter | `status`, `object_type` | HTTP requests to origin |
| `streaminfa_delivery_request_duration_seconds` | Histogram | `object_type` | Origin response latency |
| `streaminfa_delivery_bytes_sent_total` | Counter | | Total bytes served to players/CDN |

---

## 9. Storage Failure Modes

| Failure | Detection | Impact | Recovery |
|---------|-----------|--------|----------|
| **S3 unavailable** (MinIO down, AWS outage) | All PUTs/GETs fail | New segments cannot be stored; existing cached segments still serveable | Retry with backoff; alert if sustained >5 minutes |
| **S3 slow** (high latency) | PUT/GET latency exceeds threshold | Pipeline backpressure; live segments delayed | Backpressure propagates to ingest; alert on latency SLO breach |
| **Bucket deleted** | 404 on all operations | Total service failure | Alert critical; requires manual bucket recreation and data is lost |
| **Credential expiry** | 403 on all operations | Total service failure | Alert critical; rotate credentials; restart with new creds |
| **Cache OOM** | LRU eviction rate spikes | More cache misses; higher S3 GET load | Increase `max_size_bytes` or add memory; self-healing via eviction |
| **S3 eventual consistency** (rare with modern S3) | GET returns 404 for just-written object | Player gets 404 on new segment | Retry at player level (HLS players retry segment fetches); origin retries S3 GET once |

---

## Definition of Done — Storage and Delivery

- [x] Storage backend chosen and justified (S3-compatible)
- [x] Storage path scheme fully specified with naming conventions
- [x] Storage client API (trait-based) documented
- [x] S3 client configuration with all parameters
- [x] Write semantics and ordering guarantee
- [x] Retry strategy for all failure types
- [x] LRU cache design with TTL strategy per object type
- [x] Cache invalidation mechanism
- [x] HTTP origin server architecture
- [x] URL scheme for all delivery endpoints
- [x] HTTP response headers (Cache-Control, CORS, Range)
- [x] Range request support documented
- [x] Error responses with JSON bodies and status codes
- [x] CDN integration architecture and configuration recommendations
- [x] Segment lifecycle and cleanup policy
- [x] Storage and delivery metrics defined
- [x] Storage failure modes with detection and recovery
