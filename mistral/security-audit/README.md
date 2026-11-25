# Mistral security audit worker

A [Mistral Workflows](https://docs.mistral.ai/studio/workflows) worker that
reviews every pull request diff and publishes a GitHub check run. It uses
**zero GitHub Actions jobs**: the audit runs as a Studio managed deployment,
started by a webhook trigger.

- Triggers: GitHub `pull_request` events with action `opened`, `reopened` or
  `synchronize` (a push to the PR branch). Fork PRs are included: GitHub
  delivers fork `pull_request` events to the base repository webhook, and the
  worker only ever reads the diff through the base repo API — fork code is
  never checked out, built or executed.
- Review: the workflow fetches the PR diff (`Accept: application/vnd.github.v3.diff`)
  and scans **added lines only** with fixed patterns (`src/security_audit_worker/rules.py`).
  Contributor code is never executed.
- Result: a completed check run named `Mistral Security Audit` on the PR head
  SHA. Conclusion is `success` when no rule matches, `failure` when there are
  findings, and `failure` whenever the review could not complete (API error,
  oversized diff, missing token) — an incomplete review is never reported as
  a pass.

## Studio deployment settings

| Setting | Value |
| --- | --- |
| Repository | `https://github.com/ligmaSec/victory-royale` |
| Build directory | `mistral/security-audit` |
| Dockerfile path | `Dockerfile` (relative to the build directory) |
| Suggested deployment name | `security-audit-worker` |

Bind one workspace secret on the deployment: secret `github_token` exposed as
env var `GITHUB_TOKEN`. Use a fine-grained PAT for `ligmaSec/victory-royale`
with **Checks: write**, **Pull requests: read**, **Contents: read** and
**Metadata: read**. The token is only sent to the host in `GITHUB_API_URL`
(default `https://api.github.com`, HTTPS-only). For GitHub Enterprise, set
`GITHUB_API_URL` to the instance API URL.

## Webhook and trigger

1. Create a webhook in Studio with provider `github` (workspace scope), then
   register its URL as a repository webhook on `ligmaSec/victory-royale` with
   content type JSON, a signing secret, and the **Pull requests** event
   subscription (GitHub sends `opened`, `reopened`, `synchronize` and more).
2. Create a trigger on that webhook starting workflow `mistral-security-audit`,
   with the filter:

   ```
   event_type == 'pull_request' && (action == 'opened' || action == 'reopened' || action == 'synchronize')
   ```

The workflow receives the whole webhook request as its input and re-checks the
event name and action itself.

## Layout

```
mistral/security-audit/
├── Dockerfile                  # worker image; CMD runs the worker module
├── pyproject.toml              # deps: mistralai-workflows[mistralai], pydantic, httpx
├── src/security_audit_worker/
│   ├── worker.py               # entrypoint: discover + serve workflows
│   ├── rules.py                # pure static diff rules (no SDK imports)
│   └── workflows/
│       └── pr_security_audit.py  # workflow + GitHub API activities
└── tests/test_rules.py         # stdlib unittest for the rules
```

## Local test

```bash
cd mistral/security-audit
PYTHONPATH=src python -m unittest discover -s tests -v
```

The workflow modules need `mistralai-workflows` installed; the rules tests run
with the standard library only.
