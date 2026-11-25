"""Find and run a Series X hardware comparison for VIctory. Python 3.9+."""
import argparse
import csv
from datetime import datetime, timedelta, timezone
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
import textwrap
import zipfile

REPO = "ligmaSec/victory-royale"
PACKAGE = "victory-proton-x86_64.tar.gz"
# Only a reviewed runner change can select the recipient for private reports.
REPORT_RECIPIENT = "age1mdqjdgme7frky259ez5xkmzylqafkjp3e8tms5flct2lh0h735hq4xjdkj"
REPORT_MAINTAINER = "ligmaSec"
AGE_RELEASE = "v1.3.2"
AGE_PACKAGES = {
    "linux": ("linux-amd64.tar.gz", "cbe24006683f8eb669266162894b9a522a1af52f2665fbc63a4bb032ed26ac10"),
    "win32": ("windows-amd64.zip", "f48d8f8f9ebe903ab5027ed067652f2cc1db94bc206976430133b905dcd8e8c7"),
}
ROOT = Path(__file__).resolve().parent
OUTGOING = ("summary.txt", "main-frames.csv", "candidate-frames.csv", "main-submissions.csv",
            "candidate-submissions.csv", "main-events.csv", "candidate-events.csv")
REVIEWER = "victory-report-review" + (".exe" if sys.platform == "win32" else "")
PRIVACY_HEADING = "WARNING: LOGS MAY CONTAIN SENSITIVE INFORMATION"
LOG_PRIVACY = "Raw logs can include your username, file paths, credentials, or output from the game."

ANSI = re.compile(rb"\x1b\[[0-9;]*m")
MS = rb"([0-9]{1,9}\.[0-9]{2})"
INTERVAL = re.compile(rb"present intervals: ([0-9]{1,4}) frames, mean " + MS + rb" ms, median " + MS
                      + rb" ms, p95 " + MS + rb" ms, longest " + MS
                      + rb" ms, ([0-9]{1,4}) over 100 ms, ([0-9]{1,4}) over 1 s$")
# The release logger emits one sample of submission cost per reporting period.
# Match the full message so a format change cannot shift values between columns.
SUBMISSION_FORMAT = (b"%f fps, %f ms/frame (%f preparing: copies %f ms %f MB, textures %f ms %f MB; "
                     b"%f on the GPU; %f copying back %f MB), %i draws/frame, dropped %i without a "
                     b"host pipeline (%i being made) and %i indirect")
SUBMISSION = re.compile(rb"(?<![0-9.+-])" + re.escape(SUBMISSION_FORMAT)
                        .replace(b"%f", rb"([0-9]{1,10}\.[0-9])").replace(b"%i", rb"([0-9]{1,10})") + rb"$")
SUBMISSION_COLUMNS = ("fps", "ms_per_frame", "preparing_ms", "buffer_copy_ms", "buffer_copy_mb",
                      "texture_ms", "texture_mb", "gpu_wait_ms", "copy_back_ms", "copy_back_mb",
                      "draws_per_frame", "dropped_per_frame", "pipelines_being_made", "indirect_per_frame")
VENDORS = {b"0x1002": "AMD", b"0x10de": "NVIDIA", b"0x8086": "Intel", b"0x1414": "Microsoft"}
EVENTS = {b"shader does not translate": "shader refusal",
          b"host compute pipeline refused": "compute pipeline refusal",
          b"no free dynamic descriptor slots": "descriptor exhaustion",
          b"device removed": "device removal"}


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


def stage_build(request, side, folder, packages):
    build = request[side]
    prefix = "" if side == "main" else f"pr-{request['pr']}-"
    artifact = f"victory-proton-x86_64-{prefix}{build['revision']}"
    folder.mkdir()
    archive = packages / (build["sha256"] + ".tar.gz")
    if not archive.is_file():
        if not shutil.which("gh"):
            raise ValueError("The request is missing its packages. Extract the entire kit or install GitHub CLI.")
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


def github_rows(endpoint, query):
    if not shutil.which("gh"):
        raise ValueError("Published requests need GitHub CLI. Install gh and run gh auth login, "
                         "or pass a downloaded kit ZIP to this command.")
    result = subprocess.run(["gh", "api", "--paginate", f"repos/{REPO}/{endpoint}", "--jq", query],
                            capture_output=True, encoding="utf-8")
    if result.returncode:
        raise ValueError(f"GitHub lookup failed: {result.stderr.strip()}\n"
                         "Check gh auth status and your connection, or use a downloaded kit ZIP.")
    return [json.loads(line) for line in result.stdout.splitlines() if line]


