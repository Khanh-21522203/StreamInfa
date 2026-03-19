# Performance Optimization Techniques

A catalog of every optimization applied to this codebase, with explanations and before/after comparisons.

---

## 1. DashMap — Lock-Free Concurrent HashMap

**Name:** Replace `RwLock<HashMap>` with `DashMap`

**Why:**
`RwLock<HashMap>` has a single reader-writer lock covering the entire map. Every write (segment upload, manifest update) acquires an exclusive lock that blocks all concurrent readers. Under live streaming load — multiple renditions writing segments while delivery handlers are reading manifests — readers stall behind every write, even for unrelated keys.

`DashMap` shards the map into independent buckets, each with its own lock. Two writes to different keys (e.g., `stream-A/high/000001.m4s` and `stream-B/low/000002.m4s`) never block each other. Reads to a key only compete with writes to the same shard.

**Compare:**
```rust
// Before — global exclusive lock on every write, blocks all readers
struct InMemoryStore {
    objects: Arc<RwLock<HashMap<String, StoredObject>>>,
}

async fn put(&self, key: &str, data: Bytes) {
    let mut map = self.objects.write().await; // blocks ALL readers
    map.insert(key.to_string(), data);
}

async fn get(&self, key: &str) -> Option<Bytes> {
    let map = self.objects.read().await; // blocks on any concurrent write
    map.get(key).cloned()
}

// After — per-shard locking, reads and writes to different keys are fully concurrent
struct InMemoryStore {
    objects: Arc<DashMap<String, StoredObject>>,
}

fn put(&self, key: &str, data: Bytes) {
    self.objects.insert(key.to_string(), data); // locks only one shard
}

fn get(&self, key: &str) -> Option<Bytes> {
    self.objects.get(key).map(|v| v.clone()) // never blocked by unrelated writes
}
```

---

## 2. Batch-and-Drain Channel Reads — Concurrent Write Batching

**Name:** Drain channel + `JoinSet` for concurrent I/O bursts

**Why:**
The storage writer originally read one `StorageWrite` at a time from the channel, awaited completion, then read the next. When the packager produces a segment burst (segment file + media playlist + maybe master playlist in quick succession), they get serialized: total latency = sum of all individual write latencies.

By draining all immediately-available items from the channel after receiving the first, then executing the entire batch with `JoinSet`, all writes in the burst run in parallel. Total latency = max of individual write latencies, not sum.

**Compare:**
```rust
// Before — serial: 3 writes at 20ms each = 60ms total
loop {
    let write = rx.recv().await?;
    store.put(&write.path, &write.data).await; // wait 20ms
    // now read next item, which was already waiting
}

// After — parallel: 3 writes at 20ms each = 20ms total
loop {
    let first = rx.recv().await?;
    let mut batch = vec![first];
    while let Ok(w) = rx.try_recv() { batch.push(w); } // drain what's ready

    let mut join_set: JoinSet<(u64, bool)> = JoinSet::new();
    for write in batch {
        let store = Arc::clone(&store);
        join_set.spawn(async move { store.put(&write.path, &write.data).await });
    }
    while let Some(result) = join_set.join_next().await { /* accumulate stats */ }
}
```

---

## 3. `tokio::spawn` for Non-Critical Blocking I/O on the Event Loop

**Name:** Fire-and-forget spawn for metadata writes

**Why:**
The `StreamStarted` event handler awaited `write_metadata_json()` inline. If S3 is slow (cold start, retrying on error), the entire event loop stalls — no `SegmentProduced` or `StreamEnded` events are processed during the wait. The metadata write is not on the critical path for playback; it only needs to eventually complete.

Spawning it independently lets the event loop continue immediately. The metadata write runs concurrently on the async runtime without blocking event processing.

**Compare:**
```rust
// Before — event loop stalls for the entire S3 PUT duration
PipelineEvent::StreamStarted { stream_id, media_info } => {
    write_metadata_json(&*store, stream_id, &media_info).await; // blocks here
    // no other events processed until this returns
}

// After — event loop returns immediately, metadata write runs independently
PipelineEvent::StreamStarted { stream_id, media_info } => {
    let store_arc = Arc::clone(&store);
    let media_info_clone = media_info.clone();
    tokio::spawn(async move {
        write_metadata_json(store_arc.as_ref(), stream_id, &media_info_clone).await;
    });
    // event loop continues immediately
}
```

