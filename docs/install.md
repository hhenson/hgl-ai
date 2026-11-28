# Install

Status: proposed; implemented.

## Hub (one host)

```sh
brew install hhenson/hgraph/hgl-ai
hgl-ai server install --listen <tailnet-address>     # as yourself; root steps via sudo
```

The server side (hub, coordinator, console) runs as its own OS user, `hgl-ai`
(`_hgl-ai` on macOS; `--server-user`), from system units (`User=`) or launch daemons
(`UserName`), with its configuration, credentials and data in that user's home,
`/var/lib/hgl-ai` (`/Library/hgl-ai`; `--server-home`), mode 0700. The client side
(bridge, agent CLIs, their sign-ins) stays yours, or another OS user's
(`--client-user NAME`, kept for a reinstall). Each step that needs root is shown and
runs through `sudo` only when you confirm it, on your terminal (`sudo` may ask for
your password); `--print-root-steps` prints them as a script instead and runs none.
The script is quoted for `sh` and names absolute paths (no `~`):

| Step (root) | Linux | macOS |
| --- | --- | --- |
| the server user | `useradd --system --user-group --home-dir /var/lib/hgl-ai --shell /usr/sbin/nologin hgl-ai` | `sysadminctl -addUser _hgl-ai -roleAccount -home /Library/hgl-ai -shell /usr/bin/false` |
| its home | `install -d -o hgl-ai -m 0700 /var/lib/hgl-ai`, `chown hgl-ai:hgl-ai` | `install -d -o _hgl-ai -m 0700 /Library/hgl-ai` |
| `hgl-ai` it can run, when yours is under your home | `install -m 0755 <yours> /usr/local/bin/hgl-ai` | same |
| its files, as the server user | `sudo -u hgl-ai env HOME=... hgl-ai server install --same-user --no-services --github-token-file /var/lib/hgl-ai/.config/hgl-ai/github.token --nats-server <chosen> ...` | same |
| their logs | none: the journal | `install -d -o root -g wheel -m 0755 /Library/Logs/hgl-ai` |
| units | `install -m 0644 ... /etc/systemd/system/hgl-ai-{hub,coord,console}.service`, `systemctl daemon-reload`, `enable`, `restart` | `/Library/LaunchDaemons/io.github.hhenson.hgl-ai.*.plist`, `launchctl bootout system/...` when loaded, `bootstrap system`, `kickstart` |
| provisioning, as the server user's admin hub user | `sudo -u hgl-ai env ... HGL_AI_HUB_CREDENTIAL=.../<hub>-admin.cred hgl-ai init` | same |
| the coordinator's GitHub token (not `--no-coord`), before it starts | `hgl-ai` (the script: its shell) reads it from your terminal without echo and pipes it to `sudo -u hgl-ai sh -c '...' sh <its home>/.config/hgl-ai/github.token`, which writes it mode 0600; nothing under `sudo` reads the terminal. Enter keeps the current one and runs nothing; `--keep-token` does not ask | same |
| this host's client credential, for you (no root) | `sudo -u hgl-ai cat /var/lib/hgl-ai/.config/hgl-ai/hub.token \| sh -c '...' sh <your config dir>`: written by you, mode 0600 | same |
| or, with `--client-user NAME`, for it (no root) | `sudo -u hgl-ai cat ... \| sudo -u NAME sh -c '...' sh <its home>/.config/hgl-ai`: new directories 0700, `hub.token` 0600; nothing for you | same |

Root never reads a file the server user controls nor writes where a less-privileged
user can plant a symlink: the server user reads the credential and its owner writes
it, as themselves, in a temporary file renamed over `hub.token`. An empty read keeps
the old one. The server home's parent must be root's (the defaults are).

The same holds for the services' output, which systemd and launchd open as root:
system units log to the journal (`journalctl -u hgl-ai-hub.service`; as a member of
`systemd-journal` or `adm`, else with `sudo`), launch daemons to
`/Library/Logs/hgl-ai/<service>.log` in a directory only root writes (`sudo tail`).
Neither is in the server user's home. `server status` names them.

