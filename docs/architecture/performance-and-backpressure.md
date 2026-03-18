# StreamInfa — Performance and Backpressure

> **Purpose:** Define latency targets, throughput assumptions, buffering strategy, backpressure propagation, and resource constraints for the StreamInfa MVP.
> **Audience:** Engineers tuning pipeline performance; operators capacity-planning hardware.

---

## 1. Latency Targets

### 1.1 Live Path: Glass-to-Glass Latency

**Target: ≤ 18 seconds** (encoder capture → player display)

| Stage | Contribution | Controllable? | Notes |
|-------|-------------|---------------|-------|
| Camera → Encoder output | ~500ms | No (external encoder) | Depends on encoder hardware and settings |
| Encoder → RTMP wire (network) | ~200ms | No (network) | LAN: <1ms; Internet: 50–200ms |
| RTMP receive + demux | ~200ms | Yes | TCP read + FLV parsing; dominated by I/O |
| Ingest → Transcode queue | 0–500ms | Yes (channel capacity) | Depends on channel fullness |
| Transcode (1 GOP = 2s) | ~2,000ms | Partially (GOP length) | Must accumulate a full GOP before encoding; fixed by keyframe interval |
| Encode (x264 for 1 GOP) | ~500ms | Yes (preset, threads) | `medium` preset on 2 threads per rendition |
| Transcode → Packager queue | 0–200ms | Yes (channel capacity) | |
| fMP4 packaging | ~50ms | Yes | Lightweight byte-level muxing |
| S3 write (segment) | ~200ms | Partially (backend) | MinIO local: ~50ms; AWS S3: 100–500ms |
| S3 write (manifest update) | ~100ms | Partially (backend) | |
| **Segment available at origin** | **~4,000ms** | | From frame capture to S3 availability |
| Player manifest poll delay | 0–6,000ms | No (player behavior) | Players poll at `#EXT-X-TARGETDURATION` intervals |
| Player buffer (initial) | 2,000–6,000ms | No (player config) | Most players buffer 2–3 segments |
| **Total glass-to-glass** | **6,000–18,000ms** | | |

**Dominant latency contributors:**
1. **Segment duration (6s):** The player must wait up to one full segment duration before a new segment appears in the manifest. This is the single largest contributor.
2. **Player buffer:** Players buffer 2–3 segments by default for resilience. This adds 12–18s in the worst case.
3. **GOP length (2s):** The encoder must accumulate 2 seconds of frames before encoding a GOP.

**Phase 2 optimization path:** LL-HLS (Low-Latency HLS) with partial segments can reduce glass-to-glass to 2–5 seconds by:
- Emitting partial segments (sub-second chunks) before a full segment is complete.
- Using `#EXT-X-PART` tags and blocking manifest requests.
- This requires significant changes to packager and origin server. Explicitly deferred to Phase 2.

### 1.2 VOD Path: Upload-to-Available

**Target: ≤ 2× real-time for 1080p source**

| Input Duration | Expected Processing Time | Resulting Availability |
|---------------|-------------------------|----------------------|
| 1 minute | ≤ 2 minutes | |
| 10 minutes | ≤ 20 minutes | |
| 1 hour | ≤ 2 hours | |
| 6 hours (max) | ≤ 12 hours | |

**Breakdown for a 10-minute, 1080p source:**

| Stage | Duration | Notes |
|-------|----------|-------|
| Upload (assuming 100 Mbps) | ~30s for a 500 MB file | Network-bound |
| FFmpeg probe | ~1s | Read moov atom, detect codecs |
| Transcode (3 renditions) | ~15 min | CPU-bound; 3 renditions processed in parallel |
| fMP4 packaging | ~30s | Lightweight |
| S3 upload (all segments) | ~60s | ~100 segments × 3 renditions × ~2 MB each |
| **Total** | **~18 min** | Within 2× target |

**Why 2× real-time is achievable:** With the `medium` x264 preset and 2 threads per encoder, each rendition encodes at approximately 1.2× real-time on a modern x86_64 core. Three renditions run in parallel on separate cores, so the total wall-clock time is dominated by the highest-resolution rendition (1080p), which encodes at ~0.8× real-time (slightly slower due to the higher resolution). Adding upload, probe, packaging, and storage brings the total to ~1.5–2× real-time.

