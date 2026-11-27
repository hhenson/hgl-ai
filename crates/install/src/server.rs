//! `hgl-ai server install|start|stop|status|uninstall` (docs/install.md, Hub).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hgl_ai_config::Config;

use crate::hub::Hub;
use crate::nats::{self, NatsSource};
use crate::service::{self, Service, Spec, State};
use crate::sys::System;
use crate::token::{self, TokenAction};
use crate::{Error, HUB_WAIT_SECS, Result, done, io_at, load_config, other, save_config};

/// The default host label of a fresh hub configuration.
pub const DEFAULT_HOST: &str = "hub";

#[derive(Debug, Clone)]
pub struct ServerOpts {
    /// Host label for a new configuration (default `hub`).
    pub host: Option<String>,
    pub listen: Option<String>,
    pub port: Option<u16>,
    pub store: Option<String>,
    pub rotate_token: bool,
    /// Also install the coordinator service (`hgl-ai gh coord run`).
    pub coord: bool,
    /// Run `loginctl enable-linger` (Linux).
    pub linger: bool,
    /// Where the pinned `nats-server` release is downloaded from.
    pub release_base: String,
    /// How long to wait for the fresh hub to answer.
    pub wait: Duration,
}

impl Default for ServerOpts {
    fn default() -> Self {
        ServerOpts {
            host: None,
            listen: None,
            port: None,
            store: None,
            rotate_token: false,
            coord: true,
            linger: false,
            release_base: nats::RELEASE_BASE.into(),
            wait: Duration::from_secs(HUB_WAIT_SECS),
        }
    }
}

fn unspecified(addr: &str) -> bool {
    matches!(addr, "" | "0.0.0.0" | "::" | "[::]")
}

fn url_host(addr: &str) -> String {
    let a = addr.trim_start_matches('[').trim_end_matches(']');
    if a.contains(':') {
        format!("[{a}]")
    } else {
        a.to_string()
    }
}

/// The address a client on this host dials.
pub fn connect_host(listen: &str) -> String {
    if unspecified(listen) {
        "127.0.0.1".into()
    } else {
        listen.trim_start_matches('[').trim_end_matches(']').into()
    }
}

/// `nats://<host>:<port>` for clients on this host.
pub fn local_url(listen: &str, port: u16) -> String {
    format!("nats://{}:{port}", url_host(&connect_host(listen)))
}

