#!/usr/bin/env bash
# Exercise the published runtime and cache using an extracted launcher and a fake UMU.
set -euo pipefail
launcher=$(realpath -- "${1:?Pass the launcher to exercise}")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
mkdir -p "$work/tools" "$work/build with spaces"
export HOME="$work" XDG_CACHE_HOME="$work/cache" XDG_DATA_HOME="$work/data"
export VICTORY_EXE="$work/build with spaces/victory.exe"
touch "$VICTORY_EXE" "$work/title.exe"
# This fixture checks vkd3d download and staging without a Wine installation.
unset PROTONPATH
export VICTORY_ALLOW_STOCK_WINESERVER=1
export PATH="$work/tools:$PATH"
cat > "$work/tools/umu-run" <<'UMU'
#!/usr/bin/env bash
set -euo pipefail
exe=${1#Z:}
exe=${exe//\\/\/}
cd -- "${exe%/*}"
test -f victory.exe
sha256sum --check --status <<'SUMS'
6aa9637f3428b6ebfc068fccfeaa3c4620ff35604cc73005a5fe7c999d218350  d3d12.dll
20ddda82628d5ce6ffb449809425708dfc0a0a751a823cb9bccaf2bd6a1558c6  d3d12core.dll
SUMS
[[ $WINEDLLOVERRIDES == *d3d12,d3d12core=n ]]
echo 'Runtime loaded' > "$HOME/umu-called"
UMU
chmod +x "$work/tools/umu-run"

# One anonymous download on a clean host, then an offline report using the cache.
bash "$launcher" --run "$work/title.exe"
test -s "$HOME/umu-called"
cat > "$work/tools/curl" <<'CURL'
#!/usr/bin/env bash
echo called > "$HOME/curl-called"
exit 1
CURL
chmod +x "$work/tools/curl"
bash "$launcher" --report "$work/title.exe"
test ! -e "$HOME/curl-called"
reports=("$XDG_DATA_HOME"/victory/reports/*.tar.gz)
tar -xOf "${reports[0]}" ./build.txt | grep -Fxq 'runtime=pinned'

# A damaged cache and an unavailable download must fail before executing UMU.
archives=("$XDG_CACHE_HOME"/victory/vkd3d-*.tar.gz)
test "${#archives[@]}" = 1
printf 'damaged' > "${archives[0]}"
rm -- "$HOME/umu-called"
if bash "$launcher" --run --runtime pinned "$work/title.exe"; then exit 1; fi
test -f "$HOME/curl-called"
test ! -e "$HOME/umu-called"
# A successful transport carrying incorrect bytes is also rejected.
cat > "$work/tools/curl" <<'CURL'
#!/usr/bin/env bash
while (( $# )); do
    if [[ $1 == --output ]]; then echo 'incorrect download' > "$2"; exit 0; fi
    shift
done
exit 1
CURL
if bash "$launcher" --report --runtime pinned "$work/title.exe"; then exit 1; fi
test "$(<"${archives[0]}")" = damaged
test ! -e "$HOME/umu-called"
echo 'Pinned runtime checks passed.'
