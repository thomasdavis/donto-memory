"""Python Temporal worker + enqueue gateway for the donto-memory queue.

Runs two things in one asyncio process:

  1. A Temporal worker on task queue ``memory-extraction`` hosting
     ``MemorizeWorkflow`` + ``memorize_activity``.
  2. A tiny aiohttp HTTP server on :7901 exposing ``POST /enqueue`` so
     the Rust donto-memory service (which is not a Temporal client) can
     start a workflow with a single localhost HTTP call.

The Rust side generates the queue_id (a UUID) and passes it as
``workflow_id`` so the audit-log row, the 202 response, and the Temporal
workflow all share one identifier.

Run:  python worker.py
Env:  TEMPORAL_ADDRESS (default localhost:7233)
      ENQUEUE_BIND      (default 0.0.0.0:7901)
      MAX_CONCURRENT_ACTIVITIES (default 4 — deep runs are heavy)
"""

import asyncio
import logging
import os

from aiohttp import web
from temporalio.client import Client
from temporalio.worker import Worker

from workflows import MemorizeWorkflow
from activities import memorize_activity

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(name)s: %(message)s",
)
logger = logging.getLogger("memory-worker")

TEMPORAL_ADDRESS = os.environ.get("TEMPORAL_ADDRESS", "localhost:7233")
TASK_QUEUE = "memory-extraction"
# Deep runs hold an activity slot for many minutes and each makes
# sequential heavy LLM calls. Keep concurrency low so we don't hammer
# OpenRouter or the substrate; raise via env if needed.
MAX_CONCURRENT = int(os.environ.get("MAX_CONCURRENT_ACTIVITIES", "4"))
ENQUEUE_BIND = os.environ.get("ENQUEUE_BIND", "0.0.0.0:7901")


def _make_app(client: Client) -> web.Application:
    app = web.Application()

    async def enqueue(request: web.Request) -> web.Response:
        try:
            payload = await request.json()
        except Exception:  # noqa: BLE001
            return web.json_response({"error": "invalid JSON body"}, status=400)

        req = payload.get("req")
        workflow_id = payload.get("workflow_id")
        if not isinstance(req, dict) or not workflow_id:
            return web.json_response(
                {"error": "expected {workflow_id, req:{...}}"}, status=400
            )

        try:
            handle = await client.start_workflow(
                MemorizeWorkflow.run,
                req,
                id=str(workflow_id),
                task_queue=TASK_QUEUE,
            )
            logger.info(
                "enqueued workflow id=%s holder=%s mode=%s",
                handle.id, req.get("holder"), req.get("mode"),
            )
            return web.json_response(
                {"workflow_id": handle.id, "run_id": handle.result_run_id}
            )
        except Exception as e:  # noqa: BLE001
            # Most commonly WorkflowExecutionAlreadyStarted (duplicate
            # workflow_id) — surface it so the caller can treat it as
            # already-queued rather than an error.
            logger.warning("enqueue failed id=%s: %s", workflow_id, e)
            return web.json_response(
                {"error": str(e), "workflow_id": str(workflow_id)}, status=409
            )

    async def healthz(_request: web.Request) -> web.Response:
        return web.json_response({"ok": True, "task_queue": TASK_QUEUE})

    app.add_routes([
        web.post("/enqueue", enqueue),
        web.get("/healthz", healthz),
    ])
    return app


async def main() -> None:
    client = await Client.connect(TEMPORAL_ADDRESS)
    logger.info("connected to Temporal at %s", TEMPORAL_ADDRESS)

    worker = Worker(
        client,
        task_queue=TASK_QUEUE,
        workflows=[MemorizeWorkflow],
        activities=[memorize_activity],
        max_concurrent_activities=MAX_CONCURRENT,
        max_concurrent_workflow_tasks=MAX_CONCURRENT,
    )

    # Start the enqueue HTTP server alongside the worker in the same loop.
    host, _, port = ENQUEUE_BIND.partition(":")
    app = _make_app(client)
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, host or "0.0.0.0", int(port or "7901"))
    await site.start()
    logger.info("enqueue gateway listening on %s", ENQUEUE_BIND)

    logger.info(
        "worker listening on queue=%s max_concurrent=%s", TASK_QUEUE, MAX_CONCURRENT
    )
    try:
        await worker.run()
    finally:
        await runner.cleanup()


if __name__ == "__main__":
    asyncio.run(main())
