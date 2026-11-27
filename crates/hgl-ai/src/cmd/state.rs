//! `hgl-ai state`, `hgl-ai init`, `hgl-ai hosts` (bus work).

use super::Ctx;

#[derive(clap::Args)]
pub struct Args {}

pub async fn run(_ctx: &Ctx, _args: Args) -> anyhow::Result<i32> {
    super::not_yet("state")
}

pub async fn init(_ctx: &Ctx) -> anyhow::Result<i32> {
    super::not_yet("init")
}

pub async fn hosts(_ctx: &Ctx) -> anyhow::Result<i32> {
    super::not_yet("hosts")
}
