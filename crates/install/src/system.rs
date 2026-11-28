//! The server side as its own OS user (docs/install.md): the hub, coordinator and console
//! run as a dedicated user (default `hgl-ai`, `_hgl-ai` on macOS) from system units or
//! launch daemons, with their configuration, credentials and data in that user's home;
//! the client side (bridge, agent CLIs) stays the owner's.
//!
//! Every step that needs root is shown first and runs through `sudo` only when the
//! person at the terminal confirms it, or is printed for them to run
//! (`--print-root-steps`); nothing runs as root silently.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hgl_ai_config::slack::{APP_TOKEN_FILE, BOT_TOKEN_FILE};

use crate::env::Os;
use crate::service::{self, Service, Spec};
use crate::sys::{Cmd, System, shell_quote};
use crate::{Result, io_at, load_config, other, save_config};

/// The server user's name.
pub fn default_user(os: Os) -> &'static str {
    match os {
        Os::Linux => "hgl-ai",
        // Role accounts on macOS start with an underscore.
        Os::MacOs => "_hgl-ai",
    }
}

/// The server user's home: everything of the server side lives under it.
pub fn default_home(os: Os) -> PathBuf {
    match os {
        Os::Linux => PathBuf::from("/var/lib/hgl-ai"),
        Os::MacOs => PathBuf::from("/Library/hgl-ai"),
    }
}

#[derive(Debug, Clone)]
pub struct SystemOpts {
    pub user: String,
    pub home: PathBuf,
    /// Print the root steps as a script and run none.
    pub print_only: bool,
    /// Host label of the hub host (its client and admin users are named after it).
    pub host: String,
    /// Arguments for the server user's `server install` (listen, port, console, ...).
    pub inner_args: Vec<String>,
    pub listen: String,
    pub port: u16,
    pub coord: bool,
    /// Install the console service: only when the server side has `[console]`.
    pub console: bool,
    /// Install the opt-in Slack Q&A system service.
    pub slack: bool,
    /// `--console-url`, recorded so a later reinstall keeps the console.
    pub console_url: Option<String>,
    /// The OS user this host's client runs as, when not the installer: the client
    /// credential goes to it, never to the installer.
    pub client_user: Option<String>,
    /// Keep the existing GitHub and Slack tokens without asking for them.
    pub keep_token: bool,
    pub wait: Duration,
}

/// What an earlier system install left in this client's configuration: a reinstall
/// that omits `--host`, `--listen`, `--port` or `--console-url` keeps them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Previous {
    pub host: Option<String>,
    pub listen: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub home: Option<String>,
    pub console_url: Option<String>,
    pub slack: bool,
    pub client_user: Option<String>,
}

pub fn previous(sys: &dyn System) -> Previous {
    let Ok(Some(cfg)) = load_config(sys, sys.env()) else {
        return Previous::default();
    };
    let Some(su) = cfg.server_user else {
        return Previous::default();
    };
    let addr = cfg
        .hub
        .url
        .strip_prefix("nats://")
        .and_then(|r| r.rsplit_once(':'))
        .map(|(h, p)| {
            (
                h.trim_matches(['[', ']']).to_string(),
                p.parse::<u16>().ok(),
            )
        });
    let slack = slack_enabled(sys, &su);
    Previous {
        host: Some(cfg.host),
        listen: addr.as_ref().map(|(h, _)| h.clone()),
        port: addr.and_then(|(_, p)| p),
        user: Some(su.name),
        home: Some(su.home),
        console_url: su.console_url,
        slack,
        client_user: su.client_user,
    }
}

/// Older client configs do not record Slack, so check the installed service too.
pub(crate) fn slack_enabled(sys: &dyn System, user: &hgl_ai_config::ServerUser) -> bool {
    user.slack
        || service::system_account(sys, Service::Slack).is_some_and(|(name, _)| name == user.name)
}

/// Where the server user keeps things, by its home.
pub struct Layout {
    pub home: PathBuf,
}

impl Layout {
    pub fn config(&self) -> PathBuf {
        self.home.join(".config/hgl-ai/config.toml")
    }
    pub fn data(&self) -> PathBuf {
        self.home.join(".local/share/hgl-ai")
    }
    pub fn creds(&self) -> PathBuf {
        self.home.join(".config/hgl-ai/hub-users")
    }
    pub fn console_cred(&self) -> PathBuf {
        self.creds().join("console.cred")
    }
    pub fn client_cred(&self) -> PathBuf {
        self.home.join(".config/hgl-ai/hub.token")
    }
    pub fn github_token(&self) -> PathBuf {
        self.home.join(".config/hgl-ai/github.token")
    }
}

/// How root steps run.
struct Root<'a> {
    sys: &'a dyn System,
    print_only: bool,
    script: Vec<String>,
}

impl Root<'_> {
    /// Show `cmd`, then run it with `sudo` once confirmed (or only print it). The line
    /// is quoted for a POSIX shell: the script runs as it reads.
    fn step(&mut self, why: &str, cmd: Cmd) -> Result<()> {
        let line = format!("sudo {cmd}");
        if self.print_only {
            self.script.push(format!("# {why}\n{line}"));
            return Ok(());
        }
        self.sys.say(&format!("root step: {why}\n  {line}"));
        if !self.sys.confirm("Run it?") {
            return Err(other(format!(
                "stopped before `{line}`; nothing after it ran (`--print-root-steps` lists every step)"
            )));
        }
        // On the terminal: sudo may ask for a password (hgl-ai#90).
        let sudo = Cmd::new("sudo")
            .arg(cmd.program.clone())
            .args(cmd.args.iter().cloned())
            .terminal();
        crate::run_ok(self.sys, &sudo).map(drop)
    }

    /// Show `from | to`, then run that pipeline with `sh` once confirmed (or only print
    /// it). For a secret that one user reads and another writes: neither end is root,
    /// and it passes between them, never through this process.
    fn pipe(&mut self, why: &str, from: Cmd, to: Cmd) -> Result<()> {
        let line = format!("{from} | {to}");
        if self.print_only {
            self.script.push(format!("# {why}\n{line}"));
            return Ok(());
        }
        self.sys.say(&format!("sudo step: {why}\n  {line}"));
        if !self.sys.confirm("Run it?") {
            return Err(other(format!(
                "stopped before `{line}`; nothing after it ran (`--print-root-steps` lists every step)"
            )));
        }
        crate::run_ok(self.sys, &Cmd::new("sh").args(["-c", &line]).terminal()).map(drop)
    }

    /// Show `sudo to`, then once confirmed ask for a secret here, on this process's
    /// terminal without echo, and run `sudo to` with it on its standard input; an empty
    /// answer keeps the old one and runs nothing. Printed, the script's own shell reads
    /// it ([`SECRET_PROMPT`]) and pipes it. Nothing under `sudo` reads the terminal:
    /// under sudo's own pseudo-terminal it never sees the keys (hgl-ai#90).
    fn secret(&mut self, why: &str, prompt: &str, to: Cmd, keep_if: Option<Cmd>) -> Result<()> {
        let line = format!("sudo {to}");
        if let Some(check) = &keep_if {
            if self.print_only {
                self.script.push(format!("if ! sudo {check}; then"));
            } else if self
                .sys
                .query(
                    &Cmd::new("sudo")
                        .arg("-n")
                        .arg(&check.program)
                        .args(check.args.iter()),
                )
                .is_ok_and(|out| out.ok())
            {
                self.note(&format!("{why}: kept (--keep-token)"));
                return Ok(());
            }
        }
        if self.print_only {
            self.script.push(format!(
                "# {why}; Enter keeps the current one\n{}\n[ -z \"$t\" ] || printf '%s\\n' \"$t\" | {line}",
                SECRET_PROMPT.replace("PROMPT", &shell_quote(prompt))
            ));
            if keep_if.is_some() {
                self.script.push("fi".into());
            }
            return Ok(());
        }
        self.sys.say(&format!(
            "sudo step: {why}\n  {line}  (the answer on its input)"
        ));
        if !self.sys.confirm("Run it?") {
            return Err(other(format!(
                "stopped before `{line}`; nothing after it ran (`--print-root-steps` lists every step)"
            )));
        }
        let answer = self
            .sys
            .read_secret(prompt)
            .map_err(|e| other(format!("reading the answer from the terminal: {e}")))?
            .ok_or_else(|| {
                other("no terminal to read the token on (use a terminal to enter missing tokens or `--keep-token` to keep existing ones)")
            })?;
        let answer = answer.trim();
        if answer.is_empty() {
            self.sys.say("kept the current one");
            return Ok(());
        }
        let sudo = Cmd::new("sudo")
            .arg(to.program.clone())
            .args(to.args.iter().cloned())
            .secret(format!("{answer}\n"));
        crate::run_ok(self.sys, &sudo).map(drop)
    }

    fn note(&mut self, text: &str) {
        if self.print_only {
            self.script.push(format!("# {text}"));
        } else {
            self.sys.say(text);
        }
    }
}

