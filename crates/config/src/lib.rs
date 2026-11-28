//! Host configuration (`docs/design.md` §3): one `config.toml` per host.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hgl_ai_proto::{Family, Role, Runtime, Seat, Tier};
use serde::{Deserialize, Serialize};

pub mod builder;
pub mod console;
pub mod github;
pub mod slack;
pub use builder::{Builder, Remote};
pub use console::{
    AppleOAuth, Console, GithubOAuth, GoogleOAuth, Identity, Ids, LinkedEntry, Member, Notify,
    Person, SmtpTls, Unresolved, User,
};
pub use github::{Pin, ReleaseWatch, RepoGate, Tap};

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

/// Overrides `hub.token_file` for one process: a service that has a hub user of its own.
pub const CREDENTIAL_ENV: &str = "HGL_AI_HUB_CREDENTIAL";

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
    /// The owner's console (hub host only; docs/owner.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console: Option<Console>,
    /// The Slack Q&A adapter (server side only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack: Option<slack::Slack>,
    /// On a hub host whose server side runs as its own OS user (the client's
    /// configuration): that user, for `server status` and `doctor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_user: Option<ServerUser>,
    #[serde(default, skip_serializing_if = "Builder::is_default")]
    pub builder: Builder,
}

/// The OS user the hub, coordinator and console run as (docs/install.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerUser {
    pub name: String,
    /// Its home: the server side's configuration, credentials and data.
    pub home: String,
    /// The console's origin, when the server side runs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_url: Option<String>,
    /// Whether this system install includes the Slack adapter.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub slack: bool,
    /// The OS user this host's client (bridge, agents) runs as, when not the installer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_user: Option<String>,
}

/// Where the hub's services are installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Units {
    /// `systemd --user` units or launchd agents of the installing user.
    #[default]
    User,
    /// System units (`User=`) or launch daemons (`UserName`) of a server user.
    System,
}

impl Units {
    fn is_user(&self) -> bool {
        *self == Units::User
    }
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
    /// GiB (2^30 bytes) of one agent turn's command tree.
    pub memcap_gb: u64,
    /// Decimal GB (10^9 bytes) free under `work_root`, as `df -H` shows them; below it no new tree is made.
    pub min_free_gb: u64,
    /// A duration such as `3d`, `12h`, `90m`, `30s`.
    pub stale_after: String,
    pub turn_timeout: String,
    /// How long a seat stays on a fallback binding after a quota refusal before it tries
    /// its primary again (PROTO-17).
    #[serde(default = "default_failover_cooldown")]
    pub failover_cooldown: String,
}

