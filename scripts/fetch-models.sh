#!/usr/bin/env bash
# Fetch the pinned model files into vendor/models (git-ignored) for packaging.
#
# Reads models.lock.json, the same manifest the app compiles in, so build-time
# staging and runtime downloads can never disagree.
#
# Usage:
#   scripts/fetch-models.sh [--all] [--force] [--verify-only] [--mirror URL] [--dest DIR]
#
#   --all         also fetch models that releases download on demand
#   --force       re-download even when a cached copy is valid
#   --verify-only check the cache without hitting the network
#   --mirror URL  try URL/<file> before the pinned upstream mirrors; also read
#                 from HIPPO_TRANSCRIBE_MODEL_MIRROR
#   --dest DIR    stage into DIR (used by packaging); defaults to
#                 vendor/models/<manifest version>
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="$root/models.lock.json"
dest=""
mirror="${HIPPO_TRANSCRIBE_MODEL_MIRROR:-}"
include_all=0
force=0
verify_only=0

while [ $# -gt 0 ]; do
    case "$1" in
        --all) include_all=1 ;;
        --force) force=1 ;;
        --verify-only) verify_only=1 ;;
        --dest)
            shift
            [ $# -gt 0 ] || { echo "--dest needs a directory" >&2; exit 2; }
            dest="$1"
            ;;
        --mirror)
            shift
            [ $# -gt 0 ] || { echo "--mirror needs a URL" >&2; exit 2; }
            mirror="$1"
            ;;
        -h|--help) sed -n '2,22p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

[ -f "$manifest" ] || { echo "missing $manifest" >&2; exit 1; }

version="$(jq -r '.version' "$manifest" 2>/dev/null || python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["version"])' "$manifest")"
if [ -z "${dest:-}" ]; then
    dest="$root/vendor/models/$version"
fi
mkdir -p "$dest"

# id<TAB>relative file<TAB>size<TAB>sha256<TAB>first mirror
rows="$(
    if command -v jq >/dev/null 2>&1; then
        jq -r --argjson all "$include_all" '
            .models[] | select($all == 1 or .bundled) |
            [.id, .file, (.size|tostring), .sha256, .mirrors[0]] | @tsv
        ' "$manifest"
    else
        python3 - "$manifest" "$include_all" <<'PY'
import json, sys
manifest = json.load(open(sys.argv[1]))
include_all = sys.argv[2] == "1"
for model in manifest["models"]:
    if not include_all and not model.get("bundled"):
        continue
    print("\t".join([model["id"], model["file"], str(model["size"]),
                     model["sha256"], model["mirrors"][0]]))
PY
    fi
)"

file_size() {
    wc -c < "$1" | tr -d ' '
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

ok=0
failed=0
while IFS=$'\t' read -r id file size sha upstream; do
    [ -n "$id" ] || continue
    target="$dest/$file"

    if [ -f "$target" ] && [ "$(file_size "$target")" = "$size" ] && [ "$(sha256_of "$target")" = "$sha" ]; then
        echo "[ok]   $id ($(basename "$file"))"
        ok=$((ok + 1))
        continue
    fi
    if [ "$verify_only" = 1 ]; then
        echo "[bad]  $id -> $target" >&2
        failed=$((failed + 1))
        continue
    fi

    mkdir -p "$(dirname "$target")"
    urls=()
    if [ -n "$mirror" ]; then
        urls+=("${mirror%/}/$(basename "$file")")
    fi
    urls+=("$upstream")

    downloaded=0
    for url in "${urls[@]}"; do
        echo "[get]  $id <- $url"
        rm -f "$target.part"
        if curl --fail --location --retry 3 --silent --show-error \
                --output "$target.part" "$url" \
            && [ "$(file_size "$target.part")" = "$size" ] \
            && [ "$(sha256_of "$target.part")" = "$sha" ]; then
            mv "$target.part" "$target"
            downloaded=1
            break
        fi
        echo "[warn] mirror failed, trying the next one" >&2
        rm -f "$target.part"
    done

    if [ "$downloaded" = 1 ]; then
        echo "[ok]   $id -> $target"
        ok=$((ok + 1))
    else
        echo "[fail] $id: every mirror failed" >&2
        failed=$((failed + 1))
    fi
done <<< "$rows"

# Remove the flat-name alias directory entry if a nested file exists.
echo "model set v$version: $ok ok, $failed failed ($dest)"
[ "$failed" = 0 ]
