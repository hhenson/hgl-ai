#!/usr/bin/env python3
"""Minimal 5-tool stdio MCP server backed by NATS JetStream (`hgl-mcp`)."""

from __future__ import annotations

import asyncio
import json
import sys
from typing import Any

from hgl_ai.bus import VALID_KINDS, HglBus

MCP_TOOLS = [
    {
        "name": "post_note",
        "description": "Publish an atomic HGL note to another agent (claude, coord, desk, w1..w9, or all) via NATS JetStream.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "sender": {"type": "string", "description": "Sender ID (e.g. w1, coord, claude)"},
                "recipient": {"type": "string", "description": "Recipient ID (e.g. claude, w3, coord, all)"},
                "issue": {"type": "integer", "description": "GitHub issue/PR number, or 0 if none"},
                "kind": {"type": "string", "enum": sorted(VALID_KINDS)},
                "body": {"type": "string", "description": "Markdown note body (e.g. '[codex] Ready: ...')"},
            },
            "required": ["sender", "recipient", "issue", "kind", "body"],
        },
    },
    {
        "name": "poll_inbox",
        "description": "Fetch and acknowledge pending notes from an agent's durable NATS JetStream inbox.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "recipient": {"type": "string", "description": "Agent inbox name (e.g. claude, w1, coord)"},
                "limit": {"type": "integer", "default": 20},
            },
            "required": ["recipient"],
        },
    },
    {
        "name": "get_state",
        "description": "Read shared HGL state from JetStream KV ('prs', 'main', 'landed.log', or 'hold').",
        "inputSchema": {
            "type": "object",
            "properties": {"key": {"type": "string"}},
            "required": ["key"],
        },
    },
    {
        "name": "put_state",
        "description": "Write shared HGL state in JetStream KV ('prs', 'main', 'landed.log', 'hold'). Empty string deletes.",
        "inputSchema": {
            "type": "object",
            "properties": {"key": {"type": "string"}, "value": {"type": "string"}},
            "required": ["key", "value"],
        },
    },
    {
        "name": "list_hosts",
        "description": "List online HGL worker machines (auto-expiring via JetStream KV TTL) and their memcap budgets.",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


async def dispatch_rpc(bus: HglBus, payload: dict[str, Any]) -> dict[str, Any] | None:
    method = payload.get("method")
    req_id = payload.get("id")
    params = payload.get("params") or {}

    if method == "notifications/initialized":
        return None
    if method == "ping":
        return {"jsonrpc": "2.0", "id": req_id, "result": {}}
    if method == "initialize":
        return {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "protocolVersion": params.get("protocolVersion", "2024-11-05"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "hgl-nats-mcp", "version": "0.2.0"},
            },
        }
    if method == "tools/list":
        return {"jsonrpc": "2.0", "id": req_id, "result": {"tools": MCP_TOOLS}}
    if method == "tools/call":
        name = params.get("name")
        args = params.get("arguments") or {}
        try:
            await bus.connect()
            if name == "post_note":
                res = await bus.post_note(
                    sender=args["sender"],
                    recipient=args["recipient"],
                    issue=int(args["issue"]),
                    kind=args["kind"],
                    body=args["body"],
                )
            elif name == "poll_inbox":
                res = await bus.poll_inbox(recipient=args["recipient"], limit=int(args.get("limit", 20)))
            elif name == "get_state":
                res = await bus.get_state(args["key"])
            elif name == "put_state":
                res = await bus.put_state(args["key"], args["value"])
            elif name == "list_hosts":
                res = await bus.list_hosts()
            else:
                raise ValueError(f"Unknown tool: {name}")
            return {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {"content": [{"type": "text", "text": json.dumps(res, indent=2)}]},
            }
        except Exception as exc:  # noqa: BLE001
            return {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "isError": True,
                    "content": [{"type": "text", "text": f"Error: {exc}"}],
                },
            }
    return {
        "jsonrpc": "2.0",
        "id": req_id,
        "error": {"code": -32601, "message": f"Method not found: {method}"},
    }


async def serve_stdio() -> None:
    bus = HglBus()
    loop = asyncio.get_running_loop()
    reader = asyncio.StreamReader()
    protocol = asyncio.StreamReaderProtocol(reader)
    await loop.connect_read_pipe(lambda: protocol, sys.stdin)

    try:
        while True:
            line = await reader.readline()
            if not line:
                break
            text = line.decode("utf-8").strip()
            if not text:
                continue
            reply = await dispatch_rpc(bus, json.loads(text))
            if reply is not None:
                sys.stdout.write(json.dumps(reply) + "\n")
                sys.stdout.flush()
    finally:
        await bus.close()


def main() -> None:
    asyncio.run(serve_stdio())


if __name__ == "__main__":
    main()
