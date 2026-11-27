# Builds

Status: proposed. §1, §2 and §2a are implemented (`crates/build`); §3, the merge queue,
is not: today the controller lands with `hgl-ai gh land` (`docs/protocol.md` §8). The
current rules are in `docs/protocol.md`, `docs/roles.md` and `docs/design.md`.

Building and testing is deterministic work: a tool runs it, an agent only writes and
repairs the instructions and reads the summary.

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
repo = "hgl-compiler"
env = { CARGO_TERM_COLOR = "never" }     # every step

[[step]]
name = "test"
run = "cargo test --workspace --locked --message-format=json"
platforms = ["linux", "macos", "windows"] # default: all
cwd = "."                                 # in the checkout
memcap_gb = 32                            # default 16; no cap on a Windows remote
time = "40m"                              # default 60m
parser = "cargo"                          # default lines
needs = ["sdk:hgraph", "tool:cmake", "step:build"]
artifacts = ["$HGL_AI_BUILD_DIR/report.xml"]
```

- A step's `run` may be a table per platform: `run.default = "..."`, `run.windows = "..."`.
- `needs`: `sdk:NAME` is met by `[builder.sdk] NAME = PREFIX` on the builder's host (steps
  see `HGL_SDK_NAME`); `tool:NAME` by a command on `PATH`; `step:NAME` by that earlier step
  passing. A missing SDK or tool is *not run*, not a failure.
- Steps see `HGL_AI_BUILD_ID`, `HGL_AI_SRC`, `HGL_AI_BUILD_DIR` (CMake trees go there) and,
  in a builder's checkout, `CARGO_TARGET_DIR` and `TMPDIR` inside the job directory. `env`
  values expand `$VAR` from these.
- `artifacts` are copied to `<log dir>/artifacts/<step>/`; `junit` and `catch2` read the
  `.xml` ones (else the log). `lines` takes `patterns = { error = [...], warning = [...] }`
  with named groups `file`, `line`, `col`, `code`, `message`.
- Stored as written (comments kept) in `HGL_BUILD/recipe.<repo>`, history 64:
  `recipe show [--rev N]`, `recipe history`. `recipes/` in this repository holds the
  first ones.

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

  An agent reads the summary, not the log. The summary is bounded (at most 4 KiB of text;
  `summary.json` keeps 200 errors and failures). Steps run in recipe order; the first that
  does not pass skips the rest unless `--keep-going`. Logs go to
  `$HGL_AI_TASK_DIR/build-logs/<id>/<step>.log` (stdout and stderr together), with
  `status.json` and `summary.json`; `hgl-ai build log ID STEP --bytes A-B` prints a range.
  Exit status 0 pass, 1 fail, 3 not run. Build ids are `YYYYMMDD-HHMMSS-xxxxxx` (UTC).
- **BUILD-5 — Detached.** `--detach` returns a build id at once; `hgl-ai build status ID`
  and `hgl-ai build wait ID` report progress and the summary. A long build costs an agent no
  tokens while it runs.
- **BUILD-6 — Elsewhere.** `--platform macos` (or `windows`) sends the build to a host of
  that platform through the bus (a `build` job); the builder service there runs it and
  returns the summary as a `receipt`. The agent's own host need not have that platform.
  - Builders advertise in `HGL_HOSTS/builder.<label>` (platform, reachable, active,
    draining or retired, busy of slots, free disk, tool versions). The sender picks a ready
    one (`--builder LABEL` to choose); an unreachable one holds the job until it is back.
  - The job is a `build` note to `builder-<label>` at the exact head (optionally `--base`
    to merge onto). The builder checks it out in `<work_root>/builder/jobs/<id>/` from its
    clone in `clones/<repo>` (submodules too), runs the recipe, and removes the checkout
    (cleanup is part of done). One build at a time unless `[builder] concurrency = N`.
  - The receipt (the summary JSON) goes to `r-<id>`, which the sender reads from the
    stream, and to `--notify SEAT`. The step logs stay on the builder's host.
  - `hgl-ai builder drain|retire|resume LABEL` sets `HGL_STATE/builder.<label>`;
    `mode = drain` drains every builder. A shutdown cancels running builds and returns
    their jobs to the inbox.
- **BUILD-7 — Metrics.** Every run's summary is kept on the hub (`HGL_BUILD/run.<id>`, with
  a retention limit) so the console can chart build time, failure rate and peak memory per
  repository, step and platform. The hub keeps the first 20 errors and failures of each
  run, rewritten as steps finish, and the newest 1000 runs; `hgl-ai build list [--repo R]
  [--failed]`.

## 2a. Proxy builders

- **BUILD-12 — A remote over ssh.** A host without hgl-ai (Windows) is built by a proxy
  builder on a Linux or macOS host, configured there only:

  ```toml
  [[builder.remote]]
  platform = "windows"
  ssh = "winbox"              # an alias from the user's ssh configuration
  work_dir = "C:/hgl-ai/work"
  shell = "powershell"        # or sh
  label = "win1"              # default <host>-<platform>
  ```

  It is a builder like any other (`builder-win1`, its own record with `proxy`). The proxy
  owns one OpenSSH master connection per remote (control socket in
  `<data_dir>/builder/ssh/`, mode 0700), checks it on its heartbeat and reconnects with
  backoff. A job in flight when it drops is *remote unreachable*, not a failure, and is
  retried once on reconnect; jobs wait while it is down.
- **BUILD-13 — How it runs.** Git checks out the head in `<work_dir>/jobs/<id>/` on the
  remote; each step runs as `powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass
  -EncodedCommand <step>` (no machine-wide policy change), its output streamed back into
  the proxy's log and parsed there. The proxy enforces the time limit: on expiry or cancel
  it kills the remote tree (`taskkill /T /F`), then the session. No memory cap on Windows.
  The job directory is always removed; a shutdown kills the running tree, removes job
  directories and closes the connection.
- **BUILD-14 — Debugging.** `hgl-ai build shell --platform P [REPO] [--head SHA]` opens an
  interactive shell in a fresh checkout on this host's builder of P (over ssh for a
  remote), removed when the shell ends.

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
