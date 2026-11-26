"""Local hardware-test preferences and automatic, per-launch stopping conditions."""
import ast
import configparser
import gzip
import io
import json
from pathlib import Path
import os
import re
import shutil
import signal
import subprocess
import tarfile
import tempfile
import time

MARKERS = {
    "device-ready": re.compile(rb"victory_gpu::host:\s+host D3D12 device ready\b"),
    "present-1024": re.compile(rb"victory_gpu::queue:\s+(?:title requested a present|frame presented) [^\n]*\bframe=1024\b"),
    "timing-window": re.compile(rb"victory_gpu::host:\s+present intervals: [1-9][0-9]* frames, mean "),
    "first-mesh-bind": re.compile(rb'victory_gpu::[\w:]+:\s+first bind [^\n]*\bstage="mesh"'),
}
ANSI = re.compile(rb"\x1b\[[0-9;]*m")
REASONS = {"condition reached", "timeout before completion", "exited before completion",
           "manual scenario", "window closed before completion", "exception before completion"}


def stopping(marker, observe, timeout):
    if marker not in MARKERS or type(observe) is not int or type(timeout) is not int:
        raise ValueError("Invalid hardware-test stopping condition.")
    if not (0 <= observe < timeout <= 86400):
        raise ValueError("Use 0 <= observe_seconds < timeout_seconds <= 86400.")
    return {"marker": marker, "observe_seconds": observe, "timeout_seconds": timeout}


def preferences(path):
    cfg = configparser.ConfigParser(interpolation=None)
    try:
        with path.open(encoding="utf-8-sig") as f:
            cfg.read_file(f)
    except configparser.Error as error:
        raise ValueError(f"Invalid hardware-test configuration at {path}.") from error
    allowed = {"game_dir", "title", "proton_dir", "wine_prefix", "umu_dir", "save_dir", "output_dir"}
    if cfg.defaults() or cfg.sections() != ["hardware"] or set(cfg["hardware"]) - allowed - {
            "stop_after", "observe_seconds", "timeout_seconds"}:
        raise ValueError(f"Unrecognized settings in {path}. See .victoryconfig.example.")
    section = cfg["hardware"]
    paths = {k: (path.parent / Path(section[k]).expanduser()).resolve()
             for k in allowed if section.get(k, "").strip()}
    if "game_dir" in paths and "title" not in paths:
        paths["title"] = paths["game_dir"] / "game_scarlett_master.exe"
    marker = section.get("stop_after", "manual")
    if marker != "manual":
        paths["stop"] = stopping(marker, section.getint("observe_seconds", 180),
                                 section.getint("timeout_seconds", 900))
    return paths


def report_headers(report):
    if report.is_symlink():
        return
    if report.is_dir():
        for log in report.glob("steam-*.log"):
            if log.is_file() and not log.is_symlink():
                with log.open("rb") as stream:
                    yield stream.read(32768)
    elif report.name.endswith(".tar.gz") and report.is_file():
        # Completed runs usually contain build.txt and a short launcher.log before the
        # Proton header. Limit decompression even when a previous report is gigabytes long.
        with gzip.open(report, "rb") as stream:
            prefix = io.BytesIO(stream.read(128 * 1024))
        with tarfile.open(fileobj=prefix, mode="r:") as archive:
            for member in archive:
                if member.isfile() and re.fullmatch(r"(?:\./)?steam-[^/]+\.log", member.name):
                    prefix.seek(member.offset_data)
                    yield prefix.read(min(member.size, 32768))
                    return


