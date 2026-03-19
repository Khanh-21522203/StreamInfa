#!/usr/bin/env bash

set -euo pipefail

BASE_URL="${BASE_URL:-http://localhost:8080}"
CONTROL_BASE_URL="${CONTROL_BASE_URL:-${BASE_URL}}"
ADMIN_TOKEN="${ADMIN_TOKEN:-}"
DELIVERY_STREAM_ID="${DELIVERY_STREAM_ID:-}"
DELIVERY_RENDITION="${DELIVERY_RENDITION:-high}"
DELIVERY_SEGMENT="${DELIVERY_SEGMENT:-000000.m4s}"
DELIVERY_USE_RANGE="${DELIVERY_USE_RANGE:-1}"
VUS="${VUS:-80}"
DURATION="${DURATION:-90s}"

FAULT_INJECT_CMD="${FAULT_INJECT_CMD:-}"
FAULT_RECOVER_CMD="${FAULT_RECOVER_CMD:-}"
FAULT_INJECT_AFTER_SECS="${FAULT_INJECT_AFTER_SECS:-20}"
FAULT_RECOVER_AFTER_SECS="${FAULT_RECOVER_AFTER_SECS:-60}"

METRICS_SNAPSHOT="${METRICS_SNAPSHOT:-1}"
METRICS_URL="${METRICS_URL:-http://localhost:8080/metrics}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
K6_DIR="${SCRIPT_DIR}/k6"
K6_IMAGE="${K6_IMAGE:-grafana/k6:0.50.0}"
BENCH_OUT_DIR="${BENCH_OUT_DIR:-${SCRIPT_DIR}/../.tmp/benchmarks/$(date +%Y%m%d-%H%M%S)}"
METRICS_HELPER="${SCRIPT_DIR}/benchmark-metrics.sh"
UPLOAD_FIXTURE_HOST="${K6_DIR}/fixtures/upload-sample.mp4"
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
    if [[ -n "${DELIVERY_STREAM_ID}" ]]; then
        return
    fi

    if [[ -z "${ADMIN_TOKEN}" ]]; then
        echo "ERROR: ADMIN_TOKEN is required to auto-prepare delivery stream for fault benchmark." >&2
        exit 1
    fi

    if ! command -v curl >/dev/null 2>&1 && ! command -v curl.exe >/dev/null 2>&1; then
        echo "ERROR: curl/curl.exe is required for delivery stream preparation." >&2
        exit 1
    fi
    ensure_upload_fixture

    echo "Preparing delivery stream for fault benchmark..."
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

capture_metrics_snapshot() {
    local output_file="$1"
    local curl_bin="curl"
    local tmp_file="${output_file}.tmp"

    if [[ "${METRICS_SNAPSHOT}" != "1" ]]; then
        return 1
    fi

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

run_k6() {
    local summary_file="$1"

    if command -v k6 >/dev/null 2>&1; then
        k6 run \
            --summary-export "${summary_file}" \
            -e BASE_URL="${BASE_URL}" \
            -e DURATION="${DURATION}" \
            -e VUS="${VUS}" \
            -e DELIVERY_STREAM_ID="${DELIVERY_STREAM_ID}" \
            -e DELIVERY_RENDITION="${DELIVERY_RENDITION}" \
            -e DELIVERY_SEGMENT="${DELIVERY_SEGMENT}" \
            -e DELIVERY_USE_RANGE="${DELIVERY_USE_RANGE}" \
            "${K6_DIR}/fault-delivery.js"
        return
    fi

    require_cmd docker
    docker run --rm -i \
        -e BASE_URL="${BASE_URL}" \
        -e DURATION="${DURATION}" \
        -e VUS="${VUS}" \
        -e DELIVERY_STREAM_ID="${DELIVERY_STREAM_ID}" \
        -e DELIVERY_RENDITION="${DELIVERY_RENDITION}" \
        -e DELIVERY_SEGMENT="${DELIVERY_SEGMENT}" \
        -e DELIVERY_USE_RANGE="${DELIVERY_USE_RANGE}" \
        -v "${K6_DIR}:/scripts:ro" \
        -v "${BENCH_OUT_DIR}:/results" \
        "${K6_IMAGE}" run \
        --summary-export "/results/$(basename "${summary_file}")" \
        "/scripts/fault-delivery.js"
}

if [[ -z "${FAULT_INJECT_CMD}" ]]; then
    echo "ERROR: FAULT_INJECT_CMD is required for fault benchmark." >&2
    exit 1
fi

mkdir -p "${BENCH_OUT_DIR}"
prepare_delivery_stream

RUN_TS="$(date +%Y%m%d-%H%M%S)"
summary_file="${BENCH_OUT_DIR}/fault-${RUN_TS}-k6-summary.json"
before_file="${BENCH_OUT_DIR}/fault-${RUN_TS}-metrics-before.prom"
after_file="${BENCH_OUT_DIR}/fault-${RUN_TS}-metrics-after.prom"
delta_file="${BENCH_OUT_DIR}/fault-${RUN_TS}-metrics-delta.txt"

have_before=0
have_after=0
if capture_metrics_snapshot "${before_file}"; then
    have_before=1
fi

echo "== Fault-Injection Benchmark =="
echo "base_url=${BASE_URL} duration=${DURATION} vus=${VUS} stream_id=${DELIVERY_STREAM_ID}"
echo "inject_after=${FAULT_INJECT_AFTER_SECS}s inject_cmd=${FAULT_INJECT_CMD}"
if [[ -n "${FAULT_RECOVER_CMD}" ]]; then
    echo "recover_after=${FAULT_RECOVER_AFTER_SECS}s recover_cmd=${FAULT_RECOVER_CMD}"
fi

(sleep "${FAULT_INJECT_AFTER_SECS}"; bash -lc "${FAULT_INJECT_CMD}") &
inject_pid=$!

recover_pid=""
if [[ -n "${FAULT_RECOVER_CMD}" ]]; then
    (sleep "${FAULT_RECOVER_AFTER_SECS}"; bash -lc "${FAULT_RECOVER_CMD}") &
    recover_pid=$!
fi

run_k6 "${summary_file}"

wait "${inject_pid}" || true
if [[ -n "${recover_pid}" ]]; then
    wait "${recover_pid}" || true
fi

if capture_metrics_snapshot "${after_file}"; then
    have_after=1
fi

if [[ "${have_before}" == "1" && "${have_after}" == "1" && -f "${METRICS_HELPER}" ]]; then
    {
        echo "== Prometheus Metrics Delta =="
        bash "${METRICS_HELPER}" "${before_file}" "${after_file}"
    } | tee "${delta_file}"
fi

echo "Saved k6 summary: ${summary_file}"
if [[ -f "${delta_file}" ]]; then
    echo "Saved metrics delta: ${delta_file}"
fi
