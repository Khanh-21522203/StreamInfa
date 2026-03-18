# Feature: Implementation Execution Order

## 1. Purpose

Provide a single implementation sequence across all StreamInfa feature plans, including priority, dependencies, and a realistic MVP timeline.

## 2. Planning Assumptions

- Scope is MVP only (as defined in `docs/requirements.md`).
- One mid-level SWE is the primary implementer.
- Timeline estimates include coding + test automation + basic hardening.
- Some phases can overlap after core dependencies are stable.

## 3. Priority Tiers

## Tier 0 (Foundation)

1. `plan-configuration-and-runtime.md`
2. `plan-media-lifecycle.md`
3. `plan-control-plane.md`

## Tier 1 (Core Media Path)

1. `plan-live-ingest-rtmp.md`
2. `plan-vod-upload-ingest.md`
3. `plan-transcoding-and-packaging.md`
4. `plan-storage-and-delivery.md`

## Tier 2 (Cross-Cutting Hardening)

1. `plan-security.md`
2. `plan-observability.md`
3. `plan-performance-and-backpressure.md`

## Tier 3 (Release Confidence)

1. `plan-testing-and-quality.md`
2. `plan-deployment-and-operations.md`

## 4. Dependency Graph

| Plan | Depends On | Why |
|---|---|---|
| `plan-configuration-and-runtime.md` | — | Base app wiring, config, reload/shutdown primitives |
| `plan-media-lifecycle.md` | configuration-and-runtime | Shared stream state model and orchestration hooks need runtime context |
| `plan-control-plane.md` | media-lifecycle, configuration-and-runtime | APIs expose and drive lifecycle state |
| `plan-live-ingest-rtmp.md` | control-plane, security (baseline), media-lifecycle | RTMP publish auth and stream-state events depend on control/lifecycle |
| `plan-vod-upload-ingest.md` | control-plane, security (baseline), configuration-and-runtime | Upload endpoint is control API surface with validation and temp-storage config |
| `plan-transcoding-and-packaging.md` | live-ingest-rtmp or vod-upload-ingest, media-lifecycle | Needs normalized ingest frames/jobs and lifecycle transitions |
| `plan-storage-and-delivery.md` | transcoding-and-packaging, control-plane | Stores and serves packaged artifacts; delete flow controlled by API |
| `plan-security.md` | control-plane, live-ingest-rtmp, vod-upload-ingest | Security gates must align with both ingest and API surfaces |
| `plan-observability.md` | control-plane + ingest + transcode + storage | Telemetry wiring spans all major components |
| `plan-performance-and-backpressure.md` | ingest + transcode + storage + observability | Requires real pipeline boundaries and metrics to tune |
| `plan-testing-and-quality.md` | all core features | CI gates and E2E require end-to-end pipeline availability |
| `plan-deployment-and-operations.md` | testing-and-quality, observability, security | Release + runbook quality depends on tested, observable, secured service |

## 5. Recommended Implementation Sequence

1. Phase A: Foundation (Week 1-2)
   - Build configuration/runtime lifecycle.
   - Build stream lifecycle state machine.
   - Build control-plane APIs with health/readiness shell.

2. Phase B: Ingest + Process (Week 3-5)
   - Implement RTMP ingest path.
   - Implement VOD upload path.
   - Implement transcoding + HLS packaging.

3. Phase C: Store + Serve (Week 6)
   - Implement object storage integration.
   - Implement origin delivery routes and range support.

4. Phase D: Hardening (Week 7-8)
   - Complete security controls and key rotation.
   - Add full observability catalog.
   - Tune backpressure and capacity defaults.

5. Phase E: Quality + Operability (Week 9-10)
   - Implement test pyramid and CI gates.
   - Finalize deployment/rollback/runbooks.
   - Execute release-readiness checks.

## 6. Parallelization Opportunities

- Security middleware can start in Phase A and be expanded in Phase D.
- Observability scaffolding can begin in Phase A and be completed incrementally.
- Test harness work can begin in Phase B while media pipeline is still being completed.

## 7. Exit Criteria Per Phase

- Phase A exit: API can create/list/get/delete placeholder streams with valid lifecycle transitions.
- Phase B exit: live RTMP and VOD upload both produce valid packaged outputs locally.
- Phase C exit: playback works through `/streams/{id}/master.m3u8` from object storage.
- Phase D exit: auth, metrics, logs, and backpressure alerts are validated under fault scenarios.
- Phase E exit: CI gates pass, rollback tested, runbooks updated, MVP acceptance criteria satisfied.

## 8. Risks and Mitigations

- FFmpeg integration risk: start integration tests early with real fixtures.
- Backpressure regressions: enforce bounded channels + queue saturation alerts.
- Scope creep: lock to MVP codecs/protocols and defer roadmap items explicitly.

## 9. Definition of Done

- [x] All plan files mapped into a dependency-aware sequence.
- [x] Priority tiers defined for implementation order.
- [x] Timeline estimated for a mid-level SWE.
- [x] Phase exit criteria and key risks documented.
