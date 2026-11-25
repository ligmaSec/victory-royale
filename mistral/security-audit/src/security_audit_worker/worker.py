"""Worker entrypoint: discover and serve the security audit workflows."""

import asyncio

import mistralai.workflows as workflows


async def main() -> None:
    workflow_classes = workflows.discover_all_workflows_in_package("security_audit_worker.workflows")
    await workflows.run_worker(workflow_classes)


if __name__ == "__main__":
    asyncio.run(main())
