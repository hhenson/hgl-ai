//! `hgl-ai bridge run [--seat NAME ...]`: the bridge daemon (docs/design.md §2, §4).

use std::sync::Arc;
use std::time::Duration;

use hgl_ai_bridge::{Bridge, Deps, IssueTitle, Options, Statvfs};
use hgl_ai_bus::Bus;
use hgl_ai_config::Config;
use hgl_ai_gh::exec::{self, Gh, GhCli};
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
        title: Arc::new(GhTitle::new(&cfg)),
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

/// How long a new task tree waits for its issue's title before the note's line names it.
const TITLE_TIMEOUT: Duration = Duration::from_secs(30);

/// An issue's title through `gh issue view` on [`GhCli`]: bounded by [`TITLE_TIMEOUT`],
/// no prompt, and `[github] token_file` as every other `gh` call. Called on a blocking
/// thread of the bridge's runtime.
pub struct GhTitle {
    gh: std::result::Result<GhCli, String>,
    owner: String,
    rt: tokio::runtime::Handle,
}

impl GhTitle {
    /// On the current runtime. A token file that cannot be read fails each lookup, with
    /// its reason, rather than the bridge.
    pub fn new(cfg: &Config) -> Self {
        let cli = GhCli {
            timeout: TITLE_TIMEOUT,
            ..GhCli::default()
        };
        let gh = match &cfg.github.token_file {
            Some(p) => hgl_ai_config::expand(p)
                .map_err(|e| e.to_string())
                .and_then(|p| cli.with_token_file(&p).map_err(|e| e.to_string())),
            None => Ok(cli),
        };
        GhTitle {
            gh,
            owner: cfg.github.owner.clone(),
            rt: tokio::runtime::Handle::current(),
        }
    }
}

impl IssueTitle for GhTitle {
    fn issue_title(&self, repo: &str, issue: u64) -> std::result::Result<String, String> {
        let gh = self.gh.as_ref().map_err(Clone::clone)?;
        self.rt.block_on(issue_title(gh, &self.owner, repo, issue))
    }
}

/// `gh issue view ISSUE --repo OWNER/REPO --json title`: the title, else why not.
pub async fn issue_title<G: Gh>(
    gh: &G,
    owner: &str,
    repo: &str,
    issue: u64,
) -> std::result::Result<String, String> {
    let n = issue.to_string();
    let slug = format!("{owner}/{repo}");
    let args = ["issue", "view", &n, "--repo", &slug, "--json", "title"];
    let out = exec::text(gh, args.map(String::from).to_vec())
        .await
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("`gh issue view {n}`: {e}"))?;
    v.get("title")
        .and_then(|t| t.as_str())
        .map(String::from)
        .ok_or_else(|| format!("`gh issue view {n}`: no title in {}", out.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `gh` in `dir` that records its arguments and environment, then runs `body`.
    fn fake_gh(dir: &std::path::Path, body: &str) -> GhCli {
        use std::os::unix::fs::PermissionsExt as _;
        let p = dir.join("gh");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{d}/args'\n\
             printf 'prompt=%s token=%s\\n' \"$GH_PROMPT_DISABLED\" \"$GH_TOKEN\" > '{d}/env'\n{body}\n",
            d = dir.display()
        );
        // Written by another process: a fork of a parallel test can hold a write descriptor
        // of a file this process wrote, and exec'ing it then fails "Text file busy".
        let draft = dir.join("gh.draft");
        std::fs::write(&draft, script).unwrap();
        std::fs::set_permissions(&draft, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_file(&p);
        let cp = std::process::Command::new("cp")
            .arg("-p")
            .arg(&draft)
            .arg(&p)
            .status()
            .unwrap();
        assert!(cp.success());
        GhCli {
            program: p.display().to_string(),
            timeout: Duration::from_secs(10),
            token: Some("t0k".into()),
        }
    }

    #[tokio::test]
    async fn the_title_from_gh_issue_view() {
        let d = tempfile::tempdir().unwrap();
        let gh = fake_gh(d.path(), r#"echo '{"title":"[T; N] array literals"}'"#);
        assert_eq!(
            issue_title(&gh, "o", "hgl-ai", 27).await.unwrap(),
            "[T; N] array literals"
        );
        let args = std::fs::read_to_string(d.path().join("args")).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                "issue", "view", "27", "--repo", "o/hgl-ai", "--json", "title"
            ]
        );
        let env = std::fs::read_to_string(d.path().join("env")).unwrap();
        assert_eq!(env.trim(), "prompt=1 token=t0k");
    }

    #[tokio::test]
    async fn a_failed_gh_gives_its_stderr() {
        let d = tempfile::tempdir().unwrap();
        let gh = fake_gh(
            d.path(),
            "echo 'GraphQL: Could not resolve to an Issue with the number of 27.' >&2; exit 1",
        );
        let why = issue_title(&gh, "o", "hgl-ai", 27).await.unwrap_err();
        assert!(why.contains("Could not resolve to an Issue"), "{why}");
    }

    #[tokio::test]
    async fn bad_json_and_a_missing_title_are_reasons() {
        let d = tempfile::tempdir().unwrap();
        let gh = fake_gh(d.path(), "echo 'not json'");
        let why = issue_title(&gh, "o", "r", 1).await.unwrap_err();
        assert!(why.contains("gh issue view 1"), "{why}");
        let gh = fake_gh(d.path(), r#"echo '{"title":7}'"#);
        let why = issue_title(&gh, "o", "r", 1).await.unwrap_err();
        assert!(why.contains("no title"), "{why}");
    }

    #[tokio::test]
    async fn a_gh_that_hangs_times_out() {
        let d = tempfile::tempdir().unwrap();
        let mut gh = fake_gh(d.path(), "exec sleep 30");
        gh.timeout = Duration::from_millis(200);
        let start = std::time::Instant::now();
        let why = issue_title(&gh, "o", "r", 1).await.unwrap_err();
        assert!(why.contains("no answer"), "{why}");
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    /// The bridge calls it on a blocking thread of its runtime; an unreadable token file
    /// fails the lookup, not the bridge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_blocking_lookup_and_a_bad_token_file() {
        let d = tempfile::tempdir().unwrap();
        let t = GhTitle {
            gh: Ok(fake_gh(d.path(), r#"echo '{"title":"Hello"}'"#)),
            owner: "o".into(),
            rt: tokio::runtime::Handle::current(),
        };
        let got = tokio::task::spawn_blocking(move || t.issue_title("r", 3))
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Ok("Hello"));

        let mut cfg = Config::new("h");
        cfg.github.token_file = Some(d.path().join("missing.token").display().to_string());
        let t = GhTitle::new(&cfg);
        let got = tokio::task::spawn_blocking(move || t.issue_title("r", 3))
            .await
            .unwrap();
        assert!(got.unwrap_err().contains("missing.token"));
    }
}
