//! `hgl-ai agent start|attach|stop`: an interactive session for a seat in tmux
//! session `hgl-<seat>`.

use std::path::PathBuf;

use hgl_ai_config::{Config, Resolved};

use crate::env::Env;
use crate::sys::{Cmd, System};
use crate::{Result, io_at, other, run_ok};

pub fn session_name(seat: &str) -> String {
    format!("hgl-{seat}")
}

/// An exact tmux target, so `hgl-w1` never matches `hgl-w10`.
fn target(seat: &str) -> String {
    format!("={}", session_name(seat))
}

/// Model and effort flags of an interactive session (docs/design.md §4); the bridge's
/// headless turns use the same builder.
pub fn runtime_args(r: &Resolved) -> Vec<String> {
    hgl_ai_config::model_args(r)
}

/// A planned session.
#[derive(Debug, Clone)]
pub struct Launch {
    pub session: String,
    /// `<work_root>/<seat>/_interactive`: the session's task directory.
    pub task_dir: PathBuf,
    pub cwd: PathBuf,
    pub resolved: Resolved,
    pub cmd: Cmd,
}

/// Build the `tmux new-session` command for `seat`.
pub fn plan(sys: &dyn System, cfg: &Config, seat: &str, cwd: Option<PathBuf>) -> Result<Launch> {
    let env = sys.env();
    let sc = cfg.seat(seat).ok_or_else(|| {
        other(format!(
            "no seat {seat} in {}; add it with `hgl-ai seat add`",
            env.config_path.display()
        ))
    })?;
    let r = cfg
        .resolve(&sc.binding)
        .map_err(|e| other(format!("seat {seat}: {e}")))?;
    let bin = sys.which(r.runtime.as_str()).ok_or_else(|| {
        other(format!(
            "{} is not on PATH; install it and sign in",
            r.runtime
        ))
    })?;
    let task_dir = env
        .expand(&cfg.paths.work_root)
        .join(seat)
        .join("_interactive");
    let cwd = cwd.unwrap_or_else(|| task_dir.clone());
    let session = session_name(seat);
    let exe = env.service_exe(sys);
    let vars = [
        ("HGL_AI_SEAT", seat.to_string()),
        ("HGL_AI_ROLE", sc.role.to_string()),
        ("HGL_AI_CONFIG", env.config_path.display().to_string()),
        ("HGL_AI_TASK_DIR", task_dir.display().to_string()),
        ("TMPDIR", task_dir.join("tmp").display().to_string()),
        ("PATH", env.service_path(&exe)),
    ];
    let mut cmd = Cmd::new("tmux")
        .args(["new-session", "-d", "-s", &session, "-c"])
        .arg(cwd.display().to_string());
    for (k, v) in vars {
        cmd = cmd.arg("-e").arg(format!("{k}={v}"));
    }
    let cmd = cmd.arg(bin.display().to_string()).args(runtime_args(&r));
    Ok(Launch {
        session,
        task_dir,
        cwd,
        resolved: r,
        cmd,
    })
}

pub fn session_exists(sys: &dyn System, seat: &str) -> bool {
    sys.query(&Cmd::new("tmux").args(["has-session", "-t", &target(seat)]))
        .is_ok_and(|o| o.ok())
}

/// Start the seat's runtime in a detached tmux session; refuse if it exists.
pub fn start(sys: &dyn System, cfg: &Config, seat: &str, cwd: Option<PathBuf>) -> Result<Launch> {
    if sys.which("tmux").is_none() {
        return Err(other("tmux is not on PATH"));
    }
    let l = plan(sys, cfg, seat, cwd)?;
    if session_exists(sys, seat) {
        return Err(other(format!(
            "tmux session {} already exists; attach with `hgl-ai agent attach {seat}`",
            l.session
        )));
    }
    for d in [l.task_dir.join("tmp"), l.cwd.clone()] {
        sys.mkdir(&d).map_err(io_at(&d))?;
    }
    run_ok(sys, &l.cmd)?;
    sys.say(&format!(
        "started {} {} for seat {seat} in tmux session {}; attach with `hgl-ai agent attach {seat}`",
        l.resolved.runtime, l.resolved.model, l.session
    ));
    Ok(l)
}

/// The command that attaches this terminal (or switches this tmux client).
pub fn attach_cmd(env: &Env, seat: &str) -> Cmd {
    let verb = if env.in_tmux {
        "switch-client"
    } else {
        "attach-session"
    };
    Cmd::new("tmux").args([verb, "-t", &target(seat)])
}

