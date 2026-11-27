//! Wire types of builds (docs/build.md): the `build` job a builder takes, the summary it
//! returns in a `receipt` and keeps in `HGL_BUILD/run.<id>`, and the builder record in
//! `HGL_HOSTS/builder.<label>`.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ProtoError, Seat};

/// Recipes (`recipe.<repo>`, versioned) and run summaries (`run.<id>`).
pub const KV_BUILD: &str = "HGL_BUILD";
/// Summary and job format version.
pub const BUILD_VERSION: u32 = 1;
/// Prefix of builder records in `HGL_HOSTS`; host records never contain a dot.
pub const BUILDER_PREFIX: &str = "builder.";

string_enum!(
    /// Build platforms, named as `std::env::consts::OS` names them.
    Platform, "platform", { Linux => "linux", Macos => "macos", Windows => "windows" });

impl Platform {
    /// The platform this binary runs on, if it is one of the three.
    pub fn current() -> Option<Platform> {
        std::env::consts::OS.parse().ok()
    }
}

/// How a whole build ended, or where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildStatus {
    /// Sent to a builder, not started.
    Queued,
    Running,
    Pass,
    Fail,
    /// A prerequisite is missing (an SDK, a tool, the head, the remote): nothing was judged.
    NotRun,
    /// Stopped before the end (a signal, a builder shutting down).
    Stopped,
}

impl BuildStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            BuildStatus::Queued => "queued",
            BuildStatus::Running => "running",
            BuildStatus::Pass => "pass",
            BuildStatus::Fail => "fail",
            BuildStatus::NotRun => "not-run",
            BuildStatus::Stopped => "stopped",
        }
    }
    pub fn is_final(self) -> bool {
        !matches!(self, BuildStatus::Queued | BuildStatus::Running)
    }
    /// Exit status of `hgl-ai build run|wait` (docs/build.md BUILD-4): 0 pass, 1 fail,
    /// 3 not run.
    pub fn exit_code(self) -> i32 {
        match self {
            BuildStatus::Pass => 0,
            BuildStatus::NotRun => 3,
            _ => 1,
        }
    }
}

impl std::fmt::Display for BuildStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How one step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepStatus {
    Running,
    Pass,
    Fail,
    /// A need is missing; the step did not start.
    NotRun,
    /// Not run because an earlier step failed or one it needs did not pass.
    Skipped,
    Stopped,
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            StepStatus::Running => "running",
            StepStatus::Pass => "pass",
            StepStatus::Fail => "fail",
            StepStatus::NotRun => "not-run",
            StepStatus::Skipped => "skipped",
            StepStatus::Stopped => "stopped",
        }
    }
}

/// One step of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSummary {
    pub name: String,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    #[serde(default)]
    pub wall_ms: u64,
    /// User and system time of the step's process tree, where measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_bytes: Option<u64>,
    /// The memory cap it ran under; `None` where there is none (a Windows remote).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap_bytes: Option<u64>,
    /// `hit`, `partial` or `miss`, where the parser can tell (cargo's fresh artifacts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    /// `memory` or `time` when a cap ended it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
    /// Why it did not run, or what stopped it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The step's log (stdout and stderr together), which byte ranges refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    #[serde(default)]
    pub log_bytes: u64,
    #[serde(default)]
    pub errors: u64,
    #[serde(default)]
    pub warnings: u64,
    /// Artifacts kept, relative to the run's log directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
}

impl StepSummary {
    pub fn new(name: impl Into<String>, status: StepStatus) -> Self {
        StepSummary {
            name: name.into(),
            status,
            exit: None,
            wall_ms: 0,
            cpu_ms: None,
            peak_bytes: None,
            cap_bytes: None,
            cache: None,
            limit: None,
            reason: None,
            log: None,
            log_bytes: 0,
            errors: 0,
            warnings: 0,
            artifacts: Vec::new(),
        }
    }
}

/// An error or warning, deduplicated (`count` occurrences), with its byte range
/// `[start, end)` in the step's log (or in `source`, an artifact the parser read).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diag {
    pub step: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub col: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(default = "one")]
    pub count: u64,
    pub start: u64,
    pub end: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

