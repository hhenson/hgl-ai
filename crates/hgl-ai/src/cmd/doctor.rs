//! `hgl-ai doctor`: check configuration, hub, agent CLIs, skills, services and disk.

use hgl_ai_install::{BusHub, doctor};

use super::server::real_system;
use super::{Ctx, EXIT_FAIL, EXIT_OK};

pub async fn run(ctx: &Ctx) -> anyhow::Result<i32> {
    let sys = real_system(ctx)?;
    let checks = doctor::run(&sys, &BusHub).await;
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&checks)?);
    } else {
        for c in &checks {
            println!("{}", c.line());
        }
    }
    Ok(if doctor::any_fail(&checks) {
        EXIT_FAIL
    } else {
        EXIT_OK
    })
}
