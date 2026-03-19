# StreamInfa — Testing Strategy

> **Purpose:** Define the complete testing approach: unit tests, integration tests, end-to-end tests, media correctness validation, failure injection, and CI quality gates.
> **Audience:** All engineers writing tests; CI/CD maintainers; QA reviewers.

---

## 1. Testing Philosophy

StreamInfa is a media infrastructure system. Media bugs are insidious: a dropped frame, a wrong timestamp, a malformed fMP4 box—these produce silent playback failures that are extremely hard to debug without systematic validation. Our testing philosophy:

1. **Test the media, not just the code.** Every pipeline stage must validate that its output is media-correct (decodable, playable, spec-compliant).
2. **Test under realistic conditions.** Use real H.264/AAC test files, not synthetic data. Test with actual FFmpeg, not mocks.
3. **Test failure paths as thoroughly as happy paths.** Media systems fail in production (corrupt streams, storage outages, CPU starvation). Test these scenarios explicitly.
4. **Automated and reproducible.** Every test runs in CI with no manual steps. Test media fixtures are committed to the repo (small, purpose-built files).

### 1.1 Current Phase E Baseline (Implemented)

The following are now implemented in repository code:

1. Integration API coverage in `tests/integration_control_plane.rs`
2. In-process end-to-end playback flow in `tests/e2e_playback_flow.rs`
3. Shared integration helpers in `tests/common/mod.rs`
4. CI quality gate workflow in `.github/workflows/ci.yml`
5. Weekly/manual reliability smoke workflow in `.github/workflows/reliability-smoke.yml`

---

## 2. Test Pyramid

```
                    ┌─────────┐
                    │   E2E   │  ← 5–10 tests (full pipeline, slow)
                    │  Tests  │
                   ┌┴─────────┴┐
                   │Integration │  ← 20–40 tests (cross-module, real FFmpeg)
                   │   Tests    │
                  ┌┴────────────┴┐
                  │  Unit Tests   │  ← 100+ tests (per-module, fast, isolated)
                  └───────────────┘
```

| Level | Scope | Dependencies | Speed | Location |
|-------|-------|-------------|-------|----------|
| **Unit** | Single function/module | No external deps; mock storage, mock FFmpeg | < 1s per test | `src/**/*.rs` (inline `#[cfg(test)]`) |
| **Integration** | Cross-module boundaries, real FFmpeg | FFmpeg installed, MinIO running (docker-compose) | 1–30s per test | `tests/*.rs` |
| **E2E** | Full pipeline: ingest → transcode → package → store → deliver | Full docker-compose stack | 30–120s per test | `tests/e2e_*.rs` |

---

## 3. Unit Testing

### 3.1 Coverage by Module

| Module | Key Test Areas | Example Tests |
|--------|---------------|---------------|
| `core` | Config parsing, error types, auth token validation, stream state machine transitions | `test_config_loads_defaults`, `test_config_env_override`, `test_stream_state_valid_transitions`, `test_stream_state_invalid_transition_rejected`, `test_bcrypt_token_validation`, `test_constant_time_comparison` |
| `ingest` | RTMP handshake parsing, FLV tag demuxing, timestamp normalization, upload validation | `test_rtmp_handshake_c0_c1_parse`, `test_flv_video_tag_h264_demux`, `test_flv_audio_tag_aac_demux`, `test_flv_reject_non_h264`, `test_timestamp_ms_to_90khz`, `test_pts_normalization_starts_at_zero`, `test_upload_reject_oversized`, `test_upload_reject_hevc` |
| `transcode` | Profile ladder selection, segment boundary detection, rendition skip logic | `test_rendition_selection_1080p_input`, `test_rendition_selection_720p_skips_high`, `test_rendition_selection_below_480p`, `test_segment_boundary_at_idr`, `test_segment_duration_within_tolerance`, `test_no_upscaling` |
| `package` | fMP4 box construction, HLS manifest generation, sliding window logic | `test_fmp4_init_segment_structure`, `test_fmp4_media_segment_structure`, `test_hls_master_playlist_format`, `test_hls_media_playlist_live_window`, `test_hls_media_playlist_vod_endlist`, `test_segment_index_sliding_window`, `test_targetduration_rounded_up` |
| `storage` | Path scheme generation, cache hit/miss logic, retry logic | `test_storage_path_format`, `test_lru_cache_hit`, `test_lru_cache_eviction`, `test_retry_on_5xx`, `test_no_retry_on_403` |
| `delivery` | HTTP handler routing, content-type mapping, range request parsing, CORS headers | `test_content_type_m3u8`, `test_content_type_mp4`, `test_range_header_parse`, `test_cors_headers_present`, `test_404_json_body`, `test_cache_control_live_vs_vod` |
| `control` | API request validation, stream CRUD, health check, readiness check | `test_create_stream_returns_key`, `test_list_streams_filter_status`, `test_get_stream_not_found`, `test_delete_stream_transitions_to_deleted`, `test_healthz_response`, `test_readyz_storage_failure` |

