//! The agent skill (docs/design.md §7): one `SKILL.md`, embedded in the binary,
//! copied into each installed agent CLI.

use std::path::PathBuf;

use hgl_ai_proto::Runtime;

use crate::env::Env;
use crate::sys::System;
use crate::{Result, io_at};

/// `skill/SKILL.md` at build time.
pub const SKILL: &str = include_str!("../../../skill/SKILL.md");

/// The skill's directory for one CLI.
pub fn skill_dir(env: &Env, rt: Runtime) -> PathBuf {
    let base = match rt {
        Runtime::Claude => ".claude/skills",
        Runtime::Codex => ".codex/skills",
        Runtime::Agy => ".gemini/config/skills",
    };
    env.home.join(base).join("hgl-ai")
}

pub fn skill_path(env: &Env, rt: Runtime) -> PathBuf {
    skill_dir(env, rt).join("SKILL.md")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Missing,
    Current,
    Stale,
}

pub fn status(sys: &dyn System, rt: Runtime) -> Status {
    let p = skill_path(sys.env(), rt);
    match sys.read(&p) {
        Ok(b) if b == SKILL.as_bytes() => Status::Current,
        Ok(_) => Status::Stale,
        Err(_) => Status::Missing,
    }
}

/// Write the embedded skill for `rt`; returns its status before the write.
pub fn install(sys: &dyn System, rt: Runtime) -> Result<Status> {
    let before = status(sys, rt);
    if before != Status::Current {
        let p = skill_path(sys.env(), rt);
        sys.write(&p, SKILL.as_bytes(), 0o644).map_err(io_at(&p))?;
    }
    Ok(before)
}

/// Remove the skill's directory for `rt`; returns whether there was one.
pub fn remove(sys: &dyn System, rt: Runtime) -> Result<bool> {
    let d = skill_dir(sys.env(), rt);
    let had = sys.exists(&d);
    sys.remove_dir_all(&d).map_err(io_at(&d))?;
    Ok(had)
}

/// The agent CLIs found on `PATH`.
pub fn installed_clis(sys: &dyn System) -> Vec<(Runtime, PathBuf)> {
    Runtime::ALL
        .iter()
        .filter_map(|&rt| sys.which(rt.as_str()).map(|p| (rt, p)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Os;
    use crate::sys::fake::Fake;

    #[test]
    fn front_matter() {
        assert!(SKILL.starts_with("---\nname: hgl-ai\ndescription: "));
        assert!(SKILL.contains("$HGL_AI_SEAT"));
    }

    #[test]
    fn paths_per_cli() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        let h = &f.env.home;
        assert_eq!(
            skill_path(&f.env, Runtime::Claude),
            h.join(".claude/skills/hgl-ai/SKILL.md")
        );
        assert_eq!(
            skill_path(&f.env, Runtime::Codex),
            h.join(".codex/skills/hgl-ai/SKILL.md")
        );
        assert_eq!(
            skill_path(&f.env, Runtime::Agy),
            h.join(".gemini/config/skills/hgl-ai/SKILL.md")
        );
    }

    #[test]
    fn install_status_remove() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        assert_eq!(status(&f, Runtime::Codex), Status::Missing);
        assert_eq!(install(&f, Runtime::Codex).unwrap(), Status::Missing);
        assert_eq!(status(&f, Runtime::Codex), Status::Current);
        std::fs::write(skill_path(&f.env, Runtime::Codex), "old").unwrap();
        assert_eq!(status(&f, Runtime::Codex), Status::Stale);
        assert_eq!(install(&f, Runtime::Codex).unwrap(), Status::Stale);
        assert_eq!(
            std::fs::read_to_string(skill_path(&f.env, Runtime::Codex)).unwrap(),
            SKILL
        );
        assert!(remove(&f, Runtime::Codex).unwrap());
        assert!(!skill_dir(&f.env, Runtime::Codex).exists());
        assert!(
            f.env.home.join(".codex/skills").exists(),
            "only our directory goes"
        );
        assert!(!remove(&f, Runtime::Codex).unwrap());
    }

    #[test]
    fn finds_clis() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        f.tool("codex");
        f.tool("agy");
        let found: Vec<Runtime> = installed_clis(&f).into_iter().map(|(r, _)| r).collect();
        assert_eq!(found, [Runtime::Codex, Runtime::Agy]);
    }
}
