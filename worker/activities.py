"""Activities for the donto-memory memorize queue.

The single activity re-submits the request to the donto-memory Rust
service over localhost with ``async: false`` forced, so the Rust side
runs the full extraction synchronously inside the activity. This keeps
all extraction logic in Rust and makes the activity a thin, durable,
retryable wrapper.
"""

import logging
import os

import httpx
from temporalio import activity

logger = logging.getLogger("memory-worker")

# The Rust donto-memory API. The activity calls its /memorize with
# async:false so it executes synchronously and returns the full result.
DONTO_MEMORY_URL = os.environ.get(
    "DONTO_MEMORY_INTERNAL_URL", "http://127.0.0.1:7900"
)

# A deep 7-pass run has been observed at ~14 min; allow generous slack
# under the workflow's 25 min start_to_close.
ACTIVITY_HTTP_TIMEOUT_S = float(os.environ.get("MEMORIZE_HTTP_TIMEOUT_S", "1380"))


@activity.defn
async def memorize_activity(req: dict) -> dict:
    """Run one memorize synchronously via the Rust service.

    `req` is the original /memorize request body. We force async:false
    so the Rust route runs memorize_one inline and returns the full
    MemorizeResp (facts, usage, aperture_yields, ...).
    """
    body = dict(req)
    body["async"] = False  # force synchronous execution on the Rust side

    holder = body.get("holder", "?")
    session = body.get("session_id", "?")
    mode = body.get("mode", "single")
    logger.info(
        "memorize_activity start holder=%s session=%s mode=%s passes=%s",
        holder, session, mode, body.get("passes"),
    )

    # Heartbeat before the long call so Temporal knows we're alive; the
    # call itself is a single blocking await, so we heartbeat once up
    # front (the heartbeat_timeout is 3 min, the activity attempt is one
    # long HTTP request — if the worker dies, Temporal re-dispatches).
    activity.heartbeat("submitting")

    async with httpx.AsyncClient(timeout=ACTIVITY_HTTP_TIMEOUT_S) as client:
        resp = await client.post(f"{DONTO_MEMORY_URL}/memorize", json=body)

    if resp.status_code != 200:
        # Non-200 → raise so Temporal retries per the workflow policy.
        text = resp.text[:500]
        logger.warning("memorize_activity non-200 %s: %s", resp.status_code, text)
        raise RuntimeError(f"donto-memory /memorize {resp.status_code}: {text}")

    data = resp.json()
    summary = {
        "episodic_record_id": data.get("episodic_record_id"),
        "facts_extracted": data.get("facts_extracted", 0),
        "facts_ingested": data.get("facts_ingested", 0),
        "dedup_collisions": data.get("dedup_collisions", 0),
        "extract_mode": data.get("extract_mode"),
        "model": data.get("model"),
        "elapsed_ms": data.get("elapsed_ms"),
    }
    logger.info(
        "memorize_activity done holder=%s ingested=%s mode=%s elapsed_ms=%s",
        holder, summary["facts_ingested"], summary["extract_mode"], summary["elapsed_ms"],
    )
    return summary
