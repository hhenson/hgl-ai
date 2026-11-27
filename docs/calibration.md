# Model calibration

Status: done 2026-11-27. The proposal at the end is now `docs/roles.md` §3 (agreed 2026-11-27).

## Contestants

| Id | Runtime | Model | Effort |
| --- | --- | --- | --- |
| opus-high, opus-xhigh | Claude Code | `claude-opus-5-5` | high, xhigh |
| neon-high, neon-xhigh | Antigravity | `gemini-4-neon` | high, xhigh |
| hydrogen-high, hydrogen-xhigh | Antigravity | `gemini-4-hydrogen` | high, xhigh |
| agyopus-high (task 2 only) | Antigravity | `claude-opus-5-5` | high |
| astra-high, astra-xhigh | Codex | `gpt-6.2-astra` | high, xhigh |
| sol-high, sol-xhigh | Codex | `gpt-6.2-sol` | high, xhigh |
| terra-high, terra-xhigh | Codex | `gpt-6.1-terra` | high, xhigh |

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
- At most 2 Claude, 4 Antigravity and 3 Codex runs at once, so wall times share one machine's load; the Codex runs started later, against a different mix.

## Scoring

Scored by the controller from hidden tests and cross-validation (the blind Carbon scoring was not run).

| Axis | Points | Evidence |
| --- | --- | --- |
| Correctness | 0–5 | Hidden programs and tests not shown to contestants; the repository's full suite |
| Root cause | 0–3 | Fix at the cause, not the symptom |
| Scope | 0–2 | No unrelated change; rules of the repository kept |
| Tests | 0–2 | Fail before, pass after, cover the cases |
| Report | 0–3 | Claims true and checkable; doubts stated |

Also recorded: wall time, turns and tokens where the runtime reports them.

## Reviews

- Cross-review: each result reviewed by models of the other families at xhigh, fresh context, one receipt (`docs/protocol.md` PROTO-19).
- Seeded review: the best t2 result with three planted defects, reviewed by every contestant. Recall and false findings are counted.

## Controller trials

`gemini-4-carbon-high`, and `gpt-6.2-astra` at high for comparison, on: the triage of all open issues (compared with an Opus triage of the same brief), an architectural assessment of an external review (compared with an Opus assessment and the controller's checks), and blind scoring of the task results (compared with the controller's scores).

## Results

One run per contestant and task, so differences of a few points are noise; the patterns below
held across tasks. Wall times shared one loaded machine; the GPT runs started later, under a
lighter load.

### Coding

| Contestant | t1: min, union cases (of 32) | t2: min, hidden 8 + probes (of 4) | t3: min, hidden (of 4) |
| --- | --- | --- | --- |
| sol-xhigh | 15, **31** (moved a submodule pin to an unpushed commit; edited a Python test) | 12, 8+2 | 15, 4 |
| astra-xhigh | 15, 27 | 17, 8+2 | 24, 4 |
| astra-high | 24, 27 (edited a Python test) | 11, 8+2 | 23, 4 |
| neon-xhigh | 36, 28 | 42, 8+**4** | 54, 3 |
| neon-high | 30, 19 | 28, 8+**4** | 34, 4 |
| hydrogen-xhigh | 27, 27 | 34, 8+3 | 60, 3 |
| hydrogen-high | 15, 19 | 21, 8+2 | 37, 4 |
| opus-high | 13, 19 | 25, 8+2 | 40, 4 |
| opus-xhigh | 23, 19 | 30, 8+2 | 48, 4 |
| agyopus-high | — | 22, 8+2 | — |
| sol-high | 13, 22 | 12, 8+2 | 14, 4 |
| terra-xhigh | 12, 15 | 12, 8+2 | 15, 3 |
| terra-high | 18, 11 | 8, 8+1 | 18, 3 |

- t1 (hgraph#1918): each fix ran every contestant's new cases (33, cross-validation; one case
  asserting combiner visit order excluded); the unfixed base passes 1. Most fixes are partial: they
  differ on growth across a re-point, empty or invalid collections, and a re-pointed zero.
