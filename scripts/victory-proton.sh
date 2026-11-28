#!/usr/bin/env bash
# Start the Windows build with UMU and its Steam Linux Runtime.
set -euo pipefail

usage() {
    cat <<'HELP'
Usage: victory-proton.sh --build | --test
       victory-proton.sh [--run | --report] [--profile tuned-4k]
                         [--runtime proton|pinned | --vkd3d-dir DIR] TITLE.exe [options]

  --test           Run the offline launcher checks (no game, build, or GPU needed).
  --build          Build the Windows release from a source checkout with LLVM-MinGW.
  --run            Execute the title.
  --report         Execute once and save the launcher and Proton logs in one archive.
  --profile tuned-4k  Opt into the experimental native 4K Series X renderer preset.
  --runtime pinned Use the pinned vkd3d-proton build (default for --run and --report).
  --runtime proton Use the vkd3d-proton libraries supplied by Proton.
  --vkd3d-dir DIR  Use the x64 d3d12.dll and d3d12core.dll from DIR for this run.

With no mode, the launcher forwards the title and options to victory run.
Use TITLE.exe alone to check imports, or TITLE.exe --execute to launch.
Play automatically sets up pinned GE-Proton with Royale's indexed wineserver.
Set PROTONPATH only to use a custom indexed Proton build.
Set VICTORY_ALLOW_STOCK_WINESERVER=1 only for stock diagnostics.
HELP
}

apply_tuned_4k() {
    # These are renderer defaults for the preset, not overrides of caller settings.
    local setting name
    for setting in \
        VICTORY_OUTPUT=3840x2160 VICTORY_RENDER_SCALE=1.0 VICTORY_FRAME_INTERVAL_MS=16.667 \
        VICTORY_CARD_WINDOWS_MB=4096 VICTORY_CARD_BUFFERS_MIN_KB=64 VICTORY_CARD_AWAKE=1 \
        VICTORY_THIN=1 VICTORY_JOIN_PARTS=0 VICTORY_JOIN_COPIES=1 VICTORY_PORT_JOURNAL=1 \
        VICTORY_RELAY=2 VICTORY_RELAY_COPY=1 VICTORY_READBACK_POOL=1 VICTORY_READBACK_POOL_MB=32 \
        VICTORY_CARD_REHASH_RUNS=1 VICTORY_CARD_STEPS=2 VICTORY_CARD_STEP_GATHER=1 \
        VICTORY_MAP_THREADS=1 VICTORY_LEVELS_ON_DEMAND=1 VICTORY_TEXTURE_NO_WAIT=1 \
        VICTORY_UPLOAD_AHEAD=1 VICTORY_SHOW_AFTER=1 \
        VICTORY_INDIRECT_WINDOW=1 VICTORY_INDIRECT_WIDE=1 VICTORY_INDIRECT_BATCH=2 \
        VICTORY_PIPELINE_WAIT_MS=0 VICTORY_HAIR=1 VICTORY_AMBIENT=constant:0.35; do
        name=${setting%%=*}
        if [[ ! -v $name ]]; then export "$name=${setting#*=}"; fi
    done
}

pinned_asset=vkd3d-proton-ec20719-x64.tar.gz

verify_runtime_archive() {
    local digest=64b13a6d73bb48394e0135a2048bcf423a69c08226ab410a8a8bde075f5ed4b5
    (cd -- "$1" && printf '%s  %s\n' "$digest" "$pinned_asset" | sha256sum --check --status)
}

