#!/usr/bin/env bash

set -euo pipefail

SCENARIO="${1:-all}"
BASE_URL="${BASE_URL:-http://localhost:8080}"
DURATION="${DURATION:-30s}"
VUS="${VUS:-20}"
CONTROL_CREATE_VUS="${CONTROL_CREATE_VUS:-5}"
DELIVERY_STREAM_ID="${DELIVERY_STREAM_ID:-}"
DELIVERY_RENDITION="${DELIVERY_RENDITION:-high}"
DELIVERY_SEGMENT="${DELIVERY_SEGMENT:-000000.m4s}"
DELIVERY_USE_RANGE="${DELIVERY_USE_RANGE:-1}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
K6_DIR="${SCRIPT_DIR}/k6"
K6_IMAGE="${K6_IMAGE:-grafana/k6:0.50.0}"

usage() {
    cat <<'EOF'
Usage:
  ./scripts/benchmark.sh [control|delivery|all]

Environment:
  BASE_URL                 Service base URL (default: http://localhost:8080)
  DURATION                 K6 scenario duration (default: 30s)
  VUS                      VUs for steady read scenarios (default: 20)
  CONTROL_CREATE_VUS       VUs for create/delete scenario (default: 5)
  ADMIN_TOKEN              Required for control benchmark

  DELIVERY_STREAM_ID       Required for delivery benchmark
  DELIVERY_RENDITION       Rendition for delivery benchmark (default: high)
  DELIVERY_SEGMENT         Segment filename (default: 000000.m4s)
  DELIVERY_USE_RANGE       1 to include a ranged segment request, 0 to disable

Examples:
  ADMIN_TOKEN=at_token ./scripts/benchmark.sh control
  DELIVERY_STREAM_ID=<uuidv7> ./scripts/benchmark.sh delivery
  ADMIN_TOKEN=at_token DELIVERY_STREAM_ID=<uuidv7> ./scripts/benchmark.sh all
EOF
}

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "ERROR: required command '$1' is not available" >&2
        exit 1
    fi
}

run_k6() {
    local script_name="$1"

    if command -v k6 >/dev/null 2>&1; then
        k6 run \
            -e BASE_URL="${BASE_URL}" \
            -e DURATION="${DURATION}" \
            -e VUS="${VUS}" \
            -e CONTROL_CREATE_VUS="${CONTROL_CREATE_VUS}" \
            -e ADMIN_TOKEN="${ADMIN_TOKEN:-}" \
            -e DELIVERY_STREAM_ID="${DELIVERY_STREAM_ID}" \
            -e DELIVERY_RENDITION="${DELIVERY_RENDITION}" \
            -e DELIVERY_SEGMENT="${DELIVERY_SEGMENT}" \
            -e DELIVERY_USE_RANGE="${DELIVERY_USE_RANGE}" \
            "${K6_DIR}/${script_name}"
        return
    fi

    require_cmd docker
    docker run --rm -i \
        -e BASE_URL="${BASE_URL}" \
        -e DURATION="${DURATION}" \
        -e VUS="${VUS}" \
        -e CONTROL_CREATE_VUS="${CONTROL_CREATE_VUS}" \
        -e ADMIN_TOKEN="${ADMIN_TOKEN:-}" \
        -e DELIVERY_STREAM_ID="${DELIVERY_STREAM_ID}" \
        -e DELIVERY_RENDITION="${DELIVERY_RENDITION}" \
        -e DELIVERY_SEGMENT="${DELIVERY_SEGMENT}" \
        -e DELIVERY_USE_RANGE="${DELIVERY_USE_RANGE}" \
        -v "${K6_DIR}:/scripts:ro" \
        "${K6_IMAGE}" run "/scripts/${script_name}"
}

run_control() {
    if [[ -z "${ADMIN_TOKEN:-}" ]]; then
        echo "ERROR: ADMIN_TOKEN is required for control benchmark." >&2
        exit 1
    fi

    echo "== Control Plane Benchmark =="
    echo "base_url=${BASE_URL} duration=${DURATION} vus=${VUS} create_vus=${CONTROL_CREATE_VUS}"
    run_k6 "control-plane.js"
}

run_delivery() {
    if [[ -z "${DELIVERY_STREAM_ID}" ]]; then
        echo "ERROR: DELIVERY_STREAM_ID is required for delivery benchmark." >&2
        exit 1
    fi

    echo "== Delivery Benchmark =="
    echo "base_url=${BASE_URL} duration=${DURATION} vus=${VUS} stream_id=${DELIVERY_STREAM_ID}"
    run_k6 "delivery-origin.js"
}

case "${SCENARIO}" in
    control)
        run_control
        ;;
    delivery)
        run_delivery
        ;;
    all)
        run_control
        run_delivery
        ;;
    -h|--help|help)
        usage
        ;;
    *)
        echo "ERROR: unknown scenario '${SCENARIO}'" >&2
        usage
        exit 1
        ;;
esac
