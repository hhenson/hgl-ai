//! User services (docs/design.md §6): `systemd --user` units on Linux, launchd
//! agents on macOS.

use std::path::{Path, PathBuf};

use crate::env::{Env, Os};
use crate::sys::{Cmd, System};
use crate::{Result, io_at, other, run_ok};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Service {
    Hub,
    Bridge,
    Coord,
    Console,
    Builder,
}

impl Service {
    pub const ALL: [Service; 5] = [
        Service::Hub,
        Service::Bridge,
        Service::Coord,
        Service::Console,
        Service::Builder,
    ];

    pub fn short(self) -> &'static str {
        match self {
            Service::Hub => "hub",
            Service::Bridge => "bridge",
            Service::Coord => "coord",
            Service::Console => "console",
            Service::Builder => "builder",
        }
    }
    /// `hgl-ai-<short>.service`
    pub fn unit(self) -> String {
        format!("hgl-ai-{}.service", self.short())
    }
    /// `io.github.hhenson.hgl-ai.<short>`
    pub fn label(self) -> String {
        format!("io.github.hhenson.hgl-ai.{}", self.short())
    }
    fn description(self) -> &'static str {
        match self {
            Service::Hub => "hgl-ai hub (NATS JetStream)",
            Service::Bridge => "hgl-ai bridge",
            Service::Coord => "hgl-ai coordinator",
            Service::Console => "hgl-ai owner console",
            Service::Builder => "hgl-ai builder",
        }
    }
}

/// What one service runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub service: Service,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The file its output is appended to; `None`: the journal (systemd) or nowhere
    /// (launchd).
    pub log: Option<PathBuf>,
}

impl Spec {
    /// The hub: `nats-server -c <data_dir>/nats.conf`.
    pub fn hub(nats_server: &Path, conf: &Path, data_dir: &Path) -> Spec {
        Spec {
            service: Service::Hub,
            program: nats_server.to_path_buf(),
            args: vec!["-c".into(), conf.display().to_string()],
            env: Vec::new(),
            log: Some(log_path(data_dir, Service::Hub)),
        }
    }

    /// `hgl-ai bridge run`, `hgl-ai gh coord run` or `hgl-ai console run`, with the
    /// install-time `PATH` (agent CLIs, `git`, `gh`) and the configuration in use.
    pub fn hgl_ai(service: Service, sys: &dyn System, data_dir: &Path) -> Spec {
        let env = sys.env();
        let exe = env.service_exe(sys);
        let args = match service {
            Service::Coord => vec!["gh", "coord", "run"],
            Service::Console => vec!["console", "run"],
            Service::Builder => vec!["builder", "run"],
            _ => vec!["bridge", "run"],
        };
        Spec {
            service,
            env: vec![
                ("PATH".into(), env.service_path(&exe)),
                (
                    "HGL_AI_CONFIG".into(),
                    env.config_path.display().to_string(),
                ),
            ],
            program: exe,
            args: args.into_iter().map(String::from).collect(),
            log: Some(log_path(data_dir, service)),
        }
    }
}

impl Spec {
    /// The service connects to the hub as its own hub user (`HGL_AI_HUB_CREDENTIAL`).
    pub fn with_credential(mut self, cred: Option<&Path>) -> Spec {
        if let Some(c) = cred {
            self.env.push((
                hgl_ai_config::CREDENTIAL_ENV.to_string(),
                c.display().to_string(),
            ));
        }
        self
    }
}

/// A system unit (`/etc/systemd/system`) or launch daemon (`/Library/LaunchDaemons`):
/// the server side run as its own OS user (docs/install.md).
pub fn system_unit_path(os: Os, s: Service) -> PathBuf {
    match os {
        Os::Linux => PathBuf::from("/etc/systemd/system").join(s.unit()),
        Os::MacOs => PathBuf::from("/Library/LaunchDaemons").join(format!("{}.plist", s.label())),
    }
}

/// Where a launch daemon's output goes: a directory root owns, so the server user
/// cannot plant a symlink that root (which opens `StandardOutPath`) would follow
/// (hgl-ai#63). Linux system units log to the journal.
pub const DAEMON_LOG_DIR: &str = "/Library/Logs/hgl-ai";

/// A system service's log file: macOS only.
pub fn system_log_path(os: Os, s: Service) -> Option<PathBuf> {
    match os {
        Os::Linux => None,
        Os::MacOs => Some(Path::new(DAEMON_LOG_DIR).join(format!("{}.log", s.short()))),
    }
}

/// How to read a system service's log, for messages.
pub fn system_log_hint(os: Os, s: Service) -> String {
    match os {
        Os::Linux => format!("`journalctl -u {}`", s.unit()),
        Os::MacOs => format!("`sudo tail {DAEMON_LOG_DIR}/{}.log`", s.short()),
    }
}

