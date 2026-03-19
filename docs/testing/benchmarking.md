# StreamInfa - Benchmarking Guide

> **Purpose:** Run repeatable latency/throughput/capacity/reliability benchmarks across control, ingest, transcode/package, storage, delivery, and operational paths.

## 1. Harness

Entrypoints:

```bash
./scripts/benchmark.sh [control|ingest|delivery|soak|overload|live-rtmp|fault|capacity|ops|micro|all|all-plus]
```

Execution mode:
1. k6 scenarios use local `k6` if installed.
2. Otherwise they fall back to Docker `grafana/k6:0.50.0`.
3. Non-k6 helpers are plain shell workflows (`live-rtmp`, `fault`, `capacity`, `ops`, `micro`).

Generated artifacts:
1. k6 summary JSON: `.tmp/benchmarks/<run>/...-k6-summary.json`
2. Metrics snapshots (when enabled): `...-metrics-before.prom` and `...-metrics-after.prom`
3. Metrics delta summary (when both snapshots exist): `...-metrics-delta.txt`
4. Helper summaries:
   - live RTMP: `live-rtmp-<ts>-summary.json`
   - capacity search: `capacity-<scenario>-<ts>-summary.json`
   - ops path: `ops-<ts>-summary.json`
   - microbench: `micro-<ts>.txt`

## 2. Scenario Catalog

### 2.1 Baseline Throughput/Latency

1. `control`: authenticated list + create/delete control API load.
2. `ingest`: VOD upload request latency and upload-to-ready latency.
3. `delivery`: HLS origin read path latency/throughput (master/media/init/segment).

Commands:

```bash
ADMIN_TOKEN=<admin_token> ./scripts/benchmark.sh control
ADMIN_TOKEN=<admin_token> ./scripts/benchmark.sh ingest
ADMIN_TOKEN=<admin_token> DELIVERY_STREAM_ID=<uuidv7> ./scripts/benchmark.sh delivery
```

### 2.2 Soak / Endurance

`soak` runs a long-duration mixed load:
1. Delivery polling against a prepared stream.
2. Low-rate control-plane probes.
3. Success-rate + latency thresholds over long runtime.

Command:

```bash
ADMIN_TOKEN=<admin_token> SOAK_DURATION=30m SOAK_DELIVERY_VUS=20 SOAK_CONTROL_VUS=2 ./scripts/benchmark.sh soak
```

### 2.3 Overload / Backpressure

`overload` runs intentionally high pressure:
1. High-VU delivery reads.
2. Optional control churn (create/delete) if admin token is present.
3. Validates graceful degradation and error-rate bounds rather than perfect latency.

Command:

```bash
ADMIN_TOKEN=<admin_token> OVERLOAD_DURATION=120s OVERLOAD_VUS=120 OVERLOAD_CONTROL_CREATE_VUS=12 ./scripts/benchmark.sh overload
```

### 2.4 Live RTMP End-to-End

`live-rtmp` uses FFmpeg publisher and measures:
1. Create-stream -> RTMP publish start.
2. Time to `master.m3u8` / media playlist availability.
3. Time to first served media segment.

Command:

```bash
ADMIN_TOKEN=<admin_token> LIVE_RTMP_PUBLISH_SECS=45 ./scripts/benchmark.sh live-rtmp
```

Prerequisites:
1. `ffmpeg` available in PATH.
2. RTMP ingest listener reachable on configured host/port.

### 2.5 Fault-Injection Reliability

`fault` runs delivery load while executing an operator-provided fault command.
Typical usage: inject storage/network degradation and optionally recover.

Command:

```bash
ADMIN_TOKEN=<admin_token> \
FAULT_INJECT_CMD='docker compose stop minio' \
FAULT_RECOVER_CMD='docker compose start minio' \
FAULT_INJECT_AFTER_SECS=20 \
FAULT_RECOVER_AFTER_SECS=60 \
./scripts/benchmark.sh fault
```

### 2.6 Capacity Search

`capacity` incrementally increases VUs until threshold failure, then records:
1. Max passing VU level.
2. First failing VU level.

Command:

```bash
ADMIN_TOKEN=<admin_token> CAPACITY_START_VUS=10 CAPACITY_STEP_VUS=10 CAPACITY_MAX_VUS=200 ./scripts/benchmark.sh capacity
```

### 2.7 Operational Path

`ops` measures startup/readiness/reload/shutdown timing for operational workflows.
It can benchmark an already running service or a managed process.

Commands:

```bash
# Probe existing service
./scripts/benchmark.sh ops

# Manage process lifecycle under benchmark
OPS_START_CMD='cargo run --release' OPS_RELOAD_CMD='pkill -HUP streaminfa' ./scripts/benchmark.sh ops
```

### 2.8 Pipeline Microbenchmarks

`micro` runs Rust criterion benches for packaging hot paths:
1. Master/media manifest generation.
2. Init segment generation.
3. Segment index update + playlist build.

Command:

```bash
./scripts/benchmark.sh micro
```

### 2.9 Bundled Runs

1. `all`: baseline suite (`control`, `ingest`, `delivery`).
2. `all-plus`: extended suite (`all` + `soak`, `overload`, `capacity`, `ops`, `micro`).

## 3. Key Environment Variables

Core:
1. `BASE_URL` (default `http://localhost:8080`)
2. `DURATION` (default `30s`)
3. `VUS` (default `20`)
4. `CONTROL_BASE_URL` (default `BASE_URL`, auto-maps `host.docker.internal` -> `localhost` for shell-side API calls)
5. `ADMIN_TOKEN` (required for control/ingest/live-rtmp and auto-prep workflows)
6. `BENCH_OUT_DIR` (default `.tmp/benchmarks/<timestamp>`)

