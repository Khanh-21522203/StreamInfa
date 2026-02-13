#!/bin/bash
# StreamInfa — Deployment Helper Script
# (from deployment.md §3.2)
#
# Usage:
#   ./scripts/deploy.sh <image_tag>
#   ./scripts/deploy.sh v0.1.0
#   ./scripts/deploy.sh latest
#
# This script performs a graceful deployment:
# 1. Pull the new image
# 2. Graceful shutdown of current instance (SIGTERM, 30s timeout)
# 3. Start new instance
# 4. Verify health and readiness
# 5. Run smoke test

set -euo pipefail

# --------------------------------------------------------
# Configuration
# --------------------------------------------------------
REGISTRY="${STREAMINFA_REGISTRY:-ghcr.io/streaminfa}"
CONTAINER_NAME="streaminfa"
ENV_FILE="${STREAMINFA_ENV_FILE:-/etc/streaminfa/env}"
UPLOAD_VOLUME="/tmp/streaminfa/uploads"
HEALTH_URL="http://localhost:8080/healthz"
READY_URL="http://localhost:8080/readyz"
API_URL="http://localhost:8080/api/v1/streams"
SHUTDOWN_TIMEOUT=30
HEALTH_RETRIES=10
HEALTH_DELAY=3

# --------------------------------------------------------
# Parse arguments
# --------------------------------------------------------
if [ $# -lt 1 ]; then
    echo "Usage: $0 <image_tag>"
    echo "Example: $0 v0.1.0"
    exit 1
fi

NEW_VERSION="$1"
IMAGE="${REGISTRY}/streaminfa:${NEW_VERSION}"

echo "=== StreamInfa Deployment ==="
echo "Image: ${IMAGE}"
echo "Container: ${CONTAINER_NAME}"
echo ""

# --------------------------------------------------------
# Pre-deployment checks
# --------------------------------------------------------
echo "[1/6] Pre-deployment checks..."

if [ ! -f "$ENV_FILE" ]; then
    echo "WARNING: Env file not found at ${ENV_FILE}"
    echo "  Set STREAMINFA_ENV_FILE to override."
fi

# --------------------------------------------------------
# Pull new image
# --------------------------------------------------------
echo "[2/6] Pulling new image..."
docker pull "${IMAGE}"

# --------------------------------------------------------
# Graceful shutdown of current instance
# --------------------------------------------------------
echo "[3/6] Stopping current instance (graceful, ${SHUTDOWN_TIMEOUT}s timeout)..."
if docker ps --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"; then
    # Record current version for rollback
    CURRENT_IMAGE=$(docker inspect --format='{{.Config.Image}}' "${CONTAINER_NAME}" 2>/dev/null || echo "unknown")
    echo "  Current image: ${CURRENT_IMAGE}"
    echo "  (Save this for rollback if needed)"

    docker kill --signal=SIGTERM "${CONTAINER_NAME}" 2>/dev/null || true
    # Wait for graceful shutdown
    timeout "${SHUTDOWN_TIMEOUT}" docker wait "${CONTAINER_NAME}" 2>/dev/null || true
    docker rm -f "${CONTAINER_NAME}" 2>/dev/null || true
    echo "  Previous instance stopped."
else
    echo "  No running instance found."
fi

# --------------------------------------------------------
# Start new instance
# --------------------------------------------------------
echo "[4/6] Starting new instance..."
DOCKER_RUN_ARGS=(
    -d
    --name "${CONTAINER_NAME}"
    --restart unless-stopped
    -p 1935:1935
    -p 8080:8080
    -v "${UPLOAD_VOLUME}:${UPLOAD_VOLUME}"
    --ulimit nofile=65535:65535
)

if [ -f "$ENV_FILE" ]; then
    DOCKER_RUN_ARGS+=(--env-file "${ENV_FILE}")
fi

docker run "${DOCKER_RUN_ARGS[@]}" "${IMAGE}"
echo "  Container started."

# --------------------------------------------------------
# Verify health
# --------------------------------------------------------
echo "[5/6] Verifying health..."
HEALTHY=false
for i in $(seq 1 ${HEALTH_RETRIES}); do
    if curl -sf "${HEALTH_URL}" > /dev/null 2>&1; then
        HEALTHY=true
        break
    fi
    echo "  Attempt ${i}/${HEALTH_RETRIES} — waiting ${HEALTH_DELAY}s..."
    sleep "${HEALTH_DELAY}"
done

if [ "$HEALTHY" = false ]; then
    echo "ERROR: Health check failed after ${HEALTH_RETRIES} attempts!"
    echo "  Check logs: docker logs ${CONTAINER_NAME}"
    echo "  Consider rollback: $0 <previous_version>"
    exit 1
fi

echo "  Health check passed."

# Readiness check
echo "  Checking readiness..."
READY_RESPONSE=$(curl -sf "${READY_URL}" 2>/dev/null || echo '{"status":"unknown"}')
echo "  Readiness: ${READY_RESPONSE}"

# --------------------------------------------------------
# Smoke test
# --------------------------------------------------------
echo "[6/6] Smoke test..."
# Only run smoke test if admin token is available
if [ -n "${STREAMINFA_ADMIN_TOKEN:-}" ]; then
    SMOKE_RESPONSE=$(curl -sf -X POST "${API_URL}" \
        -H "Authorization: Bearer ${STREAMINFA_ADMIN_TOKEN}" \
        -H "Content-Type: application/json" \
        -d '{"ingest_type": "rtmp"}' 2>/dev/null || echo "FAILED")

    if echo "${SMOKE_RESPONSE}" | grep -q "stream_id"; then
        echo "  Smoke test passed: stream created successfully."
        # Clean up: delete the test stream
        STREAM_ID=$(echo "${SMOKE_RESPONSE}" | grep -o '"stream_id":"[^"]*"' | cut -d'"' -f4)
        if [ -n "${STREAM_ID}" ]; then
            curl -sf -X DELETE "${API_URL}/${STREAM_ID}" \
                -H "Authorization: Bearer ${STREAMINFA_ADMIN_TOKEN}" > /dev/null 2>&1 || true
        fi
    else
        echo "  WARNING: Smoke test failed. Response: ${SMOKE_RESPONSE}"
        echo "  This may be expected if auth tokens are not configured."
    fi
else
    echo "  Skipped (set STREAMINFA_ADMIN_TOKEN to enable)."
fi

echo ""
echo "=== Deployment complete ==="
echo "Image: ${IMAGE}"
echo "Monitor: docker logs -f ${CONTAINER_NAME}"
echo "Metrics: curl ${HEALTH_URL/healthz/metrics}"
echo ""
echo "If issues arise within 15 minutes, rollback with:"
echo "  $0 <previous_version>"
