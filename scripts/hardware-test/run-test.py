"""Find and run a hardware comparison for VIctory. Python 3.9+."""
import argparse
from collections import Counter
import csv
from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import zipfile

import observations
import report_keys
import report_ui

REPO = report_keys.REPO
PACKAGE = "victory-proton-x86_64.tar.gz"
# Stamped into the official extracted runner. A loose request cannot supply this value.
PUBLISHED_SHA256 = None
ROOT = Path(__file__).resolve().parent
TITLE_ID = "7952987A"
OUTGOING = ("summary.txt", "main-frames.csv", "candidate-frames.csv", "main-submissions.csv",
            "candidate-submissions.csv", "main-events.csv", "candidate-events.csv")
REVIEWER = "victory-report-review" + (".exe" if sys.platform == "win32" else "")
PRIVACY_HEADING = report_ui.WARNING
LOG_PRIVACY = "Raw logs can include your username, file paths, credentials, or output from the game."

ANSI = re.compile(rb"\x1b\[[0-9;]*m")
MS = rb"([0-9]{1,9}\.[0-9]{2})"
INTERVAL = re.compile(rb"present intervals: ([0-9]{1,4}) frames, mean " + MS + rb" ms, median " + MS
                      + rb" ms, p95 " + MS + rb" ms, longest " + MS
                      + rb" ms, ([0-9]{1,4}) over 100 ms, ([0-9]{1,4}) over 1 s$")
SUBMISSION_START = (b"%f fps, %f ms/frame (%f preparing: copies %f ms %f MB, textures %f ms %f MB; "
                    b"%f on the GPU; %f copying back %f MB), %i draws/frame, ")
SUBMISSION_END = b" (%i being made) and %i indirect"


def submission_pattern(operations, extra=b""):
    # Accept a complete numeric sample before the known renderer detail suffix.
    # Its detail text is never copied into the report.
    fmt = SUBMISSION_START + operations + SUBMISSION_END + extra
    fields = re.escape(fmt).replace(b"%f", rb"([0-9]{1,10}\.[0-9])").replace(b"%i", rb"([0-9]{1,10})")
    return re.compile(rb"(?<![0-9.+-])" + fields + rb"(?=; heap buffers |$)")


SUBMISSION = submission_pattern(b"unsubmitted %i", b"; BVH builder blocked %i")
LEGACY_SUBMISSION = submission_pattern(b"dropped %i without a host pipeline")
SUBMISSION_COLUMNS = ("fps", "ms_per_frame", "preparing_ms", "buffer_copy_ms", "buffer_copy_mb",
                      "texture_ms", "texture_mb", "gpu_wait_ms", "copy_back_ms", "copy_back_mb",
                      "draws_per_frame", "dropped_per_frame", "pipelines_being_made", "indirect_per_frame",
                      "bvh_builder_blocked_per_frame")
# vkd3d-proton reports the shader and adapter ranges at pipeline creation.
# Accept only legal lane counts, so the summary never copies driver text.
LANES = rb"(4|8|16|32|64|128)"
WAVE_RANGES = re.compile(rb"\bd3d12_device_validate_shader_meta: Required WaveSize range \[" + LANES + b", " + LANES
                         + rb"\], but supported range is \[" + LANES + b", " + LANES + rb"\]\.$")
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
    return parse_request(path.read_bytes())


def console_name(console):
    return {"series-x": "Series X", "series-s": "Series S"}[console]