Control:
1. `CONTROL_CREATE_VUS` (default `5`)

Ingest:
1. `INGEST_UPLOAD_VUS` (default `2`)
2. `INGEST_READY_TIMEOUT_SECS` (default `120`)
3. `INGEST_POLL_INTERVAL_MS` (default `500`)
4. `UPLOAD_SAMPLE_SIZE_BYTES` (default `262144`)

Delivery:
1. `DELIVERY_STREAM_ID`
2. `DELIVERY_RENDITION` (default `high`)
3. `DELIVERY_SEGMENT` (default `000000.m4s`)
4. `DELIVERY_USE_RANGE` (`1` or `0`)
5. `AUTO_PREPARE_DELIVERY` (default `1`)

Soak:
1. `SOAK_DURATION` (default `DURATION`)
2. `SOAK_DELIVERY_VUS` (default `VUS`)
3. `SOAK_CONTROL_VUS` (default `2`)

Overload:
1. `OVERLOAD_DURATION` (default `120s`)
2. `OVERLOAD_VUS` (default `80`)
3. `OVERLOAD_CONTROL_CREATE_VUS` (default `8`)

Fault:
1. `FAULT_INJECT_CMD` (required)
2. `FAULT_RECOVER_CMD` (optional)
3. `FAULT_INJECT_AFTER_SECS` (default `20`)
4. `FAULT_RECOVER_AFTER_SECS` (default `60`)

Capacity:
1. `CAPACITY_START_VUS` (default `10`)
2. `CAPACITY_STEP_VUS` (default `10`)
3. `CAPACITY_MAX_VUS` (default `200`)
4. `CAPACITY_DURATION` (default `20s`)
5. `CAPACITY_P95_MS` (default `400`)
6. `CAPACITY_ERROR_RATE_MAX` (default `0.01`)

Ops:
1. `OPS_START_CMD`
2. `OPS_STOP_CMD`
3. `OPS_RELOAD_CMD`
4. `OPS_AUTO_RELOAD_SIGNAL`
5. `OPS_READY_TIMEOUT_SECS`
6. `OPS_PROBE_COUNT`
7. `OPS_REQUIRE_READY` (`1` by default; set `0` to benchmark health/probe timings even when readiness is degraded)

Metrics snapshots:
1. `METRICS_SNAPSHOT` (default `1`)
2. `METRICS_URL` (default `http://localhost:8080/metrics`)

## 4. Coverage Matrix (What To Watch)

Control-plane HTTP:
1. `http_reqs`, `http_req_duration`, `http_req_failed`
2. `control_create_delete_duration_ms`

Ingest/transcode/package/storage:
1. `streaminfa_upload_duration_seconds`, `streaminfa_upload_size_bytes`
2. `streaminfa_transcode_latency_seconds`, `streaminfa_transcode_errors_total`
3. `streaminfa_package_segments_written_total`, `streaminfa_package_manifest_updates_total`
4. `streaminfa_storage_put_duration_seconds`, `streaminfa_storage_errors_total`

Delivery/cache:
1. `streaminfa_delivery_requests_total`, `streaminfa_delivery_request_duration_seconds`
2. `streaminfa_delivery_bytes_sent_total`
3. `streaminfa_cache_hits_total`, `streaminfa_cache_misses_total`

Backpressure/reliability:
1. `streaminfa_backpressure_events_total`
2. `streaminfa_auth_failures_total`
3. fault scenario threshold pass/fail and recovery timing

Operational:
1. startup to `/healthz` and `/readyz`
2. reload command latency and re-ready timing
3. shutdown timing

## 5. Metrics Delta Helper

Manual diff:

```bash
bash scripts/benchmark-metrics.sh <before.prom> <after.prom>
```

Reports:
1. Counter deltas in benchmark window.
2. Histogram delta averages (`_sum / _count`) for upload/storage/delivery/transcode.
3. Cache hit ratio over benchmark window.

## 6. Result Table Style

Preferred reporting format:

| Case | avg_ms | p50 | p95 | min | max | qps |
|---|---:|---:|---:|---:|---:|---:|
| `control.http_req_duration` | 1002.705 | 809.130 | 2237.553 | 112.836 | 8920.774 | 10.195 |
| `ingest.upload_to_ready_duration_ms` | 240.077 | 224.000 | 318.000 | 198.000 | 332.000 | 4.029 |

Column mapping:
1. `p50` = k6 `med`
2. `qps` = request/iteration rate for that case
3. If a metric does not expose one column, use `-`

## 7. Docker/Host Networking Tip

When k6 runs in Docker against a host-local StreamInfa:
1. Use `BASE_URL=http://host.docker.internal:8080`
2. Keep `METRICS_URL=http://localhost:8080/metrics` for host-side metric scraping.

## 8. Source Files

1. `scripts/benchmark.sh`
2. `scripts/benchmark-metrics.sh`
3. `scripts/benchmark-live-rtmp.sh`
4. `scripts/benchmark-fault-injection.sh`
5. `scripts/benchmark-capacity.sh`
6. `scripts/benchmark-ops-path.sh`
7. `scripts/benchmark-micro.sh`
8. `scripts/k6/control-plane.js`
9. `scripts/k6/vod-upload.js`
10. `scripts/k6/delivery-origin.js`
11. `scripts/k6/soak-mixed.js`
12. `scripts/k6/overload-backpressure.js`
13. `scripts/k6/capacity-delivery.js`
14. `scripts/k6/fault-delivery.js`
15. `benches/pipeline_micro.rs`
