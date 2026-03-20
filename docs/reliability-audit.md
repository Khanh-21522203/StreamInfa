# Reliability & Data-Safety Audit

**Date:** 2026-03-20
**Scope:** Full codebase — ingest, transcode, package, storage, control plane, shutdown
**Reviewer:** Static analysis of all pipeline stages

---

## Summary

StreamInfa is a well-structured streaming pipeline with thoughtful concurrency patterns, but it is **volatile by design**: all stream state lives in a `DashMap` with no persistence layer. Any unplanned process termination — OOM kill, panic, SIGKILL from the container runtime, host reboot — causes total loss of in-progress stream state. Beyond the fundamental durability gap, there are several concrete data-loss paths: temp files that survive crashes, segments silently dropped when storage retries are exhausted, and a storage cleanup loop that can delete newly-written segments due to a TOCTOU race. The risk is highest for VOD pipelines (file + pipeline takes 3–10 seconds; any crash in that window loses the upload) and live streams (state never persisted, so a restart creates a split-brain with any RTMP client that auto-reconnects).

---

## Findings

### 1. All stream state is lost on crash

**Risk: Critical**

**Code location:** `src/control/state.rs` — `StreamStateManager` backed by `DashMap` in `Arc`; no write-through to any durable store.

**Failure scenario:**
1. 10 live streams and 3 VOD uploads are in flight.
2. Process is OOM-killed (or panics — see Finding #2).
3. On restart, `DashMap` is empty.
4. Existing RTMP clients reconnect; their stream keys are valid, but the old `stream_id` records are gone, so the server either creates duplicate stream records or rejects them depending on client behaviour.
5. VOD uploads in `PROCESSING` are permanently stuck: the caller polled `/api/v1/streams/{id}` and received `processing`, but the stream record will never exist again.
6. S3 already contains partial segment data with no manifest, consuming storage indefinitely.

**Impact:** Complete loss of all runtime state; duplicate or orphaned S3 objects; clients receive indefinitely stale `processing` status on retry.

**Recommended fix:** Write a lightweight checkpoint to durable storage on every state transition (`transition()` in `state.rs`). An embedded SQLite (via `sqlx`) or a Redis hash is sufficient for MVP. On startup, read the checkpoint and rebuild the `DashMap`. Mark any stream that was `Live` or `Processing` at shutdown as `Error` with reason `"recovered_from_unclean_shutdown"` so operators can detect and replay.

---

### 2. Panics do not trigger graceful shutdown

**Risk: Critical**

**Code location:** `src/main.rs:24–30` — panic hook increments a counter and calls the default panic handler. No shutdown signal is sent.

**Failure scenario:**
1. A panic occurs anywhere in the async runtime.
2. None of the four graceful-shutdown phases run.
3. All channels are dropped instantly; any segments in the `packager → storage` channel are discarded.
4. Temp files from active VOD uploads remain on disk.

**Impact:** Every in-flight segment is lost; all temp files are orphaned; state is gone (see Finding #1).

**Recommended fix:** In the panic hook, call the root `CancellationToken::cancel()` before invoking the default handler. This gives the rest of the system a chance to flush on panic. Also audit all `unwrap()`/`expect()` calls in spawned tasks and replace with `?` or explicit error events.

---

### 3. Graceful shutdown Phase 3 does not flush storage — it sleeps

**Risk: Critical**

**Code location:** `src/main.rs:255–263`

```rust
// Phase 3: Flush remaining segments to storage (5s timeout)
tokio::time::sleep(std::time::Duration::from_secs(
    streaminfa::core::shutdown::STORAGE_FLUSH_TIMEOUT_SECS,
)).await;
```

**Failure scenario:**
1. `SIGTERM` is received (e.g., `docker stop`, Kubernetes pod eviction).
2. Phase 1 cancels the RTMP ingest. The transcode pipeline is still encoding the last GOP.
3. Phase 2 waits up to 10s for transcode jobs to clear. Jobs drain and push final `EncodedSegment`s into the `transcode → packager` channel (capacity 32).
4. Phase 3 just sleeps 5s — the packager is still running, building fMP4 boxes, and pushing `StorageWrite`s into the `packager → storage` channel.
5. After the sleep, Phase 4 aborts all background tasks including the storage writer mid-write.
6. Any segments still in the `packager → storage` channel are discarded. The final media playlist is never written to S3.

**Impact:** Last N segments of every active stream are lost on every clean shutdown. The HLS manifest permanently lags behind the last written segment, leaving the stream in an unplayable state.

**Recommended fix:** Phase 3 should close the packager's ingest channel and wait for the packager task to finish (with a timeout), then close the storage writer's channel and wait for it to drain. The pipeline already has structured `Drop`-based channel closure — join on the task handles rather than sleeping.

---

### 4. VOD temp file orphaned if process crashes after `201` but before transcode completes

**Risk: High**

**Code location:** `src/delivery/handlers.rs:1499` (`start_vod_pipeline_from_file`); temp file deleted only at `src/delivery/handlers.rs:1757` at the very end of `transcode_vod_from_file_blocking`.

**Failure scenario:**
1. Client uploads a 500 MB video. Server writes it to `/tmp/streaminfa/uploads/{id}.tmp`, returns `201 CREATED`.
2. `start_vod_pipeline_from_file` spawns a detached `tokio::spawn` that transcodes, packages, and *at the very end* deletes the temp file.
3. Process crashes (OOM, SIGKILL, panic) 2 seconds into transcoding.
4. `/tmp/streaminfa/uploads/{id}.tmp` remains forever. No startup cleanup exists.

**Impact:** Disk exhaustion over time, especially in upload-heavy workloads. In a container with a size-limited `/tmp`, this will eventually cause upload failures for all clients.

**Recommended fix:** On startup, scan the upload directory for any `.tmp` files older than a configurable threshold (e.g., 30 minutes) and delete them. Add this scan to `HttpUploadHandler::ensure_upload_dir()` which is already called at startup.

---

### 5. Storage cleanup loop has a TOCTOU race that can delete live segments

**Risk: High**

**Code location:** `src/storage/cleanup.rs:87–107`

**Failure scenario:**
1. Cleanup cycle runs for a live stream. It reads `media.m3u8` from S3 and builds the set of referenced segments (e.g., `seg_004.m4s … seg_008.m4s`).
2. Concurrently, the packager writes `seg_009.m4s` to S3, then updates `media.m3u8` to include it and evict `seg_004.m4s`.
3. Cleanup's stale snapshot still shows `seg_004.m4s` as referenced — it does not delete it. But it also has not yet seen `seg_009.m4s` in its S3 object listing.
4. Cleanup deletes `seg_009.m4s` because it is present in S3 but absent from the stale playlist snapshot.
5. HLS clients request `seg_009.m4s` and receive 404.

**Impact:** Active live streams can have segments silently deleted mid-playback, causing client rebuffering or fatal playback errors.

**Recommended fix:** Use the in-memory `SegmentIndex` (already maintained by the packager in `src/package/segment_index.rs`) as the authoritative reference set instead of re-reading the manifest from S3. This eliminates the race entirely. If cross-process cleanup is needed, only delete objects older than `now - (live_window * segment_duration + grace_period)` using S3 object last-modified timestamps.

---

### 6. Storage write failures are silently swallowed — no upstream signal

**Risk: High**

**Code location:** `src/storage/writer.rs:136–143` — after all retries are exhausted, records `(size, false)` and logs an error, but the packager and control plane are never notified.

**Failure scenario:**
1. S3 becomes temporarily unavailable (credential rotation, VPC routing issue).
2. The storage writer exhausts 3 retries (~700ms total) and logs an error.
3. The stream remains in `Live` or `Processing` state. The packager continues producing segments into the channel.
4. All subsequent segments are also dropped silently.
5. The HLS manifest in S3 is never updated. Clients see a frozen stream with no error indication.

**Impact:** Extended S3 outages cause silent, unrecoverable stream data loss with no client-visible error and no automated recovery path.

**Recommended fix:** After write failure, send a `PipelineEvent::StreamError` with a `"storage_write_failed"` reason. This transitions the stream to `Error` state, surfaces to clients via the status API, and triggers cleanup. Also consider a per-stream circuit breaker: if >N consecutive writes fail, cancel the pipeline's `CancellationToken`.

---

### 7. `write_metadata_json` is fire-and-forget with no retry and no failure propagation

**Risk: High**

**Code location:** `src/control/events.rs:102–103`

```rust
tokio::spawn(async move {
    write_metadata_json(store_arc.as_ref(), stream_id, &media_info_clone).await;
});
```

**Failure scenario:**
1. A VOD upload completes. `MediaInfoReceived` event triggers `write_metadata_json`.
2. S3 is briefly unavailable. The `put_manifest` call fails; a warning is logged; the task exits.
3. `metadata.json` is never written to S3 and there is no retry.
4. If the process crashes immediately after, even the warning is lost.

**Impact:** `metadata.json` silently absent from S3 with no indication it was ever expected. Any downstream tooling that depends on its presence will fail silently.

**Recommended fix:** Route the metadata write through the same `storage/writer.rs` path (with its existing retry logic) rather than a detached spawn. At minimum, increment a `metadata_write_failures_total` counter so the gap is visible in Grafana/Loki.

---

### 8. RTMP inactivity timeout — possible `StreamEnded` loss on shutdown race

**Risk: Medium (possible risk)**

**Code location:** `src/ingest/rtmp.rs:634–638` (inactivity break); `src/ingest/rtmp.rs:451–453` (`StreamEnded` send after `run_demux_loop` returns).

**Note:** The inactivity timeout `break`s the demux loop cleanly, `run_demux_loop` returns `Ok(())`, and the caller sends `StreamEnded`. This specific path is safe. The possible risk is: if the caller task is cancelled by shutdown *between* the demux loop exiting and the `event_tx.send(StreamEnded)`, the stream state never transitions out of `Live`.

**Impact (possible):** Stream stuck in `Live` state after restart; new RTMP connections with the same stream key rejected because the state check still sees `Live`.

**Recommended fix:** This becomes a real risk only without durable state (Finding #1). Once startup recovery exists, any stream in `Live` state with no active connection should be transitioned to `Error` on boot.

---

### 9. `mark_rendition_complete` has a check-then-act gap on the auto-transition

**Risk: Medium**

**Code location:** `src/control/state.rs:346–360`

```rust
let should_transition = {
    let mut entry = self.streams.get_mut(&stream_id)?;
    // check complete_count >= expected
    entry.state == StreamState::Processing && complete_count >= expected
};   // DashMap shard lock released here

if should_transition {
    self.transition(stream_id, StreamState::Ready).await  // acquires lock again
```

**Failure scenario:** Two concurrent `RenditionComplete` events arrive. Both acquire the shard lock in turn, both count 2/2 complete, both set `should_transition = true`. Both then call `transition(…, Ready)`. The second call gets `InvalidTransition(Ready → Ready)` — already handled gracefully.

**Impact:** Harmless double-transition attempt. The error is caught and logged as a warning, which can be misleading during debugging but causes no data loss.

**Recommended fix:** Low priority. Can be eliminated by keeping the shard lock held across both the check and the transition in a single `get_mut` call.

---

### 10. Segment write retry may duplicate manifest on versioned S3 buckets

**Risk: Medium**

**Code location:** `src/storage/writer.rs:78–84`

**Failure scenario:**
1. Manifest write succeeds on S3 (HTTP 200 returned).
2. Network drops the response before the SDK receives it.
3. `put_manifest` returns `Err(timeout)`.
4. The writer retries with the same manifest content.
5. With S3 versioning enabled, two versions of the manifest exist with slightly different metadata (e.g., clock skew in `created_at`).

**Impact:** With default S3 (no versioning), the retry overwrites safely. With versioned buckets, two versions exist — low risk but can confuse audit tools. The more concrete risk is that the per-attempt timeout is not explicitly bounded in the writer loop, so a single hanging attempt can consume most of the retry window.

**Recommended fix:** Verify that `tokio::time::timeout` wraps each individual attempt (already done in `src/storage/s3.rs:89`) and not just the outer loop. Adding `Content-MD5` to S3 `PUT` requests enables server-side idempotency detection.

---

### 11. No on-startup cleanup of orphaned temp files

**Risk: Medium**

**Code location:** `src/delivery/handlers.rs` — no startup scan of `upload_dir`.

**Failure scenario:** Process crashes mid-VOD-upload or mid-transcode repeatedly. Each crash leaves a `.tmp` file. Over time `/tmp` fills up, causing upload failures for all clients.

**Impact:** Disk exhaustion; upload failures once `/tmp` is full.

**Recommended fix:** In `HttpUploadHandler::ensure_upload_dir()`, scan for `.tmp` files older than 30 minutes and delete them. ~10 lines.

---

### 12. Packager uses hardcoded SPS/PPS fallback for fMP4 init segment

**Risk: Medium**

**Code location:** `src/package/runner.rs` — `generate_init_segment` call; fallback SPS/PPS used when `extract_avc_parameter_sets` returns empty.

**Failure scenario:**
1. Source stream uses a non-standard H.264 profile (e.g., High 4:2:2 or Baseline with custom constraints).
2. SPS/PPS extraction fails or returns empty bytes.
3. `generate_init_segment` silently falls back to hardcoded SPS/PPS for 1080p High 4.0.
4. The init segment mismatches the actual stream parameters.
5. Strict clients (Safari, Chromecast) refuse to play or produce visual artefacts.

**Impact:** Playback failure on strict clients for non-standard sources. No data loss, but stream appears broken from the operator's perspective.

**Recommended fix:** If `extract_avc_parameter_sets` returns empty, emit `PipelineEvent::StreamError` rather than silently falling back. Reserve the fallback for development mode only.

---

## Recovery / Resilience Gaps

| Gap | Severity |
|-----|----------|
| No durable state — entire `StreamStateManager` is volatile; crash = total state loss | Critical |
| No crash recovery on restart — no detection of which streams were in-flight, no cleanup or resume logic | Critical |
| No idempotency on VOD upload — duplicate HTTP retries create duplicate stream records | High |
| No pipeline error propagation from storage writer — silent write failures not surfaced to control plane | High |
| No idempotency on `StreamStarted` / `RenditionComplete` events — duplicate delivery causes bad transitions | Medium |
| No WAL / checkpointing for segments — buffered segments in channels have no durability guarantee | Medium |
| No temp file garbage collection on startup | Medium |
| No per-stream circuit breaker — one unhealthy stream can starve channel capacity for others | Medium |
| Cleanup uses S3 manifest as source of truth — creates TOCTOU race with packager | High |

---

## Quick Wins

Small changes with high safety impact, each implementable in under a day:

1. **Panic hook calls `cancel.cancel()`** — in `src/main.rs:24–30`, call the root `CancellationToken::cancel()` before the default handler. Gives the rest of the system a chance to flush on panic. ~3 lines.

2. **Real flush in shutdown Phase 3** — replace the `sleep(5s)` with `drop(packager_ingest_tx); packager_handle.await` then `drop(storage_tx); storage_handle.await`. Ensures last segments are written on every clean shutdown. ~10 lines.

3. **Propagate storage write failure as `StreamError`** — in `src/storage/writer.rs`, when all retries are exhausted, send `PipelineEvent::StreamError` via a cloned `event_tx`. Prevents silent zombie streams. ~5 lines.

4. **Startup temp-file GC** — in `ensure_upload_dir()`, delete `.tmp` files older than 30 minutes. Prevents disk exhaustion after repeated crashes. ~10 lines.

5. **Use in-memory `SegmentIndex` for cleanup** — replace the S3 manifest read in `src/storage/cleanup.rs` with a reference to the live `SegmentIndex`. Eliminates the segment deletion race at zero performance cost.

6. **Add `Content-MD5` to S3 PUT** — enables server-side idempotency detection for segment and manifest writes. ~2 lines in `src/storage/s3.rs`.