### 3.2 Unit Test Principles

1. **No FFmpeg in unit tests.** FFmpeg FFI calls are wrapped behind a trait (`trait Transcoder`). Unit tests use a mock transcoder that returns pre-recorded output.
2. **No network in unit tests.** Storage is mocked via `InMemoryMediaStore`. RTMP parsing is tested with byte buffers, not TCP connections.
3. **Property-based testing** for parsers: Use `proptest` to generate random byte sequences and verify that parsers either produce valid output or return errors (never panic).
4. **Snapshot testing** for manifest generation: Use `insta` crate for snapshot tests. The expected manifest output is stored as a file and compared on each run.

### 3.3 Mock Implementations

```
// In storage module (src/storage/mod.rs)
struct InMemoryMediaStore {
    objects: Arc<RwLock<HashMap<String, Bytes>>>,
}

impl MediaStore for InMemoryMediaStore {
    async fn put_segment(&self, path: &str, data: Bytes, _ct: &str) -> Result<()> {
        self.objects.write().await.insert(path.to_string(), data);
        Ok(())
    }
    async fn get_object(&self, path: &str) -> Result<GetObjectOutput> {
        // return from hashmap or NotFound
    }
    // ...
}
```

---

## 4. Integration Testing

### 4.1 Test Environment

Integration tests require:
- **FFmpeg** installed (matching the production version).
- **MinIO** running (via `docker-compose -f docker/docker-compose.yml up minio`).
- **Test fixtures** in `fixtures/` directory.

Integration tests are gated by a feature flag or environment variable:
```
STREAMINFA_INTEGRATION_TESTS=1 cargo test --test '*'
```

Current baseline command set:

```bash
# Integration API contract tests
cargo test --test integration_control_plane -- --nocapture

# In-process E2E playback flow
cargo test --test e2e_playback_flow -- --nocapture
```

### 4.2 Test Fixtures

| File | Format | Codec | Duration | Resolution | Size | Purpose |
|------|--------|-------|----------|-----------|------|---------|
| `test_h264_aac.mp4` | MP4 | H.264+AAC | 10s | 1920×1080 | ~5 MB | Standard happy-path test |
| `test_h264_aac_720p.mp4` | MP4 | H.264+AAC | 10s | 1280×720 | ~3 MB | Test rendition selection (skip 1080p) |
| `test_h264_aac.flv` | FLV | H.264+AAC | 10s | 1920×1080 | ~5 MB | RTMP ingest simulation |
| `test_h264_mp3.mp4` | MP4 | H.264+MP3 | 5s | 1280×720 | ~2 MB | Test MP3→AAC re-encode |
| `test_hevc_aac.mp4` | MP4 | HEVC+AAC | 5s | 1920×1080 | ~3 MB | Test codec rejection |
| `test_corrupt.mp4` | MP4 | Corrupted | — | — | ~1 MB | Test corrupt file handling |
| `test_no_audio.mp4` | MP4 | H.264 only | 5s | 1280×720 | ~2 MB | Test video-only stream |
| `test_long.mp4` | MP4 | H.264+AAC | 60s | 854×480 | ~10 MB | Test multi-segment output |

**Fixture generation script:** `scripts/generate_test_fixtures.sh` uses FFmpeg to create all fixtures from a synthetic source (color bars + tone). This script is idempotent and documented.

### 4.3 Integration Test Cases

#### Transcode Pipeline Integration

