# Install

Status: proposed.

## Hub (one host)

```sh
brew install hhenson/hgraph/hgl-ai
hgl-ai server install --listen <tailnet-address>     # as yourself; root steps via sudo
```

The server side (hub, coordinator, console) runs as its own OS user, `hgl-ai`
(`_hgl-ai` on macOS; `--server-user`), from system units (`User=`) or launch daemons
(`UserName`), with its configuration, credentials and data in that user's home,
`/var/lib/hgl-ai` (`/Library/hgl-ai`; `--server-home`), mode 0700. The client side
(bridge, agent CLIs, their sign-ins) stays yours. Each step that needs root is shown
and runs through `sudo` only when you confirm it; `--print-root-steps` prints them as
a script instead and runs none:

| Step (root) | Linux | macOS |
| --- | --- | --- |
| the server user | `useradd --system --user-group --home-dir /var/lib/hgl-ai --shell /usr/sbin/nologin hgl-ai` | `sysadminctl -addUser _hgl-ai -roleAccount -home /Library/hgl-ai -shell /usr/bin/false` |
| its home | `install -d -o hgl-ai -m 0700 /var/lib/hgl-ai`, `chown hgl-ai:hgl-ai` | `install -d -o _hgl-ai -m 0700 /Library/hgl-ai` |
| `hgl-ai` it can run, when yours is under your home | `install -m 0755 <yours> /usr/local/bin/hgl-ai` | same |
| its files, as the server user | `sudo -u hgl-ai env HOME=... hgl-ai server install --same-user --no-services ...` | same |
| units | `install -m 0644 ... /etc/systemd/system/hgl-ai-{hub,coord,console}.service`, `systemctl daemon-reload`, `enable`, `restart` | `/Library/LaunchDaemons/io.github.hhenson.hgl-ai.*.plist`, `launchctl bootstrap system`, `kickstart` |
| provisioning, as the server user's admin hub user | `sudo -u hgl-ai env ... HGL_AI_HUB_CREDENTIAL=.../<hub>-admin.cred hgl-ai init` | same |
| this host's client credential, for you | `install -m 0600 -o <you> /var/lib/hgl-ai/.config/hgl-ai/hub.token ~/.config/hgl-ai/hub.token` | same |

`--same-user` installs the server side as yourself with user services (development
and tests; `doctor` warns that agents can then read the console's credential).

The server user's step of `server install`:

1. Uses `nats-server` from `PATH`, or installs it (`brew install
   nats-server` where Homebrew exists and the install is `--same-user`, else the
   pinned release, checksum verified, into `<data_dir>/bin`).
2. Makes the hub users (below): `<host>-admin`, `<host>` (this host's client,
   in `hub.token_file`), `<host>-coord` and `console`, keeping existing keys, and
   writes `<data_dir>/nats.conf` (mode 0600: listen address, JetStream store,
   one NKey user per client with its role's permissions).
3. Writes the `[server]` section, installs and starts the hub user service,
   waits for the port, provisions the streams and buckets as the admin user (as
   `hgl-ai init`), then installs the coordinator service (`--no-coord` skips it)
   and the console service (`--no-console` skips it; it needs `[console]`).
4. Prints the client commands. A credential is copied to its host by hand or a
   secret manager; no secret is ever printed.

`server start|stop|status` manage the services (`start`, `stop` as root steps);
`server uninstall` removes them, keeping the server user's home and JetStream store
unless `--purge`, which also removes the user.

The coordinator reads GitHub with its own token, not yours: make a fine-grained,
read-only token (Metadata, Contents, Issues, Pull requests, Checks: read) for the
repositories, and store it as the server user (`[github] token_file`):

```sh
sudo -u hgl-ai sh -c 'umask 077; cat > /var/lib/hgl-ai/.config/hgl-ai/github.token'
```

The server user's own commands run with `sudo -u hgl-ai env HOME=/var/lib/hgl-ai
hgl-ai ...` (`server user add`, `doctor`).

## Hub users

Every client has its own NKey user (OWN-5). `<data_dir>/hub-users.toml` lists
names, roles and public keys; a credential (`user`, `role`, `seed`, mode 0600) is
written once to `~/.config/hgl-ai/hub-users/<name>.cred` (the server user's home in
a system install: run `server user` there as `sudo -u hgl-ai env HOME=/var/lib/hgl-ai
hgl-ai server user ...`).

| Role | May publish |
| --- | --- |
| `seat` | `hgl.notes.<seat>`, `hgl.console.>`, `HGL_STATE`, `HGL_HOSTS`, `HGL_TREES`, `HGL_LOCKS`; never `hgl.owner.>`, `hgl.notes.owner.>`, `HGL_THREADS`, stream changes |
| `coord` | a seat's, and `HGL_METRICS` |
| `console` | `hgl.owner.>`, `hgl.notes.owner.<seat>`, `tick` notes, `HGL_THREADS`, `decisions.log` |
| `admin` | everything (`hgl-ai init`, `server install`) |

Every role reads every stream and bucket.

```sh
hgl-ai server user add build-1 --role seat         # writes hub-users/build-1.cred
hgl-ai server user add build-2 --role seat --nkey U...   # a host's own key; no secret here
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
4. Installs and starts the bridge user service, with the `PATH` of the install.

`client uninstall` removes the service and the skills, keeping the
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
you, checks that you (and so your agents) cannot read them.

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
