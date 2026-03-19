#!/usr/bin/env bash

set -euo pipefail

SCENARIO="${1:-all}"
BASE_URL="${BASE_URL:-http://localhost:8080}"
DURATION="${DURATION:-30s}"
VUS="${VUS:-20}"
CONTROL_CREATE_VUS="${CONTROL_CREATE_VUS:-5}"
INGEST_UPLOAD_VUS="${INGEST_UPLOAD_VUS:-2}"
INGEST_READY_TIMEOUT_SECS="${INGEST_READY_TIMEOUT_SECS:-120}"
INGEST_POLL_INTERVAL_MS="${INGEST_POLL_INTERVAL_MS:-500}"
DELIVERY_STREAM_ID="${DELIVERY_STREAM_ID:-}"
DELIVERY_RENDITION="${DELIVERY_RENDITION:-high}"
DELIVERY_SEGMENT="${DELIVERY_SEGMENT:-000000.m4s}"
DELIVERY_USE_RANGE="${DELIVERY_USE_RANGE:-1}"
AUTO_PREPARE_DELIVERY="${AUTO_PREPARE_DELIVERY:-1}"
UPLOAD_SAMPLE_SIZE_BYTES="${UPLOAD_SAMPLE_SIZE_BYTES:-262144}"
METRICS_SNAPSHOT="${METRICS_SNAPSHOT:-1}"
METRICS_URL="${METRICS_URL:-http://localhost:8080/metrics}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
K6_DIR="${SCRIPT_DIR}/k6"
K6_IMAGE="${K6_IMAGE:-grafana/k6:0.50.0}"
BENCH_OUT_DIR="${BENCH_OUT_DIR:-${SCRIPT_DIR}/../.tmp/benchmarks/$(date +%Y%m%d-%H%M%S)}"
METRICS_HELPER="${SCRIPT_DIR}/benchmark-metrics.sh"
UPLOAD_FIXTURE_HOST="${K6_DIR}/fixtures/upload-sample.mp4"
UPLOAD_FIXTURE_CONTAINER="/scripts/fixtures/upload-sample.mp4"

usage() {
    cat <<'EOF'
Usage:
  ./scripts/benchmark.sh [control|ingest|delivery|all]

Environment:
  BASE_URL                 Service base URL (default: http://localhost:8080)
  DURATION                 k6 scenario duration (default: 30s)
  VUS                      VUs for steady read scenarios (default: 20)
  CONTROL_CREATE_VUS       VUs for create/delete scenario (default: 5)
  ADMIN_TOKEN              Required for control, ingest, and auto-prepare delivery

  INGEST_UPLOAD_VUS        VUs for ingest upload scenario (default: 2)
  INGEST_READY_TIMEOUT_SECS Max wait for upload stream to reach READY (default: 120)
  INGEST_POLL_INTERVAL_MS  Poll interval for READY check (default: 500)
  UPLOAD_SAMPLE_SIZE_BYTES Generated upload fixture size in bytes (default: 262144)

  DELIVERY_STREAM_ID       Required for delivery benchmark unless AUTO_PREPARE_DELIVERY=1
  DELIVERY_RENDITION       Rendition for delivery benchmark (default: high)
  DELIVERY_SEGMENT         Segment filename (default: 000000.m4s)
  DELIVERY_USE_RANGE       1 to include a ranged segment request, 0 to disable
  AUTO_PREPARE_DELIVERY    1 to upload/poll a stream if DELIVERY_STREAM_ID missing (default: 1)

  METRICS_SNAPSHOT         1 to capture before/after metrics snapshots (default: 1)
  METRICS_URL              Metrics endpoint URL (default: BASE_URL/metrics)
  BENCH_OUT_DIR            Output directory for summaries and snapshots

Examples:
  ADMIN_TOKEN=at_token ./scripts/benchmark.sh control
  ADMIN_TOKEN=at_token ./scripts/benchmark.sh ingest
  ADMIN_TOKEN=at_token DELIVERY_STREAM_ID=<uuidv7> ./scripts/benchmark.sh delivery
  ADMIN_TOKEN=at_token ./scripts/benchmark.sh all
EOF
}

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "ERROR: required command '$1' is not available" >&2
        exit 1
    fi
}

