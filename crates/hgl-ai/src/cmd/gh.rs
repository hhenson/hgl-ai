//! `hgl-ai gh`: GitHub services and gates (crates/gh). Gates exit 3 with one line when they
//! are not satisfied; every command has `--dry-run`, which reads GitHub and changes nothing.

use std::path::PathBuf;
use std::time::Duration;

use hgl_ai_bus::Bus;
use hgl_ai_gh::exec::{GhCli, ReadOnly};
use hgl_ai_gh::gate::GateBuilds;
use hgl_ai_gh::gate::{Kind, Opts};
use hgl_ai_gh::land::{LandOpts, Landing};
use hgl_ai_gh::{Settings, coord, land, pins, release, tap, verdict};
use hgl_ai_proto::Seat;

use super::{Ctx, EXIT_OK, NotReady};

fn duration(s: &str) -> Result<Duration, String> {
    hgl_ai_config::parse_duration(s).ok_or_else(|| format!("{s:?}: use 30s, 90m, 2h or 3d"))
}

#[derive(clap::Args)]
pub struct Args {
    // Boxed: the command enum in main.rs stays small.
    #[command(subcommand)]
    cmd: Box<Cmd>,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// The coordinator service: pull requests, `hold`, alerts, backlog, metrics and triage intake.
    Coord {
        #[command(subcommand)]
        cmd: CoordCmd,
    },
    /// Merge a pull request at HEAD if every gate holds (one attempt).
    MergeIfClean(GateArgs),
    /// Land a pull request: the gate until it decides, then `main`, `landed.log`, notes and pins.
    /// A pull request an earlier attempt merged has the steps it left finished.
    Land(LandArgs),
    /// The wrapper's submodule pins.
    Pins {
        #[command(subcommand)]
        cmd: PinsCmd,
    },
    /// Workflow runs of a release.
    Release {
        #[command(subcommand)]
        cmd: ReleaseCmd,
    },
    /// The Homebrew tap.
    Tap {
        #[command(subcommand)]
        cmd: TapCmd,
    },
    /// Approve an exact head: one GitHub comment, then an `approved` note to the author.
    Approve(VerdictArgs),
    /// Send findings back on an exact head: one GitHub comment, then a `sendback` note.
    Sendback(VerdictArgs),
}

#[derive(clap::Args)]
#[command(group = clap::ArgGroup::new("text").required(true).args(["body", "body_file"]))]
struct VerdictArgs {
    repo: String,
    pr: u64,
    /// The full 40-character head the verdict is for.
    head: String,
    /// Seat to send to (default: the author's, from the trees, branch or Ready comment).
    #[arg(long = "to")]
    to: Vec<Seat>,
    /// The task's issue (default: the one in the branch name, else PR).
    #[arg(long)]
    issue: Option<u64>,
    #[arg(long)]
    body: Option<String>,
    /// A file with the body; `-` reads standard input.
    #[arg(long)]
    body_file: Option<PathBuf>,
    /// Sendback only: the class the task moves up to after its second sendback for the
    /// same finding (ROLE-15).
    #[arg(long, value_name = "CLASS")]
    class: Option<hgl_ai_proto::TaskClass>,
    /// Send the note only (the comment is already on GitHub).
    #[arg(long)]
    no_comment: bool,
    #[arg(long)]
    dry_run: bool,
}

impl VerdictArgs {
    fn body(&self) -> anyhow::Result<String> {
        match (&self.body, &self.body_file) {
            (Some(b), _) => Ok(b.clone()),
            (None, Some(p)) if p.as_os_str() == "-" => {
                Ok(std::io::read_to_string(std::io::stdin())?)
            }
            (None, Some(p)) => std::fs::read_to_string(p)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display())),
            (None, None) => anyhow::bail!("give --body or --body-file"),
        }
    }
}

