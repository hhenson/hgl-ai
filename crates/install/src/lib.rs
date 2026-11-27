//! Hub and client installers for hgl-ai (docs/install.md, docs/design.md §6, §7):
//! `nats-server` provisioning, user services, the agent skill, interactive
//! agent sessions and `doctor`.
//!
//! Every filesystem and process action goes through [`sys::System`], so a dry
//! run ([`sys::DryRun`]) or a test fake never touches the host.

pub mod agent;
pub mod client;
pub mod doctor;
pub mod env;
pub mod hub;
pub mod nats;
pub mod server;
pub mod service;
pub mod skill;
pub mod sys;
pub mod token;

use std::path::{Path, PathBuf};

pub use env::{Env, Os};
pub use hub::{BusHub, Hub};
pub use sys::{Cmd, DryRun, Output, RealSystem, System};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("`{cmd}` failed ({status}): {detail}")]
    Command {
        cmd: String,
        status: String,
        detail: String,
    },
    #[error(transparent)]
    Config(#[from] hgl_ai_config::ConfigError),
    #[error(transparent)]
    Bus(#[from] hgl_ai_bus::BusError),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Map an I/O error to [`Error::Io`] for `path`.
pub(crate) fn io_at(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub(crate) fn other(msg: impl Into<String>) -> Error {
    Error::Other(msg.into())
}

/// Report a finished step; a dry run has already said what it would do.
pub(crate) fn done(sys: &dyn System, line: &str) {
    if !sys.dry_run() {
        sys.say(line);
    }
}

/// Run a mutating command and fail unless it exits 0.
pub(crate) fn run_ok(sys: &dyn System, cmd: &Cmd) -> Result<Output> {
    let out = sys.run(cmd).map_err(|e| Error::Command {
        cmd: cmd.to_string(),
        status: "not started".into(),
        detail: e.to_string(),
    })?;
    if out.ok() {
        Ok(out)
    } else {
        Err(Error::Command {
            cmd: cmd.to_string(),
            status: out.status_text(),
            detail: out.detail(),
        })
    }
}

/// Load the configuration at `env.config_path`, or `None` when the file is absent.
pub fn load_config(sys: &dyn System, env: &Env) -> Result<Option<hgl_ai_config::Config>> {
    let p = &env.config_path;
    if !sys.exists(p) {
        return Ok(None);
    }
    let text = sys.read_to_string(p).map_err(io_at(p))?;
    let cfg: hgl_ai_config::Config =
        toml::from_str(&text).map_err(|source| hgl_ai_config::ConfigError::Parse {
            path: p.clone(),
            source,
        })?;
    cfg.validate()?;
    Ok(Some(cfg))
}

/// Validate and write the configuration through `sys` (mode 0644).
pub fn save_config(sys: &dyn System, env: &Env, cfg: &hgl_ai_config::Config) -> Result<()> {
    cfg.validate()?;
    let text = toml::to_string_pretty(cfg)
        .map_err(|e| other(format!("serialising the configuration: {e}")))?;
    sys.write(&env.config_path, text.as_bytes(), 0o644)
        .map_err(io_at(&env.config_path))
}

/// Seconds to wait for a fresh hub to answer.
pub const HUB_WAIT_SECS: u64 = 20;
