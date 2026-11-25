# Contributing to VIctory

## Help by testing

If this is your first contribution, try one test and tell us what happened.
You can help without writing a patch. Browse [PRs looking for testers](https://github.com/ligmaSec/victory-royale/pulls?q=is%3Apr+is%3Aopen+label%3A%22tester+wanted%22) to find a test. A result from a second machine can
show whether a failure follows a particular build, GPU driver, or setting.

- **Testing a pull request?** Use the [interactive Series X test](docs/testing.md#run-a-series-x-hardware-test) on Windows or Linux/Proton. Pick the PR and follow the same scenario twice. Logs may contain usernames, local paths, or other sensitive information. Use the results screen to preview the summary, redact text or exclude files before sharing. The runner previews a summary you can post to the PR and can send detailed diagnostics encrypted to the verified requester named on screen using GitHub CLI authenticated with gist access. Keep the raw logs local.
- **Reporting a single Linux run?** Install UMU (`umu-run`) and extract a tested
  [Proton build](docs/testing.md#ci-and-build-downloads). From its `victory`
  directory, run `./victory-proton.sh --report "/path/to/title.exe"`.
  If you built from a checkout, use the same arguments with
  `./scripts/victory-proton.sh`.
- **Reporting a single Windows run?** Follow the [Windows build and launch instructions](README.md#getting-started).
  Keep the terminal output from the run. For a crash, check the newest entry
  in the title's `crashes.log` and confirm its timestamp matches your run.
- **Have a checkout but no game files?** On Linux, `./scripts/victory-proton.sh --test`
  exercises the launcher without Wine or a GPU. With the Rust toolchain installed,
  `cargo test --workspace --locked` runs the workspace tests. If a test fails,
  record its name and the first error.

[Search existing issues](https://github.com/ligmaSec/victory-royale/issues) for the
symptom or error. If someone has reported it, reply there with your build,
hardware, and result. A successful run on a machine where a fix needed checking
is worth posting too. For a new failure, open **one issue per distinct symptom**
using the [test report form](https://github.com/ligmaSec/victory-royale/issues/new?template=test-report.yml).

**Requesting or publishing a hardware test?** From the repository root, run
`python3 scripts/hardware-test/report-inbox.py setup` once with your own GitHub CLI
login and gist access (Windows uses `py`). Back up the private folder it prints.
If a maintainer will publish for you, start a comment on the target PR with
`/victory hardware-test` alone on its first line. Give the scenario below it and
send the maintainer the comment link. When publishing on someone’s behalf, enter
that link in the workflow’s **requester_comment** field. For your own request,
leave it empty. Check the requester shown by `--plan` before inviting testers.
The requester’s Gist notification includes the command to decrypt the reviewed
ZIP in their own `~/uploads`. For automatic pickup run
`python3 scripts/hardware-test/report-inbox.py collect --watch` on the machine with the requester key.
Testers may opt into a public encrypted receipt after sending. Their username and posting time
are visible on the requester’s key Gist. Keep this optional when recruiting testers. [Setup, rotation and report retrieval](docs/testing.md#receive-private-hardware-reports).

When you ask for testers on a pull request, add the `tester wanted` label to
the PR. If you cannot set labels, ask a maintainer in the same request. Say
which configurations still need a run and remove the label when those results
are in or the request is withdrawn. For a test that runs by PR number, publish
the [matched kit](docs/testing.md#run-a-series-x-hardware-test) from `main` and
check `python3 scripts/hardware-test/run-test.py PR_NUMBER --plan` before sharing
the command. Put any manual test instructions in the PR.

## Help by reviewing pull requests

Security reviews of [open pull requests](https://github.com/ligmaSec/victory-royale/pulls)
are especially welcome. Even a review of one changed function helps. In a review
comment, point to the relevant lines and explain what could go wrong and under
what conditions. If you have a suggested fix or regression test, share it too.

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

Give the issue a title someone else could search for, such as “Proton exits
as the loading screen opens”. Include enough detail to repeat your run:

1. Identify the build (commit or CI run, release or diagnostics) and your
   environment. For a game run, include the title version, OS, CPU, GPU and
   driver. On Linux, add the UMU and Proton versions when applicable.
2. Give the command or shortest sequence of steps, including changed settings.
   Describe what you expected, the last stage that worked, what you saw next,
   and whether the process exited, hung, or you closed it. If you already
   compared builds, give both revisions and the result with the same settings.
3. Quote the relevant error with a few surrounding lines from that run. On
   Linux, `--report` prints a local archive path. Inside it, `build.txt`
   identifies the executable, `launcher-exit.txt` records the exit status,
   and `launcher.log` and any `steam-*.log` hold the output. If the process
   stopped silently, say so and include the last stage or exit code you saw.

Keep the full archive local. Share only reviewed excerpts allowed by the
[asset policy](docs/asset-policy.md). Replace usernames and personal paths
with placeholders and remove credentials and any output from the title itself.
If you use an AI assistant to draft the report, verify its commands, versions,
and error codes against your run. Mark missing details as unknown. A single
observed failure is enough to open an issue.

For graphics failures, see [Reporting a GPU failure](README.md#reporting-a-gpu-failure).