/// An installed system service's output setting, when it is a file root opens outside
/// a directory only root writes (an install before hgl-ai#63): `StandardOutput=append`,
/// `stdout path = <file>`. Read without privilege; `None` when it is safe, not
/// installed, or unknown.
pub fn system_log_unsafe(sys: &dyn System, s: Service) -> Option<String> {
    match sys.env().os {
        Os::Linux => {
            let o = sys
                .query(&Cmd::new("systemctl").args([
                    "show",
                    "-p",
                    "StandardOutput,StandardError",
                    &s.unit(),
                ]))
                .ok()
                .filter(|o| o.ok())?;
            o.stdout
                .lines()
                .filter_map(|l| l.trim().split_once('='))
                .find(|(_, v)| matches!(*v, "append" | "file" | "truncate"))
                .map(|(k, v)| format!("{k}={v}"))
        }
        Os::MacOs => {
            let o = sys
                .query(
                    &Cmd::new("launchctl")
                        .arg("print")
                        .arg(format!("system/{}", s.label())),
                )
                .ok()
                .filter(|o| o.ok())?;
            let dir = Path::new(DAEMON_LOG_DIR);
            o.stdout
                .lines()
                .map(str::trim)
                .find(|l| match l.split_once(" = ") {
                    Some(("stdout path" | "stderr path", p)) => Path::new(p).parent() != Some(dir),
                    _ => false,
                })
                .map(str::to_string)
        }
    }
}

/// A system unit that runs `spec` as `user` (and `group`) with `HOME=home`. Its output
/// goes to the journal, whatever `spec.log` says: systemd opens an `append:` file as
/// root, and in the server user's directory that follows a symlink it plants
/// (hgl-ai#63).
pub fn render_systemd_system(spec: &Spec, user: &str, group: &str, home: &Path) -> String {
    let user_unit = render_systemd(&Spec {
        log: None,
        ..spec.clone()
    });
    let mut s = String::new();
    for line in user_unit.lines() {
        match line {
            "[Service]" => {
                s.push_str("[Service]\n");
                s.push_str(&format!("User={user}\nGroup={group}\nUMask=0077\n"));
                s.push_str(&format!("Environment=\"HOME={}\"\n", home.display()));
            }
            "[Unit]" => s.push_str("[Unit]\nAfter=network-online.target\n"),
            "WantedBy=default.target" => s.push_str("WantedBy=multi-user.target\n"),
            l => {
                s.push_str(l);
                s.push('\n');
            }
        }
    }
    // `Description=` follows `[Unit]` after the ordering line.
    s
}

/// A launch daemon that runs `spec` as `user` with `HOME=home`. Its output goes to
/// [`DAEMON_LOG_DIR`], whatever `spec.log` says: launchd opens `StandardOutPath` as
/// root (hgl-ai#63).
pub fn render_launchd_daemon(spec: &Spec, user: &str, home: &Path) -> String {
    let mut sp = spec.clone();
    sp.log = system_log_path(Os::MacOs, spec.service);
    sp.env
        .insert(0, ("HOME".into(), home.display().to_string()));
    let agent = render_launchd(&sp);
    agent.replacen(
        "  <key>ProgramArguments</key>",
        &format!(
            "  <key>UserName</key>\n  <string>{}</string>\n  <key>Umask</key>\n  <integer>63</integer>\n  <key>ProgramArguments</key>",
            xml(user)
        ),
        1,
    )
}

/// The OS user an installed system service runs as (`User=`, `UserName`) and the
/// `HOME` it gives it (`Environment=HOME=`, `EnvironmentVariables.HOME`), read without
/// privilege; `None` when it is not installed or names no user.
pub fn system_account(sys: &dyn System, s: Service) -> Option<(String, Option<PathBuf>)> {
    let os = sys.env().os;
    let query = |c: Cmd| {
        sys.query(&c)
            .ok()
            .filter(|o| o.ok())
            .map(|o| o.stdout.trim().to_string())
    };
    let (user, home) = match os {
        Os::Linux => {
            let out = query(Cmd::new("systemctl").args([
                "show",
                "-p",
                "LoadState,User,Environment",
                &s.unit(),
            ]))?;
            let field = |k: &str| {
                out.lines()
                    .find_map(|l| l.trim().strip_prefix(k))
                    .map(str::to_string)
            };
            if field("LoadState=").as_deref() != Some("loaded") {
                return None;
            }
            let home = field("Environment=").and_then(|e| {
                e.split_whitespace()
                    .find_map(|w| w.strip_prefix("HOME="))
                    .map(str::to_string)
            });
            (field("User=")?, home)
        }
        Os::MacOs => {
            let plist = system_unit_path(os, s).display().to_string();
            let key = |k: &str| {
                query(
                    Cmd::new("plutil")
                        .args(["-extract", k, "raw", "-o", "-"])
                        .arg(plist.clone()),
                )
            };
            (key("UserName")?, key("EnvironmentVariables.HOME"))
        }
    };
    let home = home.filter(|h| h.starts_with('/')).map(PathBuf::from);
    (!user.is_empty()).then_some((user, home))
}

