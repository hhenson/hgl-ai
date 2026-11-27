//! Host configuration (`docs/design.md` §3): one `config.toml` per host.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hgl_ai_proto::{Family, Role, Runtime, Seat, Tier};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("no home directory")]
    NoHome,
}

pub type Result<T> = std::result::Result<T, ConfigError>;

/// The whole file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// A label for this host; never a network name or address.
    pub host: String,
    #[serde(default)]
    pub hub: Hub,
    #[serde(default)]
    pub paths: Paths,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub github: Github,
    #[serde(default = "default_models")]
    pub models: BTreeMap<Family, TierModels>,
    #[serde(default, rename = "seat")]
    pub seats: Vec<SeatConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Server>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hub {
    pub url: String,
    pub token_file: String,
}

impl Default for Hub {
    fn default() -> Self {
        Hub {
            url: "nats://127.0.0.1:4222".into(),
            token_file: "~/.config/hgl-ai/hub.token".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    pub work_root: String,
    pub data_dir: String,
}

impl Default for Paths {
    fn default() -> Self {
        Paths {
            work_root: "~/hgl-ai/work".into(),
            data_dir: "~/.local/share/hgl-ai".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub memcap_gb: u64,
    pub min_free_gb: u64,
    /// A duration such as `3d`, `12h`, `90m`, `30s`.
    pub stale_after: String,
    pub turn_timeout: String,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            memcap_gb: 64,
            min_free_gb: 200,
            stale_after: "3d".into(),
            turn_timeout: "2h".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Github {
    pub owner: String,
    pub repos: Vec<String>,
    pub branch_prefix: String,
    pub wrapper: String,
    /// Clone URL template; `{owner}` and `{repo}` are replaced.
    #[serde(default = "default_clone_url")]
    pub clone_url: String,
}

pub fn default_clone_url() -> String {
    "https://github.com/{owner}/{repo}.git".into()
}

impl Github {
    /// The clone URL of `repo` (`clone_url` with `{owner}` and `{repo}` filled in).
    pub fn clone_url_for(&self, repo: &str) -> String {
        self.clone_url.replace("{owner}", &self.owner).replace("{repo}", repo)
    }
}

impl Default for Github {
    fn default() -> Self {
        Github {
            owner: "hhenson".into(),
            repos: [
                "hgl-compiler",
                "hgl_spec",
                "hgl_core",
                "hgl-project",
                "homebrew-hgraph",
            ]
            .map(String::from)
            .to_vec(),
            branch_prefix: "codex/".into(),
            wrapper: "hgl-project".into(),
            clone_url: default_clone_url(),
        }
    }
}

/// Model slugs of one family, by tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierModels {
    pub frontier: String,
    pub balanced: String,
    pub fast: String,
}

impl TierModels {
    pub fn get(&self, t: Tier) -> &str {
        match t {
            Tier::Frontier => &self.frontier,
            Tier::Balanced => &self.balanced,
            Tier::Fast => &self.fast,
        }
    }
}

pub fn default_models() -> BTreeMap<Family, TierModels> {
    let m = |f: &str, b: &str, s: &str| TierModels {
        frontier: f.into(),
        balanced: b.into(),
        fast: s.into(),
    };
    BTreeMap::from([
        (
            Family::Claude,
            m("claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"),
        ),
        (Family::Gpt, m("gpt-6.1-sol", "gpt-6.1-sol", "gpt-6-luna")),
        (
            Family::Gemini,
            m("gemini-4-carbon", "gemini-4-argon", "gemini-3.8-flash"),
        ),
    ])
}

/// A runtime and model choice: either a pinned `model` or a `family` + `tier`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub runtime: Runtime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<Family>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeatConfig {
    pub name: Seat,
    pub role: Role,
    #[serde(flatten)]
    pub binding: Binding,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<Binding>,
    /// Per-seat memory cap; defaults to `limits.memcap_gb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memcap_gb: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub listen: String,
    pub port: u16,
    pub store_dir: String,
    pub nats_version: String,
}

impl Default for Server {
    fn default() -> Self {
        Server {
            listen: "127.0.0.1".into(),
            port: 4222,
            store_dir: "~/.local/share/hgl-ai/nats".into(),
            nats_version: "2.15.3".into(),
        }
    }
}

/// A binding resolved to one model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub runtime: Runtime,
    pub family: Family,
    pub model: String,
    pub effort: Option<String>,
}

/// The family a runtime uses when the binding names none.
pub fn native_family(r: Runtime) -> Family {
    match r {
        Runtime::Claude => Family::Claude,
        Runtime::Codex => Family::Gpt,
        Runtime::Agy => Family::Gemini,
    }
}

/// Families a runtime can drive.
pub fn runtime_supports(r: Runtime, f: Family) -> bool {
    match r {
        Runtime::Claude => f == Family::Claude,
        Runtime::Codex => f == Family::Gpt,
        Runtime::Agy => matches!(f, Family::Gemini | Family::Claude | Family::Gpt),
    }
}

impl Config {
    /// A fresh configuration for `host` with defaults everywhere else.
    pub fn new(host: impl Into<String>) -> Self {
        Config {
            host: host.into(),
            hub: Hub::default(),
            paths: Paths::default(),
            limits: Limits::default(),
            github: Github::default(),
            models: default_models(),
            seats: Vec::new(),
            server: None,
        }
    }

    /// Default location: `$XDG_CONFIG_HOME/hgl-ai/config.toml`, else `~/.config/hgl-ai/config.toml`.
    /// `HGL_AI_CONFIG` overrides it.
    pub fn default_path() -> Result<PathBuf> {
        if let Some(p) = std::env::var_os("HGL_AI_CONFIG") {
            return Ok(PathBuf::from(p));
        }
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(x) if !x.is_empty() => PathBuf::from(x),
            _ => home()?.join(".config"),
        };
        Ok(base.join("hgl-ai").join("config.toml"))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.into(),
            source,
        })?;
        let cfg: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.into(),
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Write atomically (temporary file, then rename), creating the directory.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let text = toml::to_string_pretty(self).map_err(|e| ConfigError::Invalid(e.to_string()))?;
        let w = |source| ConfigError::Write {
            path: path.into(),
            source,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(w)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(w)?;
        std::fs::rename(&tmp, path).map_err(w)
    }