fn absolute(p: PathBuf) -> PathBuf {
    std::path::absolute(&p).unwrap_or(p)
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

/// Install `nats-server`, its configuration, the token and the hub service;
/// start it, provision the stream and buckets, and print the client command.
pub async fn install(sys: &dyn System, hub: &impl Hub, o: &ServerOpts) -> Result<Config> {
    let env = sys.env();
    let existing = load_config(sys, env)?;
    let created = existing.is_none();
    let mut cfg =
        existing.unwrap_or_else(|| Config::new(o.host.as_deref().unwrap_or(DEFAULT_HOST)));
    if let Some(h) = &o.host {
        cfg.host = h.clone();
    }
    let mut srv = cfg.server.clone().unwrap_or_default();
    if let Some(l) = &o.listen {
        srv.listen = l.clone();
    }
    if let Some(p) = o.port {
        srv.port = p;
    }
    if let Some(s) = &o.store {
        srv.store_dir = env.contract(&absolute(env.expand(s)));
    }
    cfg.hub.url = local_url(&srv.listen, srv.port);
    cfg.server = Some(srv.clone());
    cfg.validate()?;

    let data_dir = env.expand(&cfg.paths.data_dir);
    let store = env.expand(&srv.store_dir);
    let token_path = env.expand(&cfg.hub.token_file);

    let (nats_bin, src) = nats::ensure_server(sys, &data_dir, &srv.nats_version, &o.release_base)?;
    let how = match src {
        NatsSource::Path => "on PATH".to_string(),
        NatsSource::Brew => "Homebrew".to_string(),
        NatsSource::Release => format!("release {}, checksum verified", srv.nats_version),
    };
    sys.say(&format!("nats-server: {} ({how})", nats_bin.display()));

    let (tok, action) = token::ensure(sys, &token_path, o.rotate_token)?;
    let shown = env.contract(&token_path);
    match action {
        TokenAction::Kept => sys.say(&format!("token: {shown} (kept)")),
        TokenAction::Created => sys.say(&format!("token: {shown} (new, mode 600)")),
        TokenAction::Rotated => sys.say(&format!(
            "token: {shown} (rotated; every client needs the new file and a rerun of `hgl-ai client install`)"
        )),
    }

    sys.mkdir(&store).map_err(io_at(&store))?;
    let conf = data_dir.join("nats.conf");
    let text = nats::render_conf(&srv.listen, srv.port, &store, &tok);
    sys.write(&conf, text.as_bytes(), token::MODE)
        .map_err(io_at(&conf))?;
    sys.say(&format!("nats.conf: {}", env.contract(&conf)));

    save_config(sys, env, &cfg)?;
    let verb = if created { "created" } else { "updated" };
    sys.say(&format!("config: {} ({verb})", env.config_path.display()));

    service::install(sys, &Spec::hub(&nats_bin, &conf, &data_dir))?;
    done(
        sys,
        &format!("service: {} started", service_name(sys, Service::Hub)),
    );

    let host = connect_host(&srv.listen);
    if sys.dry_run() {
        sys.say(&format!(
            "would wait for {host}:{} and provision the stream and buckets",
            srv.port
        ));
    } else {
        if !wait_port(sys, &host, srv.port, o.wait) {
            return Err(other(format!(
                "the hub did not answer on {host}:{} within {} s; see {}",
                srv.port,
                o.wait.as_secs(),
                service::log_path(&data_dir, Service::Hub).display()
            )));
        }
        sys.say(&format!("hub: answering on {host}:{}", srv.port));
        hub.provision(&cfg).await.map_err(Error::Bus)?;
        sys.say("hub: stream and buckets provisioned");
    }

    if o.coord {
        service::install(sys, &Spec::hgl_ai(Service::Coord, sys, &data_dir))?;
        done(
            sys,
            &format!("service: {} started", service_name(sys, Service::Coord)),
        );
    }
    if let Some(l) = service::linger_line(service::linger(sys, o.linger)?) {
        sys.say(&l);
    }

    sys.say("");
    sys.say("Next, on this host:");
    sys.say(&format!(
        "  hgl-ai client install --hub {} --token-file {shown} --host {}",
        cfg.hub.url, cfg.host
    ));
    sys.say(&format!(
        "On each other host, after copying {shown} there (same path, mode 600) by hand or a secret manager:"
    ));
    let remote = if unspecified(&srv.listen) {
        "<hub-address>".to_string()
    } else {
        url_host(&srv.listen)
    };
    sys.say(&format!(
        "  hgl-ai client install --hub nats://{remote}:{} --token-file {shown} --host <label>",
        srv.port
    ));
    if unspecified(&srv.listen) {
        sys.say("  (the hub listens on every address; use the one other hosts reach, such as a tailnet address)");
    } else if host == "127.0.0.1" || host == "::1" || host == "localhost" {
        sys.say("  (the hub listens on loopback only: other hosts need an SSH forward, or reinstall with --listen <tailnet address>)");
    }
    Ok(cfg)
}

fn service_name(sys: &dyn System, s: Service) -> String {
    match sys.env().os {
        crate::Os::Linux => s.unit(),
        crate::Os::MacOs => s.label(),
    }
}

/// The hub's services that are installed on this host, hub first.
fn installed(sys: &dyn System) -> Vec<Service> {
    [Service::Hub, Service::Coord]
        .into_iter()
        .filter(|&s| sys.exists(&service::unit_path(sys.env(), s)))
        .collect()
}

pub fn start(sys: &dyn System) -> Result<()> {
    let svcs = installed(sys);
    if svcs.is_empty() {
        return Err(other(
            "no hub service on this host; run `hgl-ai server install`",
        ));
    }
    for s in svcs {
        service::start(sys, s)?;
        sys.say(&format!("{}: started", s.short()));
    }
    Ok(())
}

pub fn stop(sys: &dyn System) -> Result<()> {
    for s in installed(sys).into_iter().rev() {
        service::stop(sys, s)?;
        sys.say(&format!("{}: stopped", s.short()));
    }
    Ok(())
}

/// Print each service's state and whether the port answers; `true` when all is up.
pub fn status(sys: &dyn System) -> Result<bool> {
    let env = sys.env();
    let Some(srv) = load_config(sys, env)?.and_then(|c| c.server) else {
        sys.say("no hub configured on this host; run `hgl-ai server install`");
        return Ok(false);
    };
    let mut up = true;
    for s in [Service::Hub, Service::Coord] {
        let st = service::state(sys, s);
        let text = match st {
            State::Running => "running",
            State::Stopped => "stopped",
            State::NotInstalled => "not installed",
        };
        up &= st == State::Running || (s == Service::Coord && st == State::NotInstalled);
        sys.say(&format!("{}: {text}", s.short()));
    }
    let host = connect_host(&srv.listen);
    let open = sys.port_open(&host, srv.port);
    sys.say(&format!(
        "port {host}:{}: {}",
        srv.port,
        if open { "answering" } else { "not answering" }
    ));
    Ok(up && open)
}

/// Remove the hub services, `nats.conf` and the `[server]` section. The
/// JetStream store and the token stay unless `purge` (the token always stays:
/// this host's client uses it).
pub fn uninstall(sys: &dyn System, purge: bool) -> Result<()> {
    let env = sys.env();
    let mut cfg = load_config(sys, env)?;
    let data_dir = env.expand(
        cfg.as_ref()
            .map_or("~/.local/share/hgl-ai", |c| c.paths.data_dir.as_str()),
    );
    for s in [Service::Coord, Service::Hub] {
        if sys.exists(&service::unit_path(env, s)) {
            service::uninstall(sys, s)?;
            sys.say(&format!("{}: removed", s.short()));
        }
    }
    let conf = data_dir.join("nats.conf");
    sys.remove_file(&conf).map_err(io_at(&conf))?;
    let store = cfg
        .as_ref()
        .and_then(|c| c.server.as_ref())
        .map(|s| env.expand(&s.store_dir))
        .unwrap_or_else(|| env.expand(&hgl_ai_config::Server::default().store_dir));
    if let Some(c) = cfg.as_mut().filter(|c| c.server.is_some()) {
        c.server = None;
        save_config(sys, env, c)?;
        sys.say(&format!(
            "config: removed [server] from {}",
            env.config_path.display()
        ));
    }
    if purge {
        remove_tree(sys, &store)?;
        let ours = data_dir.join("bin/nats-server");
        sys.remove_file(&ours).map_err(io_at(&ours))?;
        sys.say(&format!(
            "purged the JetStream store {}",
            env.contract(&store)
        ));
    } else {
        sys.say(&format!(
            "kept the JetStream store {} (--purge removes it)",
            env.contract(&store)
        ));
    }
    Ok(())
}

fn remove_tree(sys: &dyn System, p: &Path) -> Result<()> {
    sys.remove_dir_all(p).map_err(io_at(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::fake::FakeHub;
    use crate::sys::fake::{Fake, files};
    use crate::{DryRun, Os, Output};

    #[test]
    fn urls() {
        assert_eq!(local_url("192.0.2.10", 4222), "nats://192.0.2.10:4222");
        assert_eq!(local_url("0.0.0.0", 4222), "nats://127.0.0.1:4222");
        assert_eq!(local_url("2001:db8::1", 4222), "nats://[2001:db8::1]:4222");
        assert_eq!(connect_host("::"), "127.0.0.1");
    }

    fn fake(d: &Path, os: Os) -> Fake {
        let f = Fake::new(d, os);
        f.tool("nats-server");
        f.on(
            |c| match (c.program.as_str(), c.args.first().map(String::as_str)) {
                ("id", _) => Output::success("1000\n"),
                ("loginctl", Some("show-user")) => Output::success("yes\n"),
                _ => Output::success(""),
            },
        );
        f
    }

    #[tokio::test]
    async fn install_end_to_end_linux() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux);
        let hub = FakeHub::up();
        let o = ServerOpts {
            listen: Some("192.0.2.10".into()),
            ..ServerOpts::default()
        };
        let cfg = install(&f, &hub, &o).await.unwrap();
        let home = &f.env.home;

        assert_eq!(cfg.host, "hub");
        assert_eq!(cfg.hub.url, "nats://192.0.2.10:4222");
        let saved = load_config(&f, &f.env).unwrap().unwrap();
        assert_eq!(saved, cfg);
        assert_eq!(saved.server.as_ref().unwrap().listen, "192.0.2.10");

        let tok_path = home.join(".config/hgl-ai/hub.token");
        let tok = token::read(&f, &tok_path).unwrap().unwrap();
        assert_eq!(tok.len(), 64);
        assert_eq!(f.mode(&tok_path).unwrap(), 0o600);
        let conf_path = home.join(".local/share/hgl-ai/nats.conf");
        let conf = std::fs::read_to_string(&conf_path).unwrap();
        assert!(conf.contains(&format!("token: \"{tok}\"")));
        assert!(conf.contains(&format!(
            "store_dir: \"{}\"",
            home.join(".local/share/hgl-ai/nats").display()
        )));
        assert_eq!(f.mode(&conf_path).unwrap(), 0o600);

        let hub_unit =
            std::fs::read_to_string(home.join(".config/systemd/user/hgl-ai-hub.service")).unwrap();
        assert!(hub_unit.contains(&format!(
            "ExecStart=/usr/bin/nats-server -c {}",
            conf_path.display()
        )));
        let coord_unit =
            std::fs::read_to_string(home.join(".config/systemd/user/hgl-ai-coord.service"))
                .unwrap();
        assert!(coord_unit.contains("hgl-ai gh coord run"));
        assert_eq!(*hub.calls.borrow(), ["provision"]);
        let runs = f.runs();
        assert_eq!(
            runs.iter().filter(|r| r.contains("daemon-reload")).count(),
            2
        );
        assert!(runs.contains(&"systemctl --user restart hgl-ai-hub.service".to_string()));
        let hub_at = runs
            .iter()
            .position(|r| r.ends_with("restart hgl-ai-hub.service"))
            .unwrap();
        let coord_at = runs
            .iter()
            .position(|r| r.ends_with("restart hgl-ai-coord.service"))
            .unwrap();
        assert!(hub_at < coord_at, "coordinator starts after the hub");

        let out = f.output();
        assert!(!out.contains(&tok), "the token is never printed");
        assert!(out.contains("hgl-ai client install --hub nats://192.0.2.10:4222 --token-file ~/.config/hgl-ai/hub.token --host <label>"));
        assert!(out.contains("lingering: on"));

        // Reinstall keeps the token.
        install(&f, &hub, &ServerOpts::default()).await.unwrap();
        assert_eq!(token::read(&f, &tok_path).unwrap().unwrap(), tok);
        assert!(f.output().contains("(kept)"));
        let again = load_config(&f, &f.env).unwrap().unwrap();
        assert_eq!(
            again.server.unwrap().listen,
            "192.0.2.10",
            "existing [server] is the default"
        );
    }

    #[tokio::test]
    async fn dry_run_writes_and_runs_nothing() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux);
        let before = files(d.path());
        let hub = FakeHub::up();
        let o = ServerOpts {
            host: Some("hub-1".into()),
            linger: true,
            ..ServerOpts::default()
        };
        install(&DryRun::new(&f), &hub, &o).await.unwrap();
        assert_eq!(files(d.path()), before);
        assert!(f.runs().is_empty());
        assert!(hub.calls.borrow().is_empty());
        let out = f.output();
        assert!(
            out.contains("would write")
                && out.contains("would run: systemctl --user enable hgl-ai-hub.service")
        );
        assert!(out.contains("would run: loginctl enable-linger"));
    }

    #[tokio::test]
    async fn downloads_release_without_nats_or_brew() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        let o = ServerOpts {
            coord: false,
            ..ServerOpts::default()
        };
        install(&DryRun::new(&f), &FakeHub::up(), &o).await.unwrap();
        let out = f.output();
        assert!(out.contains("would run: curl -fsSL --retry 3 -o"), "{out}");
        assert!(out.contains("nats-server-v2.15.3-linux-amd64.tar.gz"));
        assert!(out.contains("would verify"));
        assert!(!out.contains("coord"));
    }

    #[tokio::test]
    async fn hub_that_never_answers_fails() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux);
        f.ports_open.set(false);
        let hub = FakeHub::up();
        let o = ServerOpts {
            wait: Duration::ZERO,
            ..ServerOpts::default()
        };
        let err = install(&f, &hub, &o).await.unwrap_err();
        assert!(err.to_string().contains("did not answer"), "{err}");
        assert!(
            hub.calls.borrow().is_empty(),
            "no provisioning without a hub"
        );
    }

    #[tokio::test]
    async fn macos_uses_launch_agents() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::MacOs);
        install(&f, &FakeHub::up(), &ServerOpts::default())
            .await
            .unwrap();
        let agents = f.env.home.join("Library/LaunchAgents");
        assert!(agents.join("io.github.hhenson.hgl-ai.hub.plist").exists());
        assert!(agents.join("io.github.hhenson.hgl-ai.coord.plist").exists());
        assert!(
            f.runs()
                .iter()
                .any(|r| r == "launchctl kickstart -k gui/1000/io.github.hhenson.hgl-ai.hub")
        );
        assert!(!f.output().contains("lingering"));
    }

    #[tokio::test]
    async fn status_and_uninstall() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux);
        install(&f, &FakeHub::up(), &ServerOpts::default())
            .await
            .unwrap();
        f.on(|c| {
            if c.args.contains(&"is-active".to_string()) {
                Output::success("active\n")
            } else {
                Output::success("")
            }
        });
        assert!(status(&f).unwrap());
        let store = f.env.home.join(".local/share/hgl-ai/nats");
        std::fs::write(store.join("data"), "x").unwrap();

        uninstall(&f, false).unwrap();
        let home = &f.env.home;
        assert!(
            !home
                .join(".config/systemd/user/hgl-ai-hub.service")
                .exists()
        );
        assert!(!home.join(".local/share/hgl-ai/nats.conf").exists());
        assert!(store.join("data").exists(), "store kept");
        assert!(home.join(".config/hgl-ai/hub.token").exists(), "token kept");
        assert!(load_config(&f, &f.env).unwrap().unwrap().server.is_none());
        assert!(!status(&f).unwrap());

        uninstall(&f, true).unwrap();
        assert!(!store.exists(), "store purged");
    }
}