def previous_title():
    """Recover a live title path from recent local Proton reports."""
    home = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local/share")
    if not home.is_absolute():
        home = Path.home() / ".local/share"
    reports = home / "victory/reports"
    for report in sorted(reports.glob("run-*"), reverse=True)[:32]:
        try:
            for header in report_headers(report):
                for line in header.splitlines():
                    if not line.startswith(b"Command: "):
                        continue
                    args = ast.literal_eval(line[len(b"Command: "):].decode())
                    if not (isinstance(args, list) and len(args) >= 3
                            and all(isinstance(arg, str) for arg in args[:3])
                            and args[0].replace("\\", "/").split("/")[-1].lower() == "victory.exe"
                            and args[1] == "run"):
                        continue
                    title = args[2]
                    if title[:3].upper() == "Z:\\":
                        title = title[2:].replace("\\", "/")
                    candidate = Path(title)
                    if (candidate.is_absolute() and candidate.name.lower() == "game_scarlett_master.exe"
                            and candidate.is_file()):
                        return candidate
        except (OSError, EOFError, tarfile.TarError, ValueError, SyntaxError, RecursionError):
            continue
    return None


def title_record():
    if os.name == "nt":
        base = Path(os.environ.get("LOCALAPPDATA") or Path.home() / "AppData/Local")
    else:
        base = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state")
        if not base.is_absolute():
            base = Path.home() / ".local/state"
    return base / "victory" / "hardware-test-title.json"


def remember_title(title):
    record = title_record()
    record.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=record.parent, delete=False) as stream:
        temp = Path(stream.name)
        try:
            json.dump(str(title), stream)
            stream.close()
            os.replace(temp, record)
        finally:
            temp.unlink(missing_ok=True)


def discover_title():
    name = "game_scarlett_master.exe"
    configured = os.environ.get("VICTORY_GAME_DIR")
    if configured:
        return Path(configured).expanduser() / name
    try:
        with title_record().open(encoding="utf-8") as stream:
            previous = Path(json.loads(stream.read(32768)))
        if previous.is_absolute() and previous.suffix.lower() == ".exe" and previous.is_file():
            return previous
    except (OSError, TypeError, ValueError):
        pass
    for games in ("games", "Games"):
        candidate = Path.home() / games / "GTA6.Xbox.v1.0.2.1" / name
        if candidate.is_file():
            return candidate
    if os.name == "posix":
        return previous_title()
    return None


def launch_path(settings):
    current = os.environ.get("PATH", os.defpath)
    directory = (settings or {}).get("umu_dir")
    return str(directory) + os.pathsep + current if directory else current


class Tail:
    """Read only appended complete lines. Bound memory and work per poll even for driver dumps."""
    def __init__(self):
        self.offsets = {}

    def lines(self, path):
        try:
            f = path.open("rb")
        except FileNotFoundError:
            return
        with f:
            offset, carry = self.offsets.get(path, (0, b""))
            f.seek(offset)
            chunk = f.read(2 * 1024 * 1024)
            lines = (carry + chunk).split(b"\n")
            self.offsets[path] = (f.tell(), lines.pop()[-4096:])
            yield from (ANSI.sub(b"", line.rstrip(b"\r")) for line in lines)


def title_processes(data_dir):
    # A pinned Proton launcher stages another copy of victory.exe and can cross a container
    # session boundary. Scope to the unique save copy AND the mapped VIctory image.
    expected = os.fsencode("VICTORY_DATA_DIR=" + data_dir)
    result = []
    for proc in Path("/proc").glob("[0-9]*"):
        try:
            if proc.stat().st_uid != os.getuid() or "wine" not in os.readlink(proc / "exe").lower():
                continue
            if expected not in (proc / "environ").read_bytes().split(b"\0"):
                continue
            if any(line.lower().endswith(b"/victory.exe") for line in (proc / "maps").read_bytes().splitlines()):
                result.append(int(proc.name))
        except OSError:
            continue
    return result


def close_window(process, windows, data_dir):
    if process.poll() is not None:
        return False
    if windows:
        # Without /F, taskkill sends WM_CLOSE. Target only the process just launched.
        result = subprocess.run(["taskkill", "/PID", str(process.pid)], capture_output=True, timeout=5)
        return result.returncode == 0
    # X11 and XWayland window managers expose the owning Unix PID through EWMH.
    if os.environ.get("DISPLAY") and shutil.which("wmctrl"):
        targets = set(title_processes(data_dir))
        rows = subprocess.run(["wmctrl", "-lp"], capture_output=True, timeout=5)
        for line in rows.stdout.splitlines():
            fields = line.split(None, 4)
            if len(fields) >= 3 and fields[2].isdigit() and int(fields[2]) in targets:
                if subprocess.run(["wmctrl", "-ic", os.fsdecode(fields[0])],
                                  capture_output=True, timeout=5).returncode == 0:
                    return True
    return False


