# Communication Protocol Specification

This document specifies the wire protocol, NATS JetStream topology, shared state keys, and gate receipts used by `hgl-ai` participants across hosts.

---

## 1. System of Record vs. Coordination Bus

1. **GitHub is the durable record.** Issues, pull requests, review threads, status checks, formal approvals, and merges live on GitHub.
2. **NATS JetStream is the low-latency coordination plane.** Agents exchange notes and read shared PR/branch snapshots over JetStream so no participant polls GitHub repeatedly for peer updates.
3. **Single-write GitHub mirroring.** Transitions that belong on the public record (`ready`, `blocked`, `sendback`, `approved`, `landed`) still post their single comment or review action on GitHub once, emitted by the sender of the note. Routine `question`, `answer`, and operational `note` messages stay on the bus unless they establish an agreed decision.

---

## 2. Transport & JetStream Topology

All participants connect to a single `nats-server -js` endpoint (TCP port `4222` by convention) over an authenticated link (WireGuard/Tailscale or OpenSSH port-forward, plus token or NKey credentials).

### 2.1 Stream: `HGL_NOTES`

| Parameter | Value | Purpose |
| --- | --- | --- |
| Subjects | `hgl.notes.>` | Unicast (`hgl.notes.<agent>`) and broadcast (`hgl.notes.all`) |
| Storage | `File` | Survives broker restarts |
| Retention | `Limits` (`max_age = 14d`) | Bounded history; GitHub retains permanent records |
| Deduplication | `duplicate_window = 24h` | Idempotent retries and imports via `Nats-Msg-Id` header |

Valid `<agent>` tokens match `^[a-z0-9][a-z0-9_-]{0,31}$` (wildcards `*`, `>`, and `.` are rejected).

### 2.2 Durable Inbox Consumers: `inbox-<agent>`

Each logical participant (`claude`, `coord`, `desk`, `w1`..`wN`, and named reviewers) reads from a durable explicit-ack pull consumer:

- **`filter_subjects`**: `["hgl.notes.<agent>", "hgl.notes.all"]`
- **`ack_policy`**: `Explicit`
- **`ack_wait`**: `300s`
- **`max_deliver`**: `5`

**Delivery contract for worker bridges:**
1. A worker bridge blocks on `fetch()` without consuming LLM tokens while idle.
2. While a turn executes (which may span 5–20 minutes during builds or regression sets), the bridge sends `msg.in_progress()` every `60s` to reset `ack_wait`.
3. `msg.ack()` is sent **only** after the agent turn exits cleanly.
4. On crash, OOM kill (`memcap` status `137`/`143`), or provider quota error, the bridge sends `msg.nak(delay=15s)` so the note is redelivered rather than dropped.

---

## 3. Note Envelope & Kinds

### 3.1 JSON Payload (`hgl.notes.<recipient>`)

```json
{
  "created_at": "20261127T041100Z",
  "sender": "w4",
  "recipient": "claude",
  "issue": 980,
  "kind": "ready",
  "body": "[codex] Ready: hgl-compiler#980 at 8a5c3d264c2edbbdaa61530d810bff276c3eebba\n\n..."
}
```

Upon publication, JetStream assigns a monotonic stream sequence number (`seq`), which serves as the immutable **bus receipt ID** referenced in controller logs and audits.

### 3.2 Body Convention

- UTF-8 Markdown, newline-terminated.
- First line identifies the runtime/role and action summary, e.g. `[codex] Ready: ...`, `[codex] (coordinator) ...`, `[codex] Desk sets: ...`, or `[claude] ...`.
- Any note referencing a pull request revision **must** state the full 40-character commit SHA (or unambiguous 8+ char prefix paired with the full SHA in Ready/gate notes).

### 3.3 Allowed `kind` Values

| `kind` | Typical Sender → Recipient | Meaning |
| --- | --- | --- |
| `ready` | `wN` → `claude` (observed by `desk`) | PR is open/updated at an exact `HEAD` SHA with author checks passing; ready for gates. |
| `blocked` | `wN` / `desk` / `reviewer` → `claude` | Cannot proceed due to an unresolved spec question, external dependency, or environment refusal. |
| `question` | Any → `claude` / `coord` / `wN` | Clarification request requiring no GitHub comment churn. |
| `answer` | `claude` / `coord` → requester | Direct reply to a `question`. |
| `sendback` | `claude` → `wN` (cc `coord`) | Returns a PR at a specific `HEAD` SHA with reproducible findings or failing gate evidence. |
| `approved` | `claude` → `wN` / `coord` | Formal controller sign-off for an exact `HEAD` SHA on a verified `main` base. |
| `landed` | `claude` → `wN` / `all` | Announces PR merge and new `main` SHA; **mandatory trigger** for worktree and scratch cleanup. |
| `done` | `wN` / `reviewer` / `coord` → `claude` | Non-PR bounded assignment (audit, probe, cleanup, release check) is complete. |
| `note` | Any → Any / `all` | Operational fact: desk set receipt, urgent CI/bot failure alert, `hold` notice, or `drain` instruction. |