/// `PATH` for the server user: the install-time `PATH` without the owner's home, which
/// the server user cannot enter.
fn server_path(sys: &dyn System) -> String {
    let env = sys.env();
    env.path_var
        .split(':')
        .filter(|p| !p.is_empty() && !Path::new(p).starts_with(&env.home))
        .collect::<Vec<_>>()
        .join(":")
}

/// The program the server user runs: this binary where it can reach it.
fn server_exe(sys: &dyn System) -> (PathBuf, bool) {
    let env = sys.env();
    let exe = env.service_exe(sys);
    if exe.starts_with(&env.home) {
        (PathBuf::from("/usr/local/bin/hgl-ai"), true)
    } else {
        (exe, false)
    }
}

/// `nats-server` for the hub unit: on the server user's `PATH`, else the pinned release
/// the server user's step downloads into its data directory. Chosen here only: the
/// server user's step is told it (`--nats-server`).
fn nats_for(sys: &dyn System, layout: &Layout) -> PathBuf {
    sys.which_on(&server_path(sys), "nats-server")
        .filter(|p| !p.starts_with(&sys.env().home))
        .unwrap_or_else(|| crate::nats::release_path(&layout.data()))
}

/// The client user's home, from the account database.
fn account(sys: &dyn System, name: &str) -> Result<PathBuf> {
    home_of(sys, name)
        .ok_or_else(|| other(format!("--client-user {name}: no such user (or no home)")))
}