- t2 (hgl-compiler#591): all thirteen pass the 8 hidden cases and the workspace suite (~1950 tests).
  The 4 probes are real defects that the seeded review found in one fix and nearly all shared:
  an `any` candidate beside list candidates, a list written in braces, a `var` parameter reported
  twice, and a lifted default whose C++ does not compile. Only Neon handled all four.
- t3 (structural `Eq`): all pass the workspace suite; the four at 3 of 4 compare a constant's
  spelling, which UNIT-13 forbids and no existing test catches.
- Effort: xhigh helped Sol, Neon and Hydrogen on t1 and not Opus; it cost Neon and Hydrogen the
  spelling rule on t3.
- Harness: Opus through Antigravity matched Opus through Claude Code on t2.

### Reviewing

Seeded review (t2 fix with three planted defects, `calib/seeded`):

| Reviewer | min | Planted found | Also |
| --- | --- | --- | --- |
| astra-xhigh | 7 | 3 | — |
| sol-xhigh | 9 | 3 | — |
| opus-xhigh | 10 | 3 | false author report; stale rule-table notes |
| hydrogen-xhigh | 11 | 3 | false author report |
| neon-xhigh | 14 | 3 | false report; **a real defect** (lifted default, C++ fails) |
| carbon-high | 20 | 3 | false report; **three real defects** (`any` widening, braces, double `var` report) |
| terra-xhigh | 3 | 2 | reviewing its own model's work; not independent |

Cross-review of four t1 fixes with known defects (14 reviews):

- Every reviewer except Opus gave the right verdict (`findings`) on every fix it reviewed, and named
  the main defect (unsampled links on growth; empty and switched re-points; the zero).
- **Opus xhigh returned `clean` on all three fixes it reviewed**, each with known defects, rating
  them minor, including the submodule pin to an unpushed commit that Neon rated a blocker.
- GPT reviewers (Astra, Sol, Terra) were fastest (6–10 min) with the right verdict; Neon and
  Hydrogen took 20–55 min and went deepest.

### Controller trials

On the triage of all open issues and the assessment of the architecture review, Carbon, Opus and
Astra 6.2 agreed on the substance; Carbon was the most thorough, Astra the most empirical and
concise, Opus the most practical (it wrote and tested a patch). Carbon's later review of hgl-ai
itself found 30 findings across two pull requests, several of them security issues the GPT line
review missed.

## Proposal for `docs/roles.md` §3

| Work | First choice | Second | Not alone |
| --- | --- | --- | --- |
| Controller, architecture | Carbon high | Astra 6.2 high | — |
| Triage | Astra 6.2 high | Carbon high | — |
| Implementation, runtime and semantics (hgraph, the C++ runtime) | Sol 6.2 xhigh, Astra 6.2 xhigh | Neon xhigh | Opus, Terra, Hydrogen high |
| Implementation, checker edge cases | Neon xhigh | Astra 6.2 xhigh | Terra |
| Implementation, cross-cutting refactors (analyser, IR) | Opus high | Astra 6.2 xhigh | Terra |
| Implementation, tooling (hgl-ai, scripts, CI) | Opus high | Sol 6.2 xhigh | — |
| Review (gate) | Neon xhigh or Carbon high | Astra 6.2 xhigh (fast) | Opus (lenient verdicts) |
| Review (design, code, logic) | Carbon high | Neon xhigh | — |
| Build agent, mechanical | Sol 6.2 high | Hydrogen high | — |

The GitHub review bot is a GPT model: for a change a GPT model wrote it is not the independent
review ROLE-11 needs, and a Neon or Carbon receipt is the gate.

Opus implements where it was strong: cross-cutting refactors (t3: 4/4 at high and xhigh alike)
and tooling (most of hgl-ai was written by Opus agents and held up under Carbon's and the GPT
bot's reviews). Its high effort matched xhigh, so high is the default. It does not review a gate
alone: its verdicts were lenient.
