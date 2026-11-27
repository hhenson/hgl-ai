# Roles, Governance Rules, and Multi-Host Distribution

This document defines the agent roles in an HGL engineering fleet, their separation-of-duties invariants, resource/cleanup rules, and how roles distribute across machines and model providers.

---

## 1. Roles & Separation of Duties

```
                          ┌───────────────────┐
                          │   Owner (Human)   │
                          │ Specs, priorities │
                          │ tags, drain/budget│
                          └─────────┬─────────┘
                                    │
                          ┌─────────▼─────────┐
               ┌─────────►│    Controller     │◄─────────┐
               │          │ (inbox: `claude`) │          │
               │          │ Probes, verdicts, │          │
               │          │ merges, pin sync  │          │
               │          └──┬──────┬──────┬──┘          │
               │             │      │      │             │
      PR table │    assign / │      │      │ exact-SHA   │ factual
     & urgent  │    sendback │      │      │ review req  │ set diffs
       alerts  │    / landed │      │      │             │
        ┌──────┴──────┐      │      │      │      ┌──────┴──────┐
        │ Coordinator │      │      │      │      │    Desk     │
        │  (`coord`)  │      │      │      │      │  (`desk`)   │
        └─────────────┘      │      │      │      └─────────────┘
                             ▼      │      ▼
                    ┌───────────────┴───────────────┐
                    │                               │
           ┌────────▼────────┐             ┌────────▼────────┐
           │  Implementors   │  disjoint   │   Independent   │
           │  (`w1` .. `wN`) │◄───────────►│    Reviewers    │
           │ 1 worktree/task │  ownership  │ Zero shared ctx │
           └─────────────────┘             └─────────────────┘
```

### 1.1 Owner (Human)
- Resolves language/specification ambiguities, approves release tag creation, and sets credit/fleet directives (`normal` vs. `drain`).
- Bus notes represent agent/queue traffic, never fabricated owner statements.

### 1.2 Controller (`claude` / `main`)
- **Sole merge and verdict authority.** No other role merges pull requests or updates `hgl-project` wrapper pins.
- Assigns slices, writes targeted positive/negative differential probes, inspects `desk` output and independent review receipts, resolves outdated review threads once verified, and posts `sendback` or `approved`.
- Executes multi-platform landing gates and updates `HGL_STATE/main` and `HGL_STATE/landed.log` immediately after each merge.
- **Provider-independent address:** The controller's inbox name stays `claude` even when the active session hands over between Claude, GPT, or Gemini models due to quota/credit rotation.

### 1.3 Coordinator (`coord`)
- Maintains `HGL_STATE/prs` via a single batched GitHub GraphQL query at most once every 5 minutes so neither the controller nor workers poll GitHub redundantly.
- Emits immediate `note` alerts to `claude` when a `ready` or `approved` PR receives a hosted review-bot finding or CI failure between table reads.
- Relays `hold` and `drain` state changes and coordinates staging/integration branches when required.

### 1.4 Desk (`desk`)
- **Strictly mechanical regression runner.** Authors no PRs, reviews no PRs, and edits no scripts or source files.
- Triggers on `ready` notes for `hgl-compiler` PRs (or explicit controller rerun requests when `main` advances under an approved head); ignores drafts and doc-only repos.
- Acquires an exclusive host lock (one run at a time, FIFO by `ready` order), verifies free disk (`>= 170 GB`), merges `HEAD` onto current `main`, builds and executes fixed regression sets under `memcap`, writes the raw report, and posts one factual `note` containing only the exit code and report path—**never** an editorial summary ("clean" or "regression").

### 1.5 Implementors (`w1` .. `wN`)
- Own one bounded issue or PR slice at a time inside a dedicated Git worktree.
- Run formatting, linters, unit tests, and target tests locally inside `memcap` before declaring `ready` with the exact 40-character `HEAD` SHA.
- Never force-push over reviewed history without instruction; address `sendback` findings with new commits and a fresh `ready` note.
- Perform mandatory worktree, branch, and build-directory removal upon receiving `landed`.

### 1.6 Independent Reviewers (`reviewer` / topic-named reviewers)
- **Strict independence:** An agent must **never** review a PR it authored, paired on, or coordinated, and starts from a clean conversation context.
- Preferably runs on a **different model family** than the implementor (e.g., Claude Opus or Gemini Carbon reviewing a Codex GPT PR, or vice versa) to eliminate correlated model blind spots.
- Merges exact `HEAD` onto current `main` in a temporary worktree, verifies specification rule status labels (`Agreed` vs. `Proposed`), constructs independent adversarial programs and native runtime assertions, and files a JSON receipt (`clean` or `findings`).

---

## 2. Gate & Revision Invariants

