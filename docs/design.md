# Design

Status: proposed. The contract between the crates of this repository.

## 1. One binary

`hgl-ai` is one Rust binary for Linux (x86_64, aarch64; static musl) and
macOS (arm64, x86_64). It replaces the Python package, the MCP server and the
shell scripts of the earlier file queue. It shells out to `git`, `gh`,
`nats-server`, `systemctl`/`launchctl`, `tmux` and the agent CLIs; it links
no GitHub or git library.

| Crate | Owns | Depends on |
| --- | --- | --- |
| `proto` | Note, kinds, receipts, host and tree records, name and key validation, message ids. No I/O. | — |
| `config` | `config.toml` schema, defaults, path expansion, load and save. | `proto` |
| `bus` | NATS connection, provisioning, send, inbox fetch, ack/nak/in-progress, KV. | `proto`, `config` |
| `cap` | Memory and time caps for a command tree (`systemd-run --user --scope` on Linux, `setrlimit` and a watchdog elsewhere). | — |
| `tree` | Worktree layout, create, register, salvage, remove, stale listing. | `proto`, `config`, `bus` |
| `bridge` | Seat loops, runtime adapters, prompt building, heartbeats, failover, cleanup on `landed`. | all above |
| `install` | Hub and client installers, user services, `nats-server` provisioning, skill installation, agent launch. | `config`, `bus` |
| `gh` | Coordinator service, merge gate, landing, pin sync, release watch, tap release. | `proto`, `config`, `bus` |
| `hgl-ai` | The `clap` command line; thin over the crates. | all |

Async runtime `tokio`; NATS client `async-nats`; errors `anyhow` at the edge,
`thiserror` in libraries; logs `tracing` to stderr.

## 2. Command line

```
hgl-ai server install [--listen ADDR] [--port 4222] [--store DIR]
hgl-ai server start|stop|status|uninstall
hgl-ai client install --hub URL --token-file FILE --host LABEL [--work-root DIR]
hgl-ai client uninstall
hgl-ai init                               provision stream and buckets (idempotent)
hgl-ai doctor                             check config, hub, CLIs, skills, services, disk

hgl-ai seat add NAME --role ROLE --runtime claude|codex|agy (--tier T [--family F] | --model SLUG)
                [--effort E] [--fallback RUNTIME:FAMILY:TIER[:EFFORT] ...]
hgl-ai seat list|remove NAME
hgl-ai agent start SEAT                   interactive session in tmux `hgl-SEAT`
hgl-ai agent attach SEAT

hgl-ai bridge run [--seat NAME ...]       the daemon (the user service runs this)

hgl-ai note send --to SEAT --kind KIND [--repo R] [--issue N] [--head SHA] (--body TEXT | --body-file F)
hgl-ai note wait SEAT [--timeout DUR]     block, print, ack
hgl-ai note peek SEAT                     print without ack
hgl-ai state get|put|del KEY [VALUE|-]
hgl-ai hosts

hgl-ai tree list [--stale] [--local] [--json]   HGL_TREES, or this host's work_root
hgl-ai tree remove SEAT REPO ISSUE [--review]  what the bridge does on `landed`, by hand

hgl-ai cap SIZE [--time DUR] -- CMD ...   memory and time cap for a command tree

hgl-ai gh coord run                       coordinator service loop
hgl-ai gh merge-if-clean REPO PR HEAD
hgl-ai gh land REPO PR HEAD
hgl-ai gh pins sync
hgl-ai gh release watch SHA
hgl-ai gh tap release VERSION
```

Exit status: 0 success, 1 failure, 2 usage, 3 not ready (a gate is not
satisfied; the message says which), 10 hub unreachable.

## 3. Configuration

One file per host: `$XDG_CONFIG_HOME/hgl-ai/config.toml` (default
`~/.config/hgl-ai/config.toml`, also on macOS). `~` is expanded.