---

## 4. Redundant Write Elimination — Master Playlist Deduplication

**Name:** Write master playlist only when rendition set changes

**Why:**
The packager runs one task per rendition. Each task, on receiving its first segment, calls `write_master_playlist()`. With 3 renditions arriving in quick succession, the master playlist is written 3 times within milliseconds — twice unnecessarily. Each write is an S3 PUT with serialization overhead.

Tracking the last written rendition count and guarding the call eliminates 2 of 3 redundant writes at stream startup.

**Compare:**
```rust
// Before — master playlist written once per rendition = 3x for 3 renditions
if !init_segments_written.contains_key(&rendition) {
    // ...write init segment...
    write_master_playlist(stream_id, &config, &rendition_infos, &storage_tx).await;
}

// After — written only when a new rendition is first seen
let mut last_master_rendition_count: usize = 0;
// ...
if rendition_infos.len() > last_master_rendition_count {
    write_master_playlist(stream_id, &config, &rendition_infos, &storage_tx).await;
    last_master_rendition_count = rendition_infos.len();
}
```

---

## 5. `BytesMut` Direct Read — Eliminate Per-Chunk Vec Allocation

**Name:** Read RTMP chunks directly into `BytesMut` instead of allocating a temporary `Vec`

**Why:**
The RTMP demux loop reassembles multi-chunk messages. Each chunk was read into a freshly allocated `Vec<u8>`, then copied into the accumulating `BytesMut` via `extend_from_slice`. For a 128 KB message split into 128-byte chunks, that is 1024 `Vec` allocations and 1024 copy operations per message.

Reading directly into the tail of the existing `BytesMut` via `resize` + slice reduces this to zero intermediate allocations and one set of copies (into the final buffer).

**Compare:**
```rust
// Before — 1024 Vec allocations + 1024 copies for a 128KB message
let mut chunk_data = vec![0u8; to_read];          // alloc every chunk
socket.read_exact(&mut chunk_data).await?;
data.extend_from_slice(&chunk_data);              // copy into BytesMut

// After — zero intermediate allocations
let offset = data.len();
data.resize(offset + to_read, 0);                 // extend BytesMut in-place
socket.read_exact(&mut data[offset..]).await?;    // read directly into tail
```

---

## 6. Buffer Reuse — Handshake Allocation Elimination

**Name:** Reuse the C1 buffer for reading C2 during RTMP handshake

**Why:**
The RTMP handshake allocates two 1536-byte buffers: one for C1 (client random) and one for C2 (server echo, which is C1 echoed back). C2 is immediately discarded after reading. Since C1 and C2 are the same size and C1's content is no longer needed when C2 is being read, the C1 buffer can be reused.

Additionally, the S1 response buffer used `vec![0xAB; 1528]` (heap allocation) which can be replaced with `BytesMut::put_bytes()` writing into an existing buffer without a separate allocation.

**Compare:**
```rust
// Before — two separate heap allocations
let mut c1 = vec![0u8; HANDSHAKE_SIZE];           // alloc 1536 bytes
socket.read_exact(&mut c1).await?;
// ... use c1 ...
let mut c2 = vec![0u8; HANDSHAKE_SIZE];           // alloc another 1536 bytes (wasted)
socket.read_exact(&mut c2).await?;
drop(c2); // never used

// After — reuse c1 buffer for C2
let mut c1 = vec![0u8; HANDSHAKE_SIZE];
socket.read_exact(&mut c1).await?;
// ... use c1 ...
c1.fill(0);
socket.read_exact(&mut c1).await?;               // read C2 into c1, no new allocation
```

---

## 7. Pre-Allocated Segment Buffers — Eliminate Reallocation During Encoding

**Name:** Pre-size `BytesMut` from bitrate × duration with headroom

