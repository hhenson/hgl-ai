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
                coord: !a.no_coord,
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
        Sub::Uninstall { purge, dry_run } => {
            server::uninstall(pick(dry_run), purge).map_err(lift)?
        }
    }
    Ok(EXIT_OK)
}
