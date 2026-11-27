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
| `proto` | Note, kinds, receipts, host, tree and lock records, name and key validation, message ids. No I/O. | — |
| `config` | `config.toml` schema, defaults, path expansion, load and save. | `proto` |
| `bus` | NATS connection, provisioning, send, inbox fetch, ack/nak/term/in-progress, KV, leases. | `proto`, `config` |
| `cap` | Memory and time caps for a command tree (`systemd-run --user --scope` on Linux, `setrlimit` and a resident-memory watchdog elsewhere; macOS does not enforce `RLIMIT_AS`, so there the watchdog alone limits memory). | — |
| `tree` | Worktree layout, create, register, salvage, remove, stale listing. | `proto`, `config`, `bus` |
| `bridge` | Seat loops, runtime adapters, prompt building, heartbeats, failover, cleanup on `landed`. | all above |
| `install` | Hub and client installers, user services, `nats-server` provisioning, skill installation, agent launch. | `config`, `bus` |
| `gh` | Coordinator service, merge gate, landing, pin sync, release watch, tap release; `backlog` and `HGL_METRICS`. Landing and release watch hold the hub lease `HGL_LOCKS/lock.land.<repo>`, so they exclude each other across hosts. | `proto`, `config`, `bus` |
| `owner` | Owner channel: thread records, events, the console's fold, status reports, `decisions.log`. | `proto`, `bus` |
| `notify` | Owner notifications: ntfy, Slack webhook, SMTP; first channel that accepts. | `config` |
| `console` | The owner's web console: pages, telemetry, passkey and OAuth sign-in, the fold, ticks. | `proto`, `config`, `bus`, `owner`, `gh`, `notify` |
| `hgl-ai` | The `clap` command line; thin over the crates. | all |

Async runtime `tokio`; NATS client `async-nats`; errors `anyhow` at the edge,
`thiserror` in libraries; logs `tracing` to stderr. Web: `axum`, `axum-server`
(TLS), `webauthn-rs` (passkeys), `reqwest` (GitHub, Apple and Google sign-in,
notifications), `ring` (`id_token` signatures), `lettre` (SMTP).
TLS is `rustls` with `ring` only. Rust 1.89 or later.

## 2. Command line

```
hgl-ai server install [--listen ADDR] [--port 4222] [--store DIR] [--users] [--no-coord]
                [--no-console] [--console-url URL] [--console-user EMAIL=HANDLE ...]
                [--console-listen ADDR] [--server-user NAME] [--server-home DIR]
                [--print-root-steps] [--same-user]
hgl-ai server start|stop|status|uninstall
hgl-ai server user add NAME --role seat|coord|console|admin [--nkey PUBKEY] [--rotate]
hgl-ai server user list|remove NAME
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
hgl-ai note wait SEAT [--timeout DUR]     block, print, ack; holds the seat lease
hgl-ai note peek SEAT                     print without ack; holds the seat lease
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

hgl-ai console run                        the owner's console (its user service runs this)
hgl-ai ask --title T --priority P [--issue R#N] [--context-file F] --question Q [--option L:LINE ...] [--suggest L]
hgl-ai thread list|show|reply|withdraw
hgl-ai status post|list
hgl-ai decision add|tail
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
# token_file = "~/.config/hgl-ai/github.token"   # the coordinator's own read-only token
repos = ["hgl-compiler", "hgl_spec", "hgl_core", "hgl-project", "homebrew-hgraph"]
branch_prefix = "codex/"
wrapper = "hgl-project"
clone_url = "https://github.com/{owner}/{repo}.git"   # default
controller = "claude"                  # default; the seat that gets the bridge's and coordinator's notes

[models.claude]
frontier = "claude-opus-5-5"
balanced = "claude-sonnet-5-5"
fast = "claude-haiku-5-5"

[models.gpt]
frontier = "gpt-6.2-astra"
balanced = "gpt-6.2-sol"
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
auth = "users"                         # a hub user per client; "token": older single token
units = "system"                       # the server user's: system units; default "user"

[console]                              # hub host only (docs/owner.md)
listen = "127.0.0.1:8443"              # loopback or tailnet address, never 0.0.0.0
url = "https://hub.example.ts.net"     # the stable name passkeys are bound to
# tls_cert = "~/.config/hgl-ai/console/cert.pem"   # omit both behind a TLS proxy
# tls_key = "~/.config/hgl-ai/console/key.pem"
users = [{ email = "owner@example.com", handle = "This Dude", github = "<login>" }]
status_every = "2h"
session_ttl = "7d"

[console.github]
client_id = "Iv1.0123456789abcdef"
secret_file = "~/.config/hgl-ai/console/github.secret"   # mode 0600

[console.apple]
client_id = "com.example.hgl-console"  # Services ID
team_id = "ABCDE12345"
key_id = "KEY1234567"
key_file = "~/.config/hgl-ai/console/apple.p8"           # mode 0600

[console.google]
client_id = "0123-example.apps.googleusercontent.com"
secret_file = "~/.config/hgl-ai/console/google.secret"   # mode 0600

[[console.notify]]                     # tried in order
kind = "ntfy"
url = "https://ntfy.sh/<topic>"
# token_file = "~/.config/hgl-ai/console/ntfy.token"

[[console.notify]]
kind = "slack"
webhook_file = "~/.config/hgl-ai/console/slack.url"

[[console.notify]]
kind = "email"
smtp = "smtp.example.com:587"
tls = "starttls"                       # or "tls", or "none" (loopback only)
from = "console@example.com"
# to = ["owner@example.com"]         # default: every user's email
# user = "console@example.com"
# password_file = "~/.config/hgl-ai/console/smtp.password"
```

