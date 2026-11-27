//! `hgl-ai server install|start|stop|status|uninstall` (docs/install.md, Hub).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hgl_ai_config::{Config, HubAuth};

use crate::hub::Hub;
use crate::nats::{self, NatsSource};
use crate::service::{self, Service, Spec, State};
use crate::sys::System;
use crate::token::{self, TokenAction};
use crate::users::{self, Registry, Role};
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
    /// Give each client its own hub user (a new hub always does; an older token hub
    /// moves to users with this).
    pub users: bool,
    /// Also install the coordinator service (`hgl-ai gh coord run`).
    pub coord: bool,
    /// Also install the console service (`hgl-ai console run`) when `[console]` is set.
    pub console: bool,
    /// Write `[console]` with this origin (`https://<stable name>`).
    pub console_url: Option<String>,
    pub console_listen: Option<String>,
    /// GitHub logins and Apple emails added to `[console] users`.
    pub console_users: Vec<String>,
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
            users: false,
            coord: true,
            console: true,
            console_url: None,
            console_listen: None,
            console_users: Vec::new(),
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

/// Install `nats-server`, its configuration, the clients' credentials and the hub
/// service; start it, provision the stream and buckets, install the coordinator and
/// console services, and print the client command.
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
    if o.users {
        srv.auth = HubAuth::Users;
    }
    cfg.hub.url = local_url(&srv.listen, srv.port);
    cfg.server = Some(srv.clone());
    if let Some(url) = &o.console_url {
        let mut c = cfg.console.clone().unwrap_or_default();
        c.url = url.clone();
        if let Some(l) = &o.console_listen {
            c.listen = l.clone();
        }
        // `EMAIL=HANDLE` (OWN-10), or a bare `EMAIL` (no handle yet; doctor warns).
        for u in &o.console_users {
            let (email, handle) = match u.split_once('=') {
                Some((e, h)) => (e.trim().to_string(), Some(h.trim().to_string())),
                None => (u.trim().to_string(), None),
            };
            let entry = hgl_ai_config::User::Person(hgl_ai_config::Person {
                email,
                handle,
                github: None,
                apple: None,
                google: None,
            });
            if !c.users.contains(&entry) {
                c.users.push(entry);
            }
        }
        cfg.console = Some(c);
    }
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

    let shown = env.contract(&token_path);
    let conf = data_dir.join("nats.conf");
    // The credentials the hub's own services use, and whether this host's client
    // credential changed (its bridge then restarts).
    let mut creds = ServiceCreds::default();
    let text = match srv.auth {
        HubAuth::Token => {
            let (tok, action) = token::ensure(sys, &token_path, o.rotate_token)?;
            match action {
                TokenAction::Kept => sys.say(&format!("token: {shown} (kept)")),
                TokenAction::Created => sys.say(&format!("token: {shown} (new, mode 600)")),
                TokenAction::Rotated => sys.say(&format!(
                    "token: {shown} (rotated; every client needs the new file and a rerun of `hgl-ai client install`)"
                )),
            }
            sys.say(
                "auth: single shared token; any client can publish as the owner. \
                 `hgl-ai server install --users` gives each client its own hub user (docs/install.md)",
            );
            nats::render_conf(&srv.listen, srv.port, &store, &nats::Auth::Token(&tok))
        }
        HubAuth::Users => {
            let mut reg = Registry::load(sys, &data_dir)?;
            let cdir = env.config_dir();
            let admin = format!("{}-admin", cfg.host);
            creds.admin = Some(ensure_user(
                sys,
                &mut reg,
                &admin,
                Role::Admin,
                &users::cred_path(&cdir, &admin),
            )?);
            // This host's own client: its seat credential in `hub.token_file`.
            creds.client_changed =
                ensure_user_at(sys, &mut reg, &cfg.host, Role::Seat, &token_path)?;
            if creds.client_changed {
                sys.say(&format!(
                    "credential: {shown} (hub user {}, seat)",
                    cfg.host
                ));
            }
            if o.coord {
                let n = format!("{}-coord", cfg.host);
                creds.coord = Some(ensure_user(
                    sys,
                    &mut reg,
                    &n,
                    Role::Coord,
                    &users::cred_path(&cdir, &n),
                )?);
            }
            if o.console {
                creds.console = Some(ensure_user(
                    sys,
                    &mut reg,
                    CONSOLE_USER,
                    Role::Console,
                    &users::cred_path(&cdir, CONSOLE_USER),
                )?);
            }
            reg.save(sys, &data_dir)?;
            sys.say(&format!(
                "auth: {} hub users ({})",
                reg.users.len(),
                env.contract(&users::registry_path(&data_dir))
            ));
            nats::render_conf(&srv.listen, srv.port, &store, &nats::Auth::Users(&reg))
        }
    };

    sys.mkdir(&store).map_err(io_at(&store))?;
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
        let mut admin_cfg = cfg.clone();
        if let Some(a) = &creds.admin {
            admin_cfg.hub.token_file = a.display().to_string();
        }
        hub.provision(&admin_cfg).await.map_err(Error::Bus)?;
        sys.say("hub: stream and buckets provisioned");
    }

    if o.coord {
        let spec =
            Spec::hgl_ai(Service::Coord, sys, &data_dir).with_credential(creds.coord.as_deref());
        service::install(sys, &spec)?;
        done(
            sys,
            &format!("service: {} started", service_name(sys, Service::Coord)),
        );
    }
    if o.console {
        if cfg.console.is_some() {
            let spec = Spec::hgl_ai(Service::Console, sys, &data_dir)
                .with_credential(creds.console.as_deref());
            service::install(sys, &spec)?;
            done(
                sys,
                &format!("service: {} started", service_name(sys, Service::Console)),
            );
        } else {
            sys.say(
                "console: not installed: no [console] in the configuration; rerun with \
                 --console-url https://<stable name> --console-user <login> (docs/install.md)",
            );
        }
    }
    if creds.client_changed && sys.exists(&service::unit_path(env, Service::Bridge)) {
        service::restart(sys, Service::Bridge)?;
        done(sys, "service: bridge restarted with the new credential");
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
    let remote = if unspecified(&srv.listen) {
        "<hub-address>".to_string()
    } else {
        url_host(&srv.listen)
    };
    match srv.auth {
        HubAuth::Token => {
            sys.say(&format!(
                "On each other host, after copying {shown} there (same path, mode 600) by hand or a secret manager:"
            ));
            sys.say(&format!(
                "  hgl-ai client install --hub nats://{remote}:{} --token-file {shown} --host <label>",
                srv.port
            ));
        }
        HubAuth::Users => {
            sys.say(
                "For each other host, here: `hgl-ai server user add <label> --role seat`; copy the",
            );
            sys.say("credential it names to that host (mode 600) by hand or a secret manager, then there:");
            sys.say(&format!(
                "  hgl-ai client install --hub nats://{remote}:{} --token-file <credential> --host <label>",
                srv.port
            ));
        }
    }
    if unspecified(&srv.listen) {
        sys.say("  (the hub listens on every address; use the one other hosts reach, such as a tailnet address)");
    } else if host == "127.0.0.1" || host == "::1" || host == "localhost" {
        sys.say("  (the hub listens on loopback only: other hosts need an SSH forward, or reinstall with --listen <tailnet address>)");
    }
    Ok(cfg)
}

