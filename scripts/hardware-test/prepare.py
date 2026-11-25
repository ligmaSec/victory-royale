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


def find_build(source, commit, prefix):
    baseline = source == "main"
    query = "branch=main" if baseline else "event=pull_request"
    runs = api(f"actions/workflows/ci.yml/runs?{query}&status=success&head_sha={commit}&per_page=100")
    for run in runs["workflow_runs"]:
        if baseline and (run["event"] not in ("push", "workflow_dispatch") or run["head_branch"] != "main"):
            continue
        for artifact in api(f"actions/runs/{run['id']}/artifacts?per_page=100")["artifacts"]:
            match = re.fullmatch(re.escape(prefix) + r"([0-9a-f]{40})", artifact["name"])
            if not match or artifact["expired"]:
                continue
            revision = match[1]
            if baseline:
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
                package = (Path(folder) / PACKAGE).read_bytes()
                digest = hashlib.sha256(package).hexdigest()
                checks = (Path(folder) / "SHA256SUMS").read_text().split()
                if checks != [digest, PACKAGE]:
                    raise ValueError("Package checksum mismatch.")
            return {"run": run["id"], "revision": revision, "sha256": digest}, base, package
    raise ValueError(f"No unexpired successful {source} CI package for {commit[:12]}. "
                     "Refresh the matched builds before asking a tester.")


def review_notices(out):
    """Include upstream license text and attribution from the locked editor dependencies."""
    tree = subprocess.check_output(["cargo", "tree", "--locked", "-p", "victory-report-review",
                                    "--target", "all", "--edges", "normal", "--prefix", "none"], text=True)
    linked = {tuple(line.split()[:2]) for line in tree.splitlines()}
    packages = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--format-version", "1"]))
    index = []
    for package in packages["packages"]:
        name, version = package["name"], package["version"]
        if not package["source"] or (name, "v" + version) not in linked:
            continue
        folder = Path(package["manifest_path"]).parent
        index.append(f"{name} {version} | {package['license']} | {package['repository']}")
        for path in sorted(folder.iterdir()):
            if path.is_file() and path.name.upper().startswith(("LICENSE", "LICENCE", "NOTICE", "COPYING", "COPYRIGHT")):
                out.writestr(f"review/licenses/{name}-{version}/{path.name}", path.read_bytes())
    out.writestr("review/licenses/INDEX.txt", "\n".join(sorted(index)) + "\n")


def prepare():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pr", type=int)
    parser.add_argument("--scenario", required=True, help="what to do in each run and when to close it")
    parser.add_argument("--output", required=True, type=Path, help="new request ZIP")
    parser.add_argument("--review-dir", type=Path, help="directory containing both native review executables")
    args = parser.parse_args()
    if args.pr < 1 or not args.scenario.strip() or not args.scenario.isprintable():
        parser.error("Use a positive PR number and a single plain-text scenario.")
    pull = api(f"pulls/{args.pr}")
    if pull["base"]["ref"] != "main" or pull["base"]["repo"]["full_name"] != REPO:
        raise ValueError("The PR must target this repository's main branch.")
    candidate, base, candidate_bytes = find_build("pull_request", pull["head"]["sha"],
                                 f"victory-proton-x86_64-pr-{args.pr}-")
    main, _, main_bytes = find_build("main", base, "victory-proton-x86_64-")
    request = {"version": 1, "pr": args.pr, "scenario": args.scenario, "main": main, "candidate": candidate}
    with zipfile.ZipFile(args.output, "x", zipfile.ZIP_DEFLATED) as out:
        for build, package in ((main, main_bytes), (candidate, candidate_bytes)):
            out.writestr(f"packages/{build['sha256']}.tar.gz", package, compress_type=zipfile.ZIP_STORED)
        if args.review_dir:
            review_notices(out)
            for name in ("victory-report-review", "victory-report-review.exe"):
                data = (args.review_dir / name).read_bytes()
                out.writestr("review/" + name, data)
                out.writestr("review/" + name + ".sha256", hashlib.sha256(data).hexdigest())
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
