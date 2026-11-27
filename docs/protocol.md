# Protocol

Status: proposed. Open decisions are issues in this repository.

## 1. Records

- **PROTO-1 — GitHub is the record.** Issues, pull requests, reviews, checks
  and merges live on GitHub. A `ready`, `blocked`, `sendback`, `approved` or
  `landed` note also gets one GitHub comment, posted once by its sender.
- **PROTO-2 — The bus is transport.** NATS JetStream carries notes and shared
  state between agents and hosts. History is bounded (14 days); nothing that
  decides something lives only on the bus.
- **PROTO-3 — No fabricated owner words.** A note is agent traffic. It never
  states an owner decision unless it quotes the GitHub record that holds it.

## 2. Access

- **PROTO-4 — One command-line tool.** Every participant uses the `hgl-ai`
  binary. There is no MCP server: agents run `hgl-ai` through their shell
  tool and learn it from one installed skill (`skill/SKILL.md`). Output is
  terse text by default, `--json` on request.
- **PROTO-5 — Delivery by the bridge.** An idle agent costs nothing. The
  bridge holds the agent's inbox, starts one headless turn per batch of notes,
  and puts the notes in the turn's prompt. Agents never poll.
- **PROTO-6 — Interactive seats.** A seat run by a person-facing session (the
  controller) waits with `hgl-ai note wait SEAT`, which blocks until a note
  arrives, prints it and acknowledges it. A seat is served by a bridge or
  interactively, never both: `note wait` and `note peek` refuse a seat that a
  live `HGL_HOSTS` record lists, unless given `--force`. On one host, whoever
  serves a seat holds the `flock` on `<data_dir>/seats/<seat>.lock`.

```sh
hgl-ai note send --to claude --issue 980 --kind ready --body-file ready.md
hgl-ai note wait claude          # blocks; prints the next notes
hgl-ai state get prs
hgl-ai tree list --stale
```

## 3. Transport

All participants connect to one `nats-server -js` hub over an authenticated
link (tailnet, WireGuard or an SSH forward) with a token or NKey credential.

| Primitive | Name | Settings |
| --- | --- | --- |
| Stream | `HGL_NOTES`, subjects `hgl.notes.>` | file storage, `max_age` 14 d, dedup window 24 h |
| Inbox | durable consumer `inbox-<seat>` | filter `hgl.notes.<seat>` and `hgl.notes.all`; explicit ack; `ack_wait` 300 s; `max_deliver` 5 |
| State | KV `HGL_STATE` | file storage, history 5 |
| Hosts | KV `HGL_HOSTS` | memory storage, TTL 120 s |
| Trees | KV `HGL_TREES` | file storage, history 1 |

- **PROTO-7 — Seat names** match `^[a-z0-9][a-z0-9_-]{0,31}$`. `all` is the
  broadcast address. A seat is a logical address; the runtime and model bound
  to it can change (§6) without peers noticing.
- **PROTO-8 — Ack after the turn.** The bridge sends `in_progress` every 60 s
  while a turn runs, `ack` after a clean exit, and `nak` with a 15 s delay on a
  crash, a memory or time limit, or a provider quota refusal.
- **PROTO-9 — Idempotent sends.** Each note carries a `Nats-Msg-Id` of
  `<sender>-<created_at>-<sha256(body)[0:12]>`, so a retried send is dropped.

## 4. Notes

```json
{
  "v": 1,
  "created_at": "20261127T041100Z",
  "sender": "w4",
  "recipient": "claude",
  "repo": "hgl-compiler",
  "issue": 980,
  "kind": "ready",
  "head": "8a5c3d264c2edbbdaa61530d810bff276c3eebba",
  "body": "[codex] Ready: hgl-compiler#980 at 8a5c3d26 ..."
}
```

- **PROTO-10 — Revisions are exact.** A note about a pull request revision
  carries its full 40-character `head`. Any verdict applies to that head only.
- **PROTO-11 — Body.** UTF-8 Markdown. First line names runtime and action,
  for example `[codex] Ready: ...` or `[claude] Approved: ...`.

| Kind | From → to | Meaning |
| --- | --- | --- |
| `assign` | controller or triage → seat | Take this issue. Carries `repo`, `issue`, optional `base`. The bridge creates the worktree first (§5). |
| `ready` | implementor → controller | Pull request at `head` passes the author's checks. |
| `blocked` | any → controller | Cannot proceed; states the question or dependency. |
| `question` / `answer` | any ↔ any | Clarification; no GitHub comment unless it settles something. |
| `review` | controller → reviewer | Review `repo#issue` at `head`. The bridge creates a review worktree. |
| `receipt` | reviewer or desk → controller | Gate result at `head` (§7). |
| `sendback` | controller → implementor | Findings at `head`; fix with new commits. |
| `approved` | controller → implementor | Sign-off for `head` on a verified `main`. |
| `landed` | controller → seat, `all` | Merged; carries the merge commit. The bridge removes the issue's worktrees (§5). |
| `closed` | controller → seat | Closed unmerged or superseded; same removal as `landed`. |
| `done` | any → controller | A bounded non-PR assignment finished, or a cleanup receipt. |
| `note` | any → any | Operational fact: an alert, a `hold`, a `drain`. |