/// The hub user name of the console.
pub const CONSOLE_USER: &str = "console";

#[derive(Default)]
struct ServiceCreds {
    admin: Option<PathBuf>,
    coord: Option<PathBuf>,
    console: Option<PathBuf>,
    client_changed: bool,
}

/// The NKey public key of a credential file, if it holds a hub user.
fn cred_key(sys: &dyn System, path: &Path) -> Option<String> {
    match token::read_credential(sys, path).ok()?? {
        hgl_ai_bus::Credential::Nkey { seed, .. } => nkeys::KeyPair::from_seed(&seed)
            .ok()
            .map(|k| k.public_key()),
        hgl_ai_bus::Credential::Token(_) => None,
    }
}

/// Keep `name` when the registry and the credential at `path` agree, else issue a key and
/// write the credential there. Returns whether it wrote one.
fn ensure_user_at(
    sys: &dyn System,
    reg: &mut Registry,
    name: &str,
    role: Role,
    path: &Path,
) -> Result<bool> {
    let current = reg
        .get(name)
        .filter(|u| u.role == role)
        .map(|u| u.nkey.clone());
    if current.is_some() && current == cred_key(sys, path) {
        return Ok(false);
    }
    let text = reg.issue(name, role, true)?;
    users::write_cred(sys, path, &text)?;
    Ok(true)
}

