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
At REPORT READY or TEST INTERRUPTED, review the on-screen summary. Press Enter
to keep it local. You can post the summary to the PR under your GitHub account
or press [i] to send detailed diagnostics encrypted to the maintainer. Sending
requires GitHub CLI signed in to github.com with gist access. The runner names
your account before sending. It downloads a checksum-verified age tool, encrypts
the filtered ZIP and returns an unlisted GitHub Gist receipt. GitHub sees your
account, upload time and size. Anyone with the receipt can retrieve the encrypted
file. The private option requires a configured project key. Offline runs still
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
