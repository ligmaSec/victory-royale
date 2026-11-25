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
exit "${UMU_EXIT:-0}"
EOF
chmod +x "$work/bin/umu-run"
export PATH="$work/bin:$PATH"
export HOME="$work/home"
export XDG_DATA_HOME="$work/data" XDG_CACHE_HOME="$work/cache"
export VICTORY_EXE="$work/build with spaces/victory.exe"
export LAUNCH_RECORD="$work/args" ENV_RECORD="$work/env"
unset WINEPREFIX VKD3D_SHADER_CACHE_PATH DXVK_STATE_CACHE_PATH

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
echo 'Proton launcher checks passed.'
