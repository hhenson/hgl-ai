//! `hgl-ai agent`: interactive agent sessions in tmux `hgl-<seat>`.

use std::path::PathBuf;

use hgl_ai_install::agent;
use hgl_ai_proto::Seat;

use super::server::{lift, real_system};
use super::{Ctx, EXIT_FAIL, EXIT_OK};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Start the seat's runtime with its model and effort in a detached tmux session.
    Start {
        seat: Seat,
        /// Working directory (default <work_root>/<seat>/_interactive).
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Attach this terminal to the seat's session.
    Attach { seat: Seat },
    /// Kill the seat's session.
    Stop { seat: Seat },
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    let sys = real_system(ctx)?;
    match args.cmd {
        Sub::Start { seat, cwd } => {
            let cfg = ctx.load_config()?;
            let cwd = cwd.map(|p| std::path::absolute(&p).unwrap_or(p));
            agent::start(&sys, &cfg, seat.as_str(), cwd).map_err(lift)?;
            Ok(EXIT_OK)
        }
        Sub::Attach { seat } => {
            if !agent::session_exists(&sys, seat.as_str()) {
                anyhow::bail!(
                    "no tmux session {}; start it with `hgl-ai agent start {seat}`",
                    agent::session_name(seat.as_str())
                );
            }
            let c = agent::attach_cmd(hgl_ai_install::System::env(&sys), seat.as_str());
            let status = std::process::Command::new(&c.program)
                .args(&c.args)
                .status()?;
            Ok(status.code().unwrap_or(EXIT_FAIL))
        }
        Sub::Stop { seat } => {
            agent::stop(&sys, seat.as_str()).map_err(lift)?;
            Ok(EXIT_OK)
        }
    }
}
