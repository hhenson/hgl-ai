//! Recipes (BUILD-1, BUILD-2): the steps that build and test a repository, as TOML.
//!
//! ```toml
//! repo = "hgl-ai"
//! env = { CARGO_TERM_COLOR = "never" }     # every step
//!
//! [[step]]
//! name = "test"
//! run = "cargo test --workspace --locked --message-format=json"
//! # or per platform: run.default = "...", run.windows = "..."
//! cwd = "."                                 # relative to the checkout
//! env = { RUST_BACKTRACE = "0" }
//! platforms = ["linux", "macos", "windows"] # default: all
//! memcap_gb = 32                            # default 16; no cap on a Windows remote
//! time = "40m"                              # default 60m
//! parser = "cargo"                          # default lines
//! needs = ["sdk:hgraph", "tool:cmake", "step:build"]
//! artifacts = ["$HGL_AI_BUILD_DIR/ctest.xml"]
//! patterns = { error = ['^ERROR: (?P<message>.*)'], warning = [] }   # parser `lines`
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use hgl_ai_proto::build::Platform;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RecipeError {
    #[error("recipe: {0}")]
    Parse(String),
    #[error("recipe: {0}")]
    Invalid(String),
}

pub const DEFAULT_MEMCAP_GB: u64 = 16;
pub const DEFAULT_TIME: &str = "60m";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub repo: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(rename = "step")]
    pub steps: Vec<Step>,
}

/// A step's command: one for every platform, or one per platform with `default` for
/// the others.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Run {
    One(String),
    PerPlatform(BTreeMap<String, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ParserKind {
    Cargo,
    Gcc,
    Clang,
    Msvc,
    Ctest,
    Catch2,
    Junit,
    HglTest,
    #[default]
    Lines,
}

/// The `lines` parser's patterns; named groups `file`, `line`, `col`, `code`, `message`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patterns {
    #[serde(default)]
    pub error: Vec<String>,
    #[serde(default)]
    pub warning: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    pub run: Run,
    #[serde(default = "dot")]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<Platform>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memcap_gb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(default)]
    pub parser: ParserKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patterns: Option<Patterns>,
}

fn dot() -> String {
    ".".into()
}

/// What a step needs before it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    /// `sdk:NAME`: a prefix configured on the builder (`[builder.sdk]`).
    Sdk(String),
    /// `tool:NAME`: a command on the builder's `PATH`.
    Tool(String),
    /// `step:NAME` (or a bare name): an earlier step that must pass when it runs.
    Step(String),
}

impl Need {
    pub fn parse(s: &str) -> Result<Need, RecipeError> {
        let ok = |n: &str| {
            !n.is_empty()
                && n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_.+-".contains(c))
        };
        let (kind, name) = s.split_once(':').unwrap_or(("step", s));
        if !ok(name) {
            return Err(RecipeError::Invalid(format!("need {s:?}")));
        }
        match kind {
            "sdk" => Ok(Need::Sdk(name.into())),
            "tool" => Ok(Need::Tool(name.into())),
            "step" => Ok(Need::Step(name.into())),
            _ => Err(RecipeError::Invalid(format!(
                "need {s:?}: use sdk:, tool: or step:"
            ))),
        }
    }
}

impl std::fmt::Display for Need {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Need::Sdk(n) => write!(f, "sdk:{n}"),
            Need::Tool(n) => write!(f, "tool:{n}"),
            Need::Step(n) => write!(f, "step:{n}"),
        }
    }
}

/// The environment variable an SDK's prefix is given in: `HGL_SDK_<NAME>`.
pub fn sdk_var(name: &str) -> String {
    format!(
        "HGL_SDK_{}",
        name.to_ascii_uppercase().replace(['-', '.', '+'], "_")
    )
}

fn name_ok(n: &str) -> bool {
    let mut c = n.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_lowercase() || f.is_ascii_digit())
        && n.len() <= 40
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c))
}

