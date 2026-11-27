# Code standards

CI runs the checks listed below. Review covers the remaining standards.

## Project language

Write documentation, code comments, diagnostics, and contributor guidance in English.
Keep setup instructions in the root `README.md` and link to it from other guides.
Use each test directory’s README for instructions specific to that suite.

## Checked by CI

| Check | Command | Where it runs |
| --- | --- | --- |
| Documentation layout | `bash tests/docs.sh` | Linux |
| Formatting | `cargo fmt --all -- --check` | Linux |
| Asset policy | `cargo xtask policy` | Linux, and again as a test |
| Lints | `cargo clippy --workspace --all-targets --locked -- -D warnings` | Windows and Linux |
| Lints, diagnostics build | the same with `--all-features` | Windows |
| Tests | `cargo test --workspace --locked` | Linux |
| Tests, serialized for the shared D3D12 driver state | `cargo test --workspace --locked -- --test-threads=1` | Windows |

Documentation-only changes run the documentation layout and asset-policy
checks. See the [CI workflow](../.github/workflows/ci.yml) for the minimum
Rust version check and the synthetic hardware, WARP and Proton checks.

The toolchain is the one pinned in `rust-toolchain.toml`. Formatting follows
`rustfmt.toml` (2024 edition, Unix newlines, field-init and `?` shorthand)
at rustfmt's default width of 100.

A warning is a failure. Do not silence one with `#[allow]` unless the lint is
wrong for that code, and then say why on the line above. The existing
module-level allows are for FFI modules that mirror console names and take
raw pointers.

## Lints

Workspace lints are set in the root `Cargo.toml` and every crate opts in with
`[lints] workspace = true`:

- `unsafe_op_in_unsafe_fn` and `unused_must_use` are errors;
- clippy's `all` group and `undocumented_unsafe_blocks` are warnings, which
  CI turns into errors.

A new crate opts in the same way.

## Unsafe code

- Every `unsafe` block has a `// SAFETY:` comment directly above it that says
  why the operation is sound here, not what it does.
- Every `unsafe fn` has a `# Safety` section saying what the caller must
  guarantee.
- Keep the block as small as the operation that needs it.
- Where soundness rests on what the title does (it calls methods only on
  objects we handed out, it passes a buffer of the size it states), the
  comment names that contract.

## Known approximations: `// shortcut:`

The tree has no TODO or FIXME comments. Where the code does less than the
console does, it says so with a comment that starts `// shortcut:` (or
`//! shortcut:` for a whole module), stating what is left out and what would
make it matter:

```rust
// shortcut: a root view has no size; constant buffers are taken as 64 KiB,
// other views as 1 MiB (both cut at their buffer's end).
```

Searching for `shortcut:` therefore lists every known gap. Remove the comment
in the change that closes the gap.

## Saying no

- A runtime method or device method we do not implement fails and says so:
  it logs a warning with the area and the slot, and returns the console's
  not-implemented code. Do not answer success for work that was not done; a
  title that believes a false success fails later and further away.
- The shader translator has one refusal, `ShaderError::Unsupported`, carrying
  the offset and what it could not handle, in our own words (`"a lane mask
  read as data"`). Reuse an existing reason when it is the same cause; testers and
  the scoreboard group by that string.
- A draw or dispatch the GPU layer cannot run is dropped with a message
  logged once for that reason, never silently.

## Errors

- Library crates whose callers match on the error define it with
  `thiserror` (`victory-core`, `victory-loader`, `victory-pe`,
  `victory-shader`). `victory-formats` writes its `Display` and `Error`
  impls by hand and stays dependency-light.
- Application paths that only report the error use `anyhow` with context
  (`apps/victory`, `victory-pipeline`, `xtask`, the outer layers of the
  loader and GPU crates).
- Functions the title calls return what the console returns, usually an
  `HRESULT`. Constants are defined in the module that uses them, named as
  the console names them.

## Logging