---

## 2. Throughput Assumptions

### 2.1 Ingest Throughput

| Metric | Target | Justification |
|--------|--------|---------------|
| **Max ingest bitrate per stream** | 20 Mbps | Covers 1080p60 at high quality. Higher bitrates (e.g., 50 Mbps for 4K) are beyond MVP scope. |
| **Aggregate ingest bandwidth** | 100 Mbps (5 streams × 20 Mbps) | Fits within 1 Gbps NIC with 90% headroom for delivery traffic. |
| **Max concurrent RTMP connections** | 50 | Most will be idle (handshake phase); only 5–10 actively streaming. |
| **Max concurrent HTTP uploads** | 10 | Each uploads at network speed to disk. |

### 2.2 Transcode Throughput

| Metric | Target | Justification |
|--------|--------|---------------|
| **Live transcode speed** | ≥ 1× real-time per rendition | Must keep up with ingest rate. At `medium` preset with 2 threads, 1080p encodes at ~1.2× real-time (sufficient). |
| **Concurrent live transcode streams** | 5 (with 3 renditions each = 15 encoders) | 15 encoders × 2 threads = 30 threads on an 8-core/16-thread machine. CPU is the bottleneck. |
| **VOD transcode speed** | ≥ 0.8× real-time for 1080p | Slightly slower is acceptable since VOD is not time-critical. |

### 2.3 Delivery Throughput

| Metric | Target | Justification |
|--------|--------|---------------|
| **Concurrent segment requests** | ≥ 500 at < 50ms p99 | Each request is an S3 GET (or cache hit) + HTTP response. Axum handles this easily. |
| **Origin bandwidth** | ≥ 500 Mbps sustained | 500 concurrent players × ~2 Mbps average rendition = ~1 Gbps. With CDN absorbing 80–90% of requests, origin sees ~100–200 Mbps. |
| **Manifest request rate** | ≥ 200/s | Live players poll every ~6s. 200/s supports ~1200 concurrent live viewers without CDN. |

---

## 3. Buffering Strategy

### 3.1 Buffer Inventory

The pipeline maintains explicit buffers at every stage boundary. Each buffer is a bounded channel with a defined capacity.

```
┌──────────┐     ┌──────────────┐     ┌──────────┐     ┌──────────┐     ┌──────────┐
│  TCP     │     │ Ingest →     │     │Transcode→│     │Packager→ │     │  S3      │
│  Recv    │────►│ Transcode    │────►│ Packager  │────►│ Storage  │────►│  Write   │
│  Buffer  │     │  Channel     │     │  Channel  │     │  Channel │     │  Buffer  │
│  (OS)    │     │  (64 frames) │     │ (32 segs) │     │ (32 ops) │     │  (SDK)   │
└──────────┘     └──────────────┘     └──────────┘     └──────────┘     └──────────┘
    64 KB             ~4 MB              ~64 MB            ~64 MB          SDK-managed
```

### 3.2 Buffer Sizing Rationale

| Buffer | Capacity | Size Estimate | Rationale |
|--------|----------|---------------|-----------|
| **TCP receive buffer** | OS default (64–256 KB) | ~128 KB | OS-managed. Sufficient for RTMP packet reassembly. |
| **Ingest → Transcode channel** | 64 `DemuxedFrame`s | ~4 MB (64 × 64 KB avg frame) | Must hold at least 1 GOP (60 frames at 30fps × 2s). 64 provides slight headroom for bursts. Too large → excess memory; too small → premature backpressure. |
| **Decoder → Encoder channel** | 8 `AVFrame`s (per encoder) | ~24 MB per rendition (8 × 3 MB raw YUV 1080p) | 8 frames = ~267ms of decode-ahead at 30fps. Gives the encoder a small lookahead buffer without excessive memory usage. |
| **Transcode → Packager channel** | 32 `EncodedSegment`s | ~64 MB (32 × 2 MB avg segment) | 32 segments = ~192 seconds of buffered output. This is generous because the packager is fast (sub-millisecond per segment). Large capacity prevents transcode stalls when storage has transient slowness. |
| **Packager → Storage channel** | 32 `StorageWrite`s | ~64 MB (mix of segments and manifests) | Similar reasoning. Storage writes are the slowest path (S3 latency). Large capacity absorbs storage jitter. |
| **LRU cache (delivery)** | 512 MB (configurable) | 512 MB | Holds ~250 hot segments (2 MB each). At 5 streams × 3 renditions, this covers the recent ~16 segments per rendition—the entire live window plus some history. |

