//! Wire types of the hgl-ai protocol. See `docs/protocol.md`; rule ids are cited inline.
//! No I/O lives here.

use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Current note envelope version.
pub const NOTE_VERSION: u32 = 1;

pub const STREAM_NOTES: &str = "HGL_NOTES";
pub const SUBJECT_PREFIX: &str = "hgl.notes";
pub const KV_STATE: &str = "HGL_STATE";
pub const KV_HOSTS: &str = "HGL_HOSTS";
pub const KV_TREES: &str = "HGL_TREES";
pub const KV_LOCKS: &str = "HGL_LOCKS";
pub const KV_ENDED: &str = "HGL_ENDED";
/// The broadcast seat (PROTO-7).
pub const BROADCAST: &str = "all";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtoError {
    #[error("invalid seat name {0:?}: expected ^[a-z0-9][a-z0-9_-]{{0,31}}$")]
    Seat(String),
    #[error("invalid state key {0:?}: expected ^[a-zA-Z0-9_./=-]{{1,64}}$")]
    StateKey(String),
    #[error("invalid head {0:?}: expected a full 40-character hexadecimal commit")]
    Head(String),
    #[error("unknown {what} {value:?}")]
    Unknown { what: &'static str, value: String },
    #[error("note: {0}")]
    Note(String),
}

static SEAT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9_-]{0,31}$").unwrap());
static KEY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_./=-]{1,64}$").unwrap());
static HEAD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{40}$").unwrap());

/// A logical address on the bus (PROTO-7).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Seat(String);

impl Seat {
    pub fn new(s: impl Into<String>) -> Result<Self, ProtoError> {
        let s = s.into();
        if SEAT_RE.is_match(&s) {
            Ok(Seat(s))
        } else {
            Err(ProtoError::Seat(s))
        }
    }
    pub fn broadcast() -> Self {
        Seat(BROADCAST.to_string())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn is_broadcast(&self) -> bool {
        self.0 == BROADCAST
    }
    /// `hgl.notes.<seat>`
    pub fn subject(&self) -> String {
        format!("{SUBJECT_PREFIX}.{}", self.0)
    }
    /// Durable consumer name `inbox-<seat>`.
    pub fn consumer(&self) -> String {
        format!("inbox-{}", self.0)
    }
}

impl TryFrom<String> for Seat {
    type Error = ProtoError;
    fn try_from(s: String) -> Result<Self, ProtoError> {
        Seat::new(s)
    }
}
impl From<Seat> for String {
    fn from(s: Seat) -> String {
        s.0
    }
}
impl FromStr for Seat {
    type Err = ProtoError;
    fn from_str(s: &str) -> Result<Self, ProtoError> {
        Seat::new(s)
    }
}
impl fmt::Display for Seat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validate an `HGL_STATE` key (§8).
pub fn check_state_key(k: &str) -> Result<(), ProtoError> {
    if KEY_RE.is_match(k) {
        Ok(())
    } else {
        Err(ProtoError::StateKey(k.to_string()))
    }
}

/// Validate a full commit id (PROTO-10).
pub fn check_head(h: &str) -> Result<(), ProtoError> {
    if HEAD_RE.is_match(h) {
        Ok(())
    } else {
        Err(ProtoError::Head(h.to_string()))
    }
}

macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident, $what:literal, { $($var:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name { $($var),+ }
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$var),+];
            pub fn as_str(self) -> &'static str { match self { $($name::$var => $s),+ } }
        }
        impl FromStr for $name {
            type Err = ProtoError;
            fn from_str(s: &str) -> Result<Self, ProtoError> {
                match s { $($s => Ok($name::$var),)+ _ => Err(ProtoError::Unknown { what: $what, value: s.to_string() }) }
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
        }
    };
}

string_enum!(
    /// Note kinds (§4).
    Kind, "note kind", {
    Assign => "assign", Ready => "ready", Blocked => "blocked", Question => "question",
    Answer => "answer", Review => "review", Receipt => "receipt", Sendback => "sendback",
    Approved => "approved", Landed => "landed", Closed => "closed", Done => "done", Note => "note",
    Build => "build",
});

