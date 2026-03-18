# Feature: Configuration, Hot Reload, and Runtime Lifecycle

## 1. Purpose

Provide deterministic startup, configuration layering, selective hot reload, and graceful shutdown behavior for safe operation.

## 2. Responsibilities

- Load configuration from defaults + environment overrides
- Validate config at startup with clear failures
- Support selective runtime reload for safe fields
- Enforce graceful shutdown ordering across components

## 3. Non-Responsibilities

- No distributed config service in MVP
- No runtime reload for unsafe infrastructure fields (ports/storage endpoints)

## 4. Architecture Design

```
Config files/env
  -> parse + validate
  -> shared immutable config snapshot
  -> watch/reload path for reloadable fields
  -> propagate updates via watch channels
```

## 5. Core Data Structures (Rust)

- `AppConfig` with nested sections (`api`, `rtmp`, `storage`, `transcode`, `obs`, `security`)
- `ReloadableConfig` projection for hot-reload-safe fields
- `ShutdownCoordinator` with global and per-stream cancellation tokens

## 6. Public Interfaces

- Startup config loader API
- Reload trigger handling (e.g., `SIGHUP` in Unix deployment)
- Health/readiness endpoints exposing runtime state

## 7. Internal Algorithms

1. Load layered config and run strict validation.
2. Build app context from validated config.
3. On reload trigger, parse+validate new snapshot.
4. Diff old/new and apply only reload-safe keys.
5. Broadcast updates over watch channels.
6. On shutdown signal, stop intake first, drain active streams, flush final artifacts, then exit.

## 8. Persistence Model

- Config persisted as files/environment outside process.
- Runtime snapshot held in memory.

## 9. Concurrency Model

- Read-mostly shared config with atomic snapshot swap for reloadable fields
- Watch channel subscribers per subsystem
- Coordinated cancellation tokens for shutdown

## 10. Configuration

- Required keys for ports, storage, auth, ladder, observability
- Reload-safe keys (tokens/log level/cors/profile defaults for new streams)
- Restart-required keys (bind ports/storage endpoint/runtime thread model)

## 11. Observability

- Startup config validation results (without exposing secrets)
- Reload success/failure counters and logs
- Graceful shutdown duration metrics and incomplete-drain warnings

## 12. Testing Strategy

- Unit: config parsing, defaulting, and validation constraints
- Integration: reload path applies allowed keys only
- Reliability: graceful shutdown during active live/VOD workloads
- Negative tests: invalid reload payload does not break running state

## 13. Implementation Plan

1. Define full config schema and validation rules.
2. Implement layered loader and startup gating checks.
3. Implement reload diff engine and watch-channel propagation.
4. Implement shutdown coordinator and deterministic stop sequence.
5. Instrument config/reload/shutdown paths.
6. Add tests for reload safety matrix and drain semantics.
7. Document runbook actions for reload vs restart decisions.

## 14. Open Questions

- Whether to expose an authenticated admin reload endpoint in addition to signal-based reload.
