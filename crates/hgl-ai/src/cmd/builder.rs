//! `hgl-ai builder`: the builder service of this host (and of the remotes it drives as a
//! proxy), the fleet's builders, and draining or retiring one (docs/build.md).

use std::sync::Arc;

use hgl_ai_build::service::{self, Settings, gate_key};
use hgl_ai_bus::Bus;
use tokio::sync::watch;

use super::{Ctx, EXIT_OK};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Take `build` jobs until SIGTERM or SIGINT (the user service runs this).
    Run,
    /// The fleet's builders and their health.
    List,
    /// Take no new jobs on builder LABEL; running ones finish.
    Drain { label: String },
    /// Drain builder LABEL and drop it from the list until `resume`.
    Retire { label: String },
    /// Take jobs again.
    Resume { label: String },
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    let cfg = ctx.load_config()?;
    // The builder's own hub user, where it has one: it alone writes the verdicts.
    let builder_cred =
        match (&args.cmd, &cfg.builder.credential) {
            (Cmd::Run, Some(p)) => {
                let path = hgl_ai_config::expand(p)?;
                Some(hgl_ai_bus::read_credential(&path)?.ok_or_else(|| {
                    anyhow::anyhow!("[builder] credential {}: empty", path.display())
                })?)
            }
            _ => None,
        };
    let bus = match &builder_cred {
        Some(c) => Bus::connect_with(&cfg.hub.url, Some(c.clone())).await?,
        None => Bus::connect(&cfg).await?,
    };
    match args.cmd {
        Cmd::Run => {
            // A `builder` hub user writes verdicts; so does any client of a hub with one
            // shared token. A seat's user does not (docs/build.md BUILD-15). The hub, not
            // the file, says which: a bare NKey seed names no role.
            let token = match &builder_cred {
                Some(c) => c.is_token(),
                None => {
                    hgl_ai_bus::read_credential(&cfg.token_file()?)?.is_some_and(|c| c.is_token())
                }
            };
            let verdicts =
                token || hgl_ai_build::store::may_write_verdicts(&bus, &cfg.host).await?;
            if !verdicts {
                tracing::warn!(
                    "no builder hub user ([builder] credential): gate builds are reported not run"
                );
            }
            let lanes = service::lanes(&cfg, service::ssh_program())?;
            let github = cfg.github.clone();
            let settings = Settings {
                clone_url: Arc::new(move |r: &str| github.clone_url_for(r)),
                toolchains: cfg.builder.toolchains.clone(),
                heartbeat: service::HEARTBEAT,
                verdicts,
            };
            let (tx, rx) = watch::channel(false);
            tokio::spawn(async move {
                super::bridge::shutdown_signal().await;
                tracing::info!("stopping: cancelling running builds");
                let _ = tx.send(true);
            });
            service::serve(Arc::new(bus), lanes, settings, rx).await?;
        }
        Cmd::List => {
            for b in bus.builders().await? {
                if ctx.json {
                    println!("{}", serde_json::to_string(&b)?);
                    continue;
                }
                let via = b
                    .proxy
                    .as_deref()
                    .map(|p| format!(" via {p}"))
                    .unwrap_or_default();
                let health = if b.reachable {
                    "reachable".to_string()
                } else {
                    format!("unreachable ({})", b.reason.as_deref().unwrap_or("?"))
                };
                let free = b.free_gb.map(|g| format!(" free {g}G")).unwrap_or_default();
                let tools: Vec<String> = b
                    .toolchains
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect();
                println!(
                    "{} {} {}{via} {} {health} busy {}/{}{free}{}",
                    b.label,
                    b.platform,
                    b.executor,
                    b.state,
                    b.busy,
                    b.slots,
                    if tools.is_empty() {
                        String::new()
                    } else {
                        format!("; {}", tools.join("; "))
                    }
                );
            }
        }
        Cmd::Drain { label } => {
            bus.state_put(&gate_key(&label), "drain").await?;
            println!("builder {label}: draining");
        }
        Cmd::Retire { label } => {
            bus.state_put(&gate_key(&label), "retire").await?;
            println!("builder {label}: retiring");
        }
        Cmd::Resume { label } => {
            bus.state_del(&gate_key(&label)).await?;
            println!("builder {label}: active");
        }
    }
    Ok(EXIT_OK)
}
