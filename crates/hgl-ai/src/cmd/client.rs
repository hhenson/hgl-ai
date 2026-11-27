//! `hgl-ai client`: configure this host as a client of a hub (docs/install.md).

use hgl_ai_install::{BusHub, DryRun, System, client};

use super::server::{lift, real_system};
use super::{Ctx, EXIT_OK, EXIT_UNREACHABLE};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Write config.toml, copy the token, check the hub, install the skill and the bridge
    /// and builder services.
    Install(InstallArgs),
    /// Remove the bridge service and the skills; keep config.toml and the token unless --purge.
    Uninstall {
        /// Also remove config.toml and the token copy.
        #[arg(long)]
        purge: bool,
        /// Print what would change; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Args)]
struct InstallArgs {
    /// Hub URL, `nats://<host>:<port>`.
    #[arg(long)]
    hub: String,
    /// The hub token file copied to this host; it is copied beside config.toml with mode 0600.
    #[arg(long)]
    token_file: String,
    /// This host's label: [a-z0-9-], never a network name.
    #[arg(long)]
    host: String,
    /// Where task directories live (default ~/hgl-ai/work).
    #[arg(long)]
    work_root: Option<String>,
    /// Enable lingering (`loginctl enable-linger`) so services survive logout.
    #[arg(long)]
    linger: bool,
    /// Do not install the builder service.
    #[arg(long)]
    no_builder: bool,
    /// Print what would change; change nothing.
    #[arg(long)]
    dry_run: bool,
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    let real = real_system(ctx)?;
    let dry = DryRun::new(&real);
    let pick = |d: bool| -> &dyn System { if d { &dry } else { &real } };
    match args.cmd {
        Sub::Install(a) => {
            let o = client::ClientOpts {
                hub: a.hub,
                token_file: a.token_file,
                host: a.host,
                work_root: a.work_root,
                linger: a.linger,
                builder: !a.no_builder,
            };
            let out = client::install(pick(a.dry_run), &BusHub, &o)
                .await
                .map_err(lift)?;
            if let Some(e) = out.hub_error {
                eprintln!("hgl-ai: {e}");
                return Ok(EXIT_UNREACHABLE);
            }
        }
        Sub::Uninstall { purge, dry_run } => {
            client::uninstall(pick(dry_run), purge).map_err(lift)?
        }
    }
    Ok(EXIT_OK)
}