```toml
host = "build-1"                       # label; never a network name

[hub]
url = "nats://127.0.0.1:4222"
token_file = "~/.config/hgl-ai/hub.token"   # mode 0600

[paths]
work_root = "~/hgl-ai/work"            # every task directory lives here
data_dir = "~/.local/share/hgl-ai"     # bridge state, server store, logs

[limits]
memcap_gb = 64                         # cap of one agent turn's command tree
min_free_gb = 200                      # stop taking `assign` below this
stale_after = "3d"
turn_timeout = "2h"

[github]
owner = "hhenson"
repos = ["hgl-compiler", "hgl_spec", "hgl_core", "hgl-project", "homebrew-hgraph"]
branch_prefix = "codex/"
wrapper = "hgl-project"
clone_url = "https://github.com/{owner}/{repo}.git"   # default

[models.claude]
frontier = "claude-opus-5-5"
balanced = "claude-sonnet-5-5"
fast = "claude-haiku-5-5"

[models.gpt]
frontier = "gpt-6.1-sol"
balanced = "gpt-6.1-sol"
fast = "gpt-6-luna"

[models.gemini]
frontier = "gemini-4-neon"
balanced = "gemini-4-hydrogen"
fast = "gemini-3.8-flash"

[[seat]]
name = "w1"
role = "implementor"                   # controller|triage|implementor|reviewer
runtime = "codex"                      # claude|codex|agy
tier = "frontier"                      # or `model = "<slug>"` to pin one
effort = "high"
fallback = [{ runtime = "agy", family = "claude", tier = "frontier" }]

[server]                               # hub host only
listen = "127.0.0.1"
port = 4222
store_dir = "~/.local/share/hgl-ai/nats"
nats_version = "2.15.3"
```

A model's family comes from the `[models.<family>]` table that names it; an
`agy` seat with a Claude slug is family `claude`.

## 4. Runtime adapters

| Runtime | Turn | Resume | Model and effort |
| --- | --- | --- | --- |
| `claude` | `claude -p PROMPT --output-format stream-json --verbose` | `--resume ID` from the last turn's `session_id` | `--model`, `--effort` |
| `codex` | `codex exec --json PROMPT` | `codex exec resume ID --json PROMPT` | `-m`, `-c model_reasoning_effort=E` |
| `agy` | `agy -p PROMPT --output-format stream-json` | `--conversation ID` | `--model SLUG-EFFORT`, `--effort` |

- A seat keeps one session per task, so context carries across that task's
  turns and is dropped when the task ends. Reviewers always start fresh.
- The bridge runs each turn under `cap` with the seat's limit, in the task's
  worktree, with permissions bypassed only inside that worktree's sandbox as
  the runtime allows. A turn that exceeds `turn_timeout` is stopped and naked.
- A quota refusal is recognised from the runtime's structured output and
  exit status; the patterns live in the adapter, with tests.

## 5. Worktree layout

```
<work_root>/
  <seat>/<repo>-<issue>/
    src/        git worktree of the seat's clone, branch <branch_prefix><issue>-<slug>
    build/      CARGO_TARGET_DIR, CMake build trees
    tmp/        TMPDIR
    task.json   seat, repo, issue, head, branch, created, session ids
    turn.lock   flock held for a turn; empty, it is the agent's stdin
    log/        the last turns' stdout and stderr
  clones/<repo>/  one bare-ish clone per repository and host, shared by seats
  salvage/<seat>-<repo>-<issue>-<UTC>.patch
```

Reviewer tasks use `<seat>/<repo>-<pr>-review/` with a detached checkout of
`head` merged onto `main`.

## 6. Services

`hgl-ai server install` and `client install` write user services:

| Service | Linux (`systemd --user`) | macOS (`launchd`) |
| --- | --- | --- |
| Hub | `hgl-ai-hub.service` running `nats-server -c <data_dir>/nats.conf` | `io.github.hhenson.hgl-ai.hub` |
| Bridge | `hgl-ai-bridge.service` running `hgl-ai bridge run` | `io.github.hhenson.hgl-ai.bridge` |
| Coordinator | `hgl-ai-coord.service` running `hgl-ai gh coord run` | `io.github.hhenson.hgl-ai.coord` |

Linux services enable lingering (`loginctl enable-linger`) when the user
allows it, so they survive logout.

## 7. Skill

`skill/SKILL.md` teaches an agent its role and the `hgl-ai` commands it may
use. `client install` copies it to each installed CLI:

| CLI | Path |
| --- | --- |
| Claude Code | `~/.claude/skills/hgl-ai/SKILL.md` |
| Codex | `~/.codex/skills/hgl-ai/SKILL.md` |
| Antigravity | `~/.gemini/config/skills/hgl-ai/SKILL.md` |

## 8. Release

A tag `vX.Y.Z` builds the four targets in GitHub Actions, attaches
`hgl-ai-X.Y.Z-<target>.tar.gz` and its `.sha256` to the release, and opens a
pull request on `hhenson/homebrew-hgraph` updating `Formula/hgl-ai.rb`.