## 5. Worktrees

- **PROTO-12 — The bridge owns worktrees.** On `assign` or `review` the
  bridge creates `<work_root>/<seat>/<repo>-<issue>/` with the checkout in
  `src/`, a scratch `tmp/` and a `build/` directory, and starts the agent's
  turn there with `TMPDIR`, `CARGO_TARGET_DIR` and `HGL_AI_TASK_DIR` pointing
  inside it. Nothing of a task goes to `/tmp`.
- **PROTO-13 — Every tree is registered** in `HGL_TREES` under
  `<host>.<seat>.<repo>-<issue>` with its path, branch, creation time and
  last measured size.
- **PROTO-14 — Removal is the bridge's.** On `landed` or `closed` for an
  issue, the bridge stops any turn in that task, saves uncommitted changes as
  a patch under `<work_root>/salvage/` if there are any, removes the worktree
  and its branch, deletes the task directory, deregisters it, and sends a
  `done` cleanup receipt to the controller. Agents do not remove trees.
- **PROTO-15 — Leftovers are visible.** `hgl-ai tree list --stale` lists
  trees whose issue is closed or that are older than `stale_after` (default
  3 days). The bridge posts that list and the disk use of `work_root` to the
  controller daily.

## 6. Seats and failover

- **PROTO-16 — Binding.** A seat binds a role to a runtime (`claude`, `codex`
  or `agy`) and a model on one host, in that host's configuration. Its record
  in `HGL_HOSTS` advertises role, runtime, model family and memory budget.
- **PROTO-17 — Failover.** A seat may list fallback bindings. On a quota
  refusal the bridge naks the note and moves the seat to its next binding.
  The independence rule (`docs/roles.md`, ROLE-9) still applies after the move.

## 7. Receipts

- **PROTO-18 — Desk receipt.** Facts only: exit code and report location.

  `[desk] Sets: hgl-compiler#<PR> at <HEAD8> on main <MAIN8>, exit <CODE>: <report>`

  | Exit | Meaning |
  | --- | --- |
  | 0 | Sets ran; difference report written |
  | 3 | `head` does not merge onto `main` |
  | 4 | Build failed |
  | 5 | Another desk run holds the lock |
  | 6 | Not enough free disk; stopped |

- **PROTO-19 — Review receipt.** JSON in the body of a `receipt` note:

```json
{
  "repo": "hgl-compiler",
  "pr": 919,
  "head": "451063637974dbc2c0ce9847fac3c5d97f8a04be",
  "main": "6f7b07a09c5fd1fca77376285de7ebf3c9d97981",
  "reviewer": "r1",
  "family": "claude",
  "model": "claude-opus-5-5",
  "verdict": "clean",
  "evidence": "Merged onto main. 34 programs: 21 accept, 13 reject ...",
  "limitations": "Linux C++ target only."
}
```

  `verdict` is `clean` or `findings`. `family` is the model's vendor family
  (`claude`, `gpt`, `gemini`); the controller checks it against the author's.

## 8. Shared state

`HGL_STATE` keys match `^[a-zA-Z0-9_./=-]{1,64}$`. An empty value deletes.

| Key | Writer | Content |
| --- | --- | --- |
| `main` | controller | `updated <UTC>`, then `<repo> main <sha>` lines and the wrapper's pins. Rewritten after each merge. |
| `prs` | coordinator service | `# read <UTC>`, `# cost <n> remaining <n> resetAt <ISO>`, then rows `REPO NUMBER HEAD draft\|open BOT UNRESOLVED CHECKS UPDATED`. At most every 5 min; stale after 15 min. |
| `landed.log` | controller | `<UTC> <repo> #<pr> <merge sha>` per merge. |
| `hold` | coordinator, controller | Present while the GitHub API allowance is spent; value is the reset time. Defer every call that can wait. |
| `mode` | controller | `normal` (default) or `drain`: finish in-flight work, take nothing new. |

`BOT` is `clean`, `findings`, `running`, `none` or `limit`. `CHECKS` is
`pass`, `fail`, `pending` or `none`.

## 9. Host record

Each bridge writes `HGL_HOSTS/<host>` every 30 s:

```json
{
  "host": "build-1",
  "seats": [{"seat": "w3", "role": "implementor", "runtime": "codex", "family": "gpt", "model": "gpt-6.1-sol"}],
  "memcap_gb": 64,
  "work_root_free_gb": 812,
  "updated_at": "20261127T083000Z"
}
```

`host` is a configured label, never a network name or address.
