# Roles and model assignment

Status: proposed; implemented, except where a rule says it is proposed (`docs/build.md`).
Open decisions are issues in this repository.

## 1. Team

```
                 Owner ─────────────── Architect
       specs, priorities, tags, budget   features, refactoring,
                   │                     design cleanup
              Controller ───────────── Triage
      verdicts, merges, pins       labels, slices, owner
                   │               question batches
       ┌───────────┼────────────┐
  Implementors  Reviewers    Services (no model)
  one task each  other family coordinator, desk, bridge
```

- **ROLE-1 — Owner.** Decides language and specification questions, release
  tags, priorities, and the fleet's budget and mode.
- **ROLE-2 — Controller** (seat `claude`, or `[github] controller`; whatever
  runtime holds it). The only role that approves, merges and updates wrapper
  pins. Judges receipts, adds a few probes of its own, sends back or approves,
  lands one change at a time against the current `main`, and updates `main`
  and `landed.log`.
- **ROLE-3 — Triage** (seat `triage`). Owns intake: labels and slices new
  issues, folds duplicates, keeps the claimable backlog ordered, and batches
  questions for the owner into one labelled list. Writes no code. Workers
  claim only what triage labelled ready.
- **ROLE-3a — Architect** (seat `architect`). Works with the owner, through the owner
  channel (`docs/owner.md`), to expand features and to engineer refactoring and design
  cleanup: makes the code more efficient and the design simpler. Writes the design
  (parent issues such as hgl-compiler P-F1..P-F5, design notes in the owning repository),
  proposes each to the owner as a question with options and a suggestion, and once a design
  is agreed splits it into bounded tasks for triage, in an order that keeps `main` working
  (quiet windows on hot files). Does not approve or merge; implements only a spike that
  proves a design, never on `main` directly (HPH, 2026-11-27).
- **ROLE-4 — Implementor** (`w1`..`wN`). One bounded issue at a time in the
  worktree its bridge made. Runs formatting, lints and tests under the memory
  cap before `ready` with the exact head. Answers a `sendback` with new
  commits. Never force-pushes reviewed history unless told.
- **ROLE-5 — Reviewer** (`r1`..`rN`). Starts from a fresh context for each
  review. Merges `head` onto `main` in its review worktree, writes adversarial
  programs (accepting and rejecting, with native runtime checks), checks the
  status labels of any rule the change relies on, and sends one receipt.
- **ROLE-6 — Services.** Deterministic programs, not agents:
  - *coordinator*: one batched GitHub read at most every 5 minutes into
    `HGL_STATE/prs`; an immediate `note` when a review bot finding or a failed
    check lands on a `ready` or `approved` pull request.
  - *desk*: runs the fixed regression sets for each `ready` compiler head
    merged onto `main`, one run at a time, and sends a factual receipt. Not part of
    this repository.
  - *bridge*: delivers notes, runs agent turns, owns worktrees (protocol §5).
  - *builder* (proposed, not implemented): runs build recipes for local and remote
    requests and the merge queue, and returns bounded summaries (`docs/build.md`).
  - *merge queue* (proposed, not implemented): builds approved pull requests in
    batches on every platform and lands them (`docs/build.md` §3).
- **ROLE-6a — Build agent** (seat `build`; proposed, not implemented). Writes and
  repairs recipes, provisions toolchains and SDKs, diagnoses failures of the build
  itself; never changes the code under build.

## 2. Gates

- **ROLE-7 — Exact head.** Every receipt and verdict binds one 40-character
  head. A new commit voids them.
- **ROLE-8 — Current main.** A change lands only after its gates ran on the
  `main` it merges onto. When `main` moves, the next candidate is revalidated. A
  branch the gate brings up to date keeps its parent's review only where `main`
  moved in files it does not touch and in no global path; otherwise the review
  bot reviews the new head.
