#!/usr/bin/env bash

set -euo pipefail

if [[ "$#" -ne 2 ]]; then
    echo "Usage: $0 <before.prom> <after.prom>" >&2
    exit 1
fi

BEFORE_FILE="$1"
AFTER_FILE="$2"

if [[ ! -f "${BEFORE_FILE}" ]]; then
    echo "ERROR: metrics snapshot not found: ${BEFORE_FILE}" >&2
    exit 1
fi

if [[ ! -f "${AFTER_FILE}" ]]; then
    echo "ERROR: metrics snapshot not found: ${AFTER_FILE}" >&2
    exit 1
fi

metric_sum() {
    local file="$1"
    local metric="$2"

    awk -v metric="${metric}" '
        $0 !~ /^#/ {
            if ($1 == metric || index($1, metric "{") == 1) {
                sum += $NF
            }
        }
        END { printf "%.10f", sum + 0 }
    ' "${file}"
}

calc_delta() {
    local before="$1"
    local after="$2"
    awk -v before="${before}" -v after="${after}" 'BEGIN { printf "%.6f", (after - before) }'
}

metric_delta() {
    local metric="$1"
    local before
    local after

    before="$(metric_sum "${BEFORE_FILE}" "${metric}")"
    after="$(metric_sum "${AFTER_FILE}" "${metric}")"
    calc_delta "${before}" "${after}"
}

hist_avg_delta() {
    local metric_base="$1"
    local sum_delta
    local count_delta

    sum_delta="$(metric_delta "${metric_base}_sum")"
    count_delta="$(metric_delta "${metric_base}_count")"

    awk -v sum_delta="${sum_delta}" -v count_delta="${count_delta}" '
        BEGIN {
            if (count_delta <= 0.0) {
                printf "n/a"
            } else {
                printf "%.6f", (sum_delta / count_delta)
            }
        }
    '
}

print_row() {
    local label="$1"
    local value="$2"
    printf "%-48s %s\n" "${label}" "${value}"
}

cache_hits_delta="$(metric_delta "streaminfa_cache_hits_total")"
cache_misses_delta="$(metric_delta "streaminfa_cache_misses_total")"
cache_hit_ratio="$(awk -v h="${cache_hits_delta}" -v m="${cache_misses_delta}" 'BEGIN { t=h+m; if (t<=0) { printf "n/a" } else { printf "%.4f", (h/t) } }')"

print_row "upload_count (delta)" "$(metric_delta "streaminfa_upload_duration_seconds_count")"
print_row "upload_avg_duration_seconds (delta avg)" "$(hist_avg_delta "streaminfa_upload_duration_seconds")"
print_row "upload_avg_size_bytes (delta avg)" "$(hist_avg_delta "streaminfa_upload_size_bytes")"
print_row "transcode_segments_total (delta)" "$(metric_delta "streaminfa_transcode_segments_total")"
print_row "transcode_latency_seconds (delta avg)" "$(hist_avg_delta "streaminfa_transcode_latency_seconds")"
print_row "package_segments_written_total (delta)" "$(metric_delta "streaminfa_package_segments_written_total")"
print_row "package_manifest_updates_total (delta)" "$(metric_delta "streaminfa_package_manifest_updates_total")"
print_row "storage_put_bytes_total (delta)" "$(metric_delta "streaminfa_storage_put_bytes_total")"
print_row "storage_get_bytes_total (delta)" "$(metric_delta "streaminfa_storage_get_bytes_total")"
print_row "storage_put_duration_seconds (delta avg)" "$(hist_avg_delta "streaminfa_storage_put_duration_seconds")"
print_row "storage_get_duration_seconds (delta avg)" "$(hist_avg_delta "streaminfa_storage_get_duration_seconds")"
print_row "delivery_requests_total (delta)" "$(metric_delta "streaminfa_delivery_requests_total")"
print_row "delivery_bytes_sent_total (delta)" "$(metric_delta "streaminfa_delivery_bytes_sent_total")"
print_row "delivery_request_duration_seconds (delta avg)" "$(hist_avg_delta "streaminfa_delivery_request_duration_seconds")"
print_row "cache_hits_total (delta)" "${cache_hits_delta}"
print_row "cache_misses_total (delta)" "${cache_misses_delta}"
print_row "cache_hit_ratio (delta window)" "${cache_hit_ratio}"
print_row "backpressure_events_total (delta)" "$(metric_delta "streaminfa_backpressure_events_total")"
print_row "auth_failures_total (delta)" "$(metric_delta "streaminfa_auth_failures_total")"
print_row "storage_errors_total (delta)" "$(metric_delta "streaminfa_storage_errors_total")"
print_row "transcode_errors_total (delta)" "$(metric_delta "streaminfa_transcode_errors_total")"
print_row "delivery_active_connections (after sum)" "$(metric_sum "${AFTER_FILE}" "streaminfa_delivery_active_connections")"
print_row "transcode_active_jobs (after sum)" "$(metric_sum "${AFTER_FILE}" "streaminfa_transcode_active_jobs")"
print_row "cache_entries (after sum)" "$(metric_sum "${AFTER_FILE}" "streaminfa_cache_entries")"
print_row "cache_size_bytes (after sum)" "$(metric_sum "${AFTER_FILE}" "streaminfa_cache_size_bytes")"
