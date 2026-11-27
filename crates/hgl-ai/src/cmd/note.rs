//! `hgl-ai note`: send and receive notes (bus work).

use super::Ctx;

#[derive(clap::Args)]
pub struct Args {}

pub async fn run(_ctx: &Ctx, _args: Args) -> anyhow::Result<i32> {
    super::not_yet("note")
}
