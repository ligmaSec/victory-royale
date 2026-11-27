#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Package the verified cross-build with only the files shipped by CI.
set -euo pipefail

if (( $# != 3 )); then
    echo 'Usage: bash scripts/package-proton.sh RELEASE_DIR REVISION OUTPUT_DIR' >&2
    exit 2
fi

src=$(cd -- "$(dirname -- "$0")/.." && pwd -P)
release=$(realpath -e -- "$1")
revision=$2
output_parent=$(realpath -e -- "$(dirname -- "$3")")
output="${output_parent%/}/$(basename -- "$3")"
if [[ ! $revision =~ ^[0-9a-f]{40}$ ]]; then
    echo 'REVISION must be a full Git commit SHA.' >&2
    exit 2
fi
if [[ $output == "$src" || $output == "$src/"* || -e $output || -L $output ]]; then
    echo 'OUTPUT_DIR must be a new directory outside the checkout.' >&2
    exit 2
fi

files=(victory.exe victory-proton.sh LICENSE victory.example.toml tests/proton-launcher.sh)
inputs=("$release/victory.exe" "$src/scripts/victory-proton.sh" "$src/LICENSE"
    "$src/victory.example.toml" "$src/tests/proton-launcher.sh")
for input in "${inputs[@]}"; do
    if [[ ! -f $input || -L $input || $(realpath -e -- "$input") != "$input" ]]; then
        printf 'Not a regular, non-symlink package input: %s\n' "$input" >&2
        exit 1
    fi
done

python3 - "$release" <<'PYTHON'
from pathlib import Path
import sys

release = Path(sys.argv[1])
binary = (release / "victory.exe").read_bytes()
for name in ("argument_clamp", "rect_expand", "rect_rewrite"):
    shaders = list(release.glob(f"build/victory-gpu-*/out/{name}.dxil"))
    if len(shaders) != 1 or not shaders[0].is_file() or shaders[0].resolve() != shaders[0]:
        raise SystemExit(f"Expected one regular GPU helper: {name}")
    data = shaders[0].read_bytes()
    if not data.startswith(b"DXBC") or data not in binary:
        raise SystemExit(f"Missing embedded GPU helper: {name}")
PYTHON

work=$(mktemp -d -- "$output_parent/.victory-proton.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
install -d -m 755 -- "$work/victory" "$work/victory/tests"
for i in "${!files[@]}"; do
    mode=644
    case ${files[i]} in victory.exe|victory-proton.sh) mode=755 ;; esac
    install -m "$mode" -- "${inputs[i]}" "$work/victory/${files[i]}"
done
printf '%s\n' "$revision" > "$work/victory/REVISION"
chmod 644 -- "$work/victory/REVISION"
archive=victory-proton-x86_64.tar.gz
(
    cd -- "$work"
    unset TAR_OPTIONS
    tar -czf "$archive" -- victory
    sha256sum -- "$archive" > SHA256SUMS
)
mkdir -- "$output"
mv -- "$work/$archive" "$work/SHA256SUMS" "$output/"
