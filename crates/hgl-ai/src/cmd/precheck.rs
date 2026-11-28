use super::{Ctx, EXIT_FAIL, EXIT_OK, NotReady};
use anyhow::{Context, bail};
use hgl_ai_bus::{Bus, BusError, MAX_VALUE_BYTES};
use hgl_ai_proto::precheck::{RuleList, StoredRules};
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Command,
}
#[derive(clap::Subcommand)]
enum Command {
    Rules(RulesArgs),
}
#[derive(clap::Args)]
struct RulesArgs {
    #[command(subcommand)]
    cmd: RulesCommand,
}
#[derive(clap::Subcommand)]
enum RulesCommand {
    /// Read the current rules and revision.
    Get,
    /// Replace rules using the owner-held writer credential.
    Set {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        if_revision: u64,
        #[arg(long)]
        reason: String,
    },
}
pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    let Command::Rules(args) = args.cmd;
    match args.cmd {
        RulesCommand::Get => {
            let Some((value, revision)) = Bus::connect(&ctx.load_config()?)
                .await?
                .precheck_rules_get()
                .await?
            else {
                return Ok(EXIT_FAIL);
            };
            let stored: StoredRules =
                serde_json::from_str(&value).context("invalid stored pre-check rules")?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"revision": revision, "reason": stored.reason, "rules": stored.rules})
                )?
            );
        }
        RulesCommand::Set {
            file,
            if_revision,
            reason,
        } => {
            if reason.trim().is_empty() || reason.contains(['\n', '\r']) {
                bail!("reason must be one non-empty line");
            }
            let f = std::fs::File::open(&file)
                .with_context(|| format!("opening {}", file.display()))?;
            let text = super::note::read_bounded(f, "rule file", MAX_VALUE_BYTES)?;
            let list: RuleList = serde_json::from_str(&text).context("invalid rule file")?;
            list.validate().map_err(anyhow::Error::msg)?;
            let value = serde_json::to_string(&StoredRules {
                reason,
                rules: list.rules,
            })?;
            let rev = Bus::connect(&ctx.load_config()?)
                .await?
                .precheck_rules_set(&value, if_revision)
                .await
                .map_err(|e| match e {
                    BusError::Conflict { .. } => anyhow::Error::from(NotReady(e.to_string())),
                    _ => e.into(),
                })?;
            if ctx.json {
                println!("{}", serde_json::json!({"revision": rev}));
            } else {
                println!("revision {rev}");
            }
        }
    }
    Ok(EXIT_OK)
}
