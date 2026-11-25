"""Core NATS JetStream stream + Key-Value client for HGL multi-agent coordination."""

from __future__ import annotations

import json
import os
import re
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any

import nats
from nats.aio.client import Client as NATSClient
from nats.aio.msg import Msg
from nats.errors import TimeoutError as NatsTimeoutError
from nats.js.api import (
    AckPolicy,
    ConsumerConfig,
    DeliverPolicy,
    KeyValueConfig,
    RetentionPolicy,
    StorageType,
    StreamConfig,
)
from nats.js.client import JetStreamContext
from nats.js.errors import KeyNotFoundError, NotFoundError
from nats.js.kv import KeyValue

STAMP_FMT = "%Y%m%dT%H%M%SZ"
NOTES_STREAM = "HGL_NOTES"
NOTES_SUBJECT_PREFIX = "hgl.notes"
STATE_BUCKET = "HGL_STATE"
HOSTS_BUCKET = "HGL_HOSTS"

AGENT_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9_-]{0,31}$")
STATE_KEY_RE = re.compile(r"^[a-zA-Z0-9_./=-]{1,64}$")
LEGACY_NOTE_RE = re.compile(
    r"^(?P<ts>\d{8}T\d{6}Z)-(?P<sender>[a-z0-9_-]+)-(?P<recipient>[a-z0-9_-]+)-(?P<issue>\d+)-(?P<kind>[a-z]+)\.md$"
)

VALID_KINDS = frozenset({
    "ready",
    "blocked",
    "done",
    "question",
    "answer",
    "sendback",
    "approved",
    "landed",
    "note",
})


def utc_now() -> str:
    return datetime.now(timezone.utc).strftime(STAMP_FMT)


def validate_agent_id(name: str) -> str:
    if not AGENT_NAME_RE.match(name):
        raise ValueError(f"Invalid agent ID {name!r}; must match {AGENT_NAME_RE.pattern}")
    return name


def validate_state_key(key: str) -> str:
    if not STATE_KEY_RE.match(key):
        raise ValueError(f"Invalid state key {key!r}")
    return key


@dataclass
class PendingNote:
    """Decoded JetStream note paired with its underlying NATS message handle for ack/nak."""

    seq: int
    data: dict[str, Any]
    msg: Msg


