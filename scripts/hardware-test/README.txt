Run this hardware comparison

Extract the whole kit. Open a terminal in the folder containing this file.
Use the command for your system.

  Linux/Proton    python3 run-test.py
  Windows         py run-test.py

Supply your local standard save in saves/7952987A/ beside this file in an
extracted kit, or in saves/7952987A/ at the repository root. You can also pass
--save-dir "/path/to/7952987A". Each build starts with a separate temporary
copy of your progress and persistent settings. The save stays on your machine.
A missing save produces a warning. Use --plan to preview without it.

Copy .victoryconfig.example to .victoryconfig beside the runner or at the checkout
root. Set game_dir or title once. A copy in your home folder applies across kits.
The example also covers Proton, Wine prefix, save, report folder and timing.
Use --config FILE to select a specific setup. Without game_dir or title, select
your game's .exe once if discovery cannot locate it. The runner checks
VICTORY_GAME_DIR, earlier file selections, the standard install in ~/games
or ~/Games and, on Linux, launch commands from recent local Proton reports. Before each launch, read WHAT TO DO.
Automatic scenarios close each build once its milestone and observation period
are complete. Follow the input instructions and leave the game open. A timeout
or early exit is recorded separately. For manual scenarios, close as instructed.
On Linux the terminal and each build's private log show the Proton version and
build reported by the runtime. A failed launch that produced no version header
is marked unreported.
Repeat the same inputs and keep the same settings and window size. Each build gets a fresh
translation cache. Add --plan to preview the instructions and build revisions.

====================================================================
WARNING: LOGS MAY CONTAIN SENSITIVE INFORMATION
====================================================================
Raw logs may include usernames, file paths, credentials and game output.
Keep the full archive local. Review the filtered ZIP before choosing to share.
During automatic runs the runner tries to capture the game window every 45 seconds
and before closing. The latest successful capture per build is kept next to
summary.txt. The private report.zip keeps the first and up to seven later samples
per build with seconds from launch in their names. A first mesh bind can happen
while loading. Record what scene was visible, or mark it unknown. On Linux this needs X11 or XWayland and XComposite. Windows uses
the game window's own drawing surface. Black images are possible with some drivers.
Use Review automatic screenshots to preview and select images for private delivery.
After the runs, use the full-screen results menu. Use Up/Down to highlight a
choice, Enter to select, Esc to go back and PgUp/PgDn to scroll the details.
Choose Describe what you saw to write notes for main and candidate in your browser.
Click either screenshot box and paste with Ctrl+V, or choose an image. Review its
preview for sensitive details. Save locally returns you to the terminal. Closing
the tab discards unsaved changes. Press Ctrl+C in the terminal to leave the editor.
Preview summary shows the timings and notes eligible for public posting. Review and edit
files opens the bundled editor: Enter edits a file, Space includes/excludes it,
Ctrl+F searches, Esc returns to the list and Ctrl+S applies your edits locally.
To use your own editor, choose Edit in my own editor, then Open folder.
Save edits or delete files, then select Apply edits. Back without applying
discards those working copies. Your original logs stay local.

Keep local and finish prints the filtered ZIP location. Post public summary
previews the account, PR and text. After posting, return to the results menu
to send diagnostics separately. Screenshots are only included in the reviewed ZIP. Send encrypted diagnostics lists the account,
verified requester and reviewed files. Confirm Encrypt and send to deliver the ZIP. Sending
requires GitHub CLI signed in to github.com with gist access. The runner names
your account before sending. It downloads a checksum-verified age tool, encrypts
the filtered ZIP and returns an unlisted GitHub Gist receipt. GitHub sees your
account, upload time and size. Anyone with the receipt can retrieve the encrypted
file. The requester receives a GitHub notification and decrypts the ZIP locally.
After sending, Finish privately is the default. Publish encrypted receipt lets
the requester collect your ZIP automatically. Your username and posting time
will be public on their key Gist. The link and diagnostics stay encrypted.
When the kit has no verified requester key, or the key has changed, keep your
diagnostics local and ask for a fresh kit. Offline runs still
save a shareable ZIP with frame times, submission costs and shader events.
Keep the raw report.zip local. Tell the requester what you saw on each build.

Both verified CI builds are included. You need Python 3.9 or newer. Linux
also needs Bash and UMU (umu-run) and may download pinned vkd3d-proton on
first use. Keep the game executable outside the VIctory checkout.

From a repository checkout, use python3 scripts/hardware-test/run-test.py
(or py on Windows) to choose a test. Requests with a kit from main can run directly. Other
requests labeled "tester wanted" link to the PR instructions. The menu requires a
signed-in GitHub CLI. You can also pass the PR number, a downloaded kit ZIP
or a request JSON. A JSON without bundled packages needs GitHub CLI to
download the builds.

Use --title "path to your game.exe" to skip the chooser, or --output-dir
"path to folder" to choose where the runner creates its report folder.
