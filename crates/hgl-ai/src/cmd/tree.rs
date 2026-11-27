//! `hgl-ai tree list [--stale] [--local]` and `hgl-ai tree remove SEAT REPO ISSUE
//! [--review]` (docs/protocol.md §5).

use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::bail;
use hgl_ai_bridge::lock::{FileLock, TURN_LOCK};
use hgl_ai_bus::Bus;
use hgl_ai_config::Config;
use hgl_ai_proto::{Seat, TaskKey, TreeRecord};
use hgl_ai_tree::stale::{self, human_age, human_bytes};
use hgl_ai_tree::{Git, IsClosed, Trees};

use super::{Ctx, EXIT_FAIL, EXIT_OK};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// List task trees: every host's from `HGL_TREES`, or this host's with --local.
    List(ListArgs),
    /// Remove a tree as the bridge does on `landed`: salvage uncommitted or unpushed
    /// work to `<work_root>/salvage/`, remove the worktree, its branch and directory,
    /// and deregister it. Refused while a turn runs in it.
    Remove(RemoveArgs),
}

#[derive(clap::Args)]
struct ListArgs {
    /// Only trees older than `stale_after` or whose issue (pull request, for a review
    /// tree) is closed; closed-ness is asked of `gh`.
    #[arg(long)]
    stale: bool,
    /// Scan this host's `work_root` instead of reading `HGL_TREES`.
    #[arg(long)]
    local: bool,
}

#[derive(clap::Args)]
struct RemoveArgs {
    seat: Seat,
    repo: String,
    issue: u64,
    /// The reviewer's tree `<repo>-<pr>-review`.
    #[arg(long)]
    review: bool,
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    match args.cmd {
        Cmd::List(a) => list(ctx, a).await,
        Cmd::Remove(a) => remove(ctx, a).await,
    }
}

async fn list(ctx: &Ctx, a: ListArgs) -> anyhow::Result<i32> {
    let cfg = ctx.load_config()?;
    let records: Vec<TreeRecord> = if a.local {
        let trees = Trees::new(&cfg, Git::default())?;
        tokio::task::spawn_blocking(move || trees.list())
            .await??
            .into_iter()
            .map(|t| t.record)
            .collect()
    } else {
        Bus::connect(&cfg).await?.trees().await?
    };
    let now = chrono::Utc::now();
    let rows: Vec<(TreeRecord, Option<String>)> = if a.stale {
        let closed = GhClosed::new(&cfg);
        let after = cfg.stale_after();
        let s = tokio::task::spawn_blocking(move || stale::stale(&records, now, after, &closed))
            .await?;
        s.into_iter()
            .map(|s| (s.reason(), s.record))
            .map(|(why, r)| (r, Some(why)))
            .collect()
    } else {
        records.into_iter().map(|r| (r, None)).collect()
    };
    if ctx.json {
        let v: Vec<serde_json::Value> = rows
            .iter()
            .map(|(r, why)| {
                let mut j = serde_json::to_value(r).unwrap_or_default();
                j["age_secs"] = stale::age(r, now).map(|d| d.as_secs()).into();
                if let Some(w) = why {
                    j["stale"] = w.clone().into();
                }
                j
            })
            .collect();
        println!("{}", serde_json::Value::Array(v));
        return Ok(EXIT_OK);
    }
    for (r, why) in &rows {
        let age = stale::age(r, now)
            .map(human_age)
            .unwrap_or_else(|| "?".into());
        let mut line = format!(
            "{} {age} {} {}",
            r.key(),
            human_bytes(r.size_bytes),
            r.branch.as_deref().unwrap_or("-")
        );
        if let Some(c) = r.class {
            line.push_str(&format!(" class={c}"));
        }
        if let Some(w) = why {
            line.push(' ');
            line.push_str(w);
        }
        println!("{line} {}", r.path);
    }
    if rows.is_empty() {
        eprintln!("no trees");
    }
    Ok(EXIT_OK)
}

async fn remove(ctx: &Ctx, a: RemoveArgs) -> anyhow::Result<i32> {
    let cfg = ctx.load_config()?;
    let trees = Arc::new(Trees::new(&cfg, Git::default())?);
    let task = TaskKey {
        repo: a.repo.clone(),
        issue: a.issue,
    };
    let dir = trees.layout().task_dir(&a.seat, &task, a.review)?;
    let lock = dir.join(TURN_LOCK);
    let _held = if lock.exists() {
        match FileLock::turn_patiently(&lock).await? {
            Some(l) => Some(l),
            None => bail!(
                "a turn is running in {} (its turn lock is held); stop the bridge or wait",
                dir.display()
            ),
        }
    } else {
        None
    };
    let (t2, seat, review) = (trees.clone(), a.seat.clone(), a.review);
    let removed = tokio::task::spawn_blocking(move || t2.remove(&seat, &task, review)).await??;
    let Some(rm) = removed else {
        eprintln!("no tree at {}", dir.display());
        return Ok(EXIT_FAIL);
    };
    let deregistered = match Bus::connect(&cfg).await {
        Ok(bus) => bus.tree_del(&rm.key).await.map_err(anyhow::Error::from),
        Err(e) => Err(e.into()),
    };
    if let Err(e) = &deregistered {
        eprintln!(
            "hgl-ai: removed, but {} is still registered in HGL_TREES: {e:#}",
            rm.key
        );
    }
    if ctx.json {
        let j = serde_json::json!({
            "key": rm.key,
            "dir": rm.dir,
            "branch": rm.branch,
            "freed_bytes": rm.freed_bytes,
            "salvage": rm.salvage,
            "deregistered": deregistered.is_ok(),
        });
        println!("{j}");
    } else {
        let salvage = rm
            .salvage
            .as_ref()
            .map_or("none".to_string(), |p| p.display().to_string());
        println!(
            "removed {} (freed {}); salvage: {salvage}",
            rm.dir.display(),
            human_bytes(rm.freed_bytes)
        );
    }
    Ok(EXIT_OK)
}

/// Closed-ness from GitHub through `gh`: `gh issue view` for an implementor's issue,
/// `gh pr view` for a reviewed pull request. `None` when `gh` cannot tell.
pub struct GhClosed {
    owner: String,
}

impl GhClosed {
    pub fn new(cfg: &Config) -> Self {
        GhClosed {
            owner: cfg.github.owner.clone(),
        }
    }
}

impl IsClosed for GhClosed {
    fn is_closed(&self, repo: &str, number: u64, pull_request: bool) -> Option<bool> {
        let what = if pull_request { "pr" } else { "issue" };
        let out = Command::new("gh")
            .args([
                what,
                "view",
                &number.to_string(),
                "--repo",
                &format!("{}/{repo}", self.owner),
                "--json",
                "state",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        Some(v.get("state")?.as_str()? != "OPEN")
    }
}
