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
  states an owner decision unless it quotes the GitHub record that holds it. The one
  exception is a note from `owner`, which only the console can send (OWN-5); quote it
  by its handle and date (OWN-10).

## 2. Access

- **PROTO-4 — One command-line tool.** Every participant uses the `hgl-ai`
  binary. There is no MCP server: agents run `hgl-ai` through their shell
  tool and learn it from one installed skill (`skill/SKILL.md`). Output is
  terse text by default, `--json` on request.
- **PROTO-5 — Delivery by the bridge.** An idle agent costs nothing. The
  bridge holds the agent's inbox, starts one headless turn per batch of notes,
  and puts the notes in the turn's prompt. Agents never poll.
- **PROTO-6 — Interactive seats.** A seat run by a person-facing session (the
  controller, by default) waits with `hgl-ai note wait SEAT`, which blocks until a
  note arrives, prints it and acknowledges it. A controller seat with
  `headless = true` is served by the bridge instead, a turn per batch of notes
  (OWN-7); `bridge run` serves every other seat without being told. A seat is served by a bridge or
  interactively, never both: `note wait` and `note peek` refuse a seat that a
  live `HGL_HOSTS` record lists, unless given `--force`. On one host, whoever
  serves a seat holds the `flock` on `<data_dir>/seats/<seat>.lock`: the bridge,
  `note wait` and `note peek` alike.
  A bridge serves a seat only while it holds the lease `HGL_LOCKS/seat.<seat>`,
  refreshed every 30 s (or a sixth of the bucket's `max_age`, if shorter), every seat's
  at once, each attempt bounded by that period. One the
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
hgl-ai note send --to controller --issue 980 --kind ready --body-file ready.md
hgl-ai note wait controller          # blocks; prints the next notes
hgl-ai state get prs
hgl-ai tree list --stale
```

## 3. Transport

All participants connect to one `nats-server -js` hub over an authenticated
link (tailnet, WireGuard or an SSH forward). Each client is its own hub user, an
NKey with its role's permissions (docs/install.md, Hub users); a hub installed
before hub users takes one shared token until it moves.

| Primitive | Name | Settings |
| --- | --- | --- |
| Stream | `HGL_NOTES`, subjects `hgl.notes.>` | file storage, `max_age` 14 d, dedup window 24 h |
| Inbox | durable consumer `inbox-<seat>` | filters `hgl.notes.<seat>`, `hgl.notes.all`, `hgl.notes.owner.<seat>` and `hgl.notes.screen.<seat>`; explicit ack; `ack_wait` 300 s; `max_deliver` unlimited (PROTO-8) |
| Owner | stream `HGL_OWNER`, KV `HGL_THREADS`, KV `HGL_METRICS`, KV `HGL_CONSOLE` | docs/owner.md §4 |
| State | KV `HGL_STATE` | file storage, history 5 |
| Hosts | KV `HGL_HOSTS` | memory storage, TTL 120 s |
| Trees | KV `HGL_TREES` | file storage, history 1 |
| Screen | KV `HGL_SCREEN` | file storage, history 5, `rule.>` and `log.>` keys |
| Locks | KV `HGL_LOCKS` | file storage, history 1, `max_age` 30 min |
| Ended | KV `HGL_ENDED` | file storage, history 1, `max_age` 15 d |
| Usage | KV `HGL_USAGE`; stream `HGL_USAGE_LOG`, subjects `hgl.usage.>` | KV file storage, history 1; stream file storage, `max_age` 35 d, dedup window 24 h (§10) |
| Builds | KV `HGL_BUILD` | file storage, history 64: `recipe.<repo>` (TOML), `run.<id>` (summary JSON; newest 1000 kept) |

Screen records use `rule.<id>` for the rule and `rule._ids` for discovery.
Retirement sets `retired_at` on the rule and preserves its older revisions.
Each add or retirement records a line in `HGL_STATE/decisions.log`; a pending
write travels with the rule revision so the command can resume after interruption.
The first result for an original note sequence is kept at `log.<seq>`.
`screen log --since SEQ` returns results with a strictly greater note sequence,
in numeric order.

- **PROTO-7 — Seat names** match `^[a-z0-9][a-z0-9_-]{0,31}$`. `all` is the
  broadcast address. `owner` and `console` are sender names. `screen` is the
  screen's sender and intake address. Those names are reserved in seat
  configuration, and host labels also reserve `all` and `screen`.
  A seat is a logical address; the runtime and model bound
  to it can change (§6) without peers noticing.
  A seat receives broadcasts published from its durable inbox's creation onward,
  measured by the hub. Unicast published earlier remains eligible while retained.
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
- **PROTO-9 — Idempotent sends.** Notes normally carry a `Nats-Msg-Id` of
  `<sender>-<created_at>-<sha256(envelope)[0:12]>` over recipient, kind, repo,
  issue, head, base and body (and `by` and `class` when set), so a retried send is
  dropped and no other note is. Coordinator PR alerts use the comparison
  table revision, repository, PR, head and finding as their message ID.

## 4. Notes

```json
{
  "v": 1,
  "created_at": "20261127T041100Z",
  "sender": "w4",
  "recipient": "controller",
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
| `assign` | controller or triage → seat | Take this issue. Carries `repo`, `issue`, optional `base` and `class` (`simple`, `standard` or `complex`; PROTO-21). The bridge creates the worktree first (§5). |
| `ready` | implementor → controller or coordinator | Pull request at `head` passes the author's checks. |
| `blocked` | any → controller | Cannot proceed; states the question or dependency. |
| `question` / `answer` | any ↔ any | Clarification; no GitHub comment unless it settles something. |
| `review` | controller or coordinator → reviewer | Review `repo#issue` at `head`. The bridge creates a review worktree. |
| `receipt` | reviewer or desk → controller, reviewer → coordinator | Gate result at `head` (§7). |
| `sendback` | controller or coordinator → implementor | Findings at `head`; fix with new commits. The controller may set `class` to move the task up (ROLE-15). |
| `approved` | controller → implementor | Sign-off for `head` on a verified `main`. |
| `landed` | controller → seat, `all` | Merged; carries the merge commit. The bridge removes the issue's worktrees (§5). |
| `closed` | controller → seat | Closed unmerged or superseded; same removal as `landed`. |
| `done` | any → controller | A bounded non-PR assignment finished, or a cleanup receipt. |
| `note` | any → any | Operational fact: an alert, a `hold`, a `drain`. |
| `build` | any → `builder-<label>` | Build `repo` at `head` (optionally merged onto `base`); the body is the job JSON (`docs/build.md`). The builder answers with a `receipt`. |
| `tick` | console → controller | A status report is due (`[console] status_every`). |

For repositories with an automatic `[github.review_pools.<repo>]` on the sender and
the coordinator, `note send --to controller` routes `ready` and structured review
receipts through the coordinator. The coordinator validates against the current PR
round and forwards exceptions with the original sender, kind, head and body. It sends
the controller a digest of routine routes and clean landing candidates after each round.
The controller handles findings following any earlier sendback so it can apply ROLE-15.
The controller also handles `hgl_spec`, `hgl_core`, and repositories in `github.spec_repos`.

- **PROTO-20 — Owner and screen notes.** A note with sender `owner` travels on
  `hgl.notes.owner.<recipient>`, which only the console's hub user may publish. The
  receiver accepts sender `owner` on that subject only, and only sender `owner` there;
  anything else is terminated as malformed. An owner note is an `answer` (to the
  asker of a question) or a `note` (an owner message; conversations go to the
  controller), and its body names the thread and the signed-in user. `by` carries the
  writer's handle (OWN-10); only an owner note has one.

  Screen replies and digests travel on `hgl.notes.screen.<recipient>`, which
  the `screen` hub role may publish. The receiver requires sender `screen`
  on exactly that subject, kind `answer` or `note`, and one recipient. The
  bridge labels these messages “From the screen, not a ruling.” Hub provisioning
  creates `inbox-screen` so the screen can pull and acknowledge its intake.

## 5. Worktrees

- **PROTO-12 — The bridge owns worktrees.** On `assign` or `review` the
  bridge creates `<work_root>/<seat>/<repo>-<issue>/` with the checkout in
  `src/`, a scratch `tmp/` and a `build/` directory, and starts the agent's
  turn there with `TMPDIR`, `CARGO_TARGET_DIR` and `HGL_AI_TASK_DIR` pointing
  inside it. Nothing of a task goes to `/tmp`, the caps' report files included.
- **PROTO-13 — Every tree is registered** in `HGL_TREES` under
  `<host>.<seat>.<repo>-<issue>` with its path, branch, creation time and
  last measured size. The daily report re-registers each tree by a revision
  compare-and-set merged with the record held, so a fresher record never goes
  back: `families` only grow, `handoff_seq` and `class_seq` never fall, and of
  two trees made at that key the later made (by `seq`, then creation time) stays.
  It writes a tree only while that tree's `task.json` still names it, checked
  under a per-key lock that a removal holds across its deregistering, so a tree
  removed after the listing is not registered again.
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
- **PROTO-21 — Task class.** An `assign` carries the task's class (docs/roles.md §3);
  only `assign` and `sendback` carry one. The class picks the seat, not the model: a seat
  runs its own binding (and its fallbacks, PROTO-17) whatever the class. The bridge records
  the class, with the stream sequence of the note that set it, in the task's `task.json`
  and `HGL_TREES` record, and the turn's prompt names it. An `assign` sets it; a `sendback`
  with a class only moves it up, never down (ROLE-15); a note no newer than the one that set
  it (an older note delivered again) changes nothing. A tree made anew for a task begun
  elsewhere (another host or seat) takes the class of the task's record set last, unless
  the note that made it carries a class: that note moved the task (`gh sendback --class C
  --to SEAT`, ROLE-15) and its class is the task's there. A kept tree that a newer handoff
  reaches takes the class set last, if newer than its own (hgl-ai#55). When another tree
  took a handoff in between, the first newer one moved the task back, and its class, if
  any, is the task's. Two trees of a host never share a local branch: a tree made while
  another has the task's branch checked out takes `<branch>@<seat>`, which pushes to
  `<branch>` and goes with the tree. A kept tree fast-forwards to the pushed branch,
  keeping its uncommitted changes; when its branch has diverged, or such a change is in a
  file the pushed commits change, no turn runs: the notes are held and retried, and the
  bridge sends the controller one `blocked` note naming the tree and the files. Each `HGL_TREES` record carries the
  stream sequence of the note that made its tree (`seq`) and of the newest `assign` or
  `sendback` a turn in it ran (`handoff_seq`; a reused tree takes the later one, hgl-ai#44):
  the seat of the task's implementor tree with the newest handoff (else `seq`) is its current
  seat, and `gh approve` and `gh sendback` without `--to` go only there (`landed` and
  `closed` still reach every seat that has a tree, PROTO-14).
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
  The merge gate reads receipts from the controller's and coordinator's notes in `HGL_NOTES`
  (14 days): `receipt` notes whose body holds this JSON (alone, or inside text or
  a fenced block) for the pull request, whose `reviewer` is the seat that sent
  the note. The authors' families are those the bridges record in the task's
  implementor trees (`families` in `task.json` and `HGL_TREES`).

- **PROTO-22 — Build receipt.** A builder answers a `build` job with a `receipt` from
  `builder-<label>` at the job's `head`, to the job's `reply_to` (`r-<id>`, an address no
  inbox serves; the sender reads the last note on `hgl.notes.r-<id>`) and to its `notify`
  seat. The body is the build summary as JSON (`docs/build.md` BUILD-4):

```json
{"v": 1, "id": "20261127-083000-3fa9c1", "repo": "hgl-compiler", "head": "8a5c...", "host": "lab1",
 "platform": "linux", "executor": "local", "status": "fail", "steps": [{"name": "test", "status": "fail",
 "exit": 101, "wall_ms": 81000, "cpu_ms": 302000, "peak_bytes": 3650000000, "cap_bytes": 34359738368}], "errors": [{"step": "test", "file": "src/lib.rs", "line": 10, "col": 5,
 "code": "E0308", "message": "mismatched types", "count": 1, "start": 1234, "end": 2345}],
 "errors_total": 1, "tests": {"passed": 120, "failed": 2, "skipped": 3, "failures": [...]},
 "warnings_total": 4, "warnings": {"unused_variables": 3, "dead_code": 1}, "log_dir": ""}
```

  `status` is `pass`, `fail`, `not-run` (a missing prerequisite), or `stopped`.

## 8. Shared state

`HGL_STATE` keys match `^[a-zA-Z0-9_./=-]{1,64}$`. An empty value deletes.

| Key | Writer | Content |
| --- | --- | --- |
| `main` | controller | `updated <UTC>`, then `<repo> main <sha>` lines and the wrapper's pins. Rewritten after each merge. |
| `prs` | coordinator service | `# read <UTC>`, `# cost <n> remaining <n> resetAt <ISO>`, then rows `REPO NUMBER HEAD draft\|open BOT UNRESOLVED CHECKS UPDATED`. At most every 5 min; stale after 15 min. |
| `coord.routing` | coordinator service | Note cursor, pending review route and controller digest. The coordinator saves progress under `lock.coord.routing`. |
| `landed.log` | controller | `<UTC> <repo> #<pr> <merge sha>` per merge. |
| `hold` | coordinator, controller | Present while the GitHub API allowance is spent; value is the reset time. Defer every call that can wait. |
| `mode` | controller | `normal` (default) or `drain`: finish in-flight work, take nothing new. |
| `backlog` | coordinator service | Open issues by label and the claimable queue (docs/owner.md §3). Every 15 min. |
| `tasks` | coordinator service | The last note about each task with a worktree: rows `REPO ISSUE KIND SENDER RECIPIENT AT`, tab-separated, sorted. Read from the notes stream every 10 s; written when it changes. |
| `decisions.log` | controller, console | The controller's decision log (OWN-8): newest 2000 lines. |
| `landing.<repo>.<pr>` | `gh land`, `gh merge-if-clean` | JSON `{"head","merge","done","notes_at","updated_at"}`: what a landing has done. Written under the landing lock from right before the merge call; deleted when every step is done. |
| `coord.alerts` | coordinator service | JSON array of pending alert notes and delivery metadata, saved before `prs` advances. Retries run during holds and GitHub read failures. The queue retains at most 100 entries within the state byte limit and reports oldest-entry drops. Five failed sends replace an alert with a dead-letter note for the controller. |
| `coord.intake` | coordinator service | JSON scan time, recent opening event IDs, and any pending triage note. The cursor and pending note are written together before delivery. |
| `verdict.<repo>.<pr>.<name>.<pair>` | builder service (`builder` hub user only) | `<head> <main> pass\|fail\|not-run\|running <build id> <builder> r<recipe rev> [until=<UTC>] [reason]`: a gate build of `head` merged onto the `main` commit, on the platform `name` names, under recipe revision `rev`; `<pair>` is 24 hex digits of SHA-256 over `<head> <main> r<rev>` (docs/build.md BUILD-15). `pass` and `fail` are final in their key; `until` bounds `running`. The gate reads only the key of the pull request's head, the current `main` and the current recipe. Purged by a builder once GitHub reports the pull request merged or closed; swept 15 days after the hub wrote it. |

`BOT` is `clean`, `findings`, `running`, `none` or `limit`. `CHECKS` is
`pass`, `fail`, `pending` or `none`.

Intake scans open issues and PRs across `[github].repos` every 15 minutes under
`hold`. The first scan looks back 15 minutes. Later scans resume from the saved
time with a 15-minute overlap to catch updates during pagination. GitHub
readiness labels, assignees, review requests, assignment notes and registered
work suppress duplicate intake. A same-repository PR with a registered
implementor branch is attributed to the fleet. Other PRs are flagged for
controller routing. Empty rounds send no note. Pending notes are retried with
the same message ID.

`HGL_LOCKS` holds leases, `{"holder", "acquired_at", "token"}`, created only if
absent, rewritten every minute while held, deleted by their holder; one not
rewritten within `max_age` expires. `token` is random per acquisition (and
take-over), so a lease's value is its own even against the same holder in the same
second; a reader treats a record without one (an older writer) as token `""`. A
rewrite or delete is a compare-and-set at the holder's revision; one that finds the
key moved and the record, token included, still its own (a rewrite whose answer it
never saw) takes that revision once, so only another record, or none, is a lost
lease.
`builder.<label>` is held by the one process serving that builder (`docs/build.md`).
`lock.land.<repo>` is the landing lock: `gh land`,
`gh merge-if-clean` and `gh release watch` hold it from before the gate until
the merge, `landed.log`, `main` and the `landed` notes are done.

Each step after the merge (`landed.log`, `main`, the `landed` notes, the comment;
then, for `gh land`, the pins) is recorded in `landing.<repo>.<pr>` as it is done.
The same command run again finds the pull request merged and does only the steps
not done: no second `landed.log` line, the notes with their first `created_at`, no
second comment. It reports success only when all are done. A step after the merge
that fails (pins that did not land, a GitHub error, a failed hub write, a lost landing
lock) gives `PARTIAL` (exit 3): the merge commit, the steps not done, the error, and
"run the same command again to finish"; the record is kept. Without `--json` the
lines of the steps done follow it. An unreachable hub is still exit 10. Before it
reads the pins, `gh pins sync` (and so `gh land`) finishes the landing of any pin pull
request (its branch the pin branch) whose record shows it merged, or that GitHub shows
merged; it is not left behind pins that read current. One another sync finished first
is done, and the sync goes on to the pins. Its dry run reads the hub for them too. A pull
request found merged with neither a record nor a `landed.log` line has every step
done then. `main` is written only whole: a failed read is tried three times, then the
landing fails; a repository with no `main` (HTTP 404) is left out with a warning.

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

## 10. Subscription usage

`hgl-ai usage collect` reads each signed-in CLI of the host's agent user, by slash
command or file, never a prompt (no model turn):

| Runtime | Limits | Account id |
| --- | --- | --- |
| `claude` | `claude -p "/usage"`: `Current week\|session (<scope>): N% used · resets <time> (<zone>)` lines; the breakdown after them is ignored | `orgId/email` of `claude auth status` |
| `codex` | the newest `rate_limits` (by event time) over the recently written `~/.codex/sessions/**/rollout-*.jsonl` whose `session_meta` is of the account; a rollout is read from its tail | `tokens.account_id` of `~/.codex/auth.json` |
| `agy` | `agy -p "/usage"`: `<bucket>\t<window> Limit Remaining\t<pct>%\t<reset>`; a reset at the epoch is a window not started | the `applyAuthResult: email=` line of its newest log |

- **PROTO-23 — One record per login.** `HGL_USAGE/<runtime>.<account>`, where
  `<account>` is the first 12 hex digits of the SHA-256 of the account id. No id, email
  or token is published; the account files are parsed into the id fields alone.
  A record: `runtime`, `account`, `label`, `host` (the reporter), `plan`,
  `observed_at` (when the numbers were true: the probe, or Codex's last turn),
  `collected_at`, `source_age_s`, `buckets` `[{scope, window, used_pct, resets_at}]`
  (`scope` `all`, a model, `gemini`, `claude+gpt`; `window` `5h`, `week` or
  `<minutes>m`; remaining is 100 − used), `limit_reached`, and `unparsed`: source lines
  that did not parse, kept as they were (at most 20). A source that fails skips its
  runtime; one that does not parse keeps its lines; neither fails the collection.
  `HGL_USAGE/label.<account>` holds the owner's label (`hgl-ai usage label`).
- **PROTO-24 — Newest observation wins.** Every host holding a login may report it. A
  record replaces its key only if its `observed_at` is newer (compare-and-set), so the
  record stays fresh while any of those hosts is up. Each record that replaces its key
  is also published to `hgl.usage.<runtime>.<account>` (message id
  `<key>.<observed_at>`), the history for burn-down; the same observation again is sent
  to the history only when the history's last record for the key is not it. `usage show` gives each record the label
  in force. Seat and coordinator users may publish both, and every role reads the
  history; a host is to publish only the accounts its registration lists
  (#36).