def hardware_requests(number):
    pulls = github_rows("pulls?state=open&base=main&per_page=100",
                        '.[] | {number, title, head: .head.sha, wanted: any(.labels[]; .name == "tester wanted")}')
    open_prs = {pr["number"]: pr for pr in pulls if number is None or pr["number"] == number}
    if not open_prs:
        return {}
    # The workflow retains kits for seven days. Trust successful dispatches on main.
    since = (datetime.now(timezone.utc) - timedelta(days=8)).date().isoformat()
    runs = github_rows("actions/workflows/hardware-request.yml/runs?branch=main&event=workflow_dispatch"
                       f"&status=success&created=>={since}&per_page=100", ".workflow_runs[] | .id")
    available = {}
    for run_id in sorted(runs, reverse=True):
        artifacts = github_rows(f"actions/runs/{run_id}/artifacts?per_page=100",
                                '.artifacts[] | select(.expired == false) | {name}')
        for artifact in artifacts:
            match = re.fullmatch(r"victory-series-x-pr-([1-9][0-9]*)", artifact["name"])
            if match:
                pr = int(match[1])
                if pr in open_prs and pr not in available:
                    available[pr] = (open_prs[pr], run_id, artifact["name"])
        if len(available) == len(open_prs):
            break
    for pr, pull in open_prs.items():
        if pull["wanted"] and pr not in available:
            available[pr] = (pull, None, None)
    return available


def select_published(number, list_only, folder):
    available = hardware_requests(number)
    if number is None:
        if not available:
            print("There are no open Series X hardware test requests.")
            return None
        print("Series X hardware test requests:")
        for pr, (pull, run_id, _) in sorted(available.items()):
            status = "kit available" if run_id else f"see request at https://github.com/{REPO}/pull/{pr}"
            print(f"  #{pr}  {pull['title']}  [{status}]")
        if list_only or not sys.stdin.isatty():
            print("Pass a PR number to see its instructions or run its kit.")
            return None
        choice = input("PR number (Enter to cancel): ").strip().lstrip("#")
        if not choice:
            return None
        if not choice.isascii() or not choice.isdecimal() or int(choice) not in available:
            raise ValueError("Choose one of the listed PR numbers.")
        number = int(choice)
    if number not in available:
        raise ValueError(f"No published kit for open PR #{number}. Ask the requester to publish a fresh kit.")
    pull, run_id, artifact = available[number]
    if run_id is None:
        print(f"PR #{number} is looking for testers. See https://github.com/{REPO}/pull/{number}")
        print("This request has no unexpired kit published from main.")
        return None
    print(f"Downloading the Series X test for PR #{number}…", flush=True)
    subprocess.run(["gh", "run", "download", str(run_id), "-R", REPO, "-n", artifact, "-D", str(folder)],
                   check=True)
    manifest = folder / "request.json"
    if not manifest.is_file():
        raise ValueError("The published kit is missing its request.json.")
    request = read_request(manifest)
    if request["pr"] != number:
        raise ValueError("The published kit names a different PR.")
    parents = github_rows("git/commits/" + request["candidate"]["revision"], ".parents[] | {sha}")
    if [parent["sha"] for parent in parents] != [request["main"]["revision"], pull["head"]]:
        raise ValueError(f"PR #{number} changed after this kit was built. Ask the requester to publish a fresh kit.")
    return manifest


def unpack_kit(path, folder, plan):
    manifest = folder / "request.json"
    with zipfile.ZipFile(path) as kit:
        if "request.json" not in kit.namelist():
            raise ValueError("This ZIP has no hardware test request.json.")
        manifest.write_bytes(kit.read("request.json"))
        request = read_request(manifest)
        if not plan:
            packages = folder / "packages"
            packages.mkdir()
            for checksum in {request[side]["sha256"] for side in ("main", "candidate")}:
                name = checksum + ".tar.gz"
                with kit.open("packages/" + name) as source, (packages / name).open("wb") as target:
                    shutil.copyfileobj(source, target)
    return manifest


