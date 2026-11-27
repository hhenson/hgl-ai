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
    Install(InstallArgs),
    /// Start the hub services.
    Start,
    /// Stop the hub services.
    Stop,
    /// Show whether the hub services run and the port answers.
    Status,
    /// Hub users: one per client, with its role's permissions (docs/install.md).
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Remove the hub services and nats.conf; keep the JetStream store unless --purge.
    Uninstall {
        /// Also remove the JetStream store and a downloaded nats-server.
        #[arg(long)]
        purge: bool,
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
        Sub::Install(a) => {
            let o = server::ServerOpts {
                host: a.host,
                listen: a.listen,
                port: a.port,
                store: a.store,
                rotate_token: a.rotate_token,
                users: a.users,
                coord: !a.no_coord,
                console: !a.no_console,
                console_url: a.console_url,
                console_listen: a.console_listen,
                console_users: a.console_users,
                linger: a.linger,
                ..server::ServerOpts::default()
            };
            server::install(pick(a.dry_run), &BusHub, &o)
                .await
                .map_err(lift)?;
        }
        Sub::Start => server::start(&real).map_err(lift)?,
        Sub::Stop => server::stop(&real).map_err(lift)?,
        Sub::Status => {
            return Ok(if server::status(&real).map_err(lift)? {
                EXIT_OK
            } else {
                EXIT_FAIL
            });
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
        Sub::Uninstall { purge, dry_run } => {
            server::uninstall(pick(dry_run), purge).map_err(lift)?
        }
    }
    Ok(EXIT_OK)
}
