"""Temporal workflow definitions for the donto-memory memorize queue.

A `MemorizeWorkflow` is started for every deferred /memorize request.
Its single activity calls back into the donto-memory Rust service's
synchronous /memorize endpoint, so all of the battle-tested extraction
logic (episodic ingest, deep multi-pass extraction, JSON salvage,
content-hash dedup, semantic-claim ingest, self-read policy grant)
stays in one place. Temporal gives us:

  - Durability: a worker/API restart no longer drops in-flight work —
    Temporal persists the workflow and resumes the activity.
  - Retries: transient OpenRouter / substrate failures are retried
    with backoff instead of silently lost.
  - Visibility: every memorize is a workflow execution browsable in
    the Temporal UI (:8233).
"""

from datetime import timedelta

from temporalio import workflow
from temporalio.common import RetryPolicy

with workflow.unsafe.imports_passed_through():
    from activities import (
        memorize_activity,
        opencode_extract_activity,
        ingest_facts_activity,
    )

# Modes that use the OpenCode agent for extraction (vs the in-process LLM
# path via memorize_activity).
OPENCODE_MODES = {"opencode", "agentic"}


@workflow.defn
class MemorizeWorkflow:
    def __init__(self) -> None:
        self._status = "queued"
        self._holder = ""
        self._session_id = ""
        self._mode = ""
        self._facts_ingested = 0
        self._error = None

    @workflow.run
    async def run(self, req: dict) -> dict:
        self._holder = req.get("holder", "")
        self._session_id = req.get("session_id") or ""
        self._mode = (req.get("mode") or "single").lower()
        self._status = "extracting"

        try:
            if self._mode in OPENCODE_MODES:
                result = await self._run_opencode(req)
            else:
                result = await self._run_inprocess(req)
            self._status = "done"
            self._facts_ingested = (result or {}).get("facts_ingested", 0)
            return result
        except Exception as e:  # noqa: BLE001 — surface the failure on the workflow
            self._status = "failed"
            self._error = str(e)
            raise

    async def _run_inprocess(self, req: dict) -> dict:
        """In-process LLM path: donto-memory does the extraction itself.
        Deep/exhaustive runs can take many minutes; generous ceiling + a
        few retries. heartbeat_timeout lets a dead worker be re-dispatched
        well before start_to_close."""
        return await workflow.execute_activity(
            memorize_activity,
            req,
            start_to_close_timeout=timedelta(minutes=25),
            heartbeat_timeout=timedelta(minutes=3),
            retry_policy=RetryPolicy(
                initial_interval=timedelta(seconds=5),
                maximum_interval=timedelta(seconds=60),
                maximum_attempts=3,
            ),
        )

    async def _run_opencode(self, req: dict) -> dict:
        """OpenCode-agent path: a heavy agentic extraction activity followed
        by a cheap ingest activity. They're split so an ingest failure
        retries WITHOUT re-running the multi-minute agent (Temporal caches
        the extract result)."""
        passes = int(req.get("passes") or 1)
        # Each agentic pass is ~5-9 min; size the ceiling to the pass count.
        extract_minutes = 12 * max(1, min(5, passes)) + 5
        extracted = await workflow.execute_activity(
            opencode_extract_activity,
            req,
            start_to_close_timeout=timedelta(minutes=extract_minutes),
            heartbeat_timeout=timedelta(minutes=5),
            retry_policy=RetryPolicy(
                initial_interval=timedelta(seconds=10),
                maximum_interval=timedelta(seconds=120),
                maximum_attempts=2,
            ),
        )
        result = await workflow.execute_activity(
            ingest_facts_activity,
            {"req": req, "facts": extracted.get("facts", []),
             "passes": extracted.get("passes")},
            start_to_close_timeout=timedelta(minutes=15),
            heartbeat_timeout=timedelta(minutes=3),
            retry_policy=RetryPolicy(
                initial_interval=timedelta(seconds=5),
                maximum_interval=timedelta(seconds=60),
                maximum_attempts=4,
            ),
        )
        if isinstance(result, dict):
            result["passes"] = extracted.get("passes")
        return result

    @workflow.query
    def status(self) -> dict:
        return {
            "status": self._status,
            "holder": self._holder,
            "session_id": self._session_id,
            "mode": self._mode,
            "facts_ingested": self._facts_ingested,
            "error": self._error,
        }