#[derive(clap::Subcommand)]
enum CoordCmd {
    /// Check PRs at least 5 minutes apart. Run backlog and intake every 15 minutes.
    Run {
        /// One round of each, then exit.
        #[arg(long)]
        once: bool,
        /// Print the planned state writes and notes.
        #[arg(long)]
        dry_run: bool,
        /// Default: `github.coord_interval`.
        #[arg(long, value_parser = duration)]
        interval: Option<Duration>,
    },
}

#[derive(clap::Args)]
struct GateArgs {
    repo: String,
    pr: u64,
    /// The full 40-character head the merge is for.
    head: String,
    /// The bot reviewed HEAD with findings and every one is triaged (threads answered and resolved).
    #[arg(long)]
    triaged: bool,
    /// The flagged added lines of HEAD were read and hold no real local path.
    #[arg(long)]
    hygiene_read: bool,
    /// Report BEHIND instead of bringing the branch up to date.
    #[arg(long)]
    no_update: bool,
    /// Say what it would do; change nothing.
    #[arg(long)]
    dry_run: bool,
    /// The issue the pull request closes (its worktrees get the `landed` note).
    #[arg(long)]
    closes: Option<u64>,
    /// What `main` gained, one clause (default: the pull request's title).
    #[arg(long)]
    what: Option<String>,
    /// Seat to tell besides `all` (default: found from the trees, branch or Ready comment).
    #[arg(long = "seat")]
    seats: Vec<Seat>,
    /// Post no GitHub comment for the `landed` note.
    #[arg(long)]
    no_comment: bool,
    /// How long to wait for the landing lock (default: 0s for merge-if-clean, 2h for land).
    #[arg(long, value_parser = duration)]
    lock_wait: Option<Duration>,
}

impl GateArgs {
    fn opts(&self) -> LandOpts {
        LandOpts {
            gate: Opts {
                triaged: self.triaged,
                hygiene_read: self.hygiene_read,
                no_update: self.no_update,
                closes: self.closes,
                tool_made: false,
                merge_until: None,
            },
            closes: self.closes,
            what: self.what.clone(),
            seats: self.seats.clone(),
            comment: !self.no_comment,
            lock_wait: self.lock_wait.unwrap_or(Duration::ZERO),
            ..LandOpts::default()
        }
    }
}

#[derive(clap::Args)]
struct LandArgs {
    #[command(flatten)]
    gate: GateArgs,
    /// Give up after this long.
    #[arg(long, value_parser = duration, default_value = "50m")]
    timeout: Duration,
    /// Between gate attempts.
    #[arg(long, value_parser = duration, default_value = "4m")]
    interval: Duration,
    /// Do not raise the wrapper's pins after the merge.
    #[arg(long)]
    no_pins: bool,
    /// Lines appended to a pin commit's message (attribution).
    #[arg(long)]
    trailer: Option<String>,
    /// Lines appended to a pin pull request's body.
    #[arg(long)]
    footer: Option<String>,
}

#[derive(clap::Subcommand)]
enum PinsCmd {
    /// Raise the pins to what they follow, through a pin pull request.
    Sync {
        /// What the followed repository's main gained, one clause.
        #[arg(long, default_value = "")]
        what: String,
        #[arg(long)]
        trailer: Option<String>,
        #[arg(long)]
        footer: Option<String>,
        #[arg(long, value_parser = duration, default_value = "25m")]
        timeout: Duration,
        #[arg(long, value_parser = duration, default_value = "90s")]
        interval: Duration,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Subcommand)]
