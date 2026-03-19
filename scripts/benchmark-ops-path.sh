#!/usr/bin/env bash

set -euo pipefail

BASE_URL="${BASE_URL:-http://localhost:8080}"
OPS_START_CMD="${OPS_START_CMD:-}"
OPS_STOP_CMD="${OPS_STOP_CMD:-}"
OPS_RELOAD_CMD="${OPS_RELOAD_CMD:-}"
OPS_AUTO_RELOAD_SIGNAL="${OPS_AUTO_RELOAD_SIGNAL:-0}"
OPS_READY_TIMEOUT_SECS="${OPS_READY_TIMEOUT_SECS:-120}"
OPS_POLL_INTERVAL_MS="${OPS_POLL_INTERVAL_MS:-500}"
OPS_PROBE_COUNT="${OPS_PROBE_COUNT:-10}"
OPS_REQUIRE_READY="${OPS_REQUIRE_READY:-1}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_OUT_DIR="${BENCH_OUT_DIR:-${SCRIPT_DIR}/../.tmp/benchmarks/$(date +%Y%m%d-%H%M%S)}"

CURL_BIN="curl"
if command -v curl.exe >/dev/null 2>&1; then
    CURL_BIN="curl.exe"
fi

if ! command -v curl >/dev/null 2>&1 && ! command -v curl.exe >/dev/null 2>&1; then
    echo "ERROR: curl/curl.exe is required for ops benchmark." >&2
    exit 1
fi

now_ms() {
    if date +%s%3N >/dev/null 2>&1; then
        date +%s%3N
    else
        echo $(( $(date +%s) * 1000 ))
    fi
}

http_status() {
    local url="$1"
    local sink="/dev/null"
    if [[ "${CURL_BIN}" == "curl.exe" ]]; then
        sink="NUL"
    fi
    "${CURL_BIN}" -s -o "${sink}" -w "%{http_code}" "${url}" || echo "000"
}

wait_for_status() {
    local url="$1"
    local expected="$2"
    local timeout_secs="$3"
    local deadline=$((SECONDS + timeout_secs))
    local poll_secs
    poll_secs="$(awk "BEGIN { printf \"%.3f\", ${OPS_POLL_INTERVAL_MS}/1000 }")"

    while (( SECONDS < deadline )); do
        local code
        code="$(http_status "${url}")"
        if [[ "${code}" == "${expected}" ]]; then
            return 0
        fi
        sleep "${poll_secs}"
    done
    return 1
}

measure_probe_avg_ms() {
    local url="$1"
    local count="$2"
    local total=0
    local i
    for (( i=0; i<count; i++ )); do
        local started ended
        started="$(now_ms)"
        http_status "${url}" >/dev/null
        ended="$(now_ms)"
        total=$((total + ended - started))
    done
    echo $((total / count))
}

mkdir -p "${BENCH_OUT_DIR}"
RUN_TS="$(date +%Y%m%d-%H%M%S)"
summary_file="${BENCH_OUT_DIR}/ops-${RUN_TS}-summary.json"
app_log="${BENCH_OUT_DIR}/ops-${RUN_TS}-app.log"

managed_pid=""
startup_to_health_ms=0
startup_to_ready_ms=0
reload_cmd_ms=0
ready_after_reload_ms=0
shutdown_ms=0
reload_method="none"
managed_process=0

if [[ -n "${OPS_START_CMD}" ]]; then
    managed_process=1
    start_ms="$(now_ms)"
    bash -lc "${OPS_START_CMD}" > "${app_log}" 2>&1 &
    managed_pid=$!

    if ! wait_for_status "${BASE_URL%/}/healthz" "200" "${OPS_READY_TIMEOUT_SECS}"; then
        echo "ERROR: /healthz did not become ready in ${OPS_READY_TIMEOUT_SECS}s" >&2
        exit 1
    fi
    health_ms="$(now_ms)"
    startup_to_health_ms=$((health_ms - start_ms))

    if [[ "${OPS_REQUIRE_READY}" == "1" ]]; then
        if ! wait_for_status "${BASE_URL%/}/readyz" "200" "${OPS_READY_TIMEOUT_SECS}"; then
            echo "ERROR: /readyz did not become ready in ${OPS_READY_TIMEOUT_SECS}s" >&2
            exit 1
        fi
        ready_ms="$(now_ms)"
        startup_to_ready_ms=$((ready_ms - start_ms))
    fi
