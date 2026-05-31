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
    from activities import memorize_activity


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
        self._mode = req.get("mode") or "single"
        self._status = "extracting"

        # Deep/exhaustive runs can take many minutes (7 sequential LLM
        # passes were observed at ~12-14 min). Give the activity a
        # generous ceiling and retry transient failures a couple times.
        # heartbeat_timeout lets Temporal detect a hung/killed worker
        # well before start_to_close so a restart re-dispatches fast.
        try:
            result = await workflow.execute_activity(
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
            self._status = "done"
            self._facts_ingested = (result or {}).get("facts_ingested", 0)
            return result
        except Exception as e:  # noqa: BLE001 — surface the failure on the workflow
            self._status = "failed"
            self._error = str(e)
            raise

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