`hub.token_file` holds the host's hub credential: its own hub user's NKey seed, or
the shared token of an older hub. `HGL_AI_HUB_CREDENTIAL` overrides it for one
process; the coordinator and console services set it to their own users.

A model's family comes from the `[models.<family>]` table that names it; an
`agy` seat with a Claude slug is family `claude`.

## 4. Runtime adapters

| Runtime | Turn | Resume | Model and effort |
| --- | --- | --- | --- |
| `claude` | `claude -p --output-format stream-json --verbose` | `--resume ID` from the last turn's `session_id` | `--model`, `--effort` |
| `codex` | `codex exec --json -` | `codex exec resume --json ID -` | `-m`, `-c model_reasoning_effort=E` |
| `agy` | `agy --input-format stream-json --output-format stream-json` | `--conversation ID` | `--model SLUG-EFFORT`, `--effort` |

- The prompt goes on stdin, never in an argument (Linux caps one at 128 KiB; a
  note may be 1 MiB). `claude -p` and `codex exec -` read it as text; `agy`
  reads one line `{"event":"user","message":{"role":"user","content":PROMPT}}`
  (its `-p` takes the prompt as a value and reads no stdin). Each turn's prompt
  is kept in the task directory's `prompts/`.
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
    task.json   seat, repo, issue, head, branch, created, session ids, seq of the note that made it
    turn.lock   flock held for a turn; the agent's stdin, holding the turn's input
    log/        the last turns' stdout and stderr
    prompts/    the last turns' prompts, <UTC>-<runtime>.md beside their logs
  clones/<repo>/  one bare-ish clone per repository and host, shared by seats
  salvage/<seat>-<repo>-<issue>-<UTC>.patch
```

Reviewer tasks use `<seat>/<repo>-<pr>-review/` with a detached checkout of
`head` merged onto `main`.

## 6. Services

`hgl-ai server install` writes system services run as the server user (`hgl-ai`,
home `/var/lib/hgl-ai`; on macOS `_hgl-ai`, `/Library/hgl-ai`); `client install`
writes the owner's user service. `server install --same-user` writes user services
for all four.

| Service | Runs as | Linux | macOS |
| --- | --- | --- | --- |
| Hub | server user | `/etc/systemd/system/hgl-ai-hub.service`: `nats-server -c <data_dir>/nats.conf` | `/Library/LaunchDaemons/io.github.hhenson.hgl-ai.hub.plist` |
| Coordinator | server user | `hgl-ai-coord.service`: `hgl-ai gh coord run` | `io.github.hhenson.hgl-ai.coord` |
| Console | server user | `hgl-ai-console.service`: `hgl-ai console run` | `io.github.hhenson.hgl-ai.console` |
| Bridge | owner | `systemd --user` `hgl-ai-bridge.service`: `hgl-ai bridge run` | LaunchAgent `io.github.hhenson.hgl-ai.bridge` |

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
