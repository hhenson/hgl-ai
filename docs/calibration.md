# Model calibration

Status: running. Results replace the provisional tiers in `docs/roles.md` §3.

## Contestants

| Id | Runtime | Model | Effort |
| --- | --- | --- | --- |
| opus-high, opus-xhigh | Claude Code | `claude-opus-5-5` | high, xhigh |
| neon-high, neon-xhigh | Antigravity | `gemini-4-neon` | high, xhigh |
| hydrogen-high, hydrogen-xhigh | Antigravity | `gemini-4-hydrogen` | high, xhigh |
| agyopus-high (task 2 only) | Antigravity | `claude-opus-5-5` | high |

`agyopus-high` separates the harness from the model.

## Tasks

| Id | Repository | Kind |
| --- | --- | --- |
| t1 | hgraph | Runtime fix: ordered `compiled::reduce_` over a re-pointed reference (hgraph#1918) |
| t2 | hgl-compiler | Checker fix: operator candidates over `list<T>` admit fixed-size lists, sized beats unsized (hgl-compiler#591) |
| t3 | hgl-compiler | Cross-cutting refactor: structural, reflexive `Eq` on `Constant` and `Type`, with an explicit semantic comparison |

## Conditions

- One brief per task, identical for every contestant; one headless turn, no questions answered.
- Same base commit; a fresh clone per run; pushing and GitHub disabled.
- Same caps: every build and test under `memcap 16G` (32G for a workspace run); build parallelism 16; 4 h wall limit.
- At most 2 Claude and 4 Antigravity runs at once, so wall times share one machine's load.

## Scoring

Scored by the controller, then independently by a controller trial (Carbon), blind to contestant ids.

| Axis | Points | Evidence |
| --- | --- | --- |
| Correctness | 0–5 | Hidden programs and tests not shown to contestants; the repository's full suite |
| Root cause | 0–3 | Fix at the cause, not the symptom |
| Scope | 0–2 | No unrelated change; rules of the repository kept |
| Tests | 0–2 | Fail before, pass after, cover the cases |
| Report | 0–3 | Claims true and checkable; doubts stated |

Also recorded: wall time, turns and tokens where the runtime reports them.

## Reviews

- Cross-review: each result reviewed by the other two models at xhigh, fresh context, one receipt (`docs/protocol.md` PROTO-19).
- Seeded review: the best t2 result with three planted defects, reviewed by every contestant. Recall and false findings are counted.

## Controller trials

`gemini-4-carbon-high` on: the triage of all open issues (compared with an Opus triage of the same brief), an architectural assessment of an external review (compared with an Opus assessment and the controller's checks), and blind scoring of the task results (compared with the controller's scores).

## Results

Pending.
