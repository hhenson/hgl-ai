//! `hgl-ai build`: recipes, runs here or on a builder of another platform, their status,
//! summaries, logs, and a debugging shell (docs/build.md).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use hgl_ai_build::driver::{self, Meta, Options};
use hgl_ai_build::exec::local::Local;
use hgl_ai_build::exec::{Cancel, Checkout, Executor, Prep, Workspace, ssh};
use hgl_ai_build::recipe::{Recipe, sdk_var};
use hgl_ai_build::{store, summary};
use hgl_ai_bus::Bus;
use hgl_ai_config::Config;
use hgl_ai_proto::Seat;
use hgl_ai_proto::build::{
    BUILD_VERSION, BuildJob, BuildStatus, Platform, StepStatus, Summary, check_build_id,
    new_build_id, reply_seat,
};
use hgl_ai_tree::Git;

use super::note::{duration, read_bounded};
use super::{Ctx, EXIT_FAIL, EXIT_OK, NotReady};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Run the recipe's steps here (or, with --platform, on a builder of that platform)
    /// and print the bounded summary. Exit 0 pass, 1 fail, 3 not run.
    Run(RunArgs),
    /// Where a build is: its steps so far, or its summary.
    Status {
        id: String,
        #[arg(long, value_name = "DIR")]
        log_dir: Option<PathBuf>,
    },
    /// Wait for a build to end and print its summary. Exit as `build run`.
    Wait {
        id: String,
        #[arg(long, value_name = "DUR", value_parser = duration)]
        timeout: Option<Duration>,
        #[arg(long, value_name = "DIR")]
        log_dir: Option<PathBuf>,
    },
    /// Recent runs kept on the hub, newest first.
    List {
        #[arg(long)]
        repo: Option<String>,
        /// Only failed and not-run builds.
        #[arg(long)]
        failed: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Print part of a local build's step log (default: the first step that did not pass),
    /// or of a report the summary names (`artifacts/<step>/<file>`).
    Log {
        id: String,
        /// A step, or a report's path in the run's directory as the summary gives it.
        step: Option<String>,
        /// A byte range of the log, START-END, as the summary gives it.
        #[arg(long, value_name = "START-END")]
        bytes: Option<String>,
        #[arg(long, value_name = "DIR")]
        log_dir: Option<PathBuf>,
    },
    /// An interactive shell in a fresh checkout on this host's builder of PLATFORM (a
    /// remote over ssh, for a proxy); the checkout is removed when the shell ends.
    Shell(ShellArgs),
    /// Show, edit, import or export a repository's recipe on the hub.
    Recipe(RecipeArgs),
    /// The local build verdicts a pull request needs for its head on the present main,
    /// asking builders for those not judged (BUILD-15), as `gh land` does. Exit 0 all
    /// pass, 1 one failed or did not run, 3 still building or missing.
    Gate {
        repo: String,
        pr: u64,
        /// Wait until every verdict is judged or did not run.
        #[arg(long)]
        wait: bool,
        /// With --wait: give up after this long (the builds go on).
        #[arg(long, value_name = "DUR", value_parser = duration, default_value = "2h")]
        timeout: Duration,
    },
}

#[derive(clap::Args)]
struct RunArgs {
    /// The repository (default: the task's, or the checkout's origin).
    repo: Option<String>,
    /// Only these steps, in recipe order.
    #[arg(long, value_delimiter = ',', value_name = "STEP,...")]
    steps: Option<Vec<String>>,
    /// Build on a builder of this platform through the hub (linux, macos, windows).
    #[arg(long)]
    platform: Option<Platform>,
    /// With --platform: this builder (its label).
    #[arg(long, value_name = "LABEL", requires = "platform")]
    builder: Option<String>,
    /// The checkout (default: $HGL_AI_TASK_DIR/src, else the git top level here).
    #[arg(long, value_name = "DIR")]
    dir: Option<PathBuf>,
    /// Where build logs go (default: $HGL_AI_TASK_DIR/build-logs).
    #[arg(long, value_name = "DIR")]
    log_dir: Option<PathBuf>,
    /// The build directory steps see as $HGL_AI_BUILD_DIR (default: $HGL_AI_TASK_DIR/build).
    #[arg(long, value_name = "DIR")]
    build_dir: Option<PathBuf>,
    /// A recipe file in place of the hub's (a local run).
    #[arg(long, value_name = "FILE", conflicts_with = "platform")]
    recipe: Option<PathBuf>,
    /// Run every step, not stopping at the first failure.
    #[arg(long)]
    keep_going: bool,
    /// Print the build id and return; `build status|wait ID` follow it.
    #[arg(long)]
    detach: bool,
    /// Also send the receipt to this seat when the build ends.
    #[arg(long, value_name = "SEAT")]
    notify: Option<Seat>,
    /// With --platform: the commit to build (default: HEAD of the checkout).
    #[arg(long, value_name = "SHA")]
    head: Option<String>,
    /// With --platform: merge the head onto this branch or commit first.
    #[arg(long, value_name = "REF", requires = "platform")]
    base: Option<String>,
    /// With --platform: the pull request, to fetch refs/pull/N/head.
    #[arg(long, value_name = "N", default_value_t = 0)]
    issue: u64,
    /// Give up waiting after this long (the build goes on).
    #[arg(long, value_name = "DUR", value_parser = duration)]
    timeout: Option<Duration>,
    /// The id of a detached run (set by --detach).
    #[arg(long, hide = true)]
    id: Option<String>,
}