fn one() -> u64 {
    1
}

/// A failing test with its assertion or panic message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestFailure {
    pub step: String,
    pub name: String,
    pub message: String,
    pub start: u64,
    pub end: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tests {
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    /// The first failures (bounded); `failed` counts all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<TestFailure>,
}

/// What a build reports (BUILD-4): in `summary.json`, a `receipt` body and `run.<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub v: u32,
    pub id: String,
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The tree had uncommitted changes (a local run).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dirty: bool,
    /// Label of the builder or host that ran it.
    pub host: String,
    pub platform: Platform,
    /// `local`, or `ssh` for a remote driven by a proxy builder.
    pub executor: String,
    /// The proxy builder that drove a remote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
    pub status: BuildStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe_rev: Option<u64>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default)]
    pub wall_ms: u64,
    /// Directory of the step logs and `summary.json`, on `host` (or `proxy`).
    pub log_dir: String,
    pub steps: Vec<StepSummary>,
    /// The first errors, deduplicated (bounded); `errors_total` counts all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<Diag>,
    #[serde(default)]
    pub errors_total: u64,
    #[serde(default)]
    pub tests: Tests,
    #[serde(default)]
    pub warnings_total: u64,
    /// Warnings counted by kind (a lint, a `-W` flag, a code).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub warnings: BTreeMap<String, u64>,
}

/// The body of a `build` note to `builder-<label>` (BUILD-6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildJob {
    pub v: u32,
    pub id: String,
    pub repo: String,
    pub head: String,
    /// Merge `head` onto this branch or commit first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Pull request number, to fetch `refs/pull/<n>/head`; 0 for none.
    #[serde(default)]
    pub issue: u64,
    /// Only these steps; all of the platform's when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<String>>,
    /// Recipe revision; the latest when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe_rev: Option<u64>,
    #[serde(default)]
    pub keep_going: bool,
    /// Where the `receipt` goes ([`reply_seat`]).
    pub reply_to: Seat,
    /// A seat that also gets the receipt, so its agent wakes when the build ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<Seat>,
    pub requester: String,
}

/// `HGL_HOSTS/builder.<label>`: a builder that takes `build` jobs, refreshed with its
/// heartbeat. For a remote driven by a proxy builder, `label` is the remote's and
/// `proxy` the proxy's host label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderRecord {
    pub label: String,
    pub inbox: Seat,
    pub platform: Platform,
    #[serde(default)]
    pub arch: String,
    pub executor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    pub reachable: bool,
    /// Why it is not reachable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `active`, `draining` or `retired`.
    pub state: String,
    pub slots: u32,
    pub busy: u32,
    /// Free space where its builds run, in GiB, where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_gb: Option<u64>,
    /// Tool name to the first line of its `--version`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub toolchains: BTreeMap<String, String>,
    pub updated_at: String,
}

impl BuilderRecord {
    /// Takes new jobs now: active, reachable, and a slot free.
    pub fn ready(&self) -> bool {
        self.state == "active" && self.reachable && self.busy < self.slots
    }
}

/// `recipe.<repo>`
pub fn recipe_key(repo: &str) -> String {
    format!("recipe.{repo}")
}

/// `run.<id>`
pub fn run_key(id: &str) -> String {
    format!("run.{id}")
}

/// `builder.<label>` in `HGL_HOSTS`.
pub fn builder_key(label: &str) -> String {
    format!("{BUILDER_PREFIX}{label}")
}

/// An address of the build machinery: a builder's inbox (`builder-*`) or a build's reply
/// address (`r-*`). No configured seat takes such a name.
pub fn is_build_address(seat: &Seat) -> bool {
    seat.as_str().starts_with("builder-") || seat.as_str().starts_with("r-")
}

/// A seat a build receipt may go to (`notify`): a configured seat's name, not `all`,
/// `owner`, `console`, a builder or a reply address.
pub fn may_receive_receipts(seat: &Seat) -> bool {
    !seat.is_reserved() && !is_build_address(seat)
}

