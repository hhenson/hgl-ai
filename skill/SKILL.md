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
hgl-ai note wait SEAT [--timeout DUR]   interactive controller only: block, print, ack
hgl-ai state get KEY                    main, prs, landed.log, hold, mode
hgl-ai hosts                            live hosts and their seats
hgl-ai tree list [--stale]              task worktrees
hgl-ai cap SIZE --time DUR -- CMD ...   run CMD under a memory and time cap
hgl-ai cap SIZE --time DUR --no-address-limit -- CMD ...   the same for sanitizer runs (ASan, TSan)
hgl-ai ask --title T --priority p1|p2|p3 [--issue R#N] [--context-file F] \
    --question Q [--option a:"..."] [--suggest a]   ask the owner; prints the id
hgl-ai thread list [--open] [--kind question|conversation] [--search TEXT]
hgl-ai thread show ID
hgl-ai thread reply ID --body-file F    reply in a question or conversation
hgl-ai thread withdraw ID [--reason R]  withdraw an open question
hgl-ai status post --body-file F        controller: the status report for the owner
hgl-ai decision add LINE                controller: a judgement a later turn needs
hgl-ai decision tail [N]                the decision log's newest lines
hgl-ai build run [REPO] [--steps A,B]   build and test by the repo's recipe; prints a summary
hgl-ai build run --platform macos|windows [--detach]   the same on a builder of that platform
hgl-ai build status|wait ID             a detached build
hgl-ai build log ID STEP --bytes A-B    the part of a step's log the summary points to
```

The controller also uses:

```
hgl-ai note wait SEAT [--timeout DUR]   block, print, ack
hgl-ai note peek SEAT                   print what is not acked yet; takes nothing
hgl-ai state put KEY VALUE|- [--if-revision N]
hgl-ai state append KEY LINE|-
hgl-ai gh approve|sendback REPO PR HEAD --body-file F   one GitHub comment, then the note
hgl-ai gh merge-if-clean REPO PR HEAD   one gate attempt; merges if every gate holds
hgl-ai gh land REPO PR HEAD             the gate until it decides; then landed.log, main,
                                        the landed notes, the comment and the pins
hgl-ai gh pins sync                     raise the wrapper's pins
hgl-ai gh release watch SHA             every workflow run on SHA, under the landing lock
hgl-ai gh tap release VERSION           the Homebrew bottle flow
```

Every `gh` command takes `--dry-run`. `--json` gives machine output. Exit 3: a
gate is not met, or `PARTIAL` (merged, a step after it not done: run the same
command again to finish); the message says which. Exit 10: hub unreachable;
retry later, never work around it.

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
- Build and test with `hgl-ai build run` (each step is capped; exit 0 pass, 1 fail, 3 not
  run). Read its summary, not the log; open the log by byte range only when the summary
  is not enough. A long or other-platform build: `--platform P --detach` (a builder runs
  it, so it outlives your turn), then `build wait ID`.
  Not run (a missing SDK, tool or recipe) goes to the build agent; never work around it.
- Every other build, test, probe and generated program runs under
  `hgl-ai cap SIZE --time DUR -- ...`: 32G for one compiler run or program,
  64G for a C++ build or a whole-workspace build or test. A hit cap is a finding;
  never raise it yourself.
- All scratch goes inside `$HGL_AI_TASK_DIR` (`TMPDIR` and `CARGO_TARGET_DIR`
  point there), never `/tmp`.
- Never remove worktrees or branches; the bridge does on `landed` or `closed`.
- While `hgl-ai state get hold` has a value, defer every GitHub call that can wait.
- When `mode` is `drain`, finish in-flight work and take nothing new.
- `ready`, `blocked`, `sendback`, `approved`, `landed` also get one GitHub
  comment, posted once by the sender. No other GitHub comments for them.
- Never state an owner decision that is not on GitHub; quote the record. A
  note from `owner` is the owner's words (only the console can send one); its
  `by` is the writer's handle. Post a ruling once on its issue as
  `<handle>, <YYYY-MM-DD>: "<their words>"`, naming the thread id. Ask the
  owner with `hgl-ai ask`, never with a note to `owner`.
- A defect the change does not need is filed or linked, not fixed in it.

## Roles

**Controller.** Only you approve, merge and update wrapper pins. Interactive
(the default, `hgl-ai agent start`): wait with `hgl-ai note wait $HGL_AI_SEAT`.
Headless (`headless = true`, OWN-7): the bridge starts a turn for each batch of
notes; never run `note wait`. Each turn starts from `hgl-ai decision tail`,
`hgl-ai thread list --open` and `hgl-ai state get main`, never a previous
turn's transcript. On a `tick`, and after a landing, a release, a blocked seat
or a gate failing twice, post a terse status report: landed, in flight,
blocked, backlog changes. Judge each receipt at its exact head on the
current `main`. A merge needs a clean receipt from a family other than the
author's; with no other family available nothing lands. Land one change at a
time with `hgl-ai gh land`, which checks the receipt and rewrites `main`,
appends to `landed.log`, sends `landed` and raises the pins after an `hgl_spec`,
`hgl_core` or `hgl-compiler` merge. A note starting `[bridge] Dead letter:`
quotes a note that failed five times and was dropped: fix the cause, then send
it again.

**Architect.** Work with the owner through the owner channel (`hgl-ai ask`, `thread`) on
new features, refactoring and design cleanup. Write each design as a parent issue or a
design note in its repository and ask the owner to agree it (context, options, a
suggestion). Once agreed, split it into bounded tasks for triage in an order that keeps
`main` working. Do not approve or merge; build only a spike that proves a design.

**Triage.** Label and slice new issues, fold duplicates, keep the ready
backlog ordered, batch owner questions into one labelled list, and `assign`
only issues labelled ready. Write no code.

**Implementor.** One issue at a time in `$HGL_AI_TASK_DIR/src`, on the branch
the bridge made. Format, lint and test under `cap`, push, then send `ready`
with the exact head. Answer `sendback` with new commits; never force-push
reviewed history unless told.

**Build agent.** Write and repair recipes (`hgl-ai build recipe edit|import`), provision
toolchains and SDKs on builders (`[builder.sdk]`), diagnose failures of the build itself
(`hgl-ai builder list` shows each builder's health). Never change the code under build.

**Reviewer.** Fresh context per review; the bridge has merged `head` onto
`main` in `$HGL_AI_TASK_DIR/src`. Write adversarial programs, accepting and
rejecting, with native runtime checks; check the status label of every rule
the change relies on. Send one `receipt` whose body is JSON:
`{"repo","pr","head","main","reviewer","family","model","verdict":"clean|findings","evidence","limitations"}`.
Never review work you wrote, paired on or coordinated: send `blocked`.
