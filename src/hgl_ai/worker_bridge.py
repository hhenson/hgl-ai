#!/usr/bin/env python3
"""Async NATS JetStream worker bridge waking Antigravity, Codex, and Claude CLI sessions."""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import json
import socket
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from hgl_ai.bus import HglBus, PendingNote


@dataclass
class WorkerSpec:
    """Binds a logical HGL agent ID (e.g. w1, verifier) to a local CLI runtime and workdir."""

    agent_id: str
    backend: str  # "agy" | "codex" | "claude"
    model: str | None
    workdir: Path

    @classmethod
    def parse(cls, spec: str) -> "WorkerSpec":
        # Formats:
        #   w1:codex:/path/to/workdir
        #   w2:agy:gemini-4-carbon-high:/path/to/workdir
        #   verifier:agy:claude-opus-5-5-high:/path/to/workdir
        parts = spec.split(":")
        if len(parts) == 3:
            agent_id, backend, workdir = parts
            model = None
        elif len(parts) == 4:
            agent_id, backend, model, workdir = parts
        else:
            raise ValueError(f"Invalid --worker spec {spec!r}; expected id:backend[:model]:workdir")
        if backend not in {"agy", "codex", "claude"}:
            raise ValueError(f"Unsupported backend {backend!r}; expected agy, codex, or claude")
        return cls(agent_id=agent_id, backend=backend, model=model or None, workdir=Path(workdir))


class WarmAgySession:
    """Persistent async `agy --input-format stream-json --output-format stream-json` process."""

    def __init__(self, workdir: Path, model: str | None) -> None:
        self.workdir = workdir
        self.model = model
        self._proc: asyncio.subprocess.Process | None = None

    async def ensure_started(self) -> asyncio.subprocess.Process:
        if self._proc is not None and self._proc.returncode is None:
            return self._proc
        cmd = ["agy", "--input-format", "stream-json", "--output-format", "stream-json"]
        if self.model:
            cmd.extend(["--model", self.model])
        self._proc = await asyncio.create_subprocess_exec(
            *cmd,
            cwd=str(self.workdir),
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
        )
        return self._proc

    async def send_turn(self, prompt: str) -> bool:
        proc = await self.ensure_started()
        assert proc.stdin and proc.stdout
        event = {"event": "user", "message": {"content": prompt}}
        proc.stdin.write((json.dumps(event) + "\n").encode("utf-8"))
        await proc.stdin.drain()

        while True:
            raw = await proc.stdout.readline()
            if not raw:
                return False
            msg = json.loads(raw.decode("utf-8"))
            if msg.get("event") == "result":
                result = msg.get("result") or {}
                return result.get("status") == "SUCCESS"


