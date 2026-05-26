#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage:
  scripts/package_obtainium_zips.sh [apk ...]

Environment overrides:
  JOBS=...  Maximum APKs to package at once. Defaults to nproc when available,
            otherwise the number of APKs.

Creates Obtainium-compatible Deflate ZIP files for release APKs. With no
arguments, packages every *.apk in mobile/release. With arguments, packages the
specified APK files.

The output for each APK is written next to it as <apk>.zip.
EOF
}

if [[ ${1:-} == "-h" || ${1:-} == "--help" ]]; then
    usage
    exit 0
fi

for command in advzip basename dirname find mktemp mv rm sort unzip zip; do
    if ! command -v "$command" >/dev/null; then
        echo "Missing required command: $command" >&2
        exit 2
    fi
done

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_dir=$(cd -- "$script_dir/.." && pwd)
release_dir=$repo_dir/mobile/release
apks=()

if (( $# > 0 )); then
    for apk in "$@"; do
        apks+=("$apk")
    done
else
    while IFS= read -r -d '' apk; do
        apks+=("$apk")
    done < <(find "$release_dir" -maxdepth 1 -type f -name '*.apk' -print0 | sort -z)
fi

if (( ${#apks[@]} == 0 )); then
    echo "No APK files found in $release_dir" >&2
    exit 1
fi

package_apk() {
    local apk=$1
    local output=$apk.zip
    local output_dir
    local output_base
    local tmp

    output_dir=$(dirname -- "$output")
    output_base=$(basename -- "$output")
    tmp=$(mktemp --tmpdir="$output_dir" ".$output_base.tmp.XXXXXX")
    rm -f "$tmp"
    trap 'rm -f "$tmp"' EXIT

    echo "Packaging $apk -> $output"
    zip -9 -X -D -j "$tmp" "$apk"
    advzip -z4 -i 15 "$tmp"
    advzip -t "$tmp"
    unzip -t "$tmp"
    mv -f "$tmp" "$output"
    trap - EXIT
}

for apk in "${apks[@]}"; do
    if [[ ! -f $apk ]]; then
        echo "APK not found: $apk" >&2
        exit 1
    fi
done

if [[ -z ${JOBS:-} ]]; then
    if command -v nproc >/dev/null; then
        JOBS=$(nproc)
    else
        JOBS=${#apks[@]}
    fi
fi
if [[ ! $JOBS =~ ^[1-9][0-9]*$ ]]; then
    echo "JOBS must be a positive integer: $JOBS" >&2
    exit 2
fi

status=0
running=0
for apk in "${apks[@]}"; do
    package_apk "$apk" &
    ((++running))
    if (( running >= JOBS )); then
        wait -n || status=1
        ((--running))
    fi
done
while (( running > 0 )); do
    wait -n || status=1
    ((--running))
done
exit "$status"
