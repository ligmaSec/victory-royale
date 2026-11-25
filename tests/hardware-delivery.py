"""Exercise requester registration, kit publication and private delivery with synthetic identities."""
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
import tempfile
import unittest
from unittest import mock
import zipfile

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts/hardware-test"
sys.path.insert(0, str(SCRIPTS))
import report_keys as keys
import prepare


def load(name):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


runner = load("run-test")
inbox = load("report-inbox")
requester = {"id": 11, "login": "requester", "type": "User"}
publisher = {"id": 22, "login": "publisher", "type": "User"}
tester = {"id": 33, "login": "tester", "type": "User"}
author = {"id": 44, "login": "author", "type": "User"}
comment = f"https://github.com/{keys.REPO}/pull/7#issuecomment-567"
receipt = "b" * 32


class RequestDelivery(unittest.TestCase):
    def setUp(self):
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.root = Path(self.stack.enter_context(tempfile.TemporaryDirectory()))
        self.viewer = requester
        self.gists = {}
        self.source = {"id": 567, "user": requester, "body": "/victory hardware-test\nCompare the opening.",
                       "issue_url": f"https://api.github.com/repos/{keys.REPO}/issues/7"}
        self.stack.enter_context(mock.patch.object(keys, "github", self.github))
        self.stack.enter_context(contextlib.redirect_stdout(io.StringIO()))

    def github(self, path, payload=None, method=None):
        if path == "user":
            return self.viewer
        if path == "users/publisher":
            return publisher
        if path == f"repos/{keys.REPO}/issues/comments/567":
            return self.source
        if path.startswith("users/") and "/gists?" in path:
            login = path.split("/")[1]
            return [g for g in self.gists.values() if g["owner"]["login"] == login]
        if payload:
            self.assertIn(self.viewer, (requester, publisher))
            self.assertEqual(set(payload["files"]), {keys.REGISTRATION})
            self.assertRegex(payload["files"][keys.REGISTRATION]["content"], "^age1[a-z0-9]{58}\n$")
            if path == "gists":
                self.assertTrue(payload["public"])
                self.serial = getattr(self, "serial", 0) + 1
                gid = f"{self.serial:032x}"
                self.gists[gid] = dict(id=gid, owner=self.viewer.copy(), **payload)
            else:
                self.assertEqual(method, "PATCH")
                gid = path.split("/")[1]
                self.assertEqual(self.gists[gid]["owner"], self.viewer)
                self.gists[gid].update(payload)
            return self.gists[gid]
        if path.startswith("gists/"):
            return self.gists[path.split("/")[1]]
        self.fail(f"unexpected GitHub call {path}")

    def test_delegation_validates_the_request_comment_and_handles_no_key(self):
        self.assertEqual(keys.publication(7, "publisher", 22, comment),
                         {"requester": keys.account(requester), "registration": None})
        self.assertEqual(keys.publication(7, "publisher", 22, "")["requester"], keys.account(publisher))
        with self.assertRaises(ValueError):
            keys.publication(7, "publisher", 99, "")
        with self.assertRaises(ValueError):
            keys.publication(8, "publisher", 22, comment)
        for change in ({"body": "@requester did testing"}, {"id": 100}, {"issue_url": "another PR"}):
            with mock.patch.dict(self.source, change), self.assertRaises(ValueError):
                keys.publication(7, "publisher", 22, comment)

    def test_confirmation_names_the_requester_and_lists_reviewed_files(self):
        for name, width in (("requester", 100), ("r" * 39, 52)):
            label = f"Send encrypted diagnostics to @{name}"
            page, _, _ = runner.report_ui.layout(label, "PR #7", ["Included files", "summary.txt", "main-events.csv"],
                                                [("y", "Encrypt and send", "Send this ZIP.")], 0, 0, width, 24, False)
            self.assertIn("@" + name, page)
            self.assertIn("summary.txt", page)
            self.assertIn("main-events.csv", page)
            if width == 100:
                actions = [("k", "Keep local and finish", "Keep this ZIP."), ("i", label, "Review files first.")]
                menu, _, _ = runner.report_ui.layout("Hardware test results", "PR #7", [], actions, 0, 0, width, 24, False)
                self.assertIn(label, menu)

    def test_published_stamp_covers_the_requester_key_and_both_builds(self):
        out = self.root / "kit.zip"
        build = {"run": 9, "revision": "f" * 40, "sha256": "0" * 64}
        pull = {"user": author, "head": {"sha": "c" * 40}, "base": {"ref": "main", "repo": {"full_name": keys.REPO}}}
        args = ["prepare", "7", "--scenario", "Observe.", "--output", str(out), "--published", "--requester-comment", comment]
        with mock.patch.dict(os.environ, GITHUB_RUN_ID="812", GITHUB_ACTOR="publisher", GITHUB_ACTOR_ID="22"), \
                mock.patch.object(sys, "argv", args), mock.patch.object(prepare, "api", return_value=pull), \
                mock.patch.object(prepare, "find_build", side_effect=[(build, "base", b"one"),
                                  (dict(build, sha256="1" * 64), None, b"two")]):
            prepare.prepare()
        with zipfile.ZipFile(out) as kit:
            data = kit.read("request.json")
            self.assertIn(hashlib.sha256(data).hexdigest(), kit.read("run-test.py").decode())
            for name in ("report_keys.py", "observations.py", "observations.html"):
                self.assertIn(name, kit.namelist())
        trusted = hashlib.sha256(data).hexdigest()
        request = json.loads(data)
        self.assertEqual(request["delivery"]["requester"], keys.account(requester))
        manifest = self.root / "request.json"
        for modified in (dict(request, delivery={"requester": keys.account(publisher), "registration": None}),
                         dict(request, candidate=dict(build, run=735)), dict(request, pr=9)):
            manifest.write_text(json.dumps(modified))
            with contextlib.redirect_stdout(io.StringIO()) as report:
                runner.run_request(manifest, mock.Mock(plan=True, trusted_digest=trusted))
            self.assertIn("Private delivery unavailable for this request.", report.getvalue())
        run = dict(path=".github/workflows/hardware-request.yml", head_branch="main", event="workflow_dispatch",
                   conclusion="success", head_repository={"full_name": keys.REPO})
        artifact = {"name": "victory-series-x-pr-7", "digest": "sha256:" + runner.digest(out)}
        with mock.patch.object(keys, "github", side_effect=[run, {"artifacts": [artifact]}, run, {"artifacts": [artifact]}]):
            self.assertTrue(runner.official_archive(out, request))
            with zipfile.ZipFile(out, "a") as zip_out:
                zip_out.writestr("modified", "new bytes")
            self.assertFalse(runner.official_archive(out, request))

    def test_collects_a_truncated_gist_using_its_pinned_raw_revision(self):
        body = b"synthetic armored report"
        token = (receipt + " " + hashlib.sha256(body).hexdigest()).encode()
        url = f"https://gist.githubusercontent.com/tester/{receipt}/raw/{'1' * 40}/report.age"
        entry = dict(truncated=True, size=len(body), raw_url=url)
        gist = dict(id=receipt, public=False, owner=tester, files={"report.age": entry})
        with mock.patch.object(keys, "github", return_value=gist), \
                mock.patch.object(subprocess, "check_output", return_value=token), \
                mock.patch.object(inbox.urllib.request, "urlopen", return_value=io.BytesIO(body)) as raw, \
                mock.patch.object(inbox, "save_report") as save:
            inbox.collect_comment("age", [], {"user": tester}, "armor", self.root)
            raw.assert_called_once_with(url, timeout=30)
            save.assert_called_once()
            for changed in (dict(raw_url=url.replace("gist.githubusercontent.com", "other.example")),
                            dict(size=keys.MAX_COLLECTED_CIPHER + 1)):
                with mock.patch.dict(entry, changed), self.assertRaises(ValueError):
                    inbox.collect_comment("age", [], {"user": tester}, "armor", self.root)
            self.assertEqual(raw.call_count, 1)

    def test_scans_past_unrelated_comments_and_retries_failed_receipts(self):
        folder = self.root / "keys"
        folder.mkdir()
        (folder / "age1fixture.agekey").touch()
        comments = [{"id": i, "updated_at": "v1", "body": "other"} for i in range(100)]
        last = {"id": 105, "updated_at": "v2", "body": keys.RECEIPT_HEADER + "fixture"}
        with mock.patch.object(keys, "find_registration", return_value={"gist": receipt}), \
                mock.patch.object(keys, "github", side_effect=[comments, [last], comments, [last]]) as pages, \
                mock.patch.object(keys, "age_tool", side_effect=lambda _: contextlib.nullcontext("age")), \
                mock.patch.object(inbox, "collect_comment", side_effect=[OSError, None]) as decrypt, \
                contextlib.redirect_stderr(io.StringIO()):
            seen = set()
            inbox.collect(requester, folder, self.root, seen)
            self.assertFalse(seen)
            inbox.collect(requester, folder, self.root, seen)
            self.assertEqual(seen, {(105, "v2")})
            self.assertEqual(decrypt.call_count, 2)
            self.assertEqual([call.args[0].split("page=")[-1] for call in pages.call_args_list], ["1", "2", "1", "2"])

    def test_only_the_verified_requester_decrypts_the_reviewed_zip(self):
        # CI requires the pinned tools on both platforms. Offline contributors can use installed age.
        tools = {}
        for name in ("age", "age-keygen"):
            if os.environ.get("VICTORY_TEST_AGE") == "1":
                tools[name] = self.stack.enter_context(keys.age_tool(name))
            elif shutil.which(name):
                tools[name] = Path(shutil.which(name))
            else:
                self.skipTest("set VICTORY_TEST_AGE=1 to download the pinned age tools")
        self.stack.enter_context(mock.patch.object(keys, "age_tool", lambda n: contextlib.nullcontext(tools[n])))
        self.stack.enter_context(mock.patch.dict(os.environ, LOCALAPPDATA=str(self.root)))
        def command(*args):
            with mock.patch.object(sys, "argv", ["report-inbox.py", *args]):
                inbox.main()
        folders = {}
        for user in (publisher, requester):
            self.viewer = user
            command("setup")
            folder = self.root / "victory" / "report-inbox" / str(user["id"])
            folders[user["login"]] = folder
            identity, = folder.glob("*.agekey")
            before = identity.read_bytes()
            command("setup")
            self.assertEqual(tuple(folder.glob("*.agekey")), (identity,))
            self.assertEqual(identity.read_bytes(), before)
        # A deleted registration can be restored using the existing sole local identity.
        original_registration = keys.find_registration(keys.account(requester))
        del self.gists[original_registration["gist"]]
        command("setup")
        self.assertEqual(keys.find_registration(keys.account(requester))["key"], original_registration["key"])
        self.assertEqual(identity.read_bytes(), before)
        self.viewer = publisher
        binding = keys.publication(7, "publisher", 22, comment)
        filtered = self.root / "outgoing.zip"
        with zipfile.ZipFile(filtered, "w") as z:
            z.writestr("summary.txt", "user will edit")
            z.writestr("main-events.csv", "user will exclude")
        (self.root / "report.zip").write_bytes(b"raw private fixture")
        def edit(_prompt):
            files, = self.root.glob("victory-review-*/files")
            (files / "summary.txt").write_text("reviewed synthetic content")
            (files / "main-events.csv").unlink()
            return "a"
        with mock.patch("builtins.input", edit), mock.patch.object(runner.report_ui, "terminal_available", return_value=False):
            self.assertTrue(runner.review_result(filtered, self.root, own_editor=True))

        self.viewer = tester
        delivery = {}
        run = subprocess.run
        def transport(cmd, **kwargs):
            if cmd[0] != "gh":
                return run(cmd, **kwargs)
            if cmd[1:5] == ["api", "--hostname", "github.com", "gists"]:
                self.assertEqual(self.viewer, tester)
                payload = json.loads(kwargs["input"])
                self.assertFalse(payload["public"])
                self.assertEqual(set(payload["files"]), {"report.age"})
                delivery["cipher"] = payload["files"]["report.age"]["content"]
                self.assertTrue(delivery["cipher"].startswith("-----BEGIN AGE ENCRYPTED FILE-----"))
                return subprocess.CompletedProcess(cmd, 0, stdout=receipt)
            if cmd[1:5] == ["api", "--hostname", "github.com", f"gists/{receipt}/comments"]:
                delivery["notice"] = json.loads(kwargs["input"])["body"]
                return subprocess.CompletedProcess(cmd, 0)
            if cmd[1:] == ["gist", "view", "https://gist.github.com/" + receipt, "--raw", "--filename", "report.age"]:
                kwargs["stdout"].write(delivery["cipher"].encode())
                return subprocess.CompletedProcess(cmd, 0)
            self.fail(f"unexpected transport call: {cmd[:5]}")
        self.stack.enter_context(mock.patch.object(subprocess, "run", transport))
        published = keys.find_registration(keys.account(publisher))
        for alternate in (published, dict(binding["registration"], key=published["key"])):
            with self.assertRaises(ValueError):
                runner.send_encrypted_report(filtered, dict(binding, registration=alternate))
        self.assertFalse(delivery)
        # Replacing both fields with a different legitimate account must fail too.
        # Exercise the extracted runner through its normal argument parsing and results menu.
        build = {"run": 712, "revision": "a" * 40, "sha256": "b" * 64}
        original = {"version": 1, "pr": 7, "scenario": "Observe.", "delivery": binding,
                    "main": build, "candidate": build}
        manifest = self.root / "request.json"
        manifest.write_text(json.dumps(original), encoding="utf-8")
        stamp = runner.digest(manifest)
        changed = dict(original, delivery={"requester": keys.account(publisher), "registration": published})
        manifest.write_text(json.dumps(changed), encoding="utf-8")
        title = self.root / "fixture.exe"
        title.touch()
        def launch(side, folder, game, results, windows):
            (results / (side + ".log")).write_text("synthetic run")
        def menu(title, context, lines, actions, default, cancel):
            self.assertEqual(title, "Hardware test results")
            self.assertIn("Private delivery unavailable", " ".join(lines))
            self.assertNotIn("i", [action[0] for action in actions])
            return "k"
        with mock.patch.object(sys, "argv", ["run-test", "--title", str(title), "--output-dir", str(self.root)]), \
                mock.patch.object(runner, "ROOT", self.root), mock.patch.object(runner, "PUBLISHED_SHA256", stamp), \
                mock.patch.object(runner, "stage_build"), mock.patch.object(runner, "launch", side_effect=launch), \
                mock.patch.object(shutil, "which", return_value="fixture-tool"), \
                mock.patch.object(sys.stdin, "isatty", return_value=True), \
                mock.patch.object(subprocess, "check_output", return_value=tester["login"]), \
                mock.patch.object(runner.report_ui, "choose", side_effect=menu) as prompt:
            runner.run()
            prompt.assert_called_once()
        self.assertFalse(delivery)
        token = runner.send_encrypted_report(filtered, binding)
        public_comments = []
        registration = binding["registration"]["gist"]
        def with_mailbox(path, payload=None, method=None):
            if path == f"gists/{registration}/comments":
                self.assertEqual(self.viewer, tester)
                public_comments.append(dict(payload, id=19, user=tester, updated_at="v1"))
                return {}
            if path == f"gists/{registration}/comments?per_page=100&page=1":
                return public_comments
            if path == f"gists/{receipt}":
                cipher = delivery["cipher"]
                return dict(id=receipt, public=False, owner=tester, files={"report.age":
                            dict(content=cipher, size=len(cipher), truncated=False)})
            return self.github(path, payload, method)
        self.stack.enter_context(mock.patch.object(keys, "github", with_mailbox))
        with mock.patch.object(runner.report_ui, "choose", return_value="k") as consent:
            runner.offer_collection(token, binding, "PR #7")
            self.assertIn("GitHub username and posting time", " ".join(consent.call_args.args[2]))
            self.assertEqual(consent.call_args.args[-2:], ("k", "k"))
        self.assertFalse(public_comments)
        with mock.patch.object(runner.report_ui, "choose", return_value="a"):
            runner.offer_collection(token, binding, "PR #7")
        notice = public_comments[0]["body"]
        self.assertTrue(notice.startswith(keys.RECEIPT_HEADER + "-----BEGIN AGE ENCRYPTED FILE-----"))
        for hidden in (receipt, token, "summary.txt", "reviewed synthetic content", "PR #7"):
            self.assertNotIn(hidden, notice)
        wrong_keys = list(folders["publisher"].glob("age1*.agekey"))
        with self.assertRaises(subprocess.CalledProcessError):
            inbox.collect_comment(tools["age"], wrong_keys, public_comments[0], notice[len(keys.RECEIPT_HEADER):],
                                  self.root / "wrong")
        self.assertIn("@requester", delivery["notice"])
        self.assertIn("receive https://gist.github.com/" + receipt, delivery["notice"])
        self.viewer = publisher
        with self.assertRaises(subprocess.CalledProcessError):
            command("receive", receipt, "--output-dir", str(self.root / "wrong"))
        self.assertFalse(list((self.root / "wrong").glob("*.zip")))
        self.viewer = requester
        command("rotate")
        self.assertEqual(len(list(folders["requester"].glob("age1*.agekey"))), 2)
        with self.assertRaises(ValueError):
            runner.send_encrypted_report(filtered, binding)
        command("receive", receipt, "--output-dir", str(self.root / "right"))
        command("receive", receipt, "--output-dir", str(self.root / "right"))
        # Comments are public and editable. Only accept the submitting account's exact ciphertext.
        automatic = self.root / "automatic"
        original = delivery["cipher"]
        delivery["cipher"] += "x"
        with contextlib.redirect_stderr(io.StringIO()):
            command("collect", "--output-dir", str(automatic))
            delivery["cipher"] = original
            with mock.patch.dict(public_comments[0], user=publisher):
                command("collect", "--output-dir", str(automatic))
        self.assertFalse(list(automatic.glob("*.zip")))
        seen = set()
        inbox.collect(keys.account(requester), folders["requester"], automatic, seen)
        self.assertEqual(seen, {(19, "v1")})
        with mock.patch.object(inbox, "collect_comment", side_effect=AssertionError):
            inbox.collect(keys.account(requester), folders["requester"], automatic, seen)
        command("collect", "--output-dir", str(automatic))
        auto_report, = automatic.glob("*.zip")
        self.assertEqual(auto_report.read_bytes(), filtered.read_bytes())
        saved, = (self.root / "right").glob("*.zip")
        self.assertEqual(saved.read_bytes(), filtered.read_bytes())
        with zipfile.ZipFile(saved) as z:
            self.assertEqual(z.namelist(), ["summary.txt"])
            self.assertEqual(z.read("summary.txt"), b"reviewed synthetic content")


if __name__ == "__main__":
    unittest.main()