/// A user's home, from the account database (`getent`, `dscl`).
pub(crate) fn home_of(sys: &dyn System, name: &str) -> Option<PathBuf> {
    let out = |c: Cmd| {
        sys.query(&c)
            .ok()
            .filter(|o| o.ok())
            .map(|o| o.stdout.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    match sys.env().os {
        Os::Linux => out(Cmd::new("getent").args(["passwd", name]))
            .and_then(|l| l.split(':').nth(5).map(str::to_string)),
        Os::MacOs => out(Cmd::new("dscl").args([
            ".",
            "-read",
            &format!("/Users/{name}"),
            "NFSHomeDirectory",
        ]))
        .and_then(|l| {
            l.strip_prefix("NFSHomeDirectory:")
                .map(|h| h.trim().to_string())
        }),
    }
    .filter(|h| h.starts_with('/'))
    .map(PathBuf::from)
}

/// A `sh -c` program, run as the credential's owner: its directory (`$1`; new
/// directories 0700) and `hub.token` in it, mode 0600, from stdin through a temporary
/// file. Run as that user, a symlink in its home reaches only what it may already
/// write; an empty read keeps the old file.
const CRED_WRITER: &str = "umask 077; d=$1; mkdir -p \"$d\" && chmod 700 \"$d\" || exit 1; \
     t=$(mktemp \"$d/.hub.token.XXXXXX\") || exit 1; \
     if cat >\"$t\" && [ -s \"$t\" ]; then mv -f \"$t\" \"$d/hub.token\"; \
     else rm -f \"$t\"; exit 1; fi";

/// A `sh -c` program, run as the server user: its file `$1`, mode 0600, from stdin
/// through a temporary file beside it; an empty read keeps the old file.
const SECRET_WRITER: &str = "umask 077; f=$1; t=$(mktemp \"$f.XXXXXX\") || exit 1; \
     if cat >\"$t\" && [ -s \"$t\" ]; then mv -f \"$t\" \"$f\"; \
     else rm -f \"$t\"; exit 1; fi";

/// The printed script's prompt, `PROMPT` replaced by its quoted text: the script's own
/// shell, at the owner's terminal, reads the answer into `$t` without echo. Echo comes
/// back on however the read ends; a signal stops the script.
const SECRET_PROMPT: &str = "t=$(trap 'stty echo </dev/tty 2>/dev/null' EXIT; \
     trap 'exit 130' INT; trap 'exit 143' TERM; trap 'exit 129' HUP; \
     printf '%s' PROMPT >/dev/tty; stty -echo </dev/tty; IFS= read -r t </dev/tty; \
     stty echo </dev/tty; echo >/dev/tty; printf '%s' \"$t\") || exit";

/// The coordinator's token prompt.
const TOKEN_PROMPT: &str = "GitHub token for the coordinator (Enter keeps the current one): ";

/// The token the coordinator reads GitHub with.
const TOKEN_SCOPE: &str = "a fine-grained, read-only token (Metadata, Contents, Issues, \
     Pull requests, Checks, Actions, Commit statuses: read) for the repositories";

fn sudo_as(
    user: &str,
    layout: &Layout,
    path: &str,
    exe: &Path,
    extra_env: &[(&str, String)],
) -> Cmd {
    let mut c = Cmd::new("-u")
        .arg(user.to_string())
        .arg("env")
        .arg(format!("HOME={}", layout.home.display()))
        .arg(format!("PATH={path}"))
        .arg(format!("HGL_AI_CONFIG={}", layout.config().display()));
    for (k, v) in extra_env {
        c = c.arg(format!("{k}={v}"));
    }
    c.arg(exe.display().to_string())
}

/// The services' specs as the server user runs them.
pub fn specs(sys: &dyn System, o: &SystemOpts) -> Vec<Spec> {
    let layout = Layout {
        home: o.home.clone(),
    };
    let data = layout.data();
    let (exe, _) = server_exe(sys);
    let path = server_path(sys);
    let os = sys.env().os;
    let mut out = vec![Spec {
        log: service::system_log_path(os, Service::Hub),
        ..Spec::hub(&nats_for(sys, &layout), &data.join("nats.conf"), &data)
    }];
    let host = &o.host;
    let svc = |s: Service, args: &[&str], cred: PathBuf| Spec {
        service: s,
        program: exe.clone(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: vec![
            ("PATH".into(), path.clone()),
            (
                "HGL_AI_CONFIG".into(),
                layout.config().display().to_string(),
            ),
            (
                hgl_ai_config::CREDENTIAL_ENV.into(),
                cred.display().to_string(),
            ),
        ],
        log: service::system_log_path(os, s),
    };
    if o.coord {
        out.push(svc(
            Service::Coord,
            &["gh", "coord", "run"],
            layout.creds().join(format!("{host}-coord.cred")),
        ));
    }
    if o.console {
        out.push(svc(
            Service::Console,
            &["console", "run"],
            layout.console_cred(),
        ));
    }
    if o.slack {
        out.push(svc(
            Service::Slack,
            &["slack", "run"],
            layout
                .creds()
                .join(format!("{}.cred", crate::server::SLACK_USER)),
        ));
    }
    out
}

fn wait_port(sys: &dyn System, host: &str, port: u16, wait: Duration) -> bool {
    let start = Instant::now();
    loop {
        if sys.port_open(host, port) {
            return true;
        }
        if start.elapsed() >= wait {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Install the server side as `o.user`. Returns the script when printing only.
pub fn install(sys: &dyn System, o: &SystemOpts) -> Result<Option<String>> {
    if o.slack && !o.console {
        return Err(other("--slack requires the owner console"));
    }
    let env = sys.env();
    let os = env.os;
    let layout = Layout {
        home: o.home.clone(),
    };
    let user = o.user.as_str();
    let owner = sys
        .query(&Cmd::new("id").arg("-un"))
        .map(|o| o.stdout.trim().to_string())
        .unwrap_or_default();
    if owner.is_empty() {
        return Err(other("`id -un` failed"));
    }
    if owner == user {
        return Err(other(format!(
            "run this as yourself, not as {user}: the client side stays yours"
        )));
    }
    let client_user = o.client_user.as_deref().filter(|c| *c != owner);
    if client_user == Some(user) {
        return Err(other(format!(
            "--client-user {user} is the server user: the client side runs as another user"
        )));
    }
    let client = client_user
        .map(|c| account(sys, c).map(|h| (c, h)))
        .transpose()?;
    let mut root = Root {
        sys,
        print_only: o.print_only,
        script: Vec::new(),
    };
    let exists = sys
        .query(&Cmd::new("id").args(["-u", user]))
        .is_ok_and(|r| r.ok());
    let home = layout.home.display().to_string();
    if !exists {
        let cmd = match os {
            Os::Linux => Cmd::new("useradd").args([
                "--system",
                "--user-group",
                "--home-dir",
                &home,
                "--shell",
                "/usr/sbin/nologin",
                user,
            ]),
            Os::MacOs => Cmd::new("sysadminctl").args([
                "-addUser",
                user,
                "-roleAccount",
                "-home",
                &home,
                "-shell",
                "/usr/bin/false",
            ]),
        };
        root.step(&format!("create the server user {user}"), cmd)?;
    }
    let owner_of = match os {
        Os::Linux => format!("{user}:{user}"),
        Os::MacOs => user.to_string(),
    };
    root.step(
        &format!("its home {home}, mode 0700"),
        Cmd::new("install").args(["-d", "-o", user, "-m", "0700", &home]),
    )?;
    if os == Os::Linux {
        root.step(
            "its group owns its home",
            Cmd::new("chown").args([&owner_of, &home]),
        )?;
    }
    let (exe, copy) = server_exe(sys);
    if copy {
        let mine = env.service_exe(sys).display().to_string();
        root.step(
            "hgl-ai where the server user can run it (yours is under your home)",
            Cmd::new("install").args(["-m", "0755", &mine, &exe.display().to_string()]),
        )?;
    }
    let path = server_path(sys);
    let token_file = layout.github_token().display().to_string();
    let nats = nats_for(sys, &layout).display().to_string();
    let mut inner = sudo_as(user, &layout, &path, &exe, &[]).args([
        "server",
        "install",
        "--same-user",
        "--no-services",
        "--no-brew",
        "--units",
        "system",
        "--github-token-file",
        &token_file,
        "--nats-server",
        &nats,
        "--host",
        &o.host,
    ]);
    inner = inner.args(o.inner_args.iter().cloned());
    // `sudo -u USER env ... hgl-ai server install ...`: written as `sudo` + those words.
    root.step(
        &format!("the server side's configuration, credentials and nats.conf, as {user}"),
        Cmd::new(inner.program.clone()).args(inner.args.iter().cloned()),
    )?;

    // Units: staged under the owner's data directory, installed by root.
    let stage = env.expand("~/.local/share/hgl-ai/system-units");
    let unit_group = match os {
        Os::Linux => user.to_string(),
        Os::MacOs => "staff".into(),
    };
    if os == Os::MacOs {
        // launchd opens the logs as root: in a directory only root writes (hgl-ai#63).
        root.step(
            &format!("the services' logs, {}, root's", service::DAEMON_LOG_DIR),
            Cmd::new("install").args([
                "-d",
                "-o",
                "root",
                "-g",
                "wheel",
                "-m",
                "0755",
                service::DAEMON_LOG_DIR,
            ]),
        )?;
    }
    let mut names = Vec::new();
    for spec in specs(sys, o) {
        let (text, name) = match os {
            Os::Linux => (
                service::render_systemd_system(&spec, user, &unit_group, &layout.home),
                spec.service.unit(),
            ),
            Os::MacOs => (
                service::render_launchd_daemon(&spec, user, &layout.home),
                format!("{}.plist", spec.service.label()),
            ),
        };
        let staged = stage.join(&name);
        sys.write(&staged, text.as_bytes(), 0o644)
            .map_err(io_at(&staged))?;
        let dest = service::system_unit_path(os, spec.service);
        root.step(
            &format!("the {} service", spec.service.short()),
            Cmd::new("install").args([
                "-m",
                "0644",
                &staged.display().to_string(),
                &dest.display().to_string(),
            ]),
        )?;
        names.push(spec.service);
    }
    let start = |root: &mut Root, s: Service| -> Result<()> {
        match os {
            Os::Linux => {
                root.step(
                    &format!("enable and start {}", s.unit()),
                    Cmd::new("systemctl").args(["enable", &s.unit()]),
                )?;
                root.step(
                    &format!("start {}", s.unit()),
                    Cmd::new("systemctl").args(["restart", &s.unit()]),
                )
            }
            Os::MacOs => {
                let plist = service::system_unit_path(os, s).display().to_string();
                // A loaded daemon keeps its old plist, and `bootstrap` refuses it.
                if service::system_state(sys, s) != service::State::NotInstalled {
                    root.step(
                        &format!("unload {} to load its new plist", s.label()),
                        Cmd::new("launchctl").args(["bootout", &format!("system/{}", s.label())]),
                    )?;
                }
                root.step(
                    &format!("load {}", s.label()),
                    Cmd::new("launchctl").args(["bootstrap", "system", &plist]),
                )?;
                root.step(
                    &format!("start {}", s.label()),
                    Cmd::new("launchctl").args([
                        "kickstart",
                        "-k",
                        &format!("system/{}", s.label()),
                    ]),
                )
            }
        }
    };
    if os == Os::Linux {
        root.step(
            "systemd rereads its units",
            Cmd::new("systemctl").arg("daemon-reload"),
        )?;
    }
    start(&mut root, Service::Hub)?;
    let host = crate::server::connect_host(&o.listen);
    if o.print_only {
        root.note(&format!("wait until the hub answers on {host}:{}", o.port));
    } else if !wait_port(sys, &host, o.port, o.wait) {
        return Err(other(format!(
            "the hub did not answer on {host}:{} within {} s; see {}",
            o.port,
            o.wait.as_secs(),
            service::system_log_hint(os, Service::Hub)
        )));
    }
    let admin = layout.creds().join(format!("{}-admin.cred", o.host));
    let init = sudo_as(
        user,
        &layout,
        &path,
        &exe,
        &[(hgl_ai_config::CREDENTIAL_ENV, admin.display().to_string())],
    )
    .arg("init");
    root.step(
        &format!("provision the streams and buckets as {user}'s admin hub user"),
        Cmd::new(init.program.clone()).args(init.args.iter().cloned()),
    )?;
    if o.coord && o.keep_token {
        root.note("the coordinator's GitHub token: kept (--keep-token)");
    } else if o.coord {
        root.secret(
            &format!(
                "the coordinator's GitHub token, {TOKEN_SCOPE}: read without echo, \
                 written as {user} (mode 0600)"
            ),
            TOKEN_PROMPT,
            Cmd::new("-u").args([
                user,
                "sh",
                "-c",
                SECRET_WRITER,
                "sh",
                &layout.github_token().display().to_string(),
            ]),
            None,
        )?;
    }
    if o.slack {
        let dir = layout.config();
        let dir = dir.parent().expect("server configuration directory");
        for (kind, file) in [("bot", BOT_TOKEN_FILE), ("app-level", APP_TOKEN_FILE)] {
            let file = dir.join(file).display().to_string();
            root.secret(
                &format!("the Slack {kind} token, written as {user} (mode 0600)"),
                &format!("Slack {kind} token (Enter keeps the current one): "),
                Cmd::new("-u").args([user, "sh", "-c", SECRET_WRITER, "sh", &file]),
                o.keep_token
                    .then(|| Cmd::new("-u").args([user, "test", "-f", &file])),
            )?;
        }
    }
    for s in names.iter().copied().filter(|s| *s != Service::Hub) {
        start(&mut root, s)?;
    }
    // This host's seat credential goes to the user its client runs as: the installer,
    // or `--client-user`. The server user reads it and that user writes it: root never
    // reads a file the server user controls, nor writes where another user can plant a
    // symlink.
    let read = Cmd::new("sudo").args([
        "-u",
        user,
        "cat",
        &layout.client_cred().display().to_string(),
    ]);
    let writer =
        |dir: &Path| Cmd::new("sh").args(["-c", CRED_WRITER, "sh", &dir.display().to_string()]);
    let (to, shown) = match &client {
        Some((name, home)) => {
            let w = writer(&home.join(".config/hgl-ai"));
            root.pipe(
                &format!("this host's client credential, for {name}: read as {user}, written as {name} (mode 0600)"),
                read,
                Cmd::new("sudo").args(["-u", name]).arg(w.program).args(w.args),
            )?;
            (name.to_string(), "~/.config/hgl-ai/hub.token".to_string())
        }
        None => {
            root.pipe(
                &format!("this host's client credential, for you: read as {user}, written by you (mode 0600)"),
                read,
                writer(&env.config_dir()),
            )?;
            (owner.clone(), env.contract(&env.client_token_path()))
        }
    };
    let next = format!(
        "hgl-ai client install --hub {} --token-file {} --host {}",
        crate::server::local_url(&o.listen, o.port),
        shown,
        o.host
    );
    let who = if client.is_some() {
        format!("as {to}")
    } else {
        "as yourself".to_string()
    };
    if o.print_only {
        root.note(&format!("next, {who}: {next}"));
        return Ok(Some(root.script.join("\n")));
    }
    // The client's configuration names the server user, for `server status` and `doctor`.
    let mut cfg =
        load_config(sys, env)?.unwrap_or_else(|| hgl_ai_config::Config::new(o.host.clone()));
    cfg.server_user = Some(hgl_ai_config::ServerUser {
        name: user.to_string(),
        home: home.clone(),
        console_url: o.console_url.clone().filter(|_| o.console),
        slack: o.slack,
        client_user: client_user.map(str::to_string),
    });
    cfg.hub.url = crate::server::local_url(&o.listen, o.port);
    save_config(sys, env, &cfg)?;
    sys.say("");
    sys.say(&format!("Next, on this host, {who}:"));
    sys.say(&format!("  {next}"));
    Ok(None)
}

/// What to do about a unit from before hgl-ai#63, whose log root opens in the server
/// user's home (docs/install.md, Upgrading).
pub const LOG_UPGRADE: &str = "rerun `hgl-ai server install`: its root steps reinstall the units";

/// The server side's services, read without privilege; `true` when all run and the
/// port answers.
pub fn status(
    sys: &dyn System,
    su: &hgl_ai_config::ServerUser,
    listen_port: Option<(String, u16)>,
) -> bool {
    let mut up = true;
    for s in [
        Service::Hub,
        Service::Coord,
        Service::Console,
        Service::Slack,
    ] {
        let st = service::system_state(sys, s);
        let text = match st {
            service::State::Running => "running",
            service::State::Stopped => "stopped",
            service::State::NotInstalled => "not installed",
        };
        up &= st == service::State::Running
            || (s != Service::Hub && st == service::State::NotInstalled);
        if st == service::State::NotInstalled {
            sys.say(&format!("{} (system, as {}): {text}", s.short(), su.name));
        } else {
            sys.say(&format!(
                "{} (system, as {}): {text}; logs: {}",
                s.short(),
                su.name,
                service::system_log_hint(sys.env().os, s)
            ));
        }
        if let Some(why) = service::system_log_unsafe(sys, s) {
            sys.say(&format!(
                "  {why}: root opens this log in the server user's home (hgl-ai#63); {LOG_UPGRADE}"
            ));
        }
    }
    if let Some((host, port)) = listen_port {
        let open = sys.port_open(&host, port);
        sys.say(&format!(
            "port {host}:{port}: {}",
            if open { "answering" } else { "not answering" }
        ));
        up &= open;
    }
    up
}

/// Start or stop (`verb`) the server side's services: root steps, each confirmed.
pub fn control(sys: &dyn System, verb: &str, print_only: bool) -> Result<Option<String>> {
    let mut root = Root {
        sys,
        print_only,
        script: Vec::new(),
    };
    let mut svcs = vec![
        Service::Hub,
        Service::Coord,
        Service::Console,
        Service::Slack,
    ];
    if verb == "stop" {
        svcs.reverse();
    }
    for s in svcs {
        if service::system_state(sys, s) == service::State::NotInstalled {
            continue;
        }
        let cmd = match sys.env().os {
            Os::Linux => Cmd::new("systemctl").args([verb, &s.unit()]),
            Os::MacOs if verb == "stop" => {
                Cmd::new("launchctl").args(["bootout", &format!("system/{}", s.label())])
            }
            Os::MacOs => {
                Cmd::new("launchctl").args(["kickstart", &format!("system/{}", s.label())])
            }
        };
        root.step(&format!("{verb} {}", s.short()), cmd)?;
    }
    Ok(print_only.then(|| root.script.join("\n")))
}

/// Remove the server side's services; with `purge`, its user and home too.
pub fn uninstall(
    sys: &dyn System,
    su: &hgl_ai_config::ServerUser,
    purge: bool,
    print_only: bool,
) -> Result<Option<String>> {
    let os = sys.env().os;
    let mut root = Root {
        sys,
        print_only,
        script: Vec::new(),
    };
    for s in [
        Service::Slack,
        Service::Console,
        Service::Coord,
        Service::Hub,
    ] {
        let path = service::system_unit_path(os, s).display().to_string();
        if s == Service::Slack && !sys.exists(Path::new(&path)) {
            continue;
        }
        match os {
            Os::Linux => {
                root.step(
                    &format!("stop {}", s.unit()),
                    Cmd::new("systemctl").args(["disable", "--now", &s.unit()]),
                )?;
                root.step(
                    &format!("remove {path}"),
                    Cmd::new("rm").args(["-f", &path]),
                )?;
            }
            Os::MacOs => {
                root.step(
                    &format!("stop {}", s.label()),
                    Cmd::new("launchctl").args(["bootout", &format!("system/{}", s.label())]),
                )?;
                root.step(
                    &format!("remove {path}"),
                    Cmd::new("rm").args(["-f", &path]),
                )?;
            }
        }
    }
    if os == Os::Linux {
        root.step(
            "systemd rereads its units",
            Cmd::new("systemctl").arg("daemon-reload"),
        )?;
    }
    if purge {
        root.step(
            &format!("remove {} and its JetStream store", su.home),
            Cmd::new("rm").args(["-rf", &su.home]),
        )?;
        let cmd = match os {
            Os::Linux => Cmd::new("userdel").arg(su.name.clone()),
            Os::MacOs => Cmd::new("sysadminctl").args(["-deleteUser", &su.name]),
        };
        root.step(&format!("remove the user {}", su.name), cmd)?;
    } else {
        root.note(&format!(
            "kept {} (--purge removes it and the user)",
            su.home
        ));
    }
    Ok(print_only.then(|| root.script.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Output;
    use crate::sys::fake::Fake;

    fn fake(d: &Path, os: Os, user_exists: bool) -> Fake {
        let f = Fake::new(d, os);
        f.tool("nats-server");
        f.on(move |c| {
            let a: Vec<&str> = c.args.iter().map(String::as_str).collect();
            match (c.program.as_str(), a.as_slice()) {
                ("id", ["-un"]) => Output::success("owner\n"),
                ("id", ["-gn"]) => Output::success("owner\n"),
                ("id", ["-gn", "agents"]) => Output::success("agents\n"),
                ("getent", ["passwd", "agents"]) => {
                    Output::success("agents:x:1001:1001::/home/agents:/bin/bash\n")
                }
                ("dscl", [".", "-read", "/Users/agents", "NFSHomeDirectory"]) => {
                    Output::success("NFSHomeDirectory: /Users/agents\n")
                }
                ("getent", _) | ("dscl", _) => Output::failure(2, ""),
                ("id", ["-u", _]) if user_exists => Output::success("990\n"),
                ("id", ["-u", _]) => Output::failure(1, "no such user"),
                _ => Output::success(""),
            }
        });
        // Enter at the token prompt: keep the current one.
        f.secrets.borrow_mut().push_back(String::new());
        f
    }

    fn opts(print: bool) -> SystemOpts {
        SystemOpts {
            user: "hgl-ai".into(),
            home: PathBuf::from("/var/lib/hgl-ai"),
            print_only: print,
            host: "hub".into(),
            inner_args: vec!["--listen".into(), "192.0.2.10".into()],
            listen: "192.0.2.10".into(),
            port: 4222,
            coord: true,
            console: true,
            slack: false,
            console_url: Some("https://console.example.com".into()),
            client_user: None,
            keep_token: false,
            wait: Duration::ZERO,
        }
    }

    #[test]
    fn slack_root_steps_install_the_service_and_prompt_for_private_tokens() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux, true);
        let mut o = opts(true);
        o.slack = true;
        o.keep_token = true;
        let script = install(&f, &o).unwrap().unwrap();
        assert!(f.runs().is_empty());
        let start = script
            .find("systemctl restart hgl-ai-slack.service")
            .unwrap();
        for want in [BOT_TOKEN_FILE, APP_TOKEN_FILE, "stty -echo", "umask 077"] {
            assert!(script.find(want).unwrap() < start, "{want}");
        }
        assert_eq!(script.matches("if ! sudo -u hgl-ai test -f ").count(), 2);
        let dir = f.env.home.join(".local/share/hgl-ai/system-units");
        let unit = std::fs::read_to_string(dir.join("hgl-ai-slack.service")).unwrap();
        assert!(unit.contains("User=hgl-ai\n"));
        assert!(unit.contains("hub-users/slack.cred"));
        assert!(unit.contains("ExecStart=/usr/local/bin/hgl-ai slack run"));
        assert!(!unit.contains(BOT_TOKEN_FILE) && !unit.contains(APP_TOKEN_FILE));
    }

    #[test]
    fn root_steps_are_printed_not_run() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux, false);
        let script = install(&f, &opts(true)).unwrap().unwrap();
        assert!(f.runs().is_empty(), "nothing ran: {:?}", f.runs());
        for want in [
            "sudo useradd --system --user-group --home-dir /var/lib/hgl-ai --shell /usr/sbin/nologin hgl-ai",
            "sudo install -d -o hgl-ai -m 0700 /var/lib/hgl-ai",
            "sudo install -m 0755 ", // hgl-ai, out of the owner's home
            "sudo -u hgl-ai env HOME=/var/lib/hgl-ai",
            "server install --same-user --no-services --no-brew --units system --github-token-file /var/lib/hgl-ai/.config/hgl-ai/github.token --nats-server /usr/bin/nats-server --host hub --listen 192.0.2.10",
            "sudo install -m 0644 ",
            "/etc/systemd/system/hgl-ai-hub.service",
            "sudo systemctl daemon-reload",
            "sudo systemctl restart hgl-ai-hub.service",
            "# wait until the hub answers on 192.0.2.10:4222",
            "HGL_AI_HUB_CREDENTIAL=/var/lib/hgl-ai/.config/hgl-ai/hub-users/hub-admin.cred",
            " init",
            "sudo systemctl restart hgl-ai-console.service",
            "sudo -u hgl-ai cat /var/lib/hgl-ai/.config/hgl-ai/hub.token | sh -c ",
            "sudo -u hgl-ai sh -c ",
            "# next, as yourself: hgl-ai client install --hub nats://192.0.2.10:4222 --token-file ~/.config/hgl-ai/hub.token --host hub",
        ] {
            assert!(script.contains(want), "missing {want:?} in\n{script}");
        }
        // The units run as the server user, with its home.
        let unit = std::fs::read_to_string(
            f.env
                .home
                .join(".local/share/hgl-ai/system-units/hgl-ai-console.service"),
        )
        .unwrap();
        for want in [
            "User=hgl-ai\n",
            "Group=hgl-ai\n",
            "Environment=\"HOME=/var/lib/hgl-ai\"",
            "Environment=\"HGL_AI_HUB_CREDENTIAL=/var/lib/hgl-ai/.config/hgl-ai/hub-users/console.cred\"",
            "WantedBy=multi-user.target",
            "ExecStart=/usr/local/bin/hgl-ai console run",
        ] {
            assert!(unit.contains(want), "missing {want:?} in\n{unit}");
        }
        let hub = std::fs::read_to_string(
            f.env
                .home
                .join(".local/share/hgl-ai/system-units/hgl-ai-hub.service"),
        )
        .unwrap();
        assert!(
            hub.contains("-c /var/lib/hgl-ai/.local/share/hgl-ai/nats.conf"),
            "{hub}"
        );
        // The journal, not a file root opens in the server user's home (hgl-ai#63).
        for u in [&unit, &hub] {
            assert!(u.contains("StandardOutput=journal\n"), "{u}");
            assert!(!u.contains("append:"), "{u}");
        }
        assert!(
            !script.contains("Logs"),
            "no log directory on Linux: {script}"
        );
    }

    /// `server status` names where the logs are, and a unit from before hgl-ai#63.
    #[test]
    fn status_names_the_logs() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        f.on(|c| {
            if c.args.iter().any(|a| a.starts_with("StandardOutput")) {
                Output::success("StandardOutput=append\nStandardError=append\n")
            } else {
                Output::success("LoadState=loaded\nActiveState=active\n")
            }
        });
        let su = hgl_ai_config::ServerUser {
            name: "hgl-ai".into(),
            home: "/var/lib/hgl-ai".into(),
            console_url: None,
            slack: false,
            client_user: None,
        };
        assert!(status(&f, &su, None));
        let out = f.output();
        assert!(
            out.contains(
                "hub (system, as hgl-ai): running; logs: `journalctl -u hgl-ai-hub.service`"
            ),
            "{out}"
        );
        assert!(
            out.contains("  StandardOutput=append: root opens this log in the server user's home (hgl-ai#63); rerun `hgl-ai server install`"),
            "{out}"
        );
    }

    #[test]
    fn each_root_step_is_confirmed() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux, true);
        f.confirms.set(false);
        let e = install(&f, &opts(false)).unwrap_err().to_string();
        assert!(
            e.contains("stopped before `sudo install -d -o hgl-ai"),
            "{e}"
        );
        assert!(f.runs().is_empty(), "a declined step runs nothing");
        assert!(
            f.output()
                .contains("root step: its home /var/lib/hgl-ai, mode 0700")
        );

        let f = fake(d.path(), Os::Linux, true);
        *f.secrets.borrow_mut() = ["ghp_secret".to_string()].into();
        install(&f, &opts(false)).unwrap();
        let runs = f.runs();
        assert!(
            runs.iter()
                .all(|r| r.starts_with("sudo ") || r.starts_with("sh -c sudo -u hgl-ai cat ")),
            "{runs:?}"
        );
        // Each reads the terminal, for sudo's password; only the token step reads the
        // token, which no command line and no output shows (hgl-ai#90).
        let token = format!(
            "sudo -u hgl-ai sh -c {SECRET_WRITER} sh /var/lib/hgl-ai/.config/hgl-ai/github.token"
        );
        for (_, c) in f.cmds.borrow().iter().filter(|(run, _)| *run) {
            let want = if c.line() == token {
                crate::sys::Input::Secret(b"ghp_secret\n".to_vec())
            } else {
                crate::sys::Input::Terminal
            };
            assert_eq!(c.input, want, "{}", c.line());
            assert!(!format!("{c} {c:?}").contains("ghp_secret"), "{c:?}");
        }
        assert!(!f.output().contains("ghp_secret"), "{}", f.output());
        assert!(
            !runs.iter().any(|r| r.contains("useradd")),
            "the user exists"
        );
        let asks = f.output().matches("ask: Run it?").count();
        assert_eq!(asks, runs.len(), "one confirmation per root step");
        let cfg = load_config(&f, &f.env).unwrap().unwrap();
        assert_eq!(cfg.server_user.unwrap().name, "hgl-ai");
        assert!(
            runs.contains(&token),
            "the token step runs as the server user: {runs:?}"
        );
        assert!(f.output().contains("Next, on this host, as yourself:"));
    }

    #[test]
    fn no_console_service_without_a_console() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux, true);
        let mut o = opts(true);
        o.console = false;
        o.console_url = None;
        let script = install(&f, &o).unwrap().unwrap();
        assert!(!script.contains("hgl-ai-console.service"), "{script}");
    }

    #[test]
    fn a_reinstall_keeps_the_address_and_the_console() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux, true);
        assert_eq!(previous(&f), Previous::default());
        install(&f, &opts(false)).unwrap();
        let p = previous(&f);
        assert_eq!(p.listen.as_deref(), Some("192.0.2.10"));
        assert_eq!(p.port, Some(4222));
        assert_eq!(p.host.as_deref(), Some("hub"));
        assert_eq!(
            p.console_url.as_deref(),
            Some("https://console.example.com")
        );
        assert_eq!(p.user.as_deref(), Some("hgl-ai"));
    }

    #[test]
    fn a_reinstall_keeps_slack_and_skips_existing_slack_tokens() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux, true);
        let mut o = opts(false);
        o.slack = true;
        o.keep_token = true;
        install(&f, &o).unwrap();
        assert!(previous(&f).slack);
        assert!(f.output().contains("Slack bot token"));
        assert!(f.output().contains("Slack app-level token"));
        assert_eq!(f.output().matches("kept (--keep-token)").count(), 3);
        assert_eq!(f.secrets.borrow().len(), 1, "no secret prompt ran");
    }

    /// `script` with the terminal replaced by files under `d`, `answer` its input, and
    /// `stty` by nothing: a test never reads the real terminal.
    fn off_tty(d: &Path, script: &str, answer: &str) -> String {
        let (tin, tout) = (d.join("tty.in"), d.join("tty.out"));
        std::fs::write(&tin, answer).unwrap();
        let q = |p: &Path| shell_quote(&p.display().to_string());
        format!(
            "stty() {{ :; }}\n{}",
            script
                .replace("</dev/tty", &format!("<{}", q(&tin)))
                .replace(">/dev/tty", &format!(">{}", q(&tout)))
        )
    }

    /// Run `script` in `sh` with `sudo` replaced by a function that records its words,
    /// one step per paragraph: what root would run, as the shell parses it. Enter at
    /// every prompt.
    fn sudo_words(d: &Path, script: &str) -> Vec<Vec<String>> {
        sudo_words_answering(d, script, "\n")
    }

    /// [`sudo_words`], `answer` typed at the prompts.
    fn sudo_words_answering(d: &Path, script: &str, answer: &str) -> Vec<Vec<String>> {
        let script = &off_tty(d, script, answer);
        let rec = d.join("sudo.words");
        let _ = std::fs::remove_dir_all(&rec);
        std::fs::create_dir(&rec).unwrap();
        // The two ends of a pipeline can record simultaneously.
        let prog = format!(
            "sudo() {{ f=$(mktemp {r}/step.XXXXXX); printf '%s\\n' \"$@\" >\"$f\"; }}\n{script}\n",
            r = shell_quote(&rec.display().to_string()),
        );
        let parse = std::process::Command::new("sh")
            .args(["-n", "-c", script])
            .output()
            .unwrap();
        assert!(
            parse.status.success(),
            "{}",
            String::from_utf8_lossy(&parse.stderr)
        );
        // Its status is not checked: a step outside `sudo` (writing your credential
        // from what the recorder did not print) may fail here.
        std::process::Command::new("sh")
            .arg("-c")
            .arg(&prog)
            // A decoy home: a `~` left in the script would expand to it.
            .env("HOME", d.join("caller"))
            .output()
            .unwrap();
        std::fs::read_dir(&rec)
            .unwrap()
            .map(|entry| {
                std::fs::read_to_string(entry.unwrap().path())
                    .unwrap()
                    .lines()
                    .map(str::to_string)
                    .collect()
            })
            .collect()
    }

    /// The agents run as their own OS user: the client credential goes to it, the
    /// script quotes every path, and nothing is handed to the installer.
    #[test]
    fn a_separate_client_user() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux, false);
        let mut o = opts(true);
        o.client_user = Some("agents".into());
        o.home = PathBuf::from("/srv/hgl ai");
        let script = install(&f, &o).unwrap().unwrap();
        assert!(f.runs().is_empty(), "{:?}", f.runs());
        let steps = sudo_words(d.path(), &script);
        let has = |want: &[&str]| steps.iter().any(|s| s == want);
        // The server user reads the credential; the client user writes it.
        assert!(
            has(&[
                "-u",
                "hgl-ai",
                "cat",
                "/srv/hgl ai/.config/hgl-ai/hub.token"
            ]),
            "{steps:#?}"
        );
        assert!(
            has(&[
                "-u",
                "agents",
                "sh",
                "-c",
                CRED_WRITER,
                "sh",
                "/home/agents/.config/hgl-ai",
            ]),
            "{steps:#?}"
        );
        // Root never names the client's home: only the client user writes there.
        for s in &steps {
            if s.iter().any(|w| w.contains("/home/agents")) {
                assert_eq!(s[..2], ["-u", "agents"], "{s:?}");
            }
        }
        assert!(
            !script.contains("sudo install -m 0600"),
            "root copies no credential: {script}"
        );
        // Nothing for the installer: no step makes them an owner or writes into their
        // home (their home is only read: the staged units, their hgl-ai).
        for s in &steps {
            assert!(!s.iter().any(|w| w == "owner"), "{s:?}");
            let dest = Path::new(s.last().unwrap());
            assert!(!dest.starts_with(&f.env.home), "{s:?}");
        }
        // The server user's step: absolute token path under its home, one word each.
        let inner = steps
            .iter()
            .find(|s| s.contains(&"--no-services".to_string()))
            .expect("the server user's step");
        assert!(inner.contains(&"HOME=/srv/hgl ai".to_string()), "{inner:?}");
        let at = inner
            .iter()
            .position(|w| w == "--github-token-file")
            .unwrap();
        assert_eq!(inner[at + 1], "/srv/hgl ai/.config/hgl-ai/github.token");
        assert!(
            !steps.iter().flatten().any(|w| w.starts_with('~')
                || w.contains(&d.path().join("caller").display().to_string())),
            "no `~` reaches the shell"
        );
        assert!(
            script.contains("# next, as agents: hgl-ai client install"),
            "{script}"
        );

        // Run, it records the client user; a reinstall without the flag keeps it.
        let f = fake(d.path(), Os::Linux, true);
        let mut o = opts(false);
        o.client_user = Some("agents".into());
        install(&f, &o).unwrap();
        let runs = f.runs();
        assert!(
            runs.iter()
                .any(|r| r.starts_with("sh -c sudo -u hgl-ai cat ")
                    && r.contains(" | sudo -u agents sh -c ")),
            "{runs:?}"
        );
        assert!(
            !f.env.home.join(".config/hgl-ai/hub.token").exists(),
            "the installer gets no credential"
        );
        assert!(f.output().contains("Next, on this host, as agents:"));
        assert_eq!(previous(&f).client_user.as_deref(), Some("agents"));
    }

    fn uid() -> String {
        let o = std::process::Command::new("id").arg("-u").output().unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// The client user plants symlinks in its home before a reinstall. Run the script
    /// with `sudo -u` as this (unprivileged) user: the credential's two ends run, every
    /// other step is only recorded. Nothing outside the client's home changes.
    #[test]
    fn a_planted_symlink_reaches_nothing() {
        if uid() == "0" {
            eprintln!("skipped: needs an unprivileged user");
            return;
        }
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let d = tempfile::tempdir().unwrap();
        let srv = d.path().join("srv");
        let cred = srv.join(".config/hgl-ai/hub.token");
        std::fs::create_dir_all(cred.parent().unwrap()).unwrap();
        std::fs::write(&cred, "seat-credential\n").unwrap();
        let run = |client: &Path| {
            let f = Fake::new(d.path(), Os::Linux);
            f.tool("nats-server");
            let c = client.display().to_string();
            f.on(move |cmd| {
                let a: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
                match (cmd.program.as_str(), a.as_slice()) {
                    ("id", ["-un"]) => Output::success("owner\n"),
                    ("id", ["-u", _]) => Output::success("990\n"),
                    ("getent", ["passwd", "agents"]) => {
                        Output::success(format!("agents:x:1001:1001::{c}:/bin/sh\n"))
                    }
                    _ => Output::success(""),
                }
            });
            let mut o = opts(true);
            o.client_user = Some("agents".into());
            o.home = srv.clone();
            let script = install(&f, &o).unwrap().unwrap();
            let script = off_tty(d.path(), &script, "\n");
            let shim = format!(
                "sudo() {{ if [ \"$1\" = -u ]; then u=$2; shift 2; \
                 case \"$u:$1\" in hgl-ai:cat|agents:sh) \"$@\"; return;; esac; fi; }}\n{script}\n"
            );
            std::process::Command::new("sh")
                .arg("-c")
                .arg(&shim)
                .env("HOME", d.path().join("caller"))
                .output()
                .unwrap()
        };
        let outside = d.path().join("outside");
        let keep = outside.join("keep");
        let fresh = || {
            let _ = std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755));
            let _ = std::fs::remove_dir_all(&outside);
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(&keep, "outside\n").unwrap();
        };
        let untouched = || {
            assert_eq!(std::fs::read_to_string(&keep).unwrap(), "outside\n");
            let names: Vec<_> = std::fs::read_dir(&outside)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, ["keep"], "nothing new outside");
        };

        // `.config` points at a directory the client user may not write.
        fresh();
        let home = d.path().join("a");
        std::fs::create_dir_all(&home).unwrap();
        symlink(&outside, home.join(".config")).unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o555)).unwrap();
        run(&home);
        untouched();
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o555
        );

        // `hub.token` points at a file outside: it is replaced, not followed.
        fresh();
        let home = d.path().join("b");
        let dir = home.join(".config/hgl-ai");
        std::fs::create_dir_all(&dir).unwrap();
        symlink(&keep, dir.join("hub.token")).unwrap();
        run(&home);
        untouched();
        let tok = dir.join("hub.token");
        assert!(!std::fs::symlink_metadata(&tok).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(&tok).unwrap(), "seat-credential\n");
        assert_eq!(crate::sys::file_mode(&tok).unwrap(), 0o600);

        // A new home: its directories 0700, the credential 0600.
        let home = d.path().join("c");
        std::fs::create_dir_all(&home).unwrap();
        run(&home);
        let tok = home.join(".config/hgl-ai/hub.token");
        assert_eq!(std::fs::read_to_string(&tok).unwrap(), "seat-credential\n");
        assert_eq!(crate::sys::file_mode(&tok).unwrap(), 0o600);
        for p in [home.join(".config"), home.join(".config/hgl-ai")] {
            assert_eq!(crate::sys::file_mode(&p).unwrap(), 0o700, "{}", p.display());
        }
        // An empty read (the server user could not read it) keeps the old one.
        std::fs::remove_file(&cred).unwrap();
        run(&home);
        assert_eq!(std::fs::read_to_string(&tok).unwrap(), "seat-credential\n");
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_client_user_is_checked() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux, true);
        let mut o = opts(true);
        o.client_user = Some("nobody-here".into());
        let e = install(&f, &o).unwrap_err().to_string();
        assert!(e.contains("--client-user nobody-here: no such user"), "{e}");
        o.client_user = Some("hgl-ai".into());
        let e = install(&f, &o).unwrap_err().to_string();
        assert!(e.contains("is the server user"), "{e}");
        // The installer named as the client user: the plain install.
        o.client_user = Some("owner".into());
        let script = install(&f, &o).unwrap().unwrap();
        assert!(
            script.contains("sudo -u hgl-ai cat /var/lib/hgl-ai/.config/hgl-ai/hub.token | sh -c ")
        );
    }

    /// One `nats-server`, chosen once: the unit and the server user's step agree.
    #[test]
    fn nats_server_is_chosen_once() {
        let d = tempfile::tempdir().unwrap();
        let hub_unit = |f: &Fake| {
            std::fs::read_to_string(
                f.env
                    .home
                    .join(".local/share/hgl-ai/system-units/hgl-ai-hub.service"),
            )
            .unwrap()
        };
        // Installed system-wide: both use it.
        let f = fake(d.path(), Os::Linux, true);
        f.tool_at("nats-server", Path::new("/usr/local/bin"));
        let script = install(&f, &opts(true)).unwrap().unwrap();
        assert!(
            script.contains("--nats-server /usr/local/bin/nats-server "),
            "{script}"
        );
        assert!(hub_unit(&f).contains("ExecStart=/usr/local/bin/nats-server -c "));
        // Only under the installer's home, which the server user cannot reach: the
        // pinned release in its data directory, for both.
        let mut f = fake(d.path(), Os::Linux, true);
        let mine = f.env.home.join(".local/bin");
        f.env.path_var = format!("{}:/usr/local/bin:/usr/bin", mine.display());
        f.tool_at("nats-server", &mine);
        let script = install(&f, &opts(true)).unwrap().unwrap();
        let ours = "/var/lib/hgl-ai/.local/share/hgl-ai/bin/nats-server";
        assert!(
            script.contains(&format!("--nats-server {ours} ")),
            "{script}"
        );
        assert!(hub_unit(&f).contains(&format!("ExecStart={ours} -c ")));
    }

    /// The coordinator's token: asked here without echo, piped to the server user, who
    /// writes it 0600. Nothing under `sudo` reads the terminal (hgl-ai#90).
    #[test]
    fn the_token_step() {
        let d = tempfile::tempdir().unwrap();
        let token = "sudo -u hgl-ai sh -c umask 077; f=$1;";
        let ran = |f: &Fake| {
            f.runs()
                .into_iter()
                .filter(|r| r.starts_with(token))
                .count()
        };
        // An answer goes on the writer's input, never on a command line.
        let f = fake(d.path(), Os::Linux, true);
        *f.secrets.borrow_mut() = ["  ghp_secret \n".to_string()].into();
        install(&f, &opts(false)).unwrap();
        assert_eq!(ran(&f), 1, "{:?}", f.runs());
        let out = f.output();
        assert!(out.contains(&format!("secret: {TOKEN_PROMPT}")), "{out}");
        assert!(out.contains("Actions, Commit statuses: read"), "{out}");
        let runs = f.runs();
        let at = |p: &str| runs.iter().position(|r| r.starts_with(p)).unwrap();
        assert!(at(token) < at("sudo systemctl restart hgl-ai-coord.service"));
        // Enter keeps the current one: nothing runs for it.
        let f = fake(d.path(), Os::Linux, true);
        install(&f, &opts(false)).unwrap();
        assert_eq!(ran(&f), 0, "{:?}", f.runs());
        assert!(
            f.output().contains("kept the current one"),
            "{}",
            f.output()
        );
        assert!(
            f.runs()
                .contains(&"sudo systemctl restart hgl-ai-coord.service".to_string()),
            "the steps after it run"
        );
        // `--keep-token`: not even asked.
        let f = fake(d.path(), Os::Linux, true);
        let mut o = opts(false);
        o.keep_token = true;
        install(&f, &o).unwrap();
        assert_eq!(ran(&f), 0);
        let out = f.output();
        assert!(!out.contains("secret:"), "{out}");
        assert!(out.contains("token: kept (--keep-token)"), "{out}");
        // No terminal to ask on: stopped, naming the way round.
        let f = fake(d.path(), Os::Linux, true);
        f.secrets.borrow_mut().clear();
        let e = install(&f, &opts(false)).unwrap_err().to_string();
        assert!(e.contains("--keep-token"), "{e}");
        assert!(
            !f.runs()
                .contains(&"sudo systemctl restart hgl-ai-coord.service".to_string()),
            "{:?}",
            f.runs()
        );
        // No coordinator, no token step.
        let f = fake(d.path(), Os::Linux, true);
        let mut o = opts(false);
        o.coord = false;
        install(&f, &o).unwrap();
        assert!(!f.output().contains("secret:"), "{}", f.output());
    }

    /// Printed, the script's own shell reads the token at the terminal and pipes it to
    /// the server user's writer; Enter keeps the file.
    #[test]
    fn the_printed_token_step() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux, true);
        let script = install(&f, &opts(true)).unwrap().unwrap();
        let writer = [
            "-u",
            "hgl-ai",
            "sh",
            "-c",
            SECRET_WRITER,
            "sh",
            "/var/lib/hgl-ai/.config/hgl-ai/github.token",
        ];
        let steps = sudo_words_answering(d.path(), &script, "ghp_secret\n");
        assert!(steps.iter().any(|s| s == &writer), "{steps:#?}");
        assert!(
            !steps.iter().flatten().any(|w| w.contains("ghp_secret")),
            "the token is on no command line"
        );
        assert!(
            !steps.iter().flatten().any(|w| w.contains("/dev/tty")),
            "nothing under sudo reads the terminal: {steps:#?}"
        );
        let steps = sudo_words(d.path(), &script);
        assert!(!steps.iter().any(|s| s == &writer), "Enter: no writer");
        let i = |needle: &str| script.find(needle).unwrap();
        assert!(i("github.token\n") < i("restart hgl-ai-coord.service"));
        assert!(script.contains("Actions, Commit statuses: read"));
        assert!(script.contains("Enter keeps the current one"));
        let mut o = opts(true);
        o.keep_token = true;
        let script = install(&f, &o).unwrap().unwrap();
        assert!(!script.contains("github.token\n"), "{script}");
        assert!(script.contains("# the coordinator's GitHub token: kept (--keep-token)"));

        // The prompt and writer themselves, the writer run as this user.
        let srv = d.path().join("srv");
        let file = srv.join(".config/hgl-ai/github.token");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut o = opts(true);
        o.home = srv.clone();
        let script = install(&f, &o).unwrap().unwrap();
        let run = |answer: &str| {
            let shim = format!(
                "sudo() {{ if [ \"$3\" = sh ]; then shift 2; \"$@\"; fi; }}\n{}\n",
                off_tty(d.path(), &script, answer)
            );
            std::process::Command::new("sh")
                .arg("-c")
                .arg(&shim)
                .env("HOME", d.path().join("caller"))
                .output()
                .unwrap()
        };
        run("ghp_secret\n");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ghp_secret\n");
        assert_eq!(crate::sys::file_mode(&file).unwrap(), 0o600);
        run("\n");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ghp_secret\n");
        let names: Vec<_> = std::fs::read_dir(file.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["github.token"], "no temporary left");
    }

    /// The server user's writer: from its input through a temporary file, 0600; an
    /// empty input fails and keeps the file.
    #[test]
    fn the_secret_writer() {
        use std::io::Write as _;
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("github.token");
        let write = |input: &[u8]| {
            let mut c = std::process::Command::new("sh")
                .args(["-c", SECRET_WRITER, "sh", &file.display().to_string()])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            c.stdin.take().unwrap().write_all(input).unwrap();
            c.wait().unwrap().success()
        };
        assert!(write(b"one\n"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
        assert_eq!(crate::sys::file_mode(&file).unwrap(), 0o600);
        assert!(!write(b""));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn the_printed_prompt_signal_test_handles_inherited_ignored_hup() {
        use std::os::unix::process::CommandExt as _;

        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "system::tests::the_printed_prompt_restores_echo_on_a_signal",
        ]);
        // SAFETY: signal is async-signal-safe and only affects the child.
        unsafe {
            command.pre_exec(|| {
                if libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "child test failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    /// A signal at the printed prompt turns echo back on and stops the script.
    #[test]
    fn the_printed_prompt_restores_echo_on_a_signal() {
        use std::os::unix::process::CommandExt as _;
        // Always reap both process groups, including when an assertion fails.
        struct ProcessGroup(std::process::Child);
        impl Drop for ProcessGroup {
            fn drop(&mut self) {
                // SAFETY: the child was started as the leader of its own group.
                unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
                let _ = self.0.wait();
            }
        }
        let d = tempfile::tempdir().unwrap();
        let fifo = d.path().join("tty.in");
        let log = d.path().join("stty.log");
        let after = d.path().join("after");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let q = |p: &Path| shell_quote(&p.display().to_string());
        for (name, sig) in [
            ("INT", libc::SIGINT),
            ("TERM", libc::SIGTERM),
            ("HUP", libc::SIGHUP),
        ] {
            let _ = std::fs::remove_file(&log);
            let prog = format!(
                "stty() {{ echo \"stty $*\" >>{}; }}\n{}\ntouch {}\n",
                q(&log),
                SECRET_PROMPT
                    .replace("PROMPT", "x")
                    .replace("</dev/tty", &format!("<{}", q(&fifo)))
                    .replace(">/dev/tty", ">/dev/null"),
                q(&after)
            );
            // Its own group, as at a terminal: the key's signal reaches every process
            // of the script.
            let mut command = std::process::Command::new("sh");
            command.arg("-c").arg(&prog).process_group(0);
            // SAFETY: only async-signal-safe operations run between fork and exec.
            unsafe {
                command.pre_exec(|| {
                    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                        if libc::signal(sig, libc::SIG_DFL) == libc::SIG_ERR {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    let mut mask = std::mem::zeroed();
                    libc::sigemptyset(&mut mask);
                    if libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut reader = ProcessGroup(command.spawn().unwrap());
            // A writer that keeps the terminal stand-in open and silent.
            let _writer = ProcessGroup(
                std::process::Command::new("sh")
                    .arg("-c")
                    .arg(format!("exec 3>{}; sleep 30", q(&fifo)))
                    .process_group(0)
                    .spawn()
                    .unwrap(),
            );
            let wait_log = |want: &dyn Fn(&str) -> bool, what: &str| {
                let start = Instant::now();
                while !std::fs::read_to_string(&log).is_ok_and(|l| want(&l)) {
                    assert!(start.elapsed() < Duration::from_secs(10), "{name}: {what}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            };
            wait_log(&|l| l.contains("stty -echo"), "no prompt");
            // `read` is waiting on the terminal now.
            std::thread::sleep(Duration::from_millis(200));
            // SAFETY: a plain kill(2) of the reader's group.
            assert_eq!(
                unsafe { libc::kill(-(reader.0.id() as i32), sig) },
                0,
                "{name}"
            );
            let start = Instant::now();
            let st = loop {
                if let Some(st) = reader.0.try_wait().unwrap() {
                    break st;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "{name}: did not exit"
                );
                std::thread::sleep(Duration::from_millis(20));
            };
            assert!(!st.success(), "{name}: {st:?}");
            wait_log(&|l| l.trim_end().ends_with("stty echo"), "echo stays off");
            assert!(!after.exists(), "{name}: the script went on");
        }
    }

    #[test]
    fn macos_launch_daemons() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::MacOs, false);
        let mut o = opts(true);
        o.user = "_hgl-ai".into();
        o.home = PathBuf::from("/Library/hgl-ai");
        let script = install(&f, &o).unwrap().unwrap();
        assert!(
            script.contains("sudo sysadminctl -addUser _hgl-ai -roleAccount -home /Library/hgl-ai")
        );
        assert!(script.contains("/Library/LaunchDaemons/io.github.hhenson.hgl-ai.hub.plist"));
        assert!(script.contains("sudo launchctl bootstrap system"));
        let plist = std::fs::read_to_string(
            f.env
                .home
                .join(".local/share/hgl-ai/system-units/io.github.hhenson.hgl-ai.coord.plist"),
        )
        .unwrap();
        assert!(
            plist.contains("<key>UserName</key>\n  <string>_hgl-ai</string>"),
            "{plist}"
        );
        assert!(
            plist.contains("<key>HOME</key>\n    <string>/Library/hgl-ai</string>"),
            "{plist}"
        );
        // launchd opens the logs as root: in a directory root owns, made before the
        // daemons (hgl-ai#63).
        assert!(
            plist.contains(
                "<key>StandardOutPath</key>\n  <string>/Library/Logs/hgl-ai/coord.log</string>"
            ),
            "{plist}"
        );
        assert!(!plist.contains("/Library/hgl-ai/.local"), "{plist}");
        let dir = script
            .find("sudo install -d -o root -g wheel -m 0755 /Library/Logs/hgl-ai\n")
            .expect(&script);
        assert!(dir < script.find("LaunchDaemons").unwrap(), "{script}");
    }

    /// A reinstall over loaded daemons (the upgrade from hgl-ai#63) unloads each before
    /// loading its new plist; a first install has nothing to unload.
    #[test]
    fn macos_reinstall_unloads_the_loaded_daemons() {
        let d = tempfile::tempdir().unwrap();
        let mut o = opts(true);
        o.user = "_hgl-ai".into();
        o.home = PathBuf::from("/Library/hgl-ai");
        for loaded in [true, false] {
            let f = fake(d.path(), Os::MacOs, true);
            if !loaded {
                f.on(|c| match c.args.first().map(String::as_str) {
                    Some("print") => Output::failure(113, "Could not find service"),
                    Some("-un") => Output::success("owner\n"),
                    _ => Output::success(""),
                });
            }
            let script = install(&f, &o).unwrap().unwrap();
            let out = "sudo launchctl bootout system/io.github.hhenson.hgl-ai.hub\n";
            let load = "sudo launchctl bootstrap system /Library/LaunchDaemons/io.github.hhenson.hgl-ai.hub.plist";
            match script.find(out) {
                Some(i) => {
                    assert!(loaded, "{script}");
                    assert!(i < script.find(load).unwrap(), "{script}");
                }
                None => assert!(!loaded, "{script}"),
            }
        }
    }
}
