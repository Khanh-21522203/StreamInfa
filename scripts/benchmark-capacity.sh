#!/usr/bin/env bash

set -euo pipefail

SCENARIO="${1:-delivery}"
BASE_URL="${BASE_URL:-http://localhost:8080}"
CONTROL_BASE_URL="${CONTROL_BASE_URL:-${BASE_URL}}"
ADMIN_TOKEN="${ADMIN_TOKEN:-}"
DELIVERY_STREAM_ID="${DELIVERY_STREAM_ID:-}"
DELIVERY_RENDITION="${DELIVERY_RENDITION:-high}"
DELIVERY_SEGMENT="${DELIVERY_SEGMENT:-000000.m4s}"
DELIVERY_USE_RANGE="${DELIVERY_USE_RANGE:-1}"

CAPACITY_START_VUS="${CAPACITY_START_VUS:-10}"
CAPACITY_STEP_VUS="${CAPACITY_STEP_VUS:-10}"
CAPACITY_MAX_VUS="${CAPACITY_MAX_VUS:-200}"
CAPACITY_DURATION="${CAPACITY_DURATION:-20s}"
CAPACITY_P95_MS="${CAPACITY_P95_MS:-400}"
CAPACITY_ERROR_RATE_MAX="${CAPACITY_ERROR_RATE_MAX:-0.01}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
K6_DIR="${SCRIPT_DIR}/k6"
K6_IMAGE="${K6_IMAGE:-grafana/k6:0.50.0}"
BENCH_OUT_DIR="${BENCH_OUT_DIR:-${SCRIPT_DIR}/../.tmp/benchmarks/$(date +%Y%m%d-%H%M%S)}"
UPLOAD_FIXTURE_HOST="${K6_DIR}/fixtures/upload-sample.mp4"
UPLOAD_FIXTURE_CONTAINER="/scripts/fixtures/upload-sample.mp4"
UPLOAD_SAMPLE_SIZE_BYTES="${UPLOAD_SAMPLE_SIZE_BYTES:-262144}"
INGEST_READY_TIMEOUT_SECS="${INGEST_READY_TIMEOUT_SECS:-120}"

if [[ "${CONTROL_BASE_URL}" == *"host.docker.internal"* ]]; then
    CONTROL_BASE_URL="${CONTROL_BASE_URL//host.docker.internal/localhost}"
fi

CURL_BIN="curl"
if command -v curl.exe >/dev/null 2>&1; then
    CURL_BIN="curl.exe"
fi

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

