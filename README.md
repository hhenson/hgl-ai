# hgl-ai

`hgl-ai` coordinates a mixed fleet of coding agents (Claude Code, Codex,
Antigravity) across hosts. GitHub stays the record of issues, pull requests,
reviews and merges; a NATS JetStream hub carries notes and shared state
between seats. One Rust binary installs the hub and the clients, runs the
bridge that starts headless agent turns when notes arrive and owns their
worktrees, and gives agents the commands they use through one installed skill.

What `hgl-ai` does now:

- [docs/protocol.md](docs/protocol.md): notes, transport, worktrees, receipts, shared state.
- [docs/roles.md](docs/roles.md): roles, gates, model assignment, resources.
- [docs/design.md](docs/design.md): crates, command line, configuration, services.
- [docs/install.md](docs/install.md): hub, clients, agents.
- [docs/work.md](docs/work.md): how a seat works an issue: labels, claiming, branches, pull requests, blocking.
- [docs/build.md](docs/build.md): build recipes, the build tool and its summaries, builders and proxy builders (the merge queue, §3, is proposed).

Proposed, not implemented:

- [docs/owner.md](docs/owner.md): the owner channel (status, questions, conversations) and the console with telemetry.
- [docs/fleet.md](docs/fleet.md): hosts register with the hub, which gives each its workers and roles.

## Install

```sh
brew install hhenson/hgraph/hgl-ai
```

From source (Rust 1.89 or later):

```sh
cargo install --path crates/hgl-ai
```

## Quick start

On the hub host:

```sh
hgl-ai server install --listen <tailnet-address>
```

It prints the `client install` command. For each other host, make its hub user
there (`hgl-ai server user add build-1 --role seat`) and copy the credential it
names to that host (mode 0600). Then on every host (on the hub, the token file
is `~/.config/hgl-ai/hub.token`, already written):

```sh
hgl-ai client install --hub nats://<hub>:4222 --token-file ~/build-1.cred --host build-1
hgl-ai seat add w1 --role implementor --runtime codex --tier frontier --effort high
hgl-ai doctor
```

An interactive seat, such as the controller (the default; `seat add --headless`
lets the bridge serve a controller instead, docs/owner.md OWN-7):

```sh
hgl-ai agent start claude
hgl-ai agent attach claude
```

Every install command takes `--dry-run`.

## Layout

| Crate | Owns |
| --- | --- |
| `crates/proto` | Wire types: notes, kinds, receipts, records, validation. |
| `crates/config` | `config.toml` schema, defaults, model resolution. |
| `crates/bus` | NATS connection, provisioning, notes, inboxes, KV. |
| `crates/cap` | Memory and time caps for a command tree. |
| `crates/tree` | Task worktrees: layout, create, salvage, remove. |
| `crates/bridge` | Seat loops, runtime adapters, heartbeats, failover. |
| `crates/install` | Hub and client installers, user services, skill, agent sessions, `doctor`. |
| `crates/gh` | Coordinator, merge gate, landing, pins, releases, backlog, metrics. |
| `crates/owner` | Owner channel: threads, events, status reports, decision log. |
| `crates/notify` | Owner notifications: ntfy, Slack, email. |
| `crates/console` | The owner's web console: sign-in, threads, telemetry. |
| `crates/build` | Build recipes, parsers, summaries, executors, the builder service. |
| `crates/hgl-ai` | The command line. |

`skill/SKILL.md` is the skill every agent CLI loads. `recipes/` holds the build recipes
(`hgl-ai build recipe import recipes/<repo>.toml`).