- Use `tracing` macros fully qualified (`tracing::warn!`), with structured
  fields rather than values formatted into the message:
  `tracing::warn!(dll = %site.dll, symbol = %site.symbol, "unimplemented
  import called, returning 0")`.
- `debug!` and `trace!` output requires a build with the `diagnostics` feature.
  Anything a tester on a release build must see is `info!` or above.
- Anything that can fire per draw or per frame logs once. The GPU crate has
  a `once!` macro for this.
- Messages are in our own words and are stable: people quote them in
  reports and tickets. Never log bytes of title data.

## Tests

- Unit tests sit in a `#[cfg(test)] mod tests` at the foot of the file;
  cross-module tests go in the crate's `tests/` directory.
- A test name states the behaviour:
  `blocking_wait_returns_at_once_when_reached`,
  `the_game_is_licensed_and_the_store_is_offline`.
- Test data is synthetic and built by code in the test. No file from a game
  is ever a fixture, and tests assert on properties, never on snapshots of
  real data.
- A test that needs real files is `#[ignore = "needs …"]` and reads its
  paths from environment variables, so `cargo test --workspace` needs no
  game data.
- Benchmarks are `#[ignore = "benchmark"]`.
- A test that needs a GPU returns early when no suitable device exists.
  How such tests should report is an open decision on the map; until it is
  settled, say in the test which capability it needs.
- A fix adds a test that fails without the fix.

## Comments and module docs

- Every file opens with a `//!` paragraph that explains the console
  behaviour being reproduced and how the file reproduces it.
- Comments say why. The code already says what.
- Keep the module docs true: if a change makes a sentence in them wrong, fix
  the sentence in the same change.

## Where code goes

- Keep platform code reusable across Xbox Series titles. Code that knows
  about one game (formats, build identities, per-build offsets) belongs in
  `victory-game`.
- Code that writes derived data calls `victory_core::ensure_outside_repo`
  first.
- A `Converter` in `victory-pipeline` is deterministic bytes in, bytes out.
  Bump its version whenever its output changes.
- Compile development-only debugging aids (environment switches, captures,
  extra validation) behind the `diagnostics` feature, and list every switch
  in the README's table.

## Portability and runtime boundaries

- Query host capabilities and limits at runtime (display modes, VRAM/RAM
  budgets, wave sizes, format support, descriptor limits) or provide safe
  fallbacks. Do not hardcode limits from one machine.
- Route hardware-dependent behaviour (wave32 vs. wave64, shader model tiers,
  optional UAV/texture format support, CPU ISA extensions, vendor quirks)
  through explicit capability checks and test each supported tier rather than
  assuming a single reference machine.
- When a buffer, heap, or budget overflows, grow or virtualize dynamically or
  fail/drop loudly with structured diagnostics (see *Saying no* above).
- When emitting platform artifacts, pair synthetic unit tests with external
  validators where applicable (DXIL validation, the D3D12 debug layer, WARP,
  and Wine/Proton smoke runs).
- New tests should isolate their state so they do not depend on running
  serially (`--test-threads=1`), even though Windows CI currently runs the
  workspace suite with `--test-threads=1` for shared GPU driver state.
- Bound host caches and resource lifetimes: do not store host GPU or memory
  allocations in unbounded static caches that only insert; tie every
  allocation to an explicit owner, eviction budget, or title-unmap/release
  hook.
- Changes touching CPU, memory, OS, or GPU boundaries should document which hardware
  and operating systems were tested.

## Commits

- The subject names the area, then states the behaviour as it now is:
  `GPU: no render target views of textures made without render-target use`,
  `HLE input: a blocking enumeration waits for callbacks another thread
  delivers`. Areas in use: `GPU`, `Shaders`, `HLE`, `Loader`, `PE`,
  `Formats`, `Pipeline`, `CLI`, `Docs`, `CI`, `xtask`, with an optional
  sub-area (`HLE input`, `GPU diagnostics`).
- The body says why, and how the change was tested.
- One behaviour change per commit where that is practical. Formatting-only
  changes go in their own commit.