#[derive(clap::Args)]
struct ShellArgs {
    #[arg(long)]
    platform: Platform,
    repo: Option<String>,
    /// The commit (default: HEAD of the checkout here).
    #[arg(long, value_name = "SHA")]
    head: Option<String>,
    #[arg(long, value_name = "REF")]
    base: Option<String>,
    #[arg(long, value_name = "N", default_value_t = 0)]
    issue: u64,
    #[arg(long, value_name = "DIR")]
    dir: Option<PathBuf>,
}

#[derive(clap::Args)]
struct RecipeArgs {
    #[command(subcommand)]
    cmd: RecipeCmd,
}

#[derive(clap::Subcommand)]
enum RecipeCmd {
    /// Print the recipe (the latest, or revision N).
    Show {
        repo: Option<String>,
        #[arg(long, value_name = "N")]
        rev: Option<u64>,
    },
    /// Edit the recipe in $EDITOR; it is stored when it validates.
    Edit { repo: Option<String> },
    /// Store FILE as the recipe of the repository it names.
    Import {
        file: PathBuf,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Write the recipe to standard output or FILE.
    Export {
        repo: Option<String>,
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// The revisions kept.
    History { repo: Option<String> },
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    match args.cmd {
        Cmd::Run(a) => {
            if a.platform.is_some() {
                run_remote(ctx, a).await
            } else {
                run_local(ctx, a).await
            }
        }
        Cmd::Status { id, log_dir } => status(ctx, &id, log_dir).await,
        Cmd::Wait {
            id,
            timeout,
            log_dir,
        } => wait(ctx, &id, timeout, log_dir).await,
        Cmd::List {
            repo,
            failed,
            limit,
        } => list(ctx, repo, failed, limit).await,
        Cmd::Log {
            id,
            step,
            bytes,
            log_dir,
        } => log(ctx, &id, step, bytes, log_dir),
        Cmd::Shell(a) => shell(ctx, a).await,
        Cmd::Recipe(a) => recipe(ctx, a).await,
        Cmd::Gate {
            repo,
            pr,
            wait,
            timeout,
        } => gate(ctx, &repo, pr, wait, timeout).await,
    }
}

fn task_dir() -> Option<PathBuf> {
    std::env::var_os("HGL_AI_TASK_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let o = Git::default().output(dir, args).ok()?;
    o.ok.then(|| o.stdout.trim().to_string())
}

/// The checkout: --dir, the task's `src/`, the git top level here, or here.
fn source_dir(arg: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let d = match arg {
        Some(d) => d,
        None => match task_dir().map(|t| t.join("src")).filter(|s| s.is_dir()) {
            Some(s) => s,
            None => {
                let here = std::env::current_dir()?;
                git_out(&here, &["rev-parse", "--show-toplevel"])
                    .map(PathBuf::from)
                    .unwrap_or(here)
            }
        },
    };
    std::path::absolute(&d).with_context(|| format!("{}", d.display()))
}

/// The repository: given, the task's, or the origin URL's last part.
fn repo_name(arg: Option<String>, src: &Path) -> anyhow::Result<String> {
    if let Some(r) = arg {
        return Ok(r);
    }
    if let Some(t) = task_dir()
        && let Ok(f) = hgl_ai_tree::read_task(&t)
    {
        return Ok(f.repo);
    }
    if let Some(url) = git_out(src, &["remote", "get-url", "origin"]) {
        let last = url
            .trim_end_matches('/')
            .rsplit(['/', ':'])
            .next()
            .unwrap_or_default()
            .trim_end_matches(".git")
            .to_string();
        if !last.is_empty() {
            return Ok(last);
        }
    }
    bail!("name the repository: hgl-ai build run REPO")
}

fn host_label(cfg: Option<&Config>) -> String {
    cfg.map(|c| c.host.clone())
        .unwrap_or_else(|| "local".into())
}

fn data_dir(cfg: Option<&Config>) -> PathBuf {
    match cfg {
        Some(c) => c.data_dir().unwrap_or_else(|_| PathBuf::from(".")),
        None => Config::new("local")
            .data_dir()
            .unwrap_or_else(|_| PathBuf::from(".")),
    }
}

fn log_root(arg: Option<PathBuf>, cfg: Option<&Config>) -> PathBuf {
    let r = arg
        .or_else(|| task_dir().map(|t| t.join("build-logs")))
        .unwrap_or_else(|| data_dir(cfg).join("build-logs"));
    std::path::absolute(&r).unwrap_or(r)
}

fn requester(cfg: Option<&Config>) -> String {
    std::env::var("HGL_AI_SEAT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| host_label(cfg))
}

fn sender(cfg: &Config) -> anyhow::Result<Seat> {
    let s = std::env::var("HGL_AI_SEAT").unwrap_or_default();
    Ok(Seat::new(if s.is_empty() { cfg.host.clone() } else { s })?)
}

async fn connect(ctx: &Ctx) -> anyhow::Result<(Config, Bus)> {
    let cfg = ctx.load_config()?;
    let bus = Bus::connect(&cfg).await?;
    Ok((cfg, bus))
}

fn print_summary(ctx: &Ctx, s: &Summary) {
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(s).unwrap_or_default());
    } else {
        print!("{}", summary::render(s));
    }
}

/// Stop `cancel` at SIGINT or SIGTERM.
fn cancel_on_signal(cancel: Cancel) -> Arc<AtomicBool> {
    let term_seen = Arc::new(AtomicBool::new(false));
    let seen = term_seen.clone();
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut term) = signal(SignalKind::terminate()) else {
        return term_seen;
    };
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => { seen.store(true, Ordering::SeqCst); }
        }
        cancel.cancel();
    });
    term_seen
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// `<run_dir>/pid`, written whole (a reader never sees it half written).
fn write_pid(run_dir: &Path, pid: u32) -> std::io::Result<()> {
    let tmp = run_dir.join(format!("pid.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{pid}\n"))?;
    std::fs::rename(&tmp, run_dir.join("pid"))
}