    pub fn validate(&self) -> Result<()> {
        let label_ok = !self.host.is_empty()
            && self.host.len() <= 32
            && self
                .host
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !self.host.starts_with('-');
        if !label_ok {
            return Err(ConfigError::Invalid(format!(
                "host label {:?}: use [a-z0-9-], at most 32",
                self.host
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for s in &self.seats {
            if s.name.is_broadcast() {
                return Err(ConfigError::Invalid("a seat cannot be named `all`".into()));
            }
            if !seen.insert(s.name.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "seat {} appears twice",
                    s.name
                )));
            }
            for b in std::iter::once(&s.binding).chain(&s.fallback) {
                self.resolve(b)
                    .map_err(|e| ConfigError::Invalid(format!("seat {}: {e}", s.name)))?;
            }
        }
        for d in [&self.limits.stale_after, &self.limits.turn_timeout] {
            parse_duration(d).ok_or_else(|| ConfigError::Invalid(format!("duration {d:?}")))?;
        }
        Ok(())
    }

    /// Resolve a binding to one model (docs/design.md §3).
    pub fn resolve(&self, b: &Binding) -> std::result::Result<Resolved, String> {
        let (family, model) = match (&b.model, b.family, b.tier) {
            (Some(m), f, None) => (
                f.or_else(|| self.family_of(m))
                    .unwrap_or(native_family(b.runtime)),
                m.clone(),
            ),
            (None, f, Some(t)) => {
                let f = f.unwrap_or(native_family(b.runtime));
                let tm = self
                    .models
                    .get(&f)
                    .ok_or_else(|| format!("no [models.{f}] table"))?;
                (f, tm.get(t).to_string())
            }
            (Some(_), _, Some(_)) => return Err("give `model` or `tier`, not both".into()),
            (None, _, None) => return Err("give `model` or `tier`".into()),
        };
        if !runtime_supports(b.runtime, family) {
            return Err(format!("runtime {} cannot run a {family} model", b.runtime));
        }
        Ok(Resolved {
            runtime: b.runtime,
            family,
            model,
            effort: b.effort.clone(),
        })
    }

    /// The family whose `[models]` table names this slug, if any.
    pub fn family_of(&self, slug: &str) -> Option<Family> {
        self.models
            .iter()
            .find(|(_, t)| {
                [&t.frontier, &t.balanced, &t.fast]
                    .iter()
                    .any(|m| slug == m.as_str() || slug.starts_with(&format!("{m}-")))
            })
            .map(|(f, _)| *f)
    }

    pub fn seat(&self, name: &str) -> Option<&SeatConfig> {
        self.seats.iter().find(|s| s.name.as_str() == name)
    }

    pub fn work_root(&self) -> Result<PathBuf> {
        expand(&self.paths.work_root)
    }
    pub fn data_dir(&self) -> Result<PathBuf> {
        expand(&self.paths.data_dir)
    }
    pub fn token_file(&self) -> Result<PathBuf> {
        expand(&self.hub.token_file)
    }
    /// `<data_dir>/seats/<seat>.lock`: exclusively `flock`ed by whoever serves the seat,
    /// the bridge or an interactive session, never both.
    pub fn seat_lock_path(&self, seat: &Seat) -> Result<PathBuf> {
        Ok(self.data_dir()?.join("seats").join(format!("{seat}.lock")))
    }
    pub fn stale_after(&self) -> Duration {
        parse_duration(&self.limits.stale_after).expect("validated")
    }
    pub fn turn_timeout(&self) -> Duration {
        parse_duration(&self.limits.turn_timeout).expect("validated")
    }
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or(ConfigError::NoHome)
}

/// Expand a leading `~/` (or a lone `~`) to `$HOME`.
pub fn expand(p: &str) -> Result<PathBuf> {
    if p == "~" {
        return home();
    }
    match p.strip_prefix("~/") {
        Some(rest) => Ok(home()?.join(rest)),
        None => Ok(PathBuf::from(p)),
    }
}

/// `30s`, `90m`, `2h`, `3d`; a bare number is seconds.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().ok()?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return None,
    };
    Some(Duration::from_secs(n.checked_mul(mult)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
host = "build-1"

[hub]
url = "nats://hub:4222"
token_file = "~/.config/hgl-ai/hub.token"

[[seat]]
name = "w1"
role = "implementor"
runtime = "codex"
tier = "frontier"
effort = "high"
fallback = [{ runtime = "agy", family = "claude", tier = "frontier" }]

[[seat]]
name = "r1"
role = "reviewer"
runtime = "agy"
model = "claude-opus-5-5-high"
"#;

    #[test]
    fn parse_and_resolve() {
        let c: Config = toml::from_str(SAMPLE).unwrap();
        c.validate().unwrap();
        assert_eq!(c.paths, Paths::default());
        let w1 = c.seat("w1").unwrap();
        let r = c.resolve(&w1.binding).unwrap();
        assert_eq!((r.family, r.model.as_str()), (Family::Gpt, "gpt-6.1-sol"));
        let f = c.resolve(&w1.fallback[0]).unwrap();
        assert_eq!(
            (f.runtime, f.family, f.model.as_str()),
            (Runtime::Agy, Family::Claude, "claude-opus-5-5")
        );
        let r1 = c.resolve(&c.seat("r1").unwrap().binding).unwrap();
        assert_eq!(
            r1.family,
            Family::Claude,
            "slug with an effort suffix maps to its family"
        );
    }

    #[test]
    fn rejects_bad_bindings() {
        let mut c = Config::new("h");
        let b = |runtime, family, tier: Option<Tier>, model: Option<&str>| Binding {
            runtime,
            family,
            tier,
            model: model.map(String::from),
            effort: None,
        };
        assert!(
            c.resolve(&b(
                Runtime::Codex,
                Some(Family::Claude),
                Some(Tier::Fast),
                None
            ))
            .is_err()
        );
        assert!(
            c.resolve(&b(Runtime::Claude, None, Some(Tier::Fast), Some("x")))
                .is_err()
        );
        assert!(c.resolve(&b(Runtime::Claude, None, None, None)).is_err());
        c.host = "Bad Host".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn unknown_fields_fail() {
        assert!(toml::from_str::<Config>("host = \"h\"\nbogus = 1\n").is_err());
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x/config.toml");
        let c: Config = toml::from_str(SAMPLE).unwrap();
        c.save(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), c);
    }

    #[test]
    fn clone_url_default_and_template() {
        let g: Github = toml::from_str("owner = \"hhenson\"\nrepos = []\nbranch_prefix = \"codex/\"\nwrapper = \"w\"\n").unwrap();
        assert_eq!(g.clone_url_for("hgl_spec"), "https://github.com/hhenson/hgl_spec.git");
        let g = Github { clone_url: "/srv/git/{repo}.git".into(), ..Github::default() };
        assert_eq!(g.clone_url_for("x"), "/srv/git/x.git");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("3d"), Some(Duration::from_secs(259200)));
        assert_eq!(parse_duration("90m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("2w"), None);
        assert_eq!(parse_duration(""), None);
    }
}