/// The `PATH` a user service's unit or agent records (the install-time `PATH`: agent
/// CLIs, `git`, `gh`); `None` when it is not installed or records none.
pub fn recorded_path(sys: &dyn System, s: Service) -> Option<String> {
    let env = sys.env();
    let text = sys.read_to_string(&unit_path(env, s)).ok()?;
    let path = match env.os {
        Os::Linux => text.lines().find_map(|l| {
            l.trim()
                .strip_prefix("Environment=\"PATH=")
                .and_then(|r| r.strip_suffix('"'))
                .map(str::to_string)
        }),
        Os::MacOs => {
            let mut lines = text.lines().map(str::trim);
            lines.find(|l| *l == "<key>PATH</key>")?;
            lines.next().and_then(|l| {
                l.strip_prefix("<string>")
                    .and_then(|r| r.strip_suffix("</string>"))
                    .map(|v| {
                        v.replace("&quot;", "\"")
                            .replace("&gt;", ">")
                            .replace("&lt;", "<")
                            .replace("&amp;", "&")
                    })
            })
        }
    };
    path.filter(|p| !p.is_empty())
}

/// A system service's state, read without privilege.
pub fn system_state(sys: &dyn System, s: Service) -> State {
    match sys.env().os {
        Os::Linux => {
            let Ok(o) = sys.query(&Cmd::new("systemctl").args([
                "show",
                "-p",
                "LoadState,ActiveState",
                &s.unit(),
            ])) else {
                return State::NotInstalled;
            };
            let loaded = o.stdout.lines().any(|l| l.trim() == "LoadState=loaded");
            let active = o.stdout.lines().any(|l| l.trim() == "ActiveState=active");
            match (loaded, active) {
                (false, _) => State::NotInstalled,
                (true, true) => State::Running,
                (true, false) => State::Stopped,
            }
        }
        Os::MacOs => {
            let target = format!("system/{}", s.label());
            match sys.query(&Cmd::new("launchctl").args(["print", &target])) {
                Ok(o) if o.ok() => {
                    if o.stdout.lines().any(|l| l.trim() == "state = running") {
                        State::Running
                    } else {
                        State::Stopped
                    }
                }
                _ => State::NotInstalled,
            }
        }
    }
}

pub fn log_path(data_dir: &Path, s: Service) -> PathBuf {
    data_dir.join("logs").join(format!("{}.log", s.short()))
}

/// Where the unit or agent file lives.
pub fn unit_path(env: &Env, s: Service) -> PathBuf {
    match env.os {
        Os::Linux => env.xdg_config_home.join("systemd/user").join(s.unit()),
        Os::MacOs => env
            .home
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", s.label())),
    }
}

/// Quote one `ExecStart` word for systemd.
fn systemd_word(s: &str) -> String {
    let s = s.replace('%', "%%");
    if s.chars()
        .any(|c| c.is_whitespace() || "\"'\\;$".contains(c))
    {
        format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
    } else {
        s
    }
}

pub fn render_systemd(spec: &Spec) -> String {
    let exec: Vec<String> = std::iter::once(spec.program.display().to_string())
        .chain(spec.args.iter().cloned())
        .map(|w| systemd_word(&w))
        .collect();
    let mut s = format!(
        "# Written by hgl-ai; reinstalling rewrites it.\n\
         [Unit]\n\
         Description={}\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={}\n",
        spec.service.description(),
        exec.join(" ")
    );
    for (k, v) in &spec.env {
        let v = v
            .replace('%', "%%")
            .replace('\\', r"\\")
            .replace('"', "\\\"");
        s.push_str(&format!("Environment=\"{k}={v}\"\n"));
    }
    s.push_str("Restart=on-failure\nRestartSec=10\n");
    match &spec.log {
        Some(log) => {
            let log = log.display().to_string().replace('%', "%%");
            s.push_str(&format!(
                "StandardOutput=append:{log}\nStandardError=append:{log}\n"
            ));
        }
        None => s.push_str("StandardOutput=journal\nStandardError=journal\n"),
    }
    s.push_str("\n[Install]\nWantedBy=default.target\n");
    s
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn render_launchd(spec: &Spec) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    s.push_str(&format!(
        "  <key>Label</key>\n  <string>{}</string>\n",
        xml(&spec.service.label())
    ));
    s.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for w in std::iter::once(spec.program.display().to_string()).chain(spec.args.iter().cloned()) {
        s.push_str(&format!("    <string>{}</string>\n", xml(&w)));
    }
    s.push_str("  </array>\n");
    if !spec.env.is_empty() {
        s.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        for (k, v) in &spec.env {
            s.push_str(&format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                xml(k),
                xml(v)
            ));
        }
        s.push_str("  </dict>\n");
    }
    s.push_str(
        "  <key>RunAtLoad</key>\n  <true/>\n\
         \x20 <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n\
         \x20 <key>ThrottleInterval</key>\n  <integer>10</integer>\n",
    );
    if let Some(log) = &spec.log {
        let log = xml(&log.display().to_string());
        s.push_str(&format!(
            "  <key>StandardOutPath</key>\n  <string>{log}</string>\n\
             \x20 <key>StandardErrorPath</key>\n  <string>{log}</string>\n"
        ));
    }
    s.push_str("</dict>\n</plist>\n");
    s
}

