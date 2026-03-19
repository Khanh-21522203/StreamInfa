#!/usr/bin/env bash

set -euo pipefail

BASE_URL="${BASE_URL:-http://localhost:8080}"
CONTROL_BASE_URL="${CONTROL_BASE_URL:-${BASE_URL}}"
ADMIN_TOKEN="${ADMIN_TOKEN:-}"
LIVE_RTMP_PUBLISH_SECS="${LIVE_RTMP_PUBLISH_SECS:-45}"
LIVE_RTMP_READY_TIMEOUT_SECS="${LIVE_RTMP_READY_TIMEOUT_SECS:-120}"
LIVE_RTMP_POLL_INTERVAL_MS="${LIVE_RTMP_POLL_INTERVAL_MS:-500}"
LIVE_RTMP_RENDITION="${LIVE_RTMP_RENDITION:-high}"
LIVE_RTMP_DELETE_STREAM="${LIVE_RTMP_DELETE_STREAM:-1}"
LIVE_RTMP_FFMPEG_LOG_LEVEL="${LIVE_RTMP_FFMPEG_LOG_LEVEL:-error}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_OUT_DIR="${BENCH_OUT_DIR:-${SCRIPT_DIR}/../.tmp/benchmarks/$(date +%Y%m%d-%H%M%S)}"

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

if [[ -z "${ADMIN_TOKEN}" ]]; then
    echo "ERROR: ADMIN_TOKEN is required for live RTMP benchmark." >&2
    exit 1
fi

if ! command -v curl >/dev/null 2>&1 && ! command -v curl.exe >/dev/null 2>&1; then
    echo "ERROR: curl/curl.exe is required for live RTMP benchmark." >&2
    exit 1
fi
require_cmd ffmpeg

mkdir -p "${BENCH_OUT_DIR}"
RUN_TS="$(date +%Y%m%d-%H%M%S)"
summary_file="${BENCH_OUT_DIR}/live-rtmp-${RUN_TS}-summary.json"
ffmpeg_log="${BENCH_OUT_DIR}/live-rtmp-${RUN_TS}-ffmpeg.log"

create_payload='{"ingest_type":"rtmp","metadata":{"benchmark":"live-rtmp"}}'
create_resp="$("${CURL_BIN}" -fsS -X POST "${CONTROL_BASE_URL%/}/api/v1/streams" \
    -H "Authorization: Bearer ${ADMIN_TOKEN}" \
    -H "Content-Type: application/json" \
    -d "${create_payload}")"

stream_id="$(printf '%s' "${create_resp}" | json_field stream_id)"
stream_key="$(printf '%s' "${create_resp}" | json_field stream_key)"
rtmp_url="$(printf '%s' "${create_resp}" | json_field rtmp_url)"

if [[ -z "${stream_id}" || -z "${stream_key}" || -z "${rtmp_url}" ]]; then
    echo "ERROR: failed to parse stream_id/stream_key/rtmp_url from create response: ${create_resp}" >&2
    exit 1
fi

echo "Created live stream_id=${stream_id}"
echo "Publishing RTMP to ${rtmp_url}"

start_ms="$(now_ms)"
ffmpeg_exit_code=0
ffmpeg -hide_banner -loglevel "${LIVE_RTMP_FFMPEG_LOG_LEVEL}" \
    -re \
    -f lavfi -i "testsrc=size=1280x720:rate=30" \
    -f lavfi -i "sine=frequency=1000:sample_rate=48000" \
    -t "${LIVE_RTMP_PUBLISH_SECS}" \
    -c:v libx264 -preset veryfast -pix_fmt yuv420p \
    -g 60 -keyint_min 60 -sc_threshold 0 \
    -c:a aac -b:a 128k \
    -f flv "${rtmp_url}" > "${ffmpeg_log}" 2>&1 &
ffmpeg_pid=$!

master_ready_ms=0
media_ready_ms=0
segment_ready_ms=0
first_segment=""

