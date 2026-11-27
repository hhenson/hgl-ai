//! Command groups. Exit status (docs/design.md §2): 0 success, 1 failure, 2 usage,
//! 3 a gate not satisfied, 10 hub unreachable.

pub mod agent;
pub mod bridge;
pub mod build;
pub mod builder;
pub mod cap;
pub mod client;
pub mod console;
pub mod doctor;
pub mod gh;
pub mod note;
pub mod owner;
pub mod seat;
pub mod server;
pub mod state;
pub mod tree;
pub mod usage;

use std::path::PathBuf;

use anyhow::Context as _;
use hgl_ai_config::Config;

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAIL: i32 = 1;
pub const EXIT_NOT_READY: i32 = 3;
pub const EXIT_UNREACHABLE: i32 = 10;

/// Global options every command sees.
pub struct Ctx {
    pub config_path: Option<PathBuf>,
    pub json: bool,
}

impl Ctx {
    pub fn config_path(&self) -> anyhow::Result<PathBuf> {
        match &self.config_path {
            Some(p) => Ok(p.clone()),
            None => Ok(Config::default_path()?),
        }
    }

    pub fn load_config(&self) -> anyhow::Result<Config> {
        let p = self.config_path()?;
        Config::load(&p).with_context(|| {
            format!(
                "no usable configuration at {}; run `hgl-ai client install`",
                p.display()
            )
        })
    }
}

/// A gate that is not satisfied; maps to exit status 3.
#[derive(Debug, thiserror::Error)]
#[error("not ready: {0}")]
pub struct NotReady(pub String);

pub fn exit_code_for(e: &anyhow::Error) -> i32 {
    if e.downcast_ref::<NotReady>().is_some() {
        return EXIT_NOT_READY;
    }
    if let Some(hgl_ai_bus::BusError::Unreachable { .. }) = e.downcast_ref::<hgl_ai_bus::BusError>()
    {
        return EXIT_UNREACHABLE;
    }
    EXIT_FAIL
}
