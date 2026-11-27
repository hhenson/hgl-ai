# Protocol

Status: proposed; implemented (this is what `hgl-ai` does). Open decisions are issues in
this repository.

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
  serves a seat holds the `flock` on `<data_dir>/seats/<seat>.lock`: the bridge,
  `note wait` and `note peek` alike.
  A bridge serves a seat only while it holds the lease `HGL_LOCKS/seat.<seat>`,
  refreshed every 30 s (or a sixth of the bucket's `max_age`, if shorter). One the
  hub has not confirmed for half of `max_age` (no answer: a partition) stops the seat
  as a lost one does (its turn stopped, its notes naked), so it has stopped before
  the lease can expire and another host take it; `note wait` and `note peek` give it
  up likewise. `note wait` and `note peek` refuse a held one
  and hold it themselves until they return; `--force` skips the lock and the
  lease, with a warning. A lease holder is `<host> pid <pid> (<what>) id <host-id>`,
  with a host id kept in `<data_dir>/host-id`. A bridge, `note wait` or `note
  peek` takes over at once a lease that a dead bridge, `note wait` or `note peek`
  of its own host held (same label and host id, the recorded process gone).
  Host labels are unique in a fleet; a machine that shares a label by mistake
  has another host id, so its leases are never taken.
  `note peek` reads the stream after the inbox's acknowledged floor and spends
  no delivery.

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
| Inbox | durable consumer `inbox-<seat>` | filter `hgl.notes.<seat>` and `hgl.notes.all`; explicit ack; `ack_wait` 300 s; `max_deliver` unlimited (PROTO-8) |
| State | KV `HGL_STATE` | file storage, history 5 |
| Hosts | KV `HGL_HOSTS` | memory storage, TTL 120 s |
| Trees | KV `HGL_TREES` | file storage, history 1 |
| Locks | KV `HGL_LOCKS` | file storage, history 1, `max_age` 30 min |
| Ended | KV `HGL_ENDED` | file storage, history 1, `max_age` 15 d |
| Builds | KV `HGL_BUILD` | file storage, history 64: `recipe.<repo>` (TOML), `run.<id>` (summary JSON; newest 1000 kept) |

- **PROTO-7 — Seat names** match `^[a-z0-9][a-z0-9_-]{0,31}$`. `all` is the
  broadcast address. A seat is a logical address; the runtime and model bound
  to it can change (§6) without peers noticing.
- **PROTO-8 — Ack after the turn.** The bridge sends `in_progress` every 60 s
  while a turn runs or a note is held, and `ack` after a clean exit. JetStream
  never drops a note (`max_deliver` is unlimited); the bridge decides:
  - *Held* (no delivery spent; kept alive and tried again on a timer): a quota
    refusal with no binding left to move to (every 10 min), a tree that cannot
    be prepared yet (from 60 s, doubling to 16 min; the controller is told once),
    `HGL_ENDED` that cannot be read or written (every 15 s), the gates
    (ROLE-18, ROLE-19), a turn lock another process holds.
  - *Real failure* (a crash, a memory or time limit, a turn that could not be
    started this time): counted per note in `<data_dir>/bridge/failures/<seat>.json`
    and naked with a 15 s delay. At the fifth the bridge sends `term` and a
    *dead letter* to the controller: a `note` whose body starts
    `[bridge] Dead letter:`, gives the reason and quotes the note.
  - A turn that can never start (the CLI is missing, or the command line is over
    the system's limit) is not retried: the bridge sends `term` and tells the
    controller which notes it dropped.
  - At shutdown or a lost lease, held and running notes are naked (handed back).
- **PROTO-9 — Idempotent sends.** Each note carries a `Nats-Msg-Id` of
  `<sender>-<created_at>-<sha256(envelope)[0:12]>` over recipient, kind, repo,
  issue, head, base and body, so a retried send is dropped and no other note is.

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
| `build` | any → `builder-<label>` | Build `repo` at `head` (optionally merged onto `base`); the body is the job JSON (`docs/build.md`). The builder answers with a `receipt`. |

## 5. Worktrees

- **PROTO-12 — The bridge owns worktrees.** On `assign` or `review` the
  bridge creates `<work_root>/<seat>/<repo>-<issue>/` with the checkout in
  `src/`, a scratch `tmp/` and a `build/` directory, and starts the agent's
  turn there with `TMPDIR`, `CARGO_TARGET_DIR` and `HGL_AI_TASK_DIR` pointing
  inside it. Nothing of a task goes to `/tmp`, the caps' report files included.
- **PROTO-13 — Every tree is registered** in `HGL_TREES` under
  `<host>.<seat>.<repo>-<issue>` with its path, branch, creation time and
  last measured size.
- **PROTO-14 — Removal is the bridge's.** On `landed` or `closed` for an
  issue, the bridge stops any turn in that task, saves uncommitted changes as
  a patch under `<work_root>/salvage/` if there are any, removes the worktree
  and its branch, deletes the task directory, deregisters it, and sends a
  `done` cleanup receipt to the controller. Agents do not remove trees. Before
  acknowledging the note it records the end in `HGL_ENDED` (§8); a note of that
  task to that seat with a lower stream sequence is then moot, on any host, and
  is acknowledged without a turn. A note of a task is held while `HGL_ENDED`
  cannot be read, an ending note while its end cannot be recorded (PROTO-8). An ending
  note whose end `HGL_ENDED` already holds (a redelivery) is acknowledged
  without action. An ending note acts only on a tree made for a note with a
  lower sequence than its own (`task.json` records it) and on that tree's turn;
  a tree made for a later note is a restart of the task and is left alone. Of
  the notes of a turn it stops, the later ones are naked, not acknowledged. A
  removal that fails (a salvage patch that cannot be written) does not undo the
  end: it is kept in `<data_dir>/bridge/cleanup/<seat>.json`, said in the cleanup
  receipt, tried again every 5 min until it works, and reported then; a later note
  of the task that runs a turn there takes the tree over instead.
- **PROTO-15 — Leftovers are visible.** `hgl-ai tree list --stale` lists
  trees whose issue is closed or that are older than `stale_after` (default
  3 days). The bridge posts that list and the disk use of `work_root` to the
  controller daily.

## 6. Seats and failover

- **PROTO-16 — Binding.** A seat binds a role to a runtime (`claude`, `codex`
  or `agy`) and a model on one host, in that host's configuration. Its record
  in `HGL_HOSTS` advertises role, runtime, model family and memory budget.
- **PROTO-17 — Failover.** A seat may list fallback bindings. A quota refusal
  is known from the CLI's structured output (an error event or result and its
  status), never from text on stderr. On one, the bridge holds the notes and
  moves the seat to its next binding whose family keeps ROLE-11 for the refused
  turn's task: a review keeps its family; an implementor's task keeps a live
  reviewer family that is none of its authors' (the families its turns ran on).
  It tells the controller, and returns the seat to its primary after
  `limits.failover_cooldown` (default 1 h). With no such binding, the notes stay
  held (PROTO-8). The merge gate checks ROLE-11 again (PROTO-19).

## 7. Receipts

- **PROTO-18 — Desk receipt.** Facts only: exit code and report location. (The desk
  is not part of this repository; the format is fixed here for its receipts.)

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
  The merge gate reads receipts from the controller's notes in `HGL_NOTES`
  (14 days): `receipt` notes whose body holds this JSON (alone, or inside text or
  a fenced block) for the pull request, whose `reviewer` is the seat that sent
  the note. The authors' families are those the bridges record in the task's
  implementor trees (`families` in `task.json` and `HGL_TREES`).

- **PROTO-20 — Build receipt.** A builder answers a `build` job with a `receipt` from
  `builder-<label>` at the job's `head`, to the job's `reply_to` (`r-<id>`, an address no
  inbox serves; the sender reads the last note on `hgl.notes.r-<id>`) and to its `notify`
  seat. The body is the build summary as JSON (`docs/build.md` BUILD-4):

```json
{"v": 1, "id": "20261127-083000-3fa9c1", "repo": "hgl-compiler", "head": "8a5c...", "host": "lab1",
 "platform": "linux", "executor": "local", "status": "fail", "steps": [{"name": "test", "status": "fail",
 "exit": 101, "wall_ms": 81000, "cpu_ms": 302000, "peak_bytes": 3650000000, "cap_bytes": 34359738368,
 "log": ".../test.log"}], "errors": [{"step": "test", "file": "src/lib.rs", "line": 10, "col": 5,
 "code": "E0308", "message": "mismatched types", "count": 1, "start": 1234, "end": 2345}],
 "errors_total": 1, "tests": {"passed": 120, "failed": 2, "skipped": 3, "failures": [...]},
 "warnings_total": 4, "warnings": {"unused_variables": 3, "dead_code": 1}, "log_dir": "..."}
```

  `status` is `pass`, `fail`, `not-run` (a missing prerequisite), or `stopped`.

## 8. Shared state

`HGL_STATE` keys match `^[a-zA-Z0-9_./=-]{1,64}$`. An empty value deletes.

| Key | Writer | Content |
| --- | --- | --- |
| `main` | controller | `updated <UTC>`, then `<repo> main <sha>` lines and the wrapper's pins. Rewritten after each merge. |
| `prs` | coordinator service | `# read <UTC>`, `# cost <n> remaining <n> resetAt <ISO>`, then rows `REPO NUMBER HEAD draft\|open BOT UNRESOLVED CHECKS UPDATED`. At most every 5 min; stale after 15 min. |
| `landed.log` | controller | `<UTC> <repo> #<pr> <merge sha>` per merge. |
| `hold` | coordinator, controller | Present while the GitHub API allowance is spent; value is the reset time. Defer every call that can wait. |
| `mode` | controller | `normal` (default) or `drain`: finish in-flight work, take nothing new. |
| `landing.<repo>.<pr>` | `gh land`, `gh merge-if-clean` | JSON `{"head","merge","done","notes_at","updated_at"}`: what a landing has done. Written under the landing lock from right before the merge call; deleted when every step is done. |
| `coord.alerts` | coordinator service | JSON array of the alert notes not sent yet; each is sent again every round until its send succeeds. |
| `verdict.<repo>.<pr>.<name>` | local build hosts | `<head> <main> pass\|fail ...`: a local build verdict the gate reads where `[github.gate.<repo>] verdicts` names it. |

`BOT` is `clean`, `findings`, `running`, `none` or `limit`. `CHECKS` is
`pass`, `fail`, `pending` or `none`.

`HGL_LOCKS` holds leases, `{"holder", "acquired_at"}`, created only if absent,
rewritten every minute while held, deleted by their holder; one not rewritten
within `max_age` expires. `lock.land.<repo>` is the landing lock: `gh land`,
`gh merge-if-clean` and `gh release watch` hold it from before the gate until
the merge, `landed.log`, `main` and the `landed` notes are done.

Each step after the merge (`landed.log`, `main`, the `landed` notes, the comment;
then, for `gh land`, the pins) is recorded in `landing.<repo>.<pr>` as it is done.
The same command run again finds the pull request merged and does only the steps
not done: no second `landed.log` line, the notes with their first `created_at`, no
second comment. It reports success only when all are done; pins that did not land
give `PARTIAL` (exit 3). A pull request found merged with neither a record nor a
`landed.log` line has every step done then. `main` is written only whole: a failed
read is tried three times, then the landing fails; a repository with no `main`
(HTTP 404) is left out with a warning.

`HGL_ENDED/<seat>.<repo>-<issue>` holds `{"seq", "ended_at"}`: the stream
sequence of the note that ended the task for the seat. A later end replaces an
earlier one; a record outlives the notes stream by a day.

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

A builder writes `HGL_HOSTS/builder.<label>` with the same heartbeat and TTL
(`hgl-ai hosts` leaves these out; `hgl-ai builder list` shows them):

```json
{"label": "win1", "inbox": "builder-win1", "platform": "windows", "executor": "ssh",
 "proxy": "lab1", "reachable": true, "state": "active", "slots": 1, "busy": 0,
 "free_gb": 410, "toolchains": {"git": "git version 2.53.0"}, "updated_at": "20261127T083000Z"}
```

`proxy` names the host that drives a remote over ssh; `state` is `active`, `draining` or
`retired`.