poll_interval_secs="$(awk "BEGIN { printf \"%.3f\", ${LIVE_RTMP_POLL_INTERVAL_MS}/1000 }")"
deadline_secs=$((SECONDS + LIVE_RTMP_READY_TIMEOUT_SECS))

while (( SECONDS < deadline_secs )); do
    now="$(now_ms)"

    if [[ "${master_ready_ms}" == "0" ]]; then
        master_status="$(http_status "${CONTROL_BASE_URL%/}/streams/${stream_id}/master.m3u8")"
        if [[ "${master_status}" == "200" ]]; then
            master_ready_ms=$((now - start_ms))
        fi
    fi

    media_body=""
    if [[ "${master_ready_ms}" != "0" && "${media_ready_ms}" == "0" ]]; then
        media_status="$(http_status "${CONTROL_BASE_URL%/}/streams/${stream_id}/${LIVE_RTMP_RENDITION}/media.m3u8")"
        if [[ "${media_status}" == "200" ]]; then
            media_ready_ms=$((now - start_ms))
        fi
    fi

    if [[ "${master_ready_ms}" != "0" ]]; then
        media_body="$("${CURL_BIN}" -fsS "${CONTROL_BASE_URL%/}/streams/${stream_id}/${LIVE_RTMP_RENDITION}/media.m3u8" || true)"
        if [[ -z "${first_segment}" ]]; then
            first_segment="$(printf '%s\n' "${media_body}" | grep -Eo '[0-9]{6}\.m4s' | head -n1 || true)"
        fi
    fi

    if [[ -n "${first_segment}" && "${segment_ready_ms}" == "0" ]]; then
        seg_status="$(http_status "${CONTROL_BASE_URL%/}/streams/${stream_id}/${LIVE_RTMP_RENDITION}/${first_segment}")"
        if [[ "${seg_status}" == "200" || "${seg_status}" == "206" ]]; then
            segment_ready_ms=$((now - start_ms))
            break
        fi
    fi

    if ! kill -0 "${ffmpeg_pid}" >/dev/null 2>&1; then
        wait "${ffmpeg_pid}" || ffmpeg_exit_code=$?
        break
    fi

    sleep "${poll_interval_secs}"
done

if kill -0 "${ffmpeg_pid}" >/dev/null 2>&1; then
    kill "${ffmpeg_pid}" >/dev/null 2>&1 || true
    wait "${ffmpeg_pid}" || ffmpeg_exit_code=$?
fi

if [[ "${LIVE_RTMP_DELETE_STREAM}" != "0" ]]; then
    local sink="/dev/null"
    if [[ "${CURL_BIN}" == "curl.exe" ]]; then
        sink="NUL"
    fi
    "${CURL_BIN}" -s -o "${sink}" -X DELETE \
        -H "Authorization: Bearer ${ADMIN_TOKEN}" \
        "${CONTROL_BASE_URL%/}/api/v1/streams/${stream_id}" || true
fi

cat > "${summary_file}" <<EOF
{
  "scenario": "live_rtmp_e2e",
  "timestamp": "${RUN_TS}",
  "stream_id": "${stream_id}",
  "master_ready_ms": ${master_ready_ms},
  "media_ready_ms": ${media_ready_ms},
  "first_segment": "${first_segment}",
  "first_segment_ready_ms": ${segment_ready_ms},
  "publish_seconds": ${LIVE_RTMP_PUBLISH_SECS},
  "ffmpeg_exit_code": ${ffmpeg_exit_code}
}
EOF

echo "Saved live RTMP summary: ${summary_file}"
echo "Saved ffmpeg log: ${ffmpeg_log}"

if [[ "${segment_ready_ms}" == "0" ]]; then
    echo "ERROR: did not observe first media segment within timeout (${LIVE_RTMP_READY_TIMEOUT_SECS}s)." >&2
    exit 1
fi