wait_for_stream_ready() {
    local stream_id="$1"
    local timeout_secs="${2:-120}"

    local deadline=$((SECONDS + timeout_secs))
    while (( SECONDS < deadline )); do
        local status_body
        status_body="$("${CURL_BIN}" -fsS -H "Authorization: Bearer ${ADMIN_TOKEN}" "${CONTROL_BASE_URL%/}/api/v1/streams/${stream_id}" || true)"
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
    if [[ "${SCENARIO}" != "delivery" ]]; then
        return
    fi

    if [[ -n "${DELIVERY_STREAM_ID}" ]]; then
        return
    fi

    if [[ -z "${ADMIN_TOKEN}" ]]; then
        echo "ERROR: ADMIN_TOKEN is required to auto-prepare delivery stream for capacity benchmark." >&2
        exit 1
    fi

    if ! command -v curl >/dev/null 2>&1 && ! command -v curl.exe >/dev/null 2>&1; then
        echo "ERROR: curl/curl.exe is required for delivery stream preparation." >&2
        exit 1
    fi
    ensure_upload_fixture

    echo "Preparing delivery stream for capacity benchmark..."
    local upload_path="${UPLOAD_FIXTURE_HOST}"
    if [[ "${CURL_BIN}" == "curl.exe" ]]; then
        local safe_dir="/tmp/streaminfa-bench"
        mkdir -p "${safe_dir}"
        local safe_file="${safe_dir}/upload-sample.mp4"
        cp "${UPLOAD_FIXTURE_HOST}" "${safe_file}"
        upload_path="${safe_file}"
    fi
    local resp
    resp="$("${CURL_BIN}" -fsS -X POST "${CONTROL_BASE_URL%/}/api/v1/streams/upload" \
        -H "Authorization: Bearer ${ADMIN_TOKEN}" \
        -F "file=@${upload_path};type=video/mp4")"

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

run_k6() {
    local script_name="$1"
    local summary_file="$2"
    local vus="$3"

    if command -v k6 >/dev/null 2>&1; then
        k6 run \
            --summary-export "${summary_file}" \
            -e BASE_URL="${BASE_URL}" \
            -e DURATION="${CAPACITY_DURATION}" \
            -e CAPACITY_DURATION="${CAPACITY_DURATION}" \
            -e VUS="${vus}" \
            -e CAPACITY_VUS="${vus}" \
            -e CAPACITY_P95_MS="${CAPACITY_P95_MS}" \
            -e CAPACITY_ERROR_RATE_MAX="${CAPACITY_ERROR_RATE_MAX}" \
            -e DELIVERY_STREAM_ID="${DELIVERY_STREAM_ID}" \
            -e DELIVERY_RENDITION="${DELIVERY_RENDITION}" \
            -e DELIVERY_SEGMENT="${DELIVERY_SEGMENT}" \
            -e DELIVERY_USE_RANGE="${DELIVERY_USE_RANGE}" \
            "${K6_DIR}/${script_name}"
        return
    fi

    require_cmd docker
    local docker_network="${K6_DOCKER_NETWORK:-}"
    local -a network_arg=()
    if [[ -n "${docker_network}" ]]; then
        network_arg=(--network "${docker_network}")
    fi
    docker run --rm -i "${network_arg[@]}" \
        -e BASE_URL="${BASE_URL}" \
        -e DURATION="${CAPACITY_DURATION}" \
        -e CAPACITY_DURATION="${CAPACITY_DURATION}" \
        -e VUS="${vus}" \
        -e CAPACITY_VUS="${vus}" \
        -e CAPACITY_P95_MS="${CAPACITY_P95_MS}" \
        -e CAPACITY_ERROR_RATE_MAX="${CAPACITY_ERROR_RATE_MAX}" \
        -e DELIVERY_STREAM_ID="${DELIVERY_STREAM_ID}" \
        -e DELIVERY_RENDITION="${DELIVERY_RENDITION}" \
        -e DELIVERY_SEGMENT="${DELIVERY_SEGMENT}" \
        -e DELIVERY_USE_RANGE="${DELIVERY_USE_RANGE}" \
        -v "${K6_DIR}:/scripts:ro" \
        -v "${BENCH_OUT_DIR}:/results" \
        "${K6_IMAGE}" run \
        --summary-export "/results/$(basename "${summary_file}")" \
        "/scripts/${script_name}"
}

mkdir -p "${BENCH_OUT_DIR}"
prepare_delivery_stream

if [[ "${SCENARIO}" != "delivery" ]]; then
    echo "ERROR: unsupported capacity scenario '${SCENARIO}'. Supported: delivery" >&2
    exit 1
fi

SCRIPT_NAME="capacity-delivery.js"
RUN_TS="$(date +%Y%m%d-%H%M%S)"

last_passing_vus=0
first_failing_vus=0
run_count=0

echo "== Capacity Benchmark =="
echo "scenario=${SCENARIO} base_url=${BASE_URL} duration=${CAPACITY_DURATION} start_vus=${CAPACITY_START_VUS} step_vus=${CAPACITY_STEP_VUS} max_vus=${CAPACITY_MAX_VUS}"

for (( vus=CAPACITY_START_VUS; vus<=CAPACITY_MAX_VUS; vus+=CAPACITY_STEP_VUS )); do
    run_count=$((run_count + 1))
    summary_file="${BENCH_OUT_DIR}/capacity-${SCENARIO}-vus-${vus}-${RUN_TS}-k6-summary.json"
    echo "-- Running capacity step: vus=${vus}"
    if run_k6 "${SCRIPT_NAME}" "${summary_file}" "${vus}"; then
        last_passing_vus="${vus}"
        echo "PASS vus=${vus} summary=${summary_file}"
    else
        first_failing_vus="${vus}"
        echo "FAIL vus=${vus} summary=${summary_file}"
        break
    fi
done

capacity_summary_file="${BENCH_OUT_DIR}/capacity-${SCENARIO}-${RUN_TS}-summary.json"
cat > "${capacity_summary_file}" <<EOF
{
  "scenario": "capacity_${SCENARIO}",
  "timestamp": "${RUN_TS}",
  "duration": "${CAPACITY_DURATION}",
  "start_vus": ${CAPACITY_START_VUS},
  "step_vus": ${CAPACITY_STEP_VUS},
  "max_vus": ${CAPACITY_MAX_VUS},
  "runs": ${run_count},
  "max_passing_vus": ${last_passing_vus},
  "first_failing_vus": ${first_failing_vus}
}
EOF

echo "Saved capacity summary: ${capacity_summary_file}"