json_field() {
    local field="$1"
    sed -n "s/.*\"${field}\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" | head -n1
}

ensure_upload_fixture() {
    mkdir -p "$(dirname "${UPLOAD_FIXTURE_HOST}")"

    if [[ "${UPLOAD_SAMPLE_SIZE_BYTES}" -lt 12 ]]; then
        echo "ERROR: UPLOAD_SAMPLE_SIZE_BYTES must be >= 12" >&2
        exit 1
    fi

    local current_size="0"
    if [[ -f "${UPLOAD_FIXTURE_HOST}" ]]; then
        current_size="$(wc -c < "${UPLOAD_FIXTURE_HOST}" | tr -d '[:space:]')"
    fi

    if [[ "${current_size}" == "${UPLOAD_SAMPLE_SIZE_BYTES}" ]]; then
        return
    fi

    echo "Generating upload fixture: ${UPLOAD_FIXTURE_HOST} (${UPLOAD_SAMPLE_SIZE_BYTES} bytes)"
    : > "${UPLOAD_FIXTURE_HOST}"
    printf '\x00\x00\x00\x18ftypisom' > "${UPLOAD_FIXTURE_HOST}"
    if [[ "${UPLOAD_SAMPLE_SIZE_BYTES}" -gt 12 ]]; then
        dd if=/dev/zero bs=1 count=$((UPLOAD_SAMPLE_SIZE_BYTES - 12)) >> "${UPLOAD_FIXTURE_HOST}" 2>/dev/null
    fi
}

capture_metrics_snapshot() {
    local output_file="$1"
    local curl_bin="curl"
    local tmp_file="${output_file}.tmp"

    if [[ "${METRICS_SNAPSHOT}" != "1" ]]; then
        return 1
    fi

    # In Git-Bash/WSL workflows, curl.exe often has better reachability
    # to the Windows-hosted service than Linux curl.
    if command -v curl.exe >/dev/null 2>&1; then
        curl_bin="curl.exe"
    elif ! command -v curl >/dev/null 2>&1; then
        echo "WARN: curl unavailable; skipping metrics snapshot" >&2
        return 1
    fi

    rm -f "${tmp_file}"

    if [[ "${curl_bin}" == "curl.exe" ]]; then
        if "${curl_bin}" -fsS --retry 2 --retry-delay 1 --max-time 15 "${METRICS_URL}" > "${tmp_file}"; then
            mv "${tmp_file}" "${output_file}"
            return 0
        fi
    elif "${curl_bin}" -fsS --retry 2 --retry-delay 1 --max-time 15 "${METRICS_URL}" -o "${tmp_file}"; then
        mv "${tmp_file}" "${output_file}"
        return 0
    fi

    rm -f "${tmp_file}" "${output_file}"

    echo "WARN: failed to scrape metrics from ${METRICS_URL}; skipping metrics snapshot" >&2
    return 1
}

print_metrics_delta() {
    local before_file="$1"
    local after_file="$2"
    local output_file="$3"

    if [[ ! -f "${METRICS_HELPER}" ]]; then
        echo "WARN: metrics helper missing: ${METRICS_HELPER}" >&2
        return
    fi

    {
        echo
        echo "== Prometheus Metrics Delta =="
        bash "${METRICS_HELPER}" "${before_file}" "${after_file}"
    } | tee "${output_file}"
}