/// A repository name as recipes and checkouts use it: `[A-Za-z0-9_.-]`, not starting
/// with `.` (it names a key and a directory).
pub fn check_repo(repo: &str) -> Result<(), ProtoError> {
    let ok = !repo.is_empty()
        && repo.len() <= 100
        && !repo.starts_with('.')
        && repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(ProtoError::Unknown {
            what: "repository name",
            value: repo.to_string(),
        })
    }
}

/// A git branch or commit as a job's `base`: no option, space or path escape.
pub fn check_base(base: &str) -> Result<(), ProtoError> {
    let ok = !base.is_empty()
        && base.len() <= 200
        && !base.starts_with(['-', '/', '.'])
        && !base.contains("..")
        && !base.contains("//")
        && base
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(ProtoError::Unknown {
            what: "base",
            value: base.to_string(),
        })
    }
}

impl BuildJob {
    /// Why this job, from the note `n`, cannot be run (it is then terminated): a version,
    /// id, head, repository or base of the wrong form, a disagreement with the note, a
    /// reply address other than its own, or a `notify` seat that may not get receipts.
    pub fn check(&self, n: &super::Note) -> Result<(), String> {
        if self.v != BUILD_VERSION {
            return Err(format!("job version {}", self.v));
        }
        check_build_id(&self.id).map_err(|e| e.to_string())?;
        super::check_head(&self.head).map_err(|e| e.to_string())?;
        check_repo(&self.repo).map_err(|e| e.to_string())?;
        if let Some(b) = &self.base {
            check_base(b).map_err(|e| e.to_string())?;
        }
        if n.repo.as_deref() != Some(self.repo.as_str())
            || n.head.as_deref() != Some(self.head.as_str())
            || n.base != self.base
            || n.issue != self.issue
        {
            return Err("the job's repo, head, base or issue differ from its note's".into());
        }
        if reply_seat(&self.id).ok().as_ref() != Some(&self.reply_to) {
            return Err(format!(
                "reply_to {} is not the build's own address r-{}",
                self.reply_to, self.id
            ));
        }
        if let Some(s) = &self.notify
            && !may_receive_receipts(s)
        {
            return Err(format!(
                "notify {s}: not a seat that may get build receipts"
            ));
        }
        Ok(())
    }
}

/// The inbox of the builder `label`: `builder-<label>`.
pub fn builder_inbox(label: &str) -> Result<Seat, ProtoError> {
    Seat::new(format!("builder-{label}"))
}

/// Where the receipt of build `id` goes: `r-<id>`, an address no seat serves, so the
/// requester reads it from the stream.
pub fn reply_seat(id: &str) -> Result<Seat, ProtoError> {
    Seat::new(format!("r-{id}"))
}

/// A build id: `YYYYMMDD-HHMMSS-xxxxxx` (UTC, then 6 hex digits), ordered by start.
pub fn new_build_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = chrono::Utc::now();
    let mut h = Sha256::new();
    h.update(now.timestamp_nanos_opt().unwrap_or_default().to_be_bytes());
    h.update(std::process::id().to_be_bytes());
    h.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    h.update(format!("{:?}", std::thread::current().id()).as_bytes());
    format!(
        "{}-{}",
        now.format("%Y%m%d-%H%M%S"),
        &hex::encode(h.finalize())[..6]
    )
}