fn systemctl(args: &[&str]) -> Cmd {
    Cmd::new("systemctl")
        .arg("--user")
        .args(args.iter().copied())
}

/// `systemctl --user` for a query. Without `XDG_RUNTIME_DIR` and
/// `DBUS_SESSION_BUS_ADDRESS` (a plain `sudo -u NAME`) it cannot find the user's manager:
/// point it at `<runtime_root>/<uid>` when that exists.
fn user_systemctl(sys: &dyn System, args: &[&str]) -> Cmd {
    let cmd = systemctl(args);
    let dir = sys
        .env()
        .runtime_root
        .as_ref()
        .and_then(|root| uid(sys).ok().map(|u| root.join(u)))
        .filter(|d| sys.exists(d));
    match dir {
        Some(d) => {
            let d = d.display().to_string();
            cmd.env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={d}/bus"))
                .env("XDG_RUNTIME_DIR", d)
        }
        None => cmd,
    }
}

/// The user's numeric id, for launchd domains and `loginctl`.
fn uid(sys: &dyn System) -> Result<String> {
    let o = sys
        .query(&Cmd::new("id").arg("-u"))
        .map_err(|e| other(format!("`id -u`: {e}")))?;
    let u = o.stdout.trim().to_string();
    if o.ok() && !u.is_empty() && u.chars().all(|c| c.is_ascii_digit()) {
        Ok(u)
    } else {
        Err(other("`id -u` failed"))
    }
}

fn launchd_target(sys: &dyn System, s: Service) -> Result<(String, String)> {
    let domain = format!("gui/{}", uid(sys)?);
    let target = format!("{domain}/{}", s.label());
    Ok((domain, target))
}

/// Write the unit, then enable and (re)start it.
pub fn install(sys: &dyn System, spec: &Spec) -> Result<()> {
    let env = sys.env();
    let path = unit_path(env, spec.service);
    if let Some(logs) = spec.log.as_deref().and_then(Path::parent) {
        sys.mkdir(logs).map_err(io_at(logs))?;
    }
    match env.os {
        Os::Linux => {
            sys.write(&path, render_systemd(spec).as_bytes(), 0o644)
                .map_err(io_at(&path))?;
            let unit = spec.service.unit();
            run_ok(sys, &systemctl(&["daemon-reload"]))?;
            run_ok(sys, &systemctl(&["enable", &unit]))?;
            run_ok(sys, &systemctl(&["restart", &unit]))?;
        }
        Os::MacOs => {
            sys.write(&path, render_launchd(spec).as_bytes(), 0o644)
                .map_err(io_at(&path))?;
            let (domain, target) = launchd_target(sys, spec.service)?;
            // Unload a previous copy; failure means it was not loaded.
            let _ = sys.run(&Cmd::new("launchctl").args(["bootout", &target]));
            run_ok(
                sys,
                &Cmd::new("launchctl").args(["bootstrap", &domain, &path.display().to_string()]),
            )?;
            run_ok(sys, &Cmd::new("launchctl").args(["enable", &target]))?;
            run_ok(
                sys,
                &Cmd::new("launchctl").args(["kickstart", "-k", &target]),
            )?;
        }
    }
    Ok(())
}

pub fn start(sys: &dyn System, s: Service) -> Result<()> {
    match sys.env().os {
        Os::Linux => run_ok(sys, &systemctl(&["start", &s.unit()])).map(drop),
        Os::MacOs => {
            let (domain, target) = launchd_target(sys, s)?;
            if !loaded(sys, &target) {
                let path = unit_path(sys.env(), s).display().to_string();
                run_ok(
                    sys,
                    &Cmd::new("launchctl").args(["bootstrap", &domain, &path]),
                )?;
            }
            run_ok(sys, &Cmd::new("launchctl").args(["kickstart", &target])).map(drop)
        }
    }
}