### 3.3 Total Memory Budget

| Component | Memory | Notes |
|-----------|--------|-------|
| Per-stream pipeline buffers (5 streams) | ~170 MB | 34 MB × 5 (see architecture overview) |
| LRU cache | 512 MB | Configurable |
| FFmpeg internal buffers (15 encoders) | ~300 MB | ~20 MB per x264 encoder instance (reference frames, lookahead) |
| Tokio runtime + application overhead | ~200 MB | Thread stacks, task allocations, gRPC/HTTP server |
| OS + shared libraries | ~500 MB | FFmpeg .so files, glibc, kernel buffers |
| **Total** | **~1.7 GB** | Well within 32 GB machine target |

**Headroom:** 32 GB - 1.7 GB = ~30 GB free. This headroom is important for:
- OS page cache (S3 SDK uses mmap for large transfers).
- Burst allocations during concurrent VOD transcodes.
- FFmpeg's internal memory pools which can spike during complex scenes.

---

## 4. Backpressure Propagation

### 4.1 Design Principle

StreamInfa uses **explicit backpressure via bounded channels**. When any stage is overwhelmed, it slows down the upstream stage by blocking on channel send. The backpressure propagates all the way to the source (encoder or HTTP client).

This is fundamentally different from (and superior to) the alternative approaches:
- **Dropping frames silently:** Causes quality degradation without the source knowing. Hard to debug.
- **Unbounded buffering:** Causes OOM crashes when the downstream is persistently slower than upstream.
- **Rate limiting at ingest:** Artificial throttling that doesn't adapt to actual processing capacity.

### 4.2 Backpressure Chain (Live)

```
Storage slow (S3 latency spike)
    │
    ▼
Packager → Storage channel fills up (32/32)
    │
    ▼
Packager blocks on storage_tx.send().await
    │
    ▼
Transcode → Packager channel fills up (32/32)
    │
    ▼
Transcode blocks on packager_tx.send().await (in spawn_blocking, blocks the OS thread)
    │
    ▼
Decoder → Encoder channels fill up (8/8)
    │
    ▼
Decoder blocks on encoder_tx.send() (crossbeam, blocking)
    │
    ▼
Ingest → Transcode channel fills up (64/64)
    │
    ▼
Ingest task blocks on transcode_tx.send().await
    │
    ▼
Ingest stops reading from TCP socket
    │
    ▼
TCP receive buffer fills (OS-level)
    │
    ▼
TCP window shrinks to 0 → Encoder's TCP send blocks
    │
    ▼
Encoder buffers frames internally → eventually drops or pauses capture
```

**Time from storage stall to encoder backpressure:**
- Channels drain at approximately the ingest rate.
- Total buffer capacity: 64 + 8 + 32 + 32 = 136 items ≈ ~60 seconds of media at 30fps.
- The encoder will feel backpressure ~60 seconds after storage stalls.
- This 60-second buffer absorbs transient storage latency spikes without any visible impact.

### 4.3 Backpressure Chain (VOD)

VOD transcode is not real-time, so backpressure is simpler:

```
Storage slow
    │
    ▼
Transcode output channel fills up
    │
    ▼
FFmpeg read loop pauses (blocks on channel send)
    │
    ▼
No data loss. Transcode job takes longer. Progress percentage stalls.
```

No external impact—the VOD file is already on disk. The job simply takes longer to complete.

### 4.4 Backpressure Metrics

| Metric | Type | Description | Alert Threshold |
|--------|------|-------------|----------------|
| `streaminfa_channel_utilization` | Gauge (labels: `channel_name`, `stream_id`) | Channel fullness ratio (0.0–1.0) | > 0.8 sustained for 30s |
| `streaminfa_channel_send_wait_seconds` | Histogram (labels: `channel_name`) | Time spent blocked on channel send | p99 > 1s |
| `streaminfa_backpressure_events_total` | Counter (labels: `channel_name`) | Number of times a send blocked for > 100ms | Any occurrence |

