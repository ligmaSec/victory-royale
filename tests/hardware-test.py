"""Exercise the hardware request with synthetic CI packages and title paths."""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import struct
import time
import zlib
import tarfile
import tempfile
import unittest
from unittest import mock
import zipfile

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts" / "hardware-test"


def load(name):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


sys.path.insert(0, str(SCRIPTS))
runner = load("run-test")
prepare = load("prepare")
inbox = load("report-inbox")
keys = runner.report_keys
REQUESTER = {"id": 83, "login": "test-requester"}
BINDING = {"requester": REQUESTER, "registration": {"key": "fixture", "gist": "e" * 32}}


class SaveLocations(unittest.TestCase):
    def test_checkout_and_extracted_kit_resolve_their_own_saves(self):
        with tempfile.TemporaryDirectory() as directory:
            checkout = Path(directory) / "checkout"
            scripts = checkout / "scripts" / "hardware-test"
            scripts.mkdir(parents=True)
            (checkout / ".victory-root").touch()
            kit = Path(directory) / "kit"
            home = mock.patch.object(Path, "home", return_value=kit)
            roots = mock.patch.object(runner, "checkout_roots", return_value=[])
            home.start()
            roots.start()
            self.addCleanup(home.stop)
            self.addCleanup(roots.stop)
            with mock.patch.object(runner, "ROOT", scripts):
                self.assertEqual(runner.default_save(), checkout / "saves" / "7952987A")
            with mock.patch.object(runner, "ROOT", kit):
                self.assertEqual(runner.default_save(), kit / "saves" / "7952987A")