enum ReleaseCmd {
    /// Hold the landing lock while every workflow run on SHA finishes; exit 0 when all are green.
    Watch {
        sha: String,
        /// Default: `github.release.repo`.
        #[arg(long)]
        repo: Option<String>,
        /// Default: `github.release.event`.
        #[arg(long)]
        event: Option<String>,
        /// Default: `github.release.timeout`.
        #[arg(long, value_parser = duration)]
        timeout: Option<Duration>,
        #[arg(long, value_parser = duration, default_value = "60s")]
        interval: Duration,
        #[arg(long, value_parser = duration, default_value = "2h")]
        lock_wait: Duration,
        /// Read the runs once; take no lock.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Subcommand)]
enum TapCmd {
    /// The bottle flow: after the tap pull request is reviewed clean, build and check the bottle and add its stanza.
    Release {
        version: String,
        /// The tap pull request (default: the open one from `<branch_prefix><formula>-VERSION`).
        #[arg(long)]
        pr: Option<u64>,
        /// The reviewed tap commit (default: the pull request's head).
        #[arg(long)]
        sha: Option<String>,
        #[arg(long)]
        skip_review_wait: bool,
        #[arg(long)]
        trailer: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Render `Formula/hgl-ai.rb` for a release from its `.sha256` files (needs no configuration).
    Formula {
        #[arg(long)]
        version: String,
        /// Directory holding `hgl-ai-VERSION-<target>.tar.gz.sha256`.
        #[arg(long)]
        checksums: PathBuf,
        /// `owner/name` whose releases hold the archives.
        #[arg(long, default_value = tap::this_repository())]
        repo: String,
        /// A template other than the built-in one.
        #[arg(long)]
        template: Option<PathBuf>,
        /// Write here instead of standard output.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

/// The hub for a dry run, which only reads it (local build verdicts, review receipts, a
/// landing's record); without one the gate says what it could not read.
async fn dry_run_bus(cfg: &hgl_ai_config::Config) -> Option<Bus> {
    match Bus::connect(cfg).await {
        Ok(b) => Some(b),
        Err(e) => {
            eprintln!("dry run without the hub ({e}): verdicts and receipts are not read");
            None
        }
    }
}

/// `gh` with the coordinator's token where configured, checked signed in: every command
/// that uses it reads GitHub, so say so clearly when `gh` cannot.
pub(crate) async fn gh_client(cfg: &hgl_ai_config::Config) -> anyhow::Result<GhCli> {
    let gh = match &cfg.github.token_file {
        Some(p) => GhCli::default()
            .with_token_file(&hgl_ai_config::expand(p)?)
            .map_err(lift)?,
        None => GhCli::default(),
    };
    hgl_ai_gh::exec::check_auth(&gh).await.map_err(lift)?;
    Ok(gh)
}

/// The gate builds a landing asks for (BUILD-15), sent as this seat (the controller), else
/// this host.
pub(crate) fn gate_builds(
    cfg: &hgl_ai_config::Config,
) -> anyhow::Result<std::sync::Arc<GateBuilds>> {
    let me = std::env::var("HGL_AI_SEAT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cfg.host.clone());
    Ok(std::sync::Arc::new(GateBuilds::new(
        hgl_ai_proto::Seat::new(me.clone())?,
        me,
    )))
}

/// A bus error stays a bus error, so an unreachable hub exits 10.
pub(crate) fn lift(e: hgl_ai_gh::Error) -> anyhow::Error {
    match e {
        hgl_ai_gh::Error::Bus(b) => b.into(),
        hgl_ai_gh::Error::Locked(m) | hgl_ai_gh::Error::LeaseLost(m) => NotReady(m).into(),
        other => other.into(),
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    if let Cmd::Tap {
        cmd:
            TapCmd::Formula {
                version,
                checksums,
                repo,
                template,
                out,
            },
    } = args.cmd.as_ref()
    {
        return formula(
            version,
            checksums,
            repo,
            template.as_deref(),
            out.as_deref(),
        );
    }
    let cfg = ctx.load_config()?;
    let s = Settings::from_config(&cfg)?;
    let gh = gh_client(&cfg).await?;
    match *args.cmd {
        Cmd::Coord {
            cmd:
                CoordCmd::Run {
                    once,
                    dry_run,
                    interval,
                },
        } => {
            let mut interval = interval.unwrap_or(s.coord_interval);
            if interval < coord::MIN_INTERVAL {
                eprintln!("interval {interval:?} is below the 5 minute minimum; using 5m");
                interval = coord::MIN_INTERVAL;
            }
            let o = coord::CoordOpts {
                once,
                dry_run,
                interval,
            };
            let mut out = std::io::stdout();
            if dry_run {
                coord::run(&ReadOnly(gh), &s, None, &o, &mut out)
                    .await
                    .map_err(lift)?;
            } else {
                let bus = Bus::connect(&cfg).await?;
                coord::run(&gh, &s, Some(&bus), &o, &mut out)
                    .await
                    .map_err(lift)?;
            }
            Ok(EXIT_OK)
        }
        Cmd::MergeIfClean(a) => {
            let mut o = a.opts();
            o.builds = Some(gate_builds(&cfg)?);
            let l = if a.dry_run {
                let bus = dry_run_bus(&cfg).await;
                land::plan(&ReadOnly(gh), &s, bus.as_ref(), &a.repo, a.pr, &a.head, &o).await
            } else {
                let bus = Bus::connect(&cfg).await?;
                land::merge_once(&gh, &s, &bus, &a.repo, a.pr, &a.head, &o).await
            };
            report(ctx, l.map_err(lift)?)
        }
        Cmd::Land(a) => {
            let mut o = a.gate.opts();
            o.timeout = a.timeout;
            o.interval = a.interval;
            o.sync_pins = !a.no_pins;
            o.trailer = a.trailer.clone();
            o.footer = a.footer.clone();
            o.lock_wait = a.gate.lock_wait.unwrap_or(LandOpts::default().lock_wait);
            o.builds = Some(gate_builds(&cfg)?);
            let g = &a.gate;
            let l = if g.dry_run {
                let bus = dry_run_bus(&cfg).await;
                land::plan(&ReadOnly(gh), &s, bus.as_ref(), &g.repo, g.pr, &g.head, &o).await
            } else {
                let bus = Bus::connect(&cfg).await?;
                land::land(&gh, &s, &bus, &g.repo, g.pr, &g.head, &o).await
            };
            report(ctx, l.map_err(lift)?)
        }
        Cmd::Pins {
            cmd:
                PinsCmd::Sync {
                    what,
                    trailer,
                    footer,
                    timeout,
                    interval,
                    dry_run,
                },
        } => {
            let o = pins::PinOpts {
                what,
                trailer,
                footer,
                timeout,
                interval,
            };
            let r = if dry_run {
                pins::sync(&ReadOnly(gh), &s, None, &o, true).await
            } else {
                let bus = Bus::connect(&cfg).await?;
                pins::sync(&gh, &s, Some(&bus), &o, false).await
            }
            .map_err(lift)?;
            if !r.ok() {
                return Err(NotReady(r.line()).into());
            }
            println!("{}", r.line());
            Ok(EXIT_OK)
        }
        Cmd::Release {
            cmd:
                ReleaseCmd::Watch {
                    sha,
                    repo,
                    event,
                    timeout,
                    interval,
                    lock_wait,
                    dry_run,
                },
        } => {
            let o = release::WatchOpts {
                repo: repo.unwrap_or_else(|| s.release.repo.clone()),
                event: event.unwrap_or_else(|| s.release.event.clone()),
                timeout: timeout.unwrap_or(s.release_timeout),
                interval,
                lock_wait,
                dry_run,
            };
            let (text, green) = if dry_run {
                release::watch(&ReadOnly(gh), &s, None, &sha, &o).await
            } else {
                let bus = Bus::connect(&cfg).await?;
                release::watch(&gh, &s, Some(&bus), &sha, &o).await
            }
            .map_err(lift)?;
            print!("{text}");
            if green {
                Ok(EXIT_OK)
            } else {
                Err(NotReady(release::verdict_line(false, &sha)).into())
            }
        }
        Cmd::Tap {
            cmd:
                TapCmd::Release {
                    version,
                    pr,
                    sha,
                    skip_review_wait,
                    trailer,
                    dry_run,
                },
        } => {
            let o = tap::TapOpts {
                pr,
                sha,
                skip_review_wait,
                trailer,
                dry_run,
                ..tap::TapOpts::default()
            };
            let r = if dry_run {
                tap::release(&ReadOnly(gh), &s, &version, &o).await
            } else {
                tap::release(&gh, &s, &version, &o).await
            }
            .map_err(lift)?;
            match r {
                tap::TapOutcome::Pushed(l) | tap::TapOutcome::Would(l) => {
                    println!("{l}");
                    Ok(EXIT_OK)
                }
                tap::TapOutcome::Stop(l) => Err(NotReady(l).into()),
            }
        }
        Cmd::Tap {
            cmd: TapCmd::Formula { .. },
        } => unreachable!("handled before the configuration is read"),
        Cmd::Approve(a) => verdict(&cfg, &s, gh, a, hgl_ai_proto::Kind::Approved).await,
        Cmd::Sendback(a) => verdict(&cfg, &s, gh, a, hgl_ai_proto::Kind::Sendback).await,
    }
}

async fn verdict(
    cfg: &hgl_ai_config::Config,
    s: &Settings,
    gh: GhCli,
    a: VerdictArgs,
    kind: hgl_ai_proto::Kind,
) -> anyhow::Result<i32> {
    let o = verdict::VerdictOpts {
        kind,
        to: a.to.clone(),
        issue: a.issue,
        body: a.body()?,
        class: a.class,
        comment: !a.no_comment,
        dry_run: a.dry_run,
    };
    let r = if a.dry_run {
        verdict::verdict(&ReadOnly(gh), s, None, &a.repo, a.pr, &a.head, &o).await
    } else {
        let bus = Bus::connect(cfg).await?;
        verdict::verdict(&gh, s, Some(&bus), &a.repo, a.pr, &a.head, &o).await
    }
    .map_err(lift)?;
    match r {
        verdict::VerdictOutcome::Sent(l) | verdict::VerdictOutcome::Would(l) => {
            println!("{l}");
            Ok(EXIT_OK)
        }
        verdict::VerdictOutcome::Refused(l) => Err(NotReady(l).into()),
    }
}

/// Print a gate's result; a gate that is not satisfied is exit 3 with its line.
fn report(ctx: &Ctx, l: Landing) -> anyhow::Result<i32> {
    let o = &l.outcome;
    if ctx.json {
        let mut v = o.to_json();
        v["after"] = serde_json::json!(l.after);
        println!("{v}");
    } else if o.kind.satisfied() {
        println!("{}", o.line);
        for a in &l.after {
            println!("  {a}");
        }
    }
    if o.kind.satisfied() {
        Ok(EXIT_OK)
    } else {
        let line = if o.kind == Kind::Updated {
            format!("{} (CI runs on the update first)", o.line)
        } else {
            o.line.clone()
        };
        Err(NotReady(line).into())
    }
}

fn formula(
    version: &str,
    checksums: &std::path::Path,
    repo: &str,
    template: Option<&std::path::Path>,
    out: Option<&std::path::Path>,
) -> anyhow::Result<i32> {
    let version = version.strip_prefix('v').unwrap_or(version);
    let tmpl = match template {
        Some(p) => std::fs::read_to_string(p)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display()))?,
        None => tap::TEMPLATE.to_string(),
    };
    let sums = tap::read_checksums(checksums, version)?;
    let text = tap::render_formula(&tmpl, repo, version, &sums)?;
    match out {
        Some(p) => {
            std::fs::write(p, text).map_err(|e| anyhow::anyhow!("writing {}: {e}", p.display()))?
        }
        None => print!("{text}"),
    }
    Ok(EXIT_OK)
}