| Test | Description | Validation |
|------|-------------|-----------|
| `test_transcode_1080p_produces_3_renditions` | Feed `test_h264_aac.mp4` → verify 3 renditions output | Check output count, resolution of each rendition |
| `test_transcode_720p_produces_2_renditions` | Feed `test_h264_aac_720p.mp4` → verify high is skipped | Check only medium + low produced |
| `test_transcode_segments_are_idr_aligned` | Feed `test_long.mp4` → verify each segment starts with IDR | Parse H.264 NALUs in output, check first NALU is type 5 |
| `test_transcode_segment_duration_within_tolerance` | Feed `test_long.mp4` → verify segments are 5–8s | Check each segment's duration |
| `test_transcode_mp3_reencoded_to_aac` | Feed `test_h264_mp3.mp4` → verify output audio is AAC | Probe output with FFmpeg |
| `test_transcode_rejects_hevc` | Feed `test_hevc_aac.mp4` → expect rejection error | Check error type |
| `test_transcode_handles_corrupt_file` | Feed `test_corrupt.mp4` → expect graceful error | No panic, error logged |

#### HLS Packaging Integration

| Test | Description | Validation |
|------|-------------|-----------|
| `test_hls_output_playable_with_ffprobe` | Full pipeline → verify output with `ffprobe` | `ffprobe` exits 0 on all segments |
| `test_hls_init_segment_valid_fmp4` | Verify init.mp4 has valid ftyp+moov | Parse MP4 boxes, check structure |
| `test_hls_media_segment_valid_fmp4` | Verify seg_*.m4s has valid styp+moof+mdat | Parse MP4 boxes, check structure |
| `test_hls_manifest_valid_m3u8` | Verify .m3u8 files parse correctly | Use an HLS manifest parser library or custom parser |
| `test_hls_master_playlist_lists_all_renditions` | Verify master.m3u8 has correct STREAM-INF entries | Parse and check bandwidth, resolution, codecs |
| `test_hls_vod_has_endlist` | Verify VOD playlist ends with `#EXT-X-ENDLIST` | String check |

#### Storage Integration

| Test | Description | Validation |
|------|-------------|-----------|
| `test_storage_put_get_roundtrip` | Write segment to MinIO → read back → compare | Byte-for-byte equality |
| `test_storage_delete_prefix` | Write 10 objects → delete prefix → verify empty | List returns 0 objects |
| `test_storage_retry_on_transient_failure` | Inject MinIO slowness → verify retry succeeds | Operation completes after retry |

---

## 5. End-to-End Testing

### 5.1 E2E Test: Live Stream (RTMP → HLS Playback)

**Setup:** Start full StreamInfa stack (docker-compose). Use FFmpeg as the RTMP encoder.

```
Step 1: POST /api/v1/streams → get stream_key
Step 2: ffmpeg -re -i test_h264_aac.mp4 -c copy -f flv rtmp://localhost:1935/live/{stream_key}
Step 3: Wait 15 seconds (allow 2+ segments to be produced)
Step 4: GET /streams/{id}/master.m3u8 → verify 200
Step 5: Parse master.m3u8 → GET each rendition's media.m3u8 → verify 200
Step 6: Parse media.m3u8 → GET first segment → verify 200 and Content-Type: video/mp4
Step 7: ffprobe the downloaded segment → verify valid H.264+AAC
Step 8: Stop FFmpeg encoder
Step 9: Wait 10 seconds → GET media.m3u8 → verify #EXT-X-ENDLIST present
Step 10: GET /api/v1/streams/{id} → verify status == "ready"
```

**Success criteria:** All HTTP requests return expected status codes. All media files are valid. Stream transitions through PENDING → LIVE → PROCESSING → READY.

### 5.2 E2E Test: VOD Upload → HLS Playback

```
Step 1: POST /api/v1/streams/upload with test_h264_aac.mp4
Step 2: Poll GET /api/v1/streams/{id} until status == "ready" (timeout: 60s)
Step 3: GET /streams/{id}/master.m3u8 → verify 200
Step 4: GET each rendition's segments → verify all segments are valid fMP4
Step 5: Verify total segment count × average duration ≈ source duration (10s)
Step 6: DELETE /api/v1/streams/{id} → verify 200
Step 7: GET /streams/{id}/master.m3u8 → verify 404
```

### 5.3 E2E Test: Concurrent Streams

```
Step 1: Create 5 streams
Step 2: Start 5 simultaneous FFmpeg RTMP encoders
Step 3: Wait 30 seconds
Step 4: Verify all 5 streams are in LIVE state
Step 5: Verify all 5 streams have produced ≥ 3 segments per rendition
Step 6: Stop all encoders
Step 7: Wait for all streams to reach READY
```