**Alert:** If `streaminfa_channel_utilization{channel_name="ingest_to_transcode"} > 0.8` for 30 seconds, it means the transcode pipeline cannot keep up with the ingest rate. Root causes: CPU saturation, too many concurrent streams, or encoder sending at unexpectedly high bitrate.

---

## 5. Resource Constraints and Capacity Planning

### 5.1 Reference Machine Specification

| Resource | Spec | Purpose |
|----------|------|---------|
| **CPU** | 8 cores / 16 threads, x86_64 with AVX2 | x264 SIMD acceleration; Tokio async workers; S3 client |
| **RAM** | 32 GB DDR4 | Pipeline buffers, FFmpeg internal memory, LRU cache, OS page cache |
| **Disk** | 500 GB NVMe SSD | Temp storage for VOD uploads; OS swap (if any); no media storage (goes to S3) |
| **Network** | 1 Gbps NIC | Ingest (100 Mbps) + delivery (500 Mbps) + S3 traffic |
| **OS** | Ubuntu 22.04 LTS, kernel 5.15+ | Stable, long-term support, good Tokio epoll performance |

### 5.2 CPU Allocation Model

```
Total: 16 logical cores (8 physical × 2 HT)

Allocation:
  ├── Tokio async workers: 4 cores (handles RTMP I/O, HTTP, S3 client, packaging)
  ├── x264 encoders: 10 cores (5 streams × 3 renditions × 2 threads = 30 threads, time-sliced)
  ├── System/FFmpeg overhead: 2 cores
  └── Total: 16 cores (fully utilized under peak load)
```

**CPU is the bottleneck** for live transcoding. Each x264 encoder instance at `medium` preset uses ~1.5 cores for 1080p. With 15 encoder instances (5 streams × 3 renditions), the theoretical CPU demand is 22.5 cores—exceeding 16 logical cores. In practice:
- Lower renditions (720p, 480p) use significantly less CPU (~0.5–0.8 cores each).
- Actual CPU demand: ~1.5 (1080p) + 0.8 (720p) + 0.5 (480p) = ~2.8 cores per stream.
- 5 streams × 2.8 = 14 cores. Fits within 16 logical cores with minimal headroom.

**Implication:** Adding a 6th concurrent live stream will cause CPU contention, leading to encoder slowdown and backpressure. The system will gracefully degrade (slow transcode → ingest backpressure → encoder buffering) rather than crash.

### 5.3 Memory Allocation Model

```
Total: 32 GB

Allocation:
  ├── OS + kernel buffers: 2 GB
  ├── FFmpeg shared libraries (.so): ~100 MB
  ├── x264 encoder memory (15 instances): ~300 MB
  ├── Pipeline channel buffers: ~170 MB
  ├── LRU cache: 512 MB
  ├── Tokio runtime + app: ~200 MB
  ├── Reserved headroom: ~28.7 GB
  └── Total used: ~3.3 GB
```

Memory is not a bottleneck. The large headroom provides safety for:
- OS page cache (improves S3 SDK performance).
- Burst allocations during high-complexity video scenes.
- Concurrent VOD transcode jobs.

### 5.4 Disk I/O Model

```
Write path (VOD uploads):
  └── Upload rate: ~100 MB/s (limited by network, not disk)
  └── NVMe SSD write: ~2 GB/s (ample headroom)

Read path (VOD transcode):
  └── FFmpeg read rate: ~50 MB/s per transcode job
  └── 10 concurrent uploads + 5 concurrent reads = ~1 GB/s peak
  └── NVMe SSD read: ~3 GB/s (sufficient)
```

Disk is not a bottleneck. The SSD is used only for temp files, not persistent media storage.

### 5.5 Network I/O Model

```
Ingest:  5 streams × 20 Mbps = 100 Mbps inbound
Storage: 5 streams × 3 renditions × ~4 Mbps = 60 Mbps to S3
Delivery: 500 segment requests/s × 2 MB avg × 8 = highly variable
           With CDN: ~100 Mbps from origin
           Without CDN: up to 800 Mbps
Total: ~260 Mbps with CDN, up to ~960 Mbps without CDN
```

