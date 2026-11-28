#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Sourced by the launcher and the native server builder.

proton_runner_tag=GE-Proton11-14
proton_runner_name=GE-Proton11-14-x86_64
proton_runner_sha256=26bcea3324b68dee68bf6b9b005f4425cdb9c29872e126d790df5abfebe332a8

proton_verify_file() {
    [[ -f $2 && $(sha256sum < "$2") == "$1  -" ]]
}

proton_fetch_file() (
    set -euo pipefail
    local path=$1 url=$2 digest=$3 stage
    if proton_verify_file "$digest" "$path"; then
        return
    fi
    stage=$(mktemp -- "${path}.XXXXXXXX")
    trap 'rm -f -- "$stage"' EXIT
    printf 'Downloading %s…\n' "${path##*/}" >&2
    curl --disable --fail --location --silent --show-error --proto '=https' --proto-redir '=https' \
        --connect-timeout 15 --max-time 900 --retry 2 --output "$stage" "$url"
    if ! proton_verify_file "$digest" "$stage"; then
        printf 'SHA-256 verification failed: %s\n' "${path##*/}" >&2
        exit 1
    fi
    mv -fT -- "$stage" "$path"
)

proton_has_view_index() {
    [[ -f $1 && -x $1 ]] && LC_ALL=C grep -aFq -- 'wineserver view tree:' "$1"
}

proton_managed_path() (
    set -euo pipefail
    umask 077
    local root=$1 scripts=$2 server source_id source_build key target work archive
    mkdir -p -- "$root"
    root=$(realpath -e -- "$root")
    exec 9> "$root/.install.lock"
    flock 9

    server="$scripts/wineserver"
    if [[ ! -e $server && -f $scripts/../.victory-root ]]; then
        source_id=$(cat -- "$scripts/build-proton-wineserver.sh" "$scripts/wineserver-view-tree.patch" | sha256sum)
        source_build="$root/server-${source_id%% *}"
        if [[ ! -x $source_build/wineserver ]]; then
            bash "$scripts/build-proton-wineserver.sh" "$source_build" "$root/downloads" >&2
        fi
        server="$source_build/wineserver"
    fi
    if ! proton_has_view_index "$server"; then
        echo 'The Royale wineserver is missing or invalid. Re-extract the checked Linux package.' >&2
        exit 1
    fi

    key=$(sha256sum < "$server")
    target="$root/$proton_runner_name-${key%% *}"
    if [[ -e $target || -L $target ]]; then
        if [[ ! -x $target/proton || ! -x $target/files/bin/wineserver ]] ||
           ! cmp -s -- "$server" "$target/files/bin/wineserver"; then
            printf 'Managed Proton cache is incomplete: %s\nClose Royale, remove that runner directory and retry.\n' "$target" >&2
            exit 1
        fi
    else
        mkdir -p -- "$root/downloads"
        archive="$root/downloads/$proton_runner_name.tar.gz"
        proton_fetch_file "$archive" \
            "https://github.com/GloriousEggroll/proton-ge-custom/releases/download/$proton_runner_tag/$proton_runner_name.tar.gz" \
            "$proton_runner_sha256"
        work=$(mktemp -d -- "$root/.install.XXXXXXXX")
        trap 'rm -rf -- "$work"' EXIT
        echo "Setting up $proton_runner_tag with Royale's indexed wineserver…" >&2
        (
            unset TAR_OPTIONS
            tar -xzf "$archive" --no-same-owner --no-same-permissions -C "$work"
        )
        test -x "$work/$proton_runner_name/proton"
        test -x "$work/$proton_runner_name/files/bin/wineserver"
        install -m 755 -T -- "$server" "$work/$proton_runner_name/files/bin/wineserver.royale"
        mv -fT -- "$work/$proton_runner_name/files/bin/wineserver.royale" "$work/$proton_runner_name/files/bin/wineserver"
        mv -T -- "$work/$proton_runner_name" "$target"
    fi
    printf '%s\n' "$target"
)