def run():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("request", nargs="?", help="PR number, test kit ZIP or request JSON")
    parser.add_argument("--list", action="store_true", help="list Series X hardware test requests")
    parser.add_argument("--output-dir", type=Path, help="parent directory for the unique report")
    parser.add_argument("--plan", action="store_true", help="preview the selected test without launching")
    parser.add_argument("--title", type=Path, help="use this executable instead of the file chooser")
    options = parser.parse_args()
    if options.list and options.request:
        parser.error("--list takes no request")
    with tempfile.TemporaryDirectory(prefix="victory-request-") as temporary:
        folder = Path(temporary)
        argument = options.request
        manifest = ROOT / "request.json"
        if options.list or (argument is None and not manifest.is_file()):
            manifest = select_published(None, options.list, folder)
        elif argument is not None:
            if re.fullmatch(r"#?[1-9][0-9]*", argument):
                pr = int(argument.lstrip("#"))
                manifest = select_published(pr, False, folder)
                if options.plan and manifest is None:
                    raise ValueError(f"PR #{pr} has no published kit to preview.")
            else:
                path = Path(argument).expanduser()
                if not path.is_file():
                    raise ValueError(f"Cannot find {path}. Pass a PR number or an existing kit ZIP or request JSON.")
                manifest = unpack_kit(path, folder, options.plan) if path.suffix.lower() == ".zip" else path
        if manifest is not None:
            # Execute tools only from our runner's directory or the official discovery path.
            # A supplied JSON or ZIP may describe game builds but cannot select an editor.
            from_picker = argument is None or re.fullmatch(r"#?[1-9][0-9]*", argument)
            options.review_kit = folder if from_picker and manifest.parent == folder else ROOT
            run_request(manifest, options)


def warning_color():
    if not sys.stdout.isatty() or "NO_COLOR" in os.environ or os.environ.get("TERM") == "dumb":
        return False
    if sys.platform != "win32":
        return True
    # Enable VT processing on traditional Windows consoles as well as Windows Terminal.
    import ctypes
    from ctypes import wintypes
    import msvcrt
    try:
        handle = wintypes.HANDLE(msvcrt.get_osfhandle(sys.stdout.fileno()))
    except (OSError, ValueError):
        return False
    mode = wintypes.DWORD()
    kernel = ctypes.windll.kernel32
    return bool(kernel.GetConsoleMode(handle, ctypes.byref(mode))
                and kernel.SetConsoleMode(handle, mode.value | 0x0004))


def instruction_block(heading, message, path_line=None):
    width = max(20, min(72, shutil.get_terminal_size().columns - 1))
    rule = "=" * width
    message = textwrap.fill(message, width, break_long_words=False, break_on_hyphens=False)
    banner = "\n".join([rule, textwrap.fill(heading, width), rule])
    if heading == PRIVACY_HEADING and warning_color():
        banner = "\x1b[1;91m" + banner + "\x1b[0m"
    lines = ["", banner, message]
    if path_line:
        lines.extend(["", path_line])
    print("\n".join(lines), end="\n\n", flush=True)


STAGE = rb"(vertex|pixel|domain|hull|geometry|mesh|amplification|compute)"
SHADER_REF = re.compile(rb'\bstage="?' + STAGE + rb'"? .*?\bhash=([0-9a-f]{16})\b')
PIPELINE_REF = re.compile(STAGE + rb" ([0-9a-f]{16})\b")
LOG_TIME = re.compile(rb"^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d{1,6})?)Z\s")


class Timeline:
    """Run-relative timing and opaque shader numbers shared by the two builds."""
    def __init__(self, output, shaders):
        self.events = csv.writer(output)
        self.events.writerow(("elapsed_ms", "event", "stage", "shader_id", "frame", "sample"))
        self.shaders = shaders
        self.start = None

    def record(self, line, event, stage=b"", shader=None, frame="", sample=""):
        elapsed = ""
        stamp = LOG_TIME.match(line)
        if stamp:
            try:
                now = datetime.fromisoformat(stamp[1].decode("ascii"))
                if self.start is None:
                    self.start = now
                elapsed = round((now - self.start).total_seconds() * 1000)
            except ValueError:
                pass
        shader_id = self.shaders.setdefault(shader, len(self.shaders) + 1) if shader else ""
        self.events.writerow((elapsed, event, stage.decode("ascii"), shader_id, frame, sample))

    def shader_event(self, raw):
        line = ANSI.sub(b"", raw)
        for marker, name in ((b"first bind pipeline=", "first bind"), (b"shader translated stage=", "translated"),
                             (b"shader does not translate;", "translation refused")):
            if marker in line:
                ref = SHADER_REF.search(line)
                if ref:
                    frame = re.search(rb"\bframe=([0-9]{1,10})\b", line)
                    self.record(line, name, *ref.groups(), frame=int(frame[1]) if frame else "")
                    return True
        pipeline = re.search(rb"\bhost (?:mesh )?pipeline( refused [^\n]*?)? shaders=\[([^\]]{1,256})\]", line)
        if pipeline:
            refs = PIPELINE_REF.findall(pipeline[2])
            for stage, shader in refs:
                self.record(line, "pipeline refused" if pipeline[1] else "pipeline ready", stage, shader)
            return bool(refs)
        return False


