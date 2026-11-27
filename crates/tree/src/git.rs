//! A thin runner over the `git` command line (docs/design.md §1: no git library).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{Result, TreeError};

/// How to run `git`: the program and extra environment (tests isolate the user's config).
#[derive(Debug, Clone)]
pub struct Git {
    pub program: PathBuf,
    pub env: Vec<(OsString, OsString)>,
}

impl Default for Git {
    fn default() -> Self {
        Git {
            program: PathBuf::from("git"),
            env: Vec::new(),
        }
    }
}

/// What a finished `git` command printed.
#[derive(Debug, Clone)]
pub struct Out {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Git {
    /// A runner that ignores the user's and the system's git configuration.
    pub fn isolated(global_config: &Path) -> Self {
        Git {
            program: PathBuf::from("git"),
            env: vec![
                (
                    "GIT_CONFIG_GLOBAL".into(),
                    global_config.as_os_str().to_owned(),
                ),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ],
        }
    }

    /// Run and return the outcome whatever the exit status.
    pub fn output<I, S>(&self, cwd: &Path, args: I) -> Result<Out>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output_env(cwd, args, &[])
    }

    /// [`Git::output`] with extra environment (`GIT_INDEX_FILE`, ...).
    pub fn output_env<I, S>(&self, cwd: &Path, args: I, env: &[(&str, &OsStr)]) -> Result<Out>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let out = Command::new(&self.program)
            .args(&args)
            .current_dir(cwd)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .envs(env.iter().map(|(k, v)| (k, v)))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| TreeError::io(&self.program, e))?;
        Ok(Out {
            ok: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Run and fail unless it exits 0; returns stdout.
    pub fn run<I, S>(&self, cwd: &Path, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_env(cwd, args, &[])
    }

    /// [`Git::run`] with extra environment.
    pub fn run_env<I, S>(&self, cwd: &Path, args: I, env: &[(&str, &OsStr)]) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let out = self.output_env(cwd, &args, env)?;
        if out.ok {
            Ok(out.stdout)
        } else {
            Err(TreeError::Git {
                args: args
                    .iter()
                    .map(|a| a.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                code: out.code,
                stderr: out.stderr.trim().to_string(),
            })
        }
    }

    /// `true` when the command exits 0.
    pub fn check<I, S>(&self, cwd: &Path, args: I) -> Result<bool>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Ok(self.output(cwd, args)?.ok)
    }
}