/// Check a build id's form (it becomes a key, a directory and an address).
pub fn check_build_id(id: &str) -> Result<(), ProtoError> {
    let b = id.as_bytes();
    let ok = b.len() == 22
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 15 => *c == b'-',
            0..=14 => c.is_ascii_digit(),
            _ => c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
        });
    if ok {
        Ok(())
    } else {
        Err(ProtoError::Unknown {
            what: "build id",
            value: id.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        let a = new_build_id();
        let b = new_build_id();
        assert_ne!(a, b);
        check_build_id(&a).unwrap();
        assert!(check_build_id("20261127-083000-ABCDEF").is_err());
        assert!(check_build_id("x").is_err());
        assert!(check_build_id("20261127-083000-abc").is_err());
        assert_eq!(reply_seat(&a).unwrap().as_str().len(), 24);
        assert_eq!(builder_inbox("lab1").unwrap().as_str(), "builder-lab1");
        assert!(builder_inbox(&"x".repeat(25)).is_err());
        assert_eq!(run_key(&a), format!("run.{a}"));
        assert_eq!(recipe_key("hgl_spec"), "recipe.hgl_spec");
        assert_eq!(builder_key("lab1"), "builder.lab1");
    }

    #[test]
    fn job_checks() {
        let id = new_build_id();
        let head = "8a5c3d264c2edbbdaa61530d810bff276c3eebba";
        let job = BuildJob {
            v: BUILD_VERSION,
            id: id.clone(),
            repo: "demo".into(),
            head: head.into(),
            base: Some("main".into()),
            issue: 0,
            steps: None,
            recipe_rev: None,
            keep_going: false,
            reply_to: reply_seat(&id).unwrap(),
            notify: Some(Seat::new("w1").unwrap()),
            requester: "w1".into(),
        };
        let mut n = crate::Note::new(
            Seat::new("w1").unwrap(),
            builder_inbox("lab1").unwrap(),
            crate::Kind::Build,
            "{}",
        );
        n.repo = Some("demo".into());
        n.head = Some(head.into());
        n.base = Some("main".into());
        job.check(&n).unwrap();
        let bad = |f: &dyn Fn(&mut BuildJob)| {
            let mut j = job.clone();
            f(&mut j);
            j.check(&n).unwrap_err()
        };
        assert!(bad(&|j| j.id = "../x".into()).contains("build id"));
        assert!(bad(&|j| j.repo = "../demo".into()).contains("repository"));
        assert!(bad(&|j| j.base = Some("--upload-pack=x".into())).contains("base"));
        assert!(bad(&|j| j.reply_to = Seat::owner()).contains("reply_to"));
        assert!(bad(&|j| j.notify = Some(Seat::broadcast())).contains("notify"));
        assert!(bad(&|j| j.notify = Some(Seat::new("r-x").unwrap())).contains("notify"));
        assert!(bad(&|j| j.head = "b".repeat(40)).contains("differ"));
        assert!(is_build_address(&Seat::new("builder-lab1").unwrap()));
        assert!(!may_receive_receipts(&Seat::console()));
    }

    #[test]
    fn statuses() {
        assert_eq!(
            serde_json::to_string(&BuildStatus::NotRun).unwrap(),
            "\"not-run\""
        );
        assert_eq!(BuildStatus::NotRun.exit_code(), 3);
        assert_eq!(BuildStatus::Pass.exit_code(), 0);
        assert_eq!(BuildStatus::Fail.exit_code(), 1);
        assert!(!BuildStatus::Running.is_final());
        assert_eq!(
            serde_json::to_string(&StepStatus::NotRun).unwrap(),
            "\"not-run\""
        );
        assert_eq!("windows".parse::<Platform>().unwrap(), Platform::Windows);
        assert_eq!(Platform::current().unwrap().as_str(), std::env::consts::OS);
    }

    #[test]
    fn summary_round_trip() {
        let s = Summary {
            v: BUILD_VERSION,
            id: new_build_id(),
            repo: "hgl-ai".into(),
            head: None,
            base: None,
            dirty: false,
            host: "lab1".into(),
            platform: Platform::Linux,
            executor: "local".into(),
            proxy: None,
            requester: None,
            status: BuildStatus::Fail,
            reason: None,
            recipe_rev: Some(2),
            started_at: "20261127T083000Z".into(),
            ended_at: None,
            wall_ms: 10,
            log_dir: "logs".into(),
            steps: vec![StepSummary::new("test", StepStatus::Fail)],
            errors: vec![],
            errors_total: 0,
            tests: Tests::default(),
            warnings_total: 0,
            warnings: BTreeMap::new(),
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(!j.contains("dirty"));
        assert_eq!(serde_json::from_str::<Summary>(&j).unwrap(), s);
    }
}
