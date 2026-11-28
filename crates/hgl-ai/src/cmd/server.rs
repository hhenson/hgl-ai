//! `hgl-ai server`: install and run the hub (docs/install.md).

use hgl_ai_install::{self as install, BusHub, DryRun, Env, RealSystem, System, server};

use super::{Ctx, EXIT_FAIL, EXIT_OK};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Install nats-server, its configuration and token, and the hub service; provision the hub.
    Install(Box<InstallArgs>),
    /// Start the hub services.
    Start {
        /// Print the root steps of a system install; run none.
        #[arg(long)]
        print_root_steps: bool,
    },
    /// Stop the hub services.
    Stop {
        #[arg(long)]
        print_root_steps: bool,
    },
    /// Show whether the hub services run and the port answers.
    Status,
    /// Hub users: one per client, with its role's permissions (docs/install.md).
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Remove the hub services and nats.conf; keep the JetStream store unless --purge.
    Uninstall {
        /// Also remove the JetStream store and a downloaded nats-server (a system
        /// install: the server user and its home).
        #[arg(long)]
        purge: bool,
        #[arg(long)]
        print_root_steps: bool,
        /// Print what would change; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Subcommand)]
enum UserCmd {
    /// Add a hub user and write its credential (mode 0600) to copy to its host.
    Add {
        /// The user: a host label for a seat user.
        name: String,
        /// seat (a client host), coord, console or admin.
        #[arg(long, value_parser = |s: &str| s.parse::<install::users::Role>())]
        role: install::users::Role,
        /// Register this public NKey (U...) instead of making a credential: the
        /// user's seed stays on its own host.
        #[arg(long, value_name = "PUBKEY")]
        nkey: Option<String>,
        /// Give an existing user a new key; its old credential stops working.
        #[arg(long)]
        rotate: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// List the hub users.
    List,
    /// Remove a hub user; the hub refuses its credential from then on.
    Remove {
        name: String,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Args)]
struct InstallArgs {
    /// Address to listen on, such as this host's tailnet address (default 127.0.0.1).
    #[arg(long)]
    listen: Option<String>,
    /// Client port (default 4222).
    #[arg(long)]
    port: Option<u16>,
    /// JetStream store directory (default ~/.local/share/hgl-ai/nats).
    #[arg(long)]
    store: Option<String>,
    /// Host label when the configuration is new (default `hub`).
    #[arg(long)]
    host: Option<String>,
    /// Replace the existing token; every client then needs the new file.
    #[arg(long)]
    rotate_token: bool,
    /// Do not install the coordinator service.
    #[arg(long)]
    no_coord: bool,
    /// Move a single-token hub to a hub user per client (a new hub always has them).
    #[arg(long)]
    users: bool,
    /// Do not install the console service.
    #[arg(long)]
    no_console: bool,
    /// Install the Slack Q&A system service using the server's [slack] configuration.
    #[arg(long)]
    slack: bool,
    /// Configure the console: its origin, https://<stable name>[:port].
    #[arg(long, value_name = "URL")]
    console_url: Option<String>,
    /// The console's listen address (default 127.0.0.1:8443, behind a TLS proxy).
    #[arg(long, value_name = "ADDR:PORT")]
    console_listen: Option<String>,
    /// Who may sign in to the console: a GitHub login or an Apple email (repeatable).
    #[arg(long = "console-user", value_name = "NAME")]
    console_users: Vec<String>,
    /// Enable lingering (`loginctl enable-linger`) so services survive logout.
    #[arg(long)]
    linger: bool,
    /// Print what would change; change nothing.
    #[arg(long)]
    dry_run: bool,
    /// Run the server side as yourself, with user services (development and tests).
    /// Without it, the hub, coordinator and console run as their own OS user.
    #[arg(long)]
    same_user: bool,
    /// The server side's OS user (default hgl-ai; _hgl-ai on macOS).
    #[arg(long, value_name = "NAME")]
    server_user: Option<String>,
    /// That user's home (default /var/lib/hgl-ai; /Library/hgl-ai on macOS).
    #[arg(long, value_name = "DIR")]
    server_home: Option<std::path::PathBuf>,
    /// The OS user this host's client (bridge, agents) runs as, when not you: this
    /// host's client credential goes to its ~/.config/hgl-ai/hub.token, not to you.
    #[arg(long, value_name = "NAME")]
    client_user: Option<String>,
    /// Print the steps that need root as a script, and run none of them.
    #[arg(long)]
    print_root_steps: bool,
    /// Keep existing tokens in the server user's home without prompting.
    #[arg(long)]
    keep_token: bool,
    /// Write the configuration, credentials and nats.conf only (a system install's
    /// server-user step).
    #[arg(long, hide = true)]
    no_services: bool,
    #[arg(long, hide = true)]
    no_brew: bool,
    #[arg(long, hide = true, value_name = "user|system")]
    units: Option<String>,
    #[arg(long, hide = true, value_name = "FILE")]
    github_token_file: Option<String>,
    /// The `nats-server` the system install chose for the hub unit.
    #[arg(long, hide = true, value_name = "FILE")]
    nats_server: Option<std::path::PathBuf>,
}

impl InstallArgs {
    /// The flags the server user's step takes over.
    fn forwarded(&self) -> Vec<String> {
        let mut v = Vec::new();
        let mut opt = |k: &str, x: &Option<String>| {
            if let Some(x) = x {
                v.push(k.to_string());
                v.push(x.clone());
            }
        };
        opt("--listen", &self.listen);
        opt("--store", &self.store);
        opt("--console-url", &self.console_url);
        opt("--console-listen", &self.console_listen);
        if let Some(p) = self.port {
            v.push("--port".into());
            v.push(p.to_string());
        }
        for u in &self.console_users {
            v.push("--console-user".into());
            v.push(u.clone());
        }
        for (on, flag) in [
            (self.rotate_token, "--rotate-token"),
            (self.no_coord, "--no-coord"),
            (self.users, "--users"),
            (self.no_console, "--no-console"),
            (self.slack, "--slack"),
        ] {
            if on {
                v.push(flag.into());
            }
        }
        v
    }
}

/// The server user this client's configuration names (a system install), with the
/// hub's address for a port check.
fn server_user(sys: &dyn System) -> Option<(hgl_ai_config::ServerUser, Option<(String, u16)>)> {
    let cfg = install::load_config(sys, sys.env()).ok()??;
    let su = cfg.server_user?;
    let addr = cfg
        .hub
        .url
        .strip_prefix("nats://")
        .and_then(|r| r.rsplit_once(':'))
        .and_then(|(h, p)| Some((h.trim_matches(['[', ']']).to_string(), p.parse().ok()?)));
    Some((su, addr))
}

/// The host, for the commands of this work package.
pub(super) fn real_system(ctx: &Ctx) -> anyhow::Result<RealSystem> {
    Ok(RealSystem::new(Env::from_process(ctx.config_path()?)?))
}

/// Keep a bus error on top, so an unreachable hub exits 10.
pub(super) fn lift(e: install::Error) -> anyhow::Error {
    match e {
        install::Error::Bus(b) => b.into(),
        e => e.into(),
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    let real = real_system(ctx)?;
    let dry = DryRun::new(&real);
    let pick = |d: bool| -> &dyn System { if d { &dry } else { &real } };
    match args.cmd {
        Sub::Install(mut a) if !a.same_user => {
            let os = real.env().os;
            // A reinstall keeps what an earlier one set unless told otherwise.
            let prev = install::system::previous(&real);
            let host = a
                .host
                .clone()
                .or(prev.host)
                .unwrap_or_else(|| server::DEFAULT_HOST.to_string());
            let console_url = a.console_url.clone().or(prev.console_url);
            a.slack |= prev.slack && !a.no_console;
            let o = install::system::SystemOpts {
                user: a
                    .server_user
                    .clone()
                    .or(prev.user)
                    .unwrap_or_else(|| install::system::default_user(os).to_string()),
                home: a
                    .server_home
                    .clone()
                    .or(prev.home.map(std::path::PathBuf::from))
                    .unwrap_or_else(|| install::system::default_home(os)),
                print_only: a.print_root_steps || a.dry_run,
                host,
                inner_args: a.forwarded(),
                listen: a
                    .listen
                    .clone()
                    .or(prev.listen)
                    .unwrap_or_else(|| "127.0.0.1".into()),
                port: a.port.or(prev.port).unwrap_or(4222),
                coord: !a.no_coord,
                console: !a.no_console && console_url.is_some(),
                slack: a.slack,
                console_url,
                client_user: a.client_user.clone().or(prev.client_user),
                keep_token: a.keep_token,
                wait: std::time::Duration::from_secs(install::HUB_WAIT_SECS),
            };
            if let Some(script) = install::system::install(&real, &o).map_err(lift)? {
                println!("{script}");
            }
        }
        Sub::Install(a) => {
            let units = match a.units.as_deref() {
                Some("system") => hgl_ai_config::Units::System,
                _ => hgl_ai_config::Units::User,
            };
            let o = server::ServerOpts {
                host: a.host,
                listen: a.listen,
                port: a.port,
                store: a.store,
                rotate_token: a.rotate_token,
                users: a.users,
                coord: !a.no_coord,
                console: !a.no_console,
                slack: a.slack,
                console_url: a.console_url,
                console_listen: a.console_listen,
                console_users: a.console_users,
                linger: a.linger,
                services: !a.no_services,
                brew: !a.no_brew,
                units,
                github_token_file: a.github_token_file,
                nats_server: a.nats_server,
                ..server::ServerOpts::default()
            };
            server::install(pick(a.dry_run), &BusHub, &o)
                .await
                .map_err(lift)?;
        }
        Sub::Start { print_root_steps } if server_user(&real).is_some() => {
            if let Some(s) =
                install::system::control(&real, "start", print_root_steps).map_err(lift)?
            {
                println!("{s}");
            }
        }
        Sub::Stop { print_root_steps } if server_user(&real).is_some() => {
            if let Some(s) =
                install::system::control(&real, "stop", print_root_steps).map_err(lift)?
            {
                println!("{s}");
            }
        }
        Sub::Start { .. } => server::start(&real).map_err(lift)?,
        Sub::Stop { .. } => server::stop(&real).map_err(lift)?,
        Sub::Status => {
            let up = match server_user(&real) {
                Some((su, url)) => install::system::status(&real, &su, url),
                None => server::status(&real).map_err(lift)?,
            };
            return Ok(if up { EXIT_OK } else { EXIT_FAIL });
        }
        Sub::User { cmd } => match cmd {
            UserCmd::Add {
                name,
                role,
                nkey,
                rotate,
                dry_run,
            } => {
                server::user_add(pick(dry_run), &name, role, nkey.as_deref(), rotate)
                    .map_err(lift)?;
            }
            UserCmd::List => {
                let users = server::user_list(&real).map_err(lift)?;
                if ctx.json {
                    let rows: Vec<_> = users
                        .iter()
                        .map(|u| serde_json::json!({"name": u.name, "role": u.role, "created_at": u.created_at}))
                        .collect();
                    println!("{}", serde_json::Value::Array(rows));
                } else {
                    for u in users {
                        println!("{}\t{}\t{}", u.name, u.role, u.created_at);
                    }
                }
            }
            UserCmd::Remove { name, dry_run } => {
                if !server::user_remove(pick(dry_run), &name).map_err(lift)? {
                    eprintln!("hgl-ai: no hub user {name}");
                    return Ok(EXIT_FAIL);
                }
            }
        },
        Sub::Uninstall {
            purge,
            print_root_steps,
            dry_run,
        } if server_user(&real).is_some() => {
            let (su, _) = server_user(&real).expect("checked");
            if let Some(s) =
                install::system::uninstall(&real, &su, purge, print_root_steps || dry_run)
                    .map_err(lift)?
            {
                println!("{s}");
            }
        }
        Sub::Uninstall { purge, dry_run, .. } => {
            server::uninstall(pick(dry_run), purge).map_err(lift)?
        }
    }
    Ok(EXIT_OK)
}
