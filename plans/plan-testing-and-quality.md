# Feature: Testing Strategy and Quality Gates

## 1. Purpose

Guarantee media correctness, API correctness, and operational resilience through automated unit/integration/E2E testing and CI quality gates.

## 2. Responsibilities

- Implement test pyramid across modules and system boundaries
- Validate media outputs (decodability, manifest correctness, timing)
- Validate failure modes (disconnects, storage outages, overload, shutdown)
- Enforce CI gates for lint, tests, security, and build integrity

## 3. Non-Responsibilities

- Not a replacement for production monitoring
- Not performance optimization logic itself (covered in performance feature)

## 4. Architecture Design

```
Unit tests (fast, isolated)
  -> Integration tests (real FFmpeg + MinIO)
  -> E2E tests (full docker stack)
  -> CI gates + reports
```

## 5. Core Data Structures (Rust/Test Infra)

- Shared test fixtures and fixture metadata
- Mock interfaces (`MediaStore`, transcoder abstraction for unit tests)
- E2E harness helpers for API calls and FFmpeg process control
- Golden files for manifests/fMP4 structure checks

## 6. Public Interfaces

- Test entry points:
  - `cargo test --lib --bins`
  - integration and E2E test targets
- CI workflow jobs and gate thresholds

## 7. Internal Algorithms

1. Build deterministic fixtures via script.
2. Run module unit suites with no external dependencies.
3. Run integration suites against FFmpeg + MinIO.
4. Run E2E scenarios: live, VOD, concurrent streams, failure injection.
5. Run media validation probes (`ffprobe`, manifest consistency checks).
6. Fail CI on gate violations.

## 8. Persistence Model

- Test artifacts and logs persisted in CI artifacts for debugging.
- Golden files versioned in repository.

## 9. Concurrency Model

- Controlled test parallelism to avoid media fixture and port collisions
- Isolated temp directories and ephemeral ports per test case

## 10. Configuration

- Feature flags/env vars for enabling integration/E2E suites
- Fixture path and external dependency configuration
- CI job matrix parameters and timeouts

## 11. Observability

- CI publishes pass/fail and timing per stage
- E2E logs/metrics captured for failed runs
- Failure-injection tests assert expected metric/log signatures

## 12. Testing Strategy

- Unit: state transitions, parsers, validators, manifest generation, auth checks
- Integration: transcode/package/storage correctness with real dependencies
- E2E: complete user flows and deletion lifecycle
- Fault tests: storage down, RTMP disconnects, disk pressure, SIGTERM drain

## 13. Implementation Plan

1. Implement and document fixture generation scripts.
2. Establish module-level unit coverage baseline.
3. Build integration harness with FFmpeg + MinIO.
4. Build E2E suites for live/VOD/concurrency core scenarios.
5. Implement media-correctness validators in tests.
6. Configure CI gates and artifact retention.
7. Add periodic reliability/failure-injection jobs.

## 14. Open Questions

- Whether to make code coverage threshold blocking in MVP or keep informational until baseline stabilizes.
