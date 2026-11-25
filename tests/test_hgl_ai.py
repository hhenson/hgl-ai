"""Unit and live-NATS integration tests for hgl-ai."""

from __future__ import annotations

import os
import unittest
from pathlib import Path
from unittest.mock import AsyncMock

from hgl_ai.bus import LEGACY_NOTE_RE, HglBus, validate_agent_id, validate_state_key
from hgl_ai.mcp_server import MCP_TOOLS, dispatch_rpc
from hgl_ai.worker_bridge import WorkerSpec


class UnitTests(unittest.IsolatedAsyncioTestCase):
    def test_validation_and_legacy_filename_regex(self) -> None:
        self.assertEqual(validate_agent_id("w3"), "w3")
        self.assertEqual(validate_agent_id("claude"), "claude")
        with self.assertRaises(ValueError):
            validate_agent_id("bad.*wildcard")
        self.assertEqual(validate_state_key("landed.log"), "landed.log")

        m = LEGACY_NOTE_RE.match("20261122T141500Z-w3-claude-532-ready.md")
        self.assertIsNotNone(m)
        assert m is not None
        self.assertEqual(m.group("sender"), "w3")
        self.assertEqual(m.group("recipient"), "claude")
        self.assertEqual(m.group("issue"), "532")
        self.assertEqual(m.group("kind"), "ready")

    def test_worker_spec_parsing(self) -> None:
        s1 = WorkerSpec.parse("w1:codex:/tmp/hgl-w1")
        self.assertEqual((s1.agent_id, s1.backend, s1.model), ("w1", "codex", None))
        s2 = WorkerSpec.parse("verifier:agy:claude-opus-5-5-high:/tmp/hgl-v")
        self.assertEqual((s2.agent_id, s2.backend, s2.model), ("verifier", "agy", "claude-opus-5-5-high"))

    async def test_mcp_rpc_dispatch(self) -> None:
        bus = AsyncMock(spec=HglBus)
        bus.post_note.return_value = {"seq": 7, "sender": "w1", "recipient": "claude", "issue": 42, "kind": "ready"}

        init_resp = await dispatch_rpc(bus, {"jsonrpc": "2.0", "id": 1, "method": "initialize"})
        assert init_resp is not None
        self.assertEqual(init_resp["result"]["serverInfo"]["name"], "hgl-nats-mcp")

        tools_resp = await dispatch_rpc(bus, {"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        assert tools_resp is not None
        self.assertEqual(len(tools_resp["result"]["tools"]), 5)
        self.assertEqual([t["name"] for t in MCP_TOOLS], ["post_note", "poll_inbox", "get_state", "put_state", "list_hosts"])

        call_resp = await dispatch_rpc(
            bus,
            {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "post_note",
                    "arguments": {
                        "sender": "w1",
                        "recipient": "claude",
                        "issue": 42,
                        "kind": "ready",
                        "body": "[codex] Ready: PR #42",
                    },
                },
            },
        )
        assert call_resp is not None
        self.assertNotIn("isError", call_resp["result"])
        bus.post_note.assert_awaited_once()


@unittest.skipUnless(os.environ.get("TEST_NATS_URL"), "Set TEST_NATS_URL to run live NATS JetStream roundtrip")
class LiveJetStreamTests(unittest.IsolatedAsyncioTestCase):
    async def test_jetstream_notes_nak_redelivery_and_kv(self) -> None:
        url = os.environ["TEST_NATS_URL"]
        async with HglBus(url=url) as bus:
            # 1. KV state roundtrip + delete
            await bus.put_state("prs", "# read 20261125T210000Z\nhgl-compiler\t532\tabc\topen\tclean\t0\tpass\n")
            prs = await bus.get_state("prs")
            assert prs is not None
            self.assertIn("hgl-compiler\t532", prs["value"])
            await bus.put_state("hold", "rate limit until 22:00Z")
            self.assertIsNotNone(await bus.get_state("hold"))
            await bus.put_state("hold", "")
            self.assertIsNone(await bus.get_state("hold"))

            # 2. Host heartbeat KV
            await bus.heartbeat_host("worker-box-b", ["w1", "w2"], memcap_gb=64)
            hosts = await bus.list_hosts()
            self.assertEqual(len(hosts), 1)
            self.assertEqual(hosts[0]["agents"], ["w1", "w2"])

            # 3. Durable stream note + NAK redelivery + ACK + deduplication
            sub = await bus.open_inbox_subscription("claude")
            r1 = await bus.post_note("w1", "claude", 532, "ready", "[codex] Ready: #532", msg_id="test-532")
            r_dup = await bus.post_note("w1", "claude", 532, "ready", "[codex] Ready: #532", msg_id="test-532")
            self.assertFalse(r1["duplicate"])
            self.assertTrue(r_dup["duplicate"])

            # Simulate worker crash (NAK) -> note is immediately redelivered!
            batch1 = await bus.fetch_pending(sub, batch=5, timeout=2.0)
            self.assertEqual(len(batch1), 1)
            await batch1[0].msg.nak()

            batch2 = await bus.fetch_pending(sub, batch=5, timeout=2.0)
            self.assertEqual(len(batch2), 1)
            self.assertEqual(batch2[0].data["issue"], 532)
            await batch2[0].msg.ack()

            # Once acked, inbox is empty
            self.assertEqual(await bus.fetch_pending(sub, batch=5, timeout=0.3), [])


if __name__ == "__main__":
    unittest.main()
