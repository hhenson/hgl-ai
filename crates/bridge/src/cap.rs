//! How a turn's command tree is capped (ROLE-15).
//!
//! Shim: `hgl-ai-cap` has no library API yet, so the bridge wraps the command in the
//! documented command line `hgl-ai cap SIZE -- CMD ...` (docs/design.md §2). Swap
//! [`SelfCap`] for a cap-crate implementation of [`Capper`] when that API exists.

use std::path::PathBuf;

use crate::proc::CmdSpec;

pub trait Capper: Send + Sync {
    /// The command that runs `cmd` under a `mem_gb` GiB memory cap.
    fn wrap(&self, mem_gb: u64, cmd: CmdSpec) -> CmdSpec;
}

/// `<exe> cap <N>G -- <program> <args...>`, with the same cwd and environment.
pub struct SelfCap {
    pub exe: PathBuf,
}

impl Capper for SelfCap {
    fn wrap(&self, mem_gb: u64, cmd: CmdSpec) -> CmdSpec {
        let mut args = vec!["cap".into(), format!("{mem_gb}G").into(), "--".into(), cmd.program.into_os_string()];
        args.extend(cmd.args);
        CmdSpec { program: self.exe.clone(), args, env: cmd.env, cwd: cmd.cwd }
    }
}

/// No cap (tests).
pub struct NoCap;

impl Capper for NoCap {
    fn wrap(&self, _: u64, cmd: CmdSpec) -> CmdSpec {
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_cap_prefixes_the_command() {
        let cmd = CmdSpec { program: "codex".into(), args: vec!["exec".into()], env: vec![], cwd: "/w".into() };
        let w = SelfCap { exe: "/bin/hgl-ai".into() }.wrap(16, cmd);
        assert_eq!(w.program, PathBuf::from("/bin/hgl-ai"));
        assert_eq!(w.args, ["cap", "16G", "--", "codex", "exec"].map(std::ffi::OsString::from).to_vec());
        assert_eq!(w.cwd, PathBuf::from("/w"));
    }
}
