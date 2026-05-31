"""Activities for the donto-memory memorize queue.

The single activity re-submits the request to the donto-memory Rust
service over localhost with ``async: false`` forced, so the Rust side
runs the full extraction synchronously inside the activity. This keeps
all extraction logic in Rust and makes the activity a thin, durable,
retryable wrapper.
"""

import asyncio
import logging
import os

import httpx
from temporalio import activity

from opencode_agent import OpenCodeAgent, DEFAULT_MODEL
from extraction import extract_facts

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

    # Deep extraction is a single long HTTP request (minutes). The
    # workflow sets a 3-min heartbeat_timeout so a dead worker is
    # detected fast, which means we MUST heartbeat throughout the call —
    # otherwise Temporal cancels a perfectly healthy long-running
    # activity. Run the POST as a task and heartbeat every 30s while it
    # is in flight.
    activity.heartbeat("submitting")
    async with httpx.AsyncClient(timeout=ACTIVITY_HTTP_TIMEOUT_S) as client:
        post_task = asyncio.ensure_future(
            client.post(f"{DONTO_MEMORY_URL}/memorize", json=body)
        )
        while True:
            done, _ = await asyncio.wait({post_task}, timeout=30)
            if post_task in done:
                break
            activity.heartbeat("extracting")
        resp = post_task.result()

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


# Cap passes so a bad request can't run the agent forever.
MAX_OPENCODE_PASSES = int(os.environ.get("MAX_OPENCODE_PASSES", "5"))


@activity.defn
async def opencode_extract_activity(req: dict) -> dict:
    """Run the OpenCode agent to extract facts from req['text'].

    Expensive (minutes per pass). Returns {facts, passes}. Temporal caches
    this result, so a downstream ingest failure does NOT re-run the agent.
    Heartbeats throughout so a dead worker is detected without cancelling a
    healthy long run.
    """
    text = req.get("text") or ""
    passes = max(1, min(MAX_OPENCODE_PASSES, int(req.get("passes") or 1)))
    model = req.get("opencode_model") or DEFAULT_MODEL
    holder = req.get("holder", "?")
    logger.info(
        "opencode_extract start holder=%s passes=%s model=%s chars=%d",
        holder, passes, model, len(text),
    )

    agent = OpenCodeAgent(model=model)
    activity.heartbeat("extracting")

    # extract_facts is blocking (spawns opencode per pass). Run it off the
    # event loop and heartbeat every 30s while it works.
    task = asyncio.ensure_future(
        asyncio.to_thread(extract_facts, text, agent=agent, passes=passes)
    )
    while True:
        done, _ = await asyncio.wait({task}, timeout=30)
        if task in done:
            break
        activity.heartbeat("extracting")
    result = task.result()

    logger.info(
        "opencode_extract done holder=%s facts=%d passes=%s",
        holder, len(result.get("facts", [])), result.get("passes"),
    )
    return result


@activity.defn
async def ingest_facts_activity(payload: dict) -> dict:
    """POST pre-extracted facts to donto-memory's supplied-facts path
    (async:false, so it ingests synchronously without its own LLM)."""
    req = payload["req"]
    facts = payload.get("facts") or []
    body = {
        "holder": req["holder"],
        "session_id": req.get("session_id"),
        "text": req.get("text") or "",
        "modality": req.get("modality") or "descriptive",
        "mode": "opencode",
        "async": False,
        "facts": facts,
    }
    if req.get("queue_id"):
        body["queue_id"] = req["queue_id"]

    async with httpx.AsyncClient(timeout=ACTIVITY_HTTP_TIMEOUT_S) as client:
        resp = await client.post(f"{DONTO_MEMORY_URL}/memorize", json=body)
    if resp.status_code != 200:
        text = resp.text[:500]
        logger.warning("ingest_facts non-200 %s: %s", resp.status_code, text)
        raise RuntimeError(f"donto-memory /memorize (facts) {resp.status_code}: {text}")
    data = resp.json()
    summary = {
        "episodic_record_id": data.get("episodic_record_id"),
        "facts_extracted": data.get("facts_extracted", 0),
        "facts_ingested": data.get("facts_ingested", 0),
        "extract_mode": data.get("extract_mode"),
        "model": data.get("model"),
        "passes": payload.get("passes"),
    }
    logger.info(
        "ingest_facts done holder=%s supplied=%d ingested=%s",
        req.get("holder", "?"), len(facts), summary["facts_ingested"],
    )
    return summary