def parse_request(data):
    request = json.loads(data)
    console = request.get("console", "series-x")
    version = request["version"]
    legacy = (version, console) in ((1, "series-x"), (2, "series-s")) and "save" not in request
    standard = version == 4 and request.get("save") == TITLE_ID and console in ("series-x", "series-s")
    if (type(version) is not int or not (legacy or standard)
            or type(request["pr"]) is not int or request["pr"] < 1
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
        subprocess.run(["gh", "run", "download", str(build["run"]), "-R", "github.com/" + REPO,
                        "-n", artifact, "-D", str(folder)], check=True)
        archive = folder / PACKAGE
    if digest(archive) != build["sha256"]:
        raise ValueError(f"{side} package checksum differs from the request.")
    # Copy known distribution members only. Never unpack arbitrary paths or links.
    with tarfile.open(archive, "r:gz") as bundle:
        names = ["victory.exe", "victory-proton.sh", "REVISION", "LICENSE"]
        if {"victory/proton-runner.sh", "victory/wineserver"}.intersection(bundle.getnames()):
            names.extend(("proton-runner.sh", "wineserver"))
        for name in names:
            member = bundle.getmember("victory/" + name)
            if not member.isfile():
                raise ValueError(f"Unexpected {side} package member: {name}")
            with (folder / name).open("wb") as out:
                shutil.copyfileobj(bundle.extractfile(member), out)
            if name == "wineserver":
                (folder / name).chmod(0o755)
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


def default_save():
    checkout = ROOT.parent.parent
    base = checkout if ROOT == checkout / "scripts" / "hardware-test" and (checkout / ".victory-root").is_file() else ROOT
    return base / "saves" / TITLE_ID


def has_standard_save(source):
    return source.name.upper() == TITLE_ID and any(p.is_file() for p in (source / "saves").rglob("*"))


def copy_save(source, destination):
    # Only persistent player data belongs in the test. In particular, a title's
    # temp directory may contain unrelated files or logs from an earlier run.
    def copy_entry(path, target):
        info = path.lstat()
        directory = stat.S_ISDIR(info.st_mode)
        if (not (directory or stat.S_ISREG(info.st_mode))
                or getattr(info, "st_file_attributes", 0) & stat.FILE_ATTRIBUTE_REPARSE_POINT):
            raise ValueError(f"Use a self-contained save. Unsupported entry in {path}.")
        if directory:
            target.mkdir(parents=True)
            for entry in path.iterdir():
                copy_entry(entry, target / entry.name)
            shutil.copystat(path, target)
        else:
            shutil.copy2(path, target)
        # Keep the original timestamps, including for read-only source media.
        # Each private slot must be readable and writable by its new owner.
        mode = stat.S_IMODE(info.st_mode) | stat.S_IRUSR | stat.S_IWUSR
        target.chmod(mode | stat.S_IXUSR if directory else mode)

    for name in ("saves", "storage"):
        path = source / name
        if name == "saves" or path.exists():
            copy_entry(path, destination / name)


def native_path(path, windows):
    return str(path) if windows else "Z:" + str(path).replace("/", "\\")


def launch_command(folder, title, windows, console, mode, runtime):
    args = [str(title), "--console", console]
    if windows:
        return [str(folder / "victory.exe"), "run", str(title), "--execute", "--console", console]
    return ["bash", str(folder / "victory-proton.sh"), mode, "--runtime", runtime, *args]


def capture_launch(command, cwd, env, log_path, find_report=False):
    report = None
    with log_path.open("wb") as log, subprocess.Popen(command, cwd=cwd, env=env,
                                                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT) as process:
        with process.stdout:
            for line in process.stdout:
                log.write(line)
                if find_report:
                    found = re.fullmatch(rb"Report: (.+) \(launcher exit [0-9]+\)", line.rstrip(b"\r\n"))
                    if found:
                        report = Path(os.fsdecode(found[1]))
        status = process.wait()
    return status, report


def launch(side, folder, title, results, windows, console="series-x"):
    env = os.environ.copy()
    # Ignore a developer override that could silently substitute a different build.
    env.pop("VICTORY_EXE", None)
    env["VICTORY_TRACE_PIPELINES"] = "1"
    env["VICTORY_CACHE_DIR"] = native_path(folder / "cache", windows)
    env["VICTORY_DATA_DIR"] = native_path(folder / "data", windows)
    if not windows:
        env.update(VKD3D_SHADER_CACHE_PATH=str(folder / "vkd3d"), DXVK_STATE_CACHE_PATH=str(folder / "dxvk"))
    command = launch_command(folder, title, windows, console, "--report", "pinned")
    status, report = capture_launch(command, title.parent, env, results / (side + ".log"), not windows)
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
    result = subprocess.run(["gh", "api", "--hostname", "github.com", "--paginate", f"repos/{REPO}/{endpoint}", "--jq", query],
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
    # A fresh publication can appear in the run list before GitHub's status
    # filter finds it. Check the conclusion on each returned run directly.
    runs = github_rows("actions/workflows/hardware-request.yml/runs?branch=main&event=workflow_dispatch"
                       f"&created=>={since}&per_page=100", ".workflow_runs[] | {id, status, conclusion}")
    published = (run["id"] for run in runs
                 if run["status"] == "completed" and run["conclusion"] == "success")
    available = {}
    for run_id in sorted(published, reverse=True):
        artifacts = github_rows(f"actions/runs/{run_id}/artifacts?per_page=100",
                                '.artifacts[] | select(.expired == false) | {name}')
        for artifact in artifacts:
            match = re.fullmatch(r"victory-series-[xs]-pr-([1-9][0-9]*)", artifact["name"])
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
            print("There are no open hardware test requests.")
            return None
        print("Hardware test requests:")
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
    print(f"Downloading the hardware test for PR #{number}…", flush=True)
    subprocess.run(["gh", "run", "download", str(run_id), "-R", "github.com/" + REPO, "-n", artifact, "-D", str(folder)],
                   check=True)
    manifest = folder / "request.json"
    if not manifest.is_file():
        raise ValueError("The published kit is missing its request.json.")
    request = read_request(manifest)
    if request["pr"] != number:
        raise ValueError("The published kit names a different PR.")
    expected = f"victory-{request.get('console', 'series-x')}-pr-{number}"
    if artifact != expected:
        raise ValueError("The published kit names a different console profile.")
    parents = github_rows("git/commits/" + request["candidate"]["revision"], ".parents[] | {sha}")
    if [parent["sha"] for parent in parents] != [request["main"]["revision"], pull["head"]]:
        raise ValueError(f"PR #{number} changed after this kit was built. Ask the requester to publish a fresh kit.")
    return manifest


def official_archive(path, request):
    run_id = request.get("publication_run")
    if type(run_id) is not int or run_id < 1:
        return False
    try:
        prefix = f"repos/{REPO}/actions/"
        run = report_keys.github(prefix + f"runs/{run_id}")
        if (run["path"] != ".github/workflows/hardware-request.yml" or run["head_branch"] != "main"
                or run["head_repository"]["full_name"] != REPO or run["event"] != "workflow_dispatch"
                or run["conclusion"] != "success"):
            return False
        expected = "sha256:" + digest(path)
        name = f"victory-{request.get('console', 'series-x')}-pr-{request['pr']}"
        artifacts = report_keys.github(prefix + f"runs/{run_id}/artifacts")["artifacts"]
        return any(a["name"] == name and a["digest"] == expected for a in artifacts)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError):
        return False


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
    parser.add_argument("--list", action="store_true", help="list hardware test requests")
    parser.add_argument("--output-dir", type=Path, help="parent directory for the unique report")
    parser.add_argument("--save-dir", type=Path, help=f"use a local {TITLE_ID} folder outside the default saves/ directory")
    parser.add_argument("--plan", action="store_true", help="preview the selected test without launching")
    parser.add_argument("--title", type=Path, help="use this executable instead of the file chooser")
    parser.add_argument("--local", action="store_true", help="run local extracted builds without a test request")
    parser.add_argument("--build", nargs=2, metavar=("NAME", "DIR"), action="append",
                        help="named extracted build directory for --local; repeat to compare builds")
    parser.add_argument("--clean-data", action="store_true", help="start each --local run with empty user data")
    parser.add_argument("--warm", action="store_true", help="repeat each --local build with its warmed caches")
    parser.add_argument("--keys", metavar="SCHEDULE", help="pass a VICTORY_KEYS schedule to each --local run")
    parser.add_argument("--console", choices=("series-x", "series-s"), help="console profile for --local (default Series X)")
    parser.add_argument("--runtime", choices=("pinned", "proton"), help="graphics runtime for --local on Linux (default pinned)")
    options = parser.parse_args()
    if options.local:
        if options.request or options.list or options.plan:
            parser.error("--local cannot be combined with a request, --list or --plan")
        if not options.build or not options.title:
            parser.error("--local requires at least one --build NAME DIR and an explicit --title")
        if options.clean_data == (options.save_dir is not None):
            parser.error("--local requires exactly one of --clean-data or --save-dir")
        return run_local(options)
    if options.build or options.clean_data or options.warm or options.keys is not None or options.console or options.runtime:
        parser.error("--build, --clean-data, --warm, --keys, --console and --runtime require --local")
    if options.list and options.request:
        parser.error("--list takes no request")
    options.save_dir = (options.save_dir or default_save()).expanduser().resolve()
    temp = Path(tempfile.gettempdir()).resolve()
    output = (options.output_dir or temp).expanduser().resolve()
    for directory in (temp, output):
        if directory.is_relative_to(options.save_dir):
            raise ValueError("Choose temporary and report directories outside the source save.")
    with tempfile.TemporaryDirectory(prefix="victory-request-") as temporary:
        folder = Path(temporary)
        options.trusted_digest = PUBLISHED_SHA256
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
                if path.suffix.lower() == ".zip":
                    # Inspect and unpack the same snapshot even if the source is replaced.
                    snapshot = folder / "kit.zip"
                    shutil.copyfile(path, snapshot)
                    manifest = unpack_kit(snapshot, folder, options.plan)
                    if official_archive(snapshot, read_request(manifest)):
                        options.trusted_digest = digest(manifest)
                else:
                    manifest = path
        if manifest is not None:
            # Execute tools only from our runner's directory or the official discovery path.
            # A supplied JSON or ZIP may describe game builds but cannot select an editor.
            from_picker = argument is None or re.fullmatch(r"#?[1-9][0-9]*", argument)
            options.review_kit = folder if from_picker and manifest.parent == folder else ROOT
            if from_picker and manifest.parent == folder:
                options.trusted_digest = digest(manifest)
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


def wave_range(raw):
    if b"Required WaveSize range" in raw:
        match = WAVE_RANGES.search(ANSI.sub(b"", raw).rstrip())
        if match:
            lo, hi, amin, amax = map(int, match.groups())
            if lo <= hi and amin <= amax and (hi < amin or lo > amax):
                return lo, hi, amin, amax
    return None


def present_window(raw):
    if match := INTERVAL.search(ANSI.sub(b"", raw).rstrip()):
        n, mean, median, p95, worst, over100, over1000 = match.groups()
        n, over100, over1000 = int(n), int(over100), int(over1000)
        if 1 <= n <= 4096 and over1000 <= over100 <= n:
            return n, float(mean), float(median), float(p95), float(worst), over100, over1000
    return None


def read_metrics(stream, frame_log, submission_log, timeline):
    """Count diagnostic messages and completed, non-overlapping present windows."""
    frames = windows = slow = stalls = submissions = 0
    shader_seen = False
    elapsed = longest = 0.0
    counts = {}
    wave_ranges = Counter()
    vendor = None
    for raw in stream:
        if b"present intervals:" in raw:
            if window := present_window(raw):
                n, mean, _, _, worst, over100, over1000 = window
                frame_log.writerow((windows + 1, *window))
                frames += n
                windows += 1
                timeline.record(ANSI.sub(b"", raw), "present window", sample=windows)
                elapsed += n * mean
                longest = max(longest, worst)
                slow += over100
                stalls += over1000
        elif b" ms/frame (" in raw:
            line = ANSI.sub(b"", raw).rstrip()
            if match := SUBMISSION.search(line):
                fields = match.groups()
            elif match := LEGACY_SUBMISSION.search(line):
                fields = (*match.groups(), b"")
            else:
                continue
            submission_log.writerow(value.decode("ascii") for value in fields)
            submissions += 1
            timeline.record(line, "submission sample", sample=submissions)
        elif b"Required WaveSize range" in raw:
            if limits := wave_range(raw):
                wave_ranges[limits] += 1
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
    return rows, bool(frames or submissions or counts or vendor or shader_seen), wave_ranges


def side_metrics(results, side, windows, frame_log, submission_log, timeline, proton_logs=None):
    console = results / (side + ".log")
    if not console.is_file():
        return ["No console log recorded"]
    rows = []
    recognized = False
    waves = Counter()

    def consume(stream):
        nonlocal rows, recognized, waves
        # Timing and driver output may be split or mirrored. Use one log per
        # measurement and prefer the log with VIctory timings when it has both.
        if not recognized:
            rows, recognized, found = read_metrics(stream, frame_log, submission_log, timeline)
            if found and (recognized or not waves):
                waves = found
        elif not waves:
            waves = Counter(limits for line in stream if (limits := wave_range(line)))

    if proton_logs is not None:
        # --run writes directly to the console and may also have a Proton log.
        # Use the source with present windows first, then look for driver messages.
        logs = [console, *proton_logs]
        if proton_logs:
            for index, path in enumerate(logs):
                with path.open("rb") as stream:
                    if any(b"present intervals:" in line and present_window(line) for line in stream):
                        logs.insert(0, logs.pop(index))
                        break
        for path in logs:
            with path.open("rb") as stream:
                consume(stream)
            if recognized and waves:
                break
    elif not windows:
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
                                consume(stream)
                            if recognized and waves:
                                break
            except (OSError, EOFError, tarfile.TarError):
                pass
    if proton_logs is None and not (recognized and waves):
        with console.open("rb") as stream:
            consume(stream)
    for (lo, hi, amin, amax), count in sorted(waves.items()):
        rows.append(f"Wave-size rejection messages: {count} (shader {lo}–{hi} lanes, adapter {amin}–{amax})")
    return rows


def measurement_summary(results, side, windows, destination, shader_ids, proton_logs=None):
    exit_path = results / (side + "-exit.txt")
    status = exit_path.read_bytes().strip() if exit_path.is_file() else b""
    code = str(int(status)) if re.fullmatch(rb"-?[0-9]{1,10}", status) else "unreported"
    with (destination / (side + "-frames.csv")).open("x", encoding="ascii", newline="") as frames, \
            (destination / (side + "-submissions.csv")).open("x", encoding="ascii", newline="") as samples, \
            (destination / (side + "-events.csv")).open("x", encoding="ascii", newline="") as events:
        frame_log = csv.writer(frames)
        submission_log = csv.writer(samples)
        submission_log.writerow(SUBMISSION_COLUMNS)
        frame_log.writerow(["window", "intervals", "mean_ms", "median_ms", "p95_ms", "longest_ms",
                            "over_100_ms", "over_1_s"])
        return [f"Launcher exit: {code}", *side_metrics(results, side, windows, frame_log, submission_log,
                                                        Timeline(events, shader_ids), proton_logs)]


def diagnostic_summary(request, results, windows, complete, destination):
    shader_ids = {}
    rows = [f"{console_name(request.get('console', 'series-x'))} hardware test | PR #{request['pr']} | " + ("Windows" if windows else "Linux/Proton"),
            "Comparison completed" if complete else "Partial comparison",
            f"Starting save: {TITLE_ID} (separate copies)",
            "Timings cover all recorded phases, including menus."]
    for side in ("main", "candidate"):
        rows.extend(["", f"{side.capitalize()} | {request[side]['revision']}"])
        rows.extend(measurement_summary(results, side, windows, destination, shader_ids))
    return "\n".join(rows) + "\n"


def encrypt_report(filtered, recipient):
    with report_keys.age_tool("age") as exe:
        return subprocess.check_output([str(exe), "--armor", "--recipient", recipient, str(filtered)],
                                       stderr=subprocess.PIPE).decode("ascii")


def send_encrypted_report(filtered, binding):
    report_keys.verify(binding)
    login = binding["requester"]["login"]
    armor = encrypt_report(filtered, binding["registration"]["key"])
    # Always create explicitly unlisted on github.com. Only the encrypted file
    # is present, including when the second request (notification) fails.
    gist = {"public": False, "files": {"report.age": {"content": armor}}}
    api = ["gh", "api", "--hostname", "github.com"]
    # The tool download and encryption can take time. Honor rotation during that interval.
    report_keys.verify(binding)
    gist_id = subprocess.run([*api, "gists", "--input", "-", "--jq", ".id"], input=json.dumps(gist),
                             capture_output=True, text=True, check=True).stdout.strip()
    if not re.fullmatch(r"[0-9a-f]{32}", gist_id):
        raise ValueError("GitHub did not return a gist receipt.")
    # Record the receipt before notification so a failed mention needs no new upload.
    receipt = f"https://gist.github.com/{gist_id}"
    print(f"Encrypted report: {receipt}", flush=True)
    notice = {"body": f"@{login} A VIctory hardware report is ready. From the repository root, run "
                      f"`python3 scripts/hardware-test/report-inbox.py receive {receipt}` (Windows: use py)."}
    result = subprocess.run([*api, f"gists/{gist_id}/comments", "--input", "-"], input=json.dumps(notice),
                            capture_output=True, text=True)
    if result.returncode:
        print(f"Send the receipt privately to @{login}. The notification was unsuccessful.")
    else:
        print(f"Notification posted for @{login}.")
    if len(armor) > report_keys.MAX_COLLECTED_CIPHER:
        print("Use the private receipt. This report exceeds the automatic collection limit.")
        return None
    return gist_id + " " + hashlib.sha256(armor.encode("ascii")).hexdigest()


def offer_collection(receipt, binding, context):
    login = binding["requester"]["login"]
    confirmed = report_ui.choose("Allow automatic collection?", context,
        [f"Your encrypted report has been uploaded for @{login}.", "",
         "Anyone can see your GitHub username and posting time on the requester's public key Gist.",
         "The report link is encrypted. The public comment contains no logs or file names."],
        [("k", "Finish privately", "Use the existing private Gist notification."),
         ("a", "Publish encrypted receipt", f"Let @{login} collect this report automatically.")], "k", "k")
    if confirmed == "a":
        try:
            registration = binding["registration"]
            report_keys.verify(binding)
            with report_keys.age_tool("age") as age:
                armor = subprocess.check_output([str(age), "--armor", "--recipient", registration["key"]],
                                                input=receipt.encode("ascii"), stderr=subprocess.PIPE).decode("ascii")
            report_keys.verify(binding)
            report_keys.github(f"gists/{registration['gist']}/comments",
                               {"body": report_keys.RECEIPT_HEADER + armor})
            print("Encrypted receipt published for automatic collection.")
        except (OSError, ValueError, KeyError, subprocess.SubprocessError):
            print(f"Automatic collection unconfirmed. Keep the private receipt for @{login}.")


def review_result(filtered, kit, own_editor=False):
    """Edit temporary copies. Replace the outgoing ZIP only after an explicit apply."""
    with tempfile.TemporaryDirectory(prefix="victory-review-", dir=filtered.parent) as temporary:
        work = Path(temporary)
        files = work / "files"
        files.mkdir()
        with zipfile.ZipFile(filtered) as archive:
            names = [name for name in OUTGOING + observations.NOTES if name in archive.namelist()]
            pictures = {name: archive.read(name) for name in observations.PICTURES if name in archive.namelist()}
            for name in names:
                (files / name).write_bytes(archive.read(name))
        if not names:
            print("Use Describe what you saw to review or remove screenshots.")
            return
        helper = kit / "review" / REVIEWER
        if helper.is_file() and not own_editor:
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
            instructions = [
                "1. Choose Open folder. Open summary.txt or a CSV with a text editor.",
                "2. Edit text or delete a file to exclude it. Save and close your editor.",
                "3. Return here and choose Apply edits. You decide whether to share afterward.",
                "", "Working folder", str(files), "",
                "Summary and observation text can be posted publicly. Manage screenshots under Describe what you saw.",
                "Working copies are removed when you leave this screen. Your original logs stay local.",
            ]
            default_action = "o" if report_ui.terminal_available() else "b"
            while True:
                action = report_ui.choose("Edit in my own editor", "Local review", instructions,
                    [("o", "Open folder", "Open the temporary working copies in your file manager."),
                     ("a", "Apply edits", "Save the files you kept to the outgoing ZIP."),
                     ("b", "Back without applying", "Discard temporary edits and return to the report.")], default_action, "b")
                if action != "o":
                    break
                default_action = "a"
                try:
                    if sys.platform == "win32":
                        os.startfile(files)
                    else:
                        subprocess.run(["xdg-open", str(files)], check=True, stdout=subprocess.DEVNULL,
                                       stderr=subprocess.DEVNULL)
                except (OSError, subprocess.CalledProcessError):
                    instructions[0] = "1. Open the working folder shown below in your file manager or editor."
            if action != "a":
                return False
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
            for name, data in pictures.items():
                archive.writestr(zipfile.ZipInfo(name), data, compress_type=zipfile.ZIP_DEFLATED)
        os.replace(replacement, filtered)
        return True


def report_overview(summary):
    """Summarize recognized rows. The preview always shows the exact public text."""
    rows = []
    sides = re.split(r"(?m)^(Main|Candidate) \| [a-f0-9]{40}\r?$", summary)
    if len(sides) != 5:
        return ["Choose Preview summary to read the outgoing text."] if summary else ["Public summary excluded."]
    preamble = sides[0].splitlines()
    if len(preamble) >= 2:
        rows.append(preamble[1] + "  ·  Timings include menus")
    for name, part in zip(sides[1::2], sides[2::2]):
        mean = re.search(r"(?m)^Mean: (.+)$", part)
        longest = re.search(r"(?m)^Longest: ([\d.]+) ms", part)
        stalls = re.search(r"over 1 s: (\d+)", part)
        detail = mean[1].strip() if mean else "Frame timings unavailable"
        rows.append(f"{name:<11}{detail}")
        if longest and stalls:
            rows.append(f"          Longest {longest[1]} ms  ·  {stalls[1]} over one second")
    if any("unavailable" in row for row in rows):
        rows.append("Both builds need timing samples for an FPS comparison.")
    return rows


def share_result(pr, filtered, kit, binding=None, console="series-x"):
    context = f"{console_name(console)}  ·  PR #{pr}"
    with zipfile.ZipFile(filtered) as archive:
        initial = archive.read("summary.txt").decode("utf-8") if "summary.txt" in archive.namelist() else ""
    if not sys.stdin.isatty():
        instruction_block(PRIVACY_HEADING, LOG_PRIVACY + " Keep the raw archive local. You can reply with a written summary.",
                          f"Filtered diagnostics: {filtered}")
        print(initial, flush=True)
        return
    try:
        account = subprocess.check_output(["gh", "api", "--hostname", "github.com", "user", "--jq", ".login"],
                                          text=True, stderr=subprocess.DEVNULL, timeout=5).strip()
    except (OSError, subprocess.SubprocessError):
        account = None
    login = binding["requester"]["login"] if binding else None
    available = False
    reason = (f"Private delivery unavailable. @{login} has no verified key for this kit." if login
              else "Private delivery unavailable. Use a published requester-bound kit.")
    if binding and binding["registration"] and account:
        try:
            report_keys.verify(binding)
            available = True
        except (OSError, ValueError, KeyError, subprocess.SubprocessError):
            reason = f"Private delivery unavailable. Could not verify @{login}’s published key. Keep this ZIP local."
    status = "Check for usernames, paths and credentials before sharing."
    public_status = None
    private_status = None
    try:
        while True:
            with zipfile.ZipFile(filtered) as archive:
                names = archive.namelist()
                report = archive.read("summary.txt").decode("utf-8") if "summary.txt" in names else ""
                if report.strip():
                    for side, note in zip(observations.SIDES, observations.NOTES):
                        if note in names:
                            text = archive.read(note).decode("utf-8").strip()
                            if text:
                                report += f"\n\n{side.capitalize()} observation\n{text}"
            folder_mode = not (kit / "review" / REVIEWER).is_file()
            actions = [("k", "Keep local and finish", "Close the report. Your ZIP stays in the temporary folder printed on exit.")]
            if names:
                actions.append(("r", "Edit in my own editor" if folder_mode else "Review and edit files",
                                "Edit or exclude outgoing files. Applying your edits saves them locally."))
            actions.append(("o", "Describe what you saw", "Add main and candidate notes or paste screenshots in a local browser window."))
            if names and not folder_mode:
                actions.append(("e", "Edit in my own editor", "Open working copies, edit or remove files, then apply locally."))
            if report.strip():
                actions.append(("v", "Preview summary", "Read the exact summary that can appear on the public PR."))
            if available and names and private_status is None:
                actions.append(("i", f"Send encrypted diagnostics to @{login}",
                                f"Send encrypted diagnostics to @{login}. Review the account and included files first."))
            if report.strip() and account and public_status is None:
                actions.append(("p", "Post public summary", "Preview and confirm the text before posting under your GitHub account."))
            body = [public_status or "Public summary: not posted", private_status or "Encrypted diagnostics: not sent",
                    "", status, *report_overview(report), "",
                    f"{len(names)} outgoing files  ·  Raw logs stay local",
                    "Describe what you saw to add observations and screenshots."]
            if not available and (account or not binding or not binding["registration"]):
                body.append(reason)
            if not account:
                body.append("Sharing requires GitHub CLI signed in to github.com. Local review is available.")
            action = report_ui.choose("Hardware test results", context, body, actions, "k", "k")
            if action in ("r", "e"):
                try:
                    changed = review_result(filtered, kit, own_editor=True) if action == "e" else review_result(filtered, kit)
                    status = "Edits saved locally. Choose a destination when ready." if changed else "Review cancelled. Your outgoing ZIP is unchanged."
                except (OSError, ValueError, subprocess.SubprocessError) as error:
                    status = f"Could not apply edits. Your ZIP is unchanged. {error}"
                continue
            if action == "v":
                report_ui.choose("Public summary preview", context, report.splitlines(),
                                 [("b", "Back to results", "Return to the report to edit, save or share.")], "b", "b")
                continue
            if action == "p":
                confirmed = report_ui.choose("Post public summary?", context,
                    [f"Account: {account}", f"Public destination: https://github.com/{REPO}/pull/{pr}",
                     "This posts text only. Screenshots and diagnostic files stay local until you send them privately.",
                     "", *report.splitlines()],
                    [("b", "Back to results", "Return without posting."),
                     ("o", "Describe what you saw", "Add observations or paste a private screenshot before posting."),
                     ("y", "Post this summary", "Anyone who can see the PR can read this exact text.")], "b", "b")
                if confirmed == "o":
                    action = "o"
                elif confirmed != "y":
                    continue
            if action == "p":
                try:
                    subprocess.run(["gh", "pr", "comment", str(pr), "-R", "github.com/" + REPO, "--body-file", "-"],
                                   input=report.encode("utf-8"), check=True)
                    public_status = f"Public summary: posted to PR #{pr}"
                except (OSError, subprocess.CalledProcessError):
                    public_status = "Public summary: posting unconfirmed. Check the PR."
                continue
            if action == "o":
                try:
                    observations.edit(filtered)
                except (ValueError, OSError) as error:
                    status = f"Could not open the observations editor. {error}"
                continue
            if action == "i":
                confirmed = report_ui.choose(f"Send encrypted diagnostics to @{login}", context,
                    [f"Recipient: @{login}", f"GitHub account: {account}", "", "Included files", *names, "",
                     "The runner encrypts these files locally, then uploads ciphertext to an unlisted GitHub Gist.",
                     "GitHub sees your account, upload time and size. Anyone with the link can retrieve the ciphertext."],
                    [("b", "Back to results", "Return without sending."),
                     ("y", "Encrypt and send", "Only the requester's report key can decrypt these files.")], "b", "b")
                if confirmed != "y":
                    continue
                try:
                    receipt = send_encrypted_report(filtered, binding)
                    private_status = f"Encrypted diagnostics: uploaded for @{login}"
                    if receipt:
                        offer_collection(receipt, binding, context)
                except (ValueError, KeyError):
                    available = False
                    reason = "Private delivery unavailable. Key verification failed. Keep this ZIP local."
                    continue
                except (OSError, subprocess.SubprocessError):
                    private_status = "Encrypted diagnostics: delivery unconfirmed. Check your Gists."
                continue
            break
    except (EOFError, KeyboardInterrupt):
        pass
    print(public_status or "Public summary: not posted", private_status or "Encrypted diagnostics: not sent",
          f"Filtered diagnostics: {filtered}", sep="\n", flush=True)


def outside_checkout(*directories):
    for directory in directories:
        if any((parent / ".victory-root").exists() for parent in (directory, *directory.parents)):
            raise ValueError("Use a game, temporary directory and report destination outside the VIctory checkout.")


def regular_file(path):
    try:
        info = path.lstat()
    except FileNotFoundError:
        return False
    return (stat.S_ISREG(info.st_mode)
            and not getattr(info, "st_file_attributes", 0) & stat.FILE_ATTRIBUTE_REPARSE_POINT)


def local_round(folder, title, metadata, cache, scratch, result, source, windows):
    if ((folder / "REVISION").read_text(encoding="ascii").strip() != metadata["revision"]
            or digest(folder / "victory.exe") != metadata["binary_sha256"]):
        raise ValueError(f"{metadata['name']}: the build changed during the comparison.")
    data = scratch / "data"
    data.mkdir(parents=True)
    if source is not None:
        shutil.copytree(source, data / TITLE_ID)
    result.mkdir()
    temporary = scratch / "tmp"
    temporary.mkdir()
    (result / "case.json").write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")
    env = {key: value for key, value in os.environ.items()
           if not key.upper().startswith(("VICTORY_", "STEAM_COMPAT_"))}
    env.update(VICTORY_DATA_DIR=native_path(data, windows),
               VICTORY_CACHE_DIR=native_path(cache / "victory", windows),
               VICTORY_TRACE_PIPELINES="1", TMP=str(temporary), TEMP=str(temporary), TMPDIR=str(temporary))
    if metadata["keys"] is not None:
        env["VICTORY_KEYS"] = metadata["keys"]
    raw = scratch / "proton-logs"
    if not windows:
        raw.mkdir()
        prefix = str(scratch / "prefix")
        env.update(WINEPREFIX=prefix, STEAM_COMPAT_DATA_PATH=prefix, GAMEID="umu-victory",
                   STEAM_COMPAT_INSTALL_PATH=str(scratch),
                   STEAM_COMPAT_SHADER_PATH=str(cache / "steam-shaders"),
                   VKD3D_SHADER_CACHE_PATH=str(cache / "vkd3d"), DXVK_STATE_CACHE_PATH=str(cache / "dxvk"),
                   MESA_SHADER_CACHE_DIR=str(cache / "mesa"), __GL_SHADER_DISK_CACHE_PATH=str(cache / "nvidia"),
                   PROTON_LOG="1", PROTON_LOG_DIR=str(raw))
    command = launch_command(folder, title, windows, metadata["console"], "--run", metadata["graphics_runtime"])
    try:
        status, _ = capture_launch(command, scratch, env, result / "console.log")
        (result / "console-exit.txt").write_text(f"{status}\n", encoding="ascii")
        return status
    finally:
        logs = []
        for path in sorted(raw.glob("steam-*.log")):
            if regular_file(path):
                logs.append(result / path.name)
                shutil.copyfile(path, logs[-1])
        rows = ["Present timings can include menus. Logs do not establish gameplay or save success."]
        rows.extend(measurement_summary(result, "console", windows, result, {}, logs))
        if metadata["keys"] is not None:
            rows.append("A key schedule was requested; the runner does not verify that the build used it.")
        (result / "summary.txt").write_text("\n".join(rows) + "\n", encoding="utf-8")


def run_local(options):
    windows = sys.platform == "win32"
    if windows and options.runtime is not None:
        raise ValueError("--runtime applies only to local Linux/Proton comparisons.")
    if not windows and not sys.platform.startswith("linux"):
        raise ValueError("Local comparisons support Windows and Linux/Proton.")
    title = options.title.expanduser().resolve()
    if not title.is_file() or title.suffix.lower() != ".exe":
        raise ValueError("Select an existing game .exe with --title.")
    temp = Path(tempfile.gettempdir()).resolve()
    output = (options.output_dir or temp).expanduser().resolve()
    outside_checkout(title.parent, output, temp)
    source = options.save_dir.expanduser().resolve() if options.save_dir is not None else None
    if source is not None and not has_standard_save(source):
        raise ValueError(f"--save-dir must name a nonempty {TITLE_ID} save folder.")
    builds = []
    names = set()
    for name, directory in options.build:
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,63}", name) or name.lower() in names:
            raise ValueError("Use a distinct build name: 1–64 ASCII letters, digits, underscores or dashes, starting with a letter or digit.")
        folder = Path(directory).expanduser().resolve()
        binary, stamp = folder / "victory.exe", folder / "REVISION"
        required = [binary, stamp] if windows else [binary, stamp, folder / "victory-proton.sh"]
        if not all(regular_file(path) for path in required):
            raise ValueError(f"{name}: expected an extracted build with regular files: "
                             + ", ".join(path.name for path in required))
        revision = stamp.read_text(encoding="ascii").strip()
        if not re.fullmatch(r"[0-9a-f]{40}", revision):
            raise ValueError(f"{name}: expected a full package revision in REVISION.")
        builds.append((folder, dict(name=name, revision=revision, binary_sha256=digest(binary))))
        names.add(name.lower())
    protected = [folder for folder, _ in builds] + ([source] if source is not None else [])
    if any(title.is_relative_to(path) for path in protected):
        raise ValueError("Use a title outside the extracted builds and the source save.")
    if any(directory.is_relative_to(path) for directory in (temp, output)
           for path in [title.parent, *protected]):
        raise ValueError("Choose temporary and report directories outside the title, extracted builds and source save.")
    runtime = "native" if windows else options.runtime or "pinned"
    if runtime == "pinned":
        cache_home = Path(os.environ.get("XDG_CACHE_HOME", ""))
        if not cache_home.is_absolute():
            cache_home = Path.home() / ".cache"
        pinned_cache = (cache_home / "victory").resolve()
        outside_checkout(pinned_cache)
        if any(pinned_cache.is_relative_to(path) for path in [title.parent, *protected]):
            raise ValueError("Choose a launcher runtime cache outside the title, extracted builds and source save.")
    if not windows:
        for binary in ("bash", "umu-run"):
            if not shutil.which(binary):
                raise ValueError(f"Missing {binary}; install it before running an extracted build on Linux.")
    console = options.console or "series-x"
    instruction_block(PRIVACY_HEADING, LOG_PRIVACY + " Local comparisons do not upload results.")
    with tempfile.TemporaryDirectory(prefix="victory-local-work-", dir=temp) as work:
        scratch = Path(work)
        seed = None
        if source is not None:
            seed = scratch / "seed"
            copy_save(source, seed)
        output.mkdir(parents=True, exist_ok=True)
        destination = Path(tempfile.mkdtemp(prefix="victory-local-", dir=output))
        print(f"Private local results: {destination}", flush=True)
        status = 0
        for number, (folder, build) in enumerate(builds, start=1):
            cache = scratch / f"{number:02}-cache"
            cache_names = ("victory",) if windows else ("victory", "vkd3d", "dxvk", "steam-shaders", "mesa", "nvidia")
            for name in cache_names:
                (cache / name).mkdir(parents=True)
            rounds = ("cold", "warm") if options.warm else ("cold",)
            for cache_class in rounds:
                label = f"{number:02}-{cache_class}"
                print(f"{label} | {build['name']} | {console_name(console)}", flush=True)
                metadata = dict(build, console=console, keys=options.keys, cache_class=cache_class,
                                starting_data="clean" if seed is None else f"copied {TITLE_ID}",
                                platform="Windows" if windows else "Linux/Proton",
                                graphics_runtime=runtime)
                with tempfile.TemporaryDirectory(prefix=f"{label}-", dir=scratch) as case:
                    code = local_round(folder, title, metadata, cache, Path(case), destination / label, seed, windows)
                print(f"{label} exited ({code}).", flush=True)
                status = status or code
        return status


