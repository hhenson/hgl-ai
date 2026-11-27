//! `hgl-ai ask`, `thread`, `status`, `decision`: the agents' side of the owner channel
//! (docs/owner.md §6). The owner answers in the console, never here.

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use hgl_ai_bus::{Bus, MAX_NOTE_BYTES};
use hgl_ai_owner::{self as owner, Ask, Priority, QOption, ThreadKind};
use hgl_ai_proto::Seat;

use super::note::read_bounded;
use super::{Ctx, EXIT_FAIL, EXIT_OK};

fn text(body: Option<String>, file: Option<PathBuf>, what: &str) -> anyhow::Result<String> {
    match (body, file) {
        (Some(b), _) => Ok(b),
        (None, Some(p)) if p.as_os_str() == "-" => {
            read_bounded(std::io::stdin().lock(), "standard input", MAX_NOTE_BYTES)
        }
        (None, Some(p)) => {
            let f = std::fs::File::open(&p).with_context(|| format!("opening {}", p.display()))?;
            read_bounded(f, &p.display().to_string(), MAX_NOTE_BYTES)
        }
        (None, None) => bail!("give the {what} with --{what} or --{what}-file"),
    }
}

async fn connect(ctx: &Ctx) -> anyhow::Result<Bus> {
    Ok(Bus::connect(&ctx.load_config()?).await?)
}

#[derive(clap::Args)]
pub struct AskArgs {
    /// A short description: the question's title.
    #[arg(long)]
    title: String,
    /// p1 blocks work now, p2 blocks work soon, p3 whenever.
    #[arg(long)]
    priority: Priority,
    /// The issue it concerns (`repo#number`) or another identifier.
    #[arg(long, value_name = "R#N")]
    issue: Option<String>,
    #[arg(long)]
    context: Option<String>,
    /// Read the context from FILE, or `-` for standard input.
    #[arg(long, value_name = "FILE")]
    context_file: Option<PathBuf>,
    #[arg(long)]
    question: String,
    /// An option, `LABEL:LINE` (repeatable).
    #[arg(long = "option", value_name = "LABEL:LINE", value_parser = QOption::parse)]
    options: Vec<QOption>,
    /// The label of the option you suggest.
    #[arg(long, value_name = "LABEL")]
    suggest: Option<String>,
    /// Asking seat.
    #[arg(long = "from", value_name = "SEAT", env = "HGL_AI_SEAT")]
    from: Seat,
}

/// `hgl-ai ask`: prints the new question's id.
pub async fn ask(ctx: &Ctx, a: AskArgs) -> anyhow::Result<i32> {
    let context = match (a.context, a.context_file) {
        (None, None) => String::new(),
        (b, f) => text(b, f, "context")?,
    };
    let q = Ask {
        title: a.title,
        priority: a.priority,
        issue: a.issue,
        context,
        question: a.question,
        options: a.options,
        suggested: a.suggest,
    };
    q.validate().map_err(anyhow::Error::msg)?;
    let id = owner::ask(&connect(ctx).await?, &a.from, q).await?;
    if ctx.json {
        println!("{}", serde_json::json!({ "id": id }));
    } else {
        println!("{id}");
    }
    Ok(EXIT_OK)
}

#[derive(clap::Args)]
pub struct ThreadArgs {
    #[command(subcommand)]
    cmd: ThreadCmd,
}

#[derive(clap::Subcommand)]
enum ThreadCmd {
    /// Threads, open questions first by priority (`id state kind priority issue title`).
    List {
        /// Only open threads.
        #[arg(long)]
        open: bool,
        #[arg(long)]
        kind: Option<ThreadKind>,
        /// Only threads whose description, issue or id contains TEXT.
        #[arg(long, value_name = "TEXT")]
        search: Option<String>,
    },
    /// One thread in full; exit status 1 when there is none.
    Show { id: String },
    /// Reply in a thread.
    Reply {
        id: String,
        #[arg(long)]
        body: Option<String>,
        /// Read the body from FILE, or `-` for standard input.
        #[arg(long, value_name = "FILE")]
        body_file: Option<PathBuf>,
        #[arg(long = "from", value_name = "SEAT", env = "HGL_AI_SEAT")]
        from: Seat,
    },
    /// Withdraw an open question.
    Withdraw {
        id: String,
        #[arg(long, default_value = "")]
        reason: String,
        #[arg(long = "from", value_name = "SEAT", env = "HGL_AI_SEAT")]
        from: Seat,
    },
}

