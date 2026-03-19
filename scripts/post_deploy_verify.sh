#!/usr/bin/env bash

set -euo pipefail

BASE_URL="${1:-http://localhost:8080}"
ADMIN_TOKEN="${2:-${STREAMINFA_ADMIN_TOKEN:-}}"
TIMEOUT_SECS="${STREAMINFA_VERIFY_TIMEOUT_SECS:-60}"
SLEEP_SECS=3

health_url="${BASE_URL%/}/healthz"
ready_url="${BASE_URL%/}/readyz"
metrics_url="${BASE_URL%/}/metrics"
streams_url="${BASE_URL%/}/api/v1/streams"

echo "=== StreamInfa Post-Deploy Verification ==="
echo "Base URL: ${BASE_URL}"

deadline=$((SECONDS + TIMEOUT_SECS))
while true; do
    if curl -fsS "${health_url}" >/dev/null 2>&1 && curl -fsS "${ready_url}" >/dev/null 2>&1; then
        break
    fi

    if (( SECONDS >= deadline )); then
        echo "ERROR: health/readiness checks did not succeed within ${TIMEOUT_SECS}s"
        exit 1
    fi

    sleep "${SLEEP_SECS}"
done

echo "Health/readiness checks passed."

metrics_body="$(curl -fsS "${metrics_url}")"
if [[ "${metrics_body}" != *"streaminfa_"* ]]; then
    echo "ERROR: /metrics response did not include expected streaminfa metric names."
    exit 1
fi
echo "Metrics endpoint check passed."

if [[ -n "${ADMIN_TOKEN}" ]]; then
    echo "Running authenticated create/delete smoke test..."
    create_payload='{"ingest_type":"rtmp","metadata":{"smoke":"post_deploy_verify"}}'
    create_response="$(
        curl -fsS -X POST "${streams_url}" \
            -H "Authorization: Bearer ${ADMIN_TOKEN}" \
            -H "Content-Type: application/json" \
            -d "${create_payload}"
    )"

    stream_id="$(printf '%s' "${create_response}" | sed -nE 's/.*"stream_id"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p')"
    if [[ -z "${stream_id}" ]]; then
        echo "ERROR: smoke create-stream response missing stream_id"
        echo "Response: ${create_response}"
        exit 1
    fi

    curl -fsS -X DELETE "${streams_url}/${stream_id}" \
        -H "Authorization: Bearer ${ADMIN_TOKEN}" >/dev/null
    echo "Authenticated smoke test passed (stream_id=${stream_id})."
else
    echo "STREAMINFA_ADMIN_TOKEN not set; skipping authenticated smoke create/delete test."
fi

echo "Post-deploy verification complete."
