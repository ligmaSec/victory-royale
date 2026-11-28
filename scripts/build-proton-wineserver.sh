#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Build only the native server from GE-Proton11-14's prepared Wine tree.
set -euo pipefail

with_source=0
if [[ ${1:-} == --with-source ]]; then with_source=1; shift; fi
if (( $# != 2 )); then
    echo 'Usage: bash scripts/build-proton-wineserver.sh [--with-source] OUTPUT_DIR DOWNLOAD_DIR' >&2
    exit 2
fi
script_dir=$(cd -- "$(dirname -- "$0")" && pwd -P)
# shellcheck source=scripts/proton-runner.sh
source "$script_dir/proton-runner.sh"
output=$(realpath -m -- "$1")
cache=$(realpath -m -- "$2")
mkdir -p -- "${output%/*}" "$cache"
if [[ -e $output || -L $output ]]; then
    printf 'Server build destination already exists: %s\n' "$output" >&2
    exit 1
fi
for program in cc c++ flex bison autoreconf make perl patch python3 strip; do
    if ! command -v "$program" >/dev/null; then
        printf 'Missing %s for the one-time source wineserver build. See docs/proton-launcher.md.\n' "$program" >&2
        exit 1
    fi
done

ge=777d72b4ae50b3121ac46df2a7605bb2d6156840
wine=a47c73f52fd7b2efafc1c560710f3a5099ae51e0
staging=6cc805ea57132eeaf44764e9213823c9b8d0d300
configure_parent=82fa1cf334159d5c4261f8f1ff345165e7942ec0

proton_fetch_file "$cache/ge.tar.gz" "https://codeload.github.com/GloriousEggroll/proton-ge-custom/tar.gz/$ge" \
    20756e85b75b9ea8d41aba8dbdda4e5d331571a939d6e34a420e671f90c94a9f
proton_fetch_file "$cache/wine.tar.gz" "https://codeload.github.com/ValveSoftware/wine/tar.gz/$wine" \
    b93c1f6dce3c082b21b08d2dd8ab129c4a25ada741deead3267ebbadd532097f
proton_fetch_file "$cache/staging.tar.gz" "https://codeload.github.com/wine-staging/wine-staging/tar.gz/$staging" \
    46fccc6412d5a0ac5e02047a545bba21c3db45abfa1c161119a1de2f7b8cb360
proton_fetch_file "$cache/configure" "https://raw.githubusercontent.com/ValveSoftware/wine/$configure_parent/configure" \
    73f7de7ff925dca71ea419269bfbfd7e9959ecd20e3247b540a395570f950352
proton_fetch_file "$cache/config.h.in" "https://raw.githubusercontent.com/ValveSoftware/wine/$configure_parent/include/config.h.in" \
    8e6aa7d6789ff91be4be63da67e61a60a4d2a058da4bbbefed4c5e7bd095eef5

work=$(mktemp -d -- "${output%/*}/.build-wineserver.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
unset TAR_OPTIONS
mkdir -p -- "$work/ge/wine" "$work/ge/wine-staging" "$work/build" "$work/output"
tar -xzf "$cache/ge.tar.gz" --strip-components=1 -C "$work/ge"
tar -xzf "$cache/wine.tar.gz" --strip-components=1 -C "$work/ge/wine"
tar -xzf "$cache/staging.tar.gz" --strip-components=1 -C "$work/ge/wine-staging"

# GE resets its Wine submodule and reverses e813ca577165 before applying
# patches. Our source is already pristine; restore exactly the two deleted
# generated files and run only its pinned Wine section, not the GPU projects.
install -m 755 -- "$cache/configure" "$work/ge/wine/configure"
install -m 644 -- "$cache/config.h.in" "$work/ge/wine/include/config.h.in"
awk '
    BEGIN { helpers = 1 }
    /^### \(1\) PREP SECTION ###$/ { helpers = 0; header++ }
    /^### \(2\) WINE PATCHING ###$/ { wine = 1; start++ }
    /^### END WINE PATCHING ###$/ { wine = 0; end++ }
    wine && /^    (autoreconf -f|[.]\/tools\/make_requests)$/ { print $0 " || exit 1"; next }
    (helpers || wine) && !/^    git (reset --hard HEAD|clean -xdf|revert --no-commit e813ca5771658b00875924ab88d525322e50d39f)$/ { print }
    END { if (header != 1 || start != 1 || end != 1) exit 1 }
' "$work/ge/patches/protonprep-valve-staging.sh" > "$work/prepare.sh"
echo 'Preparing the pinned GE Wine source…' >&2
if ! ( cd -- "$work/ge"; bash "$work/prepare.sh" ) > "$work/prepare.log" 2>&1; then
    tail -n 60 -- "$work/prepare.log" >&2
    exit 1
fi
if grep -Fq 'Failed to apply patch ' "$work/prepare.log" ||
   [[ -n $(find "$work/ge/wine" -name '*.rej' -print -quit) ]]; then
    echo 'GE Wine preparation left a rejected patch.' >&2
    tail -n 60 -- "$work/prepare.log" >&2
    exit 1
fi
patch --batch --fuzz=0 -p1 --directory="$work/ge/wine" < "$script_dir/wineserver-view-tree.patch"

# GE generates these before configure, whose dependency scanner reads the
# whole Wine source tree even if make will only build the native server.
(
    cd -- "$work/ge/wine"
    XDG_CACHE_HOME="$work/build" dlls/winevulkan/make_vulkan -x vk.xml -X video.xml
    tools/make_specfiles
)

echo 'Building the indexed wineserver…' >&2
# Static linking avoids depending on the developer host's glibc version inside
# the Steam Linux Runtime. The client and its DLLs still come from stock GE.
if ! (
    cd -- "$work/build" &&
    CC=cc CFLAGS=-O2 LDFLAGS=-static "$work/ge/wine/configure" -q \
        --enable-win64 --without-mingw --without-x --without-freetype &&
    make -s -j "$(nproc)" server/wineserver
) > "$work/build.log" 2>&1; then
    tail -n 60 -- "$work/build.log" >&2
    exit 1
fi
strip -o "$work/output/wineserver" "$work/build/server/wineserver"
proton_has_view_index "$work/output/wineserver"
if (( with_source )); then
    tar -C "$work/ge" --exclude='wine/autom4te.cache' -cf - wine |
        gzip -n -1 > "$work/output/wineserver-source.tar.gz"
fi
mv -T -- "$work/output" "$output"
