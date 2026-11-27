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
}

impl Service {
    pub const ALL: [Service; 4] = [
        Service::Hub,
        Service::Bridge,
        Service::Coord,
        Service::Console,
    ];

    pub fn short(self) -> &'static str {
        match self {
            Service::Hub => "hub",
            Service::Bridge => "bridge",
            Service::Coord => "coord",
            Service::Console => "console",
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
    pub log: PathBuf,
}

impl Spec {
    /// The hub: `nats-server -c <data_dir>/nats.conf`.
    pub fn hub(nats_server: &Path, conf: &Path, data_dir: &Path) -> Spec {
        Spec {
            service: Service::Hub,
            program: nats_server.to_path_buf(),
            args: vec!["-c".into(), conf.display().to_string()],
            env: Vec::new(),
            log: log_path(data_dir, Service::Hub),
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
            log: log_path(data_dir, service),
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
    let log = spec.log.display().to_string().replace('%', "%%");
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
    s.push_str(&format!(
        "Restart=on-failure\n\
         RestartSec=10\n\
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    ));
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
    let log = xml(&spec.log.display().to_string());
    s.push_str(&format!(
        "  <key>RunAtLoad</key>\n  <true/>\n\
         \x20 <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n\
         \x20 <key>ThrottleInterval</key>\n  <integer>10</integer>\n\
         \x20 <key>StandardOutPath</key>\n  <string>{log}</string>\n\
         \x20 <key>StandardErrorPath</key>\n  <string>{log}</string>\n\
         </dict>\n</plist>\n"
    ));
    s
}

fn systemctl(args: &[&str]) -> Cmd {
    Cmd::new("systemctl")
        .arg("--user")
        .args(args.iter().copied())
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
    if let Some(logs) = spec.log.parent() {
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
    run_ok(sys, &Cmd::new("kill").args(["-HUP", pid]))?;
    Ok(true)
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
            .query(&systemctl(&["is-active", &s.unit()]))
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
            log: PathBuf::from("/home/u/.local/share/hgl-ai/logs/bridge.log"),
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
}