def read_metrics(stream, frame_log, submission_log, timeline):
    """Count diagnostic messages and completed, non-overlapping present windows."""
    frames = windows = slow = stalls = submissions = 0
    shader_seen = False
    elapsed = longest = 0.0
    counts = {}
    vendor = None
    for raw in stream:
        if b"present intervals:" in raw:
            match = INTERVAL.search(ANSI.sub(b"", raw).rstrip())
            if match:
                n, mean, median, p95, worst, over100, over1000 = match.groups()
                n, over100, over1000 = int(n), int(over100), int(over1000)
                if 1 <= n <= 4096 and over1000 <= over100 <= n:
                    frame_log.writerow([windows + 1, n, float(mean), float(median), float(p95),
                                        float(worst), over100, over1000])
                    frames += n
                    windows += 1
                    timeline.record(ANSI.sub(b"", raw), "present window", sample=windows)
                    elapsed += n * float(mean)
                    longest = max(longest, float(worst))
                    slow += over100
                    stalls += over1000
        elif b" ms/frame (" in raw:
            match = SUBMISSION.search(ANSI.sub(b"", raw).rstrip())
            if match:
                submission_log.writerow([value.decode("ascii") for value in match.groups()])
                submissions += 1
                timeline.record(ANSI.sub(b"", raw), "submission sample", sample=submissions)
        elif b"host D3D12 device ready" in raw:
            match = re.search(rb"\bvendor=(0x[0-9a-f]{4})\b", ANSI.sub(b"", raw))
            if match:
                vendor = VENDORS.get(match[1], "other")
        else:
            if any(marker in raw for marker in (b"host pipeline", b"host mesh pipeline", b"first bind",
                                                b"shader translated", b"shader does not translate;")):
                shader_seen |= timeline.shader_event(raw)
            for marker, name in EVENTS.items():
                if marker in raw:
                    counts[name] = counts.get(name, 0) + 1
    rows = [f"GPU vendor: {vendor or 'unreported'}"]
    if frames and elapsed:
        rows.extend([f"Completed present windows: {windows} ({frames} frame intervals)",
                     f"Mean: {elapsed / frames:.2f} ms/frame ({1000 * frames / elapsed:.2f} FPS)",
                     f"Longest: {longest:.2f} ms | over 100 ms: {slow} | over 1 s: {stalls}"])
    else:
        rows.append("Frame timings: unavailable")
    if counts:
        rows.append("Log messages: " + ", ".join(f"{name}: {counts[name]}" for name in sorted(counts)))
    return rows, bool(frames or submissions or counts or vendor or shader_seen)


def side_metrics(results, side, windows, frame_log, submission_log, timeline):
    console = results / (side + ".log")
    if not console.is_file():
        return ["No console log recorded"]
    if not windows:
        archive = results / (side + ".tar.gz")
        if archive.is_file():
            # UMU typically puts VIctory output in the Steam log. Some versions
            # use the launcher log. Read one source so mirrored lines count once.
            try:
                with tarfile.open(archive, "r:gz") as bundle:
                    logs = {member.name.removeprefix("./"): member for member in bundle.getmembers()
                            if member.isfile()}
                    other_steam = sorted(name for name in logs if name != "steam-victory.log"
                                         and re.fullmatch(r"steam-[^/]+\.log", name))
                    for name in ("steam-victory.log", *other_steam, "launcher.log"):
                        if name in logs:
                            with bundle.extractfile(logs[name]) as stream:
                                rows, recognized = read_metrics(stream, frame_log, submission_log, timeline)
                            if recognized:
                                return rows
            except (OSError, EOFError, tarfile.TarError):
                pass
    with console.open("rb") as stream:
        rows, _ = read_metrics(stream, frame_log, submission_log, timeline)
        return rows


