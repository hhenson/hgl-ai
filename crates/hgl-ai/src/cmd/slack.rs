use anyhow::{Context as _, ensure};
use hgl_ai_bus::{Bus, read_credential};
use hgl_ai_config::{HubAuth, Units, expand};

use super::Ctx;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Run the Slack Q&A system service.
    Run,
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    match args.cmd {
        Sub::Run => {
            let cfg = ctx.load_config()?;
            let path = cfg.token_file()?;
            let slack = cfg
                .slack
                .context("missing [slack] in the server configuration")?;
            ensure!(
                cfg.server
                    .is_some_and(|s| s.auth == HubAuth::Users && s.units == Units::System),
                "Slack needs a system server install with hub users"
            );
            let credential = read_credential(&path)?.context("missing Slack hub credential")?;
            ensure!(
                credential.role() == Some("slack"),
                "Slack needs its own hub user"
            );
            let bus = Bus::connect_with(&cfg.hub.url, Some(credential)).await?;
            hgl_ai_slack::run(&bus, slack, &expand("~/.config/hgl-ai")?).await?;
        }
    }
    Ok(super::EXIT_OK)
}
