VIctory hardware comparison

From the repo root, run this to choose a published Series X test:

  python3 scripts/hardware-test/run-test.py

If the requester gave you a PR number, add it to the command. On Windows,
use py in place of python3. To preview a test, add --plan.

If you downloaded a kit ZIP, pass its path instead of the PR number.
You can also extract the ZIP and run from the extracted folder:

  Linux/Proton:  python3 run-test.py
  Windows:       py run-test.py

Select your game executable when prompted. Both runs use Series X and fresh
translation caches. Follow the WHAT TO DO instructions shown before each
launch. After the second run, send the ZIP at the printed Upload path
privately to the requester. The ZIP stays in a new folder under your
system's temporary directory after the runner exits. Send it before that
directory is cleaned.

To see the plan without starting, add --plan. To supply the game directly
instead of choosing a file, add --title "path to your game.exe". To use a
different report folder, add --output-dir and the folder path. You can reuse
this runner with a new request by passing the new JSON file as its argument.

Requires Python 3.9+. Linux also needs bash and umu-run. Both verified CI
packages are included. The Proton launcher may download its pinned
vkd3d-proton runtime on first use. If you received only a JSON request,
the runner uses a signed-in GitHub CLI to fetch missing CI packages.
Keep your usual driver, game settings and window size. Temporary build
files are removed after the test.
