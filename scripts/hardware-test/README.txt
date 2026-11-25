Run this Series X comparison

Extract the whole kit. Open a terminal in the folder containing this file.
Use the command for your system.

  Linux/Proton    python3 run-test.py
  Windows         py run-test.py

Select your game's .exe when prompted. Before each launch, read WHAT TO DO.
Run the scenario on main, close the game as instructed, then repeat on the
candidate with the same settings and window size. Each build gets a fresh
translation cache. Add --plan to preview the instructions and build revisions.

====================================================================
WARNING: LOGS MAY CONTAIN SENSITIVE INFORMATION
====================================================================
Raw logs may include usernames, file paths, credentials and game output.
Keep the full archive local. Review the filtered ZIP before choosing to share.
After the runs, use the full-screen results menu. Use Up/Down to highlight a
choice, Enter to select, Esc to go back and PgUp/PgDn to scroll the details.
Preview summary shows the text eligible for public posting. Review and edit
files opens the bundled editor: Enter edits a file, Space includes/excludes it,
Ctrl+F searches, Esc returns to the list and Ctrl+S applies your edits locally.
To use your own editor, choose Edit in my own editor, then Open folder.
Save edits or delete files, then select Apply edits. Back without applying
discards those working copies. Your original logs stay local.

Keep local and finish prints the filtered ZIP location. Post public summary
previews the account, PR and text. Send encrypted diagnostics lists the account,
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