impl Kind {
    /// Kinds that also get one GitHub comment from their sender (PROTO-1).
    pub fn mirrored_on_github(self) -> bool {
        matches!(
            self,
            Kind::Ready | Kind::Blocked | Kind::Sendback | Kind::Approved | Kind::Landed
        )
    }
    /// Kinds that must carry `head` (PROTO-10).
    pub fn needs_head(self) -> bool {
        matches!(
            self,
            Kind::Ready
                | Kind::Review
                | Kind::Receipt
                | Kind::Sendback
                | Kind::Approved
                | Kind::Build
        )
    }
    /// Kinds after which the bridge removes the issue's worktrees (PROTO-14).
    pub fn ends_task(self) -> bool {
        matches!(self, Kind::Landed | Kind::Closed)
    }
    /// Kinds for which the bridge creates a worktree before the turn (PROTO-12).
    pub fn starts_task(self) -> bool {
        matches!(self, Kind::Assign | Kind::Review)
    }
}

string_enum!(
    /// Team roles (docs/roles.md §1).
    Role, "role", {
    Controller => "controller", Triage => "triage", Implementor => "implementor", Reviewer => "reviewer",
});

string_enum!(
    /// Agent command-line runtimes.
    Runtime, "runtime", { Claude => "claude", Codex => "codex", Agy => "agy" });

string_enum!(
    /// Model vendor families (ROLE-9).
    Family, "family", { Claude => "claude", Gpt => "gpt", Gemini => "gemini" });

string_enum!(
    /// Capability tiers (docs/roles.md §3).
    Tier, "tier", { Frontier => "frontier", Balanced => "balanced", Fast => "fast" });

string_enum!(
    /// Review receipt verdicts (PROTO-19).
    Verdict, "verdict", { Clean => "clean", Findings => "findings" });

string_enum!(
    /// Fleet mode (`HGL_STATE/mode`).
    Mode, "mode", { Normal => "normal", Drain => "drain" });

/// UTC timestamp in the protocol's compact form, `YYYYMMDDTHHMMSSZ`.
pub fn utc_stamp(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y%m%dT%H%M%SZ").to_string()
}
pub fn utc_now_stamp() -> String {
    utc_stamp(chrono::Utc::now())
}