---

## 6. Media Correctness Validation

Beyond functional tests, we validate that output media is technically correct:

### 6.1 Segment Validation Checks

| Check | Tool | What It Validates |
|-------|------|-------------------|
| **fMP4 box structure** | Custom Rust parser in tests | Correct box hierarchy (styp, moof, mdat), valid box sizes, correct track IDs |
| **H.264 bitstream** | `ffprobe -show_frames` | All frames decodable, IDR at segment start, correct profile/level |
| **AAC bitstream** | `ffprobe -show_frames` | All frames decodable, correct sample rate and channel count |
| **Timestamp continuity** | `ffprobe -show_entries frame=pts_time` | PTS values are monotonically increasing, no gaps > 1 frame duration |
| **Segment duration** | `ffprobe -show_entries format=duration` | Duration matches `#EXTINF` value in manifest (±0.1s) |
| **HLS spec compliance** | `mediastreamvalidator` (Apple) or `hls-analyzer` | Manifest conforms to HLS spec, segments referenced in manifest exist |
| **Playability** | `ffplay` (headless, play first 5s) | No decode errors in stderr |

### 6.2 Automated Validation in CI

A post-pipeline validation step runs after every E2E test:

```bash
# Validate all segments for a given stream
for seg in $(find output/ -name "*.m4s"); do
  ffprobe -v error -show_entries stream=codec_name,width,height \
    -of csv=p=0 "$seg" || exit 1
done

# Validate manifests
for m3u8 in $(find output/ -name "*.m3u8"); do
  # Check that all referenced segments exist
  grep -oP 'seg_\d+\.m4s' "$m3u8" | while read seg; do
    [ -f "$(dirname $m3u8)/$seg" ] || exit 1
  done
done
```

---

## 7. Failure Injection Testing

### 7.1 Test Scenarios

| Scenario | Injection Method | Expected Behavior | Validation |
|----------|-----------------|-------------------|-----------|
| **RTMP disconnect mid-stream** | Kill FFmpeg encoder process | StreamInfa flushes current segment, adds `#EXT-X-ENDLIST`, transitions to READY | Check final manifest has ENDLIST; check segments are valid |
| **S3 unavailable** | Stop MinIO container | Pipeline backpressures; new segments are not lost (buffered); error metrics increment | Restart MinIO → verify pipeline resumes; buffered segments are written |
| **S3 slow (5s latency)** | Use `toxiproxy` to add latency to MinIO | Pipeline backpressures; channel utilization rises; no crash | Monitor channel utilization metric; verify it returns to normal after latency is removed |
| **CPU exhaustion** | Start 10 concurrent streams (over capacity) | Graceful degradation; backpressure on ingest; no OOM; no panic | Verify all streams either complete or error gracefully; process stays alive |
| **Disk full** | Fill temp disk before VOD upload | Upload returns 507; no crash; metric emitted | Check HTTP response code; check metric |
| **Corrupt RTMP data** | Send random bytes on RTMP connection after handshake | Connection dropped; error logged; other streams unaffected | Verify error metric; verify other concurrent streams continue |
| **OOM condition** | Set `ulimit -v` to restrict virtual memory | Process exits cleanly (or OOM killer intervenes); no data corruption | Verify segments written before OOM are valid |
| **SIGTERM during active streams** | Send SIGTERM while 3 streams are live | Graceful shutdown within 30s; current segments finalized; manifests updated | Verify process exits 0; verify final segments are valid |
| **FFmpeg segfault** | Use a specially crafted input that triggers FFmpeg crash (or mock FFI panic) | Affected stream transitions to ERROR; other streams unaffected; panic logged | Verify error state; verify other streams' metrics are healthy |

### 7.2 Failure Injection Tooling

| Tool | Purpose |
|------|---------|
| **toxiproxy** | Inject network latency, packet loss, connection reset between StreamInfa and MinIO |
| **stress-ng** | Generate CPU and memory stress on the test machine |
| **fallocate** | Fill disk to trigger disk-full scenarios |
| **kill -SIGTERM / -SIGKILL** | Test graceful and ungraceful shutdown |
| **Custom RTMP client** | Send malformed RTMP packets (invalid chunk sizes, wrong codec IDs) |

---

## 8. CI Quality Gates

### 8.1 CI Pipeline Stages

