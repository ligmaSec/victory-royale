"""Run the two builds in an unpacked VIctory hardware test request. Python 3.9+."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile

REPO = "ligmaSec/victory-royale"
PACKAGE = "victory-proton-x86_64.tar.gz"
ROOT = Path(__file__).resolve().parent


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1 << 20), b""):
            value.update(block)
    return value.hexdigest()


def read_request(path):
    request = json.loads(path.read_text(encoding="utf-8"))
    if (request["version"] != 1 or type(request["pr"]) is not int or request["pr"] < 1
            or not isinstance(request["scenario"], str) or not request["scenario"].strip()
            or not request["scenario"].isprintable()):
        raise ValueError("Unrecognized test request.")
    for side in ("main", "candidate"):
        build = request[side]
        if not (type(build["run"]) is int and build["run"] > 0
                and re.fullmatch(r"[0-9a-f]{40}", build["revision"])
                and re.fullmatch(r"[0-9a-f]{64}", build["sha256"])):
            raise ValueError(f"Invalid {side} build identity.")
    return request


def download(request, side, folder):
    build = request[side]
    prefix = "" if side == "main" else f"pr-{request['pr']}-"
    artifact = f"victory-proton-x86_64-{prefix}{build['revision']}"
    folder.mkdir()
    subprocess.run(["gh", "run", "download", str(build["run"]), "-R", REPO,
                    "-n", artifact, "-D", str(folder)], check=True)
    archive = folder / PACKAGE
    if digest(archive) != build["sha256"]:
        raise ValueError(f"{side} package checksum differs from the request.")
    # Copy known distribution members only. Never unpack arbitrary paths or links.
    with tarfile.open(archive, "r:gz") as bundle:
        for name in ("victory.exe", "victory-proton.sh", "REVISION", "LICENSE"):
            member = bundle.getmember("victory/" + name)
            if not member.isfile():
                raise ValueError(f"Unexpected {side} package member: {name}")
            with (folder / name).open("wb") as out:
                shutil.copyfileobj(bundle.extractfile(member), out)
    if (folder / "REVISION").read_text().strip() != build["revision"]:
        raise ValueError(f"{side} package contains a different revision.")


def choose_title():
    try:
        from tkinter import TclError, Tk, filedialog
    except ImportError:
        window = None
    else:
        try:
            window = Tk()
        except TclError:
            window = None
    if window is not None:
        try:
            window.withdraw()
            chosen = filedialog.askopenfilename(title="Select your game executable",
                                                filetypes=[("Executable", "*.exe"), ("All files", "*")])
        finally:
            window.destroy()
        if not chosen:
            raise ValueError("Selection cancelled.")
    else:
        chosen = input("Game executable path: ").strip().strip("\"'")
    return Path(chosen).expanduser()


def launch(side, folder, title, results, windows):
    env = os.environ.copy()
    # Ignore a developer override that could silently substitute a different build.
    env.pop("VICTORY_EXE", None)
    env["VICTORY_TRACE_PIPELINES"] = "1"
    cache = folder / "cache"
    env["VICTORY_CACHE_DIR"] = str(cache) if windows else "Z:" + str(cache).replace("/", "\\")
    args = [str(title), "--console", "series-x"]
    if windows:
        command = [str(folder / "victory.exe"), "run", str(title), "--execute", "--console", "series-x"]
    else:
        env.update(VKD3D_SHADER_CACHE_PATH=str(folder / "vkd3d"), DXVK_STATE_CACHE_PATH=str(folder / "dxvk"))
        command = ["bash", str(folder / "victory-proton.sh"), "--report", "--runtime", "pinned", *args]
    console = results / (side + ".log")
    report = None
    print(f"Starting {side}.", flush=True)
    with console.open("wb") as log, subprocess.Popen(command, cwd=title.parent, env=env,
                                                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT) as process:
        with process.stdout:
            for line in process.stdout:
                log.write(line)
                if not windows:
                    found = re.fullmatch(rb"Report: (.+) \(launcher exit [0-9]+\)", line.rstrip(b"\r\n"))
                    if found:
                        report = Path(os.fsdecode(found[1]))
        status = process.wait()
    (results / (side + "-exit.txt")).write_text(f"{status}\n")
    if not windows:
        if report is None or not report.is_file():
            raise ValueError(f"The {side} launcher did not produce a report. See the console log in the partial report.")
        shutil.copyfile(report, results / (side + ".tar.gz"))
    print(f"{side} exited ({status}).", flush=True)


def run():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("request", nargs="?", type=Path, default=ROOT / "request.json",
                        help="request JSON from the maintainer")
    parser.add_argument("--output-dir", type=Path, help="parent directory for the unique report")
    parser.add_argument("--plan", action="store_true", help="show the request without downloading or launching")
    parser.add_argument("--title", type=Path, help="use this executable instead of the file chooser")
    options = parser.parse_args()
    manifest = options.request
    request = read_request(manifest)
    print(f"VIctory PR #{request['pr']} | Series X | two launches\n{request['scenario']}")
    for side in ("main", "candidate"):
        build = request[side]
        print(f"{side}: {build['revision'][:12]}  https://github.com/{REPO}/actions/runs/{build['run']}")
    print("Reports stay on this computer. Nothing is uploaded automatically.", flush=True)
    if options.plan:
        return
    windows = sys.platform == "win32"
    if not windows and not sys.platform.startswith("linux"):
        raise ValueError("This kit supports Windows and Linux/Proton.")
    for binary in (["gh"] if windows else ["gh", "bash", "umu-run"]):
        if not shutil.which(binary):
            raise ValueError(f"Missing {binary}. See the requirements in README.txt.")
    title = (options.title or choose_title()).expanduser().resolve()
    if not title.is_file() or title.suffix.lower() != ".exe":
        raise ValueError("Select the game's .exe file.")
    state = os.getenv("LOCALAPPDATA" if windows else "XDG_STATE_HOME")
    default_home = Path(state) if state and Path(state).is_absolute() else Path.home() / ".local" / "state"
    output = (options.output_dir or default_home / "victory" / "reports").expanduser().resolve()
    for directory in (output, Path(tempfile.gettempdir()).resolve(), title.parent):
        if any((parent / ".victory-root").exists() for parent in (directory, *directory.parents)):
            raise ValueError("Use a game, temporary directory and report destination outside the VIctory checkout.")
    with tempfile.TemporaryDirectory(prefix="victory-builds-") as work:
        for side in ("main", "candidate"):
            print(f"Downloading and checking {side}…", flush=True)
            download(request, side, Path(work) / side)
        results = Path(work) / "results"
        results.mkdir()
        shutil.copyfile(manifest, results / "request.json")
        complete = False
        try:
            for side in ("main", "candidate"):
                launch(side, Path(work) / side, title, results, windows)
            complete = True
        finally:
            if any(results.glob("*.log")):
                output.mkdir(parents=True, exist_ok=True)
                destination = Path(tempfile.mkdtemp(prefix=f"pr{request['pr']}-", dir=output))
                bundle = destination / "report.zip"
                with zipfile.ZipFile(bundle, "x", zipfile.ZIP_DEFLATED, compresslevel=1) as out:
                    for path in sorted(results.iterdir()):
                        out.write(path, path.name)
                label = "Upload" if complete else "Partial report"
                print(f"{label}: {bundle}", flush=True)


if __name__ == "__main__":
    try:
        run()
    except (OSError, ValueError, TypeError, KeyError, EOFError, subprocess.CalledProcessError, tarfile.TarError) as error:
        sys.exit(str(error))