pub fn default_failover_cooldown() -> String {
    "1h".into()
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            memcap_gb: 96,
            min_free_gb: 200,
            stale_after: "3d".into(),
            turn_timeout: "2h".into(),
            failover_cooldown: default_failover_cooldown(),
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
    // GitHub services and gates (crates/gh); see `github.rs` for each key.
    /// Login of the review bot, without a `[bot]` suffix.
    #[serde(
        default = "github::review_bot",
        skip_serializing_if = "github::is_review_bot"
    )]
    pub review_bot: String,
    /// The controller's seat (ROLE-2): it gets the bridge's and the coordinator's alerts
    /// and sends `landed` notes.
    #[serde(
        default = "github::controller",
        skip_serializing_if = "github::is_controller"
    )]
    pub controller: String,
    /// Sender seat of the coordinator's notes.
    #[serde(
        default = "github::coord_seat",
        skip_serializing_if = "github::is_coord_seat"
    )]
    pub coord_seat: String,
    #[serde(
        default = "github::coord_interval",
        skip_serializing_if = "github::is_coord_interval"
    )]
    pub coord_interval: String,
    /// GraphQL allowance at or below which the coordinator writes `hold`.
    #[serde(
        default = "github::hold_below",
        skip_serializing_if = "github::is_hold_below"
    )]
    pub hold_below: u64,
    /// Path prefixes where a move of `main` brings every pull request up to date first.
    #[serde(
        default = "github::global_paths",
        skip_serializing_if = "github::is_global_paths"
    )]
    pub global_paths: Vec<String>,
    /// Regular expressions over changed file names that block a merge.
    #[serde(
        default = "github::hygiene_files",
        skip_serializing_if = "github::is_hygiene_files"
    )]
    pub hygiene_files: Vec<String>,
    /// Regular expressions over added lines that block a merge.
    #[serde(
        default = "github::hygiene_lines",
        skip_serializing_if = "github::is_hygiene_lines"
    )]
    pub hygiene_lines: Vec<String>,
    #[serde(default = "github::gates", skip_serializing_if = "github::is_gates")]
    pub gate: BTreeMap<String, RepoGate>,
    #[serde(default = "github::pins", skip_serializing_if = "github::is_pins")]
    pub pins: Vec<Pin>,
    /// Title and branch (after `branch_prefix`) of a pin pull request; `{sha8}` is replaced.
    #[serde(
        default = "github::pin_title",
        skip_serializing_if = "github::is_pin_title"
    )]
    pub pin_title: String,
    #[serde(
        default = "github::pin_branch",
        skip_serializing_if = "github::is_pin_branch"
    )]
    pub pin_branch: String,
    #[serde(
        default = "github::release",
        skip_serializing_if = "github::is_release"
    )]
    pub release: ReleaseWatch,
    #[serde(default = "github::tap", skip_serializing_if = "github::is_tap")]
    pub tap: Tap,
    /// Clone URL template; `{owner}` and `{repo}` are replaced.
    #[serde(default = "default_clone_url")]
    pub clone_url: String,
    /// A GitHub token for `gh` (`GH_TOKEN`), mode 0600: the coordinator's own read-only
    /// token when it runs as the server user. Without it, `gh`'s own sign-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_file: Option<String>,
}

pub fn default_clone_url() -> String {
    "https://github.com/{owner}/{repo}.git".into()
}