**Why:**
`SegmentAccumulator` accumulates encoded video and audio packets into `BytesMut` buffers. Without pre-sizing, each buffer starts at 0 bytes and grows via doubling on every overflow — typically 9+ reallocations for a 6-second, 3500 kbps video segment (~15 MB). Each reallocation copies all existing data to a new allocation.

Pre-computing capacity from `bitrate_kbps × 1024/8 × duration_secs × 1.1` allocates the right size upfront, trading one slightly-oversized allocation for zero reallocations.

**Compare:**
```rust
// Before — starts at 0, reallocates ~9 times per segment
video_buffer: BytesMut::new(),  // 0 bytes → grows to 15 MB via doubling

// After — one allocation, correct size from the start
fn segment_buffer_capacity(video_kbps: u32, audio_kbps: u32, duration: f64) -> (usize, usize) {
    let video = ((video_kbps as f64 * 1024.0 / 8.0) * duration * 1.1) as usize;
    let audio = ((audio_kbps as f64 * 1024.0 / 8.0) * duration * 1.1) as usize;
    (video.max(4096), audio.max(4096))
}

let (video_cap, audio_cap) = segment_buffer_capacity(video_kbps, audio_kbps, duration);
video_buffer: BytesMut::with_capacity(video_cap),  // one allocation, no reallocs
// After emit, reserve again for the next segment:
self.video_buffer.reserve(video_cap);
```

---

## 8. `Arc<str>` — Zero-Cost String Cloning Across Renditions

**Name:** Replace `String` with `Arc<str>` for immutable shared strings

**Why:**
`profile` ("high", "main") and `level` ("4.1", "3.1") are set once at startup and cloned into every `EncodedSegment`, `RenditionInfo`, and `SegmentAccumulator` throughout the pipeline. With `String`, each clone is a heap allocation + memcpy. With `Arc<str>`, each clone is a single atomic increment — the string data is shared, not copied.

In a 3-rendition stream producing 1 segment/6s per rendition, `Arc<str>` saves ~30 String clones per minute for these two fields alone.

**Compare:**
```rust
// Before — every clone allocates on the heap
#[derive(Clone)]
pub struct EncodedSegment {
    pub profile: String,  // clone() = malloc + memcpy
    pub level: String,
}

let seg = segment.clone(); // copies "high" and "4.1" strings

// After — clone is one atomic reference count increment
#[derive(Clone)]
pub struct EncodedSegment {
    pub profile: Arc<str>, // clone() = atomic increment, zero alloc
    pub level: Arc<str>,
}

let seg = segment.clone(); // profile/level clones are free
```

---

## 9. Cache `to_string()` Outside Hot Loops

**Name:** Compute `StreamId::to_string()` once before the loop, not on every iteration

**Why:**
`StreamId` wraps a UUID. `.to_string()` formats it as a 36-character hyphenated hex string — a heap allocation every call. In the transcode pipeline and RTMP demux loop, `stream_id.to_string()` was called multiple times per frame (for metric labels, log fields, path building). At 30 fps with 3 renditions, this produced hundreds of identical String allocations per second, all containing the same value.

Caching it before the loop produces one allocation for the entire stream lifetime.

**Compare:**
```rust
// Before — allocates 6+ identical Strings per frame at 30fps = 180 allocs/sec
loop {
    obs::set_transcode_queue_depth(&stream_id.to_string(), ...);   // alloc
    obs::record_transcode_latency(&rendition.id.to_string(), ...); // alloc
    obs::set_transcode_fps(&stream_id.to_string(), ...);           // alloc
    obs::inc_transcode_error(&stream_id.to_string(), ...);         // alloc
}

// After — one allocation for the lifetime of the stream
let stream_id_str = stream_id.to_string(); // once
loop {
    obs::set_transcode_queue_depth(&stream_id_str, ...);  // zero alloc
    obs::record_transcode_latency(rendition.id.as_str(), ...); // zero alloc (&'static str)
    obs::set_transcode_fps(&stream_id_str, ...);          // zero alloc
}
```

---

## 10. `spawn_blocking` for CPU-Bound Work

**Name:** Offload bcrypt verification off the async executor thread pool

