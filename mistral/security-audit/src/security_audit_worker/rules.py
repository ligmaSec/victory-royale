"""Static security rules applied to pull request diffs.

The audit never executes, imports, builds or runs contributor code. It only
parses the unified diff text returned by the GitHub API and matches fixed
patterns against added lines.
"""

from __future__ import annotations

import re
from collections.abc import Iterator
from dataclasses import dataclass

MAX_DIFF_CHARS = 1_000_000
MAX_LINE_CHARS = 500
MAX_FINDINGS = 200

WORKFLOW_PATH = re.compile(r"^\.github/(?:workflows|actions)/")

SECRET_PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("hardcoded-private-key", re.compile(r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |PGP )?PRIVATE KEY-----")),
    ("github-pat", re.compile(r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,}\b")),
    ("github-fine-grained-pat", re.compile(r"\bgithub_pat_[A-Za-z0-9_]{82,}\b")),
    ("aws-access-key-id", re.compile(r"\bAKIA[0-9A-Z]{16}\b")),
    ("slack-token", re.compile(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b")),
    (
        "generic-secret-assignment",
        re.compile(r"(?i)\b(?:api[_-]?key|secret|token|passwd|password)\b\s*[:=]\s*[\"'][^\"'\s]{16,}[\"']"),
    ),
)

CODE_EXECUTION_PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("python-eval-exec", re.compile(r"\b(?:eval|exec)\s*\(")),
    ("python-os-system", re.compile(r"\bos\.system\s*\(")),
    ("python-subprocess-shell", re.compile(r"subprocess\.(?:run|call|check_output|Popen)\s*\([^)]*shell\s*=\s*True")),
    ("python-pickle-loads", re.compile(r"\bpickle\.loads?\s*\(")),
    ("yaml-unsafe-load", re.compile(r"\byaml\.load\s*\((?![^)]*SafeLoader)")),
    ("js-child-process-exec", re.compile(r"\bchild_process\.exec(?:Sync)?\s*\(")),
    ("js-function-constructor", re.compile(r"\bnew\s+Function\s*\(")),
    ("shell-pipe-to-interpreter", re.compile(r"\b(?:curl|wget)\b[^\n|]*\|\s*(?:sudo\s+)?(?:ba|z|da)?sh\b")),
    ("powershell-encoded-command", re.compile(r"(?i)\bpowershell\b[^\n]*\s(?:-enc|-encodedcommand)\s")),
    ("base64-pipe-to-shell", re.compile(r"\bbase64\s+(?:-d|--decode)\b[^\n|]*\|\s*(?:sudo\s+)?(?:ba|z|da)?sh\b")),
    ("destructive-fs-command", re.compile(r"\b(?:rm\s+-rf\s+/|mkfs\b|dd\s+if=/dev/zero|chmod\s+777)\b")),
)


@dataclass(frozen=True)
class Finding:
    rule_id: str
    severity: str
    path: str
    line: str

    def as_dict(self) -> dict[str, str]:
        return {"rule_id": self.rule_id, "severity": self.severity, "path": self.path, "line": self.line}


def iter_added_lines(diff: str) -> Iterator[tuple[str, str]]:
    path = "<unknown>"
    for raw in diff.splitlines():
        if raw.startswith("diff --git "):
            match = re.match(r"diff --git a/(.*?) b/(.*)$", raw)
            if match:
                path = match.group(2)
        elif raw.startswith("+++ b/"):
            path = raw[len("+++ b/") :].strip() or path
        elif raw.startswith("+++ /dev/null"):
            path = "<deleted>"
        elif raw.startswith("+") and not raw.startswith("+++"):
            yield path, raw[1:]


def scan_diff(diff: str) -> list[Finding]:
    """Scan added diff lines and return findings.

    Raises ValueError when the diff is too large to review completely, so the
    caller can mark the review incomplete instead of passing a partial scan.
    """
    if len(diff) > MAX_DIFF_CHARS:
        raise ValueError(f"diff exceeds {MAX_DIFF_CHARS} characters; refusing a partial review")
    findings: list[Finding] = []
    for path, line in iter_added_lines(diff):
        if path != "<deleted>" and WORKFLOW_PATH.match(path):
            findings.append(Finding("github-actions-change", "high", path, line[:MAX_LINE_CHARS]))
            continue
        for rule_id, pattern in SECRET_PATTERNS:
            if pattern.search(line):
                findings.append(Finding(rule_id, "high", path, line[:MAX_LINE_CHARS]))
        for rule_id, pattern in CODE_EXECUTION_PATTERNS:
            if pattern.search(line):
                findings.append(Finding(rule_id, "high", path, line[:MAX_LINE_CHARS]))
        if len(findings) >= MAX_FINDINGS:
            break
    return findings