/// Make the running hub reread `nats.conf` (users and permissions) without dropping its
/// connections: SIGHUP to the process in its pid file. `false` when there is no pid
/// file (an older `nats.conf`, or the hub is down).
pub fn reload_hub(sys: &dyn System, store_dir: &Path) -> Result<bool> {
    let pid_file = crate::nats::pid_path(store_dir);
    if !sys.exists(&pid_file) {
        return Ok(false);
    }
    let pid = sys.read_to_string(&pid_file).map_err(io_at(&pid_file))?;
    let pid = pid.trim();
    if pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
        return Ok(false);
    }
    // A stale pid file (the hub stopped): no such process, or another program now.
    let alive = sys
        .query(&Cmd::new("ps").args(["-p", pid, "-o", "comm="]))
        .is_ok_and(|o| o.ok() && o.stdout.trim().ends_with("nats-server"));
    if !alive {
        return Ok(false);
    }
    match sys.run(&Cmd::new("kill").args(["-HUP", pid])) {
        Ok(o) if o.ok() => Ok(true),
        // It went between the two.
        _ => Ok(false),
    }
}

/// Restart an installed service (it rereads its configuration and credential).
pub fn restart(sys: &dyn System, s: Service) -> Result<()> {
    match sys.env().os {
        Os::Linux => run_ok(sys, &systemctl(&["restart", &s.unit()])).map(drop),
        Os::MacOs => {
            let (_, target) = launchd_target(sys, s)?;
            run_ok(
                sys,
                &Cmd::new("launchctl").args(["kickstart", "-k", &target]),
            )
            .map(drop)
        }
    }
}

pub fn stop(sys: &dyn System, s: Service) -> Result<()> {
    match sys.env().os {
        Os::Linux => run_ok(sys, &systemctl(&["stop", &s.unit()])).map(drop),
        Os::MacOs => {
            let (_, target) = launchd_target(sys, s)?;
            if loaded(sys, &target) {
                run_ok(sys, &Cmd::new("launchctl").args(["bootout", &target]))?;
            }
            Ok(())
        }
    }
}

/// Stop, disable and delete the unit. Absent is fine.
pub fn uninstall(sys: &dyn System, s: Service) -> Result<()> {
    let path = unit_path(sys.env(), s);
    match sys.env().os {
        Os::Linux => {
            if sys.exists(&path) {
                // Tolerate a unit systemd no longer knows.
                let _ = sys.run(&systemctl(&["disable", "--now", &s.unit()]));
            }
            sys.remove_file(&path).map_err(io_at(&path))?;
            run_ok(sys, &systemctl(&["daemon-reload"]))?;
        }
        Os::MacOs => {
            let (_, target) = launchd_target(sys, s)?;
            if loaded(sys, &target) {
                let _ = sys.run(&Cmd::new("launchctl").args(["bootout", &target]));
            }
            sys.remove_file(&path).map_err(io_at(&path))?;
        }
    }
    Ok(())
}

fn loaded(sys: &dyn System, target: &str) -> bool {
    sys.query(&Cmd::new("launchctl").args(["print", target]))
        .is_ok_and(|o| o.ok())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    NotInstalled,
    Running,
    Stopped,
}

pub fn state(sys: &dyn System, s: Service) -> State {
    if !sys.exists(&unit_path(sys.env(), s)) {
        return State::NotInstalled;
    }
    let running = match sys.env().os {
        Os::Linux => sys
            .query(&user_systemctl(sys, &["is-active", &s.unit()]))
            .is_ok_and(|o| o.stdout.trim() == "active"),
        Os::MacOs => match launchd_target(sys, s) {
            Ok((_, target)) => sys
                .query(&Cmd::new("launchctl").args(["print", &target]))
                .is_ok_and(|o| o.ok() && o.stdout.lines().any(|l| l.trim() == "state = running")),
            Err(_) => false,
        },
    };
    if running {
        State::Running
    } else {
        State::Stopped
    }
}

/// The command that starts a stopped service, for messages.
pub fn start_hint(env: &Env, s: Service) -> String {
    match env.os {
        Os::Linux => format!("systemctl --user start {}", s.unit()),
        Os::MacOs => format!("launchctl kickstart gui/$(id -u)/{}", s.label()),
    }
}

/// Linux only: whether lingering is on, so user services survive logout.
/// `enable` runs `loginctl enable-linger` first. `None` on macOS or when unknown.
pub fn linger(sys: &dyn System, enable: bool) -> Result<Option<bool>> {
    if sys.env().os != Os::Linux {
        return Ok(None);
    }
    if enable {
        run_ok(sys, &Cmd::new("loginctl").arg("enable-linger"))?;
        if sys.dry_run() {
            return Ok(Some(true));
        }
    }
    let Ok(u) = uid(sys) else { return Ok(None) };
    let o =
        sys.query(&Cmd::new("loginctl").args(["show-user", &u, "--property=Linger", "--value"]));
    Ok(match o {
        Ok(o) if o.ok() => Some(o.stdout.trim() == "yes"),
        _ => None,
    })
}

