//! The `hgl-ai` command line (docs/design.md §2). Each command group lives in
//! `cmd/<group>.rs` with its own arguments; this file only dispatches.

mod cmd;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "hgl-ai",
    version,
    about = "Coordination tooling for a mixed fleet of coding agents"
)]
struct Cli {
    /// Configuration file (default: $XDG_CONFIG_HOME/hgl-ai/config.toml).
    #[arg(long, global = true, env = "HGL_AI_CONFIG")]
    config: Option<std::path::PathBuf>,
    /// Print machine-readable JSON where a command supports it.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install and run the hub (NATS server) on this host.
    Server(cmd::server::Args),
    /// Configure this host as a client of a hub.
    Client(cmd::client::Args),
    /// Provision the stream and buckets on the hub (idempotent).
    Init,
    /// Check configuration, hub, agent CLIs, skills, services and disk.
    Doctor,
    /// Manage this host's seats.
    Seat(cmd::seat::Args),
    /// Start or attach an interactive agent session for a seat.
    Agent(cmd::agent::Args),
    /// Run the bridge daemon.
    Bridge(cmd::bridge::Args),
    /// Send, wait for and inspect notes.
    Note(cmd::note::Args),
    /// Read and write shared state.
    State(cmd::state::Args),
    /// List live hosts.
    Hosts,
    /// List and remove task worktrees.
    Tree(cmd::tree::Args),
    /// Run a command tree under a memory and time cap.
    Cap(cmd::cap::Args),
    /// GitHub services and gates.
    Gh(cmd::gh::Args),
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let ctx = cmd::Ctx {
        config_path: cli.config,
        json: cli.json,
    };
    let res = match cli.command {
        Command::Server(a) => cmd::server::run(&ctx, a).await,
        Command::Client(a) => cmd::client::run(&ctx, a).await,
        Command::Init => cmd::state::init(&ctx).await,
        Command::Doctor => cmd::doctor::run(&ctx).await,
        Command::Seat(a) => cmd::seat::run(&ctx, a).await,
        Command::Agent(a) => cmd::agent::run(&ctx, a).await,
        Command::Bridge(a) => cmd::bridge::run(&ctx, a).await,
        Command::Note(a) => cmd::note::run(&ctx, a).await,
        Command::State(a) => cmd::state::run(&ctx, a).await,
        Command::Hosts => cmd::state::hosts(&ctx).await,
        Command::Tree(a) => cmd::tree::run(&ctx, a).await,
        Command::Cap(a) => cmd::cap::run(&ctx, a).await,
        Command::Gh(a) => cmd::gh::run(&ctx, a).await,
    };
    let code = match res {
        Ok(code) => code,
        Err(e) => {
            eprintln!("hgl-ai: {e:#}");
            cmd::exit_code_for(&e)
        }
    };
    std::process::exit(code);
}
