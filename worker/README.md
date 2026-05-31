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
- `workflows.py` — `MemorizeWorkflow`, branching on `mode`.
- `activities.py` — the three activities (see below).
- `opencode_agent.py` — **reusable** headless OpenCode driver (see below).
- `extraction.py` — OpenCode-agent memory extraction (sophisticated prompt, multi-pass, dedup).

## Two extraction modes

`MemorizeWorkflow` picks a path from the request `mode`:

- **in-process** (`single` / `deep` / `exhaustive` / …) → `memorize_activity`
  re-submits to Rust `/memorize {async:false}`; donto-memory runs the LLM
  extraction itself (OpenRouter or whatever `DONTO_MEMORY_LLM_*` points at).
- **opencode** (`opencode` / `agentic`) → `opencode_extract_activity` drives the
  OpenCode agent on the GLM coding subscription to produce facts, then
  `ingest_facts_activity` POSTs them to Rust `/memorize {facts:[...]}` (the
  supplied-facts path — no second LLM call). The two are **separate activities**
  so an ingest failure retries without re-running the multi-minute agent
  (Temporal caches the extract result).

## OpenCodeAgent — the reusable abstraction

`opencode_agent.py` is the foundation for *any* agentic-AI step, not just
memory. It drives `opencode run` headlessly **inside the omega-bot container**
(the proven env with the GLM subscription config) and exchanges files over the
shared bind mount (`/data/omega/shared/oc/<run_id>` on the host == `/data/oc/<run_id>`
in the container), so large outputs are captured via files rather than stdout.

```python
from opencode_agent import OpenCodeAgent
agent = OpenCodeAgent(model="glm-4.7")
run = agent.run(
    prompt="Read input.txt and write a JSON summary to out.json, then self-validate it.",
    input_files={"input.txt": text},
    output_files=["out.json"],
    timeout=300,
)
if run.ok:
    data = json.loads(run.output_files["out.json"])
```

Config is injected via `OPENCODE_CONFIG_CONTENT` with `apiKey: {env:GLM_API_KEY}`
so the key stays in the container. Override the container/model/endpoint via the
`OPENCODE_*` env vars (see the module).

## Run

```bash
pip install -r requirements.txt
TEMPORAL_ADDRESS=localhost:7233 python worker.py
```

Deployed on donto-db as `memory-worker.service`.
