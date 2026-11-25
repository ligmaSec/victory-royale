FROM python:3.12-slim

ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 PIP_NO_CACHE_DIR=1
WORKDIR /app
COPY pyproject.toml ./
COPY src ./src
RUN pip install .
COPY tests ./tests
USER 65532:65532
RUN python -m unittest discover -s tests -v
CMD ["python", "-m", "security_audit_worker.worker"]
