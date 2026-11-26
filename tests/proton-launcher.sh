#!/usr/bin/env bash
# Launcher regression checks: synthetic files and a fake UMU, no GPU or Wine.
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")/.." && pwd)
launcher=$(realpath -- "${1:-$root/scripts/victory-proton.sh}")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
mkdir -p "$work/bin" "$work/build with spaces" "$work/game with spaces"
touch "$work/build with spaces/victory.exe" "$work/game with spaces/title.exe"
cat > "$work/bin/umu-run" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$@" > "$LAUNCH_RECORD"
printf '%s\n' "$WINEPREFIX" "$VKD3D_SHADER_CACHE_PATH" "$DXVK_STATE_CACHE_PATH" > "$ENV_RECORD"
if [[ -n ${STEAM_RECORD:-} ]]; then
    printf '%s\n' "${STEAM_COMPAT_INSTALL_PATH:-}" "${STEAM_COMPAT_LIBRARY_PATHS:-}" > "$STEAM_RECORD"
fi
if [[ -n ${PROTON_RECORD:-} ]]; then
    printf '%s\n' "${PROTONPATH:-}" > "$PROTON_RECORD"
fi
# Hold the first UMU invocation open while the other package launches.
if [[ -n ${SECOND_BUILD:-} ]]; then
    env -u STEAM_COMPAT_INSTALL_PATH -u STEAM_COMPAT_LIBRARY_PATHS -u PROTONPATH \
        VICTORY_EXE="$SECOND_BUILD/victory.exe" SECOND_BUILD= \
        bash "$TEST_LAUNCHER" --run --runtime proton "$3" --console series-x
fi
if [[ ${CHECK_PACKAGE_PATH:-} == 1 ]]; then
    test "$PROTONPATH" = "$HOME/.local/share/Steam/steamapps/common/Proton - Experimental"
    test "$1" = "$VICTORY_EXE"
    test "$(realpath -e -- "$STEAM_COMPAT_INSTALL_PATH")" = "${VICTORY_EXE%/*}"
