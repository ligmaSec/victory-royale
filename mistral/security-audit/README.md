# Mistral pull request security check

When someone opens, updates or reopens a PR, Studio sends the complete text diff for that revision to Mistral. The worker reads the comparison through GitHub's API, including for fork PRs. It runs from the trusted deployment on main and treats contributor changes as data.

The **Mistral security audit** check appears on the PR head commit. High-confidence findings of high or critical severity fail the check. Other findings appear in the summary with their file and line. A missing patch, a truncated comparison, more than 96,000 bytes of review input, or a model error fails the review. A GitHub permissions or delivery failure can leave the check missing or pending.

## Connect Studio to GitHub

1. Verify that the official MistralAI GitHub App has access to this repository. The worker needs pull request and contents read access and checks write access.
2. Configure a Studio HTTP connector targeting `https://api.github.com`. The worker defaults to connector `github_http` and credential `default`. Both must be accessible to the deployment identity. Set `GITHUB_HTTP_CONNECTOR` or `GITHUB_CREDENTIAL_NAME` if the workspace uses different names.
3. **Verify the publisher before deploying.** Through that same HTTP connector and identity, create an in-progress test check named `Mistral credential test` on a disposable PR head. Confirm the response has `app.slug == "mistralai"` and PATCH the returned ID to completed with a neutral conclusion. A read through the GitHub App connector alone only verifies reads. If Studio cannot supply the official App credential to the HTTP connector, the check publisher is blocked. Keep the check optional while resolving this with Mistral.

GitHub requires a GitHub App user or installation token for these writes. Studio stores connector credentials. The worker needs no GitHub token in the image or repository. See the [GitHub Checks API](https://docs.github.com/en/rest/checks/runs#create-a-check-run) and [Studio HTTP connectors](https://docs.mistral.ai/studio/connectors/http-connectors).

## Deploy and verify a real PR

| Studio field | Value |
| --- | --- |
| Name | `security-audits` |
| Repository | `ligmaSec/victory-royale` |
| Branch | `main` |
| Build directory | `mistral/security-audit` |
| Dockerfile path | `Dockerfile` |

1. Select a main revision containing the Dockerfile. Review the workspace's hosting and inference billing before activating the deployment.
2. Create a GitHub webhook in Studio. Register its URL on this repository for **Pull requests**, using JSON and the same signing secret at both ends.
3. Link the webhook to the deployed workflow named `mistral-security-audit` with the trigger filter `event_type == 'pull_request'`. The worker selects the relevant PR actions. See [Studio triggers and webhooks](https://docs.mistral.ai/studio/workflows/building-workflows/triggers-and-webhooks).
4. Open a harmless fork PR, then push a second commit. On each head, verify that the official MistralAI app publishes and completes the check in the PR's **Checks** tab. After this succeeds, the repository owner can require **Mistral security audit** from that app in the main branch ruleset.

## Test the worker locally

The Docker build runs the worker tests. To run them directly with Python 3.12, run these commands from this directory.

```bash
python3.12 -m venv .venv
.venv/bin/pip install .
.venv/bin/python -m unittest discover -s tests -v
```
