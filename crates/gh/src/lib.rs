//! GitHub services and gates for hgl-ai (docs/design.md §2, docs/protocol.md §8).
//!
//! Everything shells out to `gh` (no GitHub library). Each command is split in two: pure
//! functions that decide over parsed JSON (unit-tested with fixtures), and a thin layer that
//! runs `gh` and the bus. Every command has a dry run that reads GitHub and prints what it
//! would do, and changes nothing.
//!
//! | Module | Command |
//! | --- | --- |
//! | [`coord`] | `gh coord run`: the batched pull request table `HGL_STATE/prs`, `hold`, alerts |
//! | [`gate`] | `gh merge-if-clean`: the merge gate |
//! | [`land`] | `gh land`: the gate in a loop, then `main`, `landed.log`, `landed` notes, pins |
//! | [`progress`] | `HGL_STATE/landing.<repo>.<pr>`: what a landing has done, so a retry resumes |
//! | [`pins`] | `gh pins sync`: the wrapper's submodule pins |
//! | [`release`] | `gh release watch`: every workflow run on a commit |
//! | [`tap`] | `gh tap release`, `gh tap formula`: the Homebrew tap |
//! | [`verdict`] | `gh approve`, `gh sendback`: the controller's verdict on an exact head |

pub mod bot;
pub mod coord;
pub mod exec;
pub mod gate;
pub mod land;
pub mod lock;
pub mod pins;
pub mod progress;
pub mod release;
pub mod state;
pub mod store;
pub mod table;
pub mod tap;
pub mod verdict;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use hgl_ai_config::{Config, Pin, ReleaseWatch, RepoGate, Tap, parse_duration};
use hgl_ai_proto::Seat;
use regex::Regex;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("`gh {args}` failed: {msg}")]
    Command { args: String, msg: String },
    #[error("GitHub API rate limit reached: {0}")]
    RateLimited(String),
    /// Another landing holds the repository's landing lock: try again later.
    #[error("{0}")]
    Locked(String),
    /// The landing lock expired or was taken while held; the landing stopped.
    #[error("{0}")]
    LeaseLost(String),
    /// `gh` is missing or not signed in; waiting does not change it.
    #[error("{0}")]
    Auth(String),
    #[error("unexpected GitHub response from {what}: {msg}")]
    Response { what: String, msg: String },
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Bus(#[from] hgl_ai_bus::BusError),
    #[error(transparent)]
    Proto(#[from] hgl_ai_proto::ProtoError),
    #[error(transparent)]
    Config(#[from] hgl_ai_config::ConfigError),
    /// The cause follows in the error chain.
    #[error("{what}")]
    Io {
        what: String,
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid(msg.into())
}

pub(crate) fn io_err(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> Error {
    let what = what.into();
    move |source| Error::Io { what, source }
}

/// First eight characters of a commit, as the protocol's examples show them.
pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

/// The gate settings of one repository, compiled.
#[derive(Debug, Clone)]
pub struct GateRules {
    /// A clean review receipt from another family is required (ROLE-11).
    pub receipt: bool,
    pub global_paths: Vec<String>,
    pub verdicts: Vec<String>,
    pub relax_paths: Vec<Regex>,
    pub relax_required: Vec<String>,
    pub relax_optional: Vec<Regex>,
    pub hygiene_files: Vec<Regex>,
    pub hygiene_lines: Vec<Regex>,
}

impl GateRules {
    pub fn relax_configured(&self) -> bool {
        !self.relax_paths.is_empty()
    }
}

/// What every command needs from the host configuration, checked once.
#[derive(Debug, Clone)]
pub struct Settings {
    /// This host's label: it names the holder of a landing lock.
    pub host: String,
    pub owner: String,
    pub repos: Vec<String>,
    pub branch_prefix: String,
    pub wrapper: String,
    pub review_bot: String,
    pub controller: Seat,
    pub coord_seat: Seat,
    pub coord_interval: Duration,
    pub hold_below: u64,
    global_paths: Vec<String>,
    hygiene_files: Vec<Regex>,
    hygiene_lines: Vec<Regex>,
    gates: BTreeMap<String, RepoGate>,
    pub pins: Vec<Pin>,
    pub pin_title: String,
    pub pin_branch: String,
    pub release: ReleaseWatch,
    pub release_timeout: Duration,
    pub tap: Tap,
    /// Runtime named in the first line of note bodies the controller sends (PROTO-11).
    pub controller_runtime: String,
    pub data_dir: PathBuf,
    /// First pause before a GitHub read that failed for a reason other than a rate limit or
    /// a missing object is tried again (doubled each time, three tries).
    pub api_retry: Duration,
}

fn regexes(what: &str, v: &[String]) -> Result<Vec<Regex>> {
    v.iter()
        .map(|p| Regex::new(p).map_err(|e| invalid(format!("{what} pattern {p:?}: {e}"))))
        .collect()
}

impl Settings {
    pub fn from_config(cfg: &Config) -> Result<Settings> {
        let g = &cfg.github;
        let dur = |what: &str, s: &str| {
            parse_duration(s).ok_or_else(|| invalid(format!("{what}: duration {s:?}")))
        };
        for (repo, rules) in &g.gate {
            regexes(
                &format!("github.gate.{repo}.relax_paths"),
                &rules.relax_paths,
            )?;
            regexes(
                &format!("github.gate.{repo}.relax_optional"),
                &rules.relax_optional,
            )?;
        }
        let controller_runtime = cfg
            .seat(&g.controller)
            .map(|s| s.binding.runtime.to_string())
            .unwrap_or_else(|| "claude".to_string());
        Ok(Settings {
            host: cfg.host.clone(),
            owner: g.owner.clone(),
            repos: g.repos.clone(),
            branch_prefix: g.branch_prefix.clone(),
            wrapper: g.wrapper.clone(),
            review_bot: g.review_bot.clone(),
            controller: Seat::new(g.controller.clone())?,
            coord_seat: Seat::new(g.coord_seat.clone())?,
            coord_interval: dur("github.coord_interval", &g.coord_interval)?,
            hold_below: g.hold_below,
            global_paths: g.global_paths.clone(),
            hygiene_files: regexes("github.hygiene_files", &g.hygiene_files)?,
            hygiene_lines: regexes("github.hygiene_lines", &g.hygiene_lines)?,
            gates: g.gate.clone(),
            pins: g.pins.clone(),
            pin_title: g.pin_title.clone(),
            pin_branch: g.pin_branch.clone(),
            release: g.release.clone(),
            release_timeout: dur("github.release.timeout", &g.release.timeout)?,
            tap: g.tap.clone(),
            controller_runtime,
            data_dir: cfg.data_dir()?,
            api_retry: Duration::from_secs(2),
        })
    }

    /// The compiled gate rules of `repo`.
    pub fn gate_rules(&self, repo: &str) -> GateRules {
        let r = self.gates.get(repo).cloned().unwrap_or_default();
        let mut global_paths = self.global_paths.clone();
        global_paths.extend(r.global_paths.iter().cloned());
        GateRules {
            receipt: r.review_receipt,
            global_paths,
            verdicts: r.verdicts.clone(),
            // Checked in `from_config`.
            relax_paths: regexes("relax_paths", &r.relax_paths).unwrap_or_default(),
            relax_required: r.relax_required.clone(),
            relax_optional: regexes("relax_optional", &r.relax_optional).unwrap_or_default(),
            hygiene_files: self.hygiene_files.clone(),
            hygiene_lines: self.hygiene_lines.clone(),
        }
    }

    /// `owner/name`
    pub fn full(&self, repo: &str) -> String {
        format!("{}/{}", self.owner, repo)
    }

    /// Repositories whose merges are followed by a pin update of the wrapper (ROLE-12).
    pub fn pin_sources(&self) -> Vec<&str> {
        self.pins.iter().filter_map(|p| p.repo.as_deref()).collect()
    }
}

#[cfg(test)]
mod live_tests;

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    /// Settings from the default configuration of a host named `test`.
    pub fn settings() -> Settings {
        let mut cfg = Config::new("test");
        cfg.paths.data_dir = "/nonexistent/hgl-ai-test".into();
        let mut s = Settings::from_config(&cfg).unwrap();
        s.api_retry = Duration::from_millis(5);
        s
    }

    /// `s` with the review receipt (ROLE-11) turned off for `repos`, as `[github.gate.<repo>]
    /// review_receipt = false` does.
    pub fn without_receipts(mut s: Settings, repos: &[&str]) -> Settings {
        for r in repos {
            s.gates.entry(r.to_string()).or_default().review_receipt = false;
        }
        s
    }

    pub fn fixture(name: &str) -> String {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    pub fn json(name: &str) -> serde_json::Value {
        serde_json::from_str(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_from_defaults() {
        let s = testutil::settings();
        assert_eq!(s.controller.as_str(), "claude");
        assert_eq!(s.coord_interval, Duration::from_secs(300));
        let g = s.gate_rules("hgl-compiler");
        assert!(g.global_paths.iter().any(|p| p == "spec/hgl_spec"));
        assert!(g.global_paths.iter().any(|p| p == ".github/"));
        assert_eq!(g.verdicts, ["linux", "mac", "windows"]);
        assert!(g.relax_configured());
        let other = s.gate_rules("hgl_spec");
        assert!(other.verdicts.is_empty() && !other.relax_configured());
        assert!(g.receipt && other.receipt, "required by default (ROLE-11)");
        assert_eq!(s.pin_sources(), ["hgl-compiler"]);
        assert_eq!(short("0123456789abcdef"), "01234567");
    }

    #[test]
    fn bad_patterns_fail() {
        let mut cfg = Config::new("test");
        cfg.github.hygiene_lines = vec!["(".into()];
        assert!(Settings::from_config(&cfg).is_err());
        let mut cfg = Config::new("test");
        cfg.github.controller = "Not A Seat".into();
        assert!(Settings::from_config(&cfg).is_err());
    }
}