**Network is a concern without CDN.** Direct-to-origin delivery at scale will saturate the 1 Gbps NIC. A CDN is strongly recommended for any deployment with >100 concurrent viewers.

---

## 6. Performance Testing Strategy

### 6.1 Load Testing Scenarios

| Scenario | Setup | Success Criteria |
|----------|-------|-----------------|
| **Single stream baseline** | 1 RTMP stream at 8 Mbps 1080p | Transcode keeps up in real-time; no backpressure events |
| **Max concurrent streams** | 5 simultaneous RTMP streams at 8 Mbps | All streams transcode in real-time; CPU < 95% |
| **Overload** | 8 simultaneous RTMP streams | Graceful degradation: backpressure events logged; no crash; no OOM |
| **Storage stall** | 5 streams + S3 artificially delayed by 5s | Channels fill; backpressure propagates; streams recover when S3 returns |
| **VOD burst** | 5 concurrent 1-hour VOD uploads | All uploads accepted; transcode completes within 2× real-time |
| **Delivery peak** | 500 concurrent segment requests with JMeter | p99 latency < 50ms; no 5xx errors |

### 6.2 Performance Monitoring Checklist

During load testing, monitor:
- [ ] CPU utilization per core (should be evenly distributed)
- [ ] Memory RSS (should remain stable, not growing)
- [ ] Channel utilization (should be < 0.8 under normal load)
- [ ] Transcode FPS per rendition (should be ≥ source FPS for live)
- [ ] S3 PUT latency (should be < 500ms p99)
- [ ] Origin response latency (should be < 50ms p99 for cached segments)
- [ ] TCP connection count (should be within limits)
- [ ] Tokio task count (should be proportional to active streams)

---

## 7. Performance Tuning Guide

### 7.1 If Transcode Can't Keep Up (CPU Bottleneck)

| Action | Impact | Trade-off |
|--------|--------|-----------|
| Reduce x264 preset to `faster` | +40% encode speed | -10% visual quality at same bitrate |
| Reduce profile ladder to 2 rungs | -33% CPU usage | Loss of 480p rendition (mobile users affected) |
| Reduce max concurrent streams | Linear CPU reduction | Fewer simultaneous streams |
| Increase machine cores | Linear throughput increase | Higher cost |
| Disable B-frames (`max_b_frames=0`) | +15% encode speed | -15% compression efficiency (higher bitrate or lower quality) |

### 7.2 If Storage Is Slow (I/O Bottleneck)

| Action | Impact | Trade-off |
|--------|--------|-----------|
| Increase channel buffer sizes | Absorbs longer stalls | More memory usage |
| Use local SSD cache before S3 | Sub-ms write latency | Requires disk space; adds write-ahead complexity |
| Use S3 Transfer Acceleration | Better S3 latency | Additional AWS cost |
| Use regional S3 endpoint | Reduce network latency | Must deploy in same region |

### 7.3 If Origin Is Slow (Delivery Bottleneck)

| Action | Impact | Trade-off |
|--------|--------|-----------|
| Increase LRU cache size | More cache hits | More memory |
| Deploy CDN | Offloads 90%+ of delivery traffic | External dependency and cost |
| Increase Tokio worker threads | More concurrent request handling | Diminishing returns past 2× CPU cores |

---

## Definition of Done — Performance and Backpressure

- [x] Live latency target defined (≤ 18s) with per-stage breakdown
- [x] VOD processing time target defined (≤ 2× real-time)
- [x] Throughput targets for ingest, transcode, and delivery
- [x] Buffer inventory with capacity, size estimate, and rationale for each buffer
- [x] Total memory budget calculated
- [x] Backpressure chain documented for both live and VOD paths
- [x] Backpressure metrics defined with alert thresholds
- [x] Reference machine spec with CPU/memory/disk/network
- [x] CPU, memory, disk, and network allocation models
- [x] Resource bottleneck analysis (CPU is the bottleneck)
- [x] Performance testing scenarios defined
- [x] Performance tuning guide with trade-offs
