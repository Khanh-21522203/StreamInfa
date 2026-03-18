# Feature: Performance Targets and Backpressure Control

## 1. Purpose

Meet MVP latency/throughput goals under bounded resources by designing explicit buffering and backpressure propagation across the media pipeline.

## 2. Responsibilities

- Encode target latency/throughput objectives into measurable checks
- Define bounded queue strategy and backpressure behavior
- Define per-stage resource budgets (CPU, memory, I/O)
- Provide tuning and overload-degradation guidance

## 3. Non-Responsibilities

- No auto-scaler implementation in MVP
- No multi-node distributed scheduler in MVP

## 4. Architecture Design

```
Ingest -> Transcode -> Package -> Storage -> Delivery
  each boundary uses bounded queue + metrics
  queue pressure propagates upstream (no unbounded buffering)
```

## 5. Core Data Structures (Rust)

- `BoundedQueueConfig` per stage
- `BackpressureSignal` events and thresholds
- `CapacityProfile` (reference machine assumptions)
- `PerformanceSnapshot` for periodic health measurements

## 6. Public Interfaces

- Internal config and metrics interfaces only
- External visibility via `/metrics` and performance dashboards

## 7. Internal Algorithms

1. Configure bounded channels for every stage boundary.
2. When queue high-watermark is crossed, throttle upstream producer.
3. If pressure persists, shed or fail stream/job gracefully rather than OOM.
4. Emit queue utilization and stage latency metrics continuously.
5. Trigger alerts when latency and utilization breach SLO windows.

## 8. Persistence Model

- No persistence; runtime control and telemetry only.

## 9. Concurrency Model

- Multi-producer/multi-consumer channels where needed
- Backpressure decisions at producer write points
- Blocking FFmpeg workloads isolated from async runtime worker starvation

## 10. Configuration

- Queue capacities by pipeline edge
- Worker/concurrency limits per stage
- Performance threshold values for warning/critical alerts
- Reference-machine profile for baseline acceptance testing

## 11. Observability

- Queue depth and channel saturation metrics
- End-to-end live latency and VOD processing duration
- CPU/memory utilization and per-stage throughput metrics
- Overload/rejection counters and degradation reason logs

## 12. Testing Strategy

- Load tests at expected and above-capacity stream counts
- Soak tests for sustained live sessions
- Failure-injection tests for slow storage and CPU saturation
- Regression checks for memory growth and queue leak conditions

## 13. Implementation Plan

1. Define performance SLIs and hard capacity assumptions.
2. Implement bounded-queue config and backpressure propagation hooks.
3. Add stage-level throughput/latency instrumentation.
4. Implement overload-handling policies (throttle/fail-fast).
5. Build reproducible performance test harness scenarios.
6. Tune default capacities against reference machine targets.
7. Codify alerts and runbook thresholds.

## 14. Open Questions

- Whether overload policy should prioritize live stream continuity over VOD throughput when both contend for shared CPU.