run_k6() {
    local script_name="$1"
    local summary_file="$2"

    local -a summary_arg=()
    if [[ -n "${summary_file}" ]]; then
        summary_arg=(--summary-export "${summary_file}")
    fi

    if command -v k6 >/dev/null 2>&1; then
        k6 run \
            "${summary_arg[@]}" \
            -e BASE_URL="${BASE_URL}" \
            -e DURATION="${DURATION}" \
            -e VUS="${VUS}" \
            -e CONTROL_CREATE_VUS="${CONTROL_CREATE_VUS}" \
            -e ADMIN_TOKEN="${ADMIN_TOKEN:-}" \
            -e INGEST_UPLOAD_VUS="${INGEST_UPLOAD_VUS}" \
            -e INGEST_READY_TIMEOUT_SECS="${INGEST_READY_TIMEOUT_SECS}" \
            -e INGEST_POLL_INTERVAL_MS="${INGEST_POLL_INTERVAL_MS}" \
            -e UPLOAD_FILE_PATH="${UPLOAD_FIXTURE_HOST}" \
            -e DELIVERY_STREAM_ID="${DELIVERY_STREAM_ID}" \
            -e DELIVERY_RENDITION="${DELIVERY_RENDITION}" \
            -e DELIVERY_SEGMENT="${DELIVERY_SEGMENT}" \
            -e DELIVERY_USE_RANGE="${DELIVERY_USE_RANGE}" \
            "${K6_DIR}/${script_name}"
        return
    fi

    require_cmd docker
    local docker_summary_file="/results/$(basename "${summary_file}")"
    docker run --rm -i \
        -e BASE_URL="${BASE_URL}" \
        -e DURATION="${DURATION}" \
        -e VUS="${VUS}" \
        -e CONTROL_CREATE_VUS="${CONTROL_CREATE_VUS}" \
        -e ADMIN_TOKEN="${ADMIN_TOKEN:-}" \
        -e INGEST_UPLOAD_VUS="${INGEST_UPLOAD_VUS}" \
        -e INGEST_READY_TIMEOUT_SECS="${INGEST_READY_TIMEOUT_SECS}" \
        -e INGEST_POLL_INTERVAL_MS="${INGEST_POLL_INTERVAL_MS}" \
        -e UPLOAD_FILE_PATH="${UPLOAD_FIXTURE_CONTAINER}" \
        -e DELIVERY_STREAM_ID="${DELIVERY_STREAM_ID}" \
        -e DELIVERY_RENDITION="${DELIVERY_RENDITION}" \
        -e DELIVERY_SEGMENT="${DELIVERY_SEGMENT}" \
        -e DELIVERY_USE_RANGE="${DELIVERY_USE_RANGE}" \
        -v "${K6_DIR}:/scripts:ro" \
        -v "${BENCH_OUT_DIR}:/results" \
        "${K6_IMAGE}" run \
        --summary-export "${docker_summary_file}" \
        "/scripts/${script_name}"
}

run_scenario() {
    local scenario_name="$1"
    local script_name="$2"

    mkdir -p "${BENCH_OUT_DIR}"

    local ts
    ts="$(date +%Y%m%d-%H%M%S)"
    local summary_file="${BENCH_OUT_DIR}/${scenario_name}-${ts}-k6-summary.json"
    local before_file="${BENCH_OUT_DIR}/${scenario_name}-${ts}-metrics-before.prom"
    local after_file="${BENCH_OUT_DIR}/${scenario_name}-${ts}-metrics-after.prom"
    local delta_file="${BENCH_OUT_DIR}/${scenario_name}-${ts}-metrics-delta.txt"

    local have_before=0
    local have_after=0

    if capture_metrics_snapshot "${before_file}"; then
        have_before=1
    fi

    run_k6 "${script_name}" "${summary_file}"

    if capture_metrics_snapshot "${after_file}"; then
        have_after=1
    fi

    if [[ "${have_before}" == "1" && "${have_after}" == "1" ]]; then
        print_metrics_delta "${before_file}" "${after_file}" "${delta_file}"
    fi

    echo "Saved k6 summary: ${summary_file}"
    if [[ -f "${delta_file}" ]]; then
        echo "Saved metrics delta: ${delta_file}"
    fi
}

