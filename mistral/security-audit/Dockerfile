# syntax=docker/dockerfile:1
FROM python:3.12-slim

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_NO_CACHE_DIR=1 \
    PIP_DISABLE_PIP_VERSION_CHECK=1

RUN groupadd --gid 1000 app \
    && useradd --uid 1000 --gid 1000 --create-home --shell /usr/sbin/nologin app

WORKDIR /app

COPY pyproject.toml README.md ./
COPY src ./src
RUN pip install .

USER 1000:1000

# No credentials in the image: GITHUB_TOKEN (and optionally GITHUB_API_URL) are
# injected at runtime from a bound workspace secret.
CMD ["python", "-m", "security_audit_worker.worker"]
