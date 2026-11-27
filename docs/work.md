# Work on the HGL repositories

Status: proposed. Replaces hhenson/hgl-compiler#334 (the file-queue protocol).
Roles and gates are in `docs/roles.md`; notes and worktrees in `docs/protocol.md`.

## 1. Issues and labels

- **WORK-1 — Readiness** (one per open issue, set by triage):
  - `codex`: claimable now.
  - `needs-owner`: waits on a decision only the owner makes; the comment names it.
  - `codex-blocked`: waits on another issue, repository or condition; the comment names it.
  - `codex-claimed`: a seat holds it. `codex-ready`: its pull request is Ready at a head, awaiting the controller.
  - `codex-priority`: taken before other `codex` issues.
- **WORK-2 — Kind** (one per open issue): `wrong-acceptance`, `wrong-rejection`,
  `wrong-behaviour`, `target-gap`, `diagnostic`, `performance`, `enhancement`,
  `documentation`, `tooling`, `meta`; at most one root cause `arch-F1`..`arch-F5`.
- **WORK-3 — Order.** Triage assigns with an `assign` note: `codex-priority` first, then
  oldest first. `After #N` waits until #N is closed; `Start when …` waits for that.
- **WORK-4 — Who wrote it.** Agent comments start with their runtime tag (`[codex]`,
  `[claude]`, `[agy]`) and, for a seat, its name (`[codex] (w2)`). An untagged comment
  is the owner's, and the owner's word decides.

## 2. A task

1. The bridge makes the worktree (`docs/protocol.md` §5). Comment `[…] Claimed (w2).`
   and add `codex-claimed`. Read every comment of the issue first.
2. Read the `AGENTS.md` of every repository the task touches; they bind. The
   specification and Core are the authority: language behaviour is never decided in code
   or in a case.
3. Branch `codex/q<issue>-<short-name>` from `origin/main` (the bridge names it). No
   force-push and no rebase of a pushed branch: bring `main` in with `git merge`.
4. Do what the issue lists and nothing else. A defect outside it goes in a new,
   unlabelled issue (ROLE-11).
5. Run every check `AGENTS.md` names, under `hgl-ai cap`. A change reaching the C++
   target also runs that target's checks, plain and sanitized, against the pinned
   hgraph SDK. The list of unsupported cases does not grow.
6. Open the pull request: what changed, the rule ids, the checks with counts, and
   `Closes #N` (in another repository `Part of hhenson/hgl-compiler#N`). Where Core and
   the compiler both change, Core's pull request comes first and the compiler pins it.
7. Review bot, two rounds at most once hosted CI is green (`@codex review`): fix a
   finding that is a defect of the change; file anything else as a new issue, link it,
   resolve the thread. After the second round go on, listing what is open.
8. Take it out of draft, comment `[…] Ready:` with the links, the exact head and the
   check counts, add `codex-ready`, and send a `ready` note with the head. Do not merge.
   A `sendback` makes the task yours again; answer it with new commits.

A task with no pull request (a review, an investigation) ends with `[…] Done:`, its
findings, and a `done` note.

## 3. Several tasks

- **WORK-5 — Unrelated** means the changes do not meet: not the same rule, not the
  same functions of the parser, analyser or emitter. Unrelated tasks run in parallel,
  each with its own worktree, branch and pull request.
- **WORK-6 — Landing order.** Ready pull requests land one at a time in Ready order,
  each after its gates on the `main` it merges onto (ROLE-8); ones that merge cleanly
  together may land as one batch. When `main` moves, merge it into each open branch it
  now conflicts with and rerun the checks; a pull request that conflicts is not Ready.
- **WORK-7 — One conflict once.** Two tasks that turn out to touch the same code: finish
  one, then merge it into the other.

## 4. When you cannot go on

A question the pinned specification, Core or the issue does not settle; a check failing
for a reason outside the task; a refused action. Stop that task: comment `[…] Blocked:`
with the question or the output (quote the rule's text), add `codex-blocked`, send a
`blocked` note, take the next task. Never work around a refusal. Never settle a language
question yourself.

## 5. Keep out of everything you write

Machine names, home or workspace paths, credentials, email addresses, private projects.
Keep comments short.

## 6. Reviews of hgl-ai itself

- **WORK-8 — Three families.** A change to hgl-ai needs the GPT review bot's line review and a
  Carbon (`gemini-4-carbon`) review of its design, code and logic (HPH, 2026-11-27: "get this
  design, code and logic reviewed by at least Carbon"). Blockers and majors are fixed or filed
  before it lands.

## 7. The factory working on itself

- **WORK-9 — Self-hosting.** hgl-ai's own issues go through the factory like any other
  repository's. A task builds and tests in its worktree against hub instances of its own
  (the test suites start their own `nats-server`); it never touches the running hub or services.
- **WORK-10 — Deploying.** A landed change to hgl-ai is not live until the controller deploys it:
  install the binary from the merge commit, keep the previous one beside it, restart the services
  host by host (hub last), and run `doctor` on each. A failed `doctor` restores the previous binary.
  The factory never deploys itself automatically.
