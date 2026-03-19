# StreamInfa - Release Readiness Checklist

> **Purpose:** Provide a single, repeatable command checklist before promoting a build.
> **Audience:** Engineers and operators preparing a release candidate.

---

## 1. CI and Build Gates

Run from repository root:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all -- --nocapture
cargo check --features s3
docker build -f docker/Dockerfile -t streaminfa:release-candidate .
```

Security gate:

```bash
cargo audit
```

---

## 2. Local Operability Checks

1. Start local stack:

```bash
docker compose -f docker/docker-compose.yml up -d --build streaminfa
```

2. Run verification:

```bash
STREAMINFA_ADMIN_TOKEN=<admin_token> ./scripts/post_deploy_verify.sh http://localhost:8080
```

3. Validate rollback path:

```bash
STREAMINFA_ADMIN_TOKEN=<admin_token> ./scripts/rollback.sh <previous_image_tag_or_ref>
```

---

## 3. Sign-Off Checklist

- [ ] `ci.yml` passed on target commit.
- [ ] `reliability-smoke.yml` latest run passed.
- [ ] Post-deploy verification completed in staging.
- [ ] Rollback rehearsal completed for the same release line.
- [ ] Runbook updated for any newly observed failure mode.

---

## 4. Workflow Links

1. `.github/workflows/ci.yml`
2. `.github/workflows/reliability-smoke.yml`
3. `docs/devops/deployment.md`
4. `docs/runbooks/runbook.md`
