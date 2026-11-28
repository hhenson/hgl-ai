//! Controller commands for the screen's hub records.

use anyhow::{Context, ensure};
use hgl_ai_bus::{Bus, BusError, Credential, ScreenRuleEntry, read_credential};
use hgl_ai_owner as owner;
use hgl_ai_proto::{
    screen::{ScreenMode, ScreenRule, check_rule_id},
    utc_now_stamp,
};

use super::{Ctx, EXIT_OK, note::read_bounded};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Manage the controller's screen rules.
    Rule {
        #[command(subcommand)]
        cmd: RuleCmd,
    },
    /// Read the screen's decisions in original note sequence order.
    Log {
        /// Read entries strictly after this HGL_NOTES sequence.
        #[arg(long, default_value_t = 0)]
        since: u64,
    },
}

#[derive(clap::Subcommand)]
enum RuleCmd {
    /// Add a shadow rule from one JSON object (use - for standard input).
    Add { file: String },
    /// Retire a rule, keeping its record and history.
    Retire { id: String },
    /// List the rules by ID, including expired and retired rules.
    List,
    /// Show a rule by ID.
    Show { id: String },
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    let cfg = ctx.load_config()?;
    let cred = read_credential(&cfg.token_file()?)?;
    ensure!(
        matches!(&cred, Some(Credential::Nkey { role, .. }) if role.as_deref().is_none_or(|r| r == "screen-rules")),
        "screen commands need the controller's screen-rules NKey (HGL_AI_HUB_CREDENTIAL)"
    );
    let bus = Bus::connect_with(&cfg.hub.url, cred).await?;
    bus.screen_check_controller().await?;
    match args.cmd {
        Cmd::Rule { cmd } => match cmd {
            RuleCmd::Add { file } => {
                let rule = read_rule(&file)?;
                add(&bus, &rule).await?;
                print_json(ctx, &rule)?;
            }
            RuleCmd::Retire { id } => {
                let rule = retire(&bus, &id).await?;
                print_json(ctx, &rule)?;
            }
            RuleCmd::List => {
                let rules = bus.screen_rules().await?;
                if ctx.json {
                    print_json(ctx, &rules)?;
                } else {
                    let now = chrono::Utc::now();
                    for rule in rules {
                        let mode = match rule.mode {
                            ScreenMode::Shadow => "shadow",
                            ScreenMode::Act => "act",
                        };
                        let state = if rule.retired_at.is_some() {
                            "retired"
                        } else if rule.is_active(now) {
                            "active"
                        } else {
                            "expired"
                        };
                        println!("{}\t{mode}\t{state}\t{}", rule.id, rule.expires);
                    }
                }
            }
            RuleCmd::Show { id } => {
                let entry = bus
                    .screen_rule_get(&id)
                    .await?
                    .with_context(|| format!("no screen rule {id}"))?;
                print_json(ctx, &entry.rule)?;
            }
        },
        Cmd::Log { since } => {
            let log = bus.screen_log(since).await?;
            if ctx.json {
                print_json(ctx, &log)?;
            } else {
                for entry in log {
                    println!("{}", serde_json::to_string(&entry)?);
                }
            }
        }
    }
    Ok(EXIT_OK)
}

fn print_json(ctx: &Ctx, value: &impl serde::Serialize) -> anyhow::Result<()> {
    let text = if ctx.json {
        serde_json::to_string(value)?
    } else {
        serde_json::to_string_pretty(value)?
    };
    println!("{text}");
    Ok(())
}

fn read_rule(file: &str) -> anyhow::Result<ScreenRule> {
    let text = if file == "-" {
        read_bounded(
            std::io::stdin().lock(),
            "standard input",
            hgl_ai_bus::MAX_VALUE_BYTES,
        )?
    } else {
        let input = std::fs::File::open(file).with_context(|| format!("opening {file}"))?;
        read_bounded(input, file, hgl_ai_bus::MAX_VALUE_BYTES)?
    };
    let rule: ScreenRule = serde_json::from_str(&text).context("reading screen rule JSON")?;
    rule.validate()?;
    ensure!(
        rule.mode == ScreenMode::Shadow && rule.retired_at.is_none(),
        "a new screen rule must be in shadow mode and live"
    );
    Ok(rule)
}

async fn add(bus: &Bus, rule: &ScreenRule) -> anyhow::Result<()> {
    if let Some(entry) = bus.screen_rule_get(&rule.id).await? {
        ensure!(
            entry.rule == *rule,
            "screen rule {} already exists",
            rule.id
        );
        return record_decision(bus, &entry).await;
    }
    ensure!(
        rule.is_active(chrono::Utc::now()),
        "a new screen rule must expire in the future"
    );
    if owner::parse_issue(&rule.cites).is_none() {
        let decisions = owner::decisions_tail(bus, owner::DECISIONS_KEEP).await?;
        ensure!(
            decisions.iter().any(|line| line == &rule.cites),
            "cites must be repo#number or an exact line from decisions.log"
        );
    }
    change(bus, rule.clone(), 0, utc_now_stamp())
        .await
        .map(|_| ())
}

async fn retire(bus: &Bus, id: &str) -> anyhow::Result<ScreenRule> {
    check_rule_id(id)?;
    loop {
        let mut entry = bus
            .screen_rule_get(id)
            .await?
            .with_context(|| format!("no screen rule {id}"))?;
        if entry.decision_at.is_some() {
            record_decision(bus, &entry).await?;
            continue;
        }
        if entry.rule.retired_at.is_some() {
            return Ok(entry.rule);
        }
        let stamp = utc_now_stamp();
        entry.rule.retired_at = Some(stamp.clone());
        return change(bus, entry.rule, entry.revision, stamp).await;
    }
}

async fn change(
    bus: &Bus,
    rule: ScreenRule,
    revision: u64,
    stamp: String,
) -> anyhow::Result<ScreenRule> {
    let revision = bus
        .screen_rule_update(&rule, revision, Some(&stamp))
        .await?;
    let entry = ScreenRuleEntry {
        rule,
        revision,
        decision_at: Some(stamp),
    };
    record_decision(bus, &entry).await?;
    Ok(entry.rule)
}

async fn record_decision(bus: &Bus, entry: &ScreenRuleEntry) -> anyhow::Result<()> {
    let Some(stamp) = &entry.decision_at else {
        return Ok(());
    };
    let action = if entry.rule.retired_at.is_some() {
        "retire"
    } else {
        "add"
    };
    let id = &entry.rule.id;
    let line = format!(
        "{stamp} screen rule {action} {id} (revision {})",
        entry.revision
    );
    owner::decision_once(bus, &line).await.with_context(|| {
        format!("recording screen rule {action} {id} in decisions.log; retry this command")
    })?;
    match bus
        .screen_rule_update(&entry.rule, entry.revision, None)
        .await
    {
        Ok(_) | Err(BusError::Conflict { .. }) => Ok(()),
        Err(e) => Err(e)
            .with_context(|| format!("finishing screen rule {action} {id}; retry this command")),
    }
}
