//! `hgl-ai console run`: the owner's web console (docs/owner.md); the hub host's user
//! service runs it with the console's own hub user.

use super::Ctx;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Serve the console on `[console] listen` until stopped.
    Run,
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    match args.cmd {
        Cmd::Run => {
            let cfg = ctx.load_config()?;
            hgl_ai_console::service::run(cfg).await?;
            Ok(super::EXIT_OK)
        }
    }
}
