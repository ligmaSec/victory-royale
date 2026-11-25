"""Mistral security audit workflow.

Triggered by a Studio webhook trigger on GitHub ``pull_request`` events
(opened, reopened, synchronize — including fork PRs). The workflow fetches the
PR diff through the GitHub REST API, scans the diff text statically (contributor
code is never executed), and publishes a completed check run named
"Mistral Security Audit" on the PR head commit. Any finding — or any failure
that leaves the review incomplete — is published with conclusion ``failure``.
"""

from __future__ import annotations

import os
from datetime import timedelta
from typing import Any

import mistralai.workflows as workflows
from pydantic import BaseModel, ConfigDict, Field

from security_audit_worker.rules import Finding, scan_diff

CHECK_RUN_NAME = "Mistral Security Audit"
AUDITED_ACTIONS = frozenset({"opened", "reopened", "synchronize"})
DEFAULT_API_BASE = "https://api.github.com"


class WebhookRequest(BaseModel):
    """The raw webhook request a Studio trigger delivers as the workflow input."""

    model_config = ConfigDict(extra="allow")

    method: str = "POST"
    path: str = "/"
    headers: dict[str, str] = Field(default_factory=dict)
    query_params: dict[str, str] = Field(default_factory=dict)
    body: dict[str, Any] | None = None
    raw_body: str | None = None


class FetchDiffInput(BaseModel):
    owner: str
    repo: str
    pull_number: int


class PublishCheckRunInput(BaseModel):
    owner: str
    repo: str
    head_sha: str
    conclusion: str
    title: str
    summary: str
    text: str


class AuditOutput(BaseModel):
    action: str = ""
    repository: str = ""
    pull_number: int = 0
    head_sha: str = ""
    conclusion: str = "neutral"
    check_run_id: int | None = None
    findings: list[dict[str, str]] = Field(default_factory=list)
    note: str = ""


def _api_base() -> str:
    base = os.environ.get("GITHUB_API_URL", DEFAULT_API_BASE)
    if not base.startswith("https://"):
        raise workflows.WorkflowError("github_api_url_not_https", non_retryable=True)
    return base.rstrip("/")


def _github_token() -> str:
    token = os.environ.get("GITHUB_TOKEN", "")
    if not token:
        raise workflows.WorkflowError("github_token_missing", non_retryable=True)
    return token


def _github_headers(accept: str) -> dict[str, str]:
    return {
        "Authorization": f"Bearer {_github_token()}",
        "Accept": accept,
        "X-GitHub-Api-Version": "2022-11-28",
    }


@workflows.activity(
    name="github-fetch-pr-diff",
    start_to_close_timeout=timedelta(seconds=30),
    retry_policy_max_attempts=3,
    retry_policy_backoff_coefficient=2.0,
)
async def fetch_pr_diff(params: FetchDiffInput) -> str:
    import httpx

    url = f"{_api_base()}/repos/{params.owner}/{params.repo}/pulls/{params.pull_number}"
    try:
        async with httpx.AsyncClient(timeout=20, follow_redirects=False) as client:
            response = await client.get(url, headers=_github_headers("application/vnd.github.v3.diff"))
    except httpx.RequestError as exc:
        raise workflows.WorkflowError("github_api_unavailable") from exc
    if response.status_code != 200:
        retryable = response.status_code in {429, 500, 502, 503, 504}
        raise workflows.WorkflowError(f"github_api_http_{response.status_code}", non_retryable=not retryable)
    return response.text


