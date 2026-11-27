//! `hgl-ai cap`: run a command tree under a memory and time cap (docs/roles.md ROLE-17).

use std::ffi::OsString;
use std::time::Duration;

use anyhow::Context as _;
use hgl_ai_cap::CapSpec;

use super::Ctx;
use super::note::duration;

#[derive(clap::Args)]
#[command(trailing_var_arg = true)]
pub struct Args {
    /// Memory limit of the whole command tree, such as 16G or 512M.
    #[arg(value_name = "SIZE", value_parser = size)]
    size: u64,
    /// Time limit (240, 90m, 2h): TERM to the tree, then KILL after --kill-after.
    #[arg(long, value_name = "DUR", value_parser = duration)]
    time: Option<Duration>,
    /// Grace between TERM and KILL.
    #[arg(long, value_name = "DUR", value_parser = duration, default_value = "10s")]
    kill_after: Duration,
    /// Without systemd: limit the tree by its sampled resident memory only, with no
    /// per-process address-space limit. For sanitizer builds (ASan, TSan), which reserve
    /// far more address space than they use.
    #[arg(long)]
    no_address_limit: bool,
    /// The command and its arguments.
    #[arg(value_name = "CMD", required = true, allow_hyphen_values = true, value_parser = clap::value_parser!(OsString))]
    command: Vec<OsString>,
}

fn size(s: &str) -> Result<u64, String> {
    hgl_ai_cap::parse_size(s).map_err(|e| e.to_string())
}

/// Exits with the command's status; 137 or 143 when a limit ended it.
pub async fn run(_ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    let spec = CapSpec {
        mem_bytes: args.size,
        time: args.time,
        kill_after: args.kill_after,
        address_limit: !args.no_address_limit,
    };
    let argv = args.command;
    tokio::task::spawn_blocking(move || hgl_ai_cap::run(&spec, &argv))
        .await
        .context("the cap supervisor stopped")?
        .context("running the command")
}
