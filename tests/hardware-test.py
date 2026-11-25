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


runner = load("run-test")
prepare = load("prepare")


class HardwareRequest(unittest.TestCase):
    def setUp(self):
        work = tempfile.TemporaryDirectory()
        self.addCleanup(work.cleanup)
        self.root = Path(work.name).resolve()
        self.package = self.root / runner.PACKAGE
        self.revision = "a" * 40
        with tarfile.open(self.package, "w:gz") as bundle:
            for name in ("victory.exe", "victory-proton.sh", "LICENSE", "REVISION"):
                data = self.revision.encode() if name == "REVISION" else b"fixture"
                member = tarfile.TarInfo("victory/" + name)
                member.size = len(data)
                bundle.addfile(member, io.BytesIO(data))
        self.build = {"run": 12, "revision": self.revision,
                      "sha256": hashlib.sha256(self.package.read_bytes()).hexdigest()}
        self.request = {"version": 1, "pr": 7, "scenario": "Close the window.",
                        "main": self.build.copy(), "candidate": self.build.copy()}
        self.manifest = self.root / "request.json"
        self.manifest.write_text(json.dumps(self.request))
        self.title = self.root / "space & title.exe"
        self.title.touch()
        self.output = self.root / "out"

    def fake_download(self, args, check):
        self.assertEqual(args[:6], ["gh", "run", "download", "12", "-R", runner.REPO])
        shutil.copy(self.package, Path(args[-1]) / runner.PACKAGE)

    def test_verify_both_packages(self):
        with mock.patch.object(subprocess, "run", side_effect=self.fake_download) as gh:
            for side in ("main", "candidate"):
                runner.download(self.request, side, self.root / side)
                self.assertEqual((self.root / side / "REVISION").read_text(), self.revision)
            self.assertIn("pr-7-", gh.call_args.args[0][7])
            self.request["main"]["sha256"] = "0" * 64
            with self.assertRaisesRegex(ValueError, "checksum"):
                runner.download(self.request, "main", self.root / "bad")
            self.request["candidate"]["revision"] = "b" * 40
            with self.assertRaisesRegex(ValueError, "different revision"):
                runner.download(self.request, "candidate", self.root / "wrong")

    def test_preview_offline(self):
        with mock.patch.object(sys, "argv", ["runner", str(self.manifest), "--plan"]), \
                mock.patch.object(shutil, "which", side_effect=AssertionError), \
                contextlib.redirect_stdout(io.StringIO()) as out:
            runner.run()
        self.assertIn("Close the window.", out.getvalue())
        self.assertIn("actions/runs/12", out.getvalue())

    def run_pair(self, launch):
        args = ["runner", str(self.manifest), "--title", str(self.title), "--output-dir", str(self.output)]
        with mock.patch.object(sys, "argv", args), mock.patch.object(shutil, "which", return_value="tool"), \
                mock.patch.object(subprocess, "run", side_effect=self.fake_download), \
                mock.patch.object(runner, "launch", side_effect=launch), contextlib.redirect_stdout(io.StringIO()):
            runner.run()

    def test_fresh_pair_and_partial(self):
        def launched(side, folder, title, results, windows):
            self.assertEqual(title, self.title)
            (results / (side + ".log")).write_text(side)

        self.run_pair(launched)
        self.run_pair(launched)
        bundles = list(self.output.glob("*/report.zip"))
        self.assertEqual(len(bundles), 2)
        with zipfile.ZipFile(bundles[0]) as archive:
            self.assertEqual(set(archive.namelist()), {"request.json", "main.log", "candidate.log"})

        def fail(side, *args):
            if side == "candidate":
                raise ValueError("test interruption")
            launched(side, *args)

        with self.assertRaisesRegex(ValueError, "interruption"):
            self.run_pair(fail)
        partial, = set(self.output.glob("*/report.zip")) - set(bundles)
        with zipfile.ZipFile(partial) as archive:
            self.assertEqual(set(archive.namelist()), {"request.json", "main.log"})

    def test_platform_commands_and_cache_paths(self):
        for windows in (False, True):
            with self.subTest(windows=windows):
                results = self.root / ("windows" if windows else "linux")
                results.mkdir()
                report = self.root / "current.tar.gz"
                report.write_bytes(b"this launch")
                # Capture argv and environment, but exercise actual pipe/exit handling.
                popen = subprocess.Popen
                def start(args, **kwargs):
                    self.assertEqual(kwargs["cwd"], self.title.parent)
                    self.assertNotIn("VICTORY_EXE", kwargs["env"])
                    self.assertIn(str(self.title), args)
                    self.assertEqual(args[-2:], ["--console", "series-x"])
                    if windows:
                        self.assertEqual(args[:2], [str(self.root / "victory.exe"), "run"])
                        self.assertIn("--execute", args)
                    else:
                        self.assertEqual(args[:5], ["bash", str(self.root / "victory-proton.sh"),
                                                    "--report", "--runtime", "pinned"])
                        self.assertEqual(kwargs["env"]["VKD3D_SHADER_CACHE_PATH"], str(self.root / "vkd3d"))
                    return popen([sys.executable, "-c", "import sys; print(sys.argv[1]); sys.exit(42)",
                                  f"Report: {report} (launcher exit 42)"], **kwargs)
                with mock.patch.dict(os.environ, {"VICTORY_EXE": "old.exe"}), \
                        mock.patch.object(subprocess, "Popen", side_effect=start):
                    runner.launch("main", self.root, self.title, results, windows)
                self.assertEqual((results / "main-exit.txt").read_text().strip(), "42")
                if not windows:
                    self.assertEqual((results / "main.tar.gz").read_bytes(), b"this launch")

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
