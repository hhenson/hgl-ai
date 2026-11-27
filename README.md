# hgl-ai

NATS JetStream coordination bus, `stdio` MCP server (`hgl-mcp`), and async worker bridge (`hgl-bridge`) for running mixed HGL coding agents—Antigravity (Gemini and Claude), OpenAI Codex, and Claude Code—across multiple Linux hosts.

GitHub holds issues, pull requests, reviews, and merges. `hgl-ai` provides the low-latency inter-agent transport, shared PR/hold state, and host-isolated worker wakeups.

- **[Communication Protocol Specification](docs/protocol.md)** — Wire envelopes, note kinds, `HGL_NOTES` / `HGL_STATE` / `HGL_HOSTS` schemas, gate receipts, and MCP tools.
- **[Roles, Governance Rules, and Multi-Host Distribution](docs/roles-and-distribution.md)** — Controller, Coordinator, Desk, Implementor, and Independent Reviewer responsibilities, exact-SHA gates, `memcap`, worktree cleanup, and multi-machine placement.

---

## 1. Architecture

```
                     Tailscale / WireGuard / SSH Tunnel (port 4222)
          ┌──────────────────────────────────────────────────────────┐
          │                nats-server -js (JetStream)               │
          │  • Stream `HGL_NOTES` (`hgl.notes.<agent>`, 14d, dedup)  │
          │  • Durable explicit-ack consumers (`inbox-w1`, `inbox-…`)│
          │  • KV `HGL_STATE`: `prs`, `main`, `landed.log`, `hold`   │
          │  • KV `HGL_HOSTS`: 120s TTL live host + memcap registry  │
          └────────────┬───────────────────────────────┬─────────────┘
                       │                               │
        ┌──────────────▼──────────────┐ ┌──────────────▼──────────────┐
        │    Host A (Coordinator)     │ │    Host B / C (Workers)     │
        │ • Main session (`claude`,   │ │ • `hgl-bridge` blocks on    │
        │   GPT, or `agy`) + `hgl-mcp`│ │   JetStream (0 idle tokens) │
        │ • PR table sync -> `hgl-ctl │ │ • `in_progress` heartbeats  │
        │   state put prs -`          │ │   + post-turn `ack`/`nak`   │
        │ • Optional local `/swarm`   │ │ • `agy`, `codex`, `claude`  │
        │ • Isolated git worktrees    │ │ • Isolated git worktrees    │
        └──────────────┬──────────────┘ └──────────────┬──────────────┘
```

### JetStream Primitives

| Role | NATS JetStream Primitive | Behavior |
| --- | --- | --- |
| Agent notes | Stream `HGL_NOTES` (`hgl.notes.<agent>`) | Ordered delivery, 24h `Nats-Msg-Id` deduplication, 14-day retention |
| Agent inboxes | Durable consumer `inbox-<agent>` (`AckPolicy.EXPLICIT`) | Instant TCP push wakeup; `ack()` after clean turn, `nak(delay=15)` on crash/OOM |
| Long builds | `msg.in_progress()` every 60s | Extends `ack_wait` during compiler/test runs; auto-redelivers if host dies |
| Shared state | KV bucket `HGL_STATE` (5-rev history) | Atomic revisioned keys (`prs`, `main`, `landed.log`, `hold`) |
| Host registry | KV bucket `HGL_HOSTS` (120s TTL) | Live host heartbeats and advertised `memcap` GiB budgets |

---

## 2. Layout

- `src/hgl_ai/bus.py` — Async `HglBus` wrapping `nats-py` JetStream streams, explicit-ack consumers, and KV buckets.
- `src/hgl_ai/mcp_server.py` (`hgl-mcp`) — 5-tool `stdio` MCP server (`post_note`, `poll_inbox`, `get_state`, `put_state`, `list_hosts`) over `$NATS_URL`.
- `src/hgl_ai/worker_bridge.py` (`hgl-bridge`) — Async worker supervisor driving warm `agy` (`stream-json`), `codex exec resume`, and `claude -p --resume`.
- `src/hgl_ai/cli.py` (`hgl-ctl`) — Operator CLI (`init`, `import-queue`, `post`, `inbox`, `state get|put|del`, `hosts`).
- `bin/run-nats-hub` — Starts `nats-server -js` (native binary or `nats:2-alpine` container).
- `bin/agy-tmux` — `tmux` session attach/resume helper for `agy`.
- `configs/mcp-configs.md` — `stdio` MCP client configurations for Antigravity, Codex, and Claude Code.

---

## 3. Quick Start

### Install

```sh
uv venv .venv && uv pip install --python .venv/bin/python -e .
```

### Start the JetStream hub

```sh
NATS_TOKEN="your-secret-token" ./bin/run-nats-hub

# Provision stream & KV buckets (or import an existing file queue):
NATS_TOKEN="your-secret-token" .venv/bin/hgl-ctl init
```

### Run worker bridges on any host

```sh
# Over SSH without VPN:
# ssh -o ExitOnForwardFailure=yes -N -L 4222:127.0.0.1:4222 user@hub-host &

NATS_URL="nats://127.0.0.1:4222" NATS_TOKEN="your-secret-token" \
  .venv/bin/hgl-bridge --memcap-gb 64 \
  --worker w1:codex:$HOME/worktrees/hgl-w1 \
  --worker w2:agy:gemini-4-carbon-high:$HOME/worktrees/hgl-w2 \
  --worker verifier:agy:claude-opus-5-5-high:$HOME/worktrees/hgl-verifier
```
