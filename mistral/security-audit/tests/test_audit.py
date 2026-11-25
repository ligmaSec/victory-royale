import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock, patch

import mistralai.workflows as workflows
from security_audit_worker.review import Finding, IncompleteReview, Review, outcome, prepare_changes
from security_audit_worker.workflows import pr_security_audit as audit


REV = audit.Revision(number=7, base="a" * 40, head="b" * 40)
PR = {"base": {"sha": REV.base}, "head": {"sha": REV.head, "repo": {"full_name": "fork/test"}},
      "changed_files": 1}
FILE = {"filename": "a.rs", "status": "modified", "patch": "@@ -1 +1 @@\n-old\n+new",
        "additions": 1, "deletions": 1}
EVENT = audit.WebhookRequest(headers={"X-GitHub-Event": "pull_request"}, body={
    "number": 7, "repository": {"full_name": audit.REPO}, "action": "synchronize", "pull_request": PR,
})


def response(payload):
    return Mock(json=Mock(return_value=payload))


class AuditTests(unittest.IsolatedAsyncioTestCase):
    def test_entrypoint_and_event_filter(self):
        self.assertEqual(workflows.discover_all_workflows_in_package("security_audit_worker.workflows"),
                         [audit.MistralSecurityAuditWorkflow])
        self.assertEqual(audit.revision(EVENT), REV)
        event = EVENT.model_copy(deep=True)
        event.body["action"] = "closed"
        self.assertIsNone(audit.revision(event))

    def test_incomplete_diffs(self):
        for files, count in [([FILE], 301), ([{**FILE, "patch": None}], 1),
                             ([{**FILE, "additions": 2}], 1),
                             ([{**FILE, "patch": "-old\n+" + "x" * 96_000}], 1)]:
            with self.subTest(count=count), self.assertRaises(IncompleteReview):
                prepare_changes(files, count)

    def test_finding_policy(self):
        finding = Finding(path="a.rs", line=1, severity="high", confidence="high", category="access control")
        for confidence, conclusion in [("high", "failure"), ("medium", "success")]:
            result = Review(findings=[finding.model_copy(update={"confidence": confidence})])
            status, summary = outcome(result, {"a.rs"})
            self.assertEqual(status, conclusion)
            self.assertIn("access control", summary)
            with self.assertRaises(IncompleteReview):
                outcome(result, {"different.rs"})

    async def test_fork_review_uses_pinned_base_repo_paths(self):
        client = AsyncMock()
        client.get.side_effect = [response(PR), response({"check_runs": []}), response(PR),
                                  response({"files": [FILE]})]
        client.post.return_value = response({"id": 44, "app": {"id": audit.APP_ID}})
        client.patch.return_value = response({})
        connector = AsyncMock()
        connector.__aenter__.return_value = client
        model = AsyncMock()
        model.chat.parse_async.return_value = SimpleNamespace(choices=[SimpleNamespace(
            finish_reason="stop", message=SimpleNamespace(parsed=Review(findings=[])))])
        with patch.object(audit, "github", return_value=connector), patch(
                "mistralai.workflows.client.get_mistral_client", return_value=model):
            self.assertEqual(await audit.begin.__original_func__(REV), 44)
            status, summary = await audit.review_changes.__original_func__(REV)
            await audit.complete.__original_func__(44, status, summary)
        self.assertEqual(status, "success")
        self.assertEqual(client.post.await_args.kwargs["json"]["head_sha"], REV.head)
        self.assertEqual(client.get.await_args.args[0], f"{audit.API}/compare/{REV.base}...{REV.head}")
        self.assertIn("a.rs", model.chat.parse_async.await_args.kwargs["messages"][1]["content"])
        self.assertEqual(client.patch.await_args.args[0], f"{audit.API}/check-runs/44")

    async def test_stale_and_completed_revisions_skip(self):
        client = AsyncMock()
        client.get.side_effect = [response({**PR, "head": {"sha": "c" * 40}}), response(PR),
                                  response({"check_runs": [{"external_id": REV.external_id,
                                            "app": {"id": audit.APP_ID}, "status": "completed",
                                            "conclusion": "success"}]})]
        self.assertIsNone(await audit.start_check(client, REV))
        self.assertIsNone(await audit.start_check(client, REV))
        client.post.assert_not_awaited()

    async def test_wrong_publisher(self):
        client = AsyncMock()
        client.get.side_effect = [response(PR), response({"check_runs": []})]
        client.post.return_value = response({"id": 44, "app": {"id": 1}})
        with self.assertRaises(ValueError):
            await audit.start_check(client, REV)

    async def test_service_failure_closes_check(self):
        with patch.object(audit, "begin", AsyncMock(return_value=44)), patch.object(
                audit, "review_changes", AsyncMock(side_effect=RuntimeError("private details"))), patch.object(
                audit, "complete", AsyncMock()) as complete:
            result = await audit.MistralSecurityAuditWorkflow().run(EVENT)
        self.assertEqual(result, {"result": "failure"})
        self.assertEqual(complete.await_args.args[:2], (44, "failure"))
        self.assertNotIn("private details", complete.await_args.args[2])