1. **Exact-SHA Binding:** Every `ready` note, `desk` report, hosted bot review, independent review receipt, and `approved` verdict applies **only** to the exact 40-character commit SHA tested. Any new commit pushed to the PR invalidates prior verdicts.
2. **Main-Advance Revalidation:** Compiler merges serialize against `main`. When `main` advances, the next candidate PR is validated merged onto the **new** `main` head (`desk` + platform gates) before landing.
3. **Lockstep Pin Updates:** Merging a normative change to `hgl_spec`, `hgl_core`, or `hgl-compiler` is followed immediately by updating submodule pins in `hgl-project` and rewriting `HGL_STATE/main`.
4. **No Silent Scope Creep:** Pre-existing unrelated defects discovered during review are logged/cross-referenced to their tracking issue rather than silently bundled into an unrelated PR.

---

## 3. Multi-Host Distribution Model

### 3.1 Network & State Decoupling
- **Zero shared filesystem required.** Hosts share **only** Git remotes (GitHub) for code and NATS JetStream (`port 4222`) for notes and `HGL_STATE`.
- If `desk` or `reviewer` runs on a remote host, any large markdown/JSON receipt is either included in the note body (for compact JSON receipts) or pushed/uploaded to a configured artifact store rather than assuming local `/tmp` visibility.

### 3.2 Recommended Host Topology

| Host Class | Typical Roles Hosted | Sizing & Constraints |
| --- | --- | --- |
| **Hub / Control Node** | `nats-server -js`, `claude` (Controller), `coord` | Always-on (or WireGuard/Tailscale reachable); low RAM for broker (`<= 1 GiB`); fast SSD for JetStream store. |
| **Heavy Linux Build Host(s)** | `desk` (1 serialized lane), `w1`..`wK`, `reviewer_*` | High core count, `128–256 GiB` RAM, `>= 200 GB` free NVMe; `systemd` user slice enabled for `memcap`. |
| **Secondary Worker Host(s)** | Additional `wK+1`..`wN` and cross-model `reviewer_*` via `hgl-bridge` | Scales horizontally; each host advertises its own `memcap_gb` in `HGL_HOSTS`. |
| **Platform Verification Nodes** (macOS Apple Silicon, Windows) | Integrated target/release verification gates | Run native target toolchains/SDKs for three-platform landing and release validation without contending with Linux `desk`. |

### 3.3 Live Worker Migration & Quota Failover
- **Host migration:** Because consumer state (`inbox-w4`) lives in JetStream on the hub, moving `w4` to another machine is stopping `hgl-bridge` for `w4` on Host A and launching `--worker w4:...` on Host B. Unacknowledged or queued notes deliver automatically on Host B.
- **Model/provider failover:** Because agent identities (`claude`, `w4`, `reviewer_types`) are logical JetStream subjects decoupled from CLI binaries, exhausting quota on one vendor (e.g., Claude Code or Codex) allows rebinding that worker slot to `agy` (`gemini-4-carbon-high` or `claude-opus-5-5-high`) or another CLI without changing peer addresses.

---

## 4. Per-Host Execution, Memory, and Cleanup Rules

### 4.1 Hard Memory & Time Caps (`memcap`)
- Every compiler build, test suite, probe, and generated target executable **must** run inside a bounded memory/process group wrapper (`memcap <SIZE> timeout -k 10 <SECS> <CMD>`):
  - **`16G` default** for a single compiler invocation or generated program.
  - **Up to `64G`** for a full workspace build or test run.
  - **Cold C++ runtime cache:** Warm once at `16G` with a finite program before running potentially non-terminating probes at `4G`.
- Always pass `-k 10` to `timeout` so processes catching `SIGTERM` receive `SIGKILL`.
- Reaching a memory cap (`exit 137` / `143`) is a **finding to report**, never justification to raise the cap.
- Any temporary tree placed in `/dev/shm` counts directly against RAM and **must** be deleted before the turn exits.

### 4.2 Worktree Isolation & Post-Landing Removal
- **Exclusive ownership:** Never run two agents in the same checkout directory; create a dedicated `git worktree` per issue/review (`git worktree add`).
- **Remove means remove:** When a `landed` note arrives (or a PR is closed/superseded), the owning implementor—and any reviewer or controller that created a temporary tree for that PR—must:
  1. Check for uncommitted scratch items worth preserving in an issue comment.
  2. Run `git worktree remove <path>` followed by `git worktree prune`.
  3. Delete the local feature branch and any per-task build/cache/scratch directories before starting the next task.
- **Touch only your own:** An agent never deletes a directory owned by another worker; orphaned trees from retired workers are reported to `claude` in a `note`.

### 4.3 Rate-Limit (`hold`) & Credit (`drain`) Discipline
- Before making any non-essential GitHub API call, check `HGL_STATE/hold`. While `hold` is set, wait until the recorded reset time.
- When `HGL_STATE/mode` is `drain` (or a broadcast `drain` note is received), finish in-flight gates/fixes only, open no new tasks, and go idle upon completion.