def diagnostic_summary(request, results, windows, complete, destination):
    shader_ids = {}
    rows = [f"Series X hardware test | PR #{request['pr']} | " + ("Windows" if windows else "Linux/Proton"),
            "Comparison completed" if complete else "Partial comparison",
            "Timings cover all recorded phases, including menus."]
    for side in ("main", "candidate"):
        rows.extend(["", f"{side.capitalize()} | {request[side]['revision']}"])
        exit_path = results / (side + "-exit.txt")
        status = exit_path.read_bytes().strip() if exit_path.is_file() else b""
        code = str(int(status)) if re.fullmatch(rb"-?[0-9]{1,10}", status) else "unreported"
        frames_path = destination / (side + "-frames.csv")
        samples_path = destination / (side + "-submissions.csv")
        events_path = destination / (side + "-events.csv")
        with frames_path.open("x", encoding="ascii", newline="") as frames, \
                samples_path.open("x", encoding="ascii", newline="") as samples, \
                events_path.open("x", encoding="ascii", newline="") as events:
            frame_log = csv.writer(frames)
            submission_log = csv.writer(samples)
            submission_log.writerow(SUBMISSION_COLUMNS)
            frame_log.writerow(["window", "intervals", "mean_ms", "median_ms", "p95_ms", "longest_ms",
                                "over_100_ms", "over_1_s"])
            rows.append(f"Launcher exit: {code}")
            rows.extend(side_metrics(results, side, windows, frame_log, submission_log, Timeline(events, shader_ids)))
    return "\n".join(rows) + "\n"


def encrypt_report(filtered, recipient):
    # Only this file's native age recipient is accepted. No plugin or identity
    # from a test request participates in encryption.
    if not re.fullmatch(r"age1[023456789acdefghjklmnpqrstuvwxyz]{58}", recipient):
        raise ValueError("Invalid project report recipient.")
    with tempfile.TemporaryDirectory(prefix="victory-encrypt-") as temporary:
        work = Path(temporary)
        package, checksum = AGE_PACKAGES[sys.platform]
        name = f"age-{AGE_RELEASE}-{package}"
        subprocess.run(["gh", "release", "download", AGE_RELEASE, "-R", "github.com/FiloSottile/age",
                        "-p", name, "-D", str(work)], check=True, capture_output=True)
        archive = work / name
        if digest(archive) != checksum:
            raise ValueError("The age download failed its checksum.")
        if sys.platform == "win32":
            exe = work / "age.exe"
            with zipfile.ZipFile(archive) as bundle, bundle.open("age/age.exe") as src, exe.open("xb") as out:
                shutil.copyfileobj(src, out)
        else:
            exe = work / "age"
            with tarfile.open(archive, "r:gz") as bundle, bundle.extractfile("age/age") as src, exe.open("xb") as out:
                shutil.copyfileobj(src, out)
            exe.chmod(0o700)
        return subprocess.check_output([str(exe), "--armor", "--recipient", recipient, str(filtered)],
                                       stderr=subprocess.PIPE).decode("ascii")


def send_encrypted_report(filtered, recipient):
    armor = encrypt_report(filtered, recipient)
    # Always create explicitly unlisted on github.com. Only the encrypted file
    # is present, including when the second request (notification) fails.
    gist = {"public": False, "files": {"report.age": {"content": armor}}}
    api = ["gh", "api", "--hostname", "github.com"]
    gist_id = subprocess.run([*api, "gists", "--input", "-", "--jq", ".id"], input=json.dumps(gist),
                             capture_output=True, text=True, check=True).stdout.strip()
    if not re.fullmatch(r"[0-9a-f]{32}", gist_id):
        raise ValueError("GitHub did not return a gist receipt.")
    # Record the receipt before notification so a failed mention needs no new upload.
    receipt = f"https://gist.github.com/{gist_id}"
    print(f"Encrypted report: {receipt}", flush=True)
    notice = {"body": f"@{REPORT_MAINTAINER} A VIctory hardware report is ready."}
    result = subprocess.run([*api, f"gists/{gist_id}/comments", "--input", "-"], input=json.dumps(notice),
                            capture_output=True, text=True)
    if result.returncode:
        print(f"Send the receipt privately to @{REPORT_MAINTAINER}. The notification was unsuccessful.")
    else:
        print(f"Notification posted for @{REPORT_MAINTAINER}.")


