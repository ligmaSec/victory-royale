# Architecture

```
 user's files (outside repo)             VIctory
 ───────────────────────────             ─────────────────────────────────────────
 game assets ──────────────────────────► victory-pipeline + victory-game
                                         (victory-formats, victory-shader)
                                                    │
                                                    ▼
                                         cache_dir: converted data
                                                    │
 game executable ─► victory-loader ────► title code (native x86-64)
                    prepare, map, bind      │ imports
                                            ▼
                                         victory-hle ── host Windows (shared APIs)
                                            │ D3D12.X
                                            ▼
                                         victory-gpu ─► host D3D12
                                            │ shaders at pipeline creation
                                            ▼
                                         victory-shader ◄─► cache_dir
```

Shaders are translated when the title creates a pipeline, from the bytes it
passes, wherever it loaded them from (executable, archive, memory).
`victory translate` exercises the translator offline. The runtime manages
its own on-disk shader cache.

## Crates

| Crate | Role |
|-------|------|
| `victory-core` | config, asset-policy guard, logging and structured diagnostics |
| `victory-pe` | read-only PE32+ and import-library analysis |
| `victory-loader` | prepare, map, bind and start the title image |
| `victory-hle` | console API implementations, host pass-through policy |
| `victory-gpu` | D3D12.X objects, command lists and GPU memory → host D3D12 |
| `victory-shader` | Xbox shader containers → host shaders |
| `victory-formats` | the GPU's texture layouts (sizes, tiling, detiling), BCPack, audio codecs |
| `victory-pipeline` | converter trait, content-addressed cache |
| `victory-game` | Title-specific formats and build identities |
| `apps/victory` | command-line front end |
| `apps/victory-report-review` | offline hardware report review and redaction |
| `xtask` | asset-policy check, git hooks and diagnostic summaries |
| `tests/fixtures/hle-probe` | a stand-in "title" (our own code) for loader tests |

The runtime crates implement platform behavior. `victory-game` owns
title-specific formats and build information.

## Runtime terminology

The **title** is the console executable and its DLLs. The **host** runs that
code and provides the graphics device, through Windows or Proton. **Title
memory** is memory the title addresses directly. On the console the GPU can
read it in place. VIctory copies changed regions to host GPU resources.

A shader **refusal** records why the translator could not translate a program.
A **dropped draw** is a draw or dispatch the title submitted that the host
did not execute. These counts measure different stages of a frame.

## Import resolution

For every `dll!function` the title imports, in order:

1. our implementation in `victory-hle::modules` (always wins);
2. the host's own function, if `HostPolicy` allows that DLL;
3. an export of a DLL the title ships beside its executable, mapped like
   the executable. The loader considers DLLs absent from the HLE registry
   and disallowed by the active host policy;
4. a generated stub that names the import when called (`--unimplemented
   trap` breaks into the debugger and aborts; `zero` logs once and returns 0).

`LoadLibrary*` of a console-only DLL we implement (e.g. `xgameruntime.dll`)
returns a pseudo handle; `GetProcAddress` on it finds our functions.
`LoadLibrary*` of a title DLL returns its mapped image, mapping it first if
nothing imports it. Dynamic host DLL lookups use the default `HostPolicy`,
including during a `--strict` run. `GetProcAddress` on a host DLL returns
our override where one exists.
`QueryApiImpl` hands out our runtime objects by provider and interface id
(`victory-hle::modules::xgameruntime` lists the areas); online areas answer
as an offline console. Game saves are files under the configured data directory
(`[paths].data_dir` in `victory.toml`, or `VICTORY_DATA_DIR`).

Title storage is a drive on the console, and titles build paths on that.
`victory-hle::modules::files` binds every path-taking title import
(kernel32 and the C/C++ runtime) and rewrites paths on a virtual drive to
the host folder behind it; persistent local storage is one such drive over
a folder under that data directory. Other paths reach the host unchanged.

An exe's code reaches its static TLS as slot 0; mapped as a DLL it gets
another slot, so the loader redirects those loads (`victory-loader::tls`).
`victory run --execute` installs a crash reporter that names faults as
`module+rva` and unwinds the faulting thread's stack
(`victory-loader::crash`).

`victory census` scans import tables and counts HLE, host and missing
symbols. Title DLL exports are outside that classification. On Windows,
`victory run` maps and binds the executable and its title DLLs even when
`--execute` is omitted.

## Shaders

`victory_shader::ir::build` turns a native program into SSA, one
instruction per machine instruction. `victory_shader::structure` then
removes EXEC: lane masks become per-lane booleans, wave-wide mask tests
become per-lane branches, and vector instructions keep a guard only where
a lane can reach them with its EXEC bit clear. `victory_shader::resource`
traces each descriptor and scalar memory address to the root parameter it
comes from (table, heap, root descriptor or constant). A descriptor a
waterfall loop reads from the first lane (`v_readfirstlane_b32`) is traced
to the per-lane load or choice that gives each lane its own (a mask's
`v_cndmask_b32`, or an if/else whose arms each set it: traced on the paths
the lanes take, from structurisation), and accessed with a non-uniform heap
index in one pass. `victory translate`
runs every shader it finds (containers, SGD5 files) through these and
lists what they cannot handle yet.

## Converter cache

A `victory-pipeline` converter produces the same output for the same input.
Its cache key includes the converter ID, version, and input bytes. Bump the
converter version when the output format or conversion result changes.

## Host pipeline cache

The D3D12 pipeline library lives in an adapter-specific directory under
`adapters/` in the pipeline cache. Switching between integrated, discrete
and software adapters preserves their compiled libraries. The key uses the
DXGI software flag and the vendor, device, subsystem and revision IDs.
Adapters with the same key share a directory. If DXGI cannot identify the
device, the runtime uses `adapters/unknown/`.

On the first load for an adapter, the runtime can read the previous
top-level `host-pipelines.bin`. D3D12 checks compatibility and starts an
empty library if it rejects the file. Future saves go to the adapter
directory. The top-level `early-pipelines.txt` records which title pipelines
to prepare during loading and stays shared across adapters.
