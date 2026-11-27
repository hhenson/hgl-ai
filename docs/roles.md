# Roles and model assignment

Status: proposed. Open decisions are issues in this repository.

## 1. Team

```
                 Owner
       specs, priorities, tags, budget
                   │
              Controller ───────────── Triage
      verdicts, merges, pins       labels, slices, owner
                   │               question batches
       ┌───────────┼────────────┐
  Implementors  Reviewers    Services (no model)
  one task each  other family coordinator, desk, bridge
```

- **ROLE-1 — Owner.** Decides language and specification questions, release
  tags, priorities, and the fleet's budget and mode.
- **ROLE-2 — Controller** (seat `claude`, whatever runtime holds it). The only
  role that approves, merges and updates wrapper pins. Judges receipts, adds a
  few probes of its own, sends back or approves, lands one change at a time
  against the current `main`, and updates `main` and `landed.log`.
- **ROLE-3 — Triage** (seat `triage`). Owns intake: labels and slices new
  issues, folds duplicates, keeps the claimable backlog ordered, and batches
  questions for the owner into one labelled list. Writes no code. Workers
  claim only what triage labelled ready.
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
    merged onto `main`, one run at a time, and sends a factual receipt.
  - *bridge*: delivers notes, runs agent turns, owns worktrees (protocol §5).
  - *builder*: runs build recipes for local and remote requests and the merge queue,
    and returns bounded summaries (`docs/build.md`). The desk's sets are recipe steps.
  - *merge queue*: builds approved pull requests in batches on every platform and lands
    them (`docs/build.md` §3).
- **ROLE-6a — Build agent** (seat `build`). Writes and repairs recipes, provisions
  toolchains and SDKs, diagnoses failures of the build itself; never changes the code
  under build.

## 2. Gates

- **ROLE-7 — Exact head.** Every receipt and verdict binds one 40-character
  head. A new commit voids them.
- **ROLE-8 — Current main.** A change lands only after its gates ran on the
  `main` it merges onto. When `main` moves, the next candidate is revalidated.
- **ROLE-9 — Independence.** A merge needs a clean review receipt whose model
  family differs from the author's. An agent never reviews work it wrote,
  paired on or coordinated. When no other family is available, nothing lands:
  same-family review is never a fallback.
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

The Gemini column is provisional until `docs/calibration.md` has results.
`gemini-4-carbon` (close to Claude Fable) is trialled for controller and
triage. Concrete slugs live in configuration (`[models]`), so a new release
changes one file.

| Work | Tier, effort | Primary family | Alternate |
| --- | --- | --- | --- |
| Controller | frontier, high | claude | gpt |
| Specification text | frontier, high | claude | gpt |
| Implementation: analyser, types, generics, runtime semantics | frontier, high | gpt | claude |
| Implementation: target coverage, diagnostics, docs, tooling | balanced, high | gpt | gemini |
| Review | frontier, xhigh | any family but the author's | the third family |
| Triage | balanced, medium | claude | gemini |
| Bulk mechanical edits (renames, label sweeps) | fast, medium | gemini | gpt |
| Build agent: recipes, toolchains, build failures | balanced, high | gpt | claude |
| Coordinator, builder, merge queue, bridge | no model | — | — |

- **ROLE-12 — Spread by subscription.** Implementors run mostly on `gpt`, so
  `claude` and `gemini` keep capacity for control, review and triage.
- **ROLE-13 — Escalate once.** A balanced or fast seat whose work is sent back
  twice for the same finding moves that task to the frontier tier.
- **ROLE-14 — Record the model.** `ready` notes and receipts name the family
  and model that did the work, so ROLE-9 can be checked.

## 4. Resources

- **ROLE-15 — Memory and time caps.** Every build, test, probe and generated
  program runs under `hgl-ai cap SIZE -- CMD` with a time limit: 16 GiB for one
  compiler run or program, up to 64 GiB for a workspace build or test run.
  Reaching a cap is a finding, never a reason to raise it. A tree in
  `/dev/shm` counts as memory and is removed when the run ends.
- **ROLE-16 — Disk.** The desk stops below 170 GB free. Each bridge stops
  taking `assign` notes when `work_root` has less than its configured
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
