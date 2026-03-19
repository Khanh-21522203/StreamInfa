#!/usr/bin/env bash

set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "Usage: $0 <previous_image_tag_or_ref>"
    echo "Examples:"
    echo "  $0 v0.1.0"
    echo "  $0 ghcr.io/streaminfa/streaminfa:v0.1.0"
    exit 1
fi

REGISTRY="${STREAMINFA_REGISTRY:-ghcr.io/streaminfa}"
CONTAINER_NAME="${STREAMINFA_CONTAINER_NAME:-streaminfa}"
ENV_FILE="${STREAMINFA_ENV_FILE:-/etc/streaminfa/env}"
UPLOAD_VOLUME="${STREAMINFA_UPLOAD_VOLUME:-/tmp/streaminfa/uploads}"

raw_ref="$1"
if [[ "${raw_ref}" == *"/"* || "${raw_ref}" == *":"* ]]; then
    image_ref="${raw_ref}"
else
    image_ref="${REGISTRY}/streaminfa:${raw_ref}"
fi

echo "=== StreamInfa Rollback ==="
echo "Container: ${CONTAINER_NAME}"
echo "Target image: ${image_ref}"

echo "[1/4] Pulling rollback image..."
docker pull "${image_ref}"

echo "[2/4] Stopping current container (if running)..."
if docker ps --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
    docker kill --signal=SIGTERM "${CONTAINER_NAME}" >/dev/null 2>&1 || true
    timeout 30 docker wait "${CONTAINER_NAME}" >/dev/null 2>&1 || true
fi
docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true

echo "[3/4] Starting rollback container..."
run_args=(
    -d
    --name "${CONTAINER_NAME}"
    --restart unless-stopped
    -p 1935:1935
    -p 8080:8080
    -v "${UPLOAD_VOLUME}:${UPLOAD_VOLUME}"
    --ulimit nofile=65535:65535
)
if [[ -f "${ENV_FILE}" ]]; then
    run_args+=(--env-file "${ENV_FILE}")
else
    echo "WARNING: env file not found at ${ENV_FILE}; container will start without --env-file"
fi

docker run "${run_args[@]}" "${image_ref}" >/dev/null

echo "[4/4] Running post-deploy verification..."
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"${script_dir}/post_deploy_verify.sh" "http://localhost:8080" "${STREAMINFA_ADMIN_TOKEN:-}"

echo "Rollback complete."