fn ensure_user(
    sys: &dyn System,
    reg: &mut Registry,
    name: &str,
    role: Role,
    path: &Path,
) -> Result<PathBuf> {
    if ensure_user_at(sys, reg, name, role, path)? {
        sys.say(&format!(
            "credential: {} (hub user {name}, {role})",
            sys.env().contract(path)
        ));
    }
    Ok(path.to_path_buf())
}

/// What `server user add` did.
pub struct UserAdded {
    /// The credential written, unless the user was added by its public key.
    pub cred: Option<PathBuf>,
    pub hub_restarted: bool,
}

fn hub_config(sys: &dyn System) -> Result<(Config, hgl_ai_config::Server)> {
    let cfg = load_config(sys, sys.env())?
        .ok_or_else(|| other("no configuration; run `hgl-ai server install` on the hub host"))?;
    let srv = cfg
        .server
        .clone()
        .ok_or_else(|| other("this host runs no hub; run this on the hub host"))?;
    Ok((cfg, srv))
}

/// Rewrite `nats.conf` from the registry and restart the hub when it is installed. Only
/// in users mode; a token hub keeps its file. Returns whether the hub restarted.
fn apply_users(
    sys: &dyn System,
    cfg: &Config,
    srv: &hgl_ai_config::Server,
    reg: &Registry,
) -> Result<bool> {
    if srv.auth != HubAuth::Users {
        sys.say(
            "auth: the hub still takes its single shared token; `hgl-ai server install --users` \
             switches it to hub users (docs/install.md)",
        );
        return Ok(false);
    }
    let env = sys.env();
    let data_dir = env.expand(&cfg.paths.data_dir);
    let conf = data_dir.join("nats.conf");
    let text = nats::render_conf(
        &srv.listen,
        srv.port,
        &env.expand(&srv.store_dir),
        &nats::Auth::Users(reg),
    );
    sys.write(&conf, text.as_bytes(), token::MODE)
        .map_err(io_at(&conf))?;
    if sys.exists(&service::unit_path(env, Service::Hub)) {
        service::restart(sys, Service::Hub)?;
        done(sys, "hub: restarted with the new users");
        return Ok(true);
    }
    Ok(false)
}

/// `hgl-ai server user add NAME --role ROLE`: a hub user and its credential file (mode
/// 0600) under `<config_dir>/hub-users/`, to be copied to its host. With `nkey`, the user
/// is the holder of that public key, whose seed stays on its own host (FLEET-10), and
/// nothing secret is written.
pub fn user_add(
    sys: &dyn System,
    name: &str,
    role: Role,
    nkey: Option<&str>,
    rotate: bool,
) -> Result<UserAdded> {
    let (cfg, srv) = hub_config(sys)?;
    let env = sys.env();
    let data_dir = env.expand(&cfg.paths.data_dir);
    let mut reg = Registry::load(sys, &data_dir)?;
    let cred = match nkey {
        Some(k) => {
            reg.register(name, role, k, rotate)?;
            sys.say(&format!(
                "hub user {name} ({role}): public key {k}, fingerprint {}",
                users::fingerprint(k)
            ));
            None
        }
        None => {
            let text = reg.issue(name, role, rotate)?;
            let cred = users::cred_path(&env.config_dir(), name);
            users::write_cred(sys, &cred, &text)?;
            let shown = env.contract(&cred);
            sys.say(&format!(
                "credential: {shown} (hub user {name}, {role}, mode 600)"
            ));
            if role == Role::Seat {
                sys.say(&format!(
                    "copy {shown} to host {name}, then there: hgl-ai client install --hub <hub url> --token-file <copied file> --host {name}"
                ));
            }
            Some(cred)
        }
    };
    reg.save(sys, &data_dir)?;
    let hub_restarted = apply_users(sys, &cfg, &srv, &reg)?;
    Ok(UserAdded {
        cred,
        hub_restarted,
    })
}

