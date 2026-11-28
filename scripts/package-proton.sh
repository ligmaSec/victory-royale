#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Package the verified cross-build with only the files shipped by CI.
set -euo pipefail

if (( $# != 4 )); then
    echo 'Usage: bash scripts/package-proton.sh RELEASE_DIR REVISION OUTPUT_DIR SERVER_BUILD_DIR' >&2
    exit 2
fi

src=$(cd -- "$(dirname -- "$0")/.." && pwd -P)
release=$(realpath -e -- "$1")
revision=$2
output_parent=$(realpath -e -- "$(dirname -- "$3")")
output="${output_parent%/}/$(basename -- "$3")"
server_build=$(realpath -e -- "$4")
if [[ ! $revision =~ ^[0-9a-f]{40}$ ]]; then
    echo 'REVISION must be a full Git commit SHA.' >&2
    exit 2
fi
if [[ $output == "$src" || $output == "$src/"* || -e $output || -L $output ]]; then
    echo 'OUTPUT_DIR must be a new directory outside the checkout.' >&2
    exit 2
fi

files=(victory.exe victory-proton.sh LICENSE victory.example.toml proton-runner.sh wineserver
    wineserver-source.tar.gz wineserver-view-tree.patch docs/proton-launcher.md tests/proton-launcher.sh)
inputs=("$release/victory.exe" "$src/scripts/victory-proton.sh" "$src/LICENSE"
    "$src/victory.example.toml" "$src/scripts/proton-runner.sh" "$server_build/wineserver"
    "$server_build/wineserver-source.tar.gz" "$src/scripts/wineserver-view-tree.patch"
    "$src/docs/proton-launcher.md" "$src/tests/proton-launcher.sh")
for input in "${inputs[@]}"; do
    if [[ ! -f $input || -L $input || $(realpath -e -- "$input") != "$input" ]]; then
        printf 'Not a regular, non-symlink package input: %s\n' "$input" >&2
        exit 1
    fi
done
# shellcheck source=scripts/proton-runner.sh
source "$src/scripts/proton-runner.sh"
if ! proton_has_view_index "$server_build/wineserver" || [[ ! -s $server_build/wineserver-source.tar.gz ]]; then
    echo 'Build the indexed wineserver with --with-source before packaging.' >&2
    exit 1
fi

python3 - "$release" <<'PYTHON'
from pathlib import Path
import re
import sys

release = Path(sys.argv[1])
binary = (release / "victory.exe").read_bytes()
manifests = list(release.glob("build/victory-gpu-*/out/package-hlsl.list"))
if len(manifests) != 1 or not manifests[0].is_file() or manifests[0].resolve() != manifests[0]:
    raise SystemExit("Expected one regular embedded GPU helper manifest")
manifest = manifests[0]
names = manifest.read_text(encoding="ascii").splitlines()
if not names or len(names) != len(set(names)) or any(not re.fullmatch(r"[a-z0-9_]+", n) for n in names):
    raise SystemExit("Invalid embedded GPU helper manifest")
for name in names:
    shader = manifest.with_name(f"{name}.dxil")
    if not shader.is_file() or shader.resolve() != shader:
        raise SystemExit(f"Expected regular GPU helper: {name}")
    data = shader.read_bytes()
    if not data.startswith(b"DXBC") or data not in binary:
        raise SystemExit(f"Missing embedded GPU helper: {name}")
PYTHON

work=$(mktemp -d -- "$output_parent/.victory-proton.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
install -d -m 755 -- "$work/victory/docs" "$work/victory/tests"
for i in "${!files[@]}"; do
    mode=644
    case ${files[i]} in victory.exe|victory-proton.sh|wineserver) mode=755 ;; esac
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
