# Testing

The default test run needs no game data and no console SDK:

```powershell
cargo test --workspace --locked
```

Linux CI runs the portable workspace tests. Windows CI also exercises the
Windows loader and graphics paths, and runs the tests serially to avoid
contention for the graphics driver. Both jobs run Clippy. See the
[workflow](../.github/workflows/ci.yml) for the exact commands.

The workspace suite includes the [asset-policy check](asset-policy.md).
Run `cargo xtask install-hooks` to enable it on commit.

## What the host needs

The Windows graphics tests create a Direct3D 12 device.

| Need | Used by |
|------|---------|
| A D3D12 adapter with shader model 6.6 | execution tests in `victory-shader` (`emit/host.rs`) |
| Windows SDK `dxil.dll` | DXIL validation in `victory-shader` tests; they print `no dxil.dll; skipped` without it |
| `VICTORY_WARP=1` | run the host on the WARP software adapter instead of the default GPU |

`VICTORY_WARP=1` is useful on a machine whose GPU lacks a feature, or when a
test fails on one driver and you want to know whether it is the driver. WARP
is not a real GPU, so behaviour that depends on the hardware (wave size,
driver quirks) is only settled by a run on that hardware.

Run one crate or one test on its own:

```powershell
cargo test -p victory-shader --locked
cargo test -p victory-shader --locked -- <test name> --nocapture
```

## Tests that skip or are ignored

* **Differential shader tests** (`victory-shader/src/emit/diff_tests.rs`)
  compile the same HLSL with the console compiler and the PC compiler and
  compare what each draws. They need `VICTORY_CONSOLE_DXC` (the console SDK's
  `dxc.exe`) and a PC `dxc.exe` (`VICTORY_PC_DXC`, else the newest Windows
  SDK's), and **skip without them**: a green run without those variables did
  not run them. `VICTORY_DIFF_ONLY` narrows them to matching cases.
* **`#[ignore]` tests** need reference data made outside the repository and
  run with `-- --ignored`:
  * `victory-formats` XMA: `VICTORY_XMA_STREAMS=<dir>` (`*.xma` with `*.ctx`),
    `VICTORY_XMA_REFERENCE=<dir>` (`mono`/`stereo` `.xmadata` and `.ref`).
  * `victory-formats` BCPack: `VICTORY_BCPACK_TABLES`, `VICTORY_BCPACK_CASES`.

  Never commit that data; see the asset policy.

## The loader's integration tests

`victory-loader/tests/load_probe.rs` loads a stand-in title,
`tests/fixtures/hle-probe`. It is not a workspace member: the test builds it
on demand on Windows. When running the test under Proton, cross-build the
fixture on Linux and set `VICTORY_HLE_PROBE` to its Windows path. See the
[loader test commands](proton-launcher.md#testing-without-game-files).

## Writing tests

* Use synthetic inputs built in the test. Do not add game files, shader
  dumps or extracted tables, even small ones.
* A test that needs a GPU or tool feature should print why and skip, as the
  existing ones do (`no SM 6.6 ... device; skipped`), rather than fail.
* Check hardware behaviour against the ISA and, when you can, a measurement on
  real hardware. Say which GPU and driver in the PR.