impl Github {
    /// The clone URL of `repo` (`clone_url` with `{owner}` and `{repo}` filled in).
    pub fn clone_url_for(&self, repo: &str) -> String {
        self.clone_url
            .replace("{owner}", &self.owner)
            .replace("{repo}", repo)
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
            review_bot: github::review_bot(),
            controller: github::controller(),
            coord_seat: github::coord_seat(),
            coord_interval: github::coord_interval(),
            hold_below: github::hold_below(),
            global_paths: github::global_paths(),
            hygiene_files: github::hygiene_files(),
            hygiene_lines: github::hygiene_lines(),
            gate: github::gates(),
            pins: github::pins(),
            pin_title: github::pin_title(),
            pin_branch: github::pin_branch(),
            release: github::release(),
            tap: github::tap(),
            clone_url: default_clone_url(),
            token_file: None,
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
        (Family::Gpt, m("gpt-6.2-astra", "gpt-6.2-sol", "gpt-6-luna")),
        (
            Family::Gemini,
            m("gemini-4-neon", "gemini-4-hydrogen", "gemini-3.8-flash"),
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
    /// A controller served by the bridge, a headless turn per batch of notes (OWN-7);
    /// without it a controller is interactive (PROTO-6). Only for role `controller`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub headless: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub listen: String,
    pub port: u16,
    pub store_dir: String,
    pub nats_version: String,
    /// How clients authenticate. A configuration from before hub users is `token`.
    #[serde(default = "HubAuth::legacy")]
    pub auth: HubAuth,
    /// Where the hub's services are installed.
    #[serde(default, skip_serializing_if = "Units::is_user")]
    pub units: Units,
}

/// `users`: a hub user per client with its role's permissions (OWN-5); `token`: one
/// shared token, every client alike (older installs; `doctor` warns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HubAuth {
    Users,
    Token,
}

impl HubAuth {
    fn legacy() -> Self {
        HubAuth::Token
    }
}

impl Default for Server {
    fn default() -> Self {
        Server {
            listen: "127.0.0.1".into(),
            port: 4222,
            store_dir: "~/.local/share/hgl-ai/nats".into(),
            nats_version: "2.15.3".into(),
            auth: HubAuth::Users,
            units: Units::User,
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

/// Efforts a model slug may already end with (`gemini-4-carbon-high`).
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Antigravity names a model with its effort: `SLUG-EFFORT`, unless the slug has one.
pub fn agy_model(model: &str, effort: Option<&str>) -> String {
    match effort {
        Some(e) if !EFFORTS.iter().any(|x| model.ends_with(&format!("-{x}"))) => {
            format!("{model}-{e}")
        }
        _ => model.to_string(),
    }
}

/// Model and effort flags of a runtime (docs/design.md §4), shared by headless turns
/// and interactive sessions so the two never diverge.
pub fn model_args(r: &Resolved) -> Vec<String> {
    let e = r.effort.as_deref();
    let mut a: Vec<String> = match r.runtime {
        Runtime::Claude => vec!["--model".into(), r.model.clone()],
        Runtime::Codex => vec!["-m".into(), r.model.clone()],
        Runtime::Agy => vec!["--model".into(), agy_model(&r.model, e)],
    };
    if let Some(e) = e {
        match r.runtime {
            Runtime::Codex => a.extend(["-c".into(), format!("model_reasoning_effort={e}")]),
            Runtime::Claude | Runtime::Agy => a.extend(["--effort".into(), e.to_string()]),
        }
    }
    a
}

/// The Codex permission profile a turn runs under ([`codex_profile`]).
pub const CODEX_PROFILE: &str = "hgl_task";

/// The `-c` overrides that select [`CODEX_PROFILE`]: the whole disk readable, `writable`
/// (the task directory and the clone) writable, and the network on.
///
/// A named profile, not `sandbox_mode = "workspace-write"` with
/// `sandbox_workspace_write.writable_roots`: that legacy policy makes the `.git` of every
/// writable root read-only and, where `.git` is a `gitdir:` pointer (a worktree), its target
/// too. A task's `src/.git` points into the clone's `worktrees/`, so `index.lock` could not
/// be created even with the clone writable (hgl-ai#29, codex-cli 0.172.0). A profile's
/// paths are taken as given. `exec resume` honours it too, also for a session started
/// under the legacy policy. Shared by the bridge's turns and `doctor`'s probe of them.
pub fn codex_profile(writable: &[PathBuf]) -> Vec<String> {
    let mut fs = vec![format!("{}=\"read\"", toml_str(":root"))];
    fs.extend(
        writable
            .iter()
            .map(|p| format!("{}=\"write\"", toml_str(&p.to_string_lossy()))),
    );
    vec![
        format!("default_permissions={}", toml_str(CODEX_PROFILE)),
        format!(
            "permissions.{CODEX_PROFILE}.filesystem={{{}}}",
            fs.join(",")
        ),
        format!("permissions.{CODEX_PROFILE}.network.enabled=true"),
    ]
}

/// A TOML basic string.
fn toml_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if c.is_control() => o.push_str(&format!("\\u{:04X}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
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
            console: None,
            slack: None,
            server_user: None,
            builder: Builder::default(),
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
        if self.host == hgl_ai_proto::BROADCAST {
            // The bridge's notes about the host are sent from its label.
            return Err(ConfigError::Invalid(
                "host label `all` is the broadcast address; choose another".into(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for s in &self.seats {
            if s.name.is_reserved() {
                return Err(ConfigError::Invalid(format!(
                    "a seat cannot be named `{}`: `all`, `owner` and `console` are reserved",
                    s.name
                )));
            }
            if hgl_ai_proto::build::is_build_address(&s.name) {
                return Err(ConfigError::Invalid(format!(
                    "a seat cannot be named `{}`: `builder-*` and `r-*` are builders' and builds' addresses",
                    s.name
                )));
            }
            if s.headless && s.role != Role::Controller {
                return Err(ConfigError::Invalid(format!(
                    "seat {}: `headless` is for a controller; the bridge serves every other seat",
                    s.name
                )));
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
        self.controller()?;
        github::validate_pins(&self.github.pins).map_err(ConfigError::Invalid)?;
        if let Some(c) = &self.console {
            c.validate().map_err(ConfigError::Invalid)?;
        }
        if let Some(s) = &self.slack {
            s.validate().map_err(ConfigError::Invalid)?;
        }
        builder::validate(&self.builder, &self.host).map_err(ConfigError::Invalid)?;
        for d in [
            &self.limits.stale_after,
            &self.limits.turn_timeout,
            &self.limits.failover_cooldown,
            &self.github.coord_interval,
            &self.github.release.timeout,
        ] {
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

    /// The controller's seat, `[github] controller` (default `controller`).
    pub fn controller(&self) -> Result<Seat> {
        let invalid = |why: String| ConfigError::Invalid(format!("github.controller: {why}"));
        let s = Seat::new(self.github.controller.clone()).map_err(|e| invalid(e.to_string()))?;
        if s.is_reserved() {
            return Err(invalid(
                "not `all`, `owner` or `console`; name one seat".into(),
            ));
        }
        Ok(s)
    }

    pub fn work_root(&self) -> Result<PathBuf> {
        expand(&self.paths.work_root)
    }
    pub fn data_dir(&self) -> Result<PathBuf> {
        expand(&self.paths.data_dir)
    }
    /// The hub credential this process connects with: `HGL_AI_HUB_CREDENTIAL` when set (the
    /// coordinator and console services each get their own hub user), else `hub.token_file`.
    pub fn token_file(&self) -> Result<PathBuf> {
        match std::env::var_os(CREDENTIAL_ENV) {
            Some(p) if !p.is_empty() => expand(&p.to_string_lossy()),
            _ => expand(&self.hub.token_file),
        }
    }
    /// `<data_dir>/console`: the console's passkeys and cookie key.
    pub fn console_dir(&self) -> Result<PathBuf> {
        Ok(self.data_dir()?.join("console"))
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
    pub fn failover_cooldown(&self) -> Duration {
        parse_duration(&self.limits.failover_cooldown).expect("validated")
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
        assert_eq!((r.family, r.model.as_str()), (Family::Gpt, "gpt-6.2-astra"));
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
        // The broadcast address is no host label: the bridge's host notes come from it.
        assert!(Config::new("all").validate().is_err());
        assert!(Config::new("lab1").validate().is_ok());
    }

    #[test]
    fn the_failover_cool_down() {
        let c = Config::new("h");
        assert_eq!(c.failover_cooldown(), Duration::from_secs(3600));
        // A `[limits]` table written before the key existed still loads.
        let t = "host = \"h\"\n[limits]\nmemcap_gb = 8\nmin_free_gb = 10\nstale_after = \"3d\"\nturn_timeout = \"2h\"\n";
        let c: Config = toml::from_str(t).unwrap();
        assert_eq!(c.limits.failover_cooldown, "1h");
        let mut bad = Config::new("h");
        bad.limits.failover_cooldown = "soon".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn the_controller_seat() {
        let mut c = Config::new("h");
        assert_eq!(
            c.controller().unwrap().as_str(),
            "controller",
            "the default"
        );
        let base = "host = \"h\"\n[github]\nowner = \"o\"\nrepos = []\nbranch_prefix = \"codex/\"\nwrapper = \"w\"\n";
        let loaded: Config = toml::from_str(base).unwrap();
        assert_eq!(loaded.controller().unwrap().as_str(), "controller");
        let explicit: Config = toml::from_str(&format!("{base}controller = \"claude\"\n")).unwrap();
        assert_eq!(explicit.controller().unwrap().as_str(), "claude");
        c.github.controller = "boss".into();
        assert_eq!(c.controller().unwrap().as_str(), "boss");
        assert!(c.validate().is_ok());
        for bad in ["all", "owner", "console", "Not A Seat"] {
            c.github.controller = bad.into();
            assert!(c.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn headless_is_for_a_controller() {
        let t = "host = \"h\"\n[[seat]]\nname = \"w1\"\nrole = \"implementor\"\nruntime = \"codex\"\ntier = \"fast\"\nheadless = true\n";
        let c: Config = toml::from_str(t).unwrap();
        assert!(c.validate().is_err());
        let c: Config = toml::from_str(&t.replace("implementor", "controller")).unwrap();
        c.validate().unwrap();
        assert!(c.seats[0].headless);
    }

    #[test]
    fn reserved_seat_names() {
        for name in [
            "all",
            "owner",
            "console",
            "builder-lab1",
            "r-20261127-083000-3fa9c1",
        ] {
            let t = format!(
                "host = \"h\"\n[[seat]]\nname = \"{name}\"\nrole = \"reviewer\"\nruntime = \"codex\"\ntier = \"fast\"\n"
            );
            let c: Config = toml::from_str(&t).unwrap();
            assert!(c.validate().is_err(), "{name}");
        }
    }

    #[test]
    fn hub_auth_defaults() {
        let old = "host = \"h\"\n[server]\nlisten = \"127.0.0.1\"\nport = 4222\nstore_dir = \"s\"\nnats_version = \"2.15.3\"\n";
        let c: Config = toml::from_str(old).unwrap();
        assert_eq!(
            c.server.unwrap().auth,
            HubAuth::Token,
            "an older hub keeps its token"
        );
        assert_eq!(
            Server::default().auth,
            HubAuth::Users,
            "a new hub has users"
        );
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
        let g: Github = toml::from_str(
            "owner = \"hhenson\"\nrepos = []\nbranch_prefix = \"codex/\"\nwrapper = \"w\"\n",
        )
        .unwrap();
        assert_eq!(
            g.clone_url_for("hgl_spec"),
            "https://github.com/hhenson/hgl_spec.git"
        );
        let g = Github {
            clone_url: "/srv/git/{repo}.git".into(),
            ..Github::default()
        };
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

    #[test]
    fn github_service_keys() {
        // Defaults fill in, and a saved file does not repeat them.
        let c: Config = toml::from_str("host = \"h\"\n").unwrap();
        assert_eq!(c.github, Github::default());
        assert_eq!(c.github.review_bot, "chatgpt-codex-connector");
        assert_eq!(c.github.gate["hgl-compiler"].verdicts.len(), 3);
        let text = toml::to_string_pretty(&c).unwrap();
        assert!(
            !text.contains("review_bot") && !text.contains("pins"),
            "{text}"
        );

        let t = r#"
host = "h"
[github]
owner = "o"
repos = ["a", "w"]
branch_prefix = "codex/"
wrapper = "w"
review_bot = "some-bot"
gate = { a = { verdicts = ["linux"] } }
pins = [{ path = "a", repo = "a" }, { path = "s", from = "a", submodule = "spec/s" }]
[github.tap]
repo = "tap"
formula = "f"
source_repo = "a"
bottle_workflow = "b.yml"
bottle_tag = "arm64_tahoe"
anchor = "x"
"#;
        let c: Config = toml::from_str(t).unwrap();
        c.validate().unwrap();
        assert_eq!(c.github.review_bot, "some-bot");
        assert_eq!(c.github.pins[1].submodule.as_deref(), Some("spec/s"));
        assert_eq!(c.github.release, ReleaseWatch::default());
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        c.save(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), c);

        let mut bad = c.clone();
        bad.github.pins[1].from = Some("s".into());
        assert!(bad.validate().is_err(), "from must name a pin with a repo");
        bad.github.pins[1] = Pin {
            path: "s".into(),
            repo: Some("x".into()),
            from: Some("a".into()),
            submodule: None,
        };
        assert!(bad.validate().is_err());
        let unknown = "host = \"h\"\n[github]\nowner=\"o\"\nrepos=[]\nbranch_prefix=\"\"\nwrapper=\"w\"\nbogus=1\n";
        assert!(toml::from_str::<Config>(unknown).is_err());
    }
}