**Why:**
`bcrypt::verify()` is intentionally slow (~100ms CPU time). Calling it directly in an async task blocks the tokio executor thread for the entire duration — no other tasks on that thread can make progress. With a default tokio thread pool of N threads (usually `num_cpus`), a single bcrypt call can block 1/N of the total async capacity.

`tokio::task::spawn_blocking` runs the closure on a separate blocking thread pool, freeing the async thread immediately to process other futures.

**Compare:**
```rust
// Before — blocks tokio worker thread for ~100ms
async fn authenticate(&self, key: &str) -> Result<()> {
    let valid = bcrypt::verify(key, &stored_hash)?; // blocks thread for 100ms
    if !valid { return Err(AuthError); }
    Ok(())
}

// After — async thread free immediately, blocking thread handles the CPU work
async fn authenticate(&self, key: &str) -> Result<()> {
    let auth = Arc::clone(&self.auth);
    let key = key.to_string();
    tokio::task::spawn_blocking(move || {
        bcrypt::verify(&key, &stored_hash) // runs on blocking pool
    })
    .await??;
    Ok(())
}
```

---

## 11. `BytesMut` for Keyframe Injection — Eliminate Vec Intermediary

**Name:** Build SPS/PPS-prepended keyframe data in `BytesMut` instead of `Vec<u8>`

**Why:**
On IDR keyframes, the demuxer prepends SPS and PPS NALUs to the NALU data. This was done with `Vec<u8>` — allocate, push start codes, push SPS, push PPS, push NALU data, then convert to `Bytes` via `Bytes::from(vec)`. `BytesMut` can be constructed with the exact required capacity upfront and frozen into `Bytes` with `freeze()`, which transfers ownership without copying.

**Compare:**
```rust
// Before — Vec grows dynamically, then copied into Bytes
let mut buf = Vec::new();
buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
buf.extend_from_slice(&sps);
buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
buf.extend_from_slice(&pps);
buf.extend_from_slice(&nalu_bytes);
Bytes::from(buf) // may reallocate to create Bytes

// After — exact-size allocation upfront, zero-copy freeze
let mut buf = BytesMut::with_capacity(4 + sps.len() + 4 + pps.len() + nalu_bytes.len());
buf.put_slice(&[0x00, 0x00, 0x00, 0x01]);
buf.put_slice(&sps);
buf.put_slice(&[0x00, 0x00, 0x00, 0x01]);
buf.put_slice(&pps);
buf.put_slice(&nalu_bytes);
buf.freeze() // zero-copy: transfers ownership of the allocation to Bytes
```

---

## 12. `&'static str` Metric Labels — Eliminate Per-Call String Allocation

**Name:** Pass static string labels as `&'static str` instead of `String` to metrics macros

**Why:**
The `metrics` crate represents label values as `SharedString`, which is a `Cow<'static, str>`. When a `&'static str` is passed, it becomes `Cow::Borrowed` — zero allocation. When a `&str` (non-static) or `String` is passed, it becomes `Cow::Owned(String)` — a heap allocation.

Every metric recording function was calling `.to_string()` on label values like `"rtmp"`, `"audio"`, `"segment"`, `"high"`, etc. — known at compile time. These are called on every frame (73/sec ingest), every segment (0.5/sec per rendition), every delivery request. Changing these parameters to `&'static str` and removing `.to_string()` converts each from a heap allocation to a borrowed pointer.

Dynamic values (`stream_id` UUID strings) still allocate — that is unavoidable since their values are only known at runtime.

**Compare:**
```rust
// Before — every call to inc_ingest_frames allocates two Strings on the heap
pub fn inc_ingest_frames(stream_id: &str, track: &str) {
    counter!(
        "streaminfa_ingest_frames_total",
        "stream_id" => stream_id.to_string(), // Cow::Owned — heap alloc
        "track" => track.to_string()           // Cow::Owned — heap alloc ("audio"/"video")
    ).increment(1);
}
// Called 73x/sec = 73 unnecessary heap allocations/sec for "audio" or "video" alone

// After — "audio"/"video" is a compile-time constant, borrowed with zero alloc
pub fn inc_ingest_frames(stream_id: &str, track: &'static str) {
    counter!(
        "streaminfa_ingest_frames_total",
        "stream_id" => stream_id.to_string(), // Cow::Owned — unavoidable (dynamic UUID)
        "track" => track                       // Cow::Borrowed — zero alloc
    ).increment(1);
}
// track label: 0 allocs/sec regardless of call rate
```