/// The note envelope (§4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub v: u32,
    pub created_at: String,
    pub sender: Seat,
    pub recipient: Seat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Issue or pull request number; 0 where there is none.
    #[serde(default)]
    pub issue: u64,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Base branch or commit for `assign`; merge commit for `landed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    pub body: String,
}

impl Note {
    pub fn new(sender: Seat, recipient: Seat, kind: Kind, body: impl Into<String>) -> Self {
        let mut body = body.into();
        if !body.ends_with('\n') {
            body.push('\n');
        }
        Note {
            v: NOTE_VERSION,
            created_at: utc_now_stamp(),
            sender,
            recipient,
            repo: None,
            issue: 0,
            kind,
            head: None,
            base: None,
            body,
        }
    }

    /// Check the envelope against the protocol's rules before sending.
    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.v != NOTE_VERSION {
            return Err(ProtoError::Note(format!("unsupported version {}", self.v)));
        }
        if self.sender.is_broadcast() {
            return Err(ProtoError::Note("sender cannot be `all`".into()));
        }
        if let Some(h) = &self.head {
            check_head(h)?;
        } else if self.kind.needs_head() {
            return Err(ProtoError::Note(format!(
                "a `{}` note needs a full head",
                self.kind
            )));
        }
        if (self.kind.starts_task() || self.kind.ends_task())
            && (self.repo.is_none() || self.issue == 0)
        {
            return Err(ProtoError::Note(format!(
                "a `{}` note needs repo and issue",
                self.kind
            )));
        }
        if self.body.trim().is_empty() {
            return Err(ProtoError::Note("empty body".into()));
        }
        Ok(())
    }

    /// `Nats-Msg-Id` for deduplication (PROTO-9): `<sender>-<created_at>-<digest>`, the
    /// digest over the rest of the envelope, so a retry of this note gets the same id and
    /// a note to another recipient or task does not.
    pub fn msg_id(&self) -> String {
        let mut h = Sha256::new();
        let issue = self.issue.to_string();
        for field in [
            Some(self.recipient.as_str()),
            Some(self.kind.as_str()),
            self.repo.as_deref(),
            Some(issue.as_str()),
            self.head.as_deref(),
            self.base.as_deref(),
            Some(self.body.as_str()),
        ] {
            // Tagged and length-prefixed: no two envelopes encode alike.
            match field {
                None => h.update([0u8]),
                Some(s) => {
                    h.update([1u8]);
                    h.update((s.len() as u64).to_be_bytes());
                    h.update(s.as_bytes());
                }
            }
        }
        format!(
            "{}-{}-{}",
            self.sender,
            self.created_at,
            &hex::encode(h.finalize())[..12]
        )
    }

    /// The task this note belongs to, if it names one.
    pub fn task(&self) -> Option<TaskKey> {
        match (&self.repo, self.issue) {
            (Some(r), n) if n > 0 => Some(TaskKey {
                repo: r.clone(),
                issue: n,
            }),
            _ => None,
        }
    }
}

/// A repository and issue (or pull request) number: the unit a worktree belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TaskKey {
    pub repo: String,
    pub issue: u64,
}

impl fmt::Display for TaskKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.repo, self.issue)
    }
}

/// Review receipt (PROTO-19), carried as JSON in a `receipt` note's body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewReceipt {
    pub repo: String,
    pub pr: u64,
    pub head: String,
    pub main: String,
    pub reviewer: Seat,
    pub family: Family,
    pub model: String,
    pub verdict: Verdict,
    pub evidence: String,
    #[serde(default)]
    pub limitations: String,
}

/// Desk exit codes (PROTO-18).
pub mod desk_exit {
    pub const OK: i32 = 0;
    pub const NO_MERGE: i32 = 3;
    pub const BUILD: i32 = 4;
    pub const LOCKED: i32 = 5;
    pub const DISK: i32 = 6;
}

/// One seat as a host advertises it (§9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatRecord {
    pub seat: Seat,
    pub role: Role,
    pub runtime: Runtime,
    pub family: Family,
    pub model: String,
}

/// `HGL_HOSTS/<host>` (§9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRecord {
    pub host: String,
    pub seats: Vec<SeatRecord>,
    pub memcap_gb: u64,
    pub work_root_free_gb: u64,
    pub updated_at: String,
}

/// `HGL_TREES/<host>.<seat>.<repo>-<issue>` (PROTO-13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeRecord {
    pub host: String,
    pub seat: Seat,
    pub task: TaskKey,
    /// `true` for a reviewer's detached tree.
    #[serde(default)]
    pub review: bool,
    pub path: String,
    pub branch: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub size_bytes: u64,
}

impl TreeRecord {
    /// KV key; dots separate the parts, so the repo's own characters stay valid.
    pub fn key(&self) -> String {
        tree_key(&self.host, &self.seat, &self.task, self.review)
    }
}

pub fn tree_key(host: &str, seat: &Seat, task: &TaskKey, review: bool) -> String {
    let suffix = if review { "-review" } else { "" };
    format!("{host}.{seat}.{}-{}{suffix}", task.repo, task.issue)
}

/// `HGL_LOCKS/<name>`: a lease, held until released or until it is not refreshed
/// within the bucket's `max_age` (§3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockRecord {
    /// Who holds it: host label, process and purpose.
    pub holder: String,
    pub acquired_at: String,
}

/// The landing lock of `repo` (§8): one landing or release watch at a time, on any host.
pub fn land_lock_key(repo: &str) -> String {
    format!("lock.land.{repo}")
}

/// The seat lease of `seat` (PROTO-6): held by the one bridge, on any host, that serves it.
pub fn seat_lock_key(seat: &Seat) -> String {
    format!("seat.{seat}")
}

/// `HGL_ENDED/<seat>.<repo>-<issue>`: a task that ended for a seat (`landed`, `closed`;
/// PROTO-14). Notes of the task to that seat at a lower stream sequence are moot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndedRecord {
    /// Stream sequence of the ending note.
    pub seq: u64,
    pub ended_at: String,
}

pub fn ended_key(seat: &Seat, task: &TaskKey) -> String {
    format!("{seat}.{}-{}", task.repo, task.issue)
}

/// Build recipes, jobs, summaries and builder records (docs/build.md).
pub mod build;

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(s: &str) -> Seat {
        Seat::new(s).unwrap()
    }

    #[test]
    fn seat_names() {
        assert!(Seat::new("w1").is_ok());
        assert!(Seat::new("reviewer_types").is_ok());
        for bad in ["", "W1", "a.b", "a*", ">", "-x", &"x".repeat(33)] {
            assert!(Seat::new(bad).is_err(), "{bad:?}");
        }
        assert_eq!(seat("w1").subject(), "hgl.notes.w1");
        assert_eq!(seat("w1").consumer(), "inbox-w1");
    }

    #[test]
    fn state_keys() {
        assert!(check_state_key("landed.log").is_ok());
        assert!(check_state_key("a/b=c").is_ok());
        assert!(check_state_key("").is_err());
        assert!(check_state_key("a b").is_err());
    }

    #[test]
    fn kinds_round_trip() {
        for k in Kind::ALL {
            assert_eq!(k.as_str().parse::<Kind>().unwrap(), *k);
            let j = serde_json::to_string(k).unwrap();
            assert_eq!(j, format!("\"{}\"", k.as_str()));
        }
        assert!("merge".parse::<Kind>().is_err());
    }

    #[test]
    fn note_validation() {
        let head = "8a5c3d264c2edbbdaa61530d810bff276c3eebba";
        let mut n = Note::new(seat("w4"), seat("claude"), Kind::Ready, "[codex] Ready");
        assert!(n.validate().is_err(), "ready without head");
        n.head = Some(head[..8].into());
        assert!(n.validate().is_err(), "short head");
        n.head = Some(head.into());
        assert!(n.validate().is_ok());
        assert!(n.body.ends_with('\n'));

        let mut a = Note::new(seat("claude"), seat("w1"), Kind::Assign, "take it");
        assert!(a.validate().is_err(), "assign without task");
        a.repo = Some("hgl-compiler".into());
        a.issue = 992;
        assert!(a.validate().is_ok());
        assert_eq!(a.task().unwrap().to_string(), "hgl-compiler-992");

        let b = Note::new(Seat::broadcast(), seat("w1"), Kind::Note, "x");
        assert!(b.validate().is_err());
    }

    #[test]
    fn note_json_shape() {
        let mut n = Note::new(seat("w4"), seat("claude"), Kind::Note, "hello");
        n.created_at = "20261127T041100Z".into();
        let j: serde_json::Value = serde_json::to_value(&n).unwrap();
        assert_eq!(j["kind"], "note");
        assert_eq!(j["issue"], 0);
        assert!(j.get("head").is_none());
        let back: Note = serde_json::from_value(j).unwrap();
        assert_eq!(back, n);
        assert!(serde_json::from_str::<Note>(r#"{"v":1,"created_at":"x","sender":"BAD","recipient":"a","kind":"note","body":"b"}"#).is_err());
    }

    #[test]
    fn msg_id_is_stable() {
        let mut n = Note::new(seat("w4"), seat("claude"), Kind::Note, "hello");
        n.created_at = "20261127T041100Z".into();
        let id = n.msg_id();
        assert!(id.starts_with("w4-20261127T041100Z-"));
        assert_eq!(id.len(), "w4-20261127T041100Z-".len() + 12);
        assert_eq!(id, n.clone().msg_id());
    }

    #[test]
    fn msg_id_names_the_envelope() {
        let mut n = Note::new(seat("claude"), seat("w1"), Kind::Landed, "[claude] Landed");
        n.created_at = "20261127T041100Z".into();
        n.repo = Some("hgl-compiler".into());
        n.issue = 980;
        let id = n.msg_id();
        let mut ids = vec![id.clone()];
        let variants: [&dyn Fn(&mut Note); 7] = [
            &|n| n.recipient = Seat::broadcast(),
            &|n| n.kind = Kind::Closed,
            &|n| n.repo = Some("hgl_spec".into()),
            &|n| n.issue = 981,
            &|n| n.head = Some("8a5c3d264c2edbbdaa61530d810bff276c3eebba".into()),
            &|n| n.base = Some("01e99ea5e46457bac035ca8a2699b6b6575b32ed".into()),
            &|n| n.body = "[claude] Landed again\n".into(),
        ];
        for change in variants {
            let mut m = n.clone();
            change(&mut m);
            assert!(m.msg_id().starts_with("claude-20261127T041100Z-"));
            ids.push(m.msg_id());
        }
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");
        // No field runs into the next: a repo of "" differs from no repo.
        let mut a = n.clone();
        a.repo = Some(String::new());
        let mut b = n.clone();
        b.repo = None;
        assert_ne!(a.msg_id(), b.msg_id());
        assert_eq!(n.clone().msg_id(), id, "a retry keeps its id");
    }

    #[test]
    fn tree_keys() {
        let t = TaskKey {
            repo: "hgl-compiler".into(),
            issue: 992,
        };
        assert_eq!(
            tree_key("build-1", &seat("w1"), &t, false),
            "build-1.w1.hgl-compiler-992"
        );
        assert_eq!(
            tree_key("build-1", &seat("r1"), &t, true),
            "build-1.r1.hgl-compiler-992-review"
        );
        assert_eq!(land_lock_key("hgl-compiler"), "lock.land.hgl-compiler");
        assert_eq!(seat_lock_key(&seat("w1")), "seat.w1");
        assert_eq!(ended_key(&seat("w1"), &t), "w1.hgl-compiler-992");
    }
}
