# Install

Status: proposed; implemented.

## Hub (one host)

```sh
brew install hhenson/hgraph/hgl-ai
hgl-ai server install --listen <tailnet-address>
```

`server install`:

1. Uses `nats-server` from `PATH`, or installs it (`brew install
   nats-server` where Homebrew exists, else the pinned release, checksum
   verified, into `<data_dir>/bin`).
2. Writes a token (32 random bytes, hex) to `hub.token_file`
   (`~/.config/hgl-ai/hub.token`) with mode 0600, keeping an existing one
   unless `--rotate-token`, and `<data_dir>/nats.conf` (mode 0600: listen
   address, JetStream store directory, token authorization).
3. Writes the `[server]` section, installs and starts the hub user service,
   waits for the port, provisions the stream and buckets (as `hgl-ai init`),
   then installs the coordinator service (`--no-coord` skips it).
4. Prints the client command to run on other hosts. The token file is copied
   to them by hand or a secret manager; the token is never printed.

`server start|stop|status` manage the services; `server uninstall` removes
them and `nats.conf`, keeping the JetStream store unless `--purge`.

## Clients (every host, the hub too)

```sh
brew install hhenson/hgraph/hgl-ai
hgl-ai client install --hub nats://<hub>:4222 --token-file ~/hub.token --host build-1
hgl-ai seat add w1 --role implementor --runtime codex --tier frontier --effort high
hgl-ai seat add r1 --role reviewer --runtime claude --tier frontier --effort xhigh
hgl-ai doctor
```

`client install`:

1. Writes or merges `~/.config/hgl-ai/config.toml` and copies the token
   beside it with mode 0600.
2. Checks the hub connection. When it fails, the files stay and the command
   exits 10.
3. Finds the installed agent CLIs (`claude`, `codex`, `agy`) and installs the
   skill into each.
4. Installs and starts the bridge user service, with the `PATH` of the install.

`client uninstall` removes the service and the skills, keeping the
configuration unless `--purge`. Both installers take `--dry-run`, and
`--linger` to run `loginctl enable-linger` on Linux so services survive logout.

## Agents

The bridge starts headless turns by itself when notes arrive. An interactive
session for a seat, for example the controller:

```sh
hgl-ai agent start claude      # tmux session hgl-claude
hgl-ai agent attach claude
hgl-ai agent stop claude
```

The session starts the seat's runtime with its model and effort in
`<work_root>/<seat>/_interactive` (`--cwd` overrides), with `HGL_AI_SEAT`,
`HGL_AI_ROLE`, `HGL_AI_TASK_DIR` and `TMPDIR` set.

Each CLI must be signed in once by its user (`claude`, `codex login`, `agy`).
`doctor` reports a CLI that is installed but not signed in.
