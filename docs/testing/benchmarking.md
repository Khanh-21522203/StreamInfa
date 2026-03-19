# StreamInfa - Benchmarking Guide

> **Purpose:** Run repeatable latency/throughput benchmarks across control, ingest, and delivery paths, with optional Prometheus metric deltas.

## 1. Harness

Entrypoint:

```bash
./scripts/benchmark.sh [control|ingest|delivery|all]
```

Execution mode:
1. Uses local `k6` if installed.
2. Falls back to Docker `grafana/k6:0.50.0`.

Generated artifacts:
1. k6 summary JSON: `.tmp/benchmarks/<run>/...-k6-summary.json`
2. Metrics snapshots (when enabled): `...-metrics-before.prom` and `...-metrics-after.prom`
3. Metrics delta summary (when both snapshots exist): `...-metrics-delta.txt`

## 2. Scenarios

### Control

Measures:
1. Authenticated list-stream throughput/latency.
2. Create/delete lifecycle latency (`control_create_delete_duration_ms`).

Run:

```bash
ADMIN_TOKEN=<admin_token> ./scripts/benchmark.sh control
```

### Ingest (VOD Upload)

Measures:
1. Upload request latency (`vod_upload_request_duration_ms`).
2. Upload-to-ready latency (`vod_upload_to_ready_duration_ms`).
3. Ready success rate (`vod_ready_success`).

Run:

```bash
ADMIN_TOKEN=<admin_token> ./scripts/benchmark.sh ingest
```

Notes:
1. Uses generated fixture `scripts/k6/fixtures/upload-sample.mp4`.
2. Default fixture size is `262144` bytes to stay below control-plane multipart limits.

### Delivery

Measures:
1. Master/media/init/segment request latency and throughput.
2. Range behavior (optional ranged segment request).

Run:

```bash
DELIVERY_STREAM_ID=<uuidv7> ./scripts/benchmark.sh delivery
```

If `DELIVERY_STREAM_ID` is omitted and `AUTO_PREPARE_DELIVERY=1`, the harness uploads a fixture, waits for `READY`, and uses that stream automatically.

## 3. Key Environment Variables

Core:
1. `BASE_URL` (default `http://localhost:8080`)
2. `DURATION` (default `30s`)
3. `VUS` (default `20`)
4. `ADMIN_TOKEN` (required for `control`, `ingest`, and auto-prep delivery)

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

Metrics snapshots:
1. `METRICS_SNAPSHOT` (default `1`)
2. `METRICS_URL` (default `http://localhost:8080/metrics`)
3. `BENCH_OUT_DIR` (default `.tmp/benchmarks/<timestamp>`)

## 4. Coverage Matrix (What To Watch)

Control-plane HTTP:
1. `http_reqs`, `http_req_duration`, `http_req_failed`
2. `control_create_delete_duration_ms`

Ingest/transcode/package/storage:
1. `streaminfa_upload_duration_seconds`, `streaminfa_upload_size_bytes`
2. `streaminfa_package_segments_written_total`, `streaminfa_package_manifest_updates_total`
3. `streaminfa_storage_put_bytes_total`, `streaminfa_storage_put_duration_seconds`
4. `streaminfa_transcode_latency_seconds`, `streaminfa_transcode_errors_total`

Delivery/cache:
1. `streaminfa_delivery_requests_total`, `streaminfa_delivery_request_duration_seconds`
2. `streaminfa_delivery_bytes_sent_total`
3. `streaminfa_cache_hits_total`, `streaminfa_cache_misses_total`

Backpressure/auth/error signals:
1. `streaminfa_backpressure_events_total`
2. `streaminfa_auth_failures_total`
3. `streaminfa_storage_errors_total`

## 5. Metrics Delta Helper

Manual diff:

```bash
bash scripts/benchmark-metrics.sh <before.prom> <after.prom>
```

This reports:
1. Counter deltas in the benchmark window.
2. Histogram delta averages (`_sum / _count`) for upload/storage/delivery/transcode latency buckets.
3. Cache hit ratio over the window.

## 6. Result Table Style

Preferred reporting format:

| Case | avg_ms | p50 | p95 | min | max | qps |
|---|---:|---:|---:|---:|---:|---:|
| `control.http_req_duration` | 1002.705 | 809.130 | 2237.553 | 112.836 | 8920.774 | 10.195 |
| `ingest.upload_to_ready_duration_ms` | 240.077 | 224.000 | 318.000 | 198.000 | 332.000 | 4.029 |

Column mapping:
1. `p50` = k6 `med`
2. `qps` = scenario request/iteration rate (pick the metric matching that case)
3. If a metric does not expose one column, use `-`

## 7. Docker/Host Networking Tip

When k6 runs in Docker against a host-local StreamInfa:
1. Set `BASE_URL=http://host.docker.internal:8080`
2. Keep `METRICS_URL=http://localhost:8080/metrics` for host-side metric scraping.

## 8. Source Files

1. `scripts/benchmark.sh`
2. `scripts/benchmark-metrics.sh`
3. `scripts/k6/control-plane.js`
4. `scripts/k6/vod-upload.js`
5. `scripts/k6/delivery-origin.js`
