# Testing

The default test run needs no game data and no console SDK:

```powershell
cargo test --workspace --locked -- --test-threads=1
```

Linux CI runs the portable workspace tests. Windows CI also exercises the
Windows loader and graphics paths, and runs the tests serially to avoid
contention for the graphics driver. Both jobs run Clippy. See the
[workflow](../.github/workflows/ci.yml) for the exact commands.

The workspace suite includes the [asset-policy check](asset-policy.md).
Run `cargo xtask install-hooks` to enable it on commit.

## CI and build downloads

Every pull request and push to `main` runs the documentation and asset checks.
The docs-only route covers the root README, contributor and agent guides, Markdown
under `docs/`, and the Metal and runtime test READMEs. Other changes also run the
Windows and Linux workspace suites, workflow and shell lint, formatting, the
minimum-Rust check, and the distribution test.
Manual runs and merge-group checks use the full suite.

A push merging a code PR can reuse its successful minimum-Rust job when the
merged Git tree matches the one the job tested. Direct code pushes and merges
without matching test evidence rerun it. The regression gate verifies the
expected jobs for each path. A docs-only merge does not publish a binary.

The distribution job builds the x86-64 Windows release on Linux and checks the
embedded DXIL for `argument_clamp`, `rect_expand` and `rect_rewrite`. It unpacks the
archive, exercises the offline launcher checks and pinned runtime selection with
a fake UMU, and runs the packaged executable under Wine against
[synthetic PE fixtures](../tests/runtime/README.md). The suite checks import
binding, TLS, threads, memory, DLL loading, and expected title return codes.
If an archive test fails, CI retains any synthetic case logs and summary it
produced for three days. Game rendering and
the full UMU/Proton runtime still require a separate workstation test.