pub fn stop(sys: &dyn System, seat: &str) -> Result<()> {
    if !session_exists(sys, seat) {
        return Err(other(format!("no tmux session {}", session_name(seat))));
    }
    run_ok(
        sys,
        &Cmd::new("tmux").args(["kill-session", "-t", &target(seat)]),
    )?;
    sys.say(&format!("stopped tmux session {}", session_name(seat)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::sys::fake::Fake;
    use crate::{Os, Output};
    use hgl_ai_proto::{Family, Runtime};

    fn resolved(runtime: Runtime, model: &str, effort: Option<&str>) -> Resolved {
        Resolved {
            runtime,
            family: Family::Claude,
            model: model.into(),
            effort: effort.map(String::from),
        }
    }

    #[test]
    fn runtime_flags() {
        assert_eq!(
            runtime_args(&resolved(Runtime::Claude, "claude-opus-5-5", Some("high"))),
            ["--model", "claude-opus-5-5", "--effort", "high"]
        );
        assert_eq!(
            runtime_args(&resolved(Runtime::Codex, "gpt-6.1-sol", Some("xhigh"))),
            ["-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=xhigh"]
        );
        assert_eq!(
            runtime_args(&resolved(Runtime::Agy, "gemini-4-carbon", Some("high"))),
            ["--model", "gemini-4-carbon-high", "--effort", "high"]
        );
        assert_eq!(
            runtime_args(&resolved(Runtime::Agy, "claude-opus-5-5-high", None)),
            ["--model", "claude-opus-5-5-high"]
        );
        assert_eq!(
            runtime_args(&resolved(Runtime::Claude, "claude-haiku-5-5", None)),
            ["--model", "claude-haiku-5-5"]
        );
    }

    fn config() -> Config {
        let text = r#"
host = "hub"
[paths]
work_root = "~/work"
data_dir = "~/.local/share/hgl-ai"
[[seat]]
name = "claude"
role = "controller"
runtime = "claude"
tier = "frontier"
effort = "high"
"#;
        toml::from_str(text).unwrap()
    }

    #[test]
    fn tmux_command() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        f.tool("claude");
        f.tool("tmux");
        let cfg = config();
        let l = plan(&f, &cfg, "claude", None).unwrap();
        let h = f.env.home.display().to_string();
        let td = format!("{h}/work/claude/_interactive");
        assert_eq!(l.session, "hgl-claude");
        assert_eq!(l.cwd, PathBuf::from(&td));
        let want = [
            "tmux new-session -d -s hgl-claude -c",
            &td,
            "-e HGL_AI_SEAT=claude -e HGL_AI_ROLE=controller",
            &format!("-e HGL_AI_CONFIG={h}/.config/hgl-ai/config.toml"),
            &format!("-e HGL_AI_TASK_DIR={td} -e TMPDIR={td}/tmp"),
            &format!("-e PATH={h}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"),
            "/usr/bin/claude --model claude-opus-5-5 --effort high",
        ]
        .join(" ");
        assert_eq!(l.cmd.line(), want);

        let l = plan(&f, &cfg, "claude", Some(PathBuf::from("/src/x"))).unwrap();
        assert!(l.cmd.line().contains("-c /src/x -e"));
        assert!(
            plan(&f, &cfg, "w9", None)
                .unwrap_err()
                .to_string()
                .contains("no seat w9")
        );
    }

    #[test]
    fn start_refuses_a_running_session() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        f.tool("claude");
        f.tool("tmux");
        let cfg = config();
        f.on(|c| {
            if c.args.first().map(String::as_str) == Some("has-session") {
                Output::failure(1, "")
            } else {
                Output::success("")
            }
        });
        start(&f, &cfg, "claude", None).unwrap();
        assert!(f.env.home.join("work/claude/_interactive/tmp").is_dir());
        assert_eq!(f.runs().len(), 1);
        assert!(
            f.cmds
                .borrow()
                .iter()
                .any(|(m, c)| !m && c.line() == "tmux has-session -t =hgl-claude")
        );

        f.on(|_| Output::success(""));
        let err = start(&f, &cfg, "claude", None).unwrap_err().to_string();
        assert!(
            err.contains("already exists") && err.contains("hgl-ai agent attach claude"),
            "{err}"
        );
        stop(&f, "claude").unwrap();
        assert_eq!(f.runs().last().unwrap(), "tmux kill-session -t =hgl-claude");
    }

    #[test]
    fn attach() {
        let mut env = crate::sys::fake::env(Path::new("/h"), Os::Linux);
        assert_eq!(
            attach_cmd(&env, "w1").line(),
            "tmux attach-session -t =hgl-w1"
        );
        env.in_tmux = true;
        assert_eq!(
            attach_cmd(&env, "w1").line(),
            "tmux switch-client -t =hgl-w1"
        );
    }
}
