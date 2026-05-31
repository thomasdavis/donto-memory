"""Reusable headless OpenCode agentic runner.

This is the foundational abstraction for driving OpenCode (the agentic
coding tool) as a programmable AI step anywhere in the system — memory
extraction is the first consumer, but it is deliberately generic so other
agentic tasks (summarisation, classification, code transforms, report
generation) can reuse it.

Why it execs into the omega-bot container
-----------------------------------------
OpenCode only works against the GLM coding subscription when its provider
config is honoured. That works reliably *inside the omega-bot container*
(which has GLM_API_KEY in its env and a proven OpenCode install); the
host's `opencode` did not honour OPENCODE_CONFIG_CONTENT in testing. So we
`docker exec` into the bot container.

File exchange via the shared bind mount
---------------------------------------
The bot container mounts the host dir `/data/omega/shared` at `/data`.
We create a per-run scratch dir under it, so:

    host       /data/omega/shared/oc/<run_id>/
    container  /data/oc/<run_id>/

is the *same directory*. We write the prompt + any input files there, run
opencode with that dir as CWD, and read back any output files the agent
wrote — no stdin/stdout plumbing, and large outputs are captured reliably.

Example
-------
    agent = OpenCodeAgent(model="glm-4.7")
    run = agent.run(
        prompt="Read input.txt and write a JSON summary to out.json",
        input_files={"input.txt": some_text},
        output_files=["out.json"],
        timeout=300,
    )
    if run.ok:
        data = json.loads(run.output_files["out.json"])
"""

from __future__ import annotations

import json
import logging
import os
import shutil
import subprocess
import time
import uuid
from dataclasses import dataclass, field

logger = logging.getLogger("opencode-agent")

DEFAULT_CONTAINER = os.environ.get("OPENCODE_CONTAINER", "omega-vm-omega-bot-1")
DEFAULT_MODEL = os.environ.get("OPENCODE_MODEL", "glm-4.7")
DEFAULT_BASE_URL = os.environ.get(
    "OPENCODE_BASE_URL", "https://api.z.ai/api/coding/paas/v4"
)
DEFAULT_API_KEY_ENV = os.environ.get("OPENCODE_API_KEY_ENV", "GLM_API_KEY")
# Host side of the shared bind mount, and where it appears in the container.
HOST_SCRATCH = os.environ.get("OPENCODE_HOST_SCRATCH", "/data/omega/shared/oc")
CONTAINER_SCRATCH = os.environ.get("OPENCODE_CONTAINER_SCRATCH", "/data/oc")
DOCKER = os.environ.get("DOCKER_BIN", "docker")


@dataclass
class AgentRun:
    """Result of one OpenCode agentic run."""
    run_id: str
    exit_code: int
    elapsed_s: float
    text: str                                   # assembled assistant text
    output_files: dict[str, str] = field(default_factory=dict)
    event_counts: dict[str, int] = field(default_factory=dict)
    stderr_tail: str = ""
    timed_out: bool = False

    @property
    def ok(self) -> bool:
        return self.exit_code == 0 and not self.timed_out


class OpenCodeAgentError(RuntimeError):
    pass


