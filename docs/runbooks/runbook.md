# StreamInfa - Operational Runbook

> **Purpose:** Provide incident response playbooks for common failures.
> **Audience:** On-call engineers and operators.

---

## 1. Fast Triage

1. Check service health:
   - `GET /healthz`
   - `GET /readyz`
2. Check critical metrics:
   - `streaminfa_panic_total`
   - `streaminfa_storage_errors_total`
   - `streaminfa_delivery_requests_total{status="5xx"}`
   - `streaminfa_channel_utilization`
3. Inspect recent logs for:
   - auth failures
   - storage failures
   - ffmpeg/transcode errors

Quick command bundle:

```bash
curl -fsS http://localhost:8080/healthz
curl -fsS http://localhost:8080/readyz
curl -fsS http://localhost:8080/metrics | head -50
docker logs --tail=200 streaminfa
```

---

## 2. Incident Severity

1. Sev-1:
   - process down,
   - all live streams failing,
   - sustained delivery 5xx > 1%.
2. Sev-2:
   - degraded transcode throughput,
   - high latency/backpressure but partial service still functional.
3. Sev-3:
   - non-critical feature degradation, no user-visible outage.

---

## 3. Playbook: Service Unreachable

Symptoms:

1. `up == 0` in monitoring.
2. `/healthz` not reachable.

Actions:

1. Restart service/container.
2. Check crash logs and panic metrics.
3. Validate storage dependency reachability.
4. If restart fails, rollback to last known-good release.
5. Run post-deploy verification after recovery.

Recommended commands:

```bash
docker restart streaminfa
STREAMINFA_ADMIN_TOKEN=<admin_token> ./scripts/post_deploy_verify.sh http://localhost:8080
```

Exit criteria:

1. `/healthz` and `/readyz` return 200.
2. New stream can be created and played.

---

## 4. Playbook: Storage Backend Failure

Symptoms:

1. `streaminfa_storage_errors_total` rising quickly.
2. Delivery 404/502 for fresh segments.
3. Readiness fails storage check.

Actions:

1. Verify S3/MinIO endpoint health and credentials.
2. Check network path and DNS resolution.
3. Temporarily reduce ingest load if backpressure is climbing.
4. Restore storage service; verify retries recover.
5. If unresolved > 5 minutes, declare Sev-1 and rollback/degrade intake.
6. If rollback is required, use scripted rollback path.

Rollback command:

```bash
STREAMINFA_ADMIN_TOKEN=<admin_token> ./scripts/rollback.sh <previous_image_tag_or_ref>
```

Exit criteria:

1. Storage PUT/GET latencies back within normal range.
2. Segment publication resumes.

---

## 5. Playbook: Backpressure / Transcode Overload

Symptoms:

1. `streaminfa_channel_utilization > 0.8`.
2. `streaminfa_transcode_fps < source_fps`.
3. Live latency grows and players lag.

Actions:

1. Confirm CPU saturation on host.
2. Reduce concurrent live streams (temporary admission control).
3. Consider lowering encode complexity (preset/rendition count) for emergency mitigation.
4. Confirm storage latency is not the root bottleneck.

Exit criteria:

1. Channel utilization stabilizes below 0.8.
2. Transcode returns to real-time.

---

## 6. Playbook: Auth Failure Spike

Symptoms:

1. `streaminfa_auth_failures_total` spike.
2. Repeated 401/403 responses in API logs.

Actions:

1. Validate token/key rotation did not remove active credentials.
2. Check for misconfigured clients using stale tokens/keys.
3. If suspicious traffic, tighten network ACLs and rotate secrets.

Exit criteria:

1. Failure rate returns to baseline.
2. Legitimate clients authenticate successfully.

---

## 7. Playbook: Disk Pressure (VOD Upload Path)

Symptoms:

1. Readiness disk check failing.
2. Uploads failing with `507 Insufficient Storage`.

Actions:

1. Clear stale temp artifacts.
2. Pause/limit concurrent VOD uploads.
3. Expand disk if needed.
4. Verify cleanup job is running.

Exit criteria:

1. Disk free space exceeds configured minimum.
2. Upload success recovers.

---

## 8. Post-Incident Checklist

1. Record timeline and root cause.
2. Capture impacted streams/jobs and blast radius.
3. Define permanent fixes with owners and due dates.
4. Add or adjust alert thresholds if detection lagged.
5. Update this runbook if gaps were discovered.

## 9. Script Reference

1. Deploy: `./scripts/deploy.sh <image_tag>`
2. Verify: `./scripts/post_deploy_verify.sh http://localhost:8080 "$STREAMINFA_ADMIN_TOKEN"`
3. Rollback: `./scripts/rollback.sh <previous_image_tag_or_ref>`
