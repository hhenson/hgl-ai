//! The installing user's environment, captured once so tests can substitute a
//! temporary home.

use std::path::{Component, Path, PathBuf};

use crate::sys::System;
use crate::{Result, other};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    MacOs,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Linux
        }
    }
}

#[derive(Debug, Clone)]
pub struct Env {
    pub home: PathBuf,
    /// The `config.toml` in use.
    pub config_path: PathBuf,
    /// `$XDG_CONFIG_HOME`, else `~/.config`.
    pub xdg_config_home: PathBuf,
    /// `PATH` at install time; the bridge service gets this one.
    pub path_var: String,
    pub os: Os,
    /// `std::env::consts::ARCH`.
    pub arch: String,
    /// The running `hgl-ai` executable.
    pub exe: PathBuf,
    /// Inside a tmux client (`$TMUX` set).
    pub in_tmux: bool,
}

impl Env {
    /// The real process environment.
    pub fn from_process(config_path: PathBuf) -> Result<Env> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| other("HOME is not set"))?;
        let xdg_config_home = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(x) if !x.is_empty() => PathBuf::from(x),
            _ => home.join(".config"),
        };
        let config_path = std::path::absolute(&config_path).unwrap_or(config_path);
        let exe = std::env::current_exe()
            .map_err(|e| other(format!("locating the hgl-ai executable: {e}")))?;
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        Ok(Env {
            home,
            config_path,
            xdg_config_home,
            path_var: std::env::var("PATH").unwrap_or_default(),
            os: Os::current(),
            arch: std::env::consts::ARCH.to_string(),
            exe,
            in_tmux: std::env::var_os("TMUX").is_some_and(|t| !t.is_empty()),
        })
    }

    /// Expand a leading `~` against this environment's home.
    pub fn expand(&self, p: &str) -> PathBuf {
        if p == "~" {
            return self.home.clone();
        }
        match p.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None => PathBuf::from(p),
        }
    }

    /// The form stored in `config.toml`: `~/...` under home, else as given.
    pub fn contract(&self, p: &Path) -> String {
        match p.strip_prefix(&self.home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        }
    }

    pub fn config_dir(&self) -> PathBuf {
        self.config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Where `client install` keeps the hub token: beside `config.toml`.
    pub fn client_token_path(&self) -> PathBuf {
        self.config_dir().join("hub.token")
    }

    /// The executable user services run: the Homebrew `opt` path when running
    /// from a Cellar, so `brew upgrade` keeps working; else the current one.
    pub fn service_exe(&self, sys: &dyn System) -> PathBuf {
        match brew_opt_path(&self.exe) {
            Some(opt) if sys.exists(&opt) => opt,
            _ => self.exe.clone(),
        }
    }

    /// `PATH` for services and agent sessions: the install-time `PATH` with the
    /// directory of `exe` first, so agents find `hgl-ai`.
    pub fn service_path(&self, exe: &Path) -> String {
        let dir = exe
            .parent()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        let mut parts: Vec<&str> = self.path_var.split(':').filter(|p| !p.is_empty()).collect();
        if !dir.is_empty() && !parts.contains(&dir.as_str()) {
            parts.insert(0, &dir);
        }
        parts.join(":")
    }
}

/// `<prefix>/Cellar/<name>/<version>/<rest>` → `<prefix>/opt/<name>/<rest>`.
pub fn brew_opt_path(exe: &Path) -> Option<PathBuf> {
    let comps: Vec<Component> = exe.components().collect();
    let i = comps.iter().position(|c| c.as_os_str() == "Cellar")?;
    if comps.len() < i + 4 {
        return None;
    }
    let mut out: PathBuf = comps[..i].iter().collect();
    out.push("opt");
    out.push(comps[i + 1]);
    for c in &comps[i + 3..] {
        out.push(c);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brew_opt() {
        assert_eq!(
            brew_opt_path(Path::new("/opt/homebrew/Cellar/hgl-ai/0.1.0/bin/hgl-ai")),
            Some(PathBuf::from("/opt/homebrew/opt/hgl-ai/bin/hgl-ai"))
        );
        assert_eq!(brew_opt_path(Path::new("/usr/local/bin/hgl-ai")), None);
        assert_eq!(brew_opt_path(Path::new("/x/Cellar/hgl-ai")), None);
    }

    #[test]
    fn expand_and_contract() {
        let env = crate::sys::fake::env(Path::new("/h"), Os::Linux);
        assert_eq!(env.expand("~/a/b"), PathBuf::from("/h/a/b"));
        assert_eq!(env.expand("/abs"), PathBuf::from("/abs"));
        assert_eq!(env.contract(Path::new("/h/.config/x")), "~/.config/x");
        assert_eq!(env.contract(Path::new("/elsewhere")), "/elsewhere");
    }

    #[test]
    fn service_path_puts_exe_dir_first() {
        let mut env = crate::sys::fake::env(Path::new("/h"), Os::Linux);
        env.path_var = "/usr/bin:/bin".into();
        assert_eq!(
            env.service_path(Path::new("/opt/x/bin/hgl-ai")),
            "/opt/x/bin:/usr/bin:/bin"
        );
        assert_eq!(
            env.service_path(Path::new("/usr/bin/hgl-ai")),
            "/usr/bin:/bin"
        );
    }
}
