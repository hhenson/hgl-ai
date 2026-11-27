# Builds

Status: proposed. Building and testing is deterministic work: a tool runs it, an agent
only writes and repairs the instructions and reads the summary.

## 1. Recipes

- **BUILD-1 — A recipe per repository.** A recipe names the steps that build and test a
  repository on each platform. It lives on the hub (`HGL_BUILD/recipe.<repo>`, versioned)
  so every host runs the same one; `hgl-ai build recipe show|edit|import|export` manage it.
- **BUILD-2 — Steps.** Each step has a name, a command, its working directory, environment,
  the platforms it runs on, a memory cap and a time limit (`hgl-ai cap`), what it needs
  first (an SDK, a toolchain, an earlier step), and a **parser** for its output:

  | Parser | Reads |
  | --- | --- |
  | `cargo` | `--message-format=json` diagnostics and `cargo test` results |
  | `gcc`, `clang`, `msvc` | compiler and linker errors and warnings |
  | `ctest`, `catch2`, `junit` | test cases, failures and their assertions |
  | `hgl-test` | `hgl test` rejection and executable case results |
  | `lines` | lines matching configured patterns, as a fallback |

```toml
[[step]]
name = "test"
run = "cargo test --workspace --locked --message-format=json"
platforms = ["linux", "macos", "windows"]
memcap_gb = 32
time = "40m"
parser = "cargo"
needs = ["sdk:hgraph"]
```

- **BUILD-3 — Who writes them.** The build agent (`docs/roles.md`) writes and repairs
  recipes, provisions toolchains and SDKs on hosts, and diagnoses failures of the build
  itself. It never changes the code under build.

## 2. Running a build

- **BUILD-4 — One command.** `hgl-ai build run REPO [--steps …] [--platform P]` runs the
  recipe in the current task's worktree, each step under its cap, the full log kept in the
  task directory. It prints a **summary** and writes it as JSON:
  - per step: status, wall time, CPU time, peak memory, cache hit or miss;
  - the first errors, deduplicated, each with file, line, code and message (at most 20);
  - failing tests with their assertion or panic message (at most 20), and counts of passed,
    failed and skipped;
  - warnings counted by kind;
  - the log's path and the byte ranges of each error, for reading more.

  An agent reads the summary, not the log. The summary is bounded (about 4 KiB).
- **BUILD-5 — Detached.** `--detach` returns a build id at once; `hgl-ai build status ID`
  and `hgl-ai build wait ID` report progress and the summary. A long build costs an agent no
  tokens while it runs.
- **BUILD-6 — Elsewhere.** `--platform macos` (or `windows`) sends the build to a host of
  that platform through the bus (a `build` job); the builder service there runs it and
  returns the summary as a `receipt`. The agent's own host need not have that platform.
- **BUILD-7 — Metrics.** Every run's summary is kept on the hub (`HGL_BUILD/run.<id>`, with
  a retention limit) so the console can chart build time, failure rate and peak memory per
  repository, step and platform.

## 3. The merge queue

- **BUILD-8 — Queue.** An approved pull request (`approved` at its exact head) joins the
  repository's merge queue (`HGL_STATE/queue.<repo>`), in approval order.
- **BUILD-9 — Sessions.** The merge-queue service takes the next batch of queued pull
  requests that merge cleanly together (or one, for a change marked to land alone), merges
  them onto the current `main` in an integration branch, and sends that one head to the
  builder of every platform the recipe names (Linux, macOS, Windows). When all platforms
  pass, it lands each pull request in order (`gh` merge at its approved head, `landed.log`,
  `main`, pins, `landed` notes) and takes the next batch. Platform results are recorded as
  the gate verdicts `hgl-ai gh land` reads.
- **BUILD-10 — Failure.** When a batch fails, the service bisects it (each half on the
  failing platform) to the pull request that breaks it, sends that author a `sendback` with
  the summary's errors, and goes on with the rest. A failure that no pull request explains
  (a toolchain, a flaky test, a full disk) goes to the build agent and holds the queue.
- **BUILD-11 — Releases.** A release is a merge-queue session for a version change: the same
  three-platform build, then the release workflow and the owner's tag (release tags stay
  with the owner).

## 4. What this replaces

The landing scripts, the desk's regression sets (now recipe steps), the three platform gate
scripts and the hand-run three-machine landing. The controller still judges and approves;
the merge queue does the merging.