/// Start this same command again in the background, detached, with `--id`.
fn detach(id: &str, run_dir: &Path, log_root: &Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    std::fs::create_dir_all(run_dir)?;
    let mut args: Vec<std::ffi::OsString> = std::env::args_os()
        .skip(1)
        .filter(|a| a != "--detach")
        .collect();
    args.push("--id".into());
    args.push(id.into());
    if !args
        .iter()
        .any(|a| a == "--log-dir" || a.to_string_lossy().starts_with("--log-dir="))
    {
        args.push("--log-dir".into());
        args.push(log_root.into());
    }
    let out = std::fs::File::create(run_dir.join("run.out"))?;
    let err = out.try_clone()?;
    let mut c = std::process::Command::new(std::env::current_exe()?);
    c.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);
    // SAFETY: setsid is async-signal-safe; the child leaves this session.
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = c.spawn().context("starting the detached build")?;
    write_pid(run_dir, child.id())?;
    Ok(())
}

async fn run_local(ctx: &Ctx, a: RunArgs) -> anyhow::Result<i32> {
    // The bridge ends a turn with every process of its slice and every process that
    // carries its marker: a build detached here would end with the turn.
    if a.detach && a.id.is_none() && std::env::var_os("HGL_AI_TURN").is_some_and(|v| !v.is_empty())
    {
        let p = Platform::current().map_or("linux", Platform::as_str);
        return Err(NotReady(format!(
            "a build detached inside an agent turn ends with the turn; hand it to this \
             host's builder, which outlives the turn: `hgl-ai build run --platform {p} \
             --detach` (then `build wait ID`), or run it without --detach"
        ))
        .into());
    }
    let cfg = ctx.load_config().ok();
    let src = source_dir(a.dir.clone())?;
    let repo = repo_name(a.repo.clone(), &src)?;
    let id = match &a.id {
        Some(i) => {
            check_build_id(i)?;
            i.clone()
        }
        None => new_build_id(),
    };
    let root = log_root(a.log_dir.clone(), cfg.as_ref());
    let run_dir = root.join(&id);
    // The recipe: a file, or the hub's.
    let mut bus = None;
    if let Some(c) = &cfg {
        match Bus::connect(c).await {
            Ok(b) => bus = Some(b),
            Err(e) if a.recipe.is_some() => {
                eprintln!("hgl-ai: {e}; the summary is not kept on the hub")
            }
            Err(e) => return Err(e.into()),
        }
    }
    let (recipe, rev) = match &a.recipe {
        Some(f) => {
            let text = std::fs::read_to_string(f).with_context(|| format!("{}", f.display()))?;
            (Recipe::parse(&text)?, None)
        }
        None => {
            let Some(b) = &bus else {
                bail!("no configuration to reach the hub; give --recipe FILE");
            };
            match store::recipe_get(b, &repo, None).await? {
                Some(s) => (s.recipe, Some(s.revision)),
                None => {
                    return Err(NotReady(format!(
                        "no recipe for {repo} on the hub; `hgl-ai build recipe import FILE`"
                    ))
                    .into());
                }
            }
        }
    };
    if a.detach {
        detach(&id, &run_dir, &root)?;
        if ctx.json {
            println!("{}", serde_json::json!({ "id": id, "log_dir": run_dir }));
        } else {
            println!("{id}");
            eprintln!("detached; `hgl-ai build wait {id}` prints the summary");
        }
        return Ok(EXIT_OK);
    }
    std::fs::create_dir_all(&run_dir)?;
    write_pid(&run_dir, std::process::id())?;
    let head = git_out(&src, &["rev-parse", "HEAD"]);
    let dirty = git_out(&src, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    let build = a
        .build_dir
        .clone()
        .or_else(|| task_dir().map(|t| t.join("build")))
        .unwrap_or_else(|| data_dir(cfg.as_ref()).join("build-cache").join(&repo));
    std::fs::create_dir_all(&build)?;
    let build = std::path::absolute(&build).unwrap_or(build);
    let ws = Workspace {
        src: src.display().to_string(),
        build: build.display().to_string(),
        tmp: std::env::temp_dir().display().to_string(),
        job: None,
        clone: None,
        base_sha: None,
    };
    let work_root = cfg
        .as_ref()
        .and_then(|c| c.work_root().ok())
        .unwrap_or_else(|| data_dir(None).join("work"));
    let exec = Local::new(
        work_root.join("builder"),
        cfg.as_ref()
            .map(|c| c.builder.sdk.clone())
            .unwrap_or_default(),
        Git::default(),
    );
    let meta = Meta {
        id: id.clone(),
        repo: repo.clone(),
        head,
        base: None,
        base_sha: None,
        dirty,
        host: host_label(cfg.as_ref()),
        proxy: None,
        requester: Some(requester(cfg.as_ref())),
        recipe_rev: rev,
    };
    let opts = Options {
        only: a.steps.clone(),
        keep_going: a.keep_going,
        env: driver::build_env(&id, &ws),
        log_dir: run_dir.clone(),
    };
    let cancel = Cancel::default();
    let term_seen = cancel_on_signal(cancel.clone());
    let bus = bus.map(Arc::new);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Summary>();
    let fwd = {
        let (bus, dir) = (bus.clone(), run_dir.clone());
        tokio::spawn(async move {
            let mut warned = false;
            while let Some(s) = rx.recv().await {
                let _ = store::write_status(&dir, &s);
                if let Some(b) = &bus
                    && let Err(e) = store::run_put(b, &s).await
                    && !warned
                {
                    eprintln!("hgl-ai: keeping the run on the hub: {e}");
                    warned = true;
                }
            }
        })
    };
    let c2 = cancel.clone();
    let driven = tokio::task::spawn_blocking(move || {
        driver::drive(&exec, &ws, &recipe, &meta, &opts, &c2, &mut |s| {
            let _ = tx.send(s.clone());
        })
    })
    .await
    .context("the build stopped")?;
    let _ = fwd.await;
    let mut s = driven.summary;
    // A turn's reaper signals both the build and its step. The step can exit before
    // the signal task gets scheduled, so give an incoming TERM a chance to arrive.
    if s.steps.iter().any(|step| step.exit == Some(143)) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if term_seen.load(Ordering::SeqCst)
        && std::env::var_os("HGL_AI_TURN").is_some_and(|v| !v.is_empty())
    {
        for step in &mut s.steps {
            if step.status == StepStatus::Fail && step.exit == Some(143) && step.limit.is_none() {
                step.status = StepStatus::Stopped;
                step.reason = Some("turn ended".into());
            }
        }
        s.status = BuildStatus::Stopped;
        s.reason = Some("turn ended".into());
    }
    store::write_final(&run_dir, &s)?;
    if let Some(b) = &bus {
        if let Err(e) = store::run_put(b, &s).await {
            eprintln!("hgl-ai: keeping the run on the hub: {e}");
        }
        if let (Some(to), Some(c)) = (&a.notify, &cfg) {
            let from = hgl_ai_proto::build::builder_inbox(&c.host)
                .or_else(|_| Seat::new(c.host.clone()))?;
            if let Err(e) = store::send_receipt(b, &from, to, &s, a.issue).await {
                eprintln!("hgl-ai: {e}");
            }
        }
    }
    print_summary(ctx, &s);
    Ok(s.status.exit_code())
}

pub use hgl_ai_build::request::choose;

async fn run_remote(ctx: &Ctx, a: RunArgs) -> anyhow::Result<i32> {
    let platform = a.platform.expect("checked");
    let (cfg, bus) = connect(ctx).await?;
    let src = source_dir(a.dir.clone())?;
    let repo = repo_name(a.repo.clone(), &src)?;
    let head = match &a.head {
        Some(h) => h.clone(),
        None => git_out(&src, &["rev-parse", "HEAD"])
            .ok_or_else(|| anyhow::anyhow!("no commit in {}; give --head SHA", src.display()))?,
    };
    hgl_ai_proto::check_head(&head)?;
    if a.head.is_none() && git_out(&src, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty())
    {
        eprintln!(
            "hgl-ai: uncommitted changes are not part of a build on another host; building {}",
            &head[..12]
        );
    }
    if store::recipe_get(&bus, &repo, None).await?.is_none() {
        return Err(NotReady(format!(
            "no recipe for {repo} on the hub; `hgl-ai build recipe import FILE`"
        ))
        .into());
    }
    let builders = bus.builders().await?;
    let Some(b) = choose(&builders, platform, a.builder.as_deref()) else {
        return Err(NotReady(match &a.builder {
            Some(l) => format!("no {platform} builder {l} is registered"),
            None => format!("no {platform} builder is registered on the hub"),
        })
        .into());
    };
    if !b.reachable {
        eprintln!(
            "hgl-ai: builder {} is unreachable ({}); the job waits for it",
            b.label,
            b.reason.as_deref().unwrap_or("no reason given")
        );
    }
    // The task's pull request, so a `--notify` receipt resumes the seat's task.
    let issue = match (
        a.issue,
        task_dir().and_then(|t| hgl_ai_tree::read_task(&t).ok()),
    ) {
        (0, Some(t)) if t.repo == repo => t.issue,
        (n, _) => n,
    };
    let id = new_build_id();
    let reply = reply_seat(&id)?;
    let job = BuildJob {
        v: BUILD_VERSION,
        id: id.clone(),
        repo: repo.clone(),
        head: head.clone(),
        base: a.base.clone(),
        issue,
        steps: a.steps.clone(),
        recipe_rev: None,
        keep_going: a.keep_going,
        verdict: None,
        reply_to: reply.clone(),
        notify: a.notify.clone(),
        requester: requester(Some(&cfg)),
    };
    hgl_ai_build::request::send(&bus, sender(&cfg)?, &b, &job).await?;
    if a.detach {
        if ctx.json {
            println!("{}", serde_json::json!({ "id": id, "builder": b.label }));
        } else {
            println!("{id}");
            eprintln!(
                "sent to {}; `hgl-ai build wait {id}` prints the summary",
                b.label
            );
        }
        return Ok(EXIT_OK);
    }
    match wait_remote(&bus, &id, Some(&b.label), a.timeout).await? {
        Some(s) => {
            print_summary(ctx, &s);
            Ok(s.status.exit_code())
        }
        None => {
            eprintln!("hgl-ai: build {id} has not ended; `hgl-ai build wait {id}`");
            Ok(EXIT_FAIL)
        }
    }
}

/// Wait for the receipt of `id`, or the hub's final summary of it. With `builder`, give up
/// when its record is gone for three checks in a row (the job stays in its inbox).
async fn wait_remote(
    bus: &Bus,
    id: &str,
    builder: Option<&str>,
    timeout: Option<Duration>,
) -> anyhow::Result<Option<Summary>> {
    let reply = reply_seat(id)?;
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut gone = 0;
    let mut seen = 0;
    loop {
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if left.is_some_and(|l| l.is_zero()) {
            return Ok(None);
        }
        let step = left.unwrap_or(Duration::MAX).min(Duration::from_secs(30));
        // Only the builder's receipt of this build counts; another note on the address
        // (a stray or a forged one) is passed over once, the hub's copy standing in.
        if let Some((seq, n)) = bus.reply_wait_after(&reply, seen, step).await? {
            seen = seq;
            match store::parse_receipt(&n, id) {
                Ok(s) => return Ok(Some(s)),
                Err(e) => tracing::warn!("build {id}: {e}; reading the hub's copy"),
            }
        }
        // The hub's final summary stands in for a receipt not (yet) published. A stopped
        // run is queued again, so it is not final here.
        if let Some(s) = store::run_get(bus, id).await?
            && matches!(
                s.status,
                BuildStatus::Pass | BuildStatus::Fail | BuildStatus::NotRun
            )
        {
            return Ok(Some(s));
        }
        if let Some(l) = builder {
            let live = bus.builders().await?.iter().any(|b| b.label == l);
            gone = if live { 0 } else { gone + 1 };
            if gone >= 3 {
                bail!(
                    "builder {l} went away; the job waits in its inbox (`hgl-ai build status {id}`)"
                );
            }
        }
    }
}

async fn gate(
    ctx: &Ctx,
    repo: &str,
    pr: u64,
    wait: bool,
    timeout: Duration,
) -> anyhow::Result<i32> {
    use hgl_ai_gh::gate::VerdictState;
    let (cfg, bus) = connect(ctx).await?;
    let s = hgl_ai_gh::Settings::from_config(&cfg)?;
    let gh = super::gh::gh_client(&cfg).await?;
    let builds = super::gh::gate_builds(&cfg)?;
    // A lost build (its builder gone, past its bound) is asked again on a live builder by
    // each round, as `gh land` does.
    let deadline = Instant::now() + timeout;
    let mut shown = String::new();
    loop {
        let (head, main, states) = hgl_ai_gh::builds::gate(&gh, &s, &bus, &builds, repo, pr)
            .await
            .map_err(super::gh::lift)?;
        let mut lines = format!(
            "{repo} #{pr} head {} on main {}\n",
            &head[..12],
            &main[..12]
        );
        for (n, st) in &states {
            let l = match st {
                VerdictState::Pass(v) => format!("{n} pass: {v}"),
                VerdictState::Fail(v) => format!("{n} FAIL: {v}"),
                VerdictState::NotRun(w) => format!("{n} NOT RUN: {w}"),
                VerdictState::Building(w) => format!("{n} building: {w}"),
                VerdictState::Missing(w) => {
                    format!("{n} missing: {}", w.as_deref().unwrap_or("none"))
                }
            };
            lines.push_str(&l);
            lines.push('\n');
        }
        if lines != shown {
            print!("{lines}");
            shown = lines;
        }
        let failed = states
            .iter()
            .any(|(_, st)| matches!(st, VerdictState::Fail(_) | VerdictState::NotRun(_)));
        let open = states
            .iter()
            .any(|(_, st)| matches!(st, VerdictState::Building(_)));
        if failed {
            return Ok(EXIT_FAIL);
        }
        if states
            .iter()
            .all(|(_, st)| matches!(st, VerdictState::Pass(_)))
        {
            return Ok(EXIT_OK);
        }
        if !wait || !open || Instant::now() >= deadline {
            return Err(
                NotReady(format!("{repo} #{pr}: local build verdicts not all judged")).into(),
            );
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

/// A local run's directory: --log-dir, the task's, or this host's.
fn find_local(id: &str, arg: Option<PathBuf>, cfg: Option<&Config>) -> Option<PathBuf> {
    let found = |d: &Path| d.join(store::STATUS_FILE).is_file() || d.join("pid").is_file();
    let mut dirs = Vec::new();
    if let Some(a) = arg {
        // A root of runs, or the run's own directory (a summary's `log_dir`).
        if a.file_name().is_some_and(|n| n == id) {
            dirs.push(a.clone());
        }
        dirs.push(a.join(id));
    }
    dirs.extend(task_dir().map(|t| t.join("build-logs").join(id)));
    dirs.push(data_dir(cfg).join("build-logs").join(id));
    // This host's builders keep theirs under `<work_root>/builder/logs/<label>/`.
    if let Some(root) = cfg.and_then(|c| c.work_root().ok())
        && let Ok(rd) = std::fs::read_dir(root.join("builder").join("logs"))
    {
        dirs.extend(rd.flatten().map(|e| e.path().join(id)));
    }
    dirs.into_iter().find(|d| found(d))
}

/// The local status, marked stopped when its process is gone before the end.
/// A run of `build run` on this host keeps its process id in `pid`; a builder's runs do
/// not (the builder service runs them), and their end is read from the hub.
fn local_run(dir: &Path) -> bool {
    dir.join("pid").is_file()
}

fn local_status(dir: &Path) -> Option<Summary> {
    let mut s = store::read_status(dir)?;
    if !local_run(dir) {
        return Some(s);
    }
    let pid: i32 = std::fs::read_to_string(dir.join("pid"))
        .ok()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(0);
    if !s.status.is_final() && !pid_alive(pid) {
        s.status = BuildStatus::Stopped;
        s.reason = Some("the build's process is gone".into());
    }
    Some(s)
}

async fn status(ctx: &Ctx, id: &str, log_dir: Option<PathBuf>) -> anyhow::Result<i32> {
    check_build_id(id)?;
    let cfg = ctx.load_config().ok();
    if let Some(d) = find_local(id, log_dir, cfg.as_ref()) {
        match local_status(&d) {
            Some(s) => print_summary(ctx, &s),
            None => println!("build {id}: starting (log {})", d.display()),
        }
        return Ok(EXIT_OK);
    }
    let (_, bus) = connect(ctx).await?;
    match store::run_get(&bus, id).await? {
        Some(s) => {
            print_summary(ctx, &s);
            Ok(EXIT_OK)
        }
        None => {
            eprintln!("hgl-ai: no build {id} here or on the hub");
            Ok(EXIT_FAIL)
        }
    }
}

async fn wait(
    ctx: &Ctx,
    id: &str,
    timeout: Option<Duration>,
    log_dir: Option<PathBuf>,
) -> anyhow::Result<i32> {
    check_build_id(id)?;
    let cfg = ctx.load_config().ok();
    let deadline = timeout.map(|t| Instant::now() + t);
    // A builder's run on this host is waited for as from anywhere: its receipt, or the
    // hub's copy.
    if let Some(d) = find_local(id, log_dir, cfg.as_ref()).filter(|d| local_run(d)) {
        loop {
            if let Some(s) = local_status(&d).filter(|s| s.status.is_final()) {
                print_summary(ctx, &s);
                return Ok(s.status.exit_code());
            }
            if store::read_status(&d).is_none() && local_status(&d).is_none() {
                let pid: i32 = std::fs::read_to_string(d.join("pid"))
                    .ok()
                    .and_then(|p| p.trim().parse().ok())
                    .unwrap_or(0);
                if !pid_alive(pid) {
                    bail!(
                        "build {id} ended without a summary; see {}",
                        d.join("run.out").display()
                    );
                }
            }
            if deadline.is_some_and(|t| Instant::now() >= t) {
                eprintln!("hgl-ai: build {id} has not ended");
                return Ok(EXIT_FAIL);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    let (_, bus) = connect(ctx).await?;
    // A build that ended before we asked still has its receipt in the stream.
    match wait_remote(&bus, id, None, timeout).await? {
        Some(s) => {
            print_summary(ctx, &s);
            Ok(s.status.exit_code())
        }
        None => {
            if let Some(s) = store::run_get(&bus, id).await? {
                print_summary(ctx, &s);
            }
            eprintln!("hgl-ai: build {id} has not ended");
            Ok(EXIT_FAIL)
        }
    }
}

async fn list(ctx: &Ctx, repo: Option<String>, failed: bool, limit: usize) -> anyhow::Result<i32> {
    let (_, bus) = connect(ctx).await?;
    for s in store::runs(&bus, repo.as_deref(), failed, limit).await? {
        if ctx.json {
            println!("{}", serde_json::to_string(&s)?);
            continue;
        }
        let head = s
            .head
            .as_deref()
            .map(|h| &h[..h.len().min(8)])
            .unwrap_or("-");
        println!(
            "{} {} {head} {}/{} {} {} errors {} failed {}",
            s.id,
            s.repo,
            s.host,
            s.platform,
            s.status,
            summary::ms(s.wall_ms),
            s.errors_total,
            s.tests.failed
        );
    }
    Ok(EXIT_OK)
}

fn log(
    ctx: &Ctx,
    id: &str,
    step: Option<String>,
    bytes: Option<String>,
    log_dir: Option<PathBuf>,
) -> anyhow::Result<i32> {
    check_build_id(id)?;
    let cfg = ctx.load_config().ok();
    let Some(d) = find_local(id, log_dir, cfg.as_ref()) else {
        bail!("build {id} has no log on this host (a remote build's log stays on its builder)");
    };
    let s = local_status(&d);
    let step = match step {
        Some(s) => s,
        None => s
            .as_ref()
            .and_then(|s| {
                s.steps
                    .iter()
                    .find(|x| !matches!(x.status.as_str(), "pass" | "skipped"))
                    .or(s.steps.last())
                    .map(|x| x.name.clone())
            })
            .ok_or_else(|| anyhow::anyhow!("build {id} has run no step yet"))?,
    };
    // A step's log, or a file the summary names inside the run's directory (a report).
    let path = if step.contains(['/', '\\']) || step.ends_with(".xml") {
        let rel = Path::new(&step);
        let inside = rel
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)));
        if !inside {
            bail!("{step}: name a file inside the build's directory, as the summary does");
        }
        d.join(rel)
    } else {
        d.join(format!("{step}.log"))
    };
    let real = std::fs::canonicalize(&path).with_context(|| format!("{}", path.display()))?;
    if !real.starts_with(std::fs::canonicalize(&d)?) {
        bail!("{step}: not inside the build's directory");
    }
    let mut f = std::fs::File::open(&real).with_context(|| format!("{}", path.display()))?;
    let len = f.metadata()?.len();
    const MAX: u64 = 64 << 10;
    let (start, end) = match bytes {
        Some(r) => {
            let (a, b) = r
                .split_once('-')
                .ok_or_else(|| anyhow::anyhow!("--bytes START-END"))?;
            let (a, b): (u64, u64) = (a.parse()?, b.parse()?);
            (a.min(len), b.min(len).max(a.min(len)))
        }
        None => (len.saturating_sub(MAX), len),
    };
    let end = end.min(start + MAX);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(end - start).read_to_end(&mut buf)?;
    use std::io::Write;
    std::io::stdout().write_all(&buf)?;
    if !buf.ends_with(b"\n") {
        println!();
    }
    Ok(EXIT_OK)
}

/// This host's executor for `platform`: its own, or a remote it drives (borrowing the
/// proxy's connection when the builder runs).
fn executor_for(cfg: &Config, platform: Platform) -> anyhow::Result<(Arc<dyn Executor>, String)> {
    if cfg.builder.local && Platform::current() == Some(platform) {
        let root = cfg.work_root()?.join("builder");
        return Ok((
            Arc::new(Local::new(root, cfg.builder.sdk.clone(), Git::default())),
            cfg.host.clone(),
        ));
    }
    let Some(r) = cfg.builder.remote.iter().find(|r| r.platform == platform) else {
        bail!(
            "this host has no {platform} builder (no [[builder.remote]] with platform = \"{platform}\")"
        );
    };
    let label = r.label(&cfg.host);
    let dialect =
        ssh::Dialect::parse(&r.shell).ok_or_else(|| anyhow::anyhow!("shell {:?}", r.shell))?;
    let conn = ssh::Conn::borrowed(
        &label,
        &r.ssh,
        hgl_ai_build::service::ssh_program(),
        &cfg.data_dir()?.join("builder").join("ssh"),
        dialect,
    )?;
    conn.connect()
        .map_err(|e| anyhow::anyhow!("{label}: {e}"))?;
    Ok((
        Arc::new(ssh::Ssh::new(
            platform,
            dialect,
            &r.work_dir,
            r.sdk.clone(),
            conn,
        )),
        label,
    ))
}

async fn shell(ctx: &Ctx, a: ShellArgs) -> anyhow::Result<i32> {
    let cfg = ctx.load_config()?;
    let src = source_dir(a.dir.clone())?;
    let repo = repo_name(a.repo.clone(), &src)?;
    let head = match &a.head {
        Some(h) => h.clone(),
        None => git_out(&src, &["rev-parse", "HEAD"])
            .ok_or_else(|| anyhow::anyhow!("no commit here; give --head SHA"))?,
    };
    hgl_ai_proto::check_head(&head)?;
    let id = new_build_id();
    let c = Checkout {
        id: id.clone(),
        repo: repo.clone(),
        url: cfg.github.clone_url_for(&repo),
        head,
        base: a.base.clone(),
        issue: a.issue,
        shell: true,
    };
    let cfg2 = cfg.clone();
    let platform = a.platform;
    let code = tokio::task::spawn_blocking(move || -> anyhow::Result<i32> {
        let (exec, label) = executor_for(&cfg2, platform)?;
        let ws = match exec.prepare(&c) {
            Ok(ws) => ws,
            Err(Prep::NotRun(r)) | Err(Prep::Unreachable(r)) => bail!("{label}: {r}"),
        };
        eprintln!(
            "hgl-ai: checkout of {} at {} on {label}: {}; removed when the shell ends",
            c.repo,
            &c.head[..12],
            ws.src
        );
        let mut env = driver::build_env(&c.id, &ws);
        for n in exec.sdk_names() {
            if let Some(p) = exec.sdk(&n) {
                env.push((sdk_var(&n), p));
            }
        }
        let code = exec.shell(&ws, &env);
        if let Err(e) = exec.cleanup(&ws) {
            eprintln!("hgl-ai: removing the checkout: {e:?}");
        }
        Ok(code?)
    })
    .await
    .context("the shell stopped")??;
    Ok(code)
}

async fn recipe(ctx: &Ctx, a: RecipeArgs) -> anyhow::Result<i32> {
    let (cfg, bus) = connect(ctx).await?;
    let repo_or = |r: Option<String>| -> anyhow::Result<String> {
        let src = source_dir(None)?;
        repo_name(r, &src)
    };
    match a.cmd {
        RecipeCmd::Show { repo, rev } => {
            let repo = repo_or(repo)?;
            let Some(s) = store::recipe_get(&bus, &repo, rev).await? else {
                eprintln!(
                    "hgl-ai: no recipe for {repo}{}",
                    rev.map(|r| format!(" at revision {r}")).unwrap_or_default()
                );
                return Ok(EXIT_FAIL);
            };
            if ctx.json {
                println!(
                    "{}",
                    serde_json::json!({ "repo": repo, "revision": s.revision, "text": s.text })
                );
            } else {
                eprintln!("# recipe.{repo} revision {}", s.revision);
                print!("{}", s.text);
            }
        }
        RecipeCmd::Export { repo, output } => {
            let repo = repo_or(repo)?;
            let Some(s) = store::recipe_get(&bus, &repo, None).await? else {
                eprintln!("hgl-ai: no recipe for {repo}");
                return Ok(EXIT_FAIL);
            };
            match output {
                Some(f) => {
                    std::fs::write(&f, &s.text).with_context(|| format!("{}", f.display()))?
                }
                None => print!("{}", s.text),
            }
        }
        RecipeCmd::Import { file, repo } => {
            let text = read_bounded(
                std::fs::File::open(&file).with_context(|| format!("{}", file.display()))?,
                "the recipe",
                hgl_ai_bus::MAX_VALUE_BYTES,
            )?;
            let (repo, rev) = store::recipe_put(&bus, &text, repo.as_deref()).await?;
            println!("recipe.{repo} revision {rev}");
        }
        RecipeCmd::Edit { repo } => {
            let repo = repo_or(repo)?;
            let current = store::recipe_get(&bus, &repo, None).await?;
            let text = current.as_ref().map(|s| s.text.clone()).unwrap_or_else(|| {
                format!("repo = \"{repo}\"\n\n[[step]]\nname = \"test\"\nrun = \"make test\"\n")
            });
            let dir = tempfile_dir(&cfg)?;
            let f = dir.join(format!("{repo}.toml"));
            std::fs::write(&f, &text)?;
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .unwrap_or_else(|_| "vi".into());
            loop {
                let st = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(format!("{editor} \"$1\""))
                    .arg("sh")
                    .arg(&f)
                    .status()?;
                if !st.success() {
                    bail!("the editor failed; nothing stored");
                }
                let new = std::fs::read_to_string(&f)?;
                if new == text {
                    println!("unchanged");
                    break;
                }
                match store::recipe_put(&bus, &new, Some(&repo)).await {
                    Ok((repo, rev)) => {
                        println!("recipe.{repo} revision {rev}");
                        break;
                    }
                    Err(e) => {
                        eprintln!("hgl-ai: {e}; edit again (an unchanged file stops)");
                        if std::fs::read_to_string(&f)? == text {
                            break;
                        }
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
        RecipeCmd::History { repo } => {
            let repo = repo_or(repo)?;
            for (rev, t) in store::recipe_history(&bus, &repo).await? {
                let when = chrono::DateTime::from_timestamp(t, 0)
                    .map(hgl_ai_proto::utc_stamp)
                    .unwrap_or_default();
                println!("{rev} {when}");
            }
        }
    }
    Ok(EXIT_OK)
}

/// A scratch directory for the editor: under the task's `tmp/`, else the data directory.
fn tempfile_dir(cfg: &Config) -> anyhow::Result<PathBuf> {
    let base = task_dir()
        .map(|t| t.join("tmp"))
        .filter(|t| t.is_dir())
        .unwrap_or_else(|| data_dir(Some(cfg)).join("tmp"));
    let d = base.join(format!("hgl-ai-recipe-{}", std::process::id()));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    use hgl_ai_proto::build::BuilderRecord;

    fn rec(label: &str, platform: Platform, reachable: bool, busy: u32) -> BuilderRecord {
        BuilderRecord {
            label: label.into(),
            inbox: Seat::new(format!("builder-{label}")).unwrap(),
            platform,
            arch: String::new(),
            executor: "local".into(),
            proxy: None,
            reachable,
            reason: None,
            state: "active".into(),
            slots: 1,
            busy,
            free_gb: None,
            toolchains: Default::default(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn chooses_a_ready_builder() {
        let b = vec![
            rec("a", Platform::Linux, true, 1),
            rec("b", Platform::Linux, true, 0),
            rec("c", Platform::Windows, false, 0),
            rec("d", Platform::Linux, false, 0),
        ];
        assert_eq!(choose(&b, Platform::Linux, None).unwrap().label, "b");
        assert_eq!(choose(&b, Platform::Linux, Some("d")).unwrap().label, "d");
        assert_eq!(
            choose(&b, Platform::Windows, None).unwrap().label,
            "c",
            "an unreachable one holds the job"
        );
        assert!(choose(&b, Platform::Macos, None).is_none());
        let mut r = b.clone();
        r[1].state = "draining".into();
        assert_eq!(choose(&r, Platform::Linux, None).unwrap().label, "a");
    }
}
