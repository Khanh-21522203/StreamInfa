# StreamInfa - Deployment Guide

> **Purpose:** Define production-oriented deployment, rollout, and rollback workflow for MVP.
> **Audience:** Engineers and operators deploying StreamInfa.

---

## 1. Deployment Model (MVP)

1. Single service instance (modular monolith).
2. External S3-compatible storage.
3. Reverse proxy or load balancer in front of HTTP port for TLS termination.
4. RTMP ingress restricted to trusted encoder networks.

---

## 2. Required Runtime Inputs

### 2.1 Environment Variables

```bash
STREAMINFA_ENV=production
STREAMINFA_AUTH_ADMIN_BEARER_TOKENS=at_prod_token_1,at_prod_token_2
STREAMINFA_STORAGE_ENDPOINT=https://s3.example.com
STREAMINFA_STORAGE_BUCKET=streaminfa-media
STREAMINFA_STORAGE_REGION=us-east-1
STREAMINFA_STORAGE_ACCESS_KEY_ID=...
STREAMINFA_STORAGE_SECRET_ACCESS_KEY=...
```

### 2.2 Config Files

Expected layering:

1. `config/default.toml`
2. `config/production.toml`
3. environment-variable overrides

Keep secrets in environment variables, not committed config files.

---

## 3. Network and Security Baseline

1. Allow inbound `1935` only from encoder CIDRs.
2. Allow inbound `8080` only from reverse proxy/CDN/internal network.
3. Do not expose `/metrics` publicly.
4. Configure CORS to known player origins in production.
5. Keep admin API behind private network access controls.

---

## 4. Release Workflow

1. Build artifact/container image from a tagged commit.
2. Run full test gate (`fmt`, `clippy`, unit, integration, e2e, audit).
3. Push image to registry.
4. Deploy to staging and run smoke checks:
   - `/healthz`
   - `/readyz`
   - create stream and validate playback
5. Deploy to production with rolling replacement.
6. Verify SLO guardrails and error rates for 30-60 minutes.

### 4.1 Operational Scripts

Use these scripts from repository root for repeatable operations:

```bash
./scripts/deploy.sh <image_tag>
./scripts/post_deploy_verify.sh http://localhost:8080 "$STREAMINFA_ADMIN_TOKEN"
./scripts/rollback.sh <previous_image_tag_or_ref>
```

Notes:
1. `deploy.sh` performs pull, graceful stop/start, and invokes post-deploy verification.
2. `post_deploy_verify.sh` checks `/healthz`, `/readyz`, `/metrics`, and optional authenticated create/delete smoke flow.
3. `rollback.sh` restores the previous image and then runs the same verification checks.

---

## 5. Post-Deploy Verification

1. Health/readiness return 200.
2. Metrics scrape succeeds.
3. No spike in:
   - `streaminfa_storage_errors_total`
   - `streaminfa_delivery_requests_total{status="5xx"}`
   - `streaminfa_panic_total`
4. Live stream can be created and played.
5. VOD upload reaches `ready`.

Recommended command:

```bash
STREAMINFA_ADMIN_TOKEN=<admin_token> ./scripts/post_deploy_verify.sh http://localhost:8080
```

---

## 6. Rollback Procedure

1. Trigger rollback immediately if:
   - service unavailable > 5 minutes,
   - delivery 5xx > 1% sustained,
   - panics detected after deploy.
2. Revert to previous known-good image/tag.
3. Restart service with previous config bundle.
4. Validate health/readiness and playback flow.
5. Open incident review with root cause and mitigation tasks.

Recommended command:

```bash
STREAMINFA_ADMIN_TOKEN=<admin_token> ./scripts/rollback.sh <previous_image_tag_or_ref>
```

---

## 7. Config Reload vs Restart

Hot reload (`SIGHUP`) allowed for:

1. Admin tokens
2. Stream keys
3. Log level
4. CORS origins
5. Profile ladder / segment duration for new streams only

Restart required for:

1. HTTP/RTMP ports
2. Storage endpoint/bucket/credentials
3. Runtime thread limits

---

## 8. Production Readiness Checklist

- [ ] TLS termination configured at proxy/load balancer
- [ ] Security group/firewall rules applied
- [ ] S3 bucket lifecycle and access policies validated
- [ ] Metrics/log shipping verified
- [ ] Alert rules enabled and routed
- [ ] Backup and retention policy documented
- [ ] Rollback tested at least once

## 9. CI Gate Mapping

Phase E CI workflows are in:
1. `.github/workflows/ci.yml`
2. `.github/workflows/reliability-smoke.yml`

Current gate coverage:
1. Formatting (`cargo fmt --all --check`)
2. Lint (`cargo clippy --all-targets -- -D warnings`)
3. Tests (`cargo test --all`)
4. Build check (`cargo check --features s3`)
5. Dependency audit (`cargo audit`)
6. Docker image build (`docker build`)