/// A line reporting lingering, with the fix when it is off.
pub fn linger_line(state: Option<bool>) -> Option<String> {
    match state {
        Some(true) => Some("lingering: on (services survive logout)".into()),
        Some(false) => Some(
            "lingering: off; services stop at logout. Run `loginctl enable-linger` or pass --linger".into(),
        ),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Output;
    use crate::sys::fake::Fake;

    fn bridge_spec() -> Spec {
        Spec {
            service: Service::Bridge,
            program: PathBuf::from("/opt/homebrew/opt/hgl-ai/bin/hgl-ai"),
            args: vec!["bridge".into(), "run".into()],
            env: vec![
                (
                    "PATH".into(),
                    "/opt/homebrew/opt/hgl-ai/bin:/usr/bin:/bin".into(),
                ),
                (
                    "HGL_AI_CONFIG".into(),
                    "/home/u/.config/hgl-ai/config.toml".into(),
                ),
            ],
            log: Some(PathBuf::from("/home/u/.local/share/hgl-ai/logs/bridge.log")),
        }
    }

    #[test]
    fn names() {
        assert_eq!(Service::Hub.unit(), "hgl-ai-hub.service");
        assert_eq!(Service::Coord.label(), "io.github.hhenson.hgl-ai.coord");
    }

    #[test]
    fn systemd_golden() {
        assert_eq!(
            render_systemd(&bridge_spec()),
            r#"# Written by hgl-ai; reinstalling rewrites it.
[Unit]
Description=hgl-ai bridge

[Service]
Type=simple
ExecStart=/opt/homebrew/opt/hgl-ai/bin/hgl-ai bridge run
Environment="PATH=/opt/homebrew/opt/hgl-ai/bin:/usr/bin:/bin"
Environment="HGL_AI_CONFIG=/home/u/.config/hgl-ai/config.toml"
Restart=on-failure
RestartSec=10
StandardOutput=append:/home/u/.local/share/hgl-ai/logs/bridge.log
StandardError=append:/home/u/.local/share/hgl-ai/logs/bridge.log

[Install]
WantedBy=default.target
"#
        );
        let hub = Spec::hub(
            Path::new("/my dir/nats-server"),
            Path::new("/d/nats.conf"),
            Path::new("/d"),
        );
        let text = render_systemd(&hub);
        assert!(
            text.contains("ExecStart=\"/my dir/nats-server\" -c /d/nats.conf\n"),
            "{text}"
        );
        assert!(!text.contains("Environment="));
    }

    #[test]
    fn launchd_golden() {
        assert_eq!(
            render_launchd(&bridge_spec()),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>io.github.hhenson.hgl-ai.bridge</string>
  <key>ProgramArguments</key>
  <array>
    <string>/opt/homebrew/opt/hgl-ai/bin/hgl-ai</string>
    <string>bridge</string>
    <string>run</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/opt/homebrew/opt/hgl-ai/bin:/usr/bin:/bin</string>
    <key>HGL_AI_CONFIG</key>
    <string>/home/u/.config/hgl-ai/config.toml</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>StandardOutPath</key>
  <string>/home/u/.local/share/hgl-ai/logs/bridge.log</string>
  <key>StandardErrorPath</key>
  <string>/home/u/.local/share/hgl-ai/logs/bridge.log</string>
</dict>
</plist>
"#
        );
    }

    /// System units never append to a file in the server user's home: root opens it
    /// and would follow a symlink planted there (hgl-ai#63). User units keep theirs.
    #[test]
    fn system_units_log_where_root_owns() {
        let spec = bridge_spec();
        let unit = render_systemd_system(&spec, "hgl-ai", "hgl-ai", Path::new("/var/lib/hgl-ai"));
        assert!(
            unit.contains("StandardOutput=journal\nStandardError=journal\n"),
            "{unit}"
        );
        assert!(!unit.contains("append:"), "{unit}");
        assert!(!unit.contains("/logs/"), "{unit}");
        assert!(render_systemd(&spec).contains("StandardOutput=append:/home/u/"));

        let plist = render_launchd_daemon(&spec, "_hgl-ai", Path::new("/Library/hgl-ai"));
        for key in ["StandardOutPath", "StandardErrorPath"] {
            assert!(
                plist.contains(&format!(
                    "<key>{key}</key>\n  <string>/Library/Logs/hgl-ai/bridge.log</string>"
                )),
                "{plist}"
            );
        }
        assert!(!plist.contains("/home/u/.local"), "{plist}");

        // No log file at all: the journal, or no output path.
        let none = Spec {
            log: None,
            ..bridge_spec()
        };
        assert!(render_systemd(&none).contains("StandardOutput=journal\n"));
        assert!(!render_launchd(&none).contains("StandardOutPath"));
    }

    /// A unit from before hgl-ai#63 is found without privilege.
    #[test]
    fn unsafe_system_logs_are_found() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        let out = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let o = out.clone();
        f.on(move |_| Output::success(o.borrow().clone()));
        *out.borrow_mut() = "StandardOutput=append\nStandardError=append\n".into();
        assert_eq!(
            system_log_unsafe(&f, Service::Hub).as_deref(),
            Some("StandardOutput=append")
        );
        *out.borrow_mut() = "StandardOutput=journal\nStandardError=inherit\n".into();
        assert_eq!(system_log_unsafe(&f, Service::Hub), None);
        assert_eq!(
            f.cmds.borrow().last().unwrap().1.line(),
            "systemctl show -p StandardOutput,StandardError hgl-ai-hub.service"
        );
        f.on(|_| Output::failure(1, ""));
        assert_eq!(system_log_unsafe(&f, Service::Hub), None);

        let m = Fake::new(d.path(), Os::MacOs);
        let out = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let o = out.clone();
        m.on(move |_| Output::success(o.borrow().clone()));
        *out.borrow_mut() = "system/io.github.hhenson.hgl-ai.hub = {\n\
             \tstdout path = /Library/hgl-ai/.local/share/hgl-ai/logs/hub.log\n\
             \tstderr path = /Library/hgl-ai/.local/share/hgl-ai/logs/hub.log\n}\n"
            .into();
        assert_eq!(
            system_log_unsafe(&m, Service::Hub).as_deref(),
            Some("stdout path = /Library/hgl-ai/.local/share/hgl-ai/logs/hub.log")
        );
        *out.borrow_mut() = "\tstdout path = /Library/Logs/hgl-ai/hub.log\n\
             \tstderr path = /Library/Logs/hgl-ai/hub.log\n"
            .into();
        assert_eq!(system_log_unsafe(&m, Service::Hub), None);
        assert_eq!(
            system_log_hint(Os::Linux, Service::Coord),
            "`journalctl -u hgl-ai-coord.service`"
        );
        assert_eq!(
            system_log_hint(Os::MacOs, Service::Coord),
            "`sudo tail /Library/Logs/hgl-ai/coord.log`"
        );
    }

    #[test]
    fn linux_install_and_state() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        let data = f.env.home.join(".local/share/hgl-ai");
        let spec = Spec::hgl_ai(Service::Bridge, &f, &data);
        assert_eq!(spec.program, f.env.exe);
        install(&f, &spec).unwrap();
        let unit = f
            .env
            .home
            .join(".config/systemd/user/hgl-ai-bridge.service");
        assert!(
            std::fs::read_to_string(&unit)
                .unwrap()
                .contains("bridge run")
        );
        assert!(data.join("logs").is_dir());
        assert_eq!(
            f.runs(),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable hgl-ai-bridge.service",
                "systemctl --user restart hgl-ai-bridge.service"
            ]
        );
        f.on(|c| {
            if c.args.contains(&"is-active".to_string()) {
                Output::success("active\n")
            } else {
                Output::success("")
            }
        });
        assert_eq!(state(&f, Service::Bridge), State::Running);
        assert_eq!(state(&f, Service::Hub), State::NotInstalled);
        uninstall(&f, Service::Bridge).unwrap();
        assert!(!unit.exists());
        assert_eq!(state(&f, Service::Bridge), State::NotInstalled);
    }

    #[test]
    fn macos_install_uses_launchctl() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::MacOs);
        f.on(|c| match c.args.first().map(String::as_str) {
            Some("-u") => Output::success("501\n"),
            Some("bootout") => Output::failure(3, "not loaded"),
            _ => Output::success(""),
        });
        let data = f.env.home.join(".local/share/hgl-ai");
        install(&f, &Spec::hgl_ai(Service::Coord, &f, &data)).unwrap();
        let plist = f
            .env
            .home
            .join("Library/LaunchAgents/io.github.hhenson.hgl-ai.coord.plist");
        assert!(
            std::fs::read_to_string(&plist)
                .unwrap()
                .contains("<string>coord</string>")
        );
        let runs = f.runs();
        assert_eq!(
            &runs[1..],
            [
                format!("launchctl bootstrap gui/501 {}", plist.display()),
                "launchctl enable gui/501/io.github.hhenson.hgl-ai.coord".into(),
                "launchctl kickstart -k gui/501/io.github.hhenson.hgl-ai.coord".into(),
            ]
        );
    }

    #[test]
    fn lingering() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        f.on(|c| match c.program.as_str() {
            "id" => Output::success("1000\n"),
            _ if c.args.first().map(String::as_str) == Some("show-user") => Output::success("no\n"),
            _ => Output::success(""),
        });
        assert_eq!(linger(&f, false).unwrap(), Some(false));
        assert!(f.runs().is_empty());
        linger(&f, true).unwrap();
        assert_eq!(f.runs(), ["loginctl enable-linger"]);
        assert!(linger_line(Some(false)).unwrap().contains("enable-linger"));
        let m = Fake::new(d.path(), Os::MacOs);
        assert_eq!(linger(&m, true).unwrap(), None);
    }

    /// The user a system service runs as, and its `HOME`, read without privilege on
    /// either OS.
    #[test]
    fn a_system_service_account() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        f.on(|c| match c.args.last().map(String::as_str) {
            Some("hgl-ai-hub.service") => Output::success(
                "LoadState=loaded\nEnvironment=HOME=/srv/hgl PATH=/usr/bin:/bin\nUser=hgl-ai\n",
            ),
            Some("hgl-ai-coord.service") => {
                Output::success("LoadState=loaded\nEnvironment=\nUser=agents\n")
            }
            Some("hgl-ai-console.service") => Output::success("LoadState=loaded\nUser=\n"),
            _ => Output::success("LoadState=not-found\nUser=\n"),
        });
        assert_eq!(
            system_account(&f, Service::Hub),
            Some(("hgl-ai".into(), Some("/srv/hgl".into())))
        );
        assert_eq!(
            system_account(&f, Service::Coord),
            Some(("agents".into(), None)),
            "no HOME="
        );
        assert_eq!(system_account(&f, Service::Console), None, "no User=");
        assert_eq!(system_account(&f, Service::Bridge), None, "not installed");
        let m = Fake::new(d.path(), Os::MacOs);
        m.on(|c| {
            let a: Vec<&str> = c.args.iter().map(String::as_str).collect();
            let hub = "/Library/LaunchDaemons/io.github.hhenson.hgl-ai.hub.plist";
            match a.as_slice() {
                ["-extract", "UserName", "raw", "-o", "-", p] if *p == hub => {
                    Output::success("hgl-ai\n")
                }
                ["-extract", "EnvironmentVariables.HOME", "raw", "-o", "-", p] if *p == hub => {
                    Output::success("/Library/hgl-ai\n")
                }
                _ => Output::failure(1, "No value at that key path or invalid key path"),
            }
        });
        assert_eq!(
            system_account(&m, Service::Hub),
            Some(("hgl-ai".into(), Some("/Library/hgl-ai".into())))
        );
        assert_eq!(system_account(&m, Service::Coord), None);
        assert!(f.runs().is_empty() && m.runs().is_empty());
    }

    /// The `PATH` the bridge's unit or agent records, as `install_service` writes it.
    #[test]
    fn a_recorded_path() {
        let d = tempfile::tempdir().unwrap();
        for os in [Os::Linux, Os::MacOs] {
            let f = Fake::new(&d.path().join(format!("{os:?}")), os);
            assert_eq!(recorded_path(&f, Service::Bridge), None, "not installed");
            let mut spec = Spec::hgl_ai(Service::Bridge, &f, &f.env.home.join("data"));
            spec.env[0].1 = "/opt/a&b/bin:/usr/bin".into();
            let text = match os {
                Os::Linux => render_systemd(&spec),
                Os::MacOs => render_launchd(&spec),
            };
            let p = unit_path(&f.env, Service::Bridge);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
            assert_eq!(
                recorded_path(&f, Service::Bridge).as_deref(),
                Some("/opt/a&b/bin:/usr/bin"),
                "{os:?}"
            );
        }
    }

    /// A plain `sudo -u NAME` leaves `XDG_RUNTIME_DIR` unset: `systemctl --user` is
    /// pointed at `/run/user/<uid>` when it exists, and left alone otherwise.
    #[test]
    fn user_state_without_a_session() {
        let d = tempfile::tempdir().unwrap();
        let mut f = Fake::new(d.path(), Os::Linux);
        let unit = unit_path(&f.env, Service::Bridge);
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(&unit, "").unwrap();
        let run = d.path().join("run/user");
        f.env.runtime_root = Some(run.clone());
        f.on(|c| match c.program.as_str() {
            "id" => Output::success("1000\n"),
            _ if c.env.iter().any(|(k, _)| k == "XDG_RUNTIME_DIR") => Output::success("active\n"),
            _ => Output::failure(3, "Failed to connect to bus: No medium found"),
        });
        assert_eq!(
            state(&f, Service::Bridge),
            State::Stopped,
            "no /run/user/1000"
        );
        std::fs::create_dir_all(run.join("1000")).unwrap();
        assert_eq!(state(&f, Service::Bridge), State::Running);
        let q = f.cmds.borrow();
        let (_, c) = q.iter().rfind(|(_, c)| c.program == "systemctl").unwrap();
        let dir = run.join("1000").display().to_string();
        assert!(
            c.env.contains(&("XDG_RUNTIME_DIR".into(), dir.clone())),
            "{c:?}"
        );
        assert!(
            c.env.contains(&(
                "DBUS_SESSION_BUS_ADDRESS".into(),
                format!("unix:path={dir}/bus")
            )),
            "{c:?}"
        );
    }
}