def signal_titles(data_dir, sig):
    for pid in title_processes(data_dir):
        try:
            # Bind the signal to this instance even if a concurrent exit recycles the PID.
            fd = os.pidfd_open(pid)
            try:
                if pid in title_processes(data_dir):
                    signal.pidfd_send_signal(fd, sig)
            finally:
                os.close(fd)
        except (AttributeError, OSError):
            # Older kernels may lack pidfd_send_signal. Recheck the per-run identity.
            if pid in title_processes(data_dir):
                try:
                    os.kill(pid, sig)
                except ProcessLookupError:
                    pass


def terminate_run(process, windows, env):
    if process.poll() is not None:
        return
    if windows:
        process.kill()
    else:
        # Give Proton time to collect the guest exit and archive the logs.
        for sig, grace in ((signal.SIGTERM, 20), (signal.SIGKILL, 10)):
            signal_titles(env["VICTORY_DATA_DIR"], sig)
            try:
                process.wait(timeout=grace)
                return
            except subprocess.TimeoutExpired:
                pass
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    process.wait()


def monitor(process, log, windows, env, stop, close_file):
    tail = Tail()
    report_dir = None
    home = Path(env.get("XDG_DATA_HOME") or Path.home() / ".local/share")
    reports = (home if home.is_absolute() else Path.home() / ".local/share") / "victory/reports"
    start = time.monotonic()
    deadline = start + stop["timeout_seconds"]
    matched = None
    cooperative = False
    early = "exited before completion"

    def observe(line):
        nonlocal matched, cooperative, early
        if b"VIctory: unhandled exception 0x" in line:
            early = "exception before completion"
        elif b"window closed: suspending the title" in line and early != "exception before completion":
            early = "window closed before completion"
        if matched is None and MARKERS[stop["marker"]].search(line):
            matched = time.monotonic()
        if b"victory_hle::modules::lifecycle: hardware test close available" in line:
            cooperative = True

    while process.poll() is None:
        for line in tail.lines(log):
            if windows:
                observe(line)
            elif report_dir is None and line.startswith(b"Recording this run in "):
                path = Path(os.fsdecode(line[len(b"Recording this run in "):]))
                if path.parent.resolve() == reports.resolve() and path.name.startswith("run-"):
                    report_dir = path
        if report_dir:
            for path in [report_dir / "launcher.log", *report_dir.glob("steam-*.log")]:
                for line in tail.lines(path):
                    observe(line)
        if report_dir and (report_dir / "launcher-exit.txt").is_file():
            # UMU has exited; the wrapper can still be creating its archive.
            return early, False, report_dir
        now = time.monotonic()
        if early == "exited before completion" and matched is not None and matched + stop["observe_seconds"] <= min(now, deadline):
            reason = "condition reached"
            break
        if now >= deadline:
            reason = "timeout before completion"
            break
        time.sleep(0.25)
    else:
        return early, False, report_dir
    print(f"Hardware test: {reason}. Closing this build.", flush=True)
    if cooperative:
        close_file.touch()
        try:
            process.wait(timeout=30)
            return reason, False, report_dir
        except subprocess.TimeoutExpired:
            pass
    try:
        graceful = close_window(process, windows, env["VICTORY_DATA_DIR"])
    except (OSError, subprocess.TimeoutExpired):
        graceful = False
    if graceful:
        try:
            process.wait(timeout=30)
            return reason, False, report_dir
        except subprocess.TimeoutExpired:
            pass
    terminate_run(process, windows, env)
    return reason, True, report_dir
