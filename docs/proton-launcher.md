# Build and run VIctory on Linux

The native Linux CLI can inspect files and translate shaders. Running the
title uses the Windows executable under Proton. The launch path requires
x86-64 Linux, an AVX2 CPU, UMU, and a Vulkan driver that exposes Shader
Model 6.6 through vkd3d-proton. World rendering also needs mesh shaders.

## Build on Linux

Install Rust with rustup, a native C toolchain, and the x86-64 Linux UCRT
distribution of LLVM-MinGW. Put its `bin` directory first on `PATH`.
To embed the three GPU helper shaders, install the Linux DirectX Shader
Compiler (`dxc`) and set `VICTORY_BUILD_DXC` to its absolute path before
building. Run the following from the checkout.
[LLVM-MinGW releases](https://github.com/mstorsjo/llvm-mingw/releases) ·
[DirectX Shader Compiler releases](https://github.com/microsoft/DirectXShaderCompiler/releases)

```sh
rustup target add x86_64-pc-windows-gnullvm
export CC_x86_64_pc_windows_gnullvm=x86_64-w64-mingw32-clang
export AR_x86_64_pc_windows_gnullvm=llvm-ar
if [ -z "${VICTORY_BUILD_DXC:-}" ] && dxc_path=$(command -v dxc); then
    export VICTORY_BUILD_DXC="$dxc_path"
fi
cargo build --locked --release -p victory --target x86_64-pc-windows-gnullvm
```

The launcher finds the release executable in the Cargo target directory.
If you build without Linux DXC, leave `VICTORY_BUILD_DXC` unset. Supply
Windows `dxcompiler.dll` and `dxil.dll` beside `victory.exe` if Wine's HLSL
compiler rejects the helper shaders on startup.

An existing x64 MSVC build also works with the launcher. With `cargo-xwin`,
LLVM, and the MSVC Rust target installed, build one using
`cargo xwin build --locked --release -p victory --target x86_64-pc-windows-msvc`.

## Launch

Install UMU so that `umu-run` is on `PATH`. Keep your game files outside
the checkout. Pass the Linux path to the title executable to the launcher.
[UMU installation](https://github.com/Open-Wine-Components/umu-launcher#installation)

```sh
./scripts/victory-proton.sh "/path/to/game/title.exe" --execute
```

Omit `--execute` to check mapping and import binding first. The launcher
forwards other `victory run` options and arguments after `--`. To select a
specific build, set `VICTORY_EXE` to its Linux path. By default the launcher
looks beside itself for `victory.exe`, then under `${CARGO_TARGET_DIR:-target}`
for the gnullvm and MSVC release builds, in that order.

On first launch UMU fetches Proton and the Steam Linux Runtime. The default
prefix is `${XDG_DATA_HOME:-$HOME/.local/share}/victory/proton`, and the
default `GAMEID` is `umu-victory`. Set `WINEPREFIX`, `GAMEID` or `PROTONPATH`
to override them. Use Windows paths in `victory.toml`.

The launcher creates shader-cache directories at
`${XDG_CACHE_HOME:-$HOME/.cache}/victory/vkd3d` and
`${XDG_CACHE_HOME:-$HOME/.cache}/victory/dxvk`, keeping generated caches out
of the checkout. `VKD3D_SHADER_CACHE_PATH` and `DXVK_STATE_CACHE_PATH`
override these defaults. `VKD3D_SHADER_CACHE_PATH=0` disables the VKD3D cache
without creating a directory. Empty or relative XDG base directories fall back
to the standard directories under `$HOME`.

Set `VICTORY_TRACE_PIPELINES=1` to log the shader stages and hashes with the
Windows thread ID when each host pipeline starts compiling. Match that ID to
the faulting thread in a Proton log when several pipelines compile at once.

In a diagnostics build, the trace also logs the 32-digit host pipeline name and worker thread before
the driver call. To bypass a particular configuration that crashes the driver,
set `VICTORY_SKIP_HOST_PSOS` to its name or a comma-separated list of names.
Draws and dispatches using those configurations are dropped. Other pipelines
can still use the same shaders. Unset the variable to restore the passes.

## Testing without game files

`bash tests/proton-launcher.sh` checks argument quoting, cache paths, XDG
fallbacks and exit status with a fake UMU. It requires neither Wine nor a GPU,
and runs in Linux CI.

`./tests/proton-smoke.sh` compiles its synthetic title with the LLVM-MinGW
toolchain and gnullvm Rust target above, even when `VICTORY_EXE` points to
an MSVC build. The test checks the launcher's exit status and the return
value from the title. It uses a separate `victory/proton-smoke` prefix under
the XDG data directory. Set `VICTORY_SMOKE_PREFIX` to choose another test prefix.

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