else
    if ! wait_for_status "${BASE_URL%/}/healthz" "200" "${OPS_READY_TIMEOUT_SECS}"; then
        echo "ERROR: target service is not healthy at ${BASE_URL%/}/healthz" >&2
        exit 1
    fi
    if [[ "${OPS_REQUIRE_READY}" == "1" ]]; then
        if ! wait_for_status "${BASE_URL%/}/readyz" "200" "${OPS_READY_TIMEOUT_SECS}"; then
            echo "ERROR: target service is not ready at ${BASE_URL%/}/readyz" >&2
            exit 1
        fi
    fi
fi

probe_health_avg_ms="$(measure_probe_avg_ms "${BASE_URL%/}/healthz" "${OPS_PROBE_COUNT}")"
probe_ready_avg_ms="$(measure_probe_avg_ms "${BASE_URL%/}/readyz" "${OPS_PROBE_COUNT}")"

if [[ -n "${OPS_RELOAD_CMD}" ]]; then
    reload_method="command"
    reload_start="$(now_ms)"
    bash -lc "${OPS_RELOAD_CMD}"
    reload_end="$(now_ms)"
    reload_cmd_ms=$((reload_end - reload_start))

    after_reload_wait_start="$(now_ms)"
    if [[ "${OPS_REQUIRE_READY}" == "1" ]] && wait_for_status "${BASE_URL%/}/readyz" "200" "${OPS_READY_TIMEOUT_SECS}"; then
        ready_after_reload_ms=$(( $(now_ms) - after_reload_wait_start ))
    fi
elif [[ "${OPS_AUTO_RELOAD_SIGNAL}" == "1" && -n "${managed_pid}" ]]; then
    if kill -0 "${managed_pid}" >/dev/null 2>&1; then
        reload_method="sighup"
        reload_start="$(now_ms)"
        kill -HUP "${managed_pid}" >/dev/null 2>&1 || true
        reload_cmd_ms=$(( $(now_ms) - reload_start ))
        after_reload_wait_start="$(now_ms)"
        if [[ "${OPS_REQUIRE_READY}" == "1" ]] && wait_for_status "${BASE_URL%/}/readyz" "200" "${OPS_READY_TIMEOUT_SECS}"; then
            ready_after_reload_ms=$(( $(now_ms) - after_reload_wait_start ))
        fi
    fi
fi

if [[ "${managed_process}" == "1" ]]; then
    shutdown_start="$(now_ms)"
    if [[ -n "${OPS_STOP_CMD}" ]]; then
        bash -lc "${OPS_STOP_CMD}" || true
    elif [[ -n "${managed_pid}" ]]; then
        kill "${managed_pid}" >/dev/null 2>&1 || true
    fi

    if [[ -n "${managed_pid}" ]]; then
        wait "${managed_pid}" || true
    fi
    shutdown_ms=$(( $(now_ms) - shutdown_start ))
fi

cat > "${summary_file}" <<EOF
{
  "scenario": "operational_path",
  "timestamp": "${RUN_TS}",
  "base_url": "${BASE_URL}",
  "managed_process": ${managed_process},
  "startup_to_health_ms": ${startup_to_health_ms},
  "startup_to_ready_ms": ${startup_to_ready_ms},
  "health_probe_avg_ms": ${probe_health_avg_ms},
  "ready_probe_avg_ms": ${probe_ready_avg_ms},
  "reload_method": "${reload_method}",
  "reload_cmd_ms": ${reload_cmd_ms},
  "ready_after_reload_ms": ${ready_after_reload_ms},
  "shutdown_ms": ${shutdown_ms},
  "probe_count": ${OPS_PROBE_COUNT}
}
EOF

echo "Saved ops benchmark summary: ${summary_file}"
if [[ -n "${OPS_START_CMD}" ]]; then
    echo "Saved managed process log: ${app_log}"
fi