Successful main builds deliver `victory-proton-x86_64-<commit>` for 14 days.
Code PRs deliver `victory-proton-x86_64-pr-<number>-<merge-commit>` for three
days. The latter contains the revision GitHub tested with the base branch.
For a requested comparison, use the [interactive hardware test](#run-a-hardware-test)
to get both matched builds. For a single run, Windows testers can launch the extracted
`victory.exe` with the [usual Windows launch command](../README.md#getting-started).

Download the artifact from the CI run and unzip the Actions download. On Linux,
run these commands in the extracted directory.

```sh
sha256sum --check SHA256SUMS
tar -xf victory-proton-x86_64.tar.gz
./victory/victory-proton.sh --test
./victory/victory-proton.sh --run "/path/to/game/title.exe"
```

On Windows, open PowerShell in the extracted directory. Verify the inner
archive before unpacking it, then run the executable in the `victory` folder.

```powershell
$expected = ((Get-Content -LiteralPath SHA256SUMS -Raw) -split '\s+')[0]
$archive = 'victory-proton-x86_64.tar.gz'
if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $expected) { throw 'SHA-256 mismatch' }
tar.exe -xf $archive
.\victory\victory.exe run 'D:\Xbox Games\Title\title.exe' --execute
```

<a id="run-a-series-x-hardware-test"></a>

## Run a hardware test

You'll need Python 3.9 or newer and your own title executable outside the VIctory checkout.
> [!WARNING]
> **Logs may contain sensitive information**, including usernames, local file paths,
> credentials and output from the game. Review the filtered report before sharing.
> Keeping all results local is always an option.

On Linux, install Bash and UMU (`umu-run`). The Proton launcher may download its pinned
runtime on first use.

| Start here | Linux/Proton | Windows (PowerShell) |
| --- | --- | --- |
| From the repository root, choose a test request | `python3 scripts/hardware-test/run-test.py` | `py scripts/hardware-test/run-test.py` |
| With a kit ZIP, extract it and open a terminal in the folder containing `run-test.py` | `python3 run-test.py` | `py run-test.py` |

Gameplay comparisons start from the standard `7952987A` save. Place your local
copy in `saves/7952987A/` at the checkout root, or beside an extracted runner
in the same layout. It contains `saves/` with your progress and may also contain
`storage/` with settings. To keep the checkout empty, pass
`--save-dir "/path/to/7952987A"`. Each build starts with its own temporary copy
of the same progress and settings. Launching requires a folder named `7952987A`
with at least one file under its `saves/` directory. You can preview a request
with `--plan` even before the save is available.

The checkout menu uses a signed-in GitHub CLI (`gh auth login`). Type the PR number
shown in the menu, or press Enter to cancel. If a request includes a number, you
can append it directly to the checkout command. Add `--plan` to preview the scenario
and build revisions. An extracted kit includes both builds and needs no GitHub login.

1. Select your game's `.exe` when prompted, or pass its full path with
   `--title` on the runner command line. The runner checks both packages and starts
   **main** first. Follow **WHAT TO DO** above the launch and close the game at the point
   specified in the scenario.
2. The **candidate** opens next. Repeat the same steps with the same settings and window
   size. The runner gives each build its own fresh translation cache.
3. After the runs, the **Hardware test results** screen shows each build’s timings.
   Missing samples are shown as unavailable. These timings include menus. Choose **Describe what you saw** to open a local
   browser form. Describe each build separately. Click its screenshot box and paste
   with **Ctrl+V**, or choose an image. Inspect the preview for names and overlays.
   You can replace or remove either image. **Save locally** returns you to the terminal
   to choose what to share. Closing the tab leaves edits unsaved. Use Ctrl+C in the
   terminal to leave an abandoned editor. Use **↑/↓** to highlight an action,
   **Enter** to select and **Esc** to go back. **PgUp/PgDn** scroll longer details.
4. Choose **Preview summary** to read the public text. **Review and edit files**
   opens the built-in editor when supplied. Use **↑/↓** to pick a file, **Space**
   to include or exclude it, **Enter** to edit, **Ctrl+F** to search and **Esc** to
   return to the file list. **Ctrl+S** applies changes locally. To use another
   editor, choose **Edit in my own editor → Open folder**. Edit or delete
   working copies, save, then select **Apply edits**. **Back without applying**
   discards those edits. The folder option also works with older kits.
5. Choose **Keep local and finish** to print the saved ZIP path. **Post public
   summary** previews the exact text and names the signed-in account and PR.
   Public posts include the reviewed observations alongside timings. Screenshots go
   only in the filtered ZIP. After posting or sending, the results screen shows each
   destination’s status. You can choose the other destination before finishing.
   Older extracted kits retain their original interface.
   For private sharing, the filtered ZIP also contains per-window mean, median,
   p95, longest frame time and stall counts for each build. Separate submission
   samples record time spent on CPU preparation, buffer and texture copies,
   GPU waits and copy-back, alongside draw and pipeline-drop counts. The private
   event timeline connects shader binds, translations and pipeline results to
   those samples through elapsed times and shader numbers shared across the pair.
   You can inspect the filtered ZIP in the printed temporary `share` folder.
6. For **Send encrypted diagnostics to @requester**, install GitHub CLI and sign in to github.com
   with permission to create gists (`gh auth login --hostname github.com --scopes gist`).
   For an existing CLI login, `gh auth refresh --hostname github.com --scopes gist`
   adds the scope. The confirmation screen lists the account, recipient and every outgoing file.
   Choose **Encrypt and send** to complete delivery. The runner downloads the pinned,
   checksum-verified x86-64 age tool. [Official age release](https://github.com/FiloSottile/age/releases/tag/v1.3.2).
   GitHub stores the encrypted file in an unlisted gist owned by your account.
   A mention on the gist notifies the requester. Your account, gist size and
   upload time are visible to GitHub. Anyone with the URL can retrieve the
   ciphertext. The named requester's private age key is needed to read its contents.
   After delivery, **Finish privately** is selected. If you choose **Publish encrypted receipt**,
   the runner posts an encrypted copy of the report link and checksum on the requester's
   public key-registration Gist. Anyone can see your GitHub username, the requester and the
   posting time. The comment contains no readable report link, file names or logs.
   This lets the requester collect the ZIP automatically.
   Keep the receipt if notification fails. You can delete the gist from your account.

**Raw logs may contain usernames, file paths, credentials or output from the title.**
Keep `report.zip` local. When vkd3d-proton rejects a shader wave size, the
summary counts driver messages by required and supported lane ranges. Repeated
creation attempts can count more than once. The initial export uses a fixed set of recognized
fields. Unknown messages, command lines, hostnames, wall-clock timestamps, screenshots and
the request's free text are excluded. If the summary misses something, describe
it in words. See the [asset policy](asset-policy.md).

### Compare extracted builds locally

Use `--local` to run already extracted builds without selecting a hardware
request, opening the report editor, or uploading results. Use verified packages
with `victory.exe`, `REVISION` and, on Linux, `victory-proton.sh` in each `--build`
directory. Run from the checkout with Python 3.9 or later. On Linux the packaged
launcher needs Bash and UMU as above. Keep the title, report destination and
system temporary directory outside the checkout; put the results and temporary
files outside the title, build and source-save directories as well.
If `--output-dir` is omitted, the runner uses the system temporary directory.

Start by checking a clean first launch. On Linux:

```sh
python3 scripts/hardware-test/run-test.py --local \
  --build candidate "/path/to/candidate/victory" \
  --title "/path/to/game/title.exe" --clean-data \
  --output-dir "/path/to/private/results"
```

On Windows (PowerShell):

```powershell
py scripts/hardware-test/run-test.py --local --build candidate 'D:\Builds\candidate\victory' `
  --title 'D:\Games\Title\title.exe' --clean-data --output-dir 'D:\Private\VIctory results'
```

For a copied-save comparison, explicitly select `--save-dir` instead of
`--clean-data`. Repeat `--build NAME DIR` in the order you want the builds run.
The following scripted example assumes both extracted builds were compiled
with the `diagnostics` feature; omit `--keys` for normal release builds.

```sh
python3 scripts/hardware-test/run-test.py --local \
  --build main "/path/to/main/victory" --build candidate "/path/to/candidate/victory" \
  --title "/path/to/game/title.exe" --save-dir "/path/to/7952987A" \
  --warm --console series-x --keys "10:Enter,15:X" \
  --output-dir "/path/to/private/results"
```

The same flags work with `py` and Windows paths. Each build's cold run starts
with empty VIctory caches and, on Linux, separate translation and driver cache
directories. `--warm` immediately repeats the run with the same cache directories;
it does not reset system-wide caches. Each cold or warm case starts with its own
fresh user data (empty or copied from the same save snapshot), and its own
temporary working directory and Proton prefix. `--console` defaults to `series-x`
and also accepts `series-s`. Windows runs the packaged EXE directly. On Linux the
runner uses the packaged `--run` command with the pinned vkd3d-proton runtime.
If the verified pin is not cached, the launcher may download it; use
`--runtime proton` to use Proton's bundled vkd3d-proton instead. Follow the same
runtime choice for all runs you intend to compare.

`--keys` passes an optional `VICTORY_KEYS` schedule unchanged to each build.
It requires a diagnostics-enabled build. The point syntax above holds each key
for 0.2 seconds at the specified time after input starts, not after process
launch. Use range syntax such as `10-10.5:Enter` only with builds that support
it. With no `--keys`, the runner does not inherit an ambient `VICTORY_KEYS`.
Close each game normally when your scenario is finished; the runner does not
enforce a timeout. Verify gameplay and that a new save resumes manually, using
a separate persistent test profile for the create-and-resume check: local
comparison profiles are discarded and are not suitable for testing persistence
across runs.

The printed `victory-local-*` results folder contains labeled `cold` and
optional `warm` case directories. Each has `case.json` (build name, package
revision, SHA-256 of `victory.exe`, console, key schedule, starting data and
cache class and graphics runtime), raw `console.log`, any Proton Steam logs,
`console-exit.txt`, and timing/diagnostic summaries and CSVs. Timings can include menus;
neither a boot log nor a zero exit proves gameplay or a successful save. The
runner finishes normal exits serially and returns the first nonzero status, if
any. It does not archive saves or upload the results. Keep this folder private:
its unfiltered logs can contain sensitive information.

## Receive private hardware reports

From the repository root, with GitHub CLI signed in to github.com and authorized
for gists, run the following once. On Windows, use `py` in place of `python3`.

```sh
python3 scripts/hardware-test/report-inbox.py setup
```

The command generates an age identity locally. It publishes only the public key
in a public Gist under your account and verifies the Gist owner’s numeric GitHub
ID and login. Back up the private folder it prints in encrypted storage. Running
setup again checks the same key. On another computer, restore that folder first.
If GitHub CLI lacks gist access, run `gh auth refresh --hostname github.com --scopes gist`.

To receive a submission, open the Gist mention in your GitHub notifications.
Run the command in the mention from the repository root. It uses your signed-in
account’s local identities and prints the decrypted ZIP path under `~/uploads`.
You can also copy the Gist link into this command. Open the reviewed files locally.

```sh
python3 scripts/hardware-test/report-inbox.py receive GIST_URL
```

To collect opted-in reports automatically, run this on the machine with your requester keys.

```sh
python3 scripts/hardware-test/report-inbox.py collect --watch
```

Leave it running. It checks your registration every five minutes, writes complete reviewed ZIPs
to `~/uploads` and prints their paths. Ctrl+C stops the watcher. Omit `--watch` for one pass,
or use `--output-dir` to select a folder. It picks up receipts posted while the watcher
was stopped. Repeated scans preserve existing reports. Files stay zipped until you open them.
Automatic collection accepts encrypted uploads up to 16 MiB and reserves up to 512 MiB for
received reports in the output folder. Move older ZIPs elsewhere if it fills up. Use the
private `receive` command for larger files, older kits, or testers who finish privately.

Run `python3 scripts/hardware-test/report-inbox.py rotate` to publish a fresh key.
Back up the folder again. Old identities stay there and can decrypt reports
already sent. Losing the old private key loses access to those reports. Rotating
or removing the registration disables further private sends from outstanding
kits bound to it. Republish those kits after rotation or first-time registration.
An account rename also requires republication.

**Older kits.** Open an old request with the updated checkout to keep its report
local or post a reviewed public summary. A previously extracted runner still
uses the delivery rules it shipped with, which may include the project key.
Reports already sent to the project key still require its original private key.
Choose **Keep local** in an older runner. Use a newly published official kit for delivery to
the requester. Local packager output and loose request files in a checkout support
local review and public summaries. The current runner verifies an official ZIP
with GitHub when opened directly. For an extracted official kit it checks the
request against the copy pinned in its bundled runner.

## Requesting a hardware comparison

Run the [hardware kit workflow](../.github/workflows/hardware-request.yml) on
`main` with the PR number, console profile (Series X by default), and a short scenario.
Both builds use that profile. Specify where to start,
what to observe and when to close each run. If someone else asked for the test,
have them comment on this PR with `/victory hardware-test` on its own first line
followed by their scenario. Put that comment URL in **requester_comment** when
dispatching. For your own request leave the field blank. The workflow then uses
the GitHub dispatch actor. The PR author can be a third person. Publication
records the requester and their verified key at that time. If they have no key,
the results screen offers local review and public summary sharing. Check
`run-test.py PR_NUMBER --plan` for the intended account before inviting testers.
After publication, [Verify the published hardware request](../.github/workflows/hardware-request-verify.yml)
previews the PR by number using the same picker as testers. A successful preview
adds `tester wanted` to the PR. If verification fails, fix the reported problem
and republish the kit before inviting testers. Once verified, send the tester the
checkout command for their OS with the PR number appended. Check the account
shown in the plan against the person who asked for the test. Tell testers that logs can contain usernames, local paths and other
sensitive information, and that they can reply with a written summary. For manual tests, add the
`tester wanted` label to the PR while the request is open.
The picker lists open PRs labeled `tester wanted` and open PRs with a published kit.
A kit published by the workflow on `main` can run directly. For a labeled PR without
one, the picker links to the request, including any separate kit or manual instructions.
If the PR head changes or the kit expires, publish a fresh kit for the current revision.

For testers without a checkout, share the kit ZIP. To prepare one locally with a
signed-in GitHub CLI, run this from a trusted checkout, replacing `PR_NUMBER` with
the number of the PR.

```sh
python3 scripts/hardware-test/prepare.py PR_NUMBER --output test-kit.zip \
  --scenario "Wait ten seconds at the menu. Enter the opening scene. Close each run at the first subtitle."
```

Official kits bundle the native review editor for x86-64 Windows and Linux.
When preparing a kit locally, pass `--review-dir` with both built executables
to include the editor. Kits prepared without them use the manual folder review.

The packager matches the successful PR build to its tested main parent and checks
both artifact digests. A documentation-only push can pass CI without packaging a
build. If the tested parent is the current head of `main`, run
`gh workflow run ci.yml --ref main`. Wait for CI to finish, then prepare the kit
again. If the tested parent is older and its artifact is unavailable, update the
PR against a main revision with a package and wait for PR CI to test the new pair.
Post the tested revisions, configuration and observed differences on the PR. For
performance comparisons, include frame times and stalls alongside average or median FPS.

## What the host needs

The Windows graphics tests create a Direct3D 12 device.

| Need | Used by |
|------|---------|
| A D3D12 adapter with shader model 6.6 | execution tests in `victory-shader` (`emit/host.rs`) |
| `dxil.dll` from the Windows SDK or at the path set by `VICTORY_DXIL` | DXIL validation in `victory-shader` tests |
| `VICTORY_WARP=1` | select the WARP software adapter in the shader tests |
| `VICTORY_D3D12_WARP=1` with the `diagnostics` feature | select WARP for the `victory-gpu` host, as CI does for the copy-back and world capture checks |

`VICTORY_WARP=1` is useful on a machine whose GPU lacks a feature, or when a
comparing a GPU test failure with the software adapter. WARP
is not a real GPU, so behaviour that depends on the hardware (wave size,
driver quirks) is only settled by a run on that hardware.

Run one crate or one test on its own:

```powershell
cargo test -p victory-shader --locked
cargo test -p victory-shader --locked TEST_FILTER -- --nocapture
```

Replace `TEST_FILTER` with part of the test name. For synthetic shader readback
on Apple Silicon, see the [standalone Metal suite](../tests/metal/README.md).

### Running the title under Wine without a sound device

On a headless Linux host, an X display can be working while Wine has no
default playback endpoint. In that configuration the Series X 1.0.2.1
opening repeatedly stopped at guest frame 82. Check the title's local
crash report for its frame number. The host can present hundreds of
frames before the title stops.

For Wine using ALSA, a silent playback device lets a graphics test run
past this point. In a shell with your test Wine prefix and graphics
runtime already configured, give just the launch its own ALSA config:

```sh
audio_config=$(mktemp)
cat > "$audio_config" <<'EOF'
</usr/share/alsa/alsa.conf>
pcm.!default {
    type null
    hint { show on description "VIctory software playback" }
}
EOF
ALSA_CONFIG_PATH="$audio_config" wine ./victory.exe run "$TITLE_EXE" --execute --console series-x
rm -f "$audio_config"
```

Set `TITLE_EXE` to the Windows path to your executable. Use a fresh Wine
session for each side so its audio driver picks up the setting. Record
`ALSA null` with the GPU, runtime and scene cue for both builds. This
setup exercises silent playback. Audible output needs a separate test
with a working sound device.

## Comparing frame pacing

Keep the build settings, driver, window size and launch sequence the same. Compare
the menu and first scene after loading separately. Release logs include a
`present intervals` line for every five seconds of completed frame intervals
(or 4,096 intervals at high rates). Each line gives the sample count, mean,
median, nearest-rank p95, longest interval and counts over 100 ms and one second.
These are wall-clock times between successful host presents. Startup before
the first present and gaps while DXGI reports an occluded window are excluded.
A stall appears in the log when presentation resumes.

## Tests that skip or are ignored

* **Differential shader tests** (`victory-shader/src/emit/diff_tests.rs`)
  compile the same HLSL with the console compiler and the PC compiler and
  compare draw and dispatch output. They need `VICTORY_CONSOLE_DXC` (the console SDK's
  `dxc.exe`) and a PC `dxc.exe`. The PC compiler comes from `VICTORY_PC_DXC`,
  the newest Windows SDK that contains it, or the directory containing
  `VICTORY_DXIL`, in that order. Tests that compare compiler output skip when
  the compilers are unavailable. Set `VICTORY_DIFF_ONLY` to the exact case
  name to run that case.
* **Ignored format tests** use inputs made outside the repository. Set the
  variables for the test you want, then run its command. Keep the inputs
  outside Git under the [asset policy](asset-policy.md).

  | Test filter for `cargo test -p victory-formats --locked TEST_FILTER -- --ignored` | Environment and inputs |
  | --- | --- |
  | `walks_saved_streams` | `VICTORY_XMA_STREAMS` points to a directory with `*.xma` and matching `*.ctx` files |
  | `matches_a_reference_decoder` | `VICTORY_XMA_REFERENCE` points to a directory with `mono.xmadata`, `mono.ref`, `stereo.xmadata` and `stereo.ref` |
  | `reference_chunks` | `VICTORY_BCPACK_TABLES` points to the table file and `VICTORY_BCPACK_CASES` to a directory with packed `*.bc<n>` chunks and matching `*.out` files |

  Benchmarks and some device-specific tests are also ignored by default.

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
