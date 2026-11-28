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
    /// Ask the owner a question; prints its id.
    Ask(Box<cmd::owner::AskArgs>),
    /// Owner threads: list, show, reply, withdraw.
    Thread(cmd::owner::ThreadArgs),
    /// Status reports for the owner.
    Status(cmd::owner::StatusArgs),
    /// The controller's decision log.
    Decision(cmd::owner::DecisionArgs),
    /// The owner's web console.
    Console(cmd::console::Args),
    /// The Slack Socket Mode adapter for owner questions.
    Slack(cmd::slack::Args),
    /// Subscription usage per login: collect, show, label.
    Usage(cmd::usage::Args),
    /// Build and test by recipe, here or on a builder of another platform.
    Build(cmd::build::Args),
    /// The builder service and the fleet's builders.
    Builder(cmd::builder::Args),
}

#[tokio::main]
async fn main() {
    // Logs go to stderr; `HGL_AI_LOG` sets the filter (default: warnings).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HGL_AI_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
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
        Command::Ask(a) => cmd::owner::ask(&ctx, *a).await,
        Command::Thread(a) => cmd::owner::thread(&ctx, a).await,
        Command::Status(a) => cmd::owner::status(&ctx, a).await,
        Command::Decision(a) => cmd::owner::decision(&ctx, a).await,
        Command::Console(a) => cmd::console::run(&ctx, a).await,
        Command::Slack(a) => cmd::slack::run(&ctx, a).await,
        Command::Usage(a) => cmd::usage::run(&ctx, a).await,
        Command::Build(a) => cmd::build::run(&ctx, a).await,
        Command::Builder(a) => cmd::builder::run(&ctx, a).await,
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

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::{Parser, error::ErrorKind};

    #[test]
    fn verdict_text_arguments() {
        for command in ["approve", "sendback"] {
            let base = ["hgl-ai", "gh", command, "repo", "42", "head", "--to", "w1"];
            for text in [
                vec!["--body", "message"],
                vec!["--body-file", "message.txt"],
            ] {
                for no_comment in [false, true] {
                    let mut args = base.to_vec();
                    args.extend(&text);
                    if no_comment {
                        args.push("--no-comment");
                    }
                    assert!(Cli::try_parse_from(args).is_ok(), "{command}: {text:?}");
                }
            }
            assert_eq!(
                Cli::try_parse_from(base).err().unwrap().kind(),
                ErrorKind::MissingRequiredArgument
            );
            let mut both = base.to_vec();
            both.extend(["--body", "message", "--body-file", "message.txt"]);
            assert_eq!(
                Cli::try_parse_from(both).err().unwrap().kind(),
                ErrorKind::ArgumentConflict
            );
        }
    }
}
