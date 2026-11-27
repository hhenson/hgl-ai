//! `hgl-ai state`, `hgl-ai init`, `hgl-ai hosts` (docs/protocol.md §8, §9).

use anyhow::Context as _;
use hgl_ai_bus::{Bus, BusError, MAX_VALUE_BYTES};
use hgl_ai_proto::{
    HostRecord, KV_CONSOLE, KV_HOSTS, KV_LOCKS, KV_METRICS, KV_STATE, KV_THREADS, KV_TREES,
    STREAM_NOTES, STREAM_OWNER,
};

use super::note::read_bounded;
use super::{Ctx, EXIT_FAIL, EXIT_OK, NotReady};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Print a value; exit status 1 when the key is missing.
    Get { key: String },
    /// Write a value (`-`: standard input); an empty value deletes. Prints the revision.
    Put {
        key: String,
        value: String,
        /// Write only if the key is still at revision N (0: only if absent);
        /// exit status 3 when another writer got there first.
        #[arg(long, value_name = "N")]
        if_revision: Option<u64>,
    },
    /// Append one line (`-`: standard input), retrying against other writers. Prints the revision.
    Append { key: String, line: String },
    /// Delete a key.
    Del { key: String },
}

async fn connect(ctx: &Ctx) -> anyhow::Result<Bus> {
    Ok(Bus::connect(&ctx.load_config()?).await?)
}

fn arg_or_stdin(v: String) -> anyhow::Result<String> {
    if v == "-" {
        read_bounded(std::io::stdin().lock(), "standard input", MAX_VALUE_BYTES)
    } else {
        Ok(v)
    }
}

/// A lost compare-and-set is a gate not satisfied (exit status 3).
fn conflict_is_not_ready(e: BusError) -> anyhow::Error {
    match e {
        BusError::Conflict { .. } => NotReady(e.to_string()).into(),
        other => other.into(),
    }
}

fn print_revision(ctx: &Ctx, key: &str, revision: u64) {
    if ctx.json {
        println!(
            "{}",
            serde_json::json!({ "key": key, "revision": revision })
        );
    } else if revision == 0 {
        println!("deleted");
    } else {
        println!("revision {revision}");
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    match args.cmd {
        Cmd::Get { key } => {
            let Some(e) = connect(ctx).await?.state_get(&key).await? else {
                return Ok(EXIT_FAIL);
            };
            if ctx.json {
                println!(
                    "{}",
                    serde_json::json!({ "key": key, "value": e.value, "revision": e.revision })
                );
            } else if e.value.ends_with('\n') {
                print!("{}", e.value);
            } else {
                println!("{}", e.value);
            }
        }
        Cmd::Put {
            key,
            value,
            if_revision,
        } => {
            let value = arg_or_stdin(value)?;
            let bus = connect(ctx).await?;
            let rev = match if_revision {
                Some(n) => bus
                    .state_update(&key, &value, n)
                    .await
                    .map_err(conflict_is_not_ready)?,
                None => bus.state_put(&key, &value).await?,
            };
            print_revision(ctx, &key, rev);
        }
        Cmd::Append { key, line } => {
            let line = arg_or_stdin(line)?;
            let rev = connect(ctx)
                .await?
                .append_line(&key, &line)
                .await
                .map_err(conflict_is_not_ready)?;
            print_revision(ctx, &key, rev);
        }
        Cmd::Del { key } => connect(ctx).await?.state_del(&key).await?,
    }
    Ok(EXIT_OK)
}

pub async fn init(ctx: &Ctx) -> anyhow::Result<i32> {
    let bus = connect(ctx).await?;
    let who = bus.who().to_string();
    bus.provision().await.with_context(|| {
        if who.ends_with("(admin)") || who == "shared token" {
            "provisioning the hub".to_string()
        } else {
            format!(
                "provisioning the hub as {who}: it takes an admin hub user, \
                 `HGL_AI_HUB_CREDENTIAL=~/.config/hgl-ai/hub-users/<hub>-admin.cred hgl-ai init`"
            )
        }
    })?;
    let names = [
        STREAM_NOTES,
        STREAM_OWNER,
        KV_STATE,
        KV_HOSTS,
        KV_TREES,
        KV_LOCKS,
        KV_THREADS,
        KV_METRICS,
        KV_CONSOLE,
    ];
    if ctx.json {
        println!("{}", serde_json::json!({ "provisioned": names }));
    } else {
        println!("provisioned {}", names.join(" "));
    }
    Ok(EXIT_OK)
}

pub async fn hosts(ctx: &Ctx) -> anyhow::Result<i32> {
    for h in connect(ctx).await?.hosts().await? {
        if ctx.json {
            println!("{}", serde_json::to_string(&h)?);
        } else {
            println!("{}", host_line(&h));
        }
    }
    Ok(EXIT_OK)
}

/// `build-1 at STAMP memcap 64G free 812G: w3 implementor/codex/gpt-6.1-sol, ...`
fn host_line(h: &HostRecord) -> String {
    let seats: Vec<String> = h
        .seats
        .iter()
        .map(|s| format!("{} {}/{}/{}", s.seat, s.role, s.runtime, s.model))
        .collect();
    let seats = if seats.is_empty() {
        "no seats".to_string()
    } else {
        seats.join(", ")
    };
    format!(
        "{} at {} memcap {}G free {}G: {seats}",
        h.host, h.updated_at, h.memcap_gb, h.work_root_free_gb
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hgl_ai_proto::{Family, Role, Runtime, Seat, SeatRecord};

    #[test]
    fn host_lines() {
        let mut h = HostRecord {
            host: "build-1".into(),
            seats: vec![SeatRecord {
                seat: Seat::new("w3").unwrap(),
                role: Role::Implementor,
                runtime: Runtime::Codex,
                family: Family::Gpt,
                model: "gpt-6.1-sol".into(),
            }],
            memcap_gb: 64,
            work_root_free_gb: 812,
            updated_at: "20261127T083000Z".into(),
        };
        assert_eq!(
            host_line(&h),
            "build-1 at 20261127T083000Z memcap 64G free 812G: w3 implementor/codex/gpt-6.1-sol"
        );
        h.seats.clear();
        assert!(host_line(&h).ends_with(": no seats"));
    }
}
