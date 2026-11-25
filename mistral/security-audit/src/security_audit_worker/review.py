"""Validate complete patch input and render the model's structured findings."""

import json
from typing import Literal

from pydantic import BaseModel, ConfigDict, Field

MAX_PATCH_BYTES = 96_000


class IncompleteReview(ValueError):
    """A safe, public reason why the check cannot pass."""


class Finding(BaseModel):
    model_config = ConfigDict(extra="forbid")
    path: str = Field(min_length=1, max_length=1024)
    line: int = Field(gt=0)
    severity: Literal["medium", "high", "critical"]
    confidence: Literal["medium", "high"]
    category: Literal["access control", "injection", "secret exposure", "memory safety",
                      "filesystem access", "unsafe dependency", "resource exhaustion",
                      "cryptography", "data exposure", "other security defect"]


class Review(BaseModel):
    model_config = ConfigDict(extra="forbid")
    findings: list[Finding] = Field(max_length=25)


def prepare_changes(files: list[dict], expected: int) -> str:
    if not files or len(files) != expected:
        raise IncompleteReview("GitHub returned an incomplete file list.")
    changes = []
    for file in files:
        patch = file.get("patch")
        if patch is None:
            if file["status"] == "renamed" and file["changes"] == 0:
                patch = ""
            else:
                raise IncompleteReview("A changed file has no reviewable text patch.")
        lines = patch.split("\n")
        if (sum(line.startswith("+") for line in lines) != file["additions"]
                or sum(line.startswith("-") for line in lines) != file["deletions"]):
            raise IncompleteReview("GitHub returned an incomplete patch.")
        changes.append({"path": file["filename"], "status": file["status"],
                        "previous_path": file.get("previous_filename"), "patch": patch})
    data = json.dumps(changes, ensure_ascii=False)
    if len(data.encode()) > MAX_PATCH_BYTES:
        raise IncompleteReview("The full patch exceeds the single-review size limit.")
    return data


def outcome(review: Review, paths: set[str]) -> tuple[str, str]:
    if any(f.path not in paths for f in review.findings):
        raise IncompleteReview("The reviewer referenced a file outside the diff.")
    blocking = any(f.severity in {"high", "critical"} and f.confidence == "high"
                   for f in review.findings)
    # Publish only validated paths and structured labels. Keep model prose and diff text private.
    items = []
    for f in review.findings:
        path = json.dumps(f.path, ensure_ascii=True)
        items.append(f"    {path}:{f.line}  {f.category} ({f.severity}, {f.confidence} confidence)")
    return ("failure" if blocking else "success",
            "\n\n".join(items) or "Mistral reported no security findings in the reviewed changes.")
