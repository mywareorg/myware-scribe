#!/usr/bin/env bash
# Report per-stage timings for one or more audio files.
#
#   scripts/benchmark.sh [--preset speed|balanced|accuracy] [--speakers N] <audio>...
#
# Runs the CLI and prints the pipeline's own `timing` lines. Build first, e.g.
# `cargo build --profile release-fast`, and set BIN to override the binary.
set -euo pipefail

preset="${PRESET:-accuracy}"
speakers="${SPEAKERS:-2}"
bin="${BIN:-target/release-fast/hippo-transcribe}"
files=()

while [ $# -gt 0 ]; do
    case "$1" in
        --preset)
            shift
            preset="${1:?--preset needs a value}"
            ;;
        --speakers)
            shift
            speakers="${1:?--speakers needs a value}"
            ;;
        -h|--help)
            sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            files+=("$1")
            ;;
    esac
    shift
done

[ ${#files[@]} -gt 0 ] || {
    echo "usage: scripts/benchmark.sh [--preset ...] [--speakers N] <audio>..." >&2
    exit 2
}
[ -x "$bin" ] || {
    echo "missing $bin (cargo build --profile release-fast)" >&2
    exit 1
}

out="${TMPDIR:-/tmp}/hippo-bench"
mkdir -p "$out"

for file in "${files[@]}"; do
    echo "== $file (preset $preset, $speakers speakers)"
    HIPPO_TRANSCRIBE_TIER=pro "$bin" --cli "$file" \
        --preset "$preset" --num-speakers "$speakers" --language auto \
        --output-dir "$out" 2>&1 | grep -E '^timing ' || true
done
