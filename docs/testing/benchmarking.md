# StreamInfa - Benchmarking Guide

> **Purpose:** Run repeatable latency and throughput benchmarks for control-plane and delivery paths.
> **Audience:** Engineers validating performance before releases or tuning changes.

---

## 1. Benchmark Harness

Benchmark entrypoint:

```bash
./scripts/benchmark.sh [control|delivery|all]
```

Backends:
1. Uses local `k6` if installed.
2. Falls back to Docker image `grafana/k6:0.50.0`.

---

## 2. Control Plane Benchmark

Measures:
1. Authenticated list-stream throughput/latency.
2. Create+delete stream lifecycle latency.

Run:

```bash
ADMIN_TOKEN=<admin_token> ./scripts/benchmark.sh control
```

Tunables:
1. `BASE_URL` (default `http://localhost:8080`)
2. `DURATION` (default `30s`)
3. `VUS` (default `20`, list-stream scenario)
4. `CONTROL_CREATE_VUS` (default `5`, create/delete scenario)

---

## 3. Delivery Benchmark

Measures:
1. Master/media playlist read latency.
2. Init/segment delivery latency and request throughput.

Requirements:
1. A stream with playable artifacts already available in storage.

Run:

```bash
DELIVERY_STREAM_ID=<uuidv7> ./scripts/benchmark.sh delivery
```

Tunables:
1. `DELIVERY_RENDITION` (default `high`)
2. `DELIVERY_SEGMENT` (default `000000.m4s`)
3. `DELIVERY_USE_RANGE` (`1` by default; set `0` to disable ranged segment request)
4. `BASE_URL`, `DURATION`, `VUS`

---

## 4. Interpreting Results

Key k6 outputs:
1. `http_reqs`: request throughput.
2. `http_req_duration`: latency distribution (`p(95)`, `p(99)`).
3. `http_req_failed`: error rate.
4. `data_received`: transfer throughput signal for delivery path.

Custom metric:
1. `control_create_delete_duration_ms` for end-to-end control lifecycle latency.

---

## 5. Source Files

1. `scripts/benchmark.sh`
2. `scripts/k6/control-plane.js`
3. `scripts/k6/delivery-origin.js`