pub async fn thread(ctx: &Ctx, a: ThreadArgs) -> anyhow::Result<i32> {
    match a.cmd {
        ThreadCmd::List { open, kind, search } => {
            let all = owner::by_urgency(owner::list(&connect(ctx).await?).await?);
            for t in all.iter().filter(|t| {
                (!open || t.is_open())
                    && kind.is_none_or(|k| t.kind == k)
                    && search.as_deref().is_none_or(|q| t.matches(q))
            }) {
                if ctx.json {
                    println!("{}", serde_json::to_string(t)?);
                } else {
                    println!("{}", t.line());
                }
            }
        }
        ThreadCmd::Show { id } => {
            let Some(t) = owner::show_full(&connect(ctx).await?, &id).await? else {
                eprintln!(
                    "hgl-ai: no thread {id} (a question just asked appears once the console has indexed it)"
                );
                return Ok(EXIT_FAIL);
            };
            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&t)?);
            } else {
                print!("{}", t.render());
            }
        }
        ThreadCmd::Reply {
            id,
            body,
            body_file,
            from,
        } => {
            let body = text(body, body_file, "body")?;
            let seq = owner::reply(&connect(ctx).await?, &from, &id, &body).await?;
            println!("seq {seq}");
        }
        ThreadCmd::Withdraw { id, reason, from } => {
            let seq = owner::withdraw(&connect(ctx).await?, &from, &id, &reason).await?;
            println!("seq {seq}");
        }
    }
    Ok(EXIT_OK)
}

#[derive(clap::Args)]
pub struct StatusArgs {
    #[command(subcommand)]
    cmd: StatusCmd,
}

#[derive(clap::Subcommand)]
enum StatusCmd {
    /// Post a status report for the owner (Markdown).
    Post {
        #[arg(long)]
        body: Option<String>,
        /// Read the body from FILE, or `-` for standard input.
        #[arg(long, value_name = "FILE")]
        body_file: Option<PathBuf>,
        #[arg(long = "from", value_name = "SEAT", env = "HGL_AI_SEAT")]
        from: Seat,
    },
    /// Recent status reports, newest first.
    List {
        #[arg(long, default_value_t = 7)]
        days: i64,
        #[arg(long, default_value_t = 20)]
        max: usize,
    },
}

pub async fn status(ctx: &Ctx, a: StatusArgs) -> anyhow::Result<i32> {
    match a.cmd {
        StatusCmd::Post {
            body,
            body_file,
            from,
        } => {
            let body = text(body, body_file, "body")?;
            let seq = owner::status_post(&connect(ctx).await?, &from, &body).await?;
            println!("seq {seq}");
        }
        StatusCmd::List { days, max } => {
            let since = chrono::Utc::now() - chrono::Duration::days(days.max(1));
            for (seq, s) in owner::statuses(&connect(ctx).await?, since, max).await? {
                if ctx.json {
                    println!("{}", serde_json::json!({"seq": seq, "status": s}));
                } else {
                    println!(
                        "--- {} from {} (seq {seq}) ---\n{}",
                        s.at,
                        s.sender,
                        s.body.trim_end()
                    );
                }
            }
        }
    }
    Ok(EXIT_OK)
}

#[derive(clap::Args)]
pub struct DecisionArgs {
    #[command(subcommand)]
    cmd: DecisionCmd,
}

#[derive(clap::Subcommand)]
enum DecisionCmd {
    /// Append a judgement a later turn needs to `decisions.log` (`-`: standard input).
    Add {
        line: String,
        #[arg(long = "from", value_name = "SEAT", env = "HGL_AI_SEAT")]
        from: Seat,
    },
    /// The newest lines of `decisions.log`.
    Tail {
        #[arg(default_value_t = 50)]
        n: usize,
    },
}

pub async fn decision(ctx: &Ctx, a: DecisionArgs) -> anyhow::Result<i32> {
    match a.cmd {
        DecisionCmd::Add { line, from } => {
            let line = if line == "-" {
                read_bounded(std::io::stdin().lock(), "standard input", 4096)?
            } else {
                line
            };
            if line.trim().is_empty() {
                bail!("an empty line");
            }
            let stamped = format!("{} {from}: {}", hgl_ai_proto::utc_now_stamp(), line.trim());
            owner::decision_append(&connect(ctx).await?, &stamped).await?;
        }
        DecisionCmd::Tail { n } => {
            for l in owner::decisions_tail(&connect(ctx).await?, n).await? {
                println!("{l}");
            }
        }
    }
    Ok(EXIT_OK)
}
