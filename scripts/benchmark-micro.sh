#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_OUT_DIR="${BENCH_OUT_DIR:-${SCRIPT_DIR}/../.tmp/benchmarks/$(date +%Y%m%d-%H%M%S)}"

mkdir -p "${BENCH_OUT_DIR}"
RUN_TS="$(date +%Y%m%d-%H%M%S)"
report_file="${BENCH_OUT_DIR}/micro-${RUN_TS}.txt"

cargo_cmd="cargo"
if ! command -v "${cargo_cmd}" >/dev/null 2>&1; then
    if command -v cargo.exe >/dev/null 2>&1; then
        cargo_cmd="cargo.exe"
    else
        echo "ERROR: cargo is not available in PATH." >&2
        exit 1
    fi
fi

# Avoid Windows path-with-spaces linker edge cases seen in this workspace.
if [[ -z "${CARGO_TARGET_DIR:-}" ]]; then
    if [[ "${cargo_cmd}" == "cargo.exe" ]]; then
        CARGO_TARGET_DIR="C:\\temp\\streaminfa-target"
    else
        CARGO_TARGET_DIR="/tmp/streaminfa-target"
    fi
fi
export CARGO_TARGET_DIR

# Ensure temp directory is stable for linker helper tools.
if [[ "${cargo_cmd}" == "cargo.exe" ]]; then
    export TEMP="C:\\temp"
    export TMP="C:\\temp"
fi

echo "== Pipeline Microbenchmarks =="
echo "target_dir=${CARGO_TARGET_DIR}"
echo "report=${report_file}"

"${cargo_cmd}" bench --bench pipeline_micro --target-dir "${CARGO_TARGET_DIR}" -- --noplot | tee "${report_file}"

echo "Saved microbenchmark report: ${report_file}"
