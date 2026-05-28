FROM python:3.12-slim AS builder

WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    libpq-dev \
  && rm -rf /var/lib/apt/lists/*

COPY pyproject.toml README.md /app/
COPY donto_memory /app/donto_memory
COPY migrations /app/migrations

RUN pip install --no-cache-dir --upgrade pip \
  && pip install --no-cache-dir .

# --- runtime image ---
FROM python:3.12-slim
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    libpq5 \
  && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/lib/python3.12/site-packages /usr/local/lib/python3.12/site-packages
COPY --from=builder /usr/local/bin /usr/local/bin
COPY migrations /app/migrations

EXPOSE 7900

# Default command runs the API. Override to run the worker.
CMD ["donto-memory", "api", "--bind", "0.0.0.0:7900"]
