VIctory hardware comparison

Extract this ZIP. In the extracted folder, run:

  Linux/Proton:  python3 run-test.py
  Windows:       py run-test.py

The runner shows the scenario and two GitHub CI builds. Select your game
executable when prompted. Both runs use Series X and separate, fresh
translation caches. Follow the on-screen scenario for each run. At the end,
the terminal prints one Upload path. This fresh report.zip stays in your user
state directory (LocalAppData on Windows). Share it privately with the
person who requested the test. Full game logs can contain private information.

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