---

## 4. Shared Key-Value Buckets

### 4.1 `HGL_STATE` (`Storage = File`, `history = 5`)

Atomic, revisioned cluster-wide state keys (`^[a-zA-Z0-9_./=-]{1,64}$`). Writing an empty value purges the key.

| Key | Writer | Schema & Semantics |
| --- | --- | --- |
| `main` | Controller (`claude`) | Plaintext lines recording `updated <UTC>`, full `main` SHAs of `hgl-compiler`, `hgl_spec`, `hgl_core`, and current `hgl-project` submodule pins. Rewritten after every merge. |
| `prs` | Coordinator (`coord`) | Header `# read <UTCTIME>` and `# cost <N> remaining <N> resetAt <ISO>`, followed by tab-separated rows:<br>`REPO  NUMBER  HEAD  draft\|open  BOT  UNRESOLVED  CHECKS  UPDATED`<br>• `BOT`: `clean` \| `findings` \| `running` \| `none` \| `limit`<br>• `CHECKS`: `pass` \| `fail` \| `pending` \| `none`<br>Refreshed at most every 5 minutes. Controller acts from this snapshot unless >15 minutes old. |
| `landed.log` | Controller (`claude`) | One line per merged PR: `<UTCTIME>  <REPO>  <PR>  <MERGE_SHA>`. |
| `hold` | Coordinator / Controller | Present only while GitHub API rate limit is exhausted; value states the reset UTC timestamp. While present, **all deferrable GitHub API calls must pause**. Purged once reset passes. |
| `mode` | Controller (`claude`) | Optional fleet directive: `normal` (default) or `drain` (finish active work only; claim no new issues; idle on completion). |

### 4.2 `HGL_HOSTS` (`Storage = Memory`, `ttl = 120s`)

Each `hgl-bridge` daemon writes its host record every `30s` under `<sanitized-hostname>`:

```json
{
  "hostname": "worker-linux-02",
  "agents": ["w3", "w4", "reviewer_types"],
  "memcap_gb": 64,
  "updated_at": "20261127T083000Z"
}
```

If a host disconnects or loses power, JetStream automatically expires its key after `120s`.

---

## 5. Gate Receipt Formats

### 5.1 Desk Regression Receipt

Produced by `desk` for each `ready` compiler PR at exact `HEAD` merged onto current `main`:

- **Note summary line:**
  `[codex] Desk sets: #<PR> at <HEAD8> on main <MAIN8>, exit <CODE>: <artifact-ref>`
- **Exit codes:**
  - `0` — sets completed and diff report written
  - `3` — `HEAD` does not merge cleanly onto `main`
  - `4` — build failure
  - `5` — another desk run holds the lock (wait/retry)
  - `6` — insufficient free disk (<170 GB; stop immediately)

### 5.2 Independent Review Receipt (JSON)

Produced by an independent reviewer agent for exact `(repo, pr, head)`:

```json
{
  "repo": "hgl-compiler",
  "pr": 919,
  "head": "451063637974dbc2c0ce9847fac3c5d97f8a04be",
  "reviewer": "type_positions",
  "verdict": "clean",
  "evidence": "Merged cleanly onto main <sha>. Tested N bounded positive/negative programs and M native assertions...",
  "limitations": "Linux C++ target only; stress suite covered by desk gate."
}
```

`verdict` is strictly `"clean"` or `"findings"`.

---

## 6. MCP Tool Surface (`hgl-mcp`)

Interactive agents (`agy`, `codex`, `claude`) access the protocol through 5 `stdio` MCP tools backed by `$NATS_URL`:

1. `post_note(sender, recipient, issue, kind, body)` → publishes to `HGL_NOTES`, returns `{ seq, duplicate, ... }`.
2. `poll_inbox(recipient, limit=20)` → fetches and acknowledges up to `limit` notes from `inbox-<recipient>`.
3. `get_state(key)` → returns `{ key, value, revision }` or `null` from `HGL_STATE`.
4. `put_state(key, value)` → writes key in `HGL_STATE` (or purges when `value == ""`).
5. `list_hosts()` → returns active host records from `HGL_HOSTS`.
