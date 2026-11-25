# Developing VIctory

## Game data

Nothing from the game, or from any other proprietary title, enters this
repository, an issue, a comment or a pull request. That covers game files,
executable bytes, disassembly, shader dumps, captures, screenshots and logs
that contain game material. Read the [asset policy](docs/asset-policy.md)
before you share anything. Describe what you saw in words, with counts, our
own log strings and `module+rva` addresses.

## Set up

Follow the [setup instructions](README.md#getting-started). Linux host tests
require rustup and a native C toolchain. To build the Windows executable on
Linux, use the [Proton guide](docs/proton-launcher.md#build-on-linux).

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
cargo test --workspace --locked          # needs no game data
```

To run the title you also need `victory.toml` (copy `victory.example.toml`)
with every path outside the repository.

## Check a change

Follow the [code standards](docs/code-standards.md). The workspace tests use synthetic data.

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
cargo xtask policy
```

## Report a failure

For a Linux workstation run, use `./scripts/victory-proton.sh --report "/path/to/title.exe"`
from the checkout, or `./victory-proton.sh --report "/path/to/title.exe"` inside
the extracted build. The launcher prints the archive path when it exits.
Review the logs locally and share the relevant error and observations according
to the asset policy above.

Include the VIctory commit, build profile, operating system and, on Linux, Proton version, CPU, GPU and
driver. Give the command or settings needed to reproduce the problem, the
last stage that worked, and the exact error code if there is one. Replace
personal paths with placeholders. When you compare two builds, use the same
settings and give both commit IDs.

For graphics failures, see [Reporting a GPU failure](README.md#reporting-a-gpu-failure).