fi
if [[ -n ${EXPECTED_RUNTIME:-} ]]; then
    exe=${1#Z:}
    exe=${exe//\\/\/}
    cmp -- "$exe" "$VICTORY_EXE"
    cmp -- "${exe%/*}/d3d12.dll" "$EXPECTED_RUNTIME/d3d12.dll"
    cmp -- "${exe%/*}/d3d12core.dll" "$EXPECTED_RUNTIME/d3d12core.dll"
    cmp -- "${exe%/*}/dxcompiler.dll" "${VICTORY_EXE%/*}/dxcompiler.dll"
    [[ $WINEDLLOVERRIDES == 'example=b;d3d12,d3d12core=n' ]]
    printf '%s\n' "${exe%/*}" > "$STAGE_RECORD"
fi
if [[ ${PROTON_LOG:-} == 1 ]]; then
    test "$VICTORY_TRACE_PIPELINES" = 1
    echo 'Proton output' > "$PROTON_LOG_DIR/steam-victory.log"
    echo 'Victory output'
    echo 'UMU output' >&2
fi
exit "${UMU_EXIT:-0}"
EOF
chmod +x "$work/bin/umu-run"
# Any direct Wine invocation is a regression: Experimental must go via umu-run.
for wine_binary in wine wine64 wineserver; do
    cat > "$work/bin/$wine_binary" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$0" >> "$WINE_RECORD"
exit 1
EOF
    chmod +x "$work/bin/$wine_binary"
done
export PATH="$work/bin:$PATH"
export WINE_RECORD="$work/wine-invocations"
export HOME="$work/home"
export XDG_DATA_HOME="$work/data" XDG_CACHE_HOME="$work/cache"
export VICTORY_EXE="$work/build with spaces/victory.exe"
export LAUNCH_RECORD="$work/args" ENV_RECORD="$work/env"
unset WINEPREFIX VKD3D_SHADER_CACHE_PATH DXVK_STATE_CACHE_PATH PROTON_LOG EXPECTED_RUNTIME

bash "$launcher" "$work/game with spaces/title.exe" --execute --console series-s -- "argument with spaces"
binary=${VICTORY_EXE//\//\\}
title="$work/game with spaces/title.exe"
title=${title//\//\\}
printf '%s\n' "Z:$binary" run "Z:$title" --execute --console series-s -- "argument with spaces" > "$work/expected"
diff -u "$work/expected" "$LAUNCH_RECORD"
printf '%s\n' "$work/data/victory/proton" "$work/cache/victory/vkd3d" "$work/cache/victory/dxvk" > "$work/expected"
diff -u "$work/expected" "$ENV_RECORD"
test -d "$work/cache/victory/vkd3d"
test -d "$work/cache/victory/dxvk"

# Relative XDG bases are invalid; follow the same fallback as the native tools.
export XDG_DATA_HOME=relative XDG_CACHE_HOME=relative
bash "$launcher" "$work/game with spaces/title.exe"
printf '%s\n' "$HOME/.local/share/victory/proton" "$HOME/.cache/victory/vkd3d" "$HOME/.cache/victory/dxvk" > "$work/expected"
diff -u "$work/expected" "$ENV_RECORD"

# Preserve explicit paths and propagate UMU's failure status.
export WINEPREFIX="$work/custom prefix"
export VKD3D_SHADER_CACHE_PATH="$work/custom vkd3d" DXVK_STATE_CACHE_PATH="$work/custom dxvk"
status=0
UMU_EXIT=42 bash "$launcher" "$work/game with spaces/title.exe" || status=$?
test "$status" -eq 42
printf '%s\n' "$WINEPREFIX" "$VKD3D_SHADER_CACHE_PATH" "$DXVK_STATE_CACHE_PATH" > "$work/expected"
diff -u "$work/expected" "$ENV_RECORD"

# VKD3D treats 0 as "disable caching", not as a directory name.
export VKD3D_SHADER_CACHE_PATH=0 DXVK_STATE_CACHE_PATH="$work/disabled vkd3d/dxvk"
(
    cd "$work"
    bash "$launcher" "$work/game with spaces/title.exe"
)
test ! -e "$work/0"
test -d "$DXVK_STATE_CACHE_PATH"
printf '%s\n' "$WINEPREFIX" 0 "$DXVK_STATE_CACHE_PATH" > "$work/expected"
diff -u "$work/expected" "$ENV_RECORD"
# A report stages an optional runtime and captures this run even when UMU fails.
export EXPECTED_RUNTIME="$work/runtime with spaces" STAGE_RECORD="$work/stage"
export WINEDLLOVERRIDES=example=b
mkdir -p "$EXPECTED_RUNTIME"
printf 'new stub' > "$EXPECTED_RUNTIME/d3d12.dll"
printf 'new core' > "$EXPECTED_RUNTIME/d3d12core.dll"
printf 'original' > "${VICTORY_EXE%/*}/d3d12.dll"
touch "${VICTORY_EXE%/*}/dxcompiler.dll"
status=0
UMU_EXIT=3 bash "$launcher" --report --vkd3d-dir "$EXPECTED_RUNTIME" "$work/game with spaces/title.exe" \
    --console series-s -- "--execute" > "$work/report-output" || status=$?
test "$status" -eq 3
tail -n +4 "$LAUNCH_RECORD" > "$work/options"
printf '%s\n' --execute --console series-s -- --execute | diff -u - "$work/options"
test ! -e "$(<"$STAGE_RECORD")"
test "$(<"${VICTORY_EXE%/*}/d3d12.dll")" = original
reports="$HOME/.local/share/victory/reports"
archives=("$reports"/*.tar.gz)
test "${#archives[@]}" -eq 1
grep -Fxq "WARNING: LOGS MAY CONTAIN SENSITIVE INFORMATION" "$work/report-output"
if grep -Fq $'\033[' "$work/report-output"; then exit 1; fi
grep -Fq "${archives[0]}" "$work/report-output"
tar -xOf "${archives[0]}" ./launcher-exit.txt | grep -qx 3
tar -xOf "${archives[0]}" ./steam-victory.log | grep -qx 'Proton output'
tar -xOf "${archives[0]}" ./launcher.log | diff -u <(printf 'Victory output\nUMU output\n') -
tar -xOf "${archives[0]}" ./build.txt | grep -Fq victory.exe
if tar -tf "${archives[0]}" | grep -Eq '\.(exe|dll)$'; then exit 1; fi
if bash "$launcher" --run --vkd3d-dir "$work/nonexistent" "$work/game with spaces/title.exe" 2>/dev/null; then
    exit 1
fi
unset EXPECTED_RUNTIME
if bash "$launcher" --runtime pinned --vkd3d-dir "$work" "$work/game with spaces/title.exe" 2>/dev/null; then
    exit 1
fi
if bash "$launcher" --runtime unknown "$work/game with spaces/title.exe" 2>/dev/null; then exit 1; fi
mkdir -p "$work/source"
touch "$work/source/.victory-root"
ln -s "$work/source" "$work/data-link"
if XDG_DATA_HOME="$work/data-link" bash "$launcher" --report --runtime proton "$work/game with spaces/title.exe" 2>/dev/null; then
    exit 1
fi
test ! -d "$work/source/victory/reports"
bash "$launcher" --run --runtime proton "$work/game with spaces/title.exe" --execute
head -n 1 "$LAUNCH_RECORD" | diff -u <(printf '%s\n' "Z:$binary") -
tail -n +4 "$LAUNCH_RECORD" | diff -u <(echo --execute) -

# An extracted distribution finds the executable beside its launcher.
mkdir -p "$work/extracted bundle"
cp "$launcher" "$work/extracted bundle/victory-proton.sh"
touch "$work/extracted bundle/victory.exe"
unset VICTORY_EXE
export CARGO_TARGET_DIR="$work/no-build-tree"
bash "$work/extracted bundle/victory-proton.sh" "$work/game with spaces/title.exe"
read -r launched < "$LAUNCH_RECORD"
expected="$work/extracted bundle/victory.exe"
test "$launched" = "Z:${expected//\//\\}"
if bash "$work/extracted bundle/victory-proton.sh" --build 2>/dev/null; then exit 1; fi

# Steam's Proton Experimental gets Unix paths and a compat install symlink.
export VICTORY_EXE="$work/build with spaces/victory.exe"
export STEAM_RECORD="$work/steam-env"
unset PROTONPATH STEAM_COMPAT_INSTALL_PATH STEAM_COMPAT_LIBRARY_PATHS
steam_root="$HOME/.local/share/Steam"
experimental="$steam_root/steamapps/common/Proton - Experimental"
# A partial install in the first Steam root must not mask a usable runtime.
export XDG_DATA_HOME="$work/steam data"
mkdir -p "$XDG_DATA_HOME/Steam/steamapps/common/Proton - Experimental" "$experimental"
touch "$experimental/proton"
chmod +x "$experimental/proton"
printf '1794497090 experimental-11.0-20261112-x86_64\n' > "$experimental/version"
bash "$launcher" "$work/game with spaces/title.exe" --execute
mapfile -t launched_args < "$LAUNCH_RECORD"
test "${launched_args[0]}" = "$VICTORY_EXE"
test "${launched_args[1]}" = run
test "${launched_args[2]}" = "$work/game with spaces/title.exe"
mapfile -t steam_env < "$STEAM_RECORD"
default_install=${steam_env[0]}
[[ $default_install == "$steam_root/steamapps/common/"* ]]
test "${steam_env[1]}" = "$steam_root/steamapps"
test "$(readlink -- "${steam_env[0]}")" = "$work/build with spaces"

# Each running package keeps its own install directory. Both launches
# auto-select Experimental and receive Unix executable and title paths.
mkdir -p "$work/other build"
touch "$work/other build/victory.exe"
TEST_LAUNCHER="$launcher" SECOND_BUILD="$work/other build" CHECK_PACKAGE_PATH=1 \
    bash "$launcher" --run --runtime proton "$work/game with spaces/title.exe" --console series-x

# Repair a stale or dangling link at this package's own path.
ln -sfn -- "$work/other build" "$default_install"
bash "$launcher" "$work/game with spaces/title.exe"
test "$(readlink -- "$default_install")" = "$work/build with spaces"
ln -sfn -- "$work/missing build" "$default_install"
bash "$launcher" "$work/game with spaces/title.exe"
test "$(readlink -- "$default_install")" = "$work/build with spaces"

# Caller-provided compat paths are preserved.
export STEAM_COMPAT_INSTALL_PATH="$work/custom install"
export STEAM_COMPAT_LIBRARY_PATHS="$work/custom library"
bash "$launcher" "$work/game with spaces/title.exe"
mapfile -t steam_env < "$STEAM_RECORD"
test "${steam_env[0]}" = "$work/custom install"
test "${steam_env[1]}" = "$work/custom library"
unset STEAM_COMPAT_INSTALL_PATH STEAM_COMPAT_LIBRARY_PATHS

# An explicit install path bypasses the default symlink, even when an ordinary
# directory occupies the proposed default path.
rm -f -- "$default_install"
mkdir -p -- "$default_install"
printf 'caller data\n' > "$default_install/marker"
export STEAM_COMPAT_INSTALL_PATH="$work/custom install"
bash "$launcher" "$work/game with spaces/title.exe"
mapfile -t launched_args < "$LAUNCH_RECORD"
test "${launched_args[0]}" = "$VICTORY_EXE"
mapfile -t steam_env < "$STEAM_RECORD"
test "${steam_env[0]}" = "$work/custom install"
test "${steam_env[1]}" = "$steam_root/steamapps"
test -d "$default_install"
test ! -L "$default_install"
test "$(<"$default_install/marker")" = 'caller data'
unset STEAM_COMPAT_INSTALL_PATH

# Automatic Experimental discovery does not own the Steam library: an unusable
# compat path is skipped, but the launch still uses Experimental via umu-run.
: > "$LAUNCH_RECORD"
PROTON_RECORD="$work/proton-env" bash "$launcher" "$work/game with spaces/title.exe" \
    2> "$work/fallback-warning"
grep -Fq 'continuing with Proton Experimental without the default compat symlink' "$work/fallback-warning"
mapfile -t launched_args < "$LAUNCH_RECORD"
test "${launched_args[0]}" = "$VICTORY_EXE"
test "${launched_args[1]}" = run
test "${launched_args[2]}" = "$work/game with spaces/title.exe"
mapfile -t steam_env < "$STEAM_RECORD"
test "${steam_env[0]}" = ""
test "${steam_env[1]}" = "$steam_root/steamapps"
test "$(<"$work/proton-env")" = "$experimental"
test ! -s "$WINE_RECORD"
test -d "$default_install"
test ! -L "$default_install"

# The same fallback applies when the Steam common directory is read-only.
# Skip it where mode 0555 does not stop the current user (e.g. root on CI).
probe="$work/permission probe"
mkdir -p "$probe"
chmod 0555 "$probe"
probe_writable=no
if touch "$probe/probe" 2>/dev/null; then
    probe_writable=yes
    rm -f -- "$probe/probe"
fi
chmod 0755 "$probe"
if [[ $probe_writable == no ]]; then
    readonly_data="$work/readonly data"
    readonly_common="$readonly_data/Steam/steamapps/common"
    readonly_experimental="$readonly_common/Proton - Experimental"
    mkdir -p "$readonly_experimental"
    cp -- "$experimental/proton" "$readonly_experimental/proton"
    printf '1794497090 experimental-11.0-20261112-x86_64\n' > "$readonly_experimental/version"
    chmod 0555 "$readonly_common"
    : > "$LAUNCH_RECORD"
    XDG_DATA_HOME="$readonly_data" PROTON_RECORD="$work/proton-env" \
        bash "$launcher" "$work/game with spaces/title.exe" 2> "$work/readonly-warning"
    chmod 0755 "$readonly_common"
    grep -Fq 'continuing with Proton Experimental without the default compat symlink' "$work/readonly-warning"
    mapfile -t launched_args < "$LAUNCH_RECORD"
    test "${launched_args[0]}" = "$VICTORY_EXE"
    test "$(<"$work/proton-env")" = "$readonly_experimental"
    test ! -s "$WINE_RECORD"
else
    echo 'Skipped read-only Steam library case: this host can write to mode 0555.'
fi

# An explicitly requested Experimental runtime still fails when its compat
# path cannot be set up; no UMU invocation is attempted.
: > "$LAUNCH_RECORD"
status=0
PROTONPATH="$experimental" bash "$launcher" "$work/game with spaces/title.exe" \
    2> "$work/explicit-error" || status=$?
test "$status" -ne 0
grep -Fq 'not a symlink' "$work/explicit-error"
test ! -s "$LAUNCH_RECORD"
unset PROTONPATH

# Non-experimental and non-Steam runtimes keep the Z: path behaviour.
for runtime in stable custom; do
    proton="$work/$runtime proton"
    mkdir -p "$proton"
    case $runtime in
        stable) printf '1794497090 stable-10.0
' > "$proton/version" ;;
        custom) printf '1794497090 experimental-11.0-20261112-x86_64\n' > "$proton/version" ;;
    esac
    export PROTONPATH="$proton"
    bash "$launcher" "$work/game with spaces/title.exe"
    read -r launched < "$LAUNCH_RECORD"
    expected="$VICTORY_EXE"
    test "$launched" = "Z:${expected//\//\\}"
done

echo 'Proton launcher checks passed.'
