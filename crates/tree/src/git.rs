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
        let out = self.raw_output_env(cwd, args, env)?;
        Ok(Out {
            ok: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn raw_output_env<I, S>(
        &self,
        cwd: &Path,
        args: I,
        env: &[(&str, &OsStr)],
    ) -> Result<std::process::Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new(&self.program)
            .args(args)
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .envs(env.iter().map(|(k, v)| (k, v)))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| TreeError::io(&self.program, e))
    }

    /// Run and return stdout bytes unchanged, including non-UTF-8 patch content.
    pub fn run_bytes_env<I, S>(
        &self,
        cwd: &Path,
        args: I,
        env: &[(&str, &OsStr)],
    ) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let out = self.raw_output_env(cwd, &args, env)?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(TreeError::Git {
                args: args
                    .iter()
                    .map(|a| a.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
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

/// Git operations owned by Trees must name their repository explicitly.
#[derive(Debug)]
pub(crate) struct TreeGit {
    pub(crate) git: Git,
    pub(crate) clones: PathBuf,
}

type GitInvocation = (Vec<OsString>, Vec<(String, OsString)>);

impl TreeGit {
    pub(crate) fn checkout_git_dir(&self, cwd: &Path) -> Result<Option<PathBuf>> {
        let dotgit = cwd.join(".git");
        let dir = if dotgit.is_dir() {
            dotgit
        } else if let Some(dir) = crate::git_dir(cwd) {
            dir
        } else {
            return Ok(None);
        };
        let dir = match dir.canonicalize() {
            Ok(dir) => dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(TreeError::io(&dir, e)),
        };
        Ok(dir.join("HEAD").is_file().then_some(dir))
    }

    fn args_env<I, S>(&self, cwd: &Path, args: I, env: &[(&str, &OsStr)]) -> Result<GitInvocation>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let bare = cwd.parent() == Some(self.clones.as_path());
        let git_dir = if bare {
            cwd.to_path_buf()
        } else {
            self.checkout_git_dir(cwd)?
                .ok_or_else(|| TreeError::Invalid(format!("invalid checkout {}", cwd.display())))?
        };
        let mut argv = Vec::new();
        let mut vars: Vec<_> = env
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_owned()))
            .collect();
        if bare {
            argv.extend([OsString::from("--git-dir"), git_dir.into_os_string()]);
        } else {
            vars.push(("GIT_DIR".into(), git_dir.into_os_string()));
            vars.push((
                "GIT_WORK_TREE".into(),
                cwd.canonicalize()
                    .map_err(|e| TreeError::io(cwd, e))?
                    .into_os_string(),
            ));
        }
        argv.extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        Ok((argv, vars))
    }

    pub(crate) fn output<I, S>(&self, cwd: &Path, args: I) -> Result<Out>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output_env(cwd, args, &[])
    }

    fn output_env<I, S>(&self, cwd: &Path, args: I, env: &[(&str, &OsStr)]) -> Result<Out>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (args, vars) = self.args_env(cwd, args, env)?;
        let vars: Vec<_> = vars
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_os_str()))
            .collect();
        self.git.output_env(cwd, args, &vars)
    }

    pub(crate) fn run<I, S>(&self, cwd: &Path, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_env(cwd, args, &[])
    }

    pub(crate) fn run_env<I, S>(
        &self,
        cwd: &Path,
        args: I,
        env: &[(&str, &OsStr)],
    ) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (args, vars) = self.args_env(cwd, args, env)?;
        let vars: Vec<_> = vars
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_os_str()))
            .collect();
        self.git.run_env(cwd, args, &vars)
    }

    pub(crate) fn run_bytes_env<I, S>(
        &self,
        cwd: &Path,
        args: I,
        env: &[(&str, &OsStr)],
    ) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (args, vars) = self.args_env(cwd, args, env)?;
        let vars: Vec<_> = vars
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_os_str()))
            .collect();
        self.git.run_bytes_env(cwd, args, &vars)
    }

    pub(crate) fn check<I, S>(&self, cwd: &Path, args: I) -> Result<bool>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Ok(self.output(cwd, args)?.ok)
    }
}
