# Build and run VIctory on Linux

The native Linux CLI can inspect files and translate shaders. Running the
title uses the Windows executable under Proton. The launch path requires
x86-64 Linux, an AVX2 CPU, UMU, and a Vulkan driver that exposes Shader
Model 6.6 and D3D12 resource binding tier 3 through vkd3d-proton. World
rendering also needs mesh shaders.

## Build on Linux

Install Rust with rustup, a native C toolchain, and the x86-64 Linux UCRT
distribution of LLVM-MinGW. Put its `bin` directory first on `PATH`.
To embed the GPU helper shaders, install the Linux DirectX Shader
Compiler (`dxc`). The build command uses `dxc` on `PATH` or the compiler
selected by `VICTORY_BUILD_DXC`. Run the following from the checkout.
[LLVM-MinGW releases](https://github.com/mstorsjo/llvm-mingw/releases) ·
[DirectX Shader Compiler releases](https://github.com/microsoft/DirectXShaderCompiler/releases)

```sh
rustup target add x86_64-pc-windows-gnullvm
./scripts/victory-proton.sh --build
```

The launcher finds the release executable in the Cargo target directory.
If you build without Linux DXC, leave `VICTORY_BUILD_DXC` unset. Supply
Windows `dxcompiler.dll` and `dxil.dll` beside `victory.exe` if Wine's HLSL
compiler rejects a helper shader. `VICTORY_DXC` can also point to the
Windows compiler DLL or its directory.

An existing x64 MSVC build also works with the launcher. With `cargo-xwin`,
LLVM, and the MSVC Rust target installed, build one using
`cargo xwin build --locked --release -p victory --target x86_64-pc-windows-msvc`.

## Package the Linux distribution

From the checkout in Bash, install the cross-build tools and Linux DXC above
and the [server build prerequisites](#patched-wineserver) below. The packager
requires Python 3, the full Git revision, and a new output directory outside
the checkout. It checks that the executable embeds the production GPU helpers
and ships the indexed wineserver with its complete prepared Wine source.
The distribution does not bundle GE-Proton itself.

```sh
(
    set -e
    revision=$(git rev-parse HEAD)
    work=$(mktemp -d)
    export CARGO_TARGET_DIR="$work/target"
    VICTORY_BUILD_COMMIT="$revision" ./scripts/victory-proton.sh --build
    bash scripts/build-proton-wineserver.sh --with-source "$work/server" "$work/downloads"
    bash scripts/package-proton.sh "$CARGO_TARGET_DIR/x86_64-pc-windows-gnullvm/release" \
        "$revision" "$work/distribution" "$work/server"
    mkdir "$work/unpacked"
    cd "$work/distribution"
    sha256sum --check --strict SHA256SUMS
    tar -xf victory-proton-x86_64.tar.gz -C "$work/unpacked"
    test "$(cat "$work/unpacked/victory/REVISION")" = "$revision"
    printf 'Distribution and extracted files: %s\n' "$work"
)
```

## Launch

Install UMU so that `umu-run` is on `PATH`. Keep your game files outside the
checkout and pass the Linux path to the title executable. Leave `PROTONPATH`
unset for [automatic Proton setup](#patched-wineserver).
[UMU installation](https://github.com/Open-Wine-Components/umu-launcher#installation)

```sh
./scripts/victory-proton.sh --run "/path/to/game/title.exe"
```

At startup, `victory.exe run` prints `Proton version:` when it recognizes the
active Proton installation, including one selected by UMU. The label is
`unavailable` if the version file in a recognized installation cannot be read
or parsed. Direct launches through Proton use the same detection. In
`--report` mode look for the line in `launcher.log`.

Pass the title path alone to check mapping and import binding first.
Invocations without a mode, including the original `TITLE.exe --execute`,
use Proton's libraries by default. The launcher
reads launcher options before the title path. Put `victory run` options such
as `--console series-s` after the path and title arguments after `--`. To select a
specific build, set `VICTORY_EXE` to its Linux path. By default the launcher
looks beside itself for `victory.exe`, then under `${CARGO_TARGET_DIR:-target}`
for the gnullvm and MSVC release builds, in that order.

On first use, UMU may need to download its Steam Linux Runtime. The default
prefix is `${XDG_DATA_HOME:-$HOME/.local/share}/victory/proton`, and the
default `GAMEID` is `umu-victory`. Set `WINEPREFIX` or `GAMEID` to override
them. Use Windows paths in `victory.toml`.

For import checks and explicit stock diagnostics, if `PROTONPATH` is unset
the launcher searches `$XDG_DATA_HOME/Steam`, `$HOME/.steam/steam` and
`$HOME/.local/share/Steam` for Proton Experimental. It uses the first install
with an executable `steamapps/common/Proton - Experimental/proton`. An explicit
`PROTONPATH` takes precedence. With no Experimental install, UMU selects the
diagnostic runner. For an explicit path, the launcher recognizes
Experimental when its `version` file contains `experimental-` and the path
uses a `steamapps/common` layout. It gives UMU Unix paths and sets `STEAM_COMPAT_LIBRARY_PATHS` to the
Steam library. Each VIctory package directory gets its own install symlink under
that library, so two builds can run at the same time. A caller-supplied
`STEAM_COMPAT_INSTALL_PATH` is used as-is: the default symlink is neither
created nor checked in that case. When the launcher discovers Experimental on
its own and cannot set up that symlink (for example a read-only Steam library),
it warns, leaves `STEAM_COMPAT_INSTALL_PATH` for UMU to choose, and still runs
that Experimental runtime through `umu-run` with Unix paths. An explicitly set
`PROTONPATH` instead reports the failure. Other layouts keep the `Z:` path
behaviour; a non-experimental `PROTONPATH` also keeps it.

The launcher creates shader-cache directories at
`${XDG_CACHE_HOME:-$HOME/.cache}/victory/vkd3d` and
`${XDG_CACHE_HOME:-$HOME/.cache}/victory/dxvk`, keeping generated caches out
of the checkout. `VKD3D_SHADER_CACHE_PATH` and `DXVK_STATE_CACHE_PATH`
override these defaults. `VKD3D_SHADER_CACHE_PATH=0` disables the VKD3D cache
without creating a directory. Empty or relative XDG base directories fall back
to the standard directories under `$HOME`.

## Patched wineserver

The first normal play (`--run`, `--report` or `TITLE.exe --execute`) downloads
and checksums the official x86-64 **GE-Proton11-14** archive (about 571 MB).
Royale installs it with the matching indexed `wineserver` under
`${XDG_CACHE_HOME:-$HOME/.cache}/victory/proton/`. Subsequent launches reuse
that private runner. The launcher does not patch a Steam install, change
system Wine, or alter the save layout. UMU also downloads Steam Linux Runtime
on first use if needed.

The Linux package ships a prebuilt server: **players do not need a compiler
or have to apply a patch**. When running from a source checkout, the first
play builds just the matching server in the cache. Install a native C/C++
toolchain with static libc, `make`, Autoconf, Flex, Bison, Perl, Python 3,
`pkg-config`, and GNU Patch/binutils before running from source
(Arch: `base-devel` and `python`; Debian/Ubuntu: `build-essential autoconf
flex bison perl python3 pkg-config patch binutils`). No full Proton build is
needed. See the [runner benchmarks](#wine-and-proton-performance) for what the
index improves and what still needs measuring.

### Custom Proton builds

Set `PROTONPATH` only to use a custom indexed Proton runner. The included
`wineserver-view-tree.patch` can be built against that release's **exact
prepared Wine source**; build only `server/wineserver` and install it as
`files/bin/wineserver` in a private copy of that same Proton release. The
client and server protocols must match. The source patch may need rebasing.
Do not apply it twice: upstream [Wine 11.23](https://github.com/wine-mirror/wine/commit/3217449b1f719c8f7a73a6de768df536495ddcbf)
already has a separate mapped-view index. That does not imply that an older
Proton release includes it.

The launcher checks `PROTONPATH/files/bin/wineserver` for the patch's
duplicate-view diagnostic. This identifies the build, not the protocol or
the process already serving a prefix. Stop prior sessions for the prefix
before switching runners: Wine can attach to an existing server. Use a
separate `WINEPREFIX` for stock comparisons, and rebuild the patched server
whenever you change the Proton version.

Import checks do not require the patch. For a stock playback diagnostic, set
`VICTORY_ALLOW_STOCK_WINESERVER=1` and select a stock `PROTONPATH` (or leave it
unset for the diagnostic auto-selection). There is no automatic fallback
from a normal play launch to stock.

## Wine and Proton performance

These are short, controlled walks on one RTX 5090 at native 3840×2160,
Series X, with the tuned renderer profile and pinned `ec20719` graphics DLLs.
The Windows package was `REVISION` `77c9125f92f621a97aa87f5c5e4360e47b244e4e`.
Loading frames are excluded. Neither the tuned profile nor its local
shader-role map is a package default.

### Direct Wine, matched Royale build

The executable, DLLs, starting scene, renderer options, and cache seed were
held fixed. Both runs used a direct loader/server pair, not UMU.

| Wine runner | Walk FPS | p95 frame time | Worst frame |
| --- | ---: | ---: | ---: |
| Wine 11.22 with indexed views | 21.24 | 60 ms | 80 ms |
| GE-Proton11-14 Wine pair | 11.21 | 220 ms | 430 ms |

The GE run had higher median worker times for write-watch/guard queries
(52.05 vs 4.7 ms/frame) and remap waits (16.65 vs 0.2 ms/frame). These worker
totals are not additive elapsed times. [Comparison details in #92](https://github.com/ligmaSec/victory-royale/issues/92#issuecomment-7239498585).
The whole Wine build changed, so this does not isolate the server patch.

### Packaged launcher through UMU

A separate run matrix used the same package, tuned 4K profile, starting scene,
cache seed, and graphics DLLs through UMU and Steam Linux Runtime 4. Each
runner used its release server, without a view-tree replacement. Successful
walks were roughly eight seconds.

| Proton runner | Walk FPS | p95 frame time | Worst frame | Result |
| --- | ---: | ---: | ---: | --- |
| GE-Proton11-14 | 10.71 | 250 ms | 280 ms | World and movement verified |
| Proton-CachyOS `cachyos-11.0-20261015-slr` | 11.40 | 200 ms | 300 ms | World and movement verified |
| Proton-Wineland `wineland-11.0-20261127` | 10.82 | 230 ms | 330 ms | World and movement verified |
| UMU-Proton 11.0-2e | — | — | — | Reached the Player prompt; load failed before control |

UMU-Proton's single load failure was not diagnosed. These runs do not establish
save compatibility, sustained play, or 30 FPS. Track the performance gap in
[#92](https://github.com/ligmaSec/victory-royale/issues/92) and other Proton
builds in [#99](https://github.com/ligmaSec/victory-royale/issues/99) and
[#100](https://github.com/ligmaSec/victory-royale/issues/100).

### Patch evidence and startup failures

- The [7900 XT fork's server-only GE-Proton11-14 comparison](https://github.com/kroeeee-eeee/VIctory-Linux-Proton-Wine-7900XT-XTX/tree/a14cfcbfce092444488112262fb222c16fe87ea9/linux/wineserver)
  reported about **8.3 → 17.7 FPS**. The [NVIDIA fork v0.1.2](https://github.com/dfmbrollins/victory-linux-nvidia/blob/v0.1.2/README.md#where-it-stands)
  reported **about 24 FPS at 1080p** on an RTX 5070 with patched Wine-staging
  11.22; that was not a server-only A/B. These are other forks' results, not
  Royale's.
- An upgrade to Wine 11.23 broke pairing with the server built for 11.22.
  A client/server protocol mismatch fails before the title runs; changing
  vkd3d-proton DLLs will not fix it. Use a matched client and server in a
  private runner rather than downgrading system Wine.
- Royale has not measured a server-only A/B under Proton. The shipped source
  patch applies to the pinned GE-Proton11-14 Wine source in a dry run. Other
  releases may need a rebase and a fresh-prefix, game-free launch check.

## Capture a workstation run

For a requested PR comparison, follow the [interactive hardware test](testing.md#run-a-hardware-test). It runs both builds and bundles their logs. To capture a single launch, use the command below.

```sh
./scripts/victory-proton.sh --report "/path/to/game/title.exe"
```

The launcher records UMU output, enables Proton exception logging and Victory
pipeline tracing, and writes the launcher exit status and executable SHA-256.
The package revision or launcher checkout revision is included when available.
`build.txt` also records the selected patched server's SHA-256, or marks it
unverified for a stock diagnostic.
Each invocation prints its own archive path under
`${XDG_DATA_HOME:-$HOME/.local/share}/victory/reports`. A failed launch also
produces an archive once UMU has started. Logs can include personal paths and
title output. Review them privately and use the contributor guide when
summarizing a failure for a public issue.

Both `--run` and `--report` default to the project’s pinned vkd3d-proton build.
To compare with the libraries supplied by Proton, add `--runtime proton`.
Use this command from a checkout, or use `./victory-proton.sh` inside a CI package.

```sh
./scripts/victory-proton.sh --report --runtime proton "/path/to/game/title.exe"
```

On first use of the pinned runtime, the launcher downloads the x64 vkd3d-proton archive
using `curl` over HTTPS. It checks the SHA-256 pinned in this launcher and keeps
the verified archive under `${XDG_CACHE_HOME:-$HOME/.cache}/victory`. Subsequent
runs can use the cached archive offline. Each run stages the two DLLs beside a
temporary copy of the selected Victory executable. The report records the runtime selection and, for pinned or custom DLLs,
their hashes. This pin is a 3.2 development build from
upstream commit `ec20719`. The release includes provenance, notices, and source.
[Runtime release](https://github.com/ligmaSec/victory-royale/releases/tag/runtime-vkd3d-ec20719)

To test a locally built runtime, use `--vkd3d-dir DIR` and
supply the directory containing its x64 `d3d12.dll` and `d3d12core.dll`.

Set `VICTORY_TRACE_PIPELINES=1` to log the shader stages and hashes with the
Windows thread ID when each host pipeline starts compiling. Match that ID to
the faulting thread in a Proton log when several pipelines compile at once.

In a diagnostics build, the trace also logs the 32-digit host pipeline name and worker thread before
the driver call. To bypass a particular configuration that crashes the driver,
set `VICTORY_SKIP_HOST_PSOS` to its name or a comma-separated list of names.
Draws and dispatches using those configurations are dropped. Other pipelines
can still use the same shaders. Unset the variable to restore the passes.

## Testing without game files

`./scripts/victory-proton.sh --test` (or `./victory-proton.sh --test` in the
package) tests argument quoting, path handling, rejected runner downloads,
offline reuse and exit status with fake UMU. It needs no Wine or GPU.

`./tests/proton-smoke.sh` compiles its synthetic title with the LLVM-MinGW
toolchain and gnullvm Rust target above, even when `VICTORY_EXE` points to
an MSVC build. Leave `PROTONPATH` unset to exercise the managed runner, or opt
into a custom runner as above. The test checks the launcher's exit status,
the reported Proton version and the title's return value. It uses a separate
`victory/proton-smoke` prefix under the XDG data directory. Set
`VICTORY_SMOKE_PREFIX` to choose another test prefix.

To reproduce the loader integration tests after the smoke test, build the
DLL fixture on Linux and set `VICTORY_HLE_PROBE` to the Windows path of the
built DLL. This lets the test load it without starting Cargo inside Proton.
Run these commands from the checkout in Bash.

```sh
target=x86_64-pc-windows-gnullvm
fixture_dir=$(realpath -m "${CARGO_TARGET_DIR:-target}/proton-fixture")
cargo build --locked --manifest-path tests/fixtures/hle-probe/Cargo.toml \
    --target "$target" --target-dir "$fixture_dir"
export VICTORY_HLE_PROBE="Z:$fixture_dir/$target/debug/hle_probe.dll"
export WINEPREFIX=${VICTORY_SMOKE_PREFIX:-${XDG_DATA_HOME:-$HOME/.local/share}/victory/proton-smoke}
PROTON_VERB=runinprefix GAMEID=umu-victory CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_RUNNER=umu-run \
    cargo test --locked --target "$target" -p victory-loader --test load_probe -- --test-threads=1
```
