//! `hgl-ai bridge run [--seat NAME ...]`: the bridge daemon (docs/design.md §2, §4).

use std::sync::Arc;

use hgl_ai_bridge::{Bridge, Deps, Options, Statvfs};
use hgl_ai_bus::Bus;
use hgl_ai_proto::Seat;
use hgl_ai_tree::{Git, Trees};
use tokio::sync::watch;

use super::{Ctx, EXIT_OK};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Serve this host's seats until SIGTERM or SIGINT: create task trees, run one
    /// headless agent turn per batch of notes, remove trees on `landed` and `closed`.
    Run(RunArgs),
}

#[derive(clap::Args)]
struct RunArgs {
    /// Serve only this seat (repeatable); default: every configured seat except
    /// controllers, which are interactive.
    #[arg(long = "seat", value_name = "NAME")]
    seats: Vec<Seat>,
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    match args.cmd {
        Cmd::Run(a) => serve(ctx, a).await,
    }
}

async fn serve(ctx: &Ctx, a: RunArgs) -> anyhow::Result<i32> {
    let cfg = ctx.load_config()?;
    let bus = Bus::connect(&cfg).await?;
    let trees = Trees::new(&cfg, Git::default())?;
    let deps = Deps {
        closed: Arc::new(super::tree::GhClosed::new(&cfg)),
        disk: Arc::new(Statvfs),
    };
    let opts = Options::from_config(&cfg);
    let bridge = Bridge::new(cfg, bus, trees, deps, opts);
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("stopping: finishing running turns within the grace");
        let _ = tx.send(true);
    });
    bridge.run(&a.seats, rx).await?;
    Ok(EXIT_OK)
}

pub(super) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