/// `hgl-ai server user remove NAME`: the hub refuses its credential from then on.
pub fn user_remove(sys: &dyn System, name: &str) -> Result<bool> {
    let (cfg, srv) = hub_config(sys)?;
    let env = sys.env();
    let data_dir = env.expand(&cfg.paths.data_dir);
    let mut reg = Registry::load(sys, &data_dir)?;
    if !reg.remove(name) {
        return Ok(false);
    }
    reg.save(sys, &data_dir)?;
    let cred = users::cred_path(&env.config_dir(), name);
    sys.remove_file(&cred).map_err(io_at(&cred))?;
    sys.say(&format!("removed hub user {name}"));
    apply_users(sys, &cfg, &srv, &reg)?;
    Ok(true)
}

/// The registered hub users.
pub fn user_list(sys: &dyn System) -> Result<Vec<users::User>> {
    let (cfg, _) = hub_config(sys)?;
    let data_dir = sys.env().expand(&cfg.paths.data_dir);
    Ok(Registry::load(sys, &data_dir)?.users)
}

fn service_name(sys: &dyn System, s: Service) -> String {
    match sys.env().os {
        crate::Os::Linux => s.unit(),
        crate::Os::MacOs => s.label(),
    }
}

/// The hub's services that are installed on this host, hub first.
fn installed(sys: &dyn System) -> Vec<Service> {
    [Service::Hub, Service::Coord, Service::Console]
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
    for s in [Service::Hub, Service::Coord, Service::Console] {
        let st = service::state(sys, s);
        let text = match st {
            State::Running => "running",
            State::Stopped => "stopped",
            State::NotInstalled => "not installed",
        };
        up &= st == State::Running || (s != Service::Hub && st == State::NotInstalled);
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
    for s in [Service::Console, Service::Coord, Service::Hub] {
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
        let reg = users::registry_path(&data_dir);
        sys.remove_file(&reg).map_err(io_at(&reg))?;
        let creds = env.config_dir().join("hub-users");
        remove_tree(sys, &creds)?;
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
        let srv = saved.server.as_ref().unwrap();
        assert_eq!(srv.listen, "192.0.2.10");
        assert_eq!(srv.auth, HubAuth::Users, "a new hub has a user per client");

        // This host's own client is hub user `hub` (seat), in hub.token.
        let tok_path = home.join(".config/hgl-ai/hub.token");
        let c = token::read_credential(&f, &tok_path).unwrap().unwrap();
        assert_eq!(c.describe(), "hub user hub (seat)");
        assert_eq!(f.mode(&tok_path).unwrap(), 0o600);
        let creds = home.join(".config/hgl-ai/hub-users");
        for n in ["hub-admin", "hub-coord", "console"] {
            assert_eq!(
                f.mode(&creds.join(format!("{n}.cred"))).unwrap(),
                0o600,
                "{n}"
            );
        }
        assert_eq!(f.mode(&creds).unwrap(), 0o700);
        let data = home.join(".local/share/hgl-ai");
        let reg = Registry::load(&f, &data).unwrap();
        let names: Vec<_> = reg
            .users
            .iter()
            .map(|u| (u.name.as_str(), u.role))
            .collect();
        assert_eq!(
            names,
            [
                ("console", Role::Console),
                ("hub", Role::Seat),
                ("hub-admin", Role::Admin),
                ("hub-coord", Role::Coord)
            ]
        );
        let conf_path = data.join("nats.conf");
        let conf = std::fs::read_to_string(&conf_path).unwrap();
        assert!(!conf.contains("token:"), "{conf}");
        for u in &reg.users {
            assert!(conf.contains(&u.nkey), "{}", u.name);
        }
        assert!(conf.contains("deny = [\"hgl.owner.>\", \"hgl.notes.owner.>\""));
        assert_eq!(f.mode(&conf_path).unwrap(), 0o600);
        // Provisioning ran as the admin user.
        assert_eq!(*hub.calls.borrow(), ["provision"]);
        assert_eq!(
            hub.creds.borrow()[0],
            creds.join("hub-admin.cred").display().to_string()
        );

        let unit = |n: &str| {
            std::fs::read_to_string(home.join(format!(".config/systemd/user/hgl-ai-{n}.service")))
                .unwrap()
        };
        assert!(unit("hub").contains(&format!(
            "ExecStart=/usr/bin/nats-server -c {}",
            conf_path.display()
        )));
        let coord = unit("coord");
        assert!(coord.contains("hgl-ai gh coord run"));
        assert!(coord.contains(&format!(
            "Environment=\"HGL_AI_HUB_CREDENTIAL={}\"",
            creds.join("hub-coord.cred").display()
        )));
        assert!(
            !home
                .join(".config/systemd/user/hgl-ai-console.service")
                .exists(),
            "no [console], no console service"
        );
        let runs = f.runs();
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
        for p in [&tok_path, &creds.join("hub-admin.cred")] {
            let text = std::fs::read_to_string(p).unwrap();
            let seed = text.lines().find(|l| l.starts_with("seed")).unwrap();
            assert!(!out.contains(&seed[8..]), "a seed is never printed");
        }
        assert!(out.contains("hgl-ai client install --hub nats://192.0.2.10:4222 --token-file ~/.config/hgl-ai/hub.token --host hub"));
        assert!(out.contains("hgl-ai server user add <label> --role seat"));
        assert!(out.contains("console: not installed"));

        // Reinstall keeps every key.
        install(&f, &hub, &ServerOpts::default()).await.unwrap();
        assert_eq!(Registry::load(&f, &data).unwrap(), reg);
        assert_eq!(token::read_credential(&f, &tok_path).unwrap().unwrap(), c);
        let again = load_config(&f, &f.env).unwrap().unwrap();
        assert_eq!(
            again.server.unwrap().listen,
            "192.0.2.10",
            "existing [server] is the default"
        );
    }

    /// A hub installed before hub users keeps its shared token, and moves with `--users`.
    #[tokio::test]
    async fn token_hub_keeps_working_then_migrates() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux);
        let home = f.env.home.clone();
        std::fs::create_dir_all(home.join(".config/hgl-ai")).unwrap();
        std::fs::write(
            home.join(".config/hgl-ai/config.toml"),
            "host = \"hub\"\n[server]\nlisten = \"192.0.2.10\"\nport = 4222\nstore_dir = \"~/.local/share/hgl-ai/nats\"\nnats_version = \"2.15.3\"\n",
        )
        .unwrap();
        let hub = FakeHub::up();
        let cfg = install(&f, &hub, &ServerOpts::default()).await.unwrap();
        assert_eq!(cfg.server.as_ref().unwrap().auth, HubAuth::Token);
        let tok_path = home.join(".config/hgl-ai/hub.token");
        let tok = token::read(&f, &tok_path).unwrap().unwrap();
        assert_eq!(tok.len(), 64);
        let conf_path = home.join(".local/share/hgl-ai/nats.conf");
        let conf = std::fs::read_to_string(&conf_path).unwrap();
        assert!(conf.contains(&format!("token: \"{tok}\"")));
        let out = f.output();
        assert!(!out.contains(&tok), "the token is never printed");
        assert!(out.contains("any client can publish as the owner"));
        assert!(out.contains("hgl-ai client install --hub nats://192.0.2.10:4222 --token-file ~/.config/hgl-ai/hub.token --host <label>"));
        assert!(
            !std::fs::read_to_string(home.join(".config/systemd/user/hgl-ai-coord.service"))
                .unwrap()
                .contains("HGL_AI_HUB_CREDENTIAL"),
            "token mode: the coordinator uses hub.token"
        );
        install(&f, &hub, &ServerOpts::default()).await.unwrap();
        assert_eq!(token::read(&f, &tok_path).unwrap().unwrap(), tok, "kept");

        // Migrate; this host's bridge is installed and gets the new credential.
        std::fs::write(home.join(".config/systemd/user/hgl-ai-bridge.service"), "").unwrap();
        let o = ServerOpts {
            users: true,
            ..ServerOpts::default()
        };
        let cfg = install(&f, &hub, &o).await.unwrap();
        assert_eq!(cfg.server.unwrap().auth, HubAuth::Users);
        let c = token::read_credential(&f, &tok_path).unwrap().unwrap();
        assert_eq!(c.describe(), "hub user hub (seat)");
        assert!(
            !std::fs::read_to_string(&conf_path)
                .unwrap()
                .contains("token:")
        );
        assert!(
            f.runs()
                .contains(&"systemctl --user restart hgl-ai-bridge.service".to_string())
        );
    }

    #[tokio::test]
    async fn users_are_added_and_removed() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux);
        install(&f, &FakeHub::up(), &ServerOpts::default())
            .await
            .unwrap();
        let before = f.runs().len();
        let added = user_add(&f, "build-1", Role::Seat, None, false).unwrap();
        assert!(added.hub_restarted);
        let cred = added.cred.unwrap();
        assert_eq!(f.mode(&cred).unwrap(), 0o600);
        let c = token::read_credential(&f, &cred).unwrap().unwrap();
        assert_eq!(c.describe(), "hub user build-1 (seat)");
        let conf =
            std::fs::read_to_string(f.env.home.join(".local/share/hgl-ai/nats.conf")).unwrap();
        assert!(conf.contains("# build-1 (seat)"));
        assert_eq!(
            f.runs()[before..],
            ["systemctl --user restart hgl-ai-hub.service"]
        );
        assert!(
            user_add(&f, "build-1", Role::Seat, None, false).is_err(),
            "exists"
        );
        let key = user_list(&f)
            .unwrap()
            .into_iter()
            .find(|u| u.name == "build-1")
            .unwrap()
            .nkey;
        user_add(&f, "build-1", Role::Seat, None, true).unwrap();
        let rotated = user_list(&f)
            .unwrap()
            .into_iter()
            .find(|u| u.name == "build-1")
            .unwrap()
            .nkey;
        assert_ne!(key, rotated);
        assert!(user_remove(&f, "build-1").unwrap());
        assert!(!cred.exists());
        // A host that made its own key: only its public key is registered.
        let kp = nkeys::KeyPair::new_user();
        let by_key = user_add(&f, "host-2", Role::Seat, Some(&kp.public_key()), false).unwrap();
        assert!(by_key.cred.is_none());
        assert!(!users::cred_path(&f.env.config_dir(), "host-2").exists());
        let conf =
            std::fs::read_to_string(f.env.home.join(".local/share/hgl-ai/nats.conf")).unwrap();
        assert!(conf.contains(&format!(
            "{{ nkey: \"{}\", permissions: $SEAT }}  # host-2 (seat)",
            kp.public_key()
        )));
        assert!(f.output().contains(&users::fingerprint(&kp.public_key())));
        assert!(!user_remove(&f, "build-1").unwrap());
        let conf =
            std::fs::read_to_string(f.env.home.join(".local/share/hgl-ai/nats.conf")).unwrap();
        assert!(!conf.contains("build-1"));

        let g = fake(&d.path().join("other"), Os::Linux);
        assert!(
            user_add(&g, "x", Role::Seat, None, false).is_err(),
            "not a hub host"
        );
    }

    #[tokio::test]
    async fn console_service_with_its_own_user() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path(), Os::Linux);
        let o = ServerOpts {
            console_url: Some("https://hub.example.com".into()),
            console_users: vec!["owner@example.com=This Dude".into()],
            ..ServerOpts::default()
        };
        let cfg = install(&f, &FakeHub::up(), &o).await.unwrap();
        let c = cfg.console.unwrap();
        assert_eq!(
            (c.url.as_str(), c.users.as_slice()),
            (
                "https://hub.example.com",
                &[hgl_ai_config::User::person(
                    "owner@example.com",
                    "This Dude"
                )][..]
            )
        );
        let home = &f.env.home;
        let unit =
            std::fs::read_to_string(home.join(".config/systemd/user/hgl-ai-console.service"))
                .unwrap();
        assert!(unit.contains("hgl-ai console run"), "{unit}");
        assert!(unit.contains(&format!(
            "Environment=\"HGL_AI_HUB_CREDENTIAL={}\"",
            home.join(".config/hgl-ai/hub-users/console.cred").display()
        )));
        let o = ServerOpts {
            console: false,
            ..ServerOpts::default()
        };
        let d2 = tempfile::tempdir().unwrap();
        let g = fake(d2.path(), Os::Linux);
        install(&g, &FakeHub::up(), &o).await.unwrap();
        let reg = Registry::load(&g, &g.env.home.join(".local/share/hgl-ai")).unwrap();
        assert!(
            reg.get(CONSOLE_USER).is_none(),
            "--no-console: no console user"
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