class HglBus:
    """Async NATS JetStream client backing notes, cursors, shared state, and host heartbeats."""

    def __init__(
        self,
        url: str | None = None,
        token: str | None = None,
    ) -> None:
        self.url = url or os.environ.get("NATS_URL", "nats://127.0.0.1:4222")
        self.token = token if token is not None else os.environ.get("NATS_TOKEN")
        self._nc: NATSClient | None = None
        self._js: JetStreamContext | None = None
        self._kv_state: KeyValue | None = None
        self._kv_hosts: KeyValue | None = None

    async def connect(self, ensure_schema: bool = True) -> "HglBus":
        if self._nc is not None and self._nc.is_connected:
            return self
        opts: dict[str, Any] = {
            "servers": [self.url],
            "name": f"hgl-ai-{os.uname().nodename}",
            "reconnect_time_wait": 2,
            "max_reconnect_attempts": -1,
        }
        if self.token:
            opts["token"] = self.token
        if creds := os.environ.get("NATS_CREDS"):
            opts["user_credentials"] = creds

        self._nc = await nats.connect(**opts)
        self._js = self._nc.jetstream()
        if ensure_schema:
            await self.ensure_topology()
        else:
            self._kv_state = await self._js.key_value(STATE_BUCKET)
            self._kv_hosts = await self._js.key_value(HOSTS_BUCKET)
        return self

    async def close(self) -> None:
        if self._nc is not None and not self._nc.is_closed:
            await self._nc.flush(timeout=2)
            await self._nc.close()
        self._nc = None

    async def __aenter__(self) -> "HglBus":
        return await self.connect()

    async def __aexit__(self, *exc: object) -> None:
        await self.close()

    @property
    def js(self) -> JetStreamContext:
        if self._js is None:
            raise RuntimeError("HglBus is not connected")
        return self._js

    async def ensure_topology(self) -> None:
        """Idempotently provision HGL_NOTES stream plus HGL_STATE and HGL_HOSTS KV buckets."""
        stream_cfg = StreamConfig(
            name=NOTES_STREAM,
            subjects=[f"{NOTES_SUBJECT_PREFIX}.>"],
            retention=RetentionPolicy.LIMITS,
            storage=StorageType.FILE,
            max_age=14 * 86400,  # 14 days (matches /tmp/hgl-queue 10-day retention)
            duplicate_window=86400,  # 24h deduplication via Nats-Msg-Id
        )
        try:
            await self.js.stream_info(NOTES_STREAM)
            await self.js.update_stream(stream_cfg)
        except NotFoundError:
            await self.js.add_stream(stream_cfg)

        try:
            self._kv_state = await self.js.key_value(STATE_BUCKET)
        except NotFoundError:
            self._kv_state = await self.js.create_key_value(
                KeyValueConfig(
                    bucket=STATE_BUCKET,
                    history=5,
                    storage=StorageType.FILE,
                    description="Shared HGL state (prs, main, landed.log, hold)",
                )
            )

        try:
            self._kv_hosts = await self.js.key_value(HOSTS_BUCKET)
        except NotFoundError:
            self._kv_hosts = await self.js.create_key_value(
                KeyValueConfig(
                    bucket=HOSTS_BUCKET,
                    history=1,
                    ttl=120,  # Dead hosts expire automatically after 120s
                    storage=StorageType.MEMORY,
                    description="Live HGL worker host heartbeats and memcap budgets",
                )
            )

    async def post_note(
        self,
        sender: str,
        recipient: str,
        issue: int,
        kind: str,
        body: str,
        created_at: str | None = None,
        msg_id: str | None = None,
    ) -> dict[str, Any]:
        validate_agent_id(sender)
        validate_agent_id(recipient)
        if kind not in VALID_KINDS:
            raise ValueError(f"Invalid kind {kind!r}; expected one of {sorted(VALID_KINDS)}")

        stamp = created_at or utc_now()
        envelope = {
            "created_at": stamp,
            "sender": sender,
            "recipient": recipient,
            "issue": int(issue),
            "kind": kind,
            "body": body.rstrip() + "\n",
        }
        headers = {"Nats-Msg-Id": msg_id} if msg_id else None
        ack = await self.js.publish(
            f"{NOTES_SUBJECT_PREFIX}.{recipient}",
            json.dumps(envelope).encode("utf-8"),
            headers=headers,
        )
        return {
            "seq": ack.seq,
            "duplicate": bool(getattr(ack, "duplicate", False)),
            **envelope,
        }

    async def open_inbox_subscription(self, recipient: str, ack_wait: float = 300.0):
        """Bind or create a durable explicit-ack pull consumer for `recipient` (+ broadcast `all`)."""
        validate_agent_id(recipient)
        durable = f"inbox-{recipient}"
        cfg = ConsumerConfig(
            durable_name=durable,
            ack_policy=AckPolicy.EXPLICIT,
            deliver_policy=DeliverPolicy.ALL,
            ack_wait=ack_wait,
            max_deliver=5,
            filter_subjects=[
                f"{NOTES_SUBJECT_PREFIX}.{recipient}",
                f"{NOTES_SUBJECT_PREFIX}.all",
            ],
        )
        return await self.js.pull_subscribe_bind(durable, stream=NOTES_STREAM) if await self._consumer_exists(durable) else await self.js.pull_subscribe(
            "",
            durable=durable,
            stream=NOTES_STREAM,
            config=cfg,
        )

    async def _consumer_exists(self, durable: str) -> bool:
        try:
            await self.js.consumer_info(NOTES_STREAM, durable)
            return True
        except NotFoundError:
            return False

    async def fetch_pending(
        self,
        sub,
        batch: int = 20,
        timeout: float = 1.0,
    ) -> list[PendingNote]:
        """Fetch up to `batch` unacknowledged notes without auto-acking them."""
        try:
            msgs = await sub.fetch(batch=batch, timeout=timeout)
        except NatsTimeoutError:
            return []
        out: list[PendingNote] = []
        for msg in msgs:
            payload = json.loads(msg.data.decode("utf-8"))
            seq = msg.metadata.sequence.stream
            payload["seq"] = seq
            out.append(PendingNote(seq=seq, data=payload, msg=msg))
        return out

    async def poll_inbox(self, recipient: str, limit: int = 20, ack: bool = True) -> list[dict[str, Any]]:
        """Convenience fetch for interactive MCP tools / CLI inspection."""
        sub = await self.open_inbox_subscription(recipient)
        try:
            pending = await self.fetch_pending(sub, batch=limit, timeout=0.5)
            results: list[dict[str, Any]] = []
            for item in pending:
                results.append(item.data)
                if ack:
                    await item.msg.ack()
            return results
        finally:
            await sub.unsubscribe()

    async def get_state(self, key: str) -> dict[str, Any] | None:
        validate_state_key(key)
        assert self._kv_state is not None
        try:
            entry = await self._kv_state.get(key)
        except KeyNotFoundError:
            return None
        if entry.value is None:
            return None
        return {
            "key": key,
            "value": entry.value.decode("utf-8"),
            "revision": entry.revision,
        }

    async def put_state(self, key: str, value: str) -> dict[str, Any]:
        validate_state_key(key)
        assert self._kv_state is not None
        if value == "":
            await self._kv_state.purge(key)
            return {"key": key, "deleted": True, "updated_at": utc_now()}
        rev = await self._kv_state.put(key, value.encode("utf-8"))
        return {"key": key, "revision": rev, "updated_at": utc_now()}

    async def heartbeat_host(self, hostname: str, agents: list[str], memcap_gb: int) -> dict[str, Any]:
        assert self._kv_hosts is not None
        record = {
            "hostname": hostname,
            "agents": agents,
            "memcap_gb": int(memcap_gb),
            "updated_at": utc_now(),
        }
        key = re.sub(r"[^a-zA-Z0-9_.-]", "_", hostname)
        await self._kv_hosts.put(key, json.dumps(record).encode("utf-8"))
        return record

    async def list_hosts(self) -> list[dict[str, Any]]:
        assert self._kv_hosts is not None
        try:
            keys = await self._kv_hosts.keys()
        except Exception:  # noqa: BLE001 — bucket empty raises NoKeysError
            return []
        hosts: list[dict[str, Any]] = []
        for key in sorted(keys):
            try:
                entry = await self._kv_hosts.get(key)
                if entry.value:
                    hosts.append(json.loads(entry.value.decode("utf-8")))
            except KeyNotFoundError:
                continue
        return hosts