def review_result(filtered, kit):
    """Edit temporary copies. Replace the outgoing ZIP only after an explicit apply."""
    with tempfile.TemporaryDirectory(prefix="victory-review-", dir=filtered.parent) as temporary:
        work = Path(temporary)
        files = work / "files"
        files.mkdir()
        with zipfile.ZipFile(filtered) as archive:
            names = [name for name in OUTGOING if name in archive.namelist()]
            for name in names:
                (files / name).write_bytes(archive.read(name))
        if not names:
            print("All files were excluded. There is nothing to review.")
            return
        helper = kit / "review" / REVIEWER
        if helper.is_file():
            # GitHub artifact extraction can clear execute permissions.
            exe = work / REVIEWER
            shutil.copyfile(helper, exe)
            expected = (kit / "review" / (REVIEWER + ".sha256")).read_text(encoding="ascii").strip()
            if digest(exe) != expected:
                raise ValueError("The review tool failed its checksum.")
            exe.chmod(0o700)
            result = subprocess.run([str(exe), *names], cwd=files)
            if result.returncode == 2:
                return
            result.check_returncode()
        else:
            print(f"Open this folder in your editor: {files}")
            print("Edit the text or delete files to exclude them. Close your editor when finished.")
            if input("[a] Apply edits locally   [Enter] Cancel: ").strip().lower() != "a":
                return
        replacement = work / "reviewed.zip"
        with zipfile.ZipFile(replacement, "x") as archive:
            for name in names:
                path = files / name
                if path.is_symlink():
                    raise ValueError("Reviewed files must be regular text files.")
                if not path.exists():
                    continue
                if not path.is_file():
                    raise ValueError("Reviewed files must be regular text files.")
                data = path.read_bytes()
                content = data.decode("utf-8").replace("\r\n", "\n")
                if any(not ch.isprintable() and ch not in "\t\n" for ch in content):
                    raise ValueError(f"{name} contains hidden control characters. Use plain text.")
                archive.writestr(zipfile.ZipInfo(name), data, compress_type=zipfile.ZIP_DEFLATED)
        os.replace(replacement, filtered)
        print("Applied to the local ZIP. Choose a destination when ready.")


def share_result(pr, filtered, kit):
    instruction_block(PRIVACY_HEADING, LOG_PRIVACY + " Review the outgoing files for personal information, "
                      "including anything you add. Keep the raw archive local. You can reply with a written summary or press Enter to finish locally.",
                      f"Filtered diagnostics: {filtered}")
    with zipfile.ZipFile(filtered) as archive:
        if "summary.txt" in archive.namelist():
            print(archive.read("summary.txt").decode("utf-8"), flush=True)
    if not sys.stdin.isatty():
        return
    private = ""
    if REPORT_RECIPIENT:
        try:
            account = subprocess.check_output(["gh", "api", "--hostname", "github.com", "user", "--jq", ".login"],
                                              text=True, stderr=subprocess.DEVNULL, timeout=5).strip()
        except (OSError, subprocess.SubprocessError):
            print("Encrypted sharing needs GitHub CLI signed in to github.com with gist access.")
        else:
            print(f"GitHub will associate the unlisted encrypted gist with {account}. "
                  f"Only the holder of @{REPORT_MAINTAINER}'s report key can decrypt it.")
            private = "   [i] Send encrypted report"
    try:
        while True:
            with zipfile.ZipFile(filtered) as archive:
                names = archive.namelist()
                report = archive.read("summary.txt").decode("utf-8") if "summary.txt" in names else ""
            if not names:
                print("All files excluded. Nothing to send.")
                return
            public = f"   [p] Public summary on PR #{pr}" if report.strip() else ""
            choice = input(f"[r] Review/edit   [Enter] Keep local{private}{public}: ").strip().lower()
            if choice != "r":
                break
            try:
                review_result(filtered, kit)
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                print(f"Review could not be applied: {error}")
                return
        if choice == "i" and private:
            try:
                send_encrypted_report(filtered, REPORT_RECIPIENT)
            except (OSError, ValueError, subprocess.SubprocessError):
                print("Private delivery unconfirmed. Check your gists before retrying. "
                      "GitHub CLI must be signed in with gist access. "
                      f"Your filtered ZIP is saved here: {filtered}")
            return
    except (EOFError, KeyboardInterrupt):
        return
    if choice != "p" or not public:
        return
    try:
        account = subprocess.check_output(["gh", "api", "--hostname", "github.com", "user", "--jq", ".login"],
                                          text=True, stderr=subprocess.DEVNULL).strip()
        print(report, flush=True)
        answer = input(f"Post the summary above as {account} to https://github.com/{REPO}/pull/{pr}? [y/N] ")
        if answer.strip().lower() == "y":
            subprocess.run(["gh", "pr", "comment", str(pr), "-R", "github.com/" + REPO, "--body-file", "-"],
                           input=report.encode("utf-8"), check=True)
    except (EOFError, KeyboardInterrupt):
        return
    except (OSError, subprocess.CalledProcessError):
        print("Sharing did not finish. Check the PR before retrying. Your result is saved locally.")