@workflows.activity(
    name="github-publish-check-run",
    start_to_close_timeout=timedelta(seconds=30),
    retry_policy_max_attempts=3,
    retry_policy_backoff_coefficient=2.0,
)
async def publish_check_run(params: PublishCheckRunInput) -> int:
    import httpx

    base = _api_base()
    headers = _github_headers("application/vnd.github+json")
    payload = {
        "name": CHECK_RUN_NAME,
        "head_sha": params.head_sha,
        "status": "completed",
        "conclusion": params.conclusion,
        "output": {"title": params.title, "summary": params.summary, "text": params.text},
    }
    async with httpx.AsyncClient(timeout=20, follow_redirects=False) as client:
        check_run_id: int | None = None
        listed = await client.get(
            f"{base}/repos/{params.owner}/{params.repo}/commits/{params.head_sha}/check-runs",
            headers=headers,
            params={"check_name": CHECK_RUN_NAME, "per_page": 100},
        )
        if listed.status_code == 200:
            for run in listed.json().get("check_runs", []):
                if run.get("name") == CHECK_RUN_NAME:
                    check_run_id = int(run["id"])
                    break
        if check_run_id is None:
            response = await client.post(
                f"{base}/repos/{params.owner}/{params.repo}/check-runs", headers=headers, json=payload
            )
        else:
            response = await client.patch(
                f"{base}/repos/{params.owner}/{params.repo}/check-runs/{check_run_id}",
                headers=headers,
                json=payload,
            )
    if response.status_code not in {200, 201}:
        retryable = response.status_code in {429, 500, 502, 503, 504}
        raise workflows.WorkflowError(f"github_api_http_{response.status_code}", non_retryable=not retryable)
    return int(response.json()["id"])


@workflows.workflow.define(name="mistral-security-audit")
class MistralSecurityAuditWorkflow:
    @workflows.workflow.entrypoint
    async def run(self, event: WebhookRequest) -> AuditOutput:
        headers = {str(k).lower(): str(v) for k, v in event.headers.items()}
        body = event.body or {}
        action = str(body.get("action", ""))
        repository = str((body.get("repository") or {}).get("full_name", ""))
        pull_request = body.get("pull_request") or {}
        if (
            headers.get("x-github-event") != "pull_request"
            or action not in AUDITED_ACTIONS
            or not repository
            or not pull_request
        ):
            return AuditOutput(
                action=action,
                repository=repository,
                note="ignored: not a pull_request opened/reopened/synchronize event",
            )
        owner, _, repo = repository.partition("/")
        pull_number = int(pull_request.get("number", 0))
        head_sha = str((pull_request.get("head") or {}).get("sha", ""))
        if not owner or not repo or not pull_number or not head_sha:
            return AuditOutput(
                action=action,
                repository=repository,
                note="ignored: incomplete pull_request payload",
            )

        try:
            diff = await fetch_pr_diff(FetchDiffInput(owner=owner, repo=repo, pull_number=pull_number))
            findings = scan_diff(diff)
        except Exception as exc:
            findings = []
            conclusion = "failure"
            title = "Mistral security audit incomplete"
            summary = (
                "The diff review did not complete, so this check is marked failed "
                f"({type(exc).__name__}). No conclusion of safety is implied."
            )
            text = f"Review incomplete: {exc}\n\nThe PR diff was not fully audited."
        else:
            conclusion = "failure" if findings else "success"
            if findings:
                title = "Mistral security audit found issues"
                summary = f"{len(findings)} finding(s) in the added diff lines. Contributor code was not executed."
            else:
                title = "Mistral security audit passed"
                summary = "Static scan of the added diff lines found no matching security rules. Contributor code was not executed."
            lines = [f"- `{f.rule_id}` ({f.severity}) `{f.path}`: `{f.line}`" for f in findings[:50]]
            text = "\n".join(lines) if lines else "No findings."

        check_run_id = await publish_check_run(
            PublishCheckRunInput(
                owner=owner,
                repo=repo,
                head_sha=head_sha,
                conclusion=conclusion,
                title=title,
                summary=summary[:65000],
                text=text[:65000],
            )
        )
        return AuditOutput(
            action=action,
            repository=repository,
            pull_number=pull_number,
            head_sha=head_sha,
            conclusion=conclusion,
            check_run_id=check_run_id,
            findings=[f.as_dict() for f in findings],
            note="static diff review only; contributor code is never executed",
        )
