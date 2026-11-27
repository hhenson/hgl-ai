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

use crate::env::Os;
use crate::service::{self, Service, Spec};
use crate::sys::{Cmd, System};
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
    pub console: bool,
    pub wait: Duration,
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
    /// Show `cmd`, then run it with `sudo` once confirmed (or only print it).
    fn step(&mut self, why: &str, cmd: Cmd) -> Result<()> {
        let line = format!("sudo {}", cmd.line());
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
        let mut sudo = Cmd::new("sudo").arg(cmd.program.clone());
        sudo = sudo.args(cmd.args.iter().cloned());
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

/// `nats-server` for the hub unit: on `PATH` outside the owner's home, else the pinned
/// release the server user's step downloads into its data directory.
fn nats_for(sys: &dyn System, layout: &Layout) -> PathBuf {
    sys.which("nats-server")
        .filter(|p| !p.starts_with(&sys.env().home))
        .unwrap_or_else(|| layout.data().join("bin/nats-server"))
}

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
    let mut out = vec![Spec::hub(
        &nats_for(sys, &layout),
        &data.join("nats.conf"),
        &data,
    )];
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
        log: service::log_path(&data, s),
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
    let group = sys
        .query(&Cmd::new("id").arg("-gn"))
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
    let mut inner = sudo_as(user, &layout, &path, &exe, &[]).args([
        "server",
        "install",
        "--same-user",
        "--no-services",
        "--no-brew",
        "--units",
        "system",
        "--github-token-file",
        "~/.config/hgl-ai/github.token",
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
            service::log_path(&layout.data(), Service::Hub).display()
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
    for s in names.iter().copied().filter(|s| *s != Service::Hub) {
        start(&mut root, s)?;
    }
    // The hub host's own client is the owner's: its seat credential comes over to them.
    let mine = env.client_token_path();
    if !o.print_only {
        let dir = env.config_dir();
        sys.mkdir(&dir).map_err(io_at(&dir))?;
    }
    root.step(
        "this host's client credential, for you (mode 0600)",
        Cmd::new("install").args([
            "-m",
            "0600",
            "-o",
            &owner,
            "-g",
            &group,
            &layout.client_cred().display().to_string(),
            &mine.display().to_string(),
        ]),
    )?;
    if o.print_only {
        return Ok(Some(root.script.join("\n")));
    }
    // The client's configuration names the server user, for `server status` and `doctor`.
    let mut cfg =
        load_config(sys, env)?.unwrap_or_else(|| hgl_ai_config::Config::new(o.host.clone()));
    cfg.server_user = Some(hgl_ai_config::ServerUser {
        name: user.to_string(),
        home: home.clone(),
    });
    cfg.hub.url = crate::server::local_url(&o.listen, o.port);
    save_config(sys, env, &cfg)?;
    sys.say("");
    sys.say("Next, on this host:");
    sys.say(&format!(
        "  hgl-ai client install --hub {} --token-file {} --host {}",
        cfg.hub.url,
        env.contract(&mine),
        o.host
    ));
    if o.coord {
        sys.say(&format!(
            "The coordinator reads GitHub with its own token: make a fine-grained, read-only token \
             (Metadata, Contents, Issues, Pull requests, Checks: read) for the repositories, and write it as {user}:"
        ));
        sys.say(&format!(
            "  sudo -u {user} sh -c 'umask 077; cat > {}'",
            layout.github_token().display()
        ));
    }
    Ok(None)
}

/// The server side's services, read without privilege; `true` when all run and the
/// port answers.
pub fn status(
    sys: &dyn System,
    su: &hgl_ai_config::ServerUser,
    listen_port: Option<(String, u16)>,
) -> bool {
    let mut up = true;
    for s in [Service::Hub, Service::Coord, Service::Console] {
        let st = service::system_state(sys, s);
        let text = match st {
            service::State::Running => "running",
            service::State::Stopped => "stopped",
            service::State::NotInstalled => "not installed",
        };
        up &= st == service::State::Running
            || (s != Service::Hub && st == service::State::NotInstalled);
        sys.say(&format!("{} (system, as {}): {text}", s.short(), su.name));
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
    let mut svcs = vec![Service::Hub, Service::Coord, Service::Console];
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
    for s in [Service::Console, Service::Coord, Service::Hub] {
        let path = service::system_unit_path(os, s).display().to_string();
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
                ("id", ["-u", _]) if user_exists => Output::success("990\n"),
                ("id", ["-u", _]) => Output::failure(1, "no such user"),
                _ => Output::success(""),
            }
        });
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
            wait: Duration::ZERO,
        }
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
            "server install --same-user --no-services --no-brew --units system --github-token-file ~/.config/hgl-ai/github.token --host hub --listen 192.0.2.10",
            "sudo install -m 0644 ",
            "/etc/systemd/system/hgl-ai-hub.service",
            "sudo systemctl daemon-reload",
            "sudo systemctl restart hgl-ai-hub.service",
            "# wait until the hub answers on 192.0.2.10:4222",
            "HGL_AI_HUB_CREDENTIAL=/var/lib/hgl-ai/.config/hgl-ai/hub-users/hub-admin.cred",
            " init",
            "sudo systemctl restart hgl-ai-console.service",
            "sudo install -m 0600 -o owner -g owner /var/lib/hgl-ai/.config/hgl-ai/hub.token",
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
        install(&f, &opts(false)).unwrap();
        let runs = f.runs();
        assert!(runs.iter().all(|r| r.starts_with("sudo ")), "{runs:?}");
        assert!(
            !runs.iter().any(|r| r.contains("useradd")),
            "the user exists"
        );
        let asks = f.output().matches("ask: Run it?").count();
        assert_eq!(asks, runs.len(), "one confirmation per root step");
        let cfg = load_config(&f, &f.env).unwrap().unwrap();
        assert_eq!(cfg.server_user.unwrap().name, "hgl-ai");
        assert!(f.output().contains(
            "sudo -u hgl-ai sh -c 'umask 077; cat > /var/lib/hgl-ai/.config/hgl-ai/github.token'"
        ));
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
    }
}
