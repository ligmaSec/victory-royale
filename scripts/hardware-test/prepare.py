"""Prepare a shareable hardware test request from successful CI builds."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import zipfile

REPO = "ligmaSec/victory-royale"
ROOT = Path(__file__).resolve().parent
PACKAGE = "victory-proton-x86_64.tar.gz"


def api(path):
    raw = subprocess.check_output(["gh", "api", f"repos/{REPO}/{path}"])
    return json.loads(raw)


def find_build(event, commit, prefix):
    runs = api(f"actions/workflows/ci.yml/runs?event={event}&status=success&head_sha={commit}&per_page=100")
    for run in runs["workflow_runs"]:
        if event == "push" and run["head_branch"] != "main":
            continue
        for artifact in api(f"actions/runs/{run['id']}/artifacts?per_page=100")["artifacts"]:
            match = re.fullmatch(re.escape(prefix) + r"([0-9a-f]{40})", artifact["name"])
            if not match or artifact["expired"]:
                continue
            revision = match[1]
            if event == "push":
                if revision != commit:
                    continue
                base = None
            else:
                parents = api("git/commits/" + revision)["parents"]
                if len(parents) != 2 or parents[1]["sha"] != commit:
                    continue
                base = parents[0]["sha"]
            # Pin the actual bytes that the maintainer checked, not a mutable branch name.
            with tempfile.TemporaryDirectory() as folder:
                subprocess.run(["gh", "run", "download", str(run["id"]), "-R", REPO,
                                "-n", artifact["name"], "-D", folder], check=True)
                digest = hashlib.sha256((Path(folder) / PACKAGE).read_bytes()).hexdigest()
                checks = (Path(folder) / "SHA256SUMS").read_text().split()
                if checks != [digest, PACKAGE]:
                    raise ValueError("Package checksum mismatch.")
            return {"run": run["id"], "revision": revision, "sha256": digest}, base
    raise ValueError(f"No unexpired successful {event} CI package for {commit[:12]}. "
                     "Refresh the matched builds before asking a tester.")


def prepare():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pr", type=int)
    parser.add_argument("--scenario", required=True, help="what to do in each run and when to close it")
    parser.add_argument("--output", required=True, type=Path, help="new request ZIP")
    args = parser.parse_args()
    if args.pr < 1 or not args.scenario.strip() or not args.scenario.isprintable():
        parser.error("Use a positive PR number and a single plain-text scenario.")
    pull = api(f"pulls/{args.pr}")
    if pull["base"]["ref"] != "main" or pull["base"]["repo"]["full_name"] != REPO:
        raise ValueError("The PR must target this repository's main branch.")
    candidate, base = find_build("pull_request", pull["head"]["sha"],
                                 f"victory-proton-x86_64-pr-{args.pr}-")
    main, _ = find_build("push", base, "victory-proton-x86_64-")
    request = {"version": 1, "pr": args.pr, "scenario": args.scenario, "main": main, "candidate": candidate}
    with zipfile.ZipFile(args.output, "x", zipfile.ZIP_DEFLATED) as out:
        out.write(ROOT / "run-test.py", "run-test.py")
        out.write(ROOT.parents[1] / "LICENSE", "LICENSE")
        out.write(ROOT / "README.txt", "README.txt")
        out.writestr("request.json", json.dumps(request, indent=2) + "\n")
    print(args.output.resolve())


if __name__ == "__main__":
    try:
        prepare()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