class OpenCodeAgent:
    """Drives `opencode run` headlessly inside a container, exchanging files
    over a shared bind mount. Stateless and cheap to construct — make one per
    task type or reuse a module-level instance."""

    def __init__(
        self,
        *,
        container: str = DEFAULT_CONTAINER,
        model: str = DEFAULT_MODEL,
        base_url: str = DEFAULT_BASE_URL,
        api_key_env: str = DEFAULT_API_KEY_ENV,
        host_scratch: str = HOST_SCRATCH,
        container_scratch: str = CONTAINER_SCRATCH,
        keep_runs_on_error: bool = True,
    ) -> None:
        self.container = container
        self.model = model
        self.base_url = base_url
        self.api_key_env = api_key_env
        self.host_scratch = host_scratch
        self.container_scratch = container_scratch
        self.keep_runs_on_error = keep_runs_on_error

    def _config_json(self, model: str) -> str:
        """OpenCode provider config. apiKey is an {env:...} reference so the
        key never leaves the container's own environment."""
        return json.dumps({
            "provider": {
                "z-ai": {
                    "api": "openai",
                    "options": {
                        "apiKey": "{env:%s}" % self.api_key_env,
                        "baseURL": self.base_url,
                    },
                    "models": {model: {"id": model, "name": model}},
                }
            },
            "model": f"z-ai/{model}",
        })

    def run(
        self,
        prompt: str,
        *,
        input_files: dict[str, str] | None = None,
        output_files: list[str] | None = None,
        timeout: int = 600,
        model: str | None = None,
    ) -> AgentRun:
        """Run one agentic task.

        prompt        — the instruction. Reference input/output files by their
                        bare names; they live in the run's CWD.
        input_files   — {name: content} written into the run dir before launch.
        output_files  — names to read back from the run dir afterwards.
        timeout       — hard wall-clock cap (seconds).
        model         — override the default model for this run.
        """
        model = model or self.model
        run_id = uuid.uuid4().hex[:16]
        host_dir = os.path.join(self.host_scratch, run_id)
        cont_dir = f"{self.container_scratch}/{run_id}"
        os.makedirs(host_dir, exist_ok=True)

        # Materialise prompt + inputs in the shared run dir.
        with open(os.path.join(host_dir, "prompt.txt"), "w") as f:
            f.write(prompt)
        for name, content in (input_files or {}).items():
            with open(os.path.join(host_dir, name), "w") as f:
                f.write(content)

        # Run opencode inside the container with that dir as CWD. The prompt
        # is read from prompt.txt to avoid any shell-quoting of large text.
        # `timeout` (coreutils) bounds the agent; we add slack on the
        # subprocess timeout so we capture opencode's own cleanup.
        inner = (
            f'cd {cont_dir} && '
            f'timeout {timeout} opencode run --dangerously-skip-permissions '
            f'--format json "$(cat prompt.txt)"'
        )
        argv = [
            DOCKER, "exec",
            "-e", f"OPENCODE_CONFIG_CONTENT={self._config_json(model)}",
            self.container, "sh", "-c", inner,
        ]

        t0 = time.time()
        timed_out = False
        try:
            proc = subprocess.run(
                argv, capture_output=True, text=True, timeout=timeout + 60
            )
            stdout, stderr, rc = proc.stdout, proc.stderr, proc.returncode
        except subprocess.TimeoutExpired as e:
            timed_out = True
            stdout = e.stdout.decode() if isinstance(e.stdout, bytes) else (e.stdout or "")
            stderr = e.stderr.decode() if isinstance(e.stderr, bytes) else (e.stderr or "")
            rc = 124
        elapsed = time.time() - t0

        text, counts = _parse_events(stdout)

        # Read requested output files back from the shared run dir.
        outputs: dict[str, str] = {}
        for name in (output_files or []):
            p = os.path.join(host_dir, name)
            if os.path.exists(p):
                with open(p) as f:
                    outputs[name] = f.read()

        run = AgentRun(
            run_id=run_id,
            exit_code=rc,
            elapsed_s=elapsed,
            text=text,
            output_files=outputs,
            event_counts=counts,
            stderr_tail=(stderr or "")[-500:],
            timed_out=timed_out,
        )
        logger.info(
            "opencode run id=%s model=%s exit=%s elapsed=%.1fs events=%s outputs=%s",
            run_id, model, rc, elapsed, counts, {k: len(v) for k, v in outputs.items()},
        )

        # Cleanup: keep the dir on failure for debugging, remove on success.
        if run.ok and not (output_files and not outputs):
            shutil.rmtree(host_dir, ignore_errors=True)
        elif not self.keep_runs_on_error:
            shutil.rmtree(host_dir, ignore_errors=True)

        return run


def _parse_events(stdout: str) -> tuple[str, dict[str, int]]:
    """Assemble assistant text from opencode's `--format json` event stream
    and count event types. Tolerant of non-JSON lines."""
    text_parts: list[str] = []
    counts: dict[str, int] = {}
    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            ev = json.loads(line)
        except Exception:
            counts["_nonjson"] = counts.get("_nonjson", 0) + 1
            continue
        et = ev.get("type", "?")
        counts[et] = counts.get(et, 0) + 1
        part = ev.get("part") or {}
        if et == "text" and part.get("text"):
            text_parts.append(part["text"])
    return "".join(text_parts), counts
