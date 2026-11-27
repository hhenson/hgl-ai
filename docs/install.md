# Install

Status: proposed.

## Hub (one host)

```sh
brew install hhenson/hgraph/hgl-ai
hgl-ai server install --listen <tailnet-address>
```

`server install`:

1. Uses `nats-server` from `PATH`, or installs it (`brew install
   nats-server` where Homebrew exists, else the pinned release, checksum
   verified, into `<data_dir>/bin`).
2. Writes `<data_dir>/nats.conf` (JetStream on, store directory, listen
   address, token authorization) and a new token to
   `~/.config/hgl-ai/hub.token` with mode 0600.
3. Installs and starts the hub user service, then provisions the stream and
   buckets (`hgl-ai init`).
4. Prints the client command to run on other hosts. The token is copied to
   them by hand or a secret manager; it is never printed in full.

## Clients (every host, the hub too)

```sh
brew install hhenson/hgraph/hgl-ai
hgl-ai client install --hub nats://<hub>:4222 --token-file ~/hub.token --host build-1
hgl-ai seat add w1 --role implementor --runtime codex --tier frontier --effort high
hgl-ai seat add r1 --role reviewer --runtime claude --tier frontier --effort xhigh
hgl-ai doctor
```

`client install`:

1. Writes `~/.config/hgl-ai/config.toml` and moves the token beside it.
2. Checks the hub connection.
3. Finds the installed agent CLIs (`claude`, `codex`, `agy`) and installs the
   skill into each.
4. Installs and starts the bridge user service.

## Agents

The bridge starts headless turns by itself when notes arrive. An interactive
session for a seat, for example the controller:

```sh
hgl-ai agent start claude      # tmux session hgl-claude
hgl-ai agent attach claude
```

Each CLI must be signed in once by its user (`claude`, `codex login`, `agy`).
`doctor` reports a CLI that is installed but not signed in.