- **ROLE-9 — Independence.** A merge needs a clean review receipt (PROTO-19) for
  its exact head whose model family differs from every family that worked on the
  task (the families the bridges record in its implementor trees, `HGL_TREES`);
  `gh land` and `gh merge-if-clean` enforce it unless `[github.gate.<repo>]
  review_receipt = false`. A pull request the tool makes (the wrapper's pins) has
  no author and needs none. An agent never reviews work it wrote, paired on or
  coordinated. When no other family is available, nothing lands: same-family
  review is never a fallback, and failover never moves a task to a family that
  would leave it none (PROTO-17).
- **ROLE-10 — Pins follow merges.** A merge in `hgl_spec`, `hgl_core` or
  `hgl-compiler` is followed by the wrapper pin update and a new `main` state.
- **ROLE-11 — No scope creep.** A defect found on the way that the change does
  not need is filed or linked, not fixed in the same pull request.

## 3. Model assignment

Families: `claude` (Claude Code, or Claude models through Antigravity),
`gpt` (Codex), `gemini` (Antigravity). Tiers name capability, not vendor.

| Tier | claude | gpt | gemini |
| --- | --- | --- | --- |
| frontier | `claude-opus-5-5` | `gpt-6.2-astra` | `gemini-4-neon` |
| balanced | `claude-sonnet-5-5` | `gpt-6.2-sol` | `gemini-4-hydrogen` |
| fast | `claude-haiku-5-5` | `gpt-6-luna` | `gemini-3.8-flash` |

`claude-fable-5-2` (Fable) develops the specification (HPH, 2026-11-27: "specs largely
developed by claude Fable and written by Fable or Opus or Astra"); it was not part of the
calibration. `gemini-4-carbon` (close to Fable) is used for control, architecture and
design review. Concrete slugs live in configuration (`[models]`), so a new release
changes one file.

Assignment agreed with the owner, 2026-11-27 (HPH: "lets use that for now"), from
`docs/calibration.md`:

| Work | First choice | Second | Not alone |
| --- | --- | --- | --- |
| Architect: features, refactoring, design cleanup | Fable | Astra 6.2 high or Carbon high | — |
| Controller | Carbon high | Astra 6.2 high | — |
| Triage | Astra 6.2 high | Carbon high | — |
| Specification design (rules, their reasoning, the questions for the owner) | Fable | Opus high | — |
| Specification text | Fable, Opus high or Astra 6.2 high | — | — |
| Implementation, runtime and semantics (hgraph, the C++ runtime) | Sol 6.2 xhigh, Astra 6.2 xhigh | Neon xhigh | Opus, Terra, Hydrogen high |
| Implementation, checker edge cases | Neon xhigh | Astra 6.2 xhigh | Terra |
| Implementation, cross-cutting refactors (analyser, IR) | Opus high | Astra 6.2 xhigh | Terra |
| Implementation, tooling (hgl-ai, scripts, CI) | Opus high | Sol 6.2 xhigh | — |
| Review (gate) | Neon xhigh or Carbon high | Astra 6.2 xhigh (fast) | Opus (lenient verdicts) |
| Review (design, code, logic) | Carbon high | Neon xhigh | — |
| Build agent, mechanical | Sol 6.2 high | Hydrogen high | — |
| Coordinator, bridge, builder, merge queue | no model | — | — |

The gate's reviewer is never of the author's family (ROLE-9). The GitHub review bot is
a GPT model, so for a change a GPT model wrote a Neon or Carbon receipt is the gate.

- **ROLE-12 — Spread by subscription.** The table spreads work over the three
  subscriptions (GPT implements runtime work, Claude refactors and tooling, Gemini
  reviews and checker work); when one runs out, its rows move to their second choice.
- **ROLE-13 — Escalate once.** A balanced or fast seat whose work is sent back
  twice for the same finding moves that task to the frontier tier.
- **ROLE-14 — Record the model.** `ready` notes and receipts name the family
  and model that did the work, so ROLE-9 can be checked.

## 4. Resources

- **ROLE-15 — Memory and time caps.** Every build, test, probe and generated
  program runs under `hgl-ai cap SIZE -- CMD` with a time limit: 32 GiB for one
  compiler run or program, 64 GiB for a C++ build or a whole-workspace build or test
  run (a C++ build at 16 parallel jobs needs 1–4 GiB a job), and 96 GiB for an agent
  turn's whole tree (HPH, 2026-11-27).
  Reaching a cap is a finding, never a reason to raise it. Inside a turn a cap's
  scope joins the turn's slice, so the seat's limit holds over every cap the
  agent runs. Without systemd the cap also sets a per-process address-space limit;
  sanitizer runs (ASan, TSan) use `hgl-ai cap --no-address-limit`, which keeps the
  resident-memory watchdog. A tree in `/dev/shm` counts as memory; whoever makes
  one removes it when the run ends (the cap does not).
- **ROLE-16 — Disk.** Free space is in decimal GB (10^9 bytes) everywhere. The
  desk stops below 170 GB free. Each bridge makes no new tree (`assign`, `review`,
  a moved task's tree), in `drain` mode too, when `work_root` has less than its configured
  `min_free_gb`, and says so to the controller.
- **ROLE-17 — Hold and drain.** While `HGL_STATE/hold` exists, defer every
  GitHub call that can wait. In `drain` mode, finish in-flight work only and
  go idle.

## 5. Hosts

| Host class | Seats | Needs |
| --- | --- | --- |
| Hub | NATS server, controller, coordinator | Always reachable; small |
| Linux build hosts | implementors, reviewers, desk | Many cores, 128–256 GiB, 200 GB+ free NVMe |
| Platform hosts (macOS, Windows) | release and platform gates | Native toolchains |

Hosts share only GitHub and the hub. Moving a seat is stopping its bridge on
one host and starting it on another; queued notes follow, because the inbox
lives on the hub.
