#!/usr/bin/env bash
# Install only the tools requested by the current CI job.
set -euo pipefail
cd "${RUNNER_TEMP:?}"
download() {
    local url=$1 checksum=$2 archive=${1##*/}
    curl --fail --silent --show-error --location --retry 3 --output "$archive" "$url"
    printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check --strict
}

case "${1:-}" in
    lint)
        archive=actionlint_1.7.12_linux_amd64.tar.gz
        download "https://github.com/rhysd/actionlint/releases/download/v1.7.12/$archive" \
            8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8
        mkdir -p actionlint-bin
        tar -xf "$archive" -C actionlint-bin actionlint
        ;;
    cross)
        llvm='llvm-mingw-20261117-ucrt-ubuntu-22.04-x86_64'
        download "https://github.com/mstorsjo/llvm-mingw/releases/download/20261117/$llvm.tar.xz" \
            e7a15a49a2ea95e9f3d8a9d0cfabe5052a4b10fb4777cc54de5a8c27b64bb701
        tar -xf "$llvm.tar.xz"
        echo "$PWD/$llvm/bin" >> "$GITHUB_PATH"

        archive=linux_dxc_2026_09_28.x86_x64.tar.gz
        download "https://github.com/microsoft/DirectXShaderCompiler/releases/download/v1.9.2609/$archive" \
            96faadc7f5c282d2ffda49804beb4c3ee38127bc252b723234e3c5cdf7aa39a1
        mkdir -p dxc
        tar -xf "$archive" -C dxc
        echo "VICTORY_BUILD_DXC=$PWD/dxc/bin/dxc" >> "$GITHUB_ENV"
        ;;
    *) echo "Usage: $0 lint|cross" >&2; exit 2 ;;
esac