class WorkerRunner:
    """Consumes notes from JetStream with in-progress heartbeats and explicit post-turn ack/nak."""

    def __init__(self, spec: WorkerSpec) -> None:
        self.spec = spec
        self.spec.workdir.mkdir(parents=True, exist_ok=True)
        self._agy = WarmAgySession(self.spec.workdir, self.spec.model)
        self._state_file = self.spec.workdir / f".hgl-worker-{self.spec.agent_id}.json"

    def _load_state(self) -> dict[str, Any]:
        if self._state_file.is_file():
            return json.loads(self._state_file.read_text(encoding="utf-8"))
        return {}

    def _save_state(self, state: dict[str, Any]) -> None:
        tmp = self._state_file.with_suffix(".tmp")
        tmp.write_text(json.dumps(state, indent=2) + "\n", encoding="utf-8")
        tmp.replace(self._state_file)

    async def run_forever(self, bus: HglBus) -> None:
        sub = await bus.open_inbox_subscription(self.spec.agent_id, ack_wait=300.0)
        while True:
            # Long-polling fetch on NATS TCP socket: instant push wakeup when a note arrives,
            # zero LLM token cost while waiting.
            pending = await bus.fetch_pending(sub, batch=10, timeout=25.0)
            if not pending:
                continue

            hold_entry = await bus.get_state("hold")
            hold_notice = hold_entry["value"] if hold_entry else None
            print(f"[{self.spec.agent_id}] waking for {len(pending)} note(s) (seqs {[p.seq for p in pending]})")

            keepalive = asyncio.create_task(self._keep_in_progress(pending))
            try:
                ok = await self._execute_turn(pending, hold_notice)
            except Exception as exc:  # noqa: BLE001
                print(f"[{self.spec.agent_id}] turn crashed: {exc}")
                ok = False
            finally:
                keepalive.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await keepalive

            for item in pending:
                if ok:
                    await item.msg.ack()
                else:
                    # Redeliver after 15s backoff if worker crashed, hit quota, or OOM'd under memcap
                    await item.msg.nak(delay=15)

    async def _keep_in_progress(self, pending: list[PendingNote]) -> None:
        """Extend JetStream ack_wait while long compiler/test runs execute."""
        while True:
            await asyncio.sleep(60)
            for item in pending:
                with contextlib.suppress(Exception):
                    await item.msg.in_progress()

    def _format_prompt(self, pending: list[PendingNote], hold_notice: str | None) -> str:
        blocks = [f"New HGL JetStream notes for `{self.spec.agent_id}` on host `{socket.gethostname()}`:"]
        if hold_notice:
            blocks.append(f"IMPORTANT — GitHub API hold active (`state/hold`): {hold_notice.strip()}")
        for item in pending:
            n = item.data
            blocks.append(
                f"--- Note seq={item.seq} ({n['created_at']}) from={n['sender']} issue={n['issue']} kind={n['kind']} ---\n"
                f"{n['body'].strip()}"
            )
        blocks.append("Handle these notes and send any reply using the `post_note` MCP tool.")
        return "\n\n".join(blocks)

    async def _execute_turn(self, pending: list[PendingNote], hold_notice: str | None) -> bool:
        prompt = self._format_prompt(pending, hold_notice)
        if self.spec.backend == "agy":
            return await self._agy.send_turn(prompt)
        if self.spec.backend == "codex":
            return await self._run_codex(prompt)
        if self.spec.backend == "claude":
            return await self._run_claude(prompt)
        return False

    async def _run_codex(self, prompt: str) -> bool:
        state = self._load_state()
        thread_id = state.get("codex_thread_id")
        if thread_id:
            cmd = ["codex", "exec", "resume", thread_id, "--json"]
        else:
            cmd = ["codex", "exec", "--json"]
        if self.spec.model:
            cmd.extend(["-m", self.spec.model])
        cmd.append(prompt)

        proc = await asyncio.create_subprocess_exec(
            *cmd,
            cwd=str(self.spec.workdir),
            stdout=asyncio.subprocess.PIPE,
        )
        assert proc.stdout
        while raw := await proc.stdout.readline():
            with contextlib.suppress(json.JSONDecodeError):
                ev = json.loads(raw.decode("utf-8"))
                if ev.get("type") == "thread.started" and ev.get("thread_id"):
                    state["codex_thread_id"] = ev["thread_id"]
                    self._save_state(state)
        return (await proc.wait()) == 0

    async def _run_claude(self, prompt: str) -> bool:
        state = self._load_state()
        cmd = ["claude", "-p", prompt, "--output-format", "json"]
        if self.spec.model:
            cmd.extend(["--model", self.spec.model])
        if session_id := state.get("claude_session_id"):
            cmd.extend(["--resume", session_id])

        proc = await asyncio.create_subprocess_exec(
            *cmd,
            cwd=str(self.spec.workdir),
            stdout=asyncio.subprocess.PIPE,
        )
        stdout, _ = await proc.communicate()
        if proc.returncode != 0:
            return False
        if stdout.strip():
            with contextlib.suppress(json.JSONDecodeError):
                out = json.loads(stdout.decode("utf-8"))
                if out.get("session_id"):
                    state["claude_session_id"] = out["session_id"]
                    self._save_state(state)
        return True


async def heartbeat_loop(bus: HglBus, hostname: str, agents: list[str], memcap_gb: int) -> None:
    while True:
        with contextlib.suppress(Exception):
            await bus.heartbeat_host(hostname, agents, memcap_gb)
        await asyncio.sleep(30)


async def run_bridge(args: argparse.Namespace) -> None:
    specs = [WorkerSpec.parse(item) for item in args.worker]
    hostname = socket.gethostname()
    async with HglBus(url=args.nats_url) as bus:
        print(f"hgl-bridge on {hostname} connected to {bus.url}; watching {[s.agent_id for s in specs]}")
        tasks = [
            asyncio.create_task(heartbeat_loop(bus, hostname, [s.agent_id for s in specs], args.memcap_gb)),
            *(asyncio.create_task(WorkerRunner(spec).run_forever(bus)) for spec in specs),
        ]
        await asyncio.gather(*tasks)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--nats-url", default=None, help="NATS server URL (default: $NATS_URL or nats://127.0.0.1:4222)")
    parser.add_argument("--memcap-gb", type=int, default=64, help="Advertised host RAM cap in GiB")
    parser.add_argument(
        "--worker",
        action="append",
        required=True,
        help="Worker binding: agent_id:backend[:model]:workdir (repeatable)",
    )
    args = parser.parse_args()
    with contextlib.suppress(KeyboardInterrupt):
        asyncio.run(run_bridge(args))


if __name__ == "__main__":
    main()