class PathRecovery(unittest.TestCase):
    def test_remember_selection_across_kits(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            title = root / "select once.exe"
            title.touch()
            record = root / "private" / "title.json"
            with mock.patch.object(runner.session, "title_record", return_value=record), \
                    mock.patch.object(Path, "home", return_value=root), \
                    mock.patch.dict(os.environ, {"VICTORY_GAME_DIR": "", "XDG_DATA_HOME": str(root)}):
                runner.session.remember_title(title)
                self.assertEqual(runner.session.discover_title(), title)
                title.unlink()
                self.assertIsNone(runner.session.discover_title())

    @unittest.skipUnless(os.name == "posix", "Proton reports contain Unix paths")
    def test_recover_only_a_live_title_from_a_victory_run(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            title = root / "Games with spaces" / "game_scarlett_master.exe"
            title.parent.mkdir()
            title.touch()
            log = root / "victory/reports/run-01/steam-victory.log"
            log.parent.mkdir(parents=True)
            args = ["Z:" + '\\' + "kit" + '\\' + "victory.exe", "run",
                    "Z:" + str(title).replace("/", '\\')]
            with mock.patch.dict(os.environ, {"XDG_DATA_HOME": temp}):
                for bad in (["other.exe", *args[1:]], [args[0], "inspect", args[2]], args[:1], None):
                    log.write_text("Command: " + repr(bad))
                    self.assertIsNone(runner.session.previous_title())
                log.write_text("prefix" + '\n' + "Command: " + repr(args) + '\n')
                self.assertEqual(runner.session.previous_title(), title)
                with tarfile.open(log.parent.with_suffix(".tar.gz"), "w:gz") as archive:
                    archive.add(log, arcname="./steam-victory.log")
                shutil.rmtree(log.parent)
                self.assertEqual(runner.session.previous_title(), title)
                title.unlink()
                self.assertIsNone(runner.session.previous_title())


class HardwareRequest(unittest.TestCase):
    def setUp(self):
        # Launch tests capture their output and must never prompt the person running the suite.
        # Interactive cases supply their own input or create a real pseudoterminal.
        stdin = mock.patch.object(sys, "stdin", io.StringIO())
        stdin.start()
        self.addCleanup(stdin.stop)
        work = tempfile.TemporaryDirectory()
        self.addCleanup(work.cleanup)
        self.root = Path(work.name).resolve()
        config = mock.patch.object(runner, "default_config", return_value=self.root / ".victoryconfig")
        config.start()
        self.addCleanup(config.stop)
        self.package = self.root / runner.PACKAGE
        self.revision = "a" * 40
        with tarfile.open(self.package, "w:gz") as bundle:
            for name in ("victory.exe", "victory-proton.sh", "LICENSE", "REVISION"):
                data = self.revision.encode() if name == "REVISION" else b"fixture"
                member = tarfile.TarInfo("victory/" + name)
                member.size = len(data)
                bundle.addfile(member, io.BytesIO(data))
        self.packages = self.root / "packages"
        self.packages.mkdir()
        self.build = {"run": 12, "revision": self.revision,
                      "sha256": hashlib.sha256(self.package.read_bytes()).hexdigest()}
        self.request = {"version": 1, "pr": 7, "scenario": "Close the window.",
                        "main": self.build.copy(), "candidate": self.build.copy()}
        self.manifest = self.root / "request.json"
        self.manifest.write_text(json.dumps(self.request))
        self.title = self.root / "space & title.exe"
        self.title.touch()
        self.output = self.root / "out"
        self.save = self.root / "saves" / runner.TITLE_ID
        (self.save / "saves").mkdir(parents=True)
        (self.save / "saves" / "progress").write_text("starting point")
        default = mock.patch.object(runner, "default_save", return_value=self.save)
        default.start()
        self.addCleanup(default.stop)

    def fake_download(self, args, check):
        self.assertEqual(args[:6], ["gh", "run", "download", "12", "-R", "github.com/" + runner.REPO])
        shutil.copy(self.package, Path(args[-1]) / runner.PACKAGE)

    def test_bundled_build_offline(self):
        shutil.copy(self.package, self.packages / (self.build["sha256"] + ".tar.gz"))
        with mock.patch.object(subprocess, "run", side_effect=AssertionError), \
                mock.patch.object(shutil, "which", side_effect=AssertionError):
            runner.stage_build(self.request, "main", self.root / "main", self.packages)

    def test_verify_both_packages(self):
        with mock.patch.object(subprocess, "run", side_effect=self.fake_download) as gh, \
                mock.patch.object(shutil, "which", return_value="gh"):
            for side in ("main", "candidate"):
                runner.stage_build(self.request, side, self.root / side, self.packages)
                self.assertEqual((self.root / side / "REVISION").read_text(), self.revision)
            self.assertIn("pr-7-", gh.call_args.args[0][7])
            self.request["main"]["sha256"] = "0" * 64
            with self.assertRaisesRegex(ValueError, "checksum"):
                runner.stage_build(self.request, "main", self.root / "bad", self.packages)
            self.request["candidate"]["revision"] = "b" * 40
            with self.assertRaisesRegex(ValueError, "different revision"):
                runner.stage_build(self.request, "candidate", self.root / "wrong", self.packages)

    def test_preview_offline(self):
        with mock.patch.object(sys, "argv", ["runner", str(self.manifest), "--plan"]), \
                mock.patch.object(shutil, "which", side_effect=AssertionError), \
                contextlib.redirect_stdout(io.StringIO()) as out:
            runner.run()
        self.assertIn("Close the window.", out.getvalue())
        self.assertIn("actions/runs/12", out.getvalue())

    def test_checkout_menu(self):
        with mock.patch.object(runner, "ROOT", self.root / "checkout"), \
                mock.patch.object(sys, "argv", ["runner"]), \
                mock.patch.object(sys.stdin, "isatty", return_value=True), \
                mock.patch("builtins.input", return_value=""), \
                mock.patch.object(runner, "run_request", side_effect=AssertionError), \
                mock.patch.object(runner, "github_rows") as github, contextlib.redirect_stdout(io.StringIO()) as out:
            github.side_effect = [[{"number": 7, "title": "First scene", "head": "current", "wanted": False},
                                   {"number": 8, "title": "Check lights", "head": "other", "wanted": True},
                                   {"number": 11, "title": "Other PR", "head": "other", "wanted": False}],
                                  [{"id": 10, "status": "completed", "conclusion": "success"},
                                   {"id": 12, "status": "completed", "conclusion": "success"},
                                   {"id": 14, "status": "completed", "conclusion": "failure"},
                                   {"id": 15, "status": "in_progress", "conclusion": None}], [], [{"name": "victory-series-s-pr-9"},
                                                   {"name": "victory-series-x-pr-7"}]]
            runner.run()
            self.assertIn("#7  First scene", out.getvalue())
            self.assertIn("kit available", out.getvalue())
            self.assertIn("#8  Check lights", out.getvalue())
            self.assertIn("/pull/8", out.getvalue())
            self.assertNotIn("#11", out.getvalue())
            self.assertNotIn("#9", out.getvalue())
            self.assertNotIn("status=", github.call_args_list[1].args[0])
            self.assertIn("actions/runs/12/artifacts?", github.call_args_list[2].args[0])
            self.assertIn("actions/runs/10/artifacts?", github.call_args_list[3].args[0])
            self.assertIn("hardware-request.yml/runs?branch=main&event=workflow_dispatch",
                          github.call_args_list[1].args[0])
            self.assertIn("expired == false", github.call_args_list[2].args[1])
            github.side_effect = [[]]
            runner.run()
            self.assertIn("no open hardware", out.getvalue())

    def test_request_without_main_kit(self):
        request = {8: ({"title": "Check lights", "wanted": True}, None, None)}
        with mock.patch.object(runner, "hardware_requests", return_value=request), \
                mock.patch.object(runner.subprocess, "run", side_effect=AssertionError), \
                contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertIsNone(runner.select_published(8, False, self.root))
            self.assertIn("/pull/8", out.getvalue())
            self.assertIn("no unexpired kit published from main", out.getvalue())

    def test_published_request(self):
        available = {7: ({"title": "First scene", "head": "current"}, 12, "victory-series-x-pr-7")}
        def download(args, check):
            self.assertEqual(args[:-1], ["gh", "run", "download", "12", "-R", "github.com/" + runner.REPO,
                                         "-n", "victory-series-x-pr-7", "-D"])
            shutil.copy(self.manifest, Path(args[-1]) / "request.json")
        parents = [{"sha": self.revision}, {"sha": "current"}]
        with mock.patch.object(runner, "ROOT", self.root / "checkout"), \
                mock.patch.object(sys, "argv", ["runner", "7", "--plan"]), \
                mock.patch.object(subprocess, "run", side_effect=download), \
                mock.patch.object(runner, "hardware_requests", return_value=available), \
                mock.patch.object(runner, "github_rows", return_value=parents), \
                mock.patch.object(runner, "launch", side_effect=AssertionError), \
                contextlib.redirect_stdout(io.StringIO()) as out:
            runner.run()
            self.assertIn("Close the window.", out.getvalue())
            with mock.patch.object(sys, "argv", ["runner", "--plan"]), \
                    mock.patch.object(sys.stdin, "isatty", return_value=True), \
                    mock.patch("builtins.input", return_value="7"):
                runner.run()
            parents[1]["sha"] = "old"
            with self.assertRaisesRegex(ValueError, "changed after"):
                runner.run()
            self.manifest.write_text(json.dumps(dict(self.request, pr=8)))
            with self.assertRaisesRegex(ValueError, "different PR"):
                runner.run()

    def test_explicit_preview_requires_a_kit(self):
        with mock.patch.object(sys, "argv", ["runner", "7", "--plan"]), \
                mock.patch.object(runner, "select_published", return_value=None), \
                self.assertRaisesRegex(ValueError, "no published kit to preview"):
            runner.run()

    def test_checkout_zip_and_extracted_kit_offline(self):
        archive = self.root / "Series X kit.zip"
        name = self.build["sha256"] + ".tar.gz"
        with zipfile.ZipFile(archive, "w") as kit:
            kit.write(self.manifest, "request.json")
            kit.write(self.package, "packages/" + name)
            kit.writestr("../ignored", "unused")
        def check_manifest(manifest, options):
            self.assertEqual(runner.read_request(manifest), self.request)
            runner.stage_build(self.request, "candidate", self.root / "checked", manifest.parent / "packages")
        with mock.patch.object(runner, "ROOT", self.root), \
                mock.patch.object(subprocess, "run", side_effect=AssertionError), \
                mock.patch.object(shutil, "which", side_effect=AssertionError):
            with mock.patch.object(sys, "argv", ["runner", "--plan"]), contextlib.redirect_stdout(io.StringIO()):
                runner.run()
            with mock.patch.object(sys, "argv", ["runner", str(archive)]), \
                    mock.patch.object(runner, "run_request", side_effect=check_manifest):
                runner.run()

    def run_pair(self, launch, stdout=None):
        args = ["runner", str(self.manifest), "--title", str(self.title), "--output-dir", str(self.output)]
        with mock.patch.object(sys, "argv", args), mock.patch.object(shutil, "which", return_value="tool"), \
                mock.patch.object(subprocess, "run", side_effect=self.fake_download), \
                mock.patch.object(runner, "launch", side_effect=launch), contextlib.redirect_stdout(stdout or io.StringIO()):
            runner.run()

    def test_instructions_at_launch_and_private_zip_in_temp(self):
        shutil.copy(self.package, self.packages / (self.build["sha256"] + ".tar.gz"))
        temp = self.root / "system temp with spaces"
        temp.mkdir()
        stdout = io.StringIO()

        def launched(side, folder, title, results, windows, console, **kwargs):
            profile = folder / "data" / runner.TITLE_ID
            progress = profile / "saves" / "progress"
            self.assertEqual(progress.read_text(), "starting point")
            progress.write_text(side)
            self.assertFalse((profile / "temp").exists())
            self.assertEqual((profile / "storage" / "settings").read_text(), "fixture")
            number = 1 if side == "main" else 2
            lines = stdout.getvalue().rstrip().splitlines()
            self.assertEqual(lines[-3], f"RUN {number} OF 2  |  {side.upper()}  |  WHAT TO DO")
            self.assertEqual(lines[-1], self.request["scenario"])
            (results / (side + ".log")).write_text(side)
            return True

        (self.save / "temp").mkdir()
        (self.save / "temp" / "private-log").write_text("omit")
        (self.save / "storage").mkdir()
        (self.save / "storage" / "settings").write_text("fixture")
        args = ["runner", str(self.manifest), "--title", str(self.title)]
        with mock.patch.object(sys, "argv", args), mock.patch.object(tempfile, "tempdir", str(temp)), \
                mock.patch.dict(os.environ, {"COLUMNS": "52"}), \
                mock.patch.object(shutil, "which", return_value="tool"), \
                mock.patch.object(runner, "launch", side_effect=launched), contextlib.redirect_stdout(stdout):
            runner.run()
        self.assertEqual((self.save / "saves" / "progress").read_text(), "starting point")
        report, = temp.rglob("report.zip")
        self.assertEqual(list(temp.iterdir()), [report.parent])
        self.assertIn("REPORT READY", stdout.getvalue())
        blocks = stdout.getvalue().split("REPORT READY")
        self.assertIn(runner.PRIVACY_HEADING, " ".join(blocks[0].split("RUN 1 OF 2")[0].split()))
        self.assertIn(runner.PRIVACY_HEADING, " ".join(blocks[1].split()))
        self.assertIn("written summary", " ".join(blocks[1].split()))
        self.assertIn(f"Local logs: {report}", stdout.getvalue())
        filtered, = temp.rglob("*-diagnostics.zip")
        self.assertIn(f"Filtered diagnostics: {filtered}", stdout.getvalue())
        self.assertEqual(filtered.parent.name, "share")
        self.assertEqual(list(filtered.parent.iterdir()), [filtered])
        with zipfile.ZipFile(filtered) as archive:
            self.assertEqual(set(archive.namelist()), {"summary.txt", "main-frames.csv", "candidate-frames.csv",
                                                       "main-submissions.csv", "candidate-submissions.csv",
                                                       "main-events.csv", "candidate-events.csv"})
        with zipfile.ZipFile(report) as archive:
            self.assertEqual(set(archive.namelist()), {"request.json", "main.log", "candidate.log"})

    def test_session_config_precedence_and_request_contract(self):
        cfg = self.root / "setup" / ".victoryconfig"
        cfg.parent.mkdir()
        cfg.write_text("[hardware]\ngame_dir=../my game\nsave_dir=../saves/7952987A\n"
                       "proton_dir=../Proton Engine\nwine_prefix=../prefix\n"
                       "stop_after=device-ready\nobserve_seconds=3\ntimeout_seconds=8\n")
        profile = runner.session.preferences(cfg)
        self.assertEqual(profile["title"], self.root / "my game" / "game_scarlett_master.exe")
        self.assertEqual(profile["save_dir"], self.save)
        self.assertEqual(profile["proton_dir"], self.root / "Proton Engine")
        self.assertTrue(runner.session.MARKERS["timing-window"].search(
            b"victory_gpu::host: present intervals: 128 frames, mean 25.00 ms, median 20.00 ms"))
        self.assertFalse(runner.session.MARKERS["timing-window"].search(
            b"victory_gpu::host: adapter initialization has started"))
        stop = runner.session.stopping("first-mesh-bind", 7, 55)
        request = dict(self.request, version=5, save=runner.TITLE_ID, stop=stop)
        self.manifest.write_text(json.dumps(request))
        for args, message in [([], "7 s after first-mesh-bind"), (["--manual"], None)]:
            output = io.StringIO()
            with mock.patch.object(sys, "argv", ["runner", str(self.manifest), "--config", str(cfg),
                                                  "--plan", *args]), contextlib.redirect_stdout(output):
                runner.run()
            if message:
                self.assertIn(message, output.getvalue())
            else:
                self.assertNotIn("Automatic close", output.getvalue())
        with self.assertRaises(ValueError):
            runner.parse_request(json.dumps(dict(request, version=4)))
        for invalid in [dict(stop, marker="unknown"), dict(stop, observe_seconds=True),
                        dict(stop, observe_seconds=55), dict(stop, timeout_seconds=100000)]:
            with self.assertRaises(ValueError):
                runner.parse_request(json.dumps(dict(request, stop=invalid)))

    def test_monitor_both_launch_paths_and_early_exit(self):
        # A real child produces split output then waits for its own close signal.
        # The Proton shape writes the game output only in the announced report.
        for windows in (True, False):
            for outcome in ("match", "early", "timeout"):
                with self.subTest(windows=windows, outcome=outcome):
                    folder = self.root / (str(windows) + outcome)
                    folder.mkdir()
                    console = folder / "console"
                    close = folder / "close.request"
                    report = folder / "victory/reports/run-fixture"
                    report.mkdir(parents=True)
                    code = """
import os,pathlib,sys,time
report,close,platform,outcome = sys.argv[1:]
if platform == 'proton':
    print('Recording this run in ' + report, flush=True)
    log = open(pathlib.Path(report)/'steam-victory.log','w')
else:
    log = sys.stdout
print('victory_hle::modules::lifecycle: hardware test close available', file=log, flush=True)
if outcome == 'early':
    sys.exit(5)
print('victory_gpu::host: host D3D12 device ', end='', file=log, flush=True)
time.sleep(.12)
print('ready' if outcome == 'match' else 'unavailable', file=log, flush=True)
for _ in range(100):
    if os.path.isfile(close): sys.exit(0)
    time.sleep(.05)
sys.exit(90)
"""
                    with console.open("wb") as log, subprocess.Popen(
                            [sys.executable, "-c", code, str(report), str(close),
                             "windows" if windows else "proton", outcome], stdout=log) as child:
                        reason, forced, _ = runner.session.monitor(
                            child, console, windows, {"XDG_DATA_HOME": str(folder), "VICTORY_DATA_DIR": str(folder)},
                            runner.session.stopping("device-ready", 0, 1 if outcome == "timeout" else 5), close)
                    expected = {"match": "condition reached", "early": "exited before completion",
                                "timeout": "timeout before completion"}
                    self.assertEqual((reason, forced), (expected[outcome], False))
                    self.assertEqual(child.returncode, 5 if outcome == "early" else 0)
                    self.assertEqual(close.exists(), outcome != "early")

    @unittest.skipUnless(sys.platform == "win32", "uses a native Windows Forms window")
    def test_close_legacy_windows_build_by_process(self):
        log_path = self.root / "native-window.log"
        window = ("Add-Type -AssemblyName System.Windows.Forms; "
                  "$f = New-Object Windows.Forms.Form; "
                  "$f.Add_Shown({[Console]::WriteLine('victory_gpu::host: host D3D12 device ready')}); "
                  "[Windows.Forms.Application]::Run($f)")
        with log_path.open("wb") as log, subprocess.Popen(
                ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", window],
                stdout=log, stderr=subprocess.STDOUT) as child:
            reason, forced, _ = runner.session.monitor(
                child, log_path, True, {"VICTORY_DATA_DIR": str(self.root)},
                runner.session.stopping("device-ready", 0, 30), self.root / "close.request")
        self.assertEqual((reason, forced), ("condition reached", False))

    @unittest.skipUnless(sys.platform.startswith("linux") and shutil.which("Xvfb"), "requires Xvfb")
    def test_capture_obscured_x11_game_excludes_covering_window(self):
        server = subprocess.Popen(["Xvfb", "-displayfd", "1", "-screen", "0", "400x300x24", "-nolisten", "tcp"],
                                  stdout=subprocess.PIPE)
        children = []
        try:
            display = server.stdout.readline().decode().strip()
            self.assertTrue(display.isdecimal())
            env = dict(os.environ, DISPLAY=":" + display)
            for number, color in enumerate(("25c876", "e73312")):
                ready = self.root / str(number)
                child = subprocess.Popen([sys.executable, str(Path(__file__).with_name("hardware-x11-window.py")),
                                          color, str(ready)], env=env)
                children.append(child)
                for _ in range(100):
                    if ready.exists():
                        break
                    time.sleep(.05)
                self.assertTrue(ready.exists())
            destination = self.root / "main-screenshot.png"
            subprocess.run([sys.executable, str(SCRIPTS / "window_capture.py"), str(destination),
                            str(children[0].pid)], env=env, check=True, timeout=5)
            data = destination.read_bytes()
            width, height = struct.unpack(">II", data[16:24])
            self.assertEqual((width, height), (160, 120))
            start = data.index(b"IDAT")
            size = int.from_bytes(data[start - 4:start], "big")
            rows = zlib.decompress(data[start + 4:start + 4 + size])
            self.assertEqual(rows, (bytes([0]) + bytes.fromhex("25c876") * width) * height)
            with zipfile.ZipFile(self.root / "outgoing.zip", "w") as outgoing:
                self.assertFalse(runner.observations.read_draft(outgoing)["main"]["image"])
                self.assertTrue(runner.observations.read_draft(outgoing, self.root)["main"]["image"])
                self.assertEqual(outgoing.namelist(), [])
        finally:
            for child in [*children, server]:
                child.terminate()
                child.wait()
            server.stdout.close()

    @unittest.skipUnless(sys.platform == "win32", "uses a native Windows Forms window")
    def test_capture_owned_windows_client(self):
        ready = self.root / "shown"
        destination = self.root / "captured.png"
        env = dict(os.environ, FIXTURE_READY=str(ready))
        script = ("Add-Type -AssemblyName System.Windows.Forms; "
                  "$f = New-Object Windows.Forms.Form; $f.BackColor = [Drawing.Color]::Lime; "
                  "$f.Add_Shown({[IO.File]::WriteAllText($env:FIXTURE_READY, 'ready')}); "
                  "[Windows.Forms.Application]::Run($f)")
        child = subprocess.Popen(["powershell.exe", "-NoProfile", "-Command", script], env=env)
        try:
            for _ in range(150):
                if ready.exists():
                    break
                time.sleep(.1)
            self.assertTrue(ready.exists())
            self.assertTrue(runner.session.game_screenshot(child, True, env, destination))
            verify = ("Add-Type -AssemblyName System.Drawing; $b = New-Object Drawing.Bitmap($env:FIXTURE_PNG); "
                      "$p = $b.GetPixel([int]($b.Width/2), [int]($b.Height/2)); "
                      "if ($p.G -lt 240 -or $p.R -gt 10 -or $p.B -gt 10) { exit 1 }")
            subprocess.run(["powershell.exe", "-NoProfile", "-Command", verify], check=True,
                           env=dict(env, FIXTURE_PNG=str(destination)))
        finally:
            child.kill()
            child.wait()

    def test_console_profile_requires_a_compatible_runner(self):
        self.assertEqual(runner.parse_request(json.dumps(self.request)), self.request)
        for version, console in ((1, "series-s"), (2, "series-x"), (2, "unknown"), (True, "series-x")):
            with self.subTest(version=version, console=console), self.assertRaises(ValueError):
                runner.parse_request(json.dumps(dict(self.request, version=version, console=console)))

    def test_standard_requests_name_the_save_on_both_consoles(self):
        for console in ("series-x", "series-s"):
            request = dict(self.request, version=4, save=runner.TITLE_ID, console=console)
            self.assertEqual(runner.parse_request(json.dumps(request)), request)
            for change in ({"save": "../other"}, {"version": 1}, {"console": "unknown"}):
                with self.assertRaises(ValueError):
                    runner.parse_request(json.dumps(dict(request, **change)))

    def test_fresh_pair_and_partial(self):
        self.request.update(version=2, console="series-s")
        self.manifest.write_text(json.dumps(self.request))
        def launched(side, folder, title, results, windows, console, **kwargs):
            self.assertEqual(console, "series-s")
            self.assertEqual(title, self.title)
            (results / (side + ".log")).write_text(side)
            return True

        self.run_pair(launched)
        self.run_pair(launched)
        bundles = list(self.output.glob("*/report.zip"))
        self.assertEqual(len(bundles), 2)
        with zipfile.ZipFile(bundles[0]) as archive:
            self.assertEqual(set(archive.namelist()), {"request.json", "main.log", "candidate.log"})

        def fail(side, *args, **kwargs):
            if side == "candidate":
                raise ValueError("test interruption")
            launched(side, *args)

        stdout = io.StringIO()
        with self.assertRaisesRegex(ValueError, "interruption"):
            self.run_pair(fail, stdout)
        notice = " ".join(stdout.getvalue().split("TEST INTERRUPTED")[1].split())
        self.assertIn(runner.PRIVACY_HEADING, notice)
        self.assertIn("written summary", notice)
        partial, = set(self.output.glob("*/report.zip")) - set(bundles)
        with zipfile.ZipFile(partial) as archive:
            self.assertEqual(set(archive.namelist()), {"request.json", "main.log"})

    def test_sensitive_information_banner_is_red_only_on_color_terminals(self):
        for tty, term, no_color, expected in [(True, "xterm", False, True), (False, "xterm", False, False),
                                             (True, "dumb", False, False), (True, "xterm", True, False)]:
            env = {"TERM": term, "COLUMNS": "52"}
            if no_color:
                env["NO_COLOR"] = "1"
            with mock.patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(io.StringIO()) as out, \
                    mock.patch.object(out, "isatty", return_value=tty), mock.patch.object(sys, "platform", "linux"):
                runner.instruction_block(runner.PRIVACY_HEADING, runner.LOG_PRIVACY)
            text = out.getvalue()
            self.assertEqual("\x1b[1;91m" in text, expected)
            self.assertEqual("\x1b[0m" in text, expected)
            self.assertIn(runner.PRIVACY_HEADING, text)
            self.assertIn("credentials", text)

    def test_filtered_result_keeps_only_numeric_windows_and_fixed_diagnostics(self):
        request = dict(self.request, scenario="private scenario")
        private = b"private_name private@example.test /home/private/ C:\\Users\\private 192.0.2.10 secret=value"
        window = (b"present intervals: 2 frames, mean 750.00 ms, median 750.00 ms, p95 1400.00 ms, "
                  b"longest 1400.00 ms, 1 over 100 ms, 1 over 1 s")
        submission = (b"2.0 fps, 500.0 ms/frame (400.5 preparing: copies 380.0 ms 10.5 MB, "
                      b"textures 5.0 ms 1.0 MB; 33.0 on the GPU; 4.0 copying back 2.0 MB), 123 draws/frame, "
                      b"dropped 4 without a host pipeline (2 being made) and 1 indirect")
        log = b"\n".join([private, submission, submission + private, b"host D3D12 device ready adapter=" + private + b" vendor=0x1002",
                          b"shader does not translate: " + private, window, window + b" " + private])
        for windows in (True, False):
            destination = self.root / str(windows)
            destination.mkdir()
            (self.root / "main.log").write_bytes(log if windows else private)
            (self.root / "main-exit.txt").write_text("42")
            (self.root / "candidate-exit.txt").write_bytes(private)
            if not windows:
                with tarfile.open(self.root / "main.tar.gz", "w:gz") as tar:
                    for name in ("./launcher.log", "./steam-victory.log", "./private.txt"):
                        member = tarfile.TarInfo(name)
                        member.size = len(log)
                        tar.addfile(member, io.BytesIO(log))
            body = runner.diagnostic_summary(request, self.root, windows, False, destination)
            timings = (destination / "main-frames.csv").read_text()
            self.assertIn("Partial comparison", body)
            self.assertIn("GPU vendor: AMD", body)
            self.assertIn("Mean: 750.00 ms/frame (1.33 FPS)", body)
            self.assertIn("shader refusal: 1", body)
            self.assertIn("Launcher exit: 42", body)
            self.assertIn("Launcher exit: unreported", body)
            self.assertIn("1,2,750.0,750.0,1400.0,1400.0,1,1", timings)
            submissions = (destination / "main-submissions.csv").read_text().splitlines()
            self.assertEqual(len(submissions), 2)
            self.assertEqual(submissions[-1], "2.0,500.0,400.5,380.0,10.5,5.0,1.0,33.0,4.0,2.0,123,4,2,1")
            for value in ("private", "192.0.2.10", "secret=value"):
                self.assertNotIn(value, body + timings + "".join(submissions))

    def test_proton_uses_recognized_output_with_a_nondefault_game_id(self):
        (self.root / "main.log").write_bytes(b"wrapper")
        header = b"Proton header only"
        log = b"shader does not translate: synthetic refusal"
        for steam, launcher in ((log, header), (header, log)):
            with tarfile.open(self.root / "main.tar.gz", "w:gz") as tar:
                for name, contents in (("./steam-victory.log", header), ("./steam-custom.log", steam),
                                       ("./launcher.log", launcher)):
                    member = tarfile.TarInfo(name)
                    member.size = len(contents)
                    tar.addfile(member, io.BytesIO(contents))
            with tempfile.TemporaryDirectory() as output:
                body = runner.diagnostic_summary(self.request, self.root, False, False, Path(output))
            self.assertIn("shader refusal: 1", body)
            self.assertNotIn("custom", body)

    def test_wave_refusals_keep_numeric_ranges_across_proton_logs(self):
        message = b"d3d12_device_validate_shader_meta: Required WaveSize range [64, 64], but supported range is [32, 32]."
        private = b"/home/private-device/"
        # Two real messages, plus malformed, unrelated and suffixed lines.
        waves = b"\n".join([private + b" " + message, message, message.replace(b"64", b"6789"),
                            message.replace(b"shader_meta:", b"unrelated:"), message + private])
        timing = (b"present intervals: 2 frames, mean 30.00 ms, median 30.00 ms, p95 32.00 ms, "
                  b"longest 32.00 ms, 0 over 100 ms, 0 over 1 s")
        (self.root / "main.log").write_bytes(timing)
        for steam, launcher in ((waves, timing), (waves, waves + b"\n" + timing),
                               (waves + b"\n" + timing, waves), (waves, b""), (timing, waves)):
            with tarfile.open(self.root / "main.tar.gz", "w:gz") as tar:
                for name, log in (("./steam-victory.log", steam), ("./launcher.log", launcher)):
                    member = tarfile.TarInfo(name)
                    member.size = len(log)
                    tar.addfile(member, io.BytesIO(log))
            with tempfile.TemporaryDirectory() as output:
                summary = runner.diagnostic_summary(self.request, self.root, False, True, Path(output))
                self.assertEqual(summary.count("Wave-size rejection messages"), 1)
                self.assertIn("2 (shader 64–64 lanes, adapter 32–32)", summary)
                self.assertIn("2 frame intervals", summary)
                self.assertNotIn("private-device", summary)

    def outgoing_zip(self, summary):
        path = self.root / "filtered.zip"
        with zipfile.ZipFile(path, "w") as archive:
            archive.writestr("summary.txt", summary)
            archive.writestr("main-events.csv", "private event row")
        return path

    def test_public_post_requires_preview_and_uses_only_summary(self):
        filtered = self.outgoing_zip("Previewed result")
        for tty, answers, posted in ((False, [], False), (True, [""], False),
                                     (True, ["p", "", ""], False), (True, ["p", "y", "k"], True)):
            output = io.StringIO()
            with mock.patch.object(sys.stdin, "isatty", return_value=tty), \
                    mock.patch("builtins.input", side_effect=answers), \
                    mock.patch.object(subprocess, "check_output", return_value="test-account"), \
                    mock.patch.object(subprocess, "run") as send, contextlib.redirect_stdout(output):
                runner.share_result(7, filtered, self.root)
            if not tty or posted:
                self.assertIn("Previewed result", output.getvalue())
            self.assertEqual(send.called, posted)
            if posted:
                self.assertEqual(send.call_args.kwargs["input"], b"Previewed result")
                self.assertIn("github.com/" + runner.REPO, send.call_args.args[0])

    def test_private_sharing_requires_a_pinned_key_login_and_explicit_choice(self):
        safe = self.outgoing_zip("Preview")
        for key, login, choice, sent in ((None, True, "i", False), ("configured", False, "i", False),
                                         ("configured", True, "", False), ("configured", True, "i", True)):
            with mock.patch.object(sys.stdin, "isatty", return_value=True), mock.patch.object(keys, "verify"), \
                    mock.patch.object(subprocess, "check_output", side_effect=None if login else FileNotFoundError,
                                      return_value="fixture-account"), mock.patch("builtins.input", side_effect=[choice, "y", "", "k"]), \
                    mock.patch.object(runner, "send_encrypted_report") as send, contextlib.redirect_stdout(io.StringIO()) as output:
                runner.share_result(7, safe, self.root, BINDING if key else None)
            self.assertEqual(send.called, sent)
            if sent:
                send.assert_called_once_with(safe, BINDING)
                self.assertIn("fixture-account", output.getvalue())

    def test_review_applies_exact_edits_and_ignores_unrelated_files(self):
        filtered = self.outgoing_zip("original")
        raw = self.root / "report.zip"
        raw.write_bytes(b"raw canary")
        def edit(prompt):
            files, = filtered.parent.glob("victory-review-*/files")
            self.assertEqual(sorted(x.name for x in files.iterdir()), ["main-events.csv", "summary.txt"])
            (files / "main-events.csv").unlink()
            (files / "summary.txt").write_bytes("Reviewed ☀".encode())
            (files / "editor-backup~").write_bytes(b"private")
            return "a"
        for destination in ("i", "p"):
            self.outgoing_zip("original")
            choices = iter(["r", None, destination, "y", "k", "k"])
            def choose(prompt):
                return next(choices) or edit(prompt)
            with mock.patch.object(sys.stdin, "isatty", return_value=True), \
                    mock.patch("builtins.input", side_effect=choose), \
                    mock.patch.object(subprocess, "check_output", return_value="account"), \
                    mock.patch.object(subprocess, "run") as post, \
                    mock.patch.object(runner, "send_encrypted_report") as send, contextlib.redirect_stdout(io.StringIO()):
                with mock.patch.object(keys, "verify"):
                    runner.share_result(7, filtered, self.root, BINDING)
            if destination == "i":
                send.assert_called_once_with(filtered, BINDING)
                post.assert_not_called()
            else:
                self.assertEqual(post.call_args.kwargs["input"], "Reviewed ☀".encode())
                send.assert_not_called()
            with zipfile.ZipFile(filtered) as archive:
                self.assertEqual(archive.namelist(), ["summary.txt"])
                self.assertEqual(archive.read("summary.txt"), "Reviewed ☀".encode())
            self.assertEqual(raw.read_bytes(), b"raw canary")

    def test_cancel_or_invalid_review_preserves_original_zip(self):
        filtered = self.outgoing_zip("original")
        original = filtered.read_bytes()
        for apply in ("", "a"):
            def edit(prompt):
                files, = filtered.parent.glob("victory-review-*/files")
                (files / "summary.txt").write_bytes(b"unsafe\x1b")
                return apply
            with mock.patch("builtins.input", side_effect=edit), contextlib.redirect_stdout(io.StringIO()):
                if apply:
                    with self.assertRaisesRegex(ValueError, "control characters"):
                        runner.review_result(filtered, self.root)
                else:
                    runner.review_result(filtered, self.root)
            self.assertEqual(filtered.read_bytes(), original)
            self.assertFalse(list(filtered.parent.glob("victory-review-*")))

    def test_excluding_public_summary_disables_public_post(self):
        filtered = self.outgoing_zip("original")
        def exclude(_path, _kit):
            with zipfile.ZipFile(filtered, "w") as archive:
                archive.writestr("main-events.csv", "events only")
        with mock.patch.object(sys.stdin, "isatty", return_value=True), \
                mock.patch("builtins.input", side_effect=["r", "p"]), \
                mock.patch.object(subprocess, "check_output", return_value="account"), \
                mock.patch.object(runner, "review_result", side_effect=exclude), \
                mock.patch.object(subprocess, "run", side_effect=AssertionError), contextlib.redirect_stdout(io.StringIO()) as output:
            runner.share_result(7, filtered, self.root)
        self.assertNotIn("[p]", output.getvalue().rsplit("Hardware test results", 1)[-1])

    def test_results_view_handles_missing_candidate_timings(self):
        report = ("Series X hardware test | PR #43 | Linux/Proton\nComparison completed\n"
                  f"Main | {'a' * 40}\nMean: 60.92 ms/frame (16.41 FPS)\n"
                  f"Longest: 5918.97 ms | over 100 ms: 3 | over 1 s: 1\nCandidate | {'b' * 40}\n"
                  "Frame timings: unavailable\n")
        lines = runner.report_overview(report)
        self.assertIn("16.41 FPS", "\n".join(lines))
        self.assertIn("Candidate  Frame timings unavailable", lines)
        self.assertIn("Both builds need timing samples", lines[-1])
        options = [("k", "Keep local", "Save the ZIP on this machine."), ("r", "Edit files", "Edit local copies.")]
        for width, height in [(48, 16), (80, 24), (110, 28)]:
            page, _, _ = runner.report_ui.layout("Hardware test results", "Series X", lines, options, 1, 0,
                                                 width, height, False)
            self.assertIn("› Edit files", page)
            self.assertIn("Enter Select", page)
            self.assertIn("LOGS MAY CONTAIN", page)
            self.assertLessEqual(len(page.splitlines()), height)

    @unittest.skipUnless(sys.platform.startswith("linux"), "POSIX pseudoterminal")
    def test_folder_review_is_a_full_screen_workflow(self):
        import fcntl
        import pty
        import select
        import struct
        import termios
        import time
        filtered = self.outgoing_zip("Synthetic summary")
        master, slave = pty.openpty()
        self.addCleanup(os.close, master)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 98, 0, 0))
        code = ("import runpy,sys; from pathlib import Path; from unittest.mock import patch; "
                "module=runpy.run_path('run-test.py'); "
                "patch('subprocess.check_output',return_value='fixture').start(); "
                "patch('subprocess.run',side_effect=AssertionError('unexpected network or launcher')).start(); "
                "patch('report_keys.verify').start(); "
                f"module['share_result'](43,Path(sys.argv[1]),Path(sys.argv[2]),{BINDING!r})")
        child = subprocess.Popen([sys.executable, "-B", "-c", code, str(filtered), str(self.root)], cwd=SCRIPTS,
                                 stdin=slave, stdout=slave, stderr=slave, env={**os.environ, "TERM": "xterm-256color"})
        os.close(slave)
        def finish():
            if child.poll() is None:
                child.kill()
            child.wait(timeout=5)
        self.addCleanup(finish)
        def screen(marker):
            data = b""
            deadline = time.monotonic() + 6
            while marker not in data:
                remaining = deadline - time.monotonic()
                self.assertGreater(remaining, 0, data[-500:])
                if select.select([master], [], [], remaining)[0]:
                    data += os.read(master, 65536)
            return data
        first = screen(b"Enter Select")
        self.assertIn(b"\x1b[?1049h", first)
        self.assertIn(b"Hardware test results", first)
        os.write(master, b"\x1b[B\r")
        self.assertIn(b"Open folder", screen(b"Working folder"))
        files, = filtered.parent.glob("victory-review-*/files")
        (files / "summary.txt").write_bytes(b"Edited locally")
        (files / "main-events.csv").unlink()
        os.write(master, b"a")
        screen(b"Edits saved locally")
        os.write(master, b"i")
        screen(b"Encrypt and send")
        os.write(master, b"\x1b")
        screen(b"Hardware test results")
        os.write(master, b"p")
        screen(b"Edited locally")
        os.write(master, b"\x1b")
        screen(b"Hardware test results")
        os.write(master, b"\r")
        self.assertEqual(child.wait(timeout=5), 0)
        with zipfile.ZipFile(filtered) as archive:
            self.assertEqual(archive.namelist(), ["summary.txt"])
            self.assertEqual(archive.read("summary.txt"), b"Edited locally")

    def test_observations_public_and_private_destinations(self):
        import base64
        import struct
        import zlib
        def chunk(kind, data):
            return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
        image = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0))
                 + chunk(b"tEXt", b"Comment\0private metadata")
                 + chunk(b"IDAT", zlib.compress(bytes(4))) + chunk(b"IEND", b""))
        filtered = self.outgoing_zip("timings")
        draft = {"main": {"note": "Only lights", "image": base64.b64encode(image).decode()},
                 "candidate": {"note": "Still only lights", "image": ""}}
        def observe(path):
            runner.observations.save_draft(path, draft)
        with mock.patch.object(sys.stdin, "isatty", return_value=True), mock.patch.object(keys, "verify"), \
                mock.patch("builtins.input", side_effect=["p", "o", "p", "y", "i", "y", "k"]), \
                mock.patch.object(subprocess, "check_output", return_value="tester"), \
                mock.patch.object(runner.observations, "edit", side_effect=observe), \
                mock.patch.object(runner, "send_encrypted_report", return_value=None) as send, \
                mock.patch.object(subprocess, "run") as post, contextlib.redirect_stdout(io.StringIO()) as screen:
            runner.share_result(7, filtered, self.root, BINDING)
        self.assertEqual(post.call_args.kwargs["input"], b"timings\n\nMain observation\nOnly lights"
                         b"\n\nCandidate observation\nStill only lights")
        send.assert_called_once_with(filtered, BINDING)
        self.assertIn("Public summary: posted to PR #7", screen.getvalue())
        self.assertIn("Encrypted diagnostics: uploaded for @", screen.getvalue())
        with zipfile.ZipFile(filtered) as archive:
            picture = archive.read("main-screenshot.png")
            self.assertNotIn(b"private metadata", picture)
            self.assertNotIn("candidate-screenshot.png", archive.namelist())
            self.assertEqual(archive.read("main-events.csv"), b"private event row")
        # Text review leaves the selected picture intact.
        with mock.patch("builtins.input", return_value="a"), contextlib.redirect_stdout(io.StringIO()):
            runner.review_result(filtered, self.root)
        with zipfile.ZipFile(filtered) as archive:
            self.assertEqual(archive.read("main-screenshot.png"), picture)
        original = filtered.read_bytes()
        draft["candidate"]["image"] = base64.b64encode(b"private non-image file").decode()
        with self.assertRaises(ValueError):
            runner.observations.save_draft(filtered, draft)
        self.assertEqual(filtered.read_bytes(), original)
        draft["main"]["image"] = draft["candidate"]["image"] = ""
        runner.observations.save_draft(filtered, draft)
        with zipfile.ZipFile(filtered) as archive:
            self.assertFalse(set(archive.namelist()).intersection(runner.observations.PICTURES))

    def test_gist_only_receives_ciphertext_and_a_failed_mention_preserves_the_receipt(self):
        filtered = self.root / "report.zip"
        filtered.write_bytes(b"private path and raw canary")
        gist_id = "a" * 32
        for notify_status in (0, 1):
            def github(command, **kwargs):
                self.assertEqual(command[:4], ["gh", "api", "--hostname", "github.com"])
                body = json.loads(kwargs["input"])
                if command[4] == "gists":
                    self.assertEqual(body, {"public": False, "files": {"report.age": {"content": "ciphertext"}}})
                    return subprocess.CompletedProcess(command, 0, stdout=gist_id)
                self.assertEqual(command[4], f"gists/{gist_id}/comments")
                self.assertEqual(list(body), ["body"])
                self.assertIn("@" + REQUESTER["login"], body["body"])
                self.assertNotIn("canary", body["body"])
                return subprocess.CompletedProcess(command, notify_status)
            with mock.patch.object(keys, "verify"), mock.patch.object(runner, "encrypt_report", return_value="ciphertext"), \
                    mock.patch.object(subprocess, "run", side_effect=github) as api, contextlib.redirect_stdout(io.StringIO()) as out:
                runner.send_encrypted_report(filtered, BINDING)
            self.assertEqual(api.call_count, 2)
            self.assertIn(f"https://gist.github.com/{gist_id}", out.getvalue())
            self.assertIn("unsuccessful" if notify_status else "Notification posted", out.getvalue())
        with mock.patch.object(keys, "verify"), mock.patch.object(runner, "encrypt_report", side_effect=ValueError("bad checksum")), \
                mock.patch.object(subprocess, "run", side_effect=AssertionError), self.assertRaises(ValueError):
            runner.send_encrypted_report(filtered, BINDING)

    def test_private_timeline_correlates_builds_without_exporting_raw_identifiers(self):
        import csv
        shader = "0123456789abcdef"
        for side, time in (("main", "2026-01-01T23:59:58"), ("candidate", "2026-03-04T12:00:06")):
            outcome = "shader translated" if side == "candidate" else "shader does not translate; skipped"
            logs = [f'{time}.100000Z first bind pipeline=0xabc frame=12 stage="compute" size=480 hash={shader}',
                    f'{time}.350000Z {outcome} stage="compute" hash={shader} error=/home/private/file',
                    f'{time}.400000Z starting host pipeline tid=ff shaders=[compute {shader}]',
                    f'{time}.500000Z host pipeline \x1b[32mshaders\x1b[0m=[compute {shader}]']
            (self.root / (side + ".log")).write_text("\n".join(logs))
        body = runner.diagnostic_summary(self.request, self.root, True, True, self.root)
        for side in ("main", "candidate"):
            text = (self.root / (side + "-events.csv")).read_text()
            rows = list(csv.DictReader(text.splitlines()))
            self.assertEqual([(r["event"], r["elapsed_ms"], r["shader_id"]) for r in rows],
                             [("first bind", "0", "1"), ("translated" if side == "candidate" else "translation refused",
                               "250", "1"), ("pipeline ready", "400", "1")])
            for forbidden in (shader, "2026-", "0xabc", "private"):
                self.assertNotIn(forbidden, text + body)

    def test_read_only_standard_save_gives_each_build_writable_progress(self):
        progress = self.save / "saves" / "progress"
        progress.chmod(0o444)
        progress.parent.chmod(0o555)
        self.addCleanup(progress.chmod, 0o644)
        self.addCleanup(progress.parent.chmod, 0o755)

        def write_progress(side, folder, title, results, windows, console, **kwargs):
            slot = folder / "data" / runner.TITLE_ID / "saves" / "progress"
            self.assertTrue(slot.stat().st_mode & 0o200)
            self.assertTrue(slot.parent.stat().st_mode & 0o200)
            self.assertEqual(slot.read_text(), "starting point")
            slot.write_text(side)
            (slot.parent / "next-slot").write_text(side)
            (results / (side + ".log")).write_text(side)
            return True

        self.run_pair(write_progress)
        self.assertEqual(progress.read_text(), "starting point")
        self.assertFalse(progress.stat().st_mode & 0o200)
        self.assertFalse((progress.parent / "next-slot").exists())

    def test_staging_and_reports_cannot_write_into_the_source_save(self):
        inside = self.save / "saves" / "output"
        args = ["runner", str(self.manifest), "--save-dir", str(self.save), "--title", str(self.title)]
        with mock.patch.object(sys, "argv", args + ["--output-dir", str(inside)]), \
                self.assertRaisesRegex(ValueError, "outside the source save"):
            runner.run()
        self.assertFalse(inside.exists())
        with mock.patch.object(sys, "argv", args), mock.patch.object(tempfile, "tempdir", str(self.save / "saves")), \
                mock.patch.object(tempfile, "TemporaryDirectory") as create, \
                self.assertRaisesRegex(ValueError, "outside the source save"):
            runner.run()
        create.assert_not_called()

    def test_missing_standard_save_warns_in_preview_and_stops_before_launch(self):
        args = ["runner", str(self.manifest), "--save-dir", str(self.root / "missing" / runner.TITLE_ID)]
        with mock.patch.object(sys, "argv", args + ["--plan"]), contextlib.redirect_stderr(io.StringIO()) as warning:
            runner.run()
        self.assertIn("Warning: Standard save 7952987A missing", warning.getvalue())
        with mock.patch.object(sys, "argv", args), mock.patch.object(runner, "stage_build") as stage:
            with self.assertRaisesRegex(ValueError, "saves/7952987A/"):
                runner.run()
            stage.assert_not_called()

    def test_platform_commands_and_cache_paths(self):
        for windows, console in ((False, "series-x"), (False, "series-s"),
                                 (True, "series-x"), (True, "series-s")):
            with self.subTest(windows=windows, console=console):
                results = self.root / (("windows-" if windows else "linux-") + console)
                results.mkdir()
                report = self.root / "current.tar.gz"
                report.write_bytes(b"this launch")
                # Capture argv and environment, but exercise actual pipe/exit handling.
                popen = subprocess.Popen
                def start(args, **kwargs):
                    self.assertEqual(kwargs["cwd"], self.title.parent)
                    self.assertNotIn("VICTORY_EXE", kwargs["env"])
                    expected = str(self.root / "data")
                    self.assertEqual(kwargs["env"]["VICTORY_DATA_DIR"], expected if windows else "Z:" + expected.replace("/", "\\"))
                    self.assertIn(str(self.title), args)
                    self.assertEqual(args[-2:], ["--console", console])
                    if windows:
                        self.assertEqual(args[:2], [str(self.root / "victory.exe"), "run"])
                        self.assertIn("--execute", args)
                    else:
                        self.assertEqual(args[:5], ["bash", str(self.root / "victory-proton.sh"),
                                                    "--report", "--runtime", "pinned"])
                        self.assertEqual(kwargs["env"]["VKD3D_SHADER_CACHE_PATH"], str(self.root / "vkd3d"))
                    return popen([sys.executable, "-c", "import sys; print(sys.argv[1]); sys.exit(42)",
                                  f"Report: {report} (launcher exit 42)"], **kwargs)
                with mock.patch.dict(os.environ, {"VICTORY_EXE": "old.exe", "VICTORY_DATA_DIR": "unrelated profile"}), \
                        mock.patch.object(subprocess, "Popen", side_effect=start):
                    runner.launch("main", self.root, self.title, results, windows, console)
                self.assertEqual((results / "main-exit.txt").read_text().strip(), "42")
                if not windows:
                    self.assertEqual((results / "main.tar.gz").read_bytes(), b"this launch")

    def test_main_ci_refresh_after_a_docs_only_push(self):
        prefix = "victory-proton-x86_64-"
        calls = []
        def api(path):
            calls.append(path)
            if "ci.yml/runs?" in path:
                self.assertIn("head_sha=" + self.revision, path)
                self.assertIn("branch=main", path)
                return {"workflow_runs": [
                    {"id": 1, "event": "workflow_dispatch", "head_branch": "another"},
                    {"id": 2, "event": "pull_request", "head_branch": "main"},
                    {"id": 3, "event": "push", "head_branch": "main"},
                    {"id": 12, "event": "workflow_dispatch", "head_branch": "main"},
                ]}
            if "/3/artifacts" in path:
                return {"artifacts": []}
            self.assertIn("/12/artifacts", path)
            return {"artifacts": [{"name": prefix + rev, "expired": False}
                                  for rev in ("b" * 40, self.revision)]}

        def download(args, check):
            self.fake_download(args, check)
            self.assertEqual(args[args.index("-n") + 1], prefix + self.revision)
            (Path(args[-1]) / "SHA256SUMS").write_text(self.build["sha256"] + "  " + runner.PACKAGE)
        with mock.patch.object(prepare, "api", side_effect=api), \
                mock.patch.object(subprocess, "run", side_effect=download):
            build, base, data = prepare.find_build("main", self.revision, prefix)
        self.assertEqual((build, base, data), (self.build, None, self.package.read_bytes()))
        self.assertEqual(len(calls), 3)

    def test_reject_wrong_merge_parent_and_expired_builds(self):
        artifact = {"name": "pr-7-" + self.revision, "expired": False}
        for expired in (False, True):
            artifact["expired"] = expired
            replies = [{"workflow_runs": [{"id": 12}]}, {"artifacts": [artifact]}]
            if not expired:
                replies.append({"parents": [{"sha": "other"}, {"sha": "wrong head"}]})
            with mock.patch.object(prepare, "api", side_effect=replies), \
                    mock.patch.object(subprocess, "run", side_effect=AssertionError), \
                    self.assertRaisesRegex(ValueError, "No unexpired successful"):
                prepare.find_build("pull_request", self.revision, "pr-7-")


if __name__ == "__main__":
    unittest.main()