fn env_name_ok(n: &str) -> bool {
    !n.is_empty()
        && !n.starts_with(|c: char| c.is_ascii_digit())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Step {
    /// The command on `p`, if the step has one there.
    pub fn run_for(&self, p: Platform) -> Option<&str> {
        match &self.run {
            Run::One(s) => Some(s),
            Run::PerPlatform(m) => m.get(p.as_str()).or_else(|| m.get("default")),
        }
        .map(String::as_str)
    }

    /// Runs on `p`: listed (or no list) and a command for it.
    pub fn applies(&self, p: Platform) -> bool {
        self.platforms.as_ref().is_none_or(|ps| ps.contains(&p)) && self.run_for(p).is_some()
    }

    pub fn memcap_bytes(&self) -> u64 {
        self.memcap_gb.unwrap_or(DEFAULT_MEMCAP_GB) << 30
    }

    pub fn time_limit(&self) -> Duration {
        let t = self.time.as_deref().unwrap_or(DEFAULT_TIME);
        hgl_ai_config::parse_duration(t).expect("validated")
    }

    pub fn needs(&self) -> Vec<Need> {
        self.needs
            .iter()
            .map(|n| Need::parse(n).expect("validated"))
            .collect()
    }
}

impl Recipe {
    pub fn parse(text: &str) -> Result<Recipe, RecipeError> {
        let r: Recipe = toml::from_str(text).map_err(|e| RecipeError::Parse(e.to_string()))?;
        r.validate()?;
        Ok(r)
    }

    pub fn validate(&self) -> Result<(), RecipeError> {
        let bad = |m: String| Err(RecipeError::Invalid(m));
        if self.repo.is_empty()
            || !self
                .repo
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
            || self.repo.starts_with('.')
        {
            return bad(format!("repo {:?}", self.repo));
        }
        if self.steps.is_empty() {
            return bad("no steps".into());
        }
        for k in self.env.keys() {
            if !env_name_ok(k) {
                return bad(format!("env name {k:?}"));
            }
        }
        let mut seen = BTreeSet::new();
        for s in &self.steps {
            let at = |m: &str| RecipeError::Invalid(format!("step {:?}: {m}", s.name));
            if !name_ok(&s.name) {
                return Err(at("name: use [a-z0-9_.-], at most 40"));
            }
            match &s.run {
                Run::One(r) if r.trim().is_empty() => return Err(at("empty run")),
                Run::One(_) => {}
                Run::PerPlatform(m) => {
                    if m.is_empty() {
                        return Err(at("empty run table"));
                    }
                    for (k, v) in m {
                        if k != "default" && k.parse::<Platform>().is_err() {
                            return Err(at(&format!(
                                "run.{k}: use a platform (linux, macos, windows) or default"
                            )));
                        }
                        if v.trim().is_empty() {
                            return Err(at(&format!("empty run.{k}")));
                        }
                    }
                }
            }
            if s.cwd.starts_with('/')
                || s.cwd.contains(':')
                || s.cwd.split(['/', '\\']).any(|p| p == "..")
            {
                return Err(at("cwd must stay inside the checkout"));
            }
            for k in s.env.keys() {
                if !env_name_ok(k) {
                    return Err(at(&format!("env name {k:?}")));
                }
            }
            if s.memcap_gb == Some(0) {
                return Err(at("memcap_gb must be at least 1"));
            }
            if let Some(t) = &s.time
                && hgl_ai_config::parse_duration(t).is_none_or(|d| d.is_zero())
            {
                return Err(at(&format!("time {t:?}: use 90s, 40m, 2h")));
            }
            for n in &s.needs {
                if let Need::Step(dep) = Need::parse(n)?
                    && !seen.contains(&dep)
                {
                    return Err(at(&format!("needs step {dep}, which is not before it")));
                }
            }
            for a in &s.artifacts {
                if a.trim().is_empty()
                    || a.contains(['\'', '"', '`', '\n', ';', '|', '&', '<', '>'])
                    || a.split(['/', '\\']).any(|p| p == "..")
                {
                    return Err(at(&format!("artifact {a:?}")));
                }
            }
            if let Some(p) = &s.patterns {
                for re in p.error.iter().chain(&p.warning) {
                    Regex::new(re).map_err(|e| at(&format!("pattern {re:?}: {e}")))?;
                }
            }
            if !seen.insert(s.name.clone()) {
                return Err(at("appears twice"));
            }
        }
        Ok(())
    }

    /// The steps that run on `p`, in order; `only` selects some of them by name.
    pub fn plan(&self, p: Platform, only: Option<&[String]>) -> Result<Vec<&Step>, RecipeError> {
        if let Some(only) = only {
            for n in only {
                match self.steps.iter().find(|s| &s.name == n) {
                    None => {
                        return Err(RecipeError::Invalid(format!(
                            "no step {n:?} in the recipe of {}",
                            self.repo
                        )));
                    }
                    Some(s) if !s.applies(p) => {
                        return Err(RecipeError::Invalid(format!(
                            "step {n:?} does not run on {p}"
                        )));
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(self
            .steps
            .iter()
            .filter(|s| s.applies(p))
            .filter(|s| only.is_none_or(|o| o.contains(&s.name)))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: &str = r#"
repo = "demo"
env = { A = "1" }

[[step]]
name = "build"
run = "make"
parser = "gcc"

[[step]]
name = "test"
run.default = "make test"
run.windows = "nmake test"
platforms = ["linux", "windows"]
needs = ["step:build", "sdk:hgraph", "tool:cmake"]
memcap_gb = 32
time = "40m"
"#;

    #[test]
    fn parse_and_plan() {
        let r = Recipe::parse(R).unwrap();
        assert_eq!(r.steps[0].parser, ParserKind::Gcc);
        assert_eq!(r.steps[1].parser, ParserKind::Lines);
        assert_eq!(r.steps[1].run_for(Platform::Windows), Some("nmake test"));
        assert_eq!(r.steps[1].run_for(Platform::Linux), Some("make test"));
        assert_eq!(r.steps[1].memcap_bytes(), 32 << 30);
        assert_eq!(r.steps[0].memcap_bytes(), 16 << 30);
        assert_eq!(r.steps[1].time_limit(), Duration::from_secs(2400));
        let names = |v: Vec<&Step>| v.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(r.plan(Platform::Macos, None).unwrap()), ["build"]);
        assert_eq!(
            names(r.plan(Platform::Linux, None).unwrap()),
            ["build", "test"]
        );
        assert_eq!(
            names(r.plan(Platform::Linux, Some(&["test".into()])).unwrap()),
            ["test"]
        );
        assert!(r.plan(Platform::Linux, Some(&["nope".into()])).is_err());
        assert!(r.plan(Platform::Macos, Some(&["test".into()])).is_err());
        assert_eq!(
            r.steps[1].needs(),
            [
                Need::Step("build".into()),
                Need::Sdk("hgraph".into()),
                Need::Tool("cmake".into())
            ]
        );
        assert_eq!(sdk_var("hgraph-deps"), "HGL_SDK_HGRAPH_DEPS");
    }

    #[test]
    fn rejects() {
        let bad = [
            ("repo = \"x\"\nstep = []\n", "no steps"),
            (
                "repo = \"x\"\n[[step]]\nname = \"A\"\nrun = \"x\"\n",
                "name",
            ),
            (
                "repo = \"x\"\n[[step]]\nname = \"a\"\nrun = \"x\"\nneeds = [\"b\"]\n",
                "not before",
            ),
            (
                "repo = \"x\"\n[[step]]\nname = \"a\"\nrun = \"x\"\ncwd = \"../up\"\n",
                "inside",
            ),
            (
                "repo = \"x\"\n[[step]]\nname = \"a\"\nrun = \"x\"\ntime = \"soon\"\n",
                "time",
            ),
            (
                "repo = \"x\"\n[[step]]\nname = \"a\"\nrun.beos = \"x\"\n",
                "platform",
            ),
            (
                "repo = \"x\"\n[[step]]\nname = \"a\"\nrun = \"x\"\nparser = \"bogus\"\n",
                "",
            ),
            (
                "repo = \"x\"\n[[step]]\nname = \"a\"\nrun = \"x\"\npatterns = { error = [\"(\"] }\n",
                "pattern",
            ),
            (
                "repo = \"x\"\n[[step]]\nname = \"a\"\nrun = \"x\"\nartifacts = [\"a;b\"]\n",
                "artifact",
            ),
            (
                "repo = \"x\"\n[[step]]\nname = \"a\"\nrun = \"x\"\n[[step]]\nname = \"a\"\nrun = \"y\"\n",
                "twice",
            ),
        ];
        for (text, want) in bad {
            let e = Recipe::parse(text).unwrap_err().to_string();
            assert!(e.contains(want), "{text}: {e}");
        }
    }
}
