# Victory Royale

Victory Royale continues development of VIctory, which runs an Xbox Series X|S
title on Windows or under Proton on Linux using files you supply.

This independent repository started from
[Project Flamingo’s November 25 snapshot](https://github.com/JMAN730/project-flamingo/commit/8cfe46d5b17a61e962b495192e866022200c5f50).

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can and on first use where it must (shaders). See
[docs/architecture.md](docs/architecture.md).

The Windows executable can also be launched under Proton on x86-64 Linux. See
[Running with the Proton launcher](docs/proton-launcher.md). Builds and checksums are
available from [successful main CI runs that package a build](docs/testing.md#ci-and-build-downloads).

## Status

Boot, setup, and menus are testable. World rendering can still have missing
geometry, corrupted lighting, long stalls, or a GPU hang. A clean launch, a
short saved scene, or a successful software-adapter test does not establish
playability. On Windows, a GPU hang may reset the display driver.

For Windows and Linux/Proton, see the [player acceptance checklist](docs/testing.md#player-acceptance)
and [interactive hardware comparison](docs/testing.md#run-a-hardware-test).
Both keep raw logs and game files local.

## Run on Linux

Install UMU (`umu-run`) and set `PROTONPATH` to a Proton build with the
[patched wineserver](docs/proton-launcher.md#patched-wineserver) for gameplay.
Extract the verified Proton package from a packaging run on `main`, then
choose one command from its `victory` directory.

```sh
./victory-proton.sh --test                                # check the launcher offline
./victory-proton.sh --run "/path/to/title.exe"             # launch with pinned vkd3d-proton
./victory-proton.sh --report "/path/to/title.exe"          # same runtime, with diagnostic logs
```

`--report` launches once and prints the path to a compressed diagnostic archive
when the process exits. Both modes download pinned vkd3d-proton on first use
and verify the cache on each launch. Add `--runtime proton` to use Proton's own libraries.
Contributors running from source use
`./scripts/victory-proton.sh --build` after installing the cross-toolchain,
then the same commands with `./scripts/victory-proton.sh`.
[Build downloads and checksums](docs/testing.md#ci-and-build-downloads) ·
[Linux requirements and runtime options](docs/proton-launcher.md)

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 and resource binding tier 3 for menus, plus mesh shaders
for the world. Building from source also requires the Rust toolchain pinned in
`rust-toolchain.toml`. On x86-64 Linux, follow the
[Proton guide](docs/proton-launcher.md) to build and launch the Windows executable.

With an [extracted CI build](docs/testing.md#ci-and-build-downloads), open
PowerShell in the folder containing `victory.exe` and run:

```powershell
.\victory.exe run 'D:\Xbox Games\Title\title.exe' --execute
```

To build from source, clone the repository and run the remaining commands from the checkout.

```sh
git clone https://github.com/ligmaSec/victory-royale.git
cd victory-royale
```

To install Rust, use rustup (`winget install --id Rustlang.Rustup -e`, or
the installer from https://rustup.rs). The first `cargo` command in this
repository then fetches the pinned toolchain. Building needs the MSVC C++
build tools: Visual Studio or its Build Tools with the "Desktop development
with C++" workload.

The title also needs several GiB of available Windows commit at startup.
Check **Committed** under **Task Manager → Performance → Memory**. If
`crashes.log` reports Windows error 1455, close other applications or increase
the Windows paging file size, then retry.

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # optional: customize paths outside the repo
cargo test --workspace --locked -- --test-threads=1 # serializes D3D12 tests; no game data
cargo run -p victory -- config           # check your paths
```

Run the title, replacing the example path with the full path to your game's executable:

```powershell
cargo build --release -p victory
.\target\release\victory.exe run 'D:\Xbox Games\Title\title.exe' --execute
```

### Selecting the Series S profile

The title chooses its render configuration from the console model, not from the
display modes VIctory advertises through DXGI. To use the lower-resolution
profile, pass `--console series-s` when starting the title:

```powershell
.\target\release\victory.exe run 'D:\Xbox Games\Title\title.exe' --execute --console series-s
```

Series S lowers the scene resolution and can reduce memory usage. The UI
uses a separate presentation plane and can still render at a higher resolution.

The window caption shows the framerate. Saves, settings and `crashes.log`
are kept under the data folder (default `%LOCALAPPDATA%\victory\data`), derived
data under the cache folder.

Input: XInput pads. A DualSense or DualSense Edge (USB or Bluetooth) is read
over HID as well and plays as the first pad when no XInput pad answers. With
no pad connected the keyboard is a pad while the title's window has focus:

| Keys | Pad |
|------|-----|
| W A S D / I J K L | left / right stick |
| arrows | D-pad |
| Enter, O | A, B |
| Backspace, X, Y | B, X, Y |
| Q / E, Z / C | shoulders, triggers |
| F / H | stick clicks |
| Esc, Tab | Menu, View |

## Commands

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <title arguments>]
victory config
```

All commands accept `--diagnostic-jsonl FILE` to write structured diagnostics
to a new file outside the checkout.

`translate --jobs N` requires a positive worker count. Omit it to use the
available parallelism (four workers if it cannot be determined). On Windows,
`--validate` loads the DXIL validator and `--pipelines` creates host D3D12
pipelines. For a private offline dump, create a directory outside the checkout
and set `VICTORY_EMIT_DUMP` to that path before running `translate --list`.

Logging: `VICTORY_LOG=victory_loader=trace,info` (a release build logs
info and above).

For structured failure reports, see [Dependency diagnostics](docs/dependency-diagnostics.md).

## Debugging

If the title presents one or two black frames and exits with `0x80000003`,
check the archive files before investigating rendering. The loader's import
report only counts bindings, so it can report zero missing imports with a
damaged archive. Check the SHA-256 of the files your run actually uses against
a known-good copy of the **same build**. For example, in PowerShell:

```powershell
Get-FileHash -Algorithm SHA256 -LiteralPath 'C:\path\to\title\anim.rpf'
```

If a hash differs, restore that file from your verified backup and rerun with
the same build of VIctory and the same options. A breakpoint can have other
causes. Include the archive verification outcome in your test report.

Release builds include crash reporting and info-level logging. To enable
debug and trace logging, frame captures, and GPU diagnostics, build with

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

Unset a diagnostic override to return to its default. Choose paths outside the
checkout for captures and dumps.

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_PROFILE=<ms>` | sample busy threads at the specified interval and log per-thread CPU time and stack samples |
| `VICTORY_TRACE_PIPELINES=1` | log shader identities and the compiling thread in release builds too. Diagnostics adds the host pipeline name |
| `VICTORY_PRESENT_LOG=1` | records each successful host present, also in release builds; does not identify player control |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_KEYS=30:Enter,35-36:X` | taps Enter for 0.2 s and holds X for 1 s, relative to input startup; diagnostics only |
| `VICTORY_CRASH_BREAKPOINTS=1` | appends first-chance breakpoints to `crashes.log`, including ones a handler may recover from |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves power-of-two presented frames plus any listed in `VICTORY_CAPTURE_FRAMES=<list>` |
| `VICTORY_CAPTURE_WORLD=1` | with `VICTORY_CAPTURE`, also saves the first frame that ran at least 500 draws and the frames two and six presents later. Check the image to identify the scene |
| `VICTORY_DUMP_TARGETS=1` | with `VICTORY_CAPTURE`, saves supported targets bound or written on explicitly listed frames and the two `VICTORY_CAPTURE_WORLD` follow-ups |
| `VICTORY_SHOW_TARGET=2560x1440-f28` | shows the last render target of that size and DXGI format each frame instead of the title's frame (copied 1:1 to the top left, only when its format matches the window's), e.g. the world's geometry while its lit image is black |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_LOG=victory_gpu::draw=debug` | on explicitly listed frames and the two world follow-ups, logs each draw or dispatch with its targets (named as in the `VICTORY_DUMP_TARGETS` files) and, if it was dropped, why |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omit draws and dispatches, including indirect ones, whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_SLOT67_HR=<name>` | what the device's `DeserializeComputePipelineStateX` (slot 67, used by the AMD upscaler with GFX10 compute programs we do not translate) returns: `E_FAIL` (the default, also without this build), `E_INVALIDARG`, `E_NOTIMPL` or `S_OK` (a pipeline whose dispatches are dropped); the title's reaction to each shows whether it has a fallback |
| `VICTORY_FSPX_REJECT_SLOT62=1` | fail `CreateComputePipelineStateX` (slot 62) with `E_FAIL` for FSPX compute modules instead of making a pipeline whose dispatches are dropped |
| `VICTORY_GPU_TIMING=1` | times each submission list separately. Enable `VICTORY_D3D12_DRED=1` to record command detail for slow lists |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_ADAPTER=<name>` | select a D3D12 adapter by case-insensitive substring (default: the first GPU, in high-performance order, with shader model 6.6, resource binding tier 3 and mesh shaders; the log says what is missing when none has all three) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); lowering the limit changes shader execution, so check the rendered result alongside frame times |
| `VICTORY_ROOT_SRV_DIRECT=1` | use direct root shader-resource views in place of bounded views. Unset to restore the bounds (even `0` selects direct views) |
| `VICTORY_SHADER_OPT=<bits>` | which clean-up passes run over each translated shader (own shader cache entries): `0` none, 1 dead code, 2 constants and identities, 4 reuse of values, 8 branch facts, 16 loads within a block; unset runs all. If a failure changes with `0`, vary the pass bits to isolate a reproducer |
| `VICTORY_ALIAS_COPY=0` | views the title places in the memory of a texture the host writes are filled from title memory again instead of copied from that texture on the host |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |
| `VICTORY_OUTPUT=3840x2160` | request 4K output on Series X with the supported title build; verify the actual swap-chain size in the log |
| `VICTORY_RENDER_SCALE=1.0` | request native internal resolution on that Series X build, enabling the required 4K render budgets |
| `VICTORY_FRAME_INTERVAL_MS=16.667` | set the host frame clock and the supported title build's frame-rate setting; this is a cap, not measured FPS |
| `VICTORY_SHADER_ROLES=<file>` | optional private shader-role registry for the donor's rendering controls; see [matched comparisons](docs/testing.md#comparing-frame-pacing) |
| `VICTORY_SKIP_CPU_CHECK=1` | skip the AVX2 pre-flight check before `--execute` (for CPU emulators such as Intel SDE) |
| `VICTORY_PAD_LOG=1` | log changes in DualSense HID readings. Unset to disable this extra output (even `0` enables it) |
| `VICTORY_COPY_MODE=plain` | use plain copies for timing experiments. The default guards against unmapped pages |
| `VICTORY_WARP=1` | force the shader-test host device onto WARP (distinct from `VICTORY_D3D12_WARP`) |
| `VICTORY_SAVE_STOP=<n>` | test harness only: stop the crash-survival test child at save step `n` |

Every build appends unhandled exceptions, panics, and host GPU failures to
`crashes.log` in the title's data folder.

For a startup breakpoint (`0x80000003`), run the diagnostics build with
`VICTORY_CRASH_BREAKPOINTS=1`. Keep the console output along with the new
`crashes.log` entries. A host DLL refusal includes the caller stack in the
console output. The refusal is expected for DLLs outside the host policy,
including `rpcrt4.dll`.

On Series 1.0.2.1, a missing `appdata0.rpf` reproduced the breakpoint at
`+0x2237178` before the first window. Restore the archive from the same
package beside the title executable. `run --execute` checks that it can
read the RPF9 header before starting this build.

To compare hardware and software rendering, repeat with
`VICTORY_D3D12_WARP=1` and check the console for
`host device on the software adapter (WARP)` to confirm the WARP device was
created. Compare the exception's module and offset, stack frames, and
preceding messages across the two runs.

### Reporting a GPU failure

A host device removal, a hang, or a frame that comes out wrong is much easier
to place with these next to it:

* the GPU selected in the host log, driver version and OS build. For Linux
  include the Vulkan driver and Proton build.
* on Windows, whether the same failure occurs with the diagnostics build on
  WARP. Include the last stage reached on each adapter. A hardware-only
  failure can also come from a host-specific path in VIctory.
* the last lines of `crashes.log`, which name the faulting code and the
  commands in flight when `VICTORY_D3D12_DRED=1` was set;
* the variables from the table above that were set for the run, and the build
  profile (`diag` or release);
* how long it took, and whether the display reset (a TDR) or the process
  stopped.

Nothing from the title's data belongs in the report: no shader dumps, no
frames, no memory, per [docs/asset-policy.md](docs/asset-policy.md). Names of
our own functions, error codes and timings are enough to start.

## Contributing

A first contribution can be a test result from your machine. The
[testing and reporting guide](CONTRIBUTING.md#help-by-testing) covers how to
pick a test, confirm an existing issue, or report a new failure.

The shader, GPU memory and submission, and loading runtime builds on
[DoubleGhosty/cool_fox and its contributors](https://github.com/DoubleGhosty/cool_fox/tree/7cc053173e6bfc99ce9beb8a3fff3033fa467dde).
VIctory Royale retains its own distribution, diagnostics, and regression gate.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
