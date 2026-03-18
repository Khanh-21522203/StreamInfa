# Feature: Observability (Metrics, Logs, Traces, SLOs)

## 1. Purpose

Provide enough telemetry to debug per-stream failures, identify bottlenecks, and monitor service-level objectives in production.

## 2. Responsibilities

- Emit Prometheus metrics across ingest/transcode/package/storage/delivery/control
- Emit structured logs with stable field schema and redaction
- Emit distributed traces for cross-component request/stream flows
- Define SLIs/SLOs, dashboard structure, and alert baseline

## 3. Non-Responsibilities

- No vendor-locked observability stack implementation
- No business analytics/QoE product analytics in MVP

## 4. Architecture Design

```
Service components
  -> metrics registry (/metrics)
  -> structured logger
  -> tracing spans + context propagation
  -> dashboards + alert rules in ops stack
```

## 5. Core Data Structures (Rust)

- `MetricsRegistry` and typed metric handles
- `LogContext` helpers (`request_id`, `stream_id`, component)
- `TraceContext` propagation helpers
- `SloDefinition` model for threshold-driven alert setup

## 6. Public Interfaces

- `GET /metrics` endpoint (Prometheus exposition)
- Log output (JSON in production profile)
- Trace propagation through request headers and internal pipeline events

## 7. Internal Algorithms

1. Register all metrics at startup (counter/gauge/histogram).
2. Attach required labels while controlling cardinality.
3. Emit per-stage timings and queue/backpressure metrics.
4. Correlate logs/metrics/traces using `stream_id` and `request_id`.
5. Cleanup per-stream metric label sets when streams end/deleted.

## 8. Persistence Model

- Metrics are in-memory runtime state.
- Logs and traces exported to external observability stack.

## 9. Concurrency Model

- Metrics/loggers/tracers must be concurrent-safe and lock-light.
- Instrumentation in hot paths should avoid blocking calls.

## 10. Configuration

- Log level/format
- Metrics scrape endpoint/port
- Trace sampling policy
- Dashboard/alert config delivered through operations tooling

## 11. Observability

- This module defines observability policy itself:
  - key golden signals
  - per-component health indicators
  - incident triage correlation patterns

## 12. Testing Strategy

- Unit: metric emission helpers and structured-field formatting
- Integration: scrape `/metrics`, assert key series presence
- Trace tests: ensure context propagation across pipeline boundaries
- Redaction tests: secret-bearing fields never appear in logs

## 13. Implementation Plan

1. Define metric namespace and complete metric catalog constants.
2. Implement instrumentation wrappers for core components.
3. Implement logging facade and mandatory field conventions.
4. Implement tracing setup and context propagation boundaries.
5. Build starter dashboards and alert rules from SLO targets.
6. Add cardinality controls and stream-label cleanup logic.
7. Validate with integration tests and incident-runbook drills.

## 14. Open Questions

- Whether tracing should be always-on in MVP or gated to sampled production profiles only.
