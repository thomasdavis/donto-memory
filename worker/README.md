# donto-memory worker

Python Temporal worker + enqueue gateway that makes the `/memorize`
deferred path durable.

## Why

The Rust `donto-memory` service used to run deferred extraction in an
in-process `tokio` task. A service restart (deploy, OOM, billing
suspend) silently dropped every in-flight + queued job — measured at
~81% loss across one window. This worker moves that queue into the
Temporal server already running on the box (`localhost:7233`, UI on
`:8233`).

## Architecture

```
omega-bot ──POST /memorize (mode:deep)──▶ donto-memory (Rust :7900)
                                              │ deferred path
                                              ▼
                                   POST /enqueue (this worker :7901)
                                              │ start_workflow
                                              ▼
                                   Temporal server :7233  ──persists──▶ durable
                                              │ dispatch
                                              ▼
                                   MemorizeWorkflow → memorize_activity
                                              │ POST /memorize {async:false}
                                              ▼
                                   donto-memory (Rust :7900) runs extraction
```

All extraction logic stays in Rust; the activity is a thin, retryable,
durable wrapper that re-submits the request synchronously.

## Files

- `worker.py` — Temporal worker (task queue `memory-extraction`) + aiohttp `/enqueue` gateway on :7901.
- `workflows.py` — `MemorizeWorkflow`.
- `activities.py` — `memorize_activity` (calls back into Rust `/memorize` with `async:false`).

## Run

```bash
pip install -r requirements.txt
TEMPORAL_ADDRESS=localhost:7233 python worker.py
```

Deployed on donto-db as `memory-worker.service`.