```
┌──────────┐   ┌──────────┐   ┌───────────┐   ┌──────────┐   ┌──────────┐
│  Lint &  │──►│  Unit    │──►│Integration│──►│   E2E    │──►│  Build   │
│  Format  │   │  Tests   │   │   Tests   │   │  Tests   │   │  Docker  │
└──────────┘   └──────────┘   └───────────┘   └──────────┘   └──────────┘
   ~30s           ~2 min         ~5 min          ~10 min        ~5 min
```

### 8.2 Gate Criteria

| Gate | Tool | Threshold | Blocks Merge? |
|------|------|-----------|---------------|
| **Formatting** | `cargo fmt --check` | No formatting diffs | Yes |
| **Linting** | `cargo clippy -- -D warnings` | Zero warnings | Yes |
| **Unit tests** | `cargo test --lib --bins` | All pass | Yes |
| **Integration tests** | `cargo test --test '*'` | All pass | Yes |
| **E2E tests** | `cargo test --test e2e_playback_flow` | All pass | Yes |
| **Code coverage** | `cargo tarpaulin` | ≥ 70% line coverage | No (informational) |
| **Dependency audit** | `cargo audit` | No critical/high vulnerabilities | Yes |
| **License check** | `cargo deny check licenses` | No GPL-only deps in app code | Yes |
| **Docker build** | `docker build -f docker/Dockerfile .` | Builds successfully | Yes |
| **Binary size** | Custom check | < 100 MB (stripped, release) | No (informational) |

### 8.3 Test Execution Environment

CI runs on a machine with:
- 4+ CPU cores (for integration tests with FFmpeg)
- 8+ GB RAM
- Docker + docker-compose (for MinIO and E2E tests)
- FFmpeg 6.x installed (either system-level or via Docker)

**CI implementation:** GitHub Actions with `ubuntu-latest` via `.github/workflows/ci.yml` and `.github/workflows/reliability-smoke.yml`.

---

## 9. Test Data Management

### 9.1 Fixture Generation

All test fixtures are generated by a script, not manually created. This ensures reproducibility:

```bash
#!/bin/bash
# scripts/generate_test_fixtures.sh

# 10s 1080p H.264 + AAC
ffmpeg -y -f lavfi -i "color=c=blue:s=1920x1080:d=10:r=30" \
       -f lavfi -i "sine=frequency=440:duration=10:sample_rate=48000" \
       -c:v libx264 -profile:v high -level 4.1 -b:v 4000k -g 60 \
       -c:a aac -b:a 128k \
       fixtures/test_h264_aac.mp4

# (similar for other fixtures...)
```

### 9.2 Fixture Size Budget

Total fixture size in the repository: **< 50 MB**. This is small enough to commit to Git without LFS. All fixtures are short (5–60 seconds) and at moderate bitrates.

### 9.3 Golden File Testing

For manifest generation and fMP4 box construction, we use golden file testing:
- Expected output is stored in `fixtures/golden/`.
- Tests generate output and compare byte-for-byte (or structurally for manifests).
- Golden files are regenerated explicitly with `REGENERATE_GOLDEN=1 cargo test`.

---

## 10. Performance Testing (Separate from Functional)

Performance tests are not part of the CI gate but are run on-demand before releases:

| Test | Tool | Metric |
|------|------|--------|
| **Transcode throughput** | Custom benchmark (`criterion`) | Frames per second per rendition |
| **fMP4 muxing throughput** | Custom benchmark | MB/s throughput |
| **Manifest generation** | Custom benchmark | Manifests per second |
| **Origin request latency** | `wrk` or `k6` | p50, p95, p99 latency under load |
| **Concurrent stream limit** | Custom test harness | Max streams before backpressure exceeds threshold |

---

## Definition of Done — Testing Strategy

- [x] Testing philosophy stated (test the media, test failures, automate everything)
- [x] Test pyramid with level definitions and counts
- [x] Unit test coverage by module with example test names
- [x] Unit test principles (no FFmpeg, no network, property-based, snapshot)
- [x] Mock implementations documented
- [x] Integration test environment and fixtures
- [x] Integration test cases for transcode, packaging, and storage
- [x] E2E test scenarios (live, VOD, concurrent)
- [x] Media correctness validation checks with tools
- [x] Failure injection scenarios with injection methods and expected behavior
- [x] CI quality gates with thresholds and blocking status
- [x] Test data management (fixture generation, golden files)
- [x] Performance testing approach documented
