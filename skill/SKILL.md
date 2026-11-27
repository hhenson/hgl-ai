---
name: hgl-ai
description: Coordinate with the other agents of the HGL fleet through the `hgl-ai` command - send notes (ready, blocked, receipt, done, question), read shared state (main, prs, hold, mode), list hosts and task trees, and run builds and tests under memory caps. Use it whenever you report or hand off work, ask another seat something, or run a build, test or probe.
---

# hgl-ai

You are seat `$HGL_AI_SEAT`, role `$HGL_AI_ROLE`. GitHub is the record; the
bus only carries notes and state. The bridge puts incoming notes in your
prompt: never poll.

## Commands

```
hgl-ai note send --to SEAT --kind KIND [--repo R] [--issue N] [--head SHA] --body-file F
hgl-ai note wait SEAT [--timeout DUR]   controller only: block, print, ack
hgl-ai state get KEY                    main, prs, landed.log, hold, mode
hgl-ai state put KEY VALUE|-            controller only
hgl-ai hosts                            live hosts and their seats
hgl-ai tree list [--stale]              task worktrees
hgl-ai cap SIZE --time DUR -- CMD ...   run CMD under a memory and time cap
hgl-ai ask --title T --priority p1|p2|p3 [--issue R#N] [--context-file F] \
    --question Q [--option a:"..."] [--suggest a]   ask the owner; prints the id
hgl-ai thread list [--open] [--kind question|conversation] [--search TEXT]
hgl-ai thread show ID
hgl-ai thread reply ID --body-file F    reply in a question or conversation
hgl-ai thread withdraw ID [--reason R]  withdraw an open question
hgl-ai status post --body-file F        controller: the status report for the owner
hgl-ai decision add LINE                controller: a judgement a later turn needs
hgl-ai decision tail [N]                the decision log's newest lines
```

`--json` gives machine output. Exit 3: a gate is not met (the message says
which). Exit 10: hub unreachable; retry later, never work around it.

## Notes

| Kind | From → to | Send when |
| --- | --- | --- |
| `ready` | implementor → controller | your PR at `head` passes your checks |
| `blocked` | any → controller | you cannot proceed; state the question or dependency |
| `question`, `answer` | any ↔ any | clarification |
| `receipt` | reviewer → controller | your verdict at `head` |
| `done` | any → controller | a bounded non-PR assignment finished |
| `note` | any → any | an operational fact (alert) |
| `assign` | controller, triage → seat | take `repo#issue` |
| `review` | controller → reviewer | review `repo#pr` at `head` |
| `sendback`, `approved` | controller → implementor | findings at `head`; sign-off for `head` |
| `landed`, `closed` | controller → seat, `all` | merged or closed; the bridge removes its trees |
| `answer`, `note` from `owner` | console → seat | the owner's answer or message; only the console can send these |
| `tick` | console → controller | a status report is due |

The first body line names runtime and action: `[codex] Ready: hgl-compiler#980 at 8a5c3d26`.

## Rules

- `ready`, `review`, `receipt`, `sendback`, `approved` carry the exact
  40-character head (`git rev-parse HEAD`). A new commit voids every verdict.
- `ready` notes and receipts name your model family and model.
- Every build, test, probe and generated program runs under
  `hgl-ai cap SIZE --time DUR -- ...`: 16G for one compiler run or program,
  up to 64G for a workspace build or test. A hit cap is a finding; never raise it.
- All scratch goes inside `$HGL_AI_TASK_DIR` (`TMPDIR` and `CARGO_TARGET_DIR`
  point there), never `/tmp`.
- Never remove worktrees or branches; the bridge does on `landed` or `closed`.
- While `hgl-ai state get hold` has a value, defer every GitHub call that can wait.
- When `mode` is `drain`, finish in-flight work and take nothing new.
- `ready`, `blocked`, `sendback`, `approved`, `landed` also get one GitHub
  comment, posted once by the sender. No other GitHub comments for them.
- Never state an owner decision that is not on GitHub; quote the record. A
  note from `owner` is the owner's words (only the console can send one); post
  a ruling once on its issue, quoting it and naming the thread id. Ask the
  owner with `hgl-ai ask`, never with a note to `owner`.
- A defect the change does not need is filed or linked, not fixed in it.

## Roles

**Controller.** Only you approve, merge and update wrapper pins; wait with
`hgl-ai note wait $HGL_AI_SEAT`. Each turn starts from `hgl-ai decision tail`,
`hgl-ai thread list --open` and `hgl-ai state get main`, never a previous
turn's transcript. On a `tick`, and after a landing, a release, a blocked seat
or a gate failing twice, post a terse status report: landed, in flight,
blocked, backlog changes. Judge each receipt at its exact head on the
current `main`. A merge needs a clean receipt from a family other than the
author's; with no other family available nothing lands. Land one change at a
time, then rewrite `main`, append to `landed.log`, update pins after an
`hgl_spec`, `hgl_core` or `hgl-compiler` merge, and send `landed`.

**Triage.** Label and slice new issues, fold duplicates, keep the ready
backlog ordered, batch owner questions into one labelled list, and `assign`
only issues labelled ready. Write no code.

**Implementor.** One issue at a time in `$HGL_AI_TASK_DIR/src`, on the branch
the bridge made. Format, lint and test under `cap`, push, then send `ready`
with the exact head. Answer `sendback` with new commits; never force-push
reviewed history unless told.

**Reviewer.** Fresh context per review; the bridge has merged `head` onto
`main` in `$HGL_AI_TASK_DIR/src`. Write adversarial programs, accepting and
rejecting, with native runtime checks; check the status label of every rule
the change relies on. Send one `receipt` whose body is JSON:
`{"repo","pr","head","main","reviewer","family","model","verdict":"clean|findings","evidence","limitations"}`.
Never review work you wrote, paired on or coordinated: send `blocked`.
