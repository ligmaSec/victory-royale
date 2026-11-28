#!/usr/bin/env bash
# Exercise the Windows-target build script on the Linux build host.
set -euo pipefail
cd "$(dirname "$0")/../crates/victory-gpu"
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
rustc --edition=2024 -o "$work/build-script" - <<'RS'
#[path = "build_helpers.rs"]
mod build_helpers;
fn main() { build_helpers::bundle(); }
RS
export CARGO_CFG_TARGET_OS=windows OUT_DIR="$work"

env -u VICTORY_BUILD_DXC "$work/build-script" > "$work/unset.log"
VICTORY_BUILD_DXC='' "$work/build-script" > "$work/empty.log"
for log in "$work/unset.log" "$work/empty.log"; do
    grep -Fxq 'cargo:rerun-if-env-changed=VICTORY_BUILD_DXC' "$log"
    if grep -Eq '^cargo:(rustc-cfg=victory_bundled_hlsl|rerun-if-changed=)' "$log"; then
        cat "$log" >&2
        exit 1
    fi
done

if VICTORY_BUILD_DXC="$work/missing-dxc" "$work/build-script" > "$work/invalid.log" 2>&1; then
    echo 'An explicitly configured missing DXC should fail the build.' >&2
    exit 1
fi
grep -Fq "could not start $work/missing-dxc" "$work/invalid.log"