`nats-server` is chosen once, by the install: the first on the server user's `PATH`
(yours without your home), else the pinned release in its `<data_dir>/bin`. The unit
and the server user's step (`--nats-server`) use the same one.

`--same-user` installs the server side as yourself with user services (development
and tests; `doctor` warns that agents can then read the console's credential).

The server user's step of `server install`:

1. Uses `nats-server` from `PATH`, or installs it (`brew install
   nats-server` where Homebrew exists and the install is `--same-user`, else the
   pinned release, checksum verified, into `<data_dir>/bin`). In a system install it
   uses the one chosen for the unit, downloading only the pinned release.
2. Makes the hub users (below): `<host>-admin`, `<host>` (this host's client,
   in `hub.token_file`), `<host>-coord`, `console` and `viewer`, keeping existing
   keys, and writes `<data_dir>/nats.conf` (mode 0600: listen address, JetStream
   store, one NKey user per client with its role's permissions; with `[console]`,
   the WebSocket listener for the console's page, below).
3. Writes the `[server]` section, installs and starts the hub user service (its log
   in `<data_dir>/logs`; a system install has none there),
   waits for the port, provisions the streams and buckets as the admin user (as
   `hgl-ai init`), then installs the coordinator service (`--no-coord` skips it)
   and the console service (`--no-console` skips it; it needs `[console]`).
4. Prints the client commands. A credential is copied to its host by hand or a
   secret manager; no secret is ever printed.

`server start|stop|status` manage the services (`start`, `stop` as root steps);
`server uninstall` removes them, keeping the server user's home and JetStream store
unless `--purge`, which also removes the user.

The coordinator reads GitHub with its own token, not yours: make a fine-grained,
read-only token (Metadata, Contents, Issues, Pull requests, Checks, Actions, Commit
statuses: read) for the repositories. The token step above stores it as the server
user (`[github] token_file`); run that step again to replace it.

During the intake cutover, deploy and verify the coordinator's triage delivery
before disabling the interim `hgl-intakewatch` timer.

The server user's own commands run with `sudo -u hgl-ai env HOME=/var/lib/hgl-ai
hgl-ai ...` (`server user add`, `doctor`).

### Slack Q&A

Use the existing Slack app with Socket Mode, `chat:write`, `groups:history`,
`app_mentions:read`, and the `message.groups` and `app_mention` bot events.
Enable `settings.interactivity.is_enabled` in the
app manifest for the answer and Close buttons. Socket Mode delivers their callbacks
over the outbound connection. Install it in the workspace and invite it to the
private `#hgl-qa` channel. In the server user's configuration add the workspace,
channel and owner member IDs, and use the owner's console handle:

```toml
[slack]
team = "T12345678"
channel = "C12345678"
user = "U12345678"
handle = "Owner Handle"
```

In the app settings, generate an app-level token with `connections:write` under
**Basic Information** and copy the installed bot token from **OAuth &
Permissions**. Run `hgl-ai server install --slack --keep-token --print-root-steps`
and review the printed script before running it. Paste the tokens at its
no-echo prompts. The script writes `slack-app.token` and `slack-bot.token` under
the server user's `~/.config/hgl-ai/`, both mode 0600. Enter keeps a token file
already in place. With `--keep-token`, only missing Slack tokens are prompted for.
A plain server reinstall retains the Slack service once enabled.

The script provisions the Slack hub user and starts `hgl-ai-slack` as the server
user alongside the console. The adapter opens outbound HTTPS and WebSocket
connections. `server status` shows its service state and log location.

### Upgrading an install from before hgl-ai#63

Its units append to `<data_dir>/logs/*.log` in the server user's home, opened by
root; `doctor` (`service <name> log`) and `server status` report it. Rerun
`hgl-ai server install` as yourself (the earlier address, host and console are
kept). Its root steps reinstall the units, then `systemctl daemon-reload` and
`restart` on Linux. On macOS it first makes `/Library/Logs/hgl-ai`, then for each
loaded daemon runs `launchctl bootout`, `bootstrap` and `kickstart`. The old logs stay
in `<data_dir>/logs`; the server user may delete them (`sudo -u hgl-ai rm -r
/var/lib/hgl-ai/.local/share/hgl-ai/logs`).

## Hub users

Every client has its own NKey user (OWN-5). `<data_dir>/hub-users.toml` lists
names, roles and public keys; a credential (`user`, `role`, `seed`, mode 0600) is
written once to `~/.config/hgl-ai/hub-users/<name>.cred` (the server user's home in
a system install: run `server user` there as `sudo -u hgl-ai env HOME=/var/lib/hgl-ai
hgl-ai server user ...`).

| Role | May publish |
| --- | --- |
| `seat` | `hgl.notes.<seat>`, `hgl.console.>`, `HGL_STATE`, `HGL_HOSTS`, `HGL_TREES`, `HGL_LOCKS`, `HGL_ENDED`, `HGL_BUILD/run.*`, `HGL_USAGE`, `hgl.usage.>`; never `hgl.owner.>`, `hgl.notes.owner.>`, `HGL_THREADS`, recipes (`HGL_BUILD/recipe.*`), verdicts (`HGL_STATE/verdict.*`), stream changes |
| `coord` | a seat's, and `HGL_METRICS` |
| `builder` | a seat's, and the local build verdicts `HGL_STATE/verdict.*` (BUILD-15) |
| `console` | `hgl.owner.thread.<id>`, `hgl.notes.owner.<seat>`, `tick` notes, `HGL_THREADS`, `decisions.log` |
| `slack` | `hgl.owner.thread.<id>.slack.<team>.<channel>.<source>` and `HGL_SLACK` |
| `viewer` | nothing by subject (replies are not enforced until hgl-ai#141): reads and watches `HGL_HOSTS`, `HGL_TREES`, `HGL_METRICS`, `HGL_BUILD`, `HGL_USAGE`, and of `HGL_STATE` only `prs`, `backlog`, `landed.log`, `tasks`, `mode`, `hold` (the console's page, docs/owner.md §3); consumers only in the filtered create form; subscribes to `_INBOX_VIEWER.>` only; no other stream or bucket, no consumer delete (docs/design.md §9) |
| `admin` | everything (`hgl-ai init`, `server install`, `build recipe import`) |

The console keeps session state in `HGL_CONSOLE`. The Slack adapter reads its own
delivery state, its durable `HGL_OWNER` feed and opening events by sequence.
The console can read the Slack permalink index.
Seat, coordinator and builder users read `HGL_OWNER` through short-lived consumers.
The `viewer` reads only the telemetry, as above.

Each user's replies go to its own inbox, `_INBOX.<public key>`, and that is all it may
subscribe to: no user reads another's JetStream or KV replies (the console's sessions,
threads, notes); the `viewer`'s is `_INBOX_VIEWER`. A client before this used the shared
`_INBOX.>`, which the hub then refuses. Upgrade the clients first (their inbox works on an
older hub too), then rerun `hgl-ai server install`; until then `doctor` on the hub warns
`hub inboxes`. Upgrading a client includes restarting its running bridge and builder
services (rerun `hgl-ai client install`, which restarts them): a running process keeps
the shared inbox until it restarts.

```sh
hgl-ai server user add build-1 --role seat         # writes hub-users/build-1.cred
hgl-ai server user add build-2 --role seat --nkey U...   # a host's own key; no secret here
hgl-ai server user add lab1-builder --role builder # its builder: [builder] credential
hgl-ai server user list
hgl-ai server user remove build-1                  # the hub refuses it from then on
```

`user add` and `remove` rewrite `nats.conf` and reload the hub (SIGHUP through
its pid file; connections that stay valid are kept). On the client: `hgl-ai client install --token-file <the .cred>`. A
credential file may also be a bare NKey seed (`SU...`). `hgl-ai init` needs the
admin user: `HGL_AI_HUB_CREDENTIAL=~/.config/hgl-ai/hub-users/<hub>-admin.cred hgl-ai init`.

### From a single token

A hub installed before hub users keeps its shared token (`[server] auth =
"token"`); `doctor` warns that any client can then publish as the owner. To move:

1. On the hub, `hgl-ai server user add <label> --role seat` for every other host;
   copy each credential to its host.
2. `hgl-ai server install --users`: users replace the token, the hub restarts,
   this host's `hub.token` becomes its seat credential and its bridge restarts.
   Other clients are refused until step 3.
3. On each other host, `hgl-ai client install --hub ... --token-file <credential> --host <label>`.
4. `hgl-ai doctor` everywhere: `hub auth` is `ok`.

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
4. Installs and starts the bridge user service, with the `PATH` of the install, and
   the builder service (`hgl-ai builder run`; `--no-builder` skips it), which builds
   this host's platform for the fleet (`docs/build.md`). SDK prefixes for recipes go in
   `[builder.sdk]`.

For a builder running as a separate OS user, pass `--builder-user NAME` to
`client install`. It records the user and leaves installation of that user's
builder service to the administrator. Configure that user's own `config.toml`
with `seat = []` and a `[builder]` section, and put its builder-role credential
in `[builder] credential`, readable only by that user. Start the builder service
under that account. Doctor checks the system service or the named user's service
when its service manager is accessible. Remove any old client-user builder
service when moving it to the separate account.

`client uninstall` removes the services and the skills, keeping the
configuration unless `--purge`. Both installers take `--dry-run`, and
`--linger` to run `loginctl enable-linger` on Linux so services survive logout.

## Console (hub host)

The console (docs/owner.md) serves the owner over HTTPS at a stable name, as its
own hub user `console`.

1. **Name and TLS.** Pick the name, for example the hub's tailnet name
   `hub.example.ts.net`. Either a local TLS proxy in front of plain HTTP on loopback:

   ```sh
   hgl-ai server install --console-url https://hub.example.ts.net \
       --console-user "owner@example.com=This Dude"
   tailscale serve --bg --https=443 http://127.0.0.1:8443
   ```

   or the console's own certificate (`tailscale cert hub.example.ts.net`, or any CA),
   with `listen` on the tailnet address and `tls_cert`, `tls_key` in `[console]`.
   Never listen on `0.0.0.0` or a public interface.

   The telemetry page reads the hub through its WebSocket listener, which `server
   install` turns on with the console (hub users only): `[server] websocket_port`
   (4223), on loopback, plain, taking only pages of the console's URL
   (`allowed_origins`). The proxy serves it on the console's origin, so the page
   stays same-origin; `server install` prints the step and does not run it:

   ```sh
   tailscale serve --bg --https=443 --set-path /nats http://127.0.0.1:4223
   ```

   The page reads as hub user `viewer`, whose credential `server install` writes and
   names in `[console] viewer_credential`. A system install (`--server-user`) prints
   the same step, with `--print-root-steps` as a comment.
2. **Users.** `[console] users`, one entry per person, keyed by email:

   ```toml
   users = [
     { email = "owner@example.com", handle = "This Dude", github = "<login>",
       apple = "<relay>@privaterelay.appleid.com" },
     { email = "second@example.com", handle = "Two" },   # Apple and Google by this email
   ]
   ```

   `handle` is the name the user signs with (OWN-10). `apple` and `google` default to
   `email`; set `apple` when Apple hides the address behind a relay. Without `github`,
   a GitHub account whose verified emails include `email` signs in as the user. Edit
   the list, then restart the console service; `doctor` fails on the earlier forms
   (`"hhenson"`, `{ id, handle }`) and names the email to add.
3. **GitHub OAuth app** (github.com → Settings → Developer settings → OAuth Apps → New):
   homepage `https://<name>`, callback `https://<name>/auth/github/callback`. Put the
   client id in `[console.github] client_id` and the client secret in
   `~/.config/hgl-ai/console/github.secret` (mode 0600). The console asks for
   `user:email` (read-only) to match a user without a `github` login.
4. **Sign in with Apple** (developer.apple.com → Identifiers): an App ID with Sign in
   with Apple, a Services ID for the console with domain `<name>` and return URL
   `https://<name>/auth/apple/callback`, and a key with Sign in with Apple. Put the
   Services ID, team id and key id in `[console.apple]`, the `.p8` key in
   `~/.config/hgl-ai/console/apple.p8` (mode 0600).
5. **Google** (console.cloud.google.com): a project; APIs & Services → OAuth consent
   screen (External or Internal, scopes `openid` and `email`, the users as test users
   while it is in testing); Credentials → Create credentials → OAuth client ID → Web
   application, authorised redirect URI `https://<name>/auth/google/callback`. Put the
   client id in `[console.google] client_id` and the client secret in
   `~/.config/hgl-ai/console/google.secret` (mode 0600).

   One of GitHub, Apple or Google is enough.
6. **Notifications** (`[[console.notify]]`, tried in order): an ntfy topic URL (a
   hard-to-guess topic, or a token in `token_file`), a Slack incoming webhook URL in
   `webhook_file`, SMTP as the last resort (without `to`, every user's `email`).
   Secrets in mode-0600 files.
7. `hgl-ai doctor`, then open `https://<name>/signin`: a provider once, then register a
   passkey; later sign-ins use the passkey. A new device: a provider again.

The console's credential and secrets belong to the server user; `doctor`, run as
the client user, checks that it (and so its agents) cannot read them. It finds the
server user and its home from the hub's system unit (`User=`, `HOME`). With
`--client-user NAME`, `doctor` as you checks the server side only and points at
`sudo -u NAME env HOME=<its home> hgl-ai doctor`; there it finds the bridge without a
login session (`/run/user/<uid>`) and the CLIs on the bridge unit's `PATH` or in
`~/.local/bin`.

## Agents

The bridge starts headless turns by itself when notes arrive, for every seat but
an interactive controller. The controller is interactive by default; with
`hgl-ai seat add controller --role controller ... --headless` (`headless = true`) the
bridge serves it too (OWN-7) and no session waits for it. An interactive session
for a seat, for example the controller:

```sh
hgl-ai agent start controller      # tmux session hgl-controller
hgl-ai agent attach controller
hgl-ai agent stop controller
```

The session starts the seat's runtime with its model and effort in
`<work_root>/<seat>/_interactive` (`--cwd` overrides), with `HGL_AI_SEAT`,
`HGL_AI_ROLE`, `HGL_AI_TASK_DIR` and `TMPDIR` set.

Each CLI must be signed in once by its user (`claude`, `codex login`, `agy`).
`doctor` reports a CLI that is installed but not signed in, and for `codex`
whether it applies the turns' permission profile (`codex profile`) and loads its
`config.toml` under `--strict-config` (`codex config`).

## Proxy builders

A Windows host is built from a Linux or macOS host over ssh (`docs/build.md` §2a); it
runs no hgl-ai.

1. On the remote: the OpenSSH server, key authentication for the proxy's user, `git`
   and the toolchains on that user's `PATH` (Rust, CMake, Ninja, MSVC as the recipes
   need), and a work directory.
2. On the proxy: an alias in `~/.ssh/config` that logs in without a prompt
   (`ssh winbox exit` works), then in `config.toml`:

   ```toml
   [[builder.remote]]
   platform = "windows"
   ssh = "winbox"
   work_dir = "C:/hgl-ai/work"
   shell = "powershell"
   ```

3. Restart the builder service. `hgl-ai builder list` shows the remote with `via
   <proxy>`, its reachability, free disk and tool versions.

The alias and directory stay in the proxy's file, never on the hub.