stage_pinned_runtime() {
    local stage_dir=$1 cache_dir=$2
    local cache="$cache_dir/$pinned_asset"
    local url="https://github.com/ligmaSec/victory-royale/releases/download/runtime-vkd3d-ec20719/$pinned_asset"
    if [[ ! -f $cache ]] || ! verify_runtime_archive "$cache_dir"; then
        echo 'Downloading pinned vkd3d-proton (ec20719)...' >&2
        curl --disable --fail --location --silent --show-error --proto '=https' --proto-redir '=https' \
            --connect-timeout 15 --max-time 120 --retry 2 --output "$stage_dir/$pinned_asset" "$url"
        if ! verify_runtime_archive "$stage_dir"; then
            echo 'The downloaded runtime failed SHA-256 verification.' >&2
            exit 1
        fi
        # Concurrent launches can publish the same verified bytes atomically.
        mv -fT -- "$stage_dir/$pinned_asset" "$cache"
    fi
    tar -xzf "$cache" -C "$stage_dir" -- d3d12.dll d3d12core.dll
    echo 'Using pinned vkd3d-proton ec20719.' >&2
}

script_dir=$(cd -- "$(dirname -- "$0")" && pwd)
# shellcheck source=scripts/proton-runner.sh
source "$script_dir/proton-runner.sh"

if [[ ${1:-} == --test ]]; then
    if (( $# != 1 )); then usage >&2; exit 2; fi
    suite="$script_dir/tests/proton-launcher.sh"
    if [[ -f $script_dir/../.victory-root ]]; then suite="$script_dir/../tests/proton-launcher.sh"; fi
    exec bash "$suite" "$script_dir/victory-proton.sh"
fi
if [[ ${1:-} == --build ]]; then
    if (( $# != 1 )); then usage >&2; exit 2; fi
    if [[ ! -f $script_dir/../.victory-root ]]; then
        echo 'Build from a source checkout. This distribution is ready to run.' >&2
        exit 2
    fi
    export CC_x86_64_pc_windows_gnullvm=${CC_x86_64_pc_windows_gnullvm:-x86_64-w64-mingw32-clang}
    export AR_x86_64_pc_windows_gnullvm=${AR_x86_64_pc_windows_gnullvm:-llvm-ar}
    if [[ -z ${VICTORY_BUILD_DXC:-} ]] && dxc_path=$(type -P dxc); then
        export VICTORY_BUILD_DXC="$dxc_path"
    fi
    if [[ -n ${CARGO_TARGET_DIR:-} ]]; then
        CARGO_TARGET_DIR=$(realpath -m -- "$CARGO_TARGET_DIR")
        export CARGO_TARGET_DIR
    fi
    cd -- "$script_dir/.."
    exec cargo build --locked --release -p victory --target x86_64-pc-windows-gnullvm
fi

mode=
profile=
runtime=
vkd3d_dir=
while (( $# )); do
    case $1 in
        --help|-h) usage; exit 0 ;;
        --run|--report)
            if [[ -n $mode ]]; then usage >&2; exit 2; fi
            mode=$1
            shift
            ;;
        --profile)
            if [[ -n $profile || ${2:-} != tuned-4k ]]; then
                echo 'Choose --profile tuned-4k once, or omit --profile for standard settings.' >&2
                exit 2
            fi
            profile=$2
            shift 2
            ;;
        --runtime|--vkd3d-dir)
            if [[ -n $runtime || -z ${2:-} ]]; then usage >&2; exit 2; fi
            if [[ $1 == --vkd3d-dir ]]; then
                runtime=custom
                vkd3d_dir=$2
            else
                case $2 in
                    pinned|proton) runtime=$2 ;;
                    *) echo 'Choose --runtime proton or --runtime pinned.' >&2; exit 2 ;;
                esac
            fi
            shift 2
            ;;
        --) shift; break ;;
        *) break ;;
    esac
