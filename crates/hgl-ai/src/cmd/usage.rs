//! `hgl-ai usage collect|show|label` (docs/protocol.md PROTO-23, PROTO-24).

use hgl_ai_bus::{Bus, UsagePut};
use hgl_ai_proto::Runtime;
use hgl_ai_usage::{Collected, Sources};

use super::{Ctx, EXIT_FAIL, EXIT_OK};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Read this host's signed-in CLIs (slash commands only, no model turn) and publish one
    /// record per login.
    Collect {
        /// Only this runtime (repeatable): claude, codex or agy.
        #[arg(long = "runtime", value_name = "RUNTIME")]
        runtimes: Vec<Runtime>,
        /// Print the records as JSON instead of publishing them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Every login's limits across hosts and runtimes, with hours to reset.
    Show,
    /// Give an account fingerprint a label; an empty label removes it.
    Label { account: String, label: String },
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    match args.cmd {
        Cmd::Collect { runtimes, dry_run } => collect(ctx, &runtimes, dry_run).await,
        Cmd::Show => show(ctx).await,
        Cmd::Label { account, label } => {
            let label = Some(label.trim()).filter(|l| !l.is_empty());
            let bus = Bus::connect(&ctx.load_config()?).await?;
            bus.usage_label(&account, label).await?;
            match label {
                Some(l) => println!("{account} labelled {l}"),
                None => println!("{account} unlabelled"),
            }
            Ok(EXIT_OK)
        }
    }
}

async fn collect(ctx: &Ctx, runtimes: &[Runtime], dry_run: bool) -> anyhow::Result<i32> {
    let cfg = ctx.load_config()?;
    let got = hgl_ai_usage::collect(&Sources::from_env(), runtimes, &cfg.host).await;
    if dry_run {
        for c in &got {
            match c {
                Collected::Record(r) => println!("{}", serde_json::to_string(r)?),
                Collected::Skipped { runtime, why } => eprintln!("{runtime} skipped: {why}"),
            }
        }
        return Ok(EXIT_OK);
    }
    let bus = Bus::connect(&cfg).await?;
    let labels = bus.usage_labels().await?;
    let mut code = EXIT_OK;
    for c in got {
        let (line, json) = match c {
            Collected::Skipped { runtime, why } => (
                format!("{runtime} skipped: {why}"),
                serde_json::json!({ "runtime": runtime, "outcome": "skipped", "why": why }),
            ),
            Collected::Record(mut r) => {
                r.label = labels.get(&r.account).cloned();
                let key = r.key();
                match bus.usage_put(&r).await {
                    Ok(UsagePut::Written { revision, .. }) => (
                        format!(
                            "{key}: {} bucket{}, written",
                            r.buckets.len(),
                            if r.buckets.len() == 1 { "" } else { "s" }
                        ),
                        serde_json::json!({ "key": key, "outcome": "written", "revision": revision }),
                    ),
                    Ok(UsagePut::Stale { host, observed_at }) => (
                        format!(
                            "{key}: the hub holds a newer observation ({observed_at} from {host})"
                        ),
                        serde_json::json!({ "key": key, "outcome": "stale", "host": host, "observed_at": observed_at }),
                    ),
                    Err(e) => {
                        code = EXIT_FAIL;
                        (
                            format!("{key}: not written: {e}"),
                            serde_json::json!({ "key": key, "outcome": "failed", "why": e.to_string() }),
                        )
                    }
                }
            }
        };
        if ctx.json {
            println!("{json}");
        } else {
            println!("{line}");
        }
    }
    Ok(code)
}

async fn show(ctx: &Ctx) -> anyhow::Result<i32> {
    let records = Bus::connect(&ctx.load_config()?).await?.usage().await?;
    let now = chrono::Utc::now();
    if !ctx.json {
        if records.is_empty() {
            println!("no usage records; run `hgl-ai usage collect`");
        } else {
            print!("{}", hgl_ai_usage::table(&records, now));
        }
        return Ok(EXIT_OK);
    }
    for r in &records {
        let mut v = serde_json::to_value(r)?;
        if let Some(bs) = v["buckets"].as_array_mut() {
            for (b, rb) in bs.iter_mut().zip(&r.buckets) {
                b["hours_to_reset"] =
                    serde_json::json!(hgl_ai_usage::hours_to(rb.resets_at.as_deref(), now));
            }
        }
        println!("{v}");
    }
    Ok(EXIT_OK)
}
