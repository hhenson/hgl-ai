//! `hgl-ai seat add|list|remove`: this host's seats in `config.toml` (docs/design.md §3).

use anyhow::{Context as _, bail};
use hgl_ai_config::{Binding, Config, SeatConfig};
use hgl_ai_proto::{Family, Role, Runtime, Seat, Tier};

use super::{Ctx, EXIT_OK};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Add a seat. The running bridge serves it after a restart.
    Add(AddArgs),
    /// List the seats with their resolved models.
    List,
    /// Remove a seat.
    Remove { name: Seat },
}

#[derive(clap::Args)]
#[command(group = clap::ArgGroup::new("choice").required(true).args(["tier", "model"]))]
struct AddArgs {
    name: Seat,
    /// controller, triage, implementor or reviewer.
    #[arg(long)]
    role: Role,
    /// claude, codex or agy.
    #[arg(long)]
    runtime: Runtime,
    /// frontier, balanced or fast; the model comes from `[models.<family>]`.
    #[arg(long)]
    tier: Option<Tier>,
    /// claude, gpt or gemini (default: the runtime's own).
    #[arg(long, requires = "tier")]
    family: Option<Family>,
    /// Pin one model slug instead of a tier.
    #[arg(long, value_name = "SLUG")]
    model: Option<String>,
    /// Reasoning effort: low, medium, high, xhigh or max.
    #[arg(long, value_name = "E")]
    effort: Option<String>,
    /// A binding to move to after a quota refusal, in order (repeatable):
    /// RUNTIME:FAMILY:TIER, optionally :EFFORT.
    #[arg(long, value_name = "RUNTIME:FAMILY:TIER", value_parser = fallback)]
    fallback: Vec<Binding>,
}

fn fallback(s: &str) -> Result<Binding, String> {
    let p: Vec<&str> = s.split(':').collect();
    if !(3..=4).contains(&p.len()) {
        return Err("expected RUNTIME:FAMILY:TIER or RUNTIME:FAMILY:TIER:EFFORT".into());
    }
    Ok(Binding {
        runtime: p[0].parse().map_err(|e| format!("{e}"))?,
        family: Some(p[1].parse().map_err(|e| format!("{e}"))?),
        tier: Some(p[2].parse().map_err(|e| format!("{e}"))?),
        model: None,
        effort: p.get(3).map(|e| e.to_string()),
    })
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    let path = ctx.config_path()?;
    let mut cfg = ctx.load_config()?;
    match args.cmd {
        Cmd::Add(a) => {
            if cfg.seat(a.name.as_str()).is_some() {
                bail!("seat {} exists; remove it first", a.name);
            }
            cfg.seats.push(SeatConfig {
                name: a.name.clone(),
                role: a.role,
                binding: Binding {
                    runtime: a.runtime,
                    family: a.family,
                    tier: a.tier,
                    model: a.model,
                    effort: a.effort,
                },
                fallback: a.fallback,
                memcap_gb: None,
            });
            cfg.save(&path)
                .with_context(|| format!("seat {}", a.name))?;
            let s = cfg.seat(a.name.as_str()).expect("added");
            println!(
                "added {}; restart the bridge service to serve it",
                describe(&cfg, s)
            );
        }
        Cmd::List => {
            if ctx.json {
                let v: Vec<serde_json::Value> = cfg.seats.iter().map(|s| json(&cfg, s)).collect();
                println!("{}", serde_json::Value::Array(v));
            } else {
                for s in &cfg.seats {
                    println!("{}", describe(&cfg, s));
                }
                if cfg.seats.is_empty() {
                    eprintln!("no seats; add one with `hgl-ai seat add`");
                }
            }
        }
        Cmd::Remove { name } => {
            let before = cfg.seats.len();
            cfg.seats.retain(|s| s.name != name);
            if cfg.seats.len() == before {
                bail!("no seat {name}");
            }
            cfg.save(&path)?;
            println!("removed seat {name}; restart the bridge service to stop serving it");
        }
    }
    Ok(EXIT_OK)
}

fn binding(cfg: &Config, b: &Binding) -> String {
    match cfg.resolve(b) {
        Ok(r) => {
            let effort = r.effort.map(|e| format!(" effort {e}")).unwrap_or_default();
            format!("{} {}/{}{effort}", r.runtime, r.family, r.model)
        }
        Err(e) => format!("{} ({e})", b.runtime),
    }
}

/// `w1 implementor codex gpt/gpt-6.1-sol effort high; fallback agy claude/claude-opus-5-5`
fn describe(cfg: &Config, s: &SeatConfig) -> String {
    let mut line = format!("{} {} {}", s.name, s.role, binding(cfg, &s.binding));
    if !s.fallback.is_empty() {
        let f: Vec<String> = s.fallback.iter().map(|b| binding(cfg, b)).collect();
        line.push_str("; fallback ");
        line.push_str(&f.join(", "));
    }
    line
}

fn json(cfg: &Config, s: &SeatConfig) -> serde_json::Value {
    let one = |b: &Binding| match cfg.resolve(b) {
        Ok(r) => {
            serde_json::json!({ "runtime": r.runtime, "family": r.family, "model": r.model, "effort": r.effort })
        }
        Err(e) => serde_json::json!({ "runtime": b.runtime, "error": e }),
    };
    let mut v = one(&s.binding);
    v["seat"] = s.name.as_str().into();
    v["role"] = s.role.as_str().into();
    v["fallback"] = s.fallback.iter().map(one).collect::<Vec<_>>().into();
    v["memcap_gb"] = s.memcap_gb.unwrap_or(cfg.limits.memcap_gb).into();
    v
}
