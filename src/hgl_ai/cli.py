#!/usr/bin/env python3
"""Operator & script CLI (`hgl-ctl`) for NATS JetStream notes, state KV, and legacy import."""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
from pathlib import Path

from hgl_ai.bus import LEGACY_NOTE_RE, VALID_KINDS, HglBus


async def import_legacy_queue(bus: HglBus, queue_dir: Path) -> None:
    state_dir = queue_dir / "state"
    state_count = 0
    if state_dir.is_dir():
        for key in ("main", "landed.log", "hold", "prs"):
            path = state_dir / key
            if path.is_file():
                await bus.put_state(key, path.read_text(encoding="utf-8"))
                state_count += 1

    note_files: list[Path] = []
    for folder in ("to-claude", "to-codex"):
        sub = queue_dir / folder
        if sub.is_dir():
            note_files.extend(sorted(sub.glob("*.md")))
    note_files.sort(key=lambda p: p.name)

    imported = 0
    duplicates = 0
    for path in note_files:
        match = LEGACY_NOTE_RE.match(path.name)
        if not match:
            continue
        info = match.groupdict()
        if info["kind"] not in VALID_KINDS:
            continue
        res = await bus.post_note(
            sender=info["sender"],
            recipient=info["recipient"],
            issue=int(info["issue"]),
            kind=info["kind"],
            body=path.read_text(encoding="utf-8", errors="replace"),
            created_at=info["ts"],
            msg_id=f"legacy-{path.stem}",
        )
        if res.get("duplicate"):
            duplicates += 1
        else:
            imported += 1
    print(f"Imported {state_count} state keys and {imported} notes ({duplicates} deduplicated) from {queue_dir}")


async def async_main(args: argparse.Namespace) -> int:
    async with HglBus(url=args.nats_url) as bus:
        if args.cmd == "init":
            print(f"Provisioned JetStream stream HGL_NOTES and KV buckets HGL_STATE, HGL_HOSTS on {bus.url}")
        elif args.cmd == "import-queue":
            await import_legacy_queue(bus, args.queue_dir)
        elif args.cmd == "post":
            body = sys.stdin.read() if args.body == "-" else args.body
            res = await bus.post_note(args.sender, args.recipient, args.issue, args.kind, body)
            print(json.dumps(res, indent=2))
        elif args.cmd == "inbox":
            notes = await bus.poll_inbox(args.agent, limit=args.limit, ack=not args.peek)
            print(json.dumps(notes, indent=2))
        elif args.cmd == "state":
            if args.state_cmd == "get":
                entry = await bus.get_state(args.key)
                if entry is None:
                    return 1
                sys.stdout.write(entry["value"])
                if not entry["value"].endswith("\n"):
                    sys.stdout.write("\n")
            elif args.state_cmd == "put":
                val = sys.stdin.read() if args.value == "-" else args.value
                print(json.dumps(await bus.put_state(args.key, val)))
            elif args.state_cmd == "del":
                print(json.dumps(await bus.put_state(args.key, "")))
        elif args.cmd == "hosts":
            print(json.dumps(await bus.list_hosts(), indent=2))
    return 0


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--nats-url", default=None, help="NATS server URL (default: $NATS_URL or nats://127.0.0.1:4222)")
    sub = parser.add_subparsers(dest="cmd", required=True)

    sub.add_parser("init", help="Ensure HGL_NOTES stream and KV buckets exist")

    imp_p = sub.add_parser("import-queue", help="Import legacy file-queue state and notes idempotently")
    imp_p.add_argument("queue_dir", type=Path)

    post_p = sub.add_parser("post", help="Publish a note to HGL_NOTES")
    post_p.add_argument("--from", dest="sender", required=True)
    post_p.add_argument("--to", dest="recipient", required=True)
    post_p.add_argument("--issue", type=int, default=0)
    post_p.add_argument("--kind", choices=sorted(VALID_KINDS), required=True)
    post_p.add_argument("--body", required=True, help="Note text, or '-' to read from stdin")

    inbox_p = sub.add_parser("inbox", help="Fetch pending notes for an agent")
    inbox_p.add_argument("agent")
    inbox_p.add_argument("--limit", type=int, default=20)
    inbox_p.add_argument("--peek", action="store_true", help="Read without acknowledging")

    state_p = sub.add_parser("state", help="Read or write HGL_STATE KV keys (prs, main, landed.log, hold)")
    state_sub = state_p.add_subparsers(dest="state_cmd", required=True)
    sg = state_sub.add_parser("get")
    sg.add_argument("key")
    sp = state_sub.add_parser("put")
    sp.add_argument("key")
    sp.add_argument("value", help="Value string, or '-' to read from stdin")
    sd = state_sub.add_parser("del")
    sd.add_argument("key")

    sub.add_parser("hosts", help="List live worker machines from HGL_HOSTS KV")

    args = parser.parse_args()
    raise SystemExit(asyncio.run(async_main(args)))


if __name__ == "__main__":
    main()