def run_request(manifest, options):
    data = manifest.read_bytes()
    request = parse_request(data)
    binding = request.get("delivery") if hashlib.sha256(data).hexdigest() == options.trusted_digest else None
    console = request.get("console", "series-x")
    print(f"VIctory PR #{request['pr']} | {console_name(console)} | two launches", flush=True)
    instruction_block(PRIVACY_HEADING, LOG_PRIVACY + " Your reports stay on this machine until you choose to share.")
    source = (options.save_dir or default_save()).expanduser().resolve()
    if not has_standard_save(source):
        message = (f"Standard save {TITLE_ID} missing or empty at {source}. "
                   f"Place your copy in saves/{TITLE_ID}/ or pass --save-dir pointing to your {TITLE_ID} folder.")
        if not options.plan:
            raise ValueError("Warning: " + message)
        print("Warning: " + message, file=sys.stderr)
    else:
        print(f"Using standard save {TITLE_ID}. Each build gets a separate copy.", flush=True)
    if options.plan:
        if binding:
            owner = binding["requester"]["login"]
            print(f"Private delivery: @{owner}" if binding["registration"] else
                  f"Private delivery unavailable: @{owner} has no registered key for this kit.")
        else:
            print("Private delivery unavailable for this request.")
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
    if title.parent.is_relative_to(source):
        raise ValueError("Select a game executable outside the source save folder.")
    if not title.is_file() or title.suffix.lower() != ".exe":
        raise ValueError("Select the game's .exe file.")
    temp = Path(tempfile.gettempdir()).resolve()
    output = (options.output_dir or temp).expanduser().resolve()
    outside_checkout(output, temp, title.parent)
    with tempfile.TemporaryDirectory(prefix="victory-builds-") as work:
        for side in ("main", "candidate"):
            print(f"Checking {side}…", flush=True)
            stage_build(request, side, Path(work) / side, manifest.resolve().parent / "packages")
        seed = Path(work) / "main" / "data" / TITLE_ID
        copy_save(source, seed)
        shutil.copytree(seed, Path(work) / "candidate" / "data" / TITLE_ID)
        results = Path(work) / "results"
        results.mkdir()
        shutil.copyfile(manifest, results / "request.json")
        complete = False
        try:
            for number, side in enumerate(("main", "candidate"), start=1):
                instruction_block(f"RUN {number} OF 2  |  {side.upper()}  |  WHAT TO DO", request["scenario"])
                launch(side, Path(work) / side, title, results, windows, console)
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
                share_result(request["pr"], filtered, options.review_kit, binding, console)


if __name__ == "__main__":
    try:
        sys.exit(run())
    except (OSError, ValueError, TypeError, KeyError, EOFError, subprocess.CalledProcessError, tarfile.TarError, zipfile.BadZipFile) as error:
        sys.exit(str(error))