done
if (( $# == 0 )); then
    usage >&2
    exit 2
fi

target_dir=${CARGO_TARGET_DIR:-$script_dir/../target}
binary=${VICTORY_EXE:-}
if [[ -z $binary ]]; then
    for candidate in "$script_dir/victory.exe" \
        "$target_dir/x86_64-pc-windows-gnullvm/release/victory.exe" \
        "$target_dir/x86_64-pc-windows-msvc/release/victory.exe"; do
        if [[ -f $candidate ]]; then
            binary=$candidate
            break
        fi
    done
fi
if [[ ! -f $binary ]]; then
    echo "Missing Windows build. Run $0 --build from a checkout or set VICTORY_EXE to an existing build." >&2
    exit 1
fi
if [[ ! -f $1 ]]; then
    echo "Missing title executable: $1" >&2
    exit 1
fi
if ! command -v umu-run >/dev/null; then
    echo 'Install umu-launcher to provide umu-run.' >&2
    exit 1
fi
binary=$(realpath -- "$binary")
title=$(realpath -- "$1")
shift
execute_title=0
for arg in "$@"; do
    [[ $arg != -- ]] || break
    if [[ $arg == --execute ]]; then execute_title=1; break; fi
done
if [[ -n $mode ]]; then
    runtime=${runtime:-pinned}
    if (( ! execute_title )); then set -- --execute "$@"; fi
    execute_title=1
fi
if [[ -n $profile ]]; then
    if (( ! execute_title )); then
        echo 'Use --profile with --run, --report, or TITLE.exe --execute.' >&2
        exit 2
    fi
    apply_tuned_4k
fi

data_home=${XDG_DATA_HOME:-$HOME/.local/share}
cache_home=${XDG_CACHE_HOME:-$HOME/.cache}
[[ $data_home == /* ]] || data_home=$HOME/.local/share
[[ $cache_home == /* ]] || cache_home=$HOME/.cache
patched_server=
if (( execute_title )); then
    if [[ -z ${PROTONPATH:-} && ${VICTORY_ALLOW_STOCK_WINESERVER:-0} != 1 ]]; then
        PROTONPATH=$(proton_managed_path "$cache_home/victory/proton" "$script_dir")
        export PROTONPATH
    fi
    # Inspect the selected server itself, not the Proton version label. The
    # fallback diagnostic survives stripping, including in backported builds.
    proton=${PROTONPATH:-}
    if [[ -n $proton ]] && proton=$(realpath -e -- "$proton" 2>/dev/null) &&
        [[ -f $proton/proton && -x $proton/proton ]] &&
        proton_has_view_index "$proton/files/bin/wineserver"; then
        export PROTONPATH=$proton
        unset WINESERVER
        patched_server=$proton/files/bin/wineserver
        echo 'Using Proton with the indexed wineserver.' >&2
    elif [[ ${VICTORY_ALLOW_STOCK_WINESERVER:-0} == 1 ]]; then
        echo 'Warning: the indexed wineserver was not verified; stock diagnostics only.' >&2
    else
        echo 'The requested PROTONPATH does not have the indexed wineserver.' >&2
        echo 'Unset PROTONPATH to install the managed runner, or set VICTORY_ALLOW_STOCK_WINESERVER=1 for diagnostics.' >&2
        exit 1
    fi
fi

# Hand Proton Windows paths on its standard Z: drive. A Windows executable
# path also lets UMU-Proton attach this CLI to the calling terminal.
export GAMEID=${GAMEID:-umu-victory}
export WINEPREFIX=${WINEPREFIX:-$data_home/victory/proton}
# Both translation layers otherwise write derived data to the working directory.
export VKD3D_SHADER_CACHE_PATH=${VKD3D_SHADER_CACHE_PATH:-$cache_home/victory/vkd3d}
export DXVK_STATE_CACHE_PATH=${DXVK_STATE_CACHE_PATH:-$cache_home/victory/dxvk}
# VKD3D reserves 0 for disabling its cache.
if [[ $VKD3D_SHADER_CACHE_PATH != 0 ]]; then
    mkdir -p -- "$VKD3D_SHADER_CACHE_PATH"
fi
mkdir -p -- "$DXVK_STATE_CACHE_PATH"
# Stage an alternate runtime for this process without changing the build or Wine prefix.
stage=
if [[ ${runtime:-proton} != proton ]]; then
    if [[ $runtime == custom ]]; then
        for dll in d3d12.dll d3d12core.dll; do
            if [[ ! -f $vkd3d_dir/$dll ]]; then
                echo "Missing $vkd3d_dir/$dll. Pass the directory containing both x64 DLLs." >&2
                exit 1
            fi
        done
    fi
    mkdir -p -- "$cache_home/victory"
    stage=$(mktemp -d -- "$cache_home/victory/runtime.XXXXXXXX")
    trap 'rm -rf -- "$stage"' EXIT
    cp --reflink=auto -- "$binary" "$stage/victory.exe"
    # Preserve companion libraries such as the optional DXC runtime.
    shopt -s nocaseglob
    for dll in "${binary%/*}"/*.dll; do
        [[ -f $dll ]] || continue
        case ${dll,,} in */d3d12.dll|*/d3d12core.dll) continue ;; esac
        ln -s -- "$dll" "$stage/"
    done
    if [[ $runtime == pinned ]]; then
        stage_pinned_runtime "$stage" "$cache_home/victory"
    else
        cp -- "$vkd3d_dir/d3d12.dll" "$vkd3d_dir/d3d12core.dll" "$stage/"
    fi
    export WINEDLLOVERRIDES="${WINEDLLOVERRIDES:+$WINEDLLOVERRIDES;}d3d12,d3d12core=n"
fi
launch_binary=$binary
if [[ -n $stage ]]; then launch_binary=$stage/victory.exe; fi
windows_title=${title//\//\\}
windows_binary=${launch_binary//\//\\}
caller_install=${STEAM_COMPAT_INSTALL_PATH:-}
export STEAM_COMPAT_INSTALL_PATH=${caller_install:-$(dirname -- "$binary")}

# Prepare the default compat symlink. Failure means this library cannot host
# it: only an Experimental runtime we discovered ourselves may skip it.
prepare_compat_link() {
    local install=$1 target=$2
    mkdir -p -- "${install%/*}" || return 1
    if [[ -e $install && ! -L $install ]]; then
        echo "Steam compat install path already exists and is not a symlink: $install" >&2
        return 1
    fi
    if [[ -L $install && $(readlink -- "$install") != "$target" ]] ||
        [[ ! -e $install && ! -L $install ]]; then
        ln -sfn -- "$target" "$install" || return 1
    fi
}

# Proton Experimental does not accept the Z: paths that UMU-Proton uses.
# Give it Unix paths and a compat install path under its Steam library.
proton_path=${PROTONPATH:-}
auto_experimental=0
library=
if [[ -z $proton_path ]]; then
    for root in "$data_home/Steam" "$HOME/.steam/steam" "$HOME/.local/share/Steam"; do
        experimental="$root/steamapps/common/Proton - Experimental"
        if [[ -f $experimental/proton && -x $experimental/proton ]]; then
            proton_path=$experimental
            library=$root/steamapps
            export PROTONPATH=$proton_path
            auto_experimental=1
            break
        fi
    done
fi
if [[ -z $library && -n $proton_path && -f $proton_path/version ]] &&
    grep -q 'experimental-' "$proton_path/version"; then
    # shortcut: only a Steam layout gives us a library to put the compat path in.
    case $proton_path in
        */steamapps/common/*) library=${proton_path%/common/*} ;;
    esac
fi
if [[ -n $library && -z $caller_install ]]; then
    # A caller-supplied install path owns this decision: the default symlink is
    # none of our business, so do not create, check, or repoint it.
    # Key the link by the canonical package directory so overlapping launches
    # cannot redirect another package's compat install path.
    package=$(dirname -- "$binary")
    package_id=$(printf '%s' "$package" | sha256sum)
    install=$library/common/victory-run-${package_id%% *}
    if prepare_compat_link "$install" "$package"; then
        export STEAM_COMPAT_INSTALL_PATH=$install
    else
        if (( ! auto_experimental )); then exit 1; fi
        echo "warning: could not set up Steam compat install path at $install; continuing with Proton Experimental without the default compat symlink" >&2
        # UMU chooses its own compat install path when none is supplied. Keep
        # PROTONPATH on the detected Experimental: no Wine or Proton downgrade.
        unset STEAM_COMPAT_INSTALL_PATH
    fi
fi
if [[ -n $library ]]; then
    export STEAM_COMPAT_LIBRARY_PATHS=${STEAM_COMPAT_LIBRARY_PATHS:-$library}
    command=(umu-run "$launch_binary" run "$title" "$@")
else
    command=(umu-run "Z:$windows_binary" run "Z:$windows_title" "$@")
fi
if [[ $mode != --report ]]; then
    if [[ -z $stage ]]; then exec "${command[@]}"; fi
    "${command[@]}"
    exit $?
fi

umask 077
reports=$(realpath -m -- "$data_home/victory/reports")
ancestor=$reports
while [[ $ancestor != / ]]; do
    if [[ -f $ancestor/.victory-root ]]; then
        echo 'Set XDG_DATA_HOME outside the source tree to record a run.' >&2
        exit 1
    fi
    ancestor=$(dirname -- "$ancestor")
done
mkdir -p -- "$reports"
report=$(mktemp -d -- "$reports/run-$(date -u +%Y%m%dT%H%M%SZ)-XXXXXXXX")
export PROTON_LOG=1 PROTON_LOG_DIR="$report" VICTORY_TRACE_PIPELINES=1
export WINEDEBUG=${WINEDEBUG:--all,+timestamp,+pid,+tid,+seh}
(
    cd -- "${binary%/*}"
    sha256sum -- "${binary##*/}"
    printf 'runtime=%s\n' "${runtime:-proton}"
    if [[ -n $profile ]]; then printf 'renderer_profile=%s\n' "$profile"; fi
    if [[ $runtime == pinned ]]; then printf 'runtime_asset=%s\n' "$pinned_asset"; fi
    if [[ -n $patched_server ]]; then
        server_digest=$(sha256sum < "$patched_server")
        printf 'wineserver_sha256=%s\n' "${server_digest%% *}"
    else
        printf 'wineserver=unverified\n'
    fi
    if [[ -f REVISION ]]; then
        printf 'package_revision=%s\n' "$(<REVISION)"
    elif [[ -d $script_dir/../.git || -f $script_dir/../.git ]]; then
        revision=$(git -C "$script_dir/.." rev-parse HEAD 2>/dev/null) &&
            printf 'launcher_checkout=%s\n' "$revision"
    fi
    if [[ -n $stage ]]; then
        cd -- "$stage"
        sha256sum -- d3d12.dll d3d12core.dll
    fi
) > "$report/build.txt"
warning="WARNING: LOGS MAY CONTAIN SENSITIVE INFORMATION"
if [[ -t 1 && -z ${NO_COLOR+x} && ${TERM:-} != dumb ]]; then
    printf '\n\033[1;91m%s\033[0m\n' "$warning"
else
    printf '\n%s\n' "$warning"
fi
echo "Raw logs can include usernames, local paths, credentials, or game output. Review them before sharing. Keep the full archive local."
echo "Recording this run in $report"
status=0
trap ':' INT
"${command[@]}" > "$report/launcher.log" 2>&1 || status=$?
trap - INT
printf '%s\n' "$status" > "$report/launcher-exit.txt"
# Keep the raw logs if archiving fails, and publish only complete archives.
if tar -czf "$report.tar.gz.part" -C "$report" .; then
    mv -- "$report.tar.gz.part" "$report.tar.gz"
    rm -rf -- "$report"
    echo "Report: $report.tar.gz (launcher exit $status)"
else
    rm -f -- "$report.tar.gz.part"
    echo "Could not pack the report. Logs are in $report (launcher exit $status)." >&2
    if (( status == 0 )); then status=1; fi
fi
exit "$status"