wait_for_stream_ready() {
    local stream_id="$1"
    local timeout_secs="${2:-120}"

    local deadline=$((SECONDS + timeout_secs))
    while (( SECONDS < deadline )); do
        local status_body
        status_body="$(curl -fsS -H "Authorization: Bearer ${ADMIN_TOKEN}" "${BASE_URL%/}/api/v1/streams/${stream_id}" || true)"
        local status
        status="$(printf '%s' "${status_body}" | json_field status)"

        case "${status}" in
            ready)
                return 0
                ;;
            error|deleted)
                return 1
                ;;
            *)
                sleep 1
                ;;
        esac
    done

    return 1
}

prepare_delivery_stream() {
    if [[ -n "${DELIVERY_STREAM_ID}" ]]; then
        return
    fi

    if [[ "${AUTO_PREPARE_DELIVERY}" != "1" ]]; then
        echo "ERROR: DELIVERY_STREAM_ID is required for delivery benchmark." >&2
        exit 1
    fi

    if [[ -z "${ADMIN_TOKEN:-}" ]]; then
        echo "ERROR: ADMIN_TOKEN is required to auto-prepare delivery benchmark stream." >&2
        exit 1
    fi

    require_cmd curl
    ensure_upload_fixture

    echo "Preparing delivery stream via upload benchmark fixture..."
    local resp
    resp="$(curl -fsS -X POST "${BASE_URL%/}/api/v1/streams/upload" \
        -H "Authorization: Bearer ${ADMIN_TOKEN}" \
        -F "file=@${UPLOAD_FIXTURE_HOST};type=video/mp4")"

    local stream_id
    stream_id="$(printf '%s' "${resp}" | json_field stream_id)"

    if [[ -z "${stream_id}" ]]; then
        echo "ERROR: failed to parse stream_id from upload response: ${resp}" >&2
        exit 1
    fi

    echo "Uploaded stream_id=${stream_id}; waiting for READY..."
    if ! wait_for_stream_ready "${stream_id}" "${INGEST_READY_TIMEOUT_SECS}"; then
        echo "ERROR: stream ${stream_id} did not reach READY within timeout" >&2
        exit 1
    fi

    DELIVERY_STREAM_ID="${stream_id}"
    echo "Delivery stream ready: ${DELIVERY_STREAM_ID}"
}

run_control() {
    if [[ -z "${ADMIN_TOKEN:-}" ]]; then
        echo "ERROR: ADMIN_TOKEN is required for control benchmark." >&2
        exit 1
    fi

    echo "== Control Plane Benchmark =="
    echo "base_url=${BASE_URL} duration=${DURATION} vus=${VUS} create_vus=${CONTROL_CREATE_VUS}"
    run_scenario "control" "control-plane.js"
}

run_ingest() {
    if [[ -z "${ADMIN_TOKEN:-}" ]]; then
        echo "ERROR: ADMIN_TOKEN is required for ingest benchmark." >&2
        exit 1
    fi

    ensure_upload_fixture

    echo "== Ingest Benchmark (VOD Upload) =="
    echo "base_url=${BASE_URL} duration=${DURATION} upload_vus=${INGEST_UPLOAD_VUS} fixture=${UPLOAD_FIXTURE_HOST}"
    run_scenario "ingest" "vod-upload.js"
}

run_delivery() {
    prepare_delivery_stream

    echo "== Delivery Benchmark =="
    echo "base_url=${BASE_URL} duration=${DURATION} vus=${VUS} stream_id=${DELIVERY_STREAM_ID}"
    run_scenario "delivery" "delivery-origin.js"
}

case "${SCENARIO}" in
    control)
        run_control
        ;;
    ingest)
        run_ingest
        ;;
    delivery)
        run_delivery
        ;;
    all)
        run_control
        run_ingest
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