def run_request(manifest, options):
    request = read_request(manifest)
    print(f"VIctory PR #{request['pr']} | Series X | two launches", flush=True)
    instruction_block(PRIVACY_HEADING, LOG_PRIVACY + " Your reports stay on this machine until you choose to share.")
    if options.plan:
        for side in ("main", "candidate"):
            build = request[side]
            print(f"{side}: {build['revision'][:12]}  https://github.com/{REPO}/actions/runs/{build['run']}")
        instruction_block("TEST INSTRUCTIONS", request["scenario"])
        return
    windows = sys.platform == "win32"
    if not windows and not sys.platform.startswith("linux"):
        raise ValueError("This kit supports Windows and Linux/Proton.")
    for binary in ([] if windows else ["bash", "umu-run"]):
        if not shutil.which(binary):
            raise ValueError(f"Missing {binary}. See the requirements in README.txt.")
    title = (options.title or choose_title()).expanduser().resolve()
    if not title.is_file() or title.suffix.lower() != ".exe":
        raise ValueError("Select the game's .exe file.")
    temp = Path(tempfile.gettempdir()).resolve()
    output = (options.output_dir or temp).expanduser().resolve()
    for directory in (output, temp, title.parent):
        if any((parent / ".victory-root").exists() for parent in (directory, *directory.parents)):
            raise ValueError("Use a game, temporary directory and report destination outside the VIctory checkout.")
    with tempfile.TemporaryDirectory(prefix="victory-builds-") as work:
        for side in ("main", "candidate"):
            print(f"Checking {side}…", flush=True)
            stage_build(request, side, Path(work) / side, manifest.resolve().parent / "packages")
        results = Path(work) / "results"
        results.mkdir()
        shutil.copyfile(manifest, results / "request.json")
        complete = False
        try:
            for number, side in enumerate(("main", "candidate"), start=1):
                instruction_block(f"RUN {number} OF 2  |  {side.upper()}  |  WHAT TO DO", request["scenario"])
                launch(side, Path(work) / side, title, results, windows)
            complete = True
        finally:
            if any(results.glob("*.log")):
                output.mkdir(parents=True, exist_ok=True)
                destination = Path(tempfile.mkdtemp(prefix=f"victory-pr{request['pr']}-", dir=output))
                bundle = destination / "report.zip"
                with zipfile.ZipFile(bundle, "x", zipfile.ZIP_DEFLATED, compresslevel=1) as out:
                    for path in sorted(results.iterdir()):
                        out.write(path, path.name)
                heading = "REPORT READY" if complete else "TEST INTERRUPTED"
                instruction_block(heading, "Keep this full troubleshooting archive on your machine.", f"Local logs: {bundle}")
                report = diagnostic_summary(request, results, windows, complete, destination)
                summary_path = destination / "summary.txt"
                summary_path.write_text(report, encoding="utf-8")
                share = destination / "share"
                share.mkdir()
                filtered = share / (destination.name + "-diagnostics.zip")
                with zipfile.ZipFile(filtered, "x", zipfile.ZIP_DEFLATED) as out:
                    for name in OUTGOING:
                        # Fixed ZIP metadata avoids recording local times or file modes.
                        entry = zipfile.ZipInfo(name)
                        out.writestr(entry, (destination / name).read_bytes(), compress_type=zipfile.ZIP_DEFLATED)
                share_result(request["pr"], filtered, options.review_kit)


if __name__ == "__main__":
    try:
        run()
    except (OSError, ValueError, TypeError, KeyError, EOFError, subprocess.CalledProcessError, tarfile.TarError, zipfile.BadZipFile) as error:
        sys.exit(str(error))