---

## 13. `RenditionId::as_str()` — Use Existing `&'static str` Instead of `to_string()`

**Name:** Use `as_str()` on enums that map to static strings

**Why:**
`RenditionId` is an enum (`High`, `Medium`, `Low`, `Source`) that maps to fixed strings. It has a `Display` impl which calls `to_string()` — producing a heap-allocated `String` every call. It also has `as_str()` which returns `&'static str` — the string is baked into the binary, no allocation.

In the transcode pipeline and packager, `rendition.id.to_string()` was called multiple times per frame/segment. Switching to `rendition.id.as_str()` converts all of these to zero-cost lookups.

**Compare:**
```rust
// Before — allocates a new String on every call
obs::record_transcode_latency(&rendition.id.to_string(), latency); // malloc "high\0"
obs::set_transcode_fps(&stream_id_str, &rendition.id.to_string(), fps); // malloc again
obs::inc_transcode_segments(&stream_id_str, &rendition.id.to_string()); // malloc again

// After — returns pointer to string literal in the binary, zero alloc
obs::record_transcode_latency(rendition.id.as_str(), latency); // points to "high" in .rodata
obs::set_transcode_fps(&stream_id_str, rendition.id.as_str(), fps);
obs::inc_transcode_segments(&stream_id_str, rendition.id.as_str());

// The implementation:
impl RenditionId {
    pub fn as_str(&self) -> &'static str {
        match self {
            RenditionId::High   => "high",   // &'static str — in binary
            RenditionId::Medium => "medium",
            RenditionId::Low    => "low",
            RenditionId::Source => "source",
        }
    }
}
```

---

## 14. Why Object Pooling Was NOT Applied

**Name:** Object pool for reusable buffers (evaluated and rejected)

**Why it was considered:**
Buffer allocation is a measurable cost in hot paths. Object pools (like `deadpool` or a `crossbeam::ArrayQueue<BytesMut>`) can amortize allocation cost by recycling fixed-size objects.

**Why it was rejected:**
The hot-path buffers in this codebase transfer ownership to `Bytes` via `BytesMut::freeze()`. `freeze()` converts the `BytesMut` into an immutable `Bytes` by transferring the underlying allocation — not copying. The `BytesMut` is left with zero capacity after the transfer. To return a buffer to a pool, you would need to either:

1. Copy the data into a fresh `Bytes` before freezing — adding a memcpy (5–20μs for a 20 KB frame) which is 10–50× more expensive than `malloc` (~300ns).
2. Use `Arc`-based split patterns — but the pool would need to wait for all `Bytes` clones to drop before reclaiming the buffer, requiring reference counting that defeats the point.

Additionally, video frames are variable size (keyframes are typically 5–10× larger than P-frames), making fixed-size pool buckets either wasteful or a source of misses. jemalloc's thread-local size-class cache already recycles same-sized recent frees efficiently.

**Compare:**
```rust
// Attempted pool approach — adds a copy, making it slower than malloc
let mut buf = pool.get(); // reuse allocation
buf.clear();
buf.put_slice(&frame_data); // memcpy ~20KB = ~5µs
let bytes = Bytes::copy_from_buffer(&buf); // another copy to make Bytes independent
pool.return(buf);
// Total: 2× memcpy ≈ 10µs vs malloc ≈ 0.3µs

// Correct approach — zero-copy freeze, let the allocator recycle
let mut buf = BytesMut::with_capacity(estimated_size); // 0.3µs malloc
buf.put_slice(&frame_data); // one copy (unavoidable — reading from socket)
let bytes = buf.freeze(); // zero-copy ownership transfer, no copy
// Total: 1× memcpy + 0.3µs malloc
// jemalloc recycles the allocation on drop without calling the OS
```
