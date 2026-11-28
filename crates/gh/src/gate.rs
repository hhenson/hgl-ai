//! The merge gate, `gh merge-if-clean REPO PR HEAD`, ported from the shell gate with every
//! check it made. A pull request is merged only when all hold:
//!
//! - its head is the `HEAD` asked for, or the merge of that head with the present `main`
//!   that bringing it up to date made;
//! - it targets `main`, is open, not a draft, has no conflict and no unresolved thread;
//! - the review bot reviewed the head clean, or the head's first parent where the head only
//!   merges the present `main` into it and `main` moved since that parent only in files the
//!   pull request does not touch and in none of the global paths, or (`--triaged`) reviewed
//!   the head with findings the controller triaged;
//! - (unless the repository turns it off) a clean review receipt (PROTO-19),
//!   from a model family none of the task's authors used (ROLE-11), sent to the controller
//!   and read from the notes stream on the hub; the authors' families come from the
//!   implementor trees the bridges registered (`HGL_TREES`). For hgl-ai the receipt is for
//!   the actual head, or carries through a conflict-free series of merges from main with
//!   a clean bot review and passing checks at the actual head;
//! - where the repository names local build verdicts, each passed for this head on this
//!   `main`; then the branch is not brought up to date;
//! - it is not behind `main`, or `main` moved only in files it does not touch and in none of
//!   the global paths; otherwise it is brought up to date (and not merged: CI runs first);
//! - (ROLE-20; the merge bypasses the rulesets, so `mergeable_state` is not relied on)
//!   GitHub computed it `mergeable`, no check failed, every required check (`[github.gate.
//!   <repo>] checks`) is present and passed, and every other check run and commit status
//!   present is done and passed, except the `relax_optional` ones where it changes only
//!   relaxed paths and every `relax_required` check passed;
//! - no build output among its files and no local path among its added lines.
//!
//! The decision is [`decide`], a pure function over [`Facts`] that names the next fact it
//! needs; [`evaluate`] fetches facts until it decides, and acts unless it is a dry run.

mod carry;

use std::collections::BTreeMap;

use hgl_ai_bus::Bus;
use hgl_ai_proto::{Family, Kind as NoteKind, ReviewReceipt, Verdict};
use serde_json::Value;

use crate::bot::{BotView, Comment, Review};
pub use crate::builds::{GateBuilds, VerdictState};
use crate::exec::{self, Gh};
use crate::lock::LandLock;
use crate::merge::{self, Merge};
use crate::progress::{self, Progress};
use crate::{Error, GateRules, Result, Settings, short, state};

/// The pull request fields the gate reads (REST `pulls/N`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pr {
    pub number: u64,
    pub state: String,
    pub merged: bool,
    /// `mergeable_state`, upper case (`CLEAN`, `UNSTABLE`, `DIRTY`, `BLOCKED`, `BEHIND`, `UNKNOWN`, ...).
    pub mergeable_state: String,
    /// `mergeable`: `None` while GitHub has not computed it.
    pub mergeable: Option<bool>,
    pub head: String,
    pub branch: String,
    pub base: String,
    pub draft: bool,
    pub title: String,
    pub merge_commit: Option<String>,
    pub url: String,
}

fn resp(what: &str, msg: &str) -> Error {
    Error::Response {
        what: what.into(),
        msg: msg.into(),
    }
}

impl Pr {
    pub fn from_rest(v: &Value) -> Result<Pr> {
        let s = |path: &[&str]| {
            let mut x = v;
            for p in path {
                x = x.get(p)?;
            }
            x.as_str().map(String::from)
        };
        Ok(Pr {
            number: v
                .get("number")
                .and_then(Value::as_u64)
                .ok_or_else(|| resp("pull request", "no number"))?,
            state: s(&["state"]).unwrap_or_default(),
            merged: v.get("merged").and_then(Value::as_bool).unwrap_or(false),
            mergeable_state: s(&["mergeable_state"])
                .unwrap_or_else(|| "unknown".into())
                .to_ascii_uppercase(),
            mergeable: v.get("mergeable").and_then(Value::as_bool),
            head: s(&["head", "sha"]).ok_or_else(|| resp("pull request", "no head.sha"))?,
            branch: s(&["head", "ref"]).unwrap_or_default(),
            base: s(&["base", "ref"]).unwrap_or_default(),
            draft: v.get("draft").and_then(Value::as_bool).unwrap_or(false),
            title: s(&["title"]).unwrap_or_default(),
            merge_commit: s(&["merge_commit_sha"]),
            url: s(&["html_url"]).unwrap_or_default(),
        })
    }
}

/// The latest run of one check (by name; a re-run has a higher id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
}

impl CheckRun {
    /// Completed with a conclusion that does not hold a merge back.
    pub fn passed(&self) -> bool {
        self.status == "completed"
            && matches!(
                self.conclusion.as_deref(),
                Some("success" | "neutral" | "skipped")
            )
    }
    pub fn failed(&self) -> bool {
        matches!(
            self.conclusion.as_deref(),
            Some("failure" | "cancelled" | "timed_out")
        )
    }
}

/// The commit statuses of `commits/SHA/status` (the latest per context), as checks named
/// by their context: `pending` is not completed, `failure` and `error` failed.
pub fn commit_statuses(v: &Value) -> Vec<CheckRun> {
    v.get("statuses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|s| {
            let state = s.get("state").and_then(Value::as_str).unwrap_or("");
            let (status, conclusion) = match state {
                "success" => ("completed", Some("success")),
                "failure" | "error" => ("completed", Some("failure")),
                _ => ("pending", None),
            };
            CheckRun {
                id: s.get("id").and_then(Value::as_u64).unwrap_or(0),
                name: s
                    .get("context")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                status: status.into(),
                conclusion: conclusion.map(String::from),
            }
        })
        .collect()
}

/// The latest run of each check from the pages of `commits/SHA/check-runs?filter=all`.
pub fn latest_checks(pages: &[Value]) -> Vec<CheckRun> {
    let mut by: BTreeMap<String, CheckRun> = BTreeMap::new();
    for page in pages {
        for c in page
            .get("check_runs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let run = CheckRun {
                id: c.get("id").and_then(Value::as_u64).unwrap_or(0),
                name: c
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                status: c
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                conclusion: c
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .map(String::from),
            };
            if by.get(&run.name).is_none_or(|o| o.id < run.id) {
                by.insert(run.name.clone(), run);
            }
        }
    }
    by.into_values().collect()
}

/// Files of a comparison; the API lists at most 300, so a list that long is not whole.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileList {
    pub files: Vec<String>,
    pub whole: bool,
}

pub fn compare_files(v: &Value) -> FileList {
    let mut files: Vec<String> = v
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| f.get("filename").and_then(Value::as_str).map(String::from))
        .collect();
    files.sort();
    files.dedup();
    FileList {
        whole: files.len() < 300,
        files,
    }
}

/// One changed file of the pull request (`pulls/N/files`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrFile {
    pub name: String,
    pub status: String,
}

pub fn pr_files(list: &[Value]) -> Vec<PrFile> {
    list.iter()
        .map(|f| PrFile {
            name: f
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            status: f
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
        .collect()
}

/// Added lines of a unified diff that match any pattern.
pub fn lines_matching(diff: &str, patterns: &[regex::Regex]) -> usize {
    diff.lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .filter(|l| patterns.iter().any(|p| p.is_match(l)))
        .count()
}

/// One local build verdict: its name and what it is for this head on this main.
pub type VerdictResult = (String, VerdictState);

/// A review receipt as the gate saw it: the seat that sent the note, and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenReceipt {
    pub sender: String,
    pub receipt: ReviewReceipt,
}

/// Everything the gate may read. `None` is not fetched yet.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub pr: Pr,
    pub main: Option<String>,
    pub unresolved: Option<u64>,
    /// How many commits `main` has that the head does not.
    pub behind: Option<u64>,
    pub bot: Option<BotView>,
    pub bot_reviews: Option<Vec<Review>>,
    /// Parents of the pull request's head.
    pub parents: Option<Vec<String>>,
    pub verdicts: Option<Vec<VerdictResult>>,
    pub checks: Option<Vec<CheckRun>>,
    /// The pull request's files since the merge base, and `main`'s.
    pub mine: Option<FileList>,
    pub theirs: Option<FileList>,
    pub pr_files: Option<Vec<PrFile>>,
    /// The pull request's diff, or why it could not be read.
    pub diff: Option<std::result::Result<String, String>>,
    /// Where the head merges `main` into a reviewed parent: the pull request's files since
    /// the merge base of that parent, and what `main` gained since then.
    pub update_files: Option<(FileList, FileList)>,
    /// The review receipts for this pull request, or why they could not be read.
    pub receipts: Option<std::result::Result<Vec<SeenReceipt>, String>>,
    /// The model families of the task's authors, or why they could not be read.
    pub authors: Option<std::result::Result<Vec<Family>, String>>,
    /// The first reviewed ancestor a receipt can carry from, or why Git could not check.
    pub receipt_carry: Option<std::result::Result<Option<String>, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    Main,
    Unresolved,
    Behind,
    BotComments,
    BotReviews,
    Parents,
    Verdicts,
    Checks,
    CompareFiles,
    PrFiles,
    Diff,
    UpdateFiles,
    Receipts,
    Authors,
    ReceiptCarry,
}

pub(crate) const RECEIPT_CARRY_REPO: &str = "hgl-ai";

/// Switches the controller gives: `TRIAGED`, `HYGIENE_READ` and `NOUPDATE` of the shell gate,
/// bound to the head asked for.
#[derive(Debug, Clone, Copy, Default)]
pub struct Opts {
    pub triaged: bool,
    pub hygiene_read: bool,
    pub no_update: bool,
    /// The issue the pull request closes (its task, for the authors' families); else the
    /// issue in its branch name, else its number.
    pub closes: Option<u64>,
    /// A pull request the tool made, not an agent (the wrapper's pin pull requests): it has
    /// no author family for a review receipt to differ from, so none is asked for.
    pub tool_made: bool,
    /// The landing's deadline: the latest an accepted merge is polled until, and no later
    /// than the landing lock's lease from the merge; else [`crate::merge::POLL_LIMIT`]
    /// from the merge ([`crate::merge::until`]).
    pub merge_until: Option<tokio::time::Instant>,
}

/// The last word of a gate line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Merged,
    AlreadyMerged,
    /// Merged by an earlier attempt, whose steps after the merge this one finished.
    Resumed,
    /// Merged, but a step after the merge is not done; the same command finishes it.
    Partial,
    Updated,
    Wait,
    Blocked,
    BotLimit,
    Behind,
    /// `gh land`: `HGL_STATE/hold` was still active at the deadline; the gate did not run.
    Hold,
    /// Dry run: the gate is satisfied and it would merge.
    WouldMerge,
    /// Dry run: it would bring the branch up to date.
    WouldUpdate,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Merged | Kind::AlreadyMerged | Kind::Resumed => "MERGED",
            Kind::Partial => "PARTIAL",
            Kind::Updated => "UPDATED",
            Kind::Wait => "WAIT",
            Kind::Blocked => "BLOCKED",
            Kind::BotLimit => "BOTLIMIT",
            Kind::Behind => "BEHIND",
            Kind::Hold => "HOLD",
            Kind::WouldMerge => "WOULD-MERGE",
            Kind::WouldUpdate => "WOULD-UPDATE",
        }
    }
    /// Whether the gate is satisfied (exit 0).
    pub fn satisfied(self) -> bool {
        matches!(
            self,
            Kind::Merged | Kind::AlreadyMerged | Kind::Resumed | Kind::WouldMerge
        )
    }
    /// Merged by this command, or its landing finished by it.
    pub fn landed(self) -> bool {
        matches!(self, Kind::Merged | Kind::Resumed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub kind: Kind,
    /// One line, ending in the kind's word.
    pub line: String,
    /// Whether waiting may change it (a landing loop tries again).
    pub retry: bool,
    pub merge_commit: Option<String>,
    /// The head the gate judged.
    pub head: String,
}

impl Outcome {
    pub(crate) fn new(kind: Kind, line: String, retry: bool, head: &str) -> Outcome {
        Outcome {
            kind,
            line,
            retry,
            merge_commit: None,
            head: head.to_string(),
        }
    }
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "outcome": self.kind.word(),
            "line": self.line,
            "head": self.head,
            "merge_commit": self.merge_commit,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Need(Need),
    Done(Outcome),
    /// Bring the branch up to date; the line so far.
    Update(String),
    /// Merge at the head; the line so far.
    Merge(String),
}

macro_rules! need {
    ($e:expr, $n:expr) => {
        match &$e {
            Some(v) => v,
            None => return Step::Need($n),
        }
    };
}

/// The gate's decision over what is known so far.
pub fn decide(f: &Facts, repo: &str, asked: &str, rules: &GateRules, opts: &Opts) -> Step {
    let pr = &f.pr;
    let head = pr.head.as_str();
    let done =
        |kind: Kind, line: String, retry: bool| Step::Done(Outcome::new(kind, line, retry, head));
    let prefix = format!("{repo} #{} {}", pr.number, pr.branch);
    if pr.merged {
        return done(
            Kind::AlreadyMerged,
            format!("{repo} #{} already MERGED", pr.number),
            false,
        );
    }
    if pr.state != "open" {
        return done(
            Kind::Blocked,
            format!("{prefix} {} closed BLOCKED", pr.state),
            false,
        );
    }
    let main = need!(f.main, Need::Main).as_str();
    if head != asked {
        let parents = need!(f.parents, Need::Parents);
        if !(parents.len() == 2 && parents[0] == asked && parents[1] == main) {
            let line = format!(
                "{prefix} head {} is not {} nor its merge with main {}: head moved BLOCKED",
                short(head),
                short(asked),
                short(main)
            );
            return done(Kind::Blocked, line, false);
        }
    }
    let unresolved = *need!(f.unresolved, Need::Unresolved);
    let mut behind = *need!(f.behind, Need::Behind);
    let bot = need!(f.bot, Need::BotComments);
    let mut reviewed = "no";
    // Why the reviewed parent does not count: `main` moved in what the review did not see.
    let mut parent_stale = None;
    if bot.clean_on(head) {
        reviewed = "head";
    } else {
        let parents = need!(f.parents, Need::Parents);
        if parents.len() == 2 && parents[1] == main && bot.clean_on(&parents[0]) {
            let (mine, theirs) = need!(f.update_files, Need::UpdateFiles);
            match moved_in(mine, theirs, &rules.global_paths) {
                None => reviewed = "parent",
                Some(w) => {
                    parent_stale = Some(format!("main moved in {w} since {}", short(&parents[0])))
                }
            }
        }
    }
    if reviewed == "no" && opts.triaged {
        let reviews = need!(f.bot_reviews, Need::BotReviews);
        if reviews.iter().any(|r| r.commit.as_deref() == Some(head)) {
            reviewed = "triaged";
        }
    }
    let ms = pr.mergeable_state.clone();
    let mut line = format!(
        "{prefix} {ms} head {} unresolved {unresolved} behind {behind} reviewed {reviewed}",
        short(head)
    );
    let bot_pending = |line: &str| {
        if bot.limited() {
            return Some(done(
                Kind::BotLimit,
                format!("{line} review bot out of quota BOTLIMIT"),
                false,
            ));
        }
        bot.running_on(head)
            .then(|| done(Kind::Wait, format!("{line} review-running WAIT"), true))
    };
    if reviewed == "no"
        && let Some(step) = bot_pending(&line)
    {
        return step;
    }
    if pr.base != "main" || unresolved != 0 || reviewed == "no" {
        let mut why = Vec::new();
        if pr.base != "main" {
            why.push(format!("base {}", pr.base));
        }
        if unresolved != 0 {
            why.push("unresolved threads".to_string());
        }
        if reviewed == "no" {
            why.push("no clean review at the head".to_string());
            if let Some(w) = &parent_stale {
                why.push(format!("the reviewed parent is not enough: {w}"));
            }
        }
        return done(
            Kind::Blocked,
            format!("{line} {}: BLOCKED", why.join(", ")),
            reviewed == "no",
        );
    }
    if ms == "DIRTY" {
        return done(Kind::Blocked, format!("{line} conflicts BLOCKED"), false);
    }
    if pr.draft {
        return done(Kind::Blocked, format!("{line} draft BLOCKED"), false);
    }
    let mut carried = None;
    if rules.receipt && !opts.tool_made {
        let authors = need!(f.authors, Need::Authors);
        let receipts = need!(f.receipts, Need::Receipts);
        let receipt_head = if repo == RECEIPT_CARRY_REPO {
            head
        } else {
            asked
        };
        match receipt_check(authors, receipts, receipt_head) {
            Ok(r) => line.push_str(&format!(
                " receipt {}/{}",
                r.receipt.reviewer, r.receipt.family
            )),
            Err((why, retry)) => {
                if repo != RECEIPT_CARRY_REPO {
                    return done(Kind::Blocked, format!("{line} {why} BLOCKED"), retry);
                }
                let verdicts = carry_verdicts(f);
                if verdicts.get(head) == Some(&Verdict::Findings) {
                    return done(
                        Kind::Blocked,
                        format!("{line} ROLE-11: the receipts at the head have findings BLOCKED"),
                        false,
                    );
                }
                if !verdicts.values().any(|v| *v == Verdict::Clean)
                    || need!(f.parents, Need::Parents).len() != 2
                {
                    return done(Kind::Blocked, format!("{line} {why} BLOCKED"), retry);
                }
                if reviewed != "head" {
                    if let Some(step) = bot_pending(&line) {
                        return step;
                    }
                    return done(
                        Kind::Blocked,
                        format!(
                            "{line} ROLE-11: receipt carry needs a clean bot review at the head BLOCKED"
                        ),
                        true,
                    );
                }
                match need!(f.receipt_carry, Need::ReceiptCarry) {
                    Ok(Some(from)) => carried = Some(from),
                    Ok(None) => return done(Kind::Blocked, format!("{line} {why} BLOCKED"), retry),
                    Err(e) => {
                        return done(
                            Kind::Blocked,
                            format!(
                                "{line} ROLE-11: receipt carry could not be checked ({e}) BLOCKED"
                            ),
                            false,
                        );
                    }
                }
            }
        }
    }
    if !rules.verdicts.is_empty() {
        let v = need!(f.verdicts, Need::Verdicts);
        let failed: Vec<&str> = v
            .iter()
            .filter_map(|(_, r)| match r {
                VerdictState::Fail(d) => Some(d.as_str()),
                _ => None,
            })
            .collect();
        if !failed.is_empty() {
            return done(
                Kind::Blocked,
                format!("{line} local build failed: {} BLOCKED", failed.join(";")),
                false,
            );
        }
        let not_run: Vec<String> = v
            .iter()
            .filter_map(|(n, r)| match r {
                VerdictState::NotRun(d) => Some(format!("{n} {d}")),
                _ => None,
            })
            .collect();
        if !not_run.is_empty() {
            return done(
                Kind::Blocked,
                format!("{line} local build not run: {} BLOCKED", not_run.join("; ")),
                false,
            );
        }
        let pending: Vec<(&str, Option<&str>)> = v
            .iter()
            .filter_map(|(n, r)| match r {
                VerdictState::Building(d) => Some((n.as_str(), Some(d.as_str()))),
                VerdictState::Missing(d) => Some((n.as_str(), d.as_deref())),
                _ => None,
            })
            .collect();
        if !pending.is_empty() || v.len() < rules.verdicts.len() {
            let names: Vec<&str> = pending.iter().map(|p| p.0).collect();
            let details: Vec<String> = pending
                .iter()
                .filter_map(|(n, d)| d.map(|d| format!("{n}: {d}")))
                .collect();
            let details = if details.is_empty() {
                String::new()
            } else {
                format!(" [{}]", details.join("; "))
            };
            return done(
                Kind::Wait,
                format!(
                    "{line} no-local-build-verdict ({}){details} WAIT",
                    names.join(",")
                ),
                true,
            );
        }
        // The verdicts are in addition to GitHub's checks (ROLE-20), judged below; the
        // build ran on the present `main`, so the branch is not brought up to date.
        line.push_str(&format!(" local-build-pass-on-main-{}", short(main)));
        behind = 0;
    }
    if behind != 0 {
        let mine = need!(f.mine, Need::CompareFiles);
        let theirs = need!(f.theirs, Need::CompareFiles);
        match moved_in(mine, theirs, &rules.global_paths) {
            None => {
                line.push_str(" behind-in-untouched-files-only");
                behind = 0;
            }
            Some(w) => line.push_str(&format!(" main-moved-in: {w}")),
        }
    }
    if behind != 0 && opts.no_update {
        return done(Kind::Behind, format!("{line} BEHIND"), false);
    }
    if behind != 0 {
        return Step::Update(line);
    }
    // ROLE-20. The merge bypasses the rulesets (`merge_by_admin` leaves every pull request
    // `BLOCKED`), so GitHub's `mergeable_state` decides nothing here and the gate judges
    // the checks itself.
    let checks = need!(f.checks, Need::Checks);
    let failed: Vec<&str> = checks
        .iter()
        .filter(|c| c.failed())
        .map(|c| c.name.as_str())
        .collect();
    if !failed.is_empty() {
        let what = if rules.verdicts.is_empty() {
            "checks failed"
        } else {
            "local build passed but GitHub failed"
        };
        return done(
            Kind::Blocked,
            format!("{line} {what}: {} BLOCKED", failed.join(",")),
            false,
        );
    }
    if pr.mergeable != Some(true) {
        let m = match pr.mergeable {
            None => "not computed yet",
            _ => "false",
        };
        return done(Kind::Wait, format!("{line} mergeable {m} WAIT"), true);
    }
    let passed = |n: &str| checks.iter().any(|c| c.name == n && c.passed());
    let missing: Vec<&str> = rules
        .required_checks
        .iter()
        .filter(|n| !checks.iter().any(|c| &c.name == *n))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return done(
            Kind::Wait,
            format!("{line} required-checks-missing: {} WAIT", missing.join(",")),
            true,
        );
    }
    // Only relaxed paths changed (some did) and every `relax_required` check passed: the
    // `relax_optional` checks are not waited for. Every other check still is.
    let mut relaxed = false;
    if rules.relax_configured() {
        let files = need!(f.pr_files, Need::PrFiles);
        relaxed = !files.is_empty()
            && files
                .iter()
                .all(|f| rules.relax_paths.iter().any(|r| r.is_match(&f.name)))
            && rules.relax_required.iter().all(|n| passed(n));
    }
    let optional = |n: &str| relaxed && rules.relax_optional.iter().any(|r| r.is_match(n));
    let open: Vec<&str> = checks
        .iter()
        .filter(|c| !c.passed() && !optional(&c.name))
        .map(|c| c.name.as_str())
        .collect();
    if !open.is_empty() {
        return done(
            Kind::Wait,
            format!("{line} checks-not-done: {} WAIT", open.join(",")),
            true,
        );
    }
    if relaxed {
        line.push_str(" relaxed-paths-only:not-waiting-for-optional-checks");
    }
    line.push_str(&format!(
        " checks {}/{} passed",
        checks.iter().filter(|c| c.passed()).count(),
        checks.len()
    ));
    if let Some(from) = carried {
        line.push_str(&format!(
            " receipt carried from {from} (conflict-free merge)"
        ));
    }
    let files = need!(f.pr_files, Need::PrFiles);
    let diff = need!(f.diff, Need::Diff);
    let bad_files: Vec<&str> = files
        .iter()
        .filter(|f| {
            f.status != "removed" && rules.hygiene_files.iter().any(|r| r.is_match(&f.name))
        })
        .take(5)
        .map(|f| f.name.as_str())
        .collect();
    let mut bad_lines = match diff {
        Ok(d) => lines_matching(d, &rules.hygiene_lines),
        Err(e) if !opts.hygiene_read => {
            return done(
                Kind::Blocked,
                format!("{line} hygiene: diff not readable ({e}) BLOCKED"),
                false,
            );
        }
        Err(_) => 0,
    };
    if bad_lines != 0 && opts.hygiene_read {
        line.push_str(&format!(" hygiene-lines-read:{bad_lines}"));
        bad_lines = 0;
    }
    if !bad_files.is_empty() || bad_lines != 0 {
        let line = format!(
            "{line} hygiene: files [{}] lines-with-local-path {bad_lines} BLOCKED",
            bad_files.join(" ")
        );
        return done(Kind::Blocked, line, false);
    }
    Step::Merge(line)
}

/// What `main` gained (`theirs`) that a change (`mine`) must be revalidated for: files both
/// touch and global paths, at most three of each; also when either list is not known whole
/// (the API lists at most 300) or is empty. `None`: nothing.
fn moved_in(mine: &FileList, theirs: &FileList, global_paths: &[String]) -> Option<String> {
    let mut overlap: String = mine
        .files
        .iter()
        .filter(|m| theirs.files.contains(m))
        .take(3)
        .map(|m| format!("{m} "))
        .collect();
    let global: String = theirs
        .files
        .iter()
        .filter(|t| global_paths.iter().any(|g| t.starts_with(g.as_str())))
        .take(3)
        .map(|t| format!("{t} "))
        .collect();
    if mine.files.is_empty() || theirs.files.is_empty() || !mine.whole || !theirs.whole {
        overlap.push_str("(file lists not known whole) ");
    }
    let w = format!("{overlap}{global}");
    (!w.is_empty()).then(|| w.trim_end().to_string())
}

fn independent_clean(r: &SeenReceipt, authors: &[Family]) -> bool {
    r.receipt.reviewer.as_str() == r.sender
        && r.receipt.verdict == Verdict::Clean
        && !authors.contains(&r.receipt.family)
}

/// An independent clean receipt clears findings at the same commit.
fn carry_verdicts(f: &Facts) -> BTreeMap<&str, Verdict> {
    let mut verdicts = BTreeMap::new();
    let (Some(Ok(authors)), Some(Ok(receipts))) = (&f.authors, &f.receipts) else {
        return verdicts;
    };
    if authors.is_empty() {
        return verdicts;
    }
    for r in receipts {
        let head = r.receipt.head.as_str();
        if r.receipt.reviewer.as_str() != r.sender || hgl_ai_proto::check_head(head).is_err() {
            continue;
        }
        if independent_clean(r, authors) {
            verdicts.insert(head, Verdict::Clean);
        } else if r.receipt.verdict == Verdict::Findings {
            verdicts.entry(head).or_insert(Verdict::Findings);
        }
    }
    verdicts
}

/// ROLE-11 over what was read: a clean receipt for the given head, sent by the reviewer it
/// names, from a family none of the authors used. Else why not, and whether waiting may
/// bring one.
fn receipt_check<'a>(
    authors: &std::result::Result<Vec<Family>, String>,
    receipts: &'a std::result::Result<Vec<SeenReceipt>, String>,
    head: &str,
) -> std::result::Result<&'a SeenReceipt, (String, bool)> {
    let authors = match authors {
        Ok(a) if a.is_empty() => {
            return Err((
                "ROLE-11: no author family known (no implementor tree of the task records one); a receipt cannot be checked".into(),
                false,
            ));
        }
        Ok(a) => a,
        Err(e) => {
            return Err((
                format!("ROLE-11: the authors' families not read ({e})"),
                false,
            ));
        }
    };
    let receipts = match receipts {
        Ok(r) => r,
        Err(e) => return Err((format!("ROLE-11: receipts not read ({e})"), false)),
    };
    let at_head: Vec<&SeenReceipt> = receipts
        .iter()
        .filter(|r| r.receipt.head == head && r.receipt.reviewer.as_str() == r.sender)
        .collect();
    let names = |fs: &[Family]| fs.iter().map(|f| f.as_str()).collect::<Vec<_>>().join(",");
    if let Some(r) = at_head.iter().rev().find(|r| independent_clean(r, authors)) {
        return Ok(r);
    }
    let why = if at_head.iter().any(|r| r.receipt.verdict == Verdict::Clean) {
        format!(
            "ROLE-11: the clean receipts at the head are from the authors' family ({}); another family must review",
            names(authors)
        )
    } else if !at_head.is_empty() {
        "ROLE-11: the receipts at the head have findings".to_string()
    } else {
        format!(
            "ROLE-11: no clean review receipt at the head from a family other than the authors' ({})",
            names(authors)
        )
    };
    Err((why, at_head.is_empty()))
}

const THREADS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!, $endCursor: String) {
  repository(owner: $owner, name: $name) { pullRequest(number: $number) {
    reviewThreads(first: 100, after: $endCursor) { pageInfo { hasNextPage endCursor } nodes { isResolved } }
  } }
}";

/// Unresolved threads over every page of the threads query.
pub fn unresolved_threads(pages: &[Value]) -> u64 {
    pages
        .iter()
        .flat_map(|p| {
            p.pointer("/data/repository/pullRequest/reviewThreads/nodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter(|t| t.get("isResolved").and_then(Value::as_bool) == Some(false))
        .count() as u64
}

/// The pull request as `pulls/N` gives it.
pub async fn fetch_pr<G: Gh>(gh: &G, s: &Settings, repo: &str, number: u64) -> Result<Pr> {
    Pr::from_rest(&exec::get(gh, &format!("repos/{}/pulls/{number}", s.full(repo))).await?)
}

/// How many times, and how far apart, a merge GitHub reported done is looked for on the
/// pull request, which may lag behind.
const READ_BACK: (u32, std::time::Duration) = (5, std::time::Duration::from_secs(2));

/// The pull request after a merge; when the merge was reported done, read again a few
/// times until it shows merged.
async fn read_back<G: Gh>(
    gh: &G,
    s: &Settings,
    repo: &str,
    number: u64,
    reported: bool,
) -> Result<Pr> {
    let mut pr = fetch_pr(gh, s, repo, number).await?;
    let mut left = if reported { READ_BACK.0 } else { 0 };
    while !pr.merged && left > 0 {
        tokio::time::sleep(READ_BACK.1).await;
        pr = fetch_pr(gh, s, repo, number).await?;
        left -= 1;
    }
    Ok(pr)
}

/// The head of `main` of a repository.
pub async fn main_head<G: Gh>(gh: &G, s: &Settings, repo: &str) -> Result<String> {
    let v = exec::get(gh, &format!("repos/{}/git/ref/heads/main", s.full(repo))).await?;
    v.pointer("/object/sha")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| resp("git/ref/heads/main", "no object.sha"))
}

/// Notes read per pass over the controller's notes for receipts.
const RECEIPT_WINDOW: usize = 1000;

/// The review receipts for `repo` #`pr` among the controller's notes on the hub (HGL_NOTES
/// keeps 14 days): `receipt` notes whose body holds PROTO-19 JSON for this pull request.
pub async fn receipts(bus: &Bus, s: &Settings, repo: &str, pr: u64) -> Result<Vec<SeenReceipt>> {
    let mut out = Vec::new();
    let mut from = 1;
    loop {
        let (got, last) = bus
            .read_page(std::slice::from_ref(&s.controller), from, RECEIPT_WINDOW)
            .await?;
        // From after the last message read, a malformed one skipped included.
        let Some(last) = last else {
            return Ok(out);
        };
        from = last + 1;
        for sn in got {
            if sn.note.kind != NoteKind::Receipt {
                continue;
            }
            if let Some(r) = state::parse_receipt(&sn.note.body)
                && r.repo == repo
                && r.pr == pr
            {
                out.push(SeenReceipt {
                    sender: sn.note.sender.to_string(),
                    receipt: r,
                });
            }
        }
    }
}

const NO_HUB: &str = "no hub connection";

#[allow(clippy::too_many_arguments)]
async fn fill<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: Option<&Bus>,
    repo: &str,
    f: &mut Facts,
    need: Need,
    issue: u64,
    builds: Option<&GateBuilds>,
) -> Result<()> {
    let r = s.full(repo);
    let n = f.pr.number;
    let head = f.pr.head.clone();
    match need {
        Need::Main => f.main = Some(main_head(gh, s, repo).await?),
        Need::Unresolved => {
            let vars = [
                ("owner", s.owner.clone()),
                ("name", repo.to_string()),
                ("number", n.to_string()),
            ];
            // `number` is an Int: gh sends `-f` values as strings, so it goes in the query.
            let q = THREADS_QUERY
                .replace("$number: Int!, ", "")
                .replace("$number", &n.to_string());
            let pages = exec::graphql(gh, &q, &vars[..2], true).await?;
            f.unresolved = Some(unresolved_threads(&pages));
        }
        Need::Behind => {
            let v = exec::get(gh, &format!("repos/{r}/compare/{head}...main")).await?;
            f.behind = Some(
                v.get("ahead_by")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| resp("compare", "no ahead_by"))?,
            );
        }
        Need::BotComments => {
            let list =
                exec::get_list(gh, &format!("repos/{r}/issues/{n}/comments?per_page=100")).await?;
            let comments: Vec<Comment> = list.iter().map(Comment::from_rest).collect();
            f.bot = Some(BotView::of(&s.review_bot, &comments, &[]));
        }
        Need::BotReviews => {
            let list =
                exec::get_list(gh, &format!("repos/{r}/pulls/{n}/reviews?per_page=100")).await?;
            let reviews: Vec<Review> = list.iter().map(Review::from_rest).collect();
            f.bot_reviews = Some(BotView::of(&s.review_bot, &[], &reviews).reviews);
        }
        Need::Parents => {
            let v = exec::get(gh, &format!("repos/{r}/git/commits/{head}")).await?;
            let parents = v
                .get("parents")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|p| p.get("sha").and_then(Value::as_str).map(String::from))
                .collect();
            f.parents = Some(parents);
        }
        Need::Verdicts => {
            let main = f.main.clone().unwrap_or_default();
            let mut out = Vec::new();
            for name in &s.gate_rules(repo).verdicts {
                let st =
                    crate::builds::verdict_state(bus, builds, repo, n, name, &head, &main).await?;
                out.push((name.clone(), st));
            }
            f.verdicts = Some(out);
        }
        Need::Checks => {
            let pages = exec::get_pages(
                gh,
                &format!("repos/{r}/commits/{head}/check-runs?per_page=100&filter=all"),
            )
            .await?;
            let mut checks = latest_checks(&pages);
            let status =
                exec::get(gh, &format!("repos/{r}/commits/{head}/status?per_page=100")).await?;
            checks.extend(commit_statuses(&status));
            f.checks = Some(checks);
        }
        Need::CompareFiles => {
            f.mine = Some(compare_files(
                &exec::get(gh, &format!("repos/{r}/compare/main...{head}")).await?,
            ));
            f.theirs = Some(compare_files(
                &exec::get(gh, &format!("repos/{r}/compare/{head}...main")).await?,
            ));
        }
        Need::PrFiles => {
            f.pr_files = Some(pr_files(
                &exec::get_list(gh, &format!("repos/{r}/pulls/{n}/files?per_page=100")).await?,
            ));
        }
        Need::Diff => {
            let d = exec::get_raw(
                gh,
                &format!("repos/{r}/pulls/{n}"),
                "application/vnd.github.v3.diff",
            )
            .await;
            f.diff = Some(d.map_err(|e| e.to_string()));
        }
        Need::UpdateFiles => {
            let parent = f
                .parents
                .as_ref()
                .and_then(|p| p.first())
                .cloned()
                .ok_or_else(|| resp("git/commits", "no parent"))?;
            let mine =
                compare_files(&exec::get(gh, &format!("repos/{r}/compare/main...{parent}")).await?);
            let theirs =
                compare_files(&exec::get(gh, &format!("repos/{r}/compare/{parent}...main")).await?);
            f.update_files = Some((mine, theirs));
        }
        Need::Receipts => {
            f.receipts = Some(match bus {
                Some(b) => Ok(receipts(b, s, repo, n).await?),
                None => Err(NO_HUB.into()),
            });
        }
        Need::Authors => {
            f.authors = Some(match bus {
                Some(b) => Ok(state::author_families(
                    &b.trees().await?,
                    repo,
                    &f.pr.branch,
                    issue,
                )),
                None => Err(NO_HUB.into()),
            });
        }
        Need::ReceiptCarry => {
            let main = f
                .main
                .as_deref()
                .ok_or_else(|| resp("receipt carry", "no main"))?;
            f.receipt_carry = Some(
                carry::from_remote(&s.receipt_carry_url, &head, main, &carry_verdicts(f)).await,
            );
        }
    }
    Ok(())
}

/// Run the gate on `repo#number` for the head `asked`. With `act`, bring the branch up to
/// date or merge where the gate says so, and ask `builds` for the local build verdicts it
/// lacks; without, say what it would do. Under a landing lock, the lock is confirmed ours
/// right before the merge ([`LandLock::confirm`]).
#[allow(clippy::too_many_arguments)]
pub async fn evaluate<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: Option<&Bus>,
    repo: &str,
    number: u64,
    asked: &str,
    opts: &Opts,
    act: bool,
    lock: Option<&LandLock>,
    builds: Option<&GateBuilds>,
) -> Result<Outcome> {
    hgl_ai_proto::check_head(asked)?;
    let builds = builds.filter(|_| act);
    let rules = s.gate_rules(repo);
    let mut f = Facts {
        pr: fetch_pr(gh, s, repo, number).await?,
        ..Facts::default()
    };
    let issue = opts
        .closes
        .or_else(|| state::issue_from_branch(&f.pr.branch, &s.branch_prefix))
        .unwrap_or(number);
    loop {
        match decide(&f, repo, asked, &rules, opts) {
            Step::Need(n) => fill(gh, s, bus, repo, &mut f, n, issue, builds).await?,
            Step::Done(o) => return Ok(o),
            Step::Update(line) if !act => {
                return Ok(Outcome::new(
                    Kind::WouldUpdate,
                    format!("{line} WOULD-UPDATE"),
                    true,
                    &f.pr.head,
                ));
            }
            Step::Merge(line) if !act => {
                return Ok(Outcome::new(
                    Kind::WouldMerge,
                    format!("{line} WOULD-MERGE"),
                    false,
                    &f.pr.head,
                ));
            }
            Step::Update(line) => {
                let path = format!("repos/{}/pulls/{number}/update-branch", s.full(repo));
                let res = exec::write(gh, "PUT", &path, &[("expected_head_sha", &f.pr.head)]).await;
                return Ok(match res {
                    Ok(_) => {
                        Outcome::new(Kind::Updated, format!("{line} UPDATED"), true, &f.pr.head)
                    }
                    Err(e) => Outcome::new(
                        Kind::Blocked,
                        format!("{line} update failed ({e}) BLOCKED"),
                        false,
                        &f.pr.head,
                    ),
                });
            }
            Step::Merge(line) => {
                if let Some(l) = lock {
                    l.confirm(&format!("merging {repo} #{number}")).await?;
                }
                // The intent first: a failure from here on leaves a record a retry resumes.
                if let Some(b) = bus {
                    progress::write(b, repo, number, &Progress::new(&f.pr.head, None)).await?;
                }
                // The lease as it is now, just confirmed: not as it was when the landing began.
                let lease = match (lock, bus) {
                    (Some(_), Some(b)) => Some(b.locks_max_age().await?),
                    _ => None,
                };
                let until = merge::until(tokio::time::Instant::now(), opts.merge_until, lease);
                let res = merge::merge(
                    gh,
                    &s.full(repo),
                    number,
                    &f.pr.head,
                    until,
                    merge::POLL_EVERY,
                )
                .await?;
                let after = read_back(gh, s, repo, number, res == Merge::Merged).await?;
                if after.merged {
                    let m = after.merge_commit.clone().unwrap_or_default();
                    if let Some(b) = bus {
                        let p = Progress::new(&f.pr.head, Some(m.clone()));
                        progress::write(b, repo, number, &p).await?;
                    }
                    let mut o = Outcome::new(
                        Kind::Merged,
                        format!("{line} -> MERGED {} MERGED", short(&m)),
                        false,
                        &f.pr.head,
                    );
                    o.merge_commit = Some(m);
                    return Ok(o);
                }
                let why = match res {
                    Merge::Refused(why) => why,
                    // Accepted and maybe still merging: the record stays, and a later
                    // attempt finds the pull request merged or adopts the pending request.
                    Merge::Pending(why) => {
                        return Ok(Outcome::new(
                            Kind::Wait,
                            format!("{line} merge accepted, not done yet ({why}) WAIT"),
                            true,
                            &f.pr.head,
                        ));
                    }
                    Merge::Merged => {
                        return Ok(Outcome::new(
                            Kind::Wait,
                            format!("{line} merge reported done, pull request not merged yet WAIT"),
                            true,
                            &f.pr.head,
                        ));
                    }
                };
                if let Some(b) = bus
                    && let Err(e) = progress::delete(b, repo, number).await
                {
                    eprintln!("{repo} #{number}: removing the landing record: {e}");
                }
                return Ok(Outcome::new(
                    Kind::Blocked,
                    format!("{line} merge refused ({why}) BLOCKED"),
                    false,
                    &f.pr.head,
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::fake::FakeGh;
    use crate::testutil::{fixture, json, settings, without_receipts};

    const HEAD: &str = "1f461fa77bba922fe81383df4c475feb58b7c233";
    const MAIN: &str = "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a";
    const REPO: &str = "alpha";

    fn bot_clean() -> BotView {
        let c: Vec<Comment> = json("comments-clean.json")
            .as_array()
            .unwrap()
            .iter()
            .map(Comment::from_rest)
            .collect();
        BotView::of("chatgpt-codex-connector", &c, &[])
    }

    /// Facts under which the gate merges.
    fn allowed() -> Facts {
        Facts {
            pr: Pr::from_rest(&json("pr-open.json")).unwrap(),
            main: Some(MAIN.into()),
            unresolved: Some(0),
            behind: Some(0),
            bot: Some(bot_clean()),
            bot_reviews: None,
            parents: Some(vec!["p".repeat(40)]),
            verdicts: None,
            checks: Some(vec![]),
            mine: None,
            theirs: None,
            pr_files: Some(pr_files(json("pr-files.json").as_array().unwrap())),
            diff: Some(Ok(fixture("pr.diff"))),
            update_files: None,
            receipts: Some(Ok(vec![seen("r1", receipt())])),
            authors: Some(Ok(vec![Family::Gpt])),
            receipt_carry: None,
        }
    }

    /// A clean review receipt of `alpha#7` at `HEAD` by `r1`, family claude.
    fn receipt() -> ReviewReceipt {
        state::parse_receipt(&fixture("receipt.json")).expect("PROTO-19 JSON")
    }

    fn seen(sender: &str, receipt: ReviewReceipt) -> SeenReceipt {
        SeenReceipt {
            sender: sender.into(),
            receipt,
        }
    }

    fn rules() -> GateRules {
        settings().gate_rules(REPO)
    }

    fn outcome(f: &Facts, rules: &GateRules, opts: &Opts) -> Outcome {
        match decide(f, REPO, HEAD, rules, opts) {
            Step::Done(o) => o,
            other => panic!("not done: {other:?}"),
        }
    }

    fn blocked(f: &Facts, word: &str) {
        let o = outcome(f, &rules(), &Opts::default());
        assert_eq!(o.kind, Kind::Blocked, "{}", o.line);
        assert!(o.line.contains(word), "{}", o.line);
        assert!(o.line.ends_with("BLOCKED"));
    }

    #[test]
    fn rest_shapes() {
        let pr = Pr::from_rest(&json("pr-open.json")).unwrap();
        assert_eq!(
            (pr.number, pr.head.as_str(), pr.mergeable_state.as_str()),
            (7, HEAD, "CLEAN")
        );
        assert_eq!(
            (pr.branch.as_str(), pr.base.as_str(), pr.draft),
            ("codex/980-format-patterns", "main", false)
        );
        let merged = Pr::from_rest(&json("pr-merged.json")).unwrap();
        assert!(merged.merged);
        assert_eq!(
            merged.merge_commit.as_deref(),
            Some("01e99ea5e46457bac035ca8a2699b6b6575b32ed")
        );
        let checks = latest_checks(&[json("check-runs.json")]);
        let lint = checks.iter().find(|c| c.name == "lint").unwrap();
        assert_eq!(
            (lint.id, lint.conclusion.as_deref()),
            (30, Some("success")),
            "the re-run counts"
        );
        assert!(
            checks
                .iter()
                .find(|c| c.name == "test (windows-latest)")
                .unwrap()
                .failed()
        );
        let cmp = compare_files(&json("compare.json"));
        assert_eq!(cmp.files, ["Cargo.lock", "crates/x/src/lib.rs"]);
        assert!(cmp.whole);
        let pages = [json("threads-page1.json"), json("threads-page2.json")];
        assert_eq!(unresolved_threads(&pages), 2);
    }

    #[test]
    fn allowed_case_merges() {
        let f = allowed();
        match decide(&f, REPO, HEAD, &rules(), &Opts::default()) {
            Step::Merge(line) => {
                assert_eq!(
                    line,
                    "alpha #7 codex/980-format-patterns CLEAN head 1f461fa7 unresolved 0 behind 0 reviewed head receipt r1/claude checks 0/0 passed"
                )
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn needs_facts_one_at_a_time() {
        let mut f = allowed();
        f.main = None;
        assert_eq!(
            decide(&f, REPO, HEAD, &rules(), &Opts::default()),
            Step::Need(Need::Main)
        );
        let mut f = allowed();
        f.diff = None;
        assert_eq!(
            decide(&f, REPO, HEAD, &rules(), &Opts::default()),
            Step::Need(Need::Diff)
        );
        let mut f = allowed();
        f.bot = None;
        assert_eq!(
            decide(&f, REPO, HEAD, &rules(), &Opts::default()),
            Step::Need(Need::BotComments)
        );
    }

    #[test]
    fn merged_and_closed() {
        let mut f = allowed();
        f.pr.merged = true;
        let o = outcome(&f, &rules(), &Opts::default());
        assert_eq!(
            (o.kind, o.line.as_str()),
            (Kind::AlreadyMerged, "alpha #7 already MERGED")
        );
        let mut f = allowed();
        f.pr.state = "closed".into();
        blocked(&f, "closed");
    }

    #[test]
    fn head_must_be_exact() {
        let mut f = allowed();
        f.pr.head = "e".repeat(40);
        f.parents = Some(vec!["d".repeat(40), MAIN.into()]);
        blocked(&f, "head moved");
        // The update merge of the asked head onto the present main is the same head, when
        // main moved only in files it does not touch.
        f.parents = Some(vec![HEAD.into(), MAIN.into()]);
        f.bot = Some(bot_clean());
        assert_eq!(
            decide(&f, REPO, HEAD, &rules(), &Opts::default()),
            Step::Need(Need::UpdateFiles)
        );
        f.update_files = Some((
            compare_files(&json("compare.json")),
            compare_files(&json("compare-docs.json")),
        ));
        match decide(&f, REPO, HEAD, &rules(), &Opts::default()) {
            Step::Merge(line) => assert!(line.contains("reviewed parent"), "{line}"),
            other => panic!("{other:?}"),
        }
        // Onto a main that has moved since, it is not.
        f.parents = Some(vec![HEAD.into(), "9".repeat(40)]);
        blocked(&f, "head moved");
    }

    /// Carbon's check 5: a branch brought up to date onto a `main` that moved in a file it
    /// touches, or in a global path, is not reviewed by its parent's clean review.
    #[test]
    fn a_reviewed_parent_does_not_cover_main_moving_under_it() {
        let rules = settings().gate_rules("hgl_spec");
        let mut f = allowed();
        f.pr.head = "3".repeat(40);
        f.parents = Some(vec![HEAD.into(), MAIN.into()]);
        let mine = FileList {
            files: vec!["spec/rules.md".into()],
            whole: true,
        };
        let check = |f: &Facts, theirs: &[&str], whole: bool| {
            let mut f = f.clone();
            f.update_files = Some((
                mine.clone(),
                FileList {
                    files: theirs.iter().map(|t| t.to_string()).collect(),
                    whole,
                },
            ));
            decide(&f, "hgl_spec", HEAD, &rules, &Opts::default())
        };
        for (theirs, whole, word) in [
            (&["spec/rules.md"][..], true, "spec/rules.md"),
            (&["Cargo.toml"][..], true, "Cargo.toml"),
            (&["docs/y.md"][..], false, "not known whole"),
        ] {
            match check(&f, theirs, whole) {
                Step::Done(o) => {
                    assert_eq!(o.kind, Kind::Blocked, "{}", o.line);
                    assert!(o.retry, "a review at the head may still come");
                    assert!(
                        o.line.contains("reviewed no")
                            && o.line
                                .contains("the reviewed parent is not enough: main moved in")
                            && o.line.contains(word),
                        "{}",
                        o.line
                    );
                }
                s => panic!("{theirs:?}: {s:?}"),
            }
        }
        assert!(
            matches!(check(&f, &["docs/y.md"], true), Step::Merge(l) if l.contains("reviewed parent"))
        );
        // A review at the merge head itself is the head's.
        let mut at = f.clone();
        let c: Vec<Comment> = json("comments-clean.json")
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                let mut c = Comment::from_rest(v);
                c.body = c
                    .body
                    .replace("1f461fa77bba922fe81383df4c475feb58b7c233", &"3".repeat(40));
                c.body = c.body.replace("1f461fa77b", "3333333333");
                c
            })
            .collect();
        at.bot = Some(BotView::of("chatgpt-codex-connector", &c, &[]));
        assert!(
            matches!(decide(&at, "hgl_spec", HEAD, &rules, &Opts::default()), Step::Merge(l) if l.contains("reviewed head"))
        );
    }

    /// ROLE-11: a clean receipt for the head asked, from a family none of the authors used.
    #[test]
    fn a_merge_needs_a_clean_receipt_from_another_family() {
        let r = rules();
        assert!(r.receipt, "required by default");
        let mut f = allowed();
        f.receipts = None;
        f.authors = None;
        assert_eq!(
            decide(&f, REPO, HEAD, &r, &Opts::default()),
            Step::Need(Need::Authors)
        );
        f.authors = Some(Ok(vec![Family::Gpt]));
        assert_eq!(
            decide(&f, REPO, HEAD, &r, &Opts::default()),
            Step::Need(Need::Receipts)
        );
        let refused = |f: &Facts| outcome(f, &r, &Opts::default());
        // None at all: a receipt may still come.
        f.receipts = Some(Ok(vec![]));
        let o = refused(&f);
        assert!(o.kind == Kind::Blocked && o.retry, "{}", o.line);
        assert!(
            o.line.contains(
                "no clean review receipt at the head from a family other than the authors' (gpt)"
            ),
            "{}",
            o.line
        );
        // From the authors' family: never enough.
        let mut gpt = receipt();
        gpt.family = Family::Gpt;
        f.receipts = Some(Ok(vec![seen("r1", gpt)]));
        let o = refused(&f);
        assert!(
            !o.retry && o.line.contains("from the authors' family (gpt)"),
            "{}",
            o.line
        );
        // A family the task changed to after a failover is an author too.
        f.receipts = Some(Ok(vec![seen("r1", receipt())]));
        f.authors = Some(Ok(vec![Family::Claude, Family::Gpt]));
        assert!(refused(&f).line.contains("authors' family (claude,gpt)"));
        f.authors = Some(Ok(vec![Family::Gpt]));
        // Findings, another head, or a receipt naming a reviewer other than its sender.
        let mut bad = receipt();
        bad.verdict = Verdict::Findings;
        f.receipts = Some(Ok(vec![seen("r1", bad)]));
        assert!(refused(&f).line.contains("have findings"));
        let mut old = receipt();
        old.head = "9".repeat(40);
        f.receipts = Some(Ok(vec![seen("r1", old)]));
        assert!(refused(&f).retry);
        f.receipts = Some(Ok(vec![seen("w1", receipt())]));
        assert!(refused(&f).line.contains("no clean review receipt"));
        // A later clean one counts.
        let mut bad = receipt();
        bad.verdict = Verdict::Findings;
        f.receipts = Some(Ok(vec![seen("r1", bad), seen("r1", receipt())]));
        assert!(matches!(
            decide(&f, REPO, HEAD, &r, &Opts::default()),
            Step::Merge(_)
        ));
        // No author family known, or nothing read from the hub: blocked, saying so.
        f.authors = Some(Ok(vec![]));
        assert!(refused(&f).line.contains("no author family known"));
        f.authors = Some(Err(NO_HUB.into()));
        assert!(
            refused(&f)
                .line
                .contains("families not read (no hub connection)")
        );
        f.authors = Some(Ok(vec![Family::Gpt]));
        f.receipts = Some(Err(NO_HUB.into()));
        assert!(
            refused(&f)
                .line
                .contains("receipts not read (no hub connection)")
        );
        // Turned off for the repository, or a pull request the tool made: not asked.
        f.receipts = None;
        f.authors = None;
        let off = without_receipts(settings(), &[REPO]).gate_rules(REPO);
        assert!(
            matches!(decide(&f, REPO, HEAD, &off, &Opts::default()), Step::Merge(l) if !l.contains("receipt"))
        );
        let tool = Opts {
            tool_made: true,
            ..Opts::default()
        };
        assert!(matches!(decide(&f, REPO, HEAD, &r, &tool), Step::Merge(_)));
    }

    #[test]
    fn carried_receipt_needs_head_checks_and_a_clean_head_review() {
        let mut f = allowed();
        let head = "a".repeat(40);
        f.pr.head.clone_from(&head);
        f.parents = Some(vec![HEAD.into(), MAIN.into()]);
        f.update_files = Some((
            compare_files(&json("compare.json")),
            compare_files(&json("compare-docs.json")),
        ));
        let mut rules = settings().gate_rules(RECEIPT_CARRY_REPO);
        rules.required_checks = vec!["lint".into()];
        let step =
            |f: &Facts, asked, opts: &Opts| decide(f, RECEIPT_CARRY_REPO, asked, &rules, opts);
        let normal = Opts::default();

        // The existing parent-bot shortcut cannot authorize receipt carry.
        assert!(matches!(
            step(&f, HEAD, &normal),
            Step::Done(o) if o.kind == Kind::Blocked && o.retry
                && o.line.contains("receipt carry needs a clean bot review at the head")
        ));
        for (file, kind, retry) in [
            ("comments-limit.json", Kind::BotLimit, false),
            ("comments-running.json", Kind::Wait, true),
        ] {
            let body = fixture(file).replace("9a8b7c6d5e4f30211203f4e5d6c7b8a9a0b1c2d3", &head);
            let mut comments: Vec<Value> = serde_json::from_str(&body).unwrap();
            comments.extend(
                json("comments-clean.json")
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned(),
            );
            let comments: Vec<Comment> = comments.iter().map(Comment::from_rest).collect();
            f.bot = Some(BotView::of("chatgpt-codex-connector", &comments, &[]));
            assert!(matches!(
                step(&f, HEAD, &normal),
                Step::Done(o) if o.kind == kind && o.retry == retry && o.line.contains("reviewed parent")
            ));
        }
        f.bot = Some(BotView::default());
        f.bot_reviews = Some(vec![Review {
            author: "b".into(),
            commit: Some(head.clone()),
            state: "COMMENTED".into(),
            url: None,
        }]);
        let triaged = Opts {
            triaged: true,
            ..Opts::default()
        };
        assert!(matches!(
            step(&f, &head, &triaged),
            Step::Done(o) if o.kind == Kind::Blocked && o.line.contains("reviewed triaged")
        ));

        let body = fixture("comments-clean.json")
            .replace(HEAD, &head)
            .replace(&HEAD[..10], &head[..10]);
        let comments: Vec<Value> = serde_json::from_str(&body).unwrap();
        let comments: Vec<Comment> = comments.iter().map(Comment::from_rest).collect();
        f.bot = Some(BotView::of("chatgpt-codex-connector", &comments, &[]));
        assert_eq!(step(&f, &head, &normal), Step::Need(Need::ReceiptCarry));
        f.receipt_carry = Some(Ok(None));
        assert!(matches!(
            step(&f, HEAD, &normal),
            Step::Done(o) if o.kind == Kind::Blocked && o.line.contains("no clean review receipt")
        ));

        f.receipt_carry = Some(Ok(Some(HEAD.into())));
        assert!(matches!(
            step(&f, &head, &normal),
            Step::Done(o) if o.kind == Kind::Wait && o.line.contains("required-checks-missing: lint")
                && !o.line.contains("receipt carried from")
        ));
        f.checks = Some(vec![CheckRun {
            id: 1,
            name: "lint".into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
        }]);
        for asked in [head.as_str(), HEAD] {
            assert!(matches!(
                step(&f, asked, &normal),
                Step::Merge(l) if l.contains(&format!("receipt carried from {HEAD} (conflict-free merge)"))
            ));
        }
        f.authors = Some(Ok(vec![Family::Gpt, Family::Claude]));
        assert!(matches!(
            step(&f, &head, &normal),
            Step::Done(o) if o.kind == Kind::Blocked && o.line.contains("no clean review receipt")
        ));
        f.authors = Some(Ok(vec![Family::Gpt]));

        f.parents = Some(vec![HEAD.into()]);
        assert!(matches!(
            step(&f, &head, &normal),
            Step::Done(o) if o.kind == Kind::Blocked && o.line.contains("no clean review receipt")
        ));
        let mut direct = receipt();
        direct.head.clone_from(&head);
        f.receipts = Some(Ok(vec![seen("r1", direct)]));
        assert!(matches!(
            step(&f, &head, &normal),
            Step::Merge(l) if l.contains("receipt r1/claude") && !l.contains("receipt carried from")
        ));
    }

    #[test]
    fn review_refusals() {
        let mut f = allowed();
        f.bot = Some(BotView::default());
        let o = outcome(&f, &rules(), &Opts::default());
        assert_eq!(o.kind, Kind::Blocked);
        assert!(o.retry, "a review may still come");
        assert!(
            o.line.contains("reviewed no") && o.line.contains("no clean review"),
            "{}",
            o.line
        );

        let limited: Vec<Comment> = json("comments-limit.json")
            .as_array()
            .unwrap()
            .iter()
            .map(Comment::from_rest)
            .collect();
        f.bot = Some(BotView::of("chatgpt-codex-connector", &limited, &[]));
        let o = outcome(&f, &rules(), &Opts::default());
        assert_eq!(o.kind, Kind::BotLimit);
        assert!(o.line.ends_with("review bot out of quota BOTLIMIT"));

        let mut r = allowed();
        let running: Vec<Comment> = json("comments-running.json")
            .as_array()
            .unwrap()
            .iter()
            .map(Comment::from_rest)
            .collect();
        r.pr.head = "9a8b7c6d5e4f30211203f4e5d6c7b8a9a0b1c2d3".into();
        r.bot = Some(BotView::of("chatgpt-codex-connector", &running, &[]));
        let o = match decide(&r, REPO, &r.pr.head.clone(), &rules(), &Opts::default()) {
            Step::Done(o) => o,
            s => panic!("{s:?}"),
        };
        assert_eq!(o.kind, Kind::Wait);
        assert!(o.line.ends_with("review-running WAIT"));
    }

    #[test]
    fn triaged_needs_a_bot_review_at_the_head() {
        let mut f = allowed();
        f.bot = Some(BotView::default());
        let opts = Opts {
            triaged: true,
            ..Opts::default()
        };
        assert_eq!(
            decide(&f, REPO, HEAD, &rules(), &opts),
            Step::Need(Need::BotReviews)
        );
        f.bot_reviews = Some(vec![]);
        assert_eq!(outcome(&f, &rules(), &opts).kind, Kind::Blocked);
        f.bot_reviews = Some(vec![Review {
            author: "b".into(),
            commit: Some(HEAD.into()),
            state: "COMMENTED".into(),
            url: None,
        }]);
        assert!(
            matches!(decide(&f, REPO, HEAD, &rules(), &opts), Step::Merge(l) if l.contains("reviewed triaged"))
        );
    }

    #[test]
    fn plain_refusals() {
        let mut f = allowed();
        f.unresolved = Some(2);
        let o = outcome(&f, &rules(), &Opts::default());
        assert!(
            o.line.contains("unresolved 2") && o.line.contains("unresolved threads") && !o.retry,
            "{}",
            o.line
        );
        let mut f = allowed();
        f.pr.base = "release".into();
        blocked(&f, "base release");
        let mut f = allowed();
        f.pr.mergeable_state = "DIRTY".into();
        blocked(&f, "conflicts");
        let mut f = allowed();
        f.pr.draft = true;
        blocked(&f, "draft");
    }

    #[test]
    fn checks_not_green() {
        let mut f = allowed();
        f.pr.mergeable_state = "UNSTABLE".into();
        f.checks = None;
        assert_eq!(
            decide(&f, REPO, HEAD, &rules(), &Opts::default()),
            Step::Need(Need::Checks)
        );
        f.checks = Some(vec![CheckRun {
            id: 1,
            name: "lint".into(),
            status: "in_progress".into(),
            conclusion: None,
        }]);
        let o = outcome(&f, &rules(), &Opts::default());
        assert_eq!(o.kind, Kind::Wait);
        assert!(o.retry);
        f.checks = Some(latest_checks(&[json("check-runs.json")]));
        let o = outcome(&f, &rules(), &Opts::default());
        assert_eq!(o.kind, Kind::Blocked);
        assert!(
            o.line.contains("checks failed: test (windows-latest)"),
            "{}",
            o.line
        );
        // Not computed yet (`UNKNOWN`, `mergeable` null): wait.
        f.pr.mergeable_state = "UNKNOWN".into();
        f.pr.mergeable = None;
        f.checks = Some(vec![]);
        let o = outcome(&f, &rules(), &Opts::default());
        assert!(
            o.kind == Kind::Wait && o.line.contains("mergeable not computed yet"),
            "{}",
            o.line
        );
    }

    #[test]
    fn behind_main() {
        let mut f = allowed();
        f.behind = Some(3);
        assert_eq!(
            decide(&f, REPO, HEAD, &rules(), &Opts::default()),
            Step::Need(Need::CompareFiles)
        );
        // Main moved in files the pull request does not touch, and in no global path.
        f.mine = Some(FileList {
            files: vec!["crates/x/src/lib.rs".into()],
            whole: true,
        });
        f.theirs = Some(FileList {
            files: vec!["docs/y.md".into()],
            whole: true,
        });
        assert!(
            matches!(decide(&f, REPO, HEAD, &rules(), &Opts::default()), Step::Merge(l) if l.contains("behind-in-untouched-files-only"))
        );
        // In a global path: bring it up to date.
        f.theirs = Some(FileList {
            files: vec!["Cargo.lock".into()],
            whole: true,
        });
        match decide(&f, REPO, HEAD, &rules(), &Opts::default()) {
            Step::Update(l) => assert!(l.ends_with("main-moved-in: Cargo.lock"), "{l}"),
            s => panic!("{s:?}"),
        }
        // In a file it touches, or lists not known whole.
        f.theirs = Some(FileList {
            files: vec!["crates/x/src/lib.rs".into()],
            whole: true,
        });
        assert!(matches!(
            decide(&f, REPO, HEAD, &rules(), &Opts::default()),
            Step::Update(_)
        ));
        f.theirs = Some(FileList {
            files: vec!["docs/y.md".into()],
            whole: false,
        });
        assert!(
            matches!(decide(&f, REPO, HEAD, &rules(), &Opts::default()), Step::Update(l) if l.contains("not known whole"))
        );
        // `--no-update` says so instead.
        let o = outcome(
            &f,
            &rules(),
            &Opts {
                no_update: true,
                ..Opts::default()
            },
        );
        assert_eq!(o.kind, Kind::Behind);
        assert!(o.line.ends_with("BEHIND"));
        // A repository's own global paths count too.
        let r = settings().gate_rules("hgl-compiler");
        assert!(
            r.global_paths
                .iter()
                .any(|g| "spec/hgl_core".starts_with(g.as_str()))
        );
    }

    fn verdict(name: &str, pass: bool) -> VerdictResult {
        let v = format!("{HEAD} {MAIN} {}", if pass { "pass" } else { "fail" });
        (
            name.into(),
            if pass {
                VerdictState::Pass(v)
            } else {
                VerdictState::Fail(v)
            },
        )
    }

    #[test]
    fn local_build_verdicts() {
        let r = settings().gate_rules("hgl-compiler");
        let mut f = allowed();
        f.pr.mergeable_state = "BLOCKED".into();
        f.behind = Some(4);
        f.checks = None;
        assert_eq!(
            decide(&f, REPO, HEAD, &r, &Opts::default()),
            Step::Need(Need::Verdicts)
        );
        f.verdicts = Some(vec![
            verdict("linux", true),
            ("mac".into(), VerdictState::Missing(None)),
            verdict("windows", true),
        ]);
        let o = outcome(&f, &r, &Opts::default());
        assert_eq!(o.kind, Kind::Wait);
        assert!(
            o.line.contains("no-local-build-verdict (mac)"),
            "{}",
            o.line
        );
        // Asked for, or being built: wait, saying where.
        f.verdicts = Some(vec![
            verdict("linux", true),
            (
                "mac".into(),
                VerdictState::Building("asked 20261127-083000-3fa9c1 of mac1".into()),
            ),
            (
                "windows".into(),
                VerdictState::Missing(Some("for main 0b0b0b0b".into())),
            ),
        ]);
        let o = outcome(&f, &r, &Opts::default());
        assert!(o.kind == Kind::Wait && o.retry, "{}", o.line);
        assert!(
            o.line.contains(
                "no-local-build-verdict (mac,windows) [mac: asked 20261127-083000-3fa9c1 of mac1; windows: for main 0b0b0b0b] WAIT"
            ),
            "{}",
            o.line
        );
        // A gate build that did not judge the code stops the landing: nothing to wait for.
        f.verdicts = Some(vec![
            verdict("linux", true),
            (
                "mac".into(),
                VerdictState::NotRun("20261127-083000-3fa9c1 on mac1: no recipe".into()),
            ),
            verdict("windows", true),
        ]);
        let o = outcome(&f, &r, &Opts::default());
        assert!(
            o.kind == Kind::Blocked
                && !o.retry
                && o.line
                    .contains("local build not run: mac 20261127-083000-3fa9c1 on mac1: no recipe"),
            "{}",
            o.line
        );
        f.verdicts = Some(vec![
            verdict("linux", true),
            verdict("mac", false),
            verdict("windows", true),
        ]);
        let o = outcome(&f, &r, &Opts::default());
        assert!(
            o.kind == Kind::Blocked && o.line.contains("local build failed"),
            "{}",
            o.line
        );
        f.verdicts = Some(vec![
            verdict("linux", true),
            verdict("mac", true),
            verdict("windows", true),
        ]);
        assert_eq!(
            decide(&f, REPO, HEAD, &r, &Opts::default()),
            Step::Need(Need::Checks)
        );
        f.checks = Some(latest_checks(&[json("check-runs.json")]));
        let o = outcome(&f, &r, &Opts::default());
        assert!(
            o.line
                .contains("local build passed but GitHub failed: test (windows-latest)"),
            "{}",
            o.line
        );
        let wait = |f: &Facts, word: &str| {
            let o = outcome(f, &r, &Opts::default());
            assert_eq!((o.kind, o.retry), (Kind::Wait, true), "{}", o.line);
            assert!(o.line.contains(word), "{word}: {}", o.line);
        };
        // All three pass and a GitHub check is still queued or running: wait for it (#60).
        for status in ["queued", "in_progress"] {
            let mut c = compiler_green();
            c[1] = run("test (ubuntu-latest)", status, None);
            f.checks = Some(c);
            wait(&f, "checks-not-done: test (ubuntu-latest)");
        }
        // A fast check passed before the required ones exist: wait for them.
        f.checks = Some(compiler_green()[..1].to_vec());
        wait(
            &f,
            "required-checks-missing: test (ubuntu-latest),cpp (plain",
        );
        // GitHub's own `CLEAN` does not override the wait.
        let mut clean = f.clone();
        clean.pr.mergeable_state = "CLEAN".into();
        let mut c = compiler_green();
        c[0] = run("lint", "queued", None);
        clean.checks = Some(c);
        wait(&clean, "checks-not-done: lint");
        // Every check passed, but not `mergeable` (or not known yet): wait (r1, major 2).
        for m in [None, Some(false)] {
            let mut g = f.clone();
            g.checks = Some(compiler_green());
            g.pr.mergeable = m;
            wait(&g, "mergeable");
        }
        // Every check done and passed: merged without an update.
        let mut c = compiler_green();
        c.push(run("docs", "completed", Some("skipped")));
        f.checks = Some(c);
        match decide(&f, REPO, HEAD, &r, &Opts::default()) {
            Step::Merge(l) => assert!(
                l.contains(&format!("local-build-pass-on-main-{}", short(MAIN)))
                    && l.ends_with("checks 7/7 passed"),
                "{l}"
            ),
            s => panic!("{s:?}"),
        }
    }

    /// hgl-compiler's required checks, each passed.
    fn compiler_green() -> Vec<CheckRun> {
        settings()
            .gate_rules("hgl-compiler")
            .required_checks
            .iter()
            .map(|n| run(n, "completed", Some("success")))
            .collect()
    }

    fn run(name: &str, status: &str, conclusion: Option<&str>) -> CheckRun {
        CheckRun {
            id: 1,
            name: name.into(),
            status: status.into(),
            conclusion: conclusion.map(String::from),
        }
    }

    /// ROLE-20 (r1 on #50, the blocker): the checks gate a merge, not `mergeable_state`.
    #[test]
    fn required_checks_and_commit_statuses() {
        let mut f = allowed();
        // Every pull request is `BLOCKED` under `merge_by_admin`.
        f.pr.mergeable_state = "BLOCKED".into();
        f.checks = None;
        let opts = Opts::default();
        assert_eq!(
            decide(&f, REPO, HEAD, &rules(), &opts),
            Step::Need(Need::Checks)
        );
        // None required and none present (hgl_spec, hgl_core): merged on `mergeable`.
        f.checks = Some(vec![]);
        assert!(matches!(
            decide(&f, REPO, HEAD, &rules(), &opts),
            Step::Merge(_)
        ));
        let required = |names: &[&str]| GateRules {
            required_checks: names.iter().map(|n| n.to_string()).collect(),
            ..rules()
        };
        let r = required(&["lint", "test"]);
        let wait = |f: &Facts, r: &GateRules, word: &str| {
            let o = outcome(f, r, &opts);
            assert_eq!((o.kind, o.retry), (Kind::Wait, true), "{}", o.line);
            assert!(o.line.contains(word), "{word}: {}", o.line);
        };
        // A fast check passed before a required one exists: wait for it.
        f.checks = Some(vec![run("lint", "completed", Some("success"))]);
        wait(&f, &r, "required-checks-missing: test");
        f.checks = Some(vec![
            run("lint", "completed", Some("success")),
            run("test", "in_progress", None),
        ]);
        wait(&f, &r, "checks-not-done: test");
        let green = vec![
            run("lint", "completed", Some("success")),
            run("test", "completed", Some("success")),
        ];
        f.checks = Some(green.clone());
        assert!(
            matches!(decide(&f, REPO, HEAD, &r, &opts), Step::Merge(l) if l.ends_with("checks 2/2 passed"))
        );
        // A check present but not required still has to finish and pass.
        let mut c = green.clone();
        c.push(run("extra", "queued", None));
        f.checks = Some(c);
        wait(&f, &r, "checks-not-done: extra");
        let mut c = green.clone();
        c.push(run("extra", "completed", Some("failure")));
        f.checks = Some(c);
        assert_eq!(outcome(&f, &r, &opts).kind, Kind::Blocked);
        // Not `mergeable`, or not known yet: wait, however green.
        for m in [None, Some(false)] {
            let mut g = f.clone();
            g.checks = Some(green.clone());
            g.pr.mergeable = m;
            wait(&g, &r, "mergeable");
        }
        // A commit status (readthedocs) is a check like the others.
        let r = required(&["docs/readthedocs.org:hgl-project"]);
        let status = |state: &str| {
            commit_statuses(&serde_json::json!({"state": state, "statuses": [
                {"id": 9, "context": "docs/readthedocs.org:hgl-project", "state": state}
            ]}))
        };
        f.checks = Some(vec![]);
        wait(
            &f,
            &r,
            "required-checks-missing: docs/readthedocs.org:hgl-project",
        );
        f.checks = Some(status("pending"));
        wait(&f, &r, "checks-not-done: docs/readthedocs.org:hgl-project");
        f.checks = Some(status("success"));
        assert!(matches!(decide(&f, REPO, HEAD, &r, &opts), Step::Merge(_)));
        for bad in ["failure", "error"] {
            f.checks = Some(status(bad));
            assert_eq!(outcome(&f, &r, &opts).kind, Kind::Blocked, "{bad}");
        }
    }

    #[tokio::test]
    async fn evaluate_reads_check_runs_and_commit_statuses() {
        let s = without_receipts(settings(), &[REPO]);
        let mut s = s;
        s.gates.insert(
            REPO.into(),
            hgl_ai_config::github::RepoGate {
                checks: vec!["docs/readthedocs.org:hgl-project".into()],
                review_receipt: false,
                ..Default::default()
            },
        );
        let gh = FakeGh::default()
            .on(
                "/status?per_page",
                r#"{"state":"pending","statuses":[{"id":1,"context":"docs/readthedocs.org:hgl-project","state":"pending"}]}"#,
            )
            .on("check-runs", r#"{"check_runs":[]}"#);
        let gh = fake_with(gh, &["pr-open.json"]);
        let gh = crate::exec::ReadOnly(gh);
        let o = evaluate(
            &gh,
            &s,
            None,
            REPO,
            7,
            HEAD,
            &Opts::default(),
            false,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(o.kind, Kind::Wait, "{}", o.line);
        assert!(
            o.line
                .contains("checks-not-done: docs/readthedocs.org:hgl-project"),
            "{}",
            o.line
        );
        assert!(
            gh.0.calls()
                .iter()
                .any(|c| c.contains(&format!("commits/{HEAD}/status")))
        );
    }

    #[test]
    fn relaxed_paths() {
        let r = settings().gate_rules("hgl-compiler");
        let mut f = allowed();
        f.verdicts = Some(vec![]);
        let r = GateRules {
            verdicts: vec![],
            ..r
        };
        f.pr.mergeable_state = "BLOCKED".into();
        let inside = vec![PrFile {
            name: "runtime/cpp/x.cpp".into(),
            status: "modified".into(),
        }];
        f.pr_files = Some(inside.clone());
        // Required and `relax_required` checks passed; the optional macOS and Windows runs
        // still going.
        let mut checks = compiler_green();
        checks.push(run("test (macos-latest)", "in_progress", None));
        checks.push(run("test (windows-latest)", "queued", None));
        f.checks = Some(checks.clone());
        assert!(
            matches!(decide(&f, REPO, HEAD, &r, &Opts::default()), Step::Merge(l) if l.contains("relaxed-paths-only")),
            "{:?}",
            decide(&f, REPO, HEAD, &r, &Opts::default())
        );
        let wait = |f: &Facts, word: &str| {
            let o = outcome(f, &r, &Opts::default());
            assert_eq!(o.kind, Kind::Wait, "{}", o.line);
            assert!(o.line.contains(word), "{word}: {}", o.line);
        };
        // An optional check that failed blocks.
        let mut c = checks.clone();
        let n = c.len();
        c[n - 2].status = "completed".into();
        c[n - 2].conclusion = Some("failure".into());
        f.checks = Some(c);
        assert_eq!(outcome(&f, &r, &Opts::default()).kind, Kind::Blocked);
        // A check neither required nor optional is waited for (r1, major 3).
        let mut c = checks.clone();
        c.push(run("extra", "in_progress", None));
        f.checks = Some(c);
        wait(&f, "checks-not-done: extra");
        // A required check still running prevents relaxation and is itself waited for.
        let mut c = checks.clone();
        let required = c.iter_mut().find(|c| c.name == "cpp (plain, sdk)").unwrap();
        required.status = "in_progress".into();
        required.conclusion = None;
        f.checks = Some(c);
        wait(&f, "checks-not-done: cpp (plain, sdk)");
        // A missing default relaxation check is also a missing required check.
        f.checks = Some(
            checks
                .iter()
                .filter(|c| c.name != "cpp (plain, sdk)")
                .cloned()
                .collect(),
        );
        wait(&f, "required-checks-missing: cpp (plain, sdk)");
        // An extra relaxation prerequisite can be missing without being required.
        f.checks = Some(checks.clone());
        let mut extra = r.clone();
        extra.relax_required.push("extra prerequisite".into());
        let o = outcome(&f, &extra, &Opts::default());
        assert_eq!(o.kind, Kind::Wait, "{}", o.line);
        assert!(
            o.line.contains("checks-not-done: test (macos-latest)"),
            "{}",
            o.line
        );
        // A file outside, or no files at all (r1, major 3): no relaxation.
        f.checks = Some(checks.clone());
        f.pr_files = Some(vec![PrFile {
            name: "crates/hgl_types/src/lib.rs".into(),
            status: "modified".into(),
        }]);
        wait(&f, "checks-not-done: test (macos-latest)");
        f.pr_files = Some(vec![]);
        wait(&f, "checks-not-done: test (macos-latest)");
        // Relaxed, but not `mergeable`: wait.
        f.pr_files = Some(inside);
        f.pr.mergeable = Some(false);
        wait(&f, "mergeable false");
    }

    #[test]
    fn hygiene() {
        let mut f = allowed();
        f.pr_files = Some(vec![
            PrFile {
                name: "src/__pycache__/a.pyc".into(),
                status: "added".into(),
            },
            PrFile {
                name: "old.log".into(),
                status: "removed".into(),
            },
        ]);
        let o = outcome(&f, &rules(), &Opts::default());
        assert!(
            o.line.contains("hygiene: files [src/__pycache__/a.pyc]")
                && !o.line.contains("old.log"),
            "{}",
            o.line
        );
        let mut f = allowed();
        // Built here, so that this repository's own diff holds no such line.
        let (home, users) = (["/", "home"].concat(), ["/", "Users"].concat());
        f.diff = Some(Ok(format!(
            "+++ b{home}/x\n+let p = \"{home}/alice/x\";\n+let q = \"{users}/Bob\";\n-{home}/old\n"
        )));
        let o = outcome(&f, &rules(), &Opts::default());
        assert!(o.line.contains("lines-with-local-path 2"), "{}", o.line);
        let read = Opts {
            hygiene_read: true,
            ..Opts::default()
        };
        assert!(
            matches!(decide(&f, REPO, HEAD, &rules(), &read), Step::Merge(l) if l.contains("hygiene-lines-read:2"))
        );
        f.diff = Some(Err("HTTP 406: diff too large".into()));
        blocked(&f, "diff not readable");
        // The default patterns do not match their own text.
        let own =
            "+    hygiene_lines = [\"/home/[a-z]\", \"/tmp/claude[-]\", \"/Users/[A-Za-z]\"]\n";
        assert_eq!(lines_matching(own, &rules().hygiene_lines), 0);
    }

    const MERGED_ASYNC: &str = "{\"status\":\"merged\",\"details\":{\"message\":\"m\"}}";

    fn fake() -> FakeGh {
        FakeGh::default()
            .on("pulls/7/merge-async", MERGED_ASYNC)
            .on("pulls/7/files", &fixture("pr-files.json"))
            .on("Accept: application/vnd.github.v3.diff", &fixture("pr.diff"))
            .on("issues/7/comments", &fixture("comments-clean.json"))
            .on("compare/", "{\"ahead_by\":0,\"files\":[]}")
            .on("git/ref/heads/main", &format!("{{\"object\":{{\"sha\":\"{MAIN}\"}}}}"))
            .on("check-runs", "{\"check_runs\":[]}")
            .on("/status?per_page", "{\"statuses\":[]}")
            .on("graphql", "{\"data\":{\"repository\":{\"pullRequest\":{\"reviewThreads\":{\"nodes\":[{\"isResolved\":true}]}}}}}")
    }

    #[tokio::test]
    async fn evaluate_acts_only_when_asked() {
        let s = without_receipts(settings(), &[REPO]);
        // A dry run on a read-only runner: the decision, and no write.
        let gh = crate::exec::ReadOnly(fake().on("pulls/7", &fixture("pr-open.json")));
        let o = evaluate(
            &gh,
            &s,
            None,
            REPO,
            7,
            HEAD,
            &Opts::default(),
            false,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(o.kind, Kind::WouldMerge, "{}", o.line);
        assert!(gh.0.calls().iter().all(|c| !c.contains("-X PUT")));
        // Acting: merged at the head, and the merge commit read back.
        let gh = fake()
            .once("pulls/7", &fixture("pr-open.json"))
            .on("pulls/7", &fixture("pr-merged.json"));
        let o = evaluate(
            &gh,
            &s,
            None,
            REPO,
            7,
            HEAD,
            &Opts::default(),
            true,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(o.kind, Kind::Merged, "{}", o.line);
        assert_eq!(
            o.merge_commit.as_deref(),
            Some("01e99ea5e46457bac035ca8a2699b6b6575b32ed")
        );
        assert!(o.line.ends_with("-> MERGED 01e99ea5 MERGED"));
        let merges: Vec<String> = gh
            .calls()
            .into_iter()
            .filter(|c| c.contains("/merge"))
            .collect();
        assert_eq!(
            merges,
            [format!(
                "api -X PUT repos/{}/pulls/7/merge-async --input -",
                s.full(REPO)
            )]
        );
        let (_, body) = gh.inputs.lock().unwrap()[0].clone();
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body, merge::body(HEAD));
        assert_eq!(body["bypass_rules"], Value::Bool(true));
        // A merged pull request is reported as such; a short head is refused before any call.
        let gh = fake().on("pulls/7", &fixture("pr-merged.json"));
        assert_eq!(
            evaluate(
                &gh,
                &s,
                None,
                REPO,
                7,
                HEAD,
                &Opts::default(),
                true,
                None,
                None
            )
            .await
            .unwrap()
            .kind,
            Kind::AlreadyMerged
        );
        assert!(
            evaluate(
                &gh,
                &s,
                None,
                REPO,
                7,
                "1f461fa7",
                &Opts::default(),
                true,
                None,
                None
            )
            .await
            .is_err()
        );
    }

    /// The fake with the merge rules `first` answering before its own, and the pull
    /// request's reads `prs` after them (a bare `pulls/7` would match its other reads).
    fn fake_with(first: FakeGh, prs: &[&str]) -> FakeGh {
        let rest = fake();
        first
            .rules
            .lock()
            .unwrap()
            .extend(rest.rules.into_inner().unwrap());
        let (last, once) = prs.split_last().unwrap();
        once.iter()
            .fold(first, |g, p| g.once("pulls/7", &fixture(p)))
            .on("pulls/7", &fixture(last))
    }

    #[tokio::test(start_paused = true)]
    async fn evaluate_after_merge_async() {
        let s = without_receipts(settings(), &[REPO]);
        let s = &s;
        let run = |gh: FakeGh, until: Option<tokio::time::Instant>| async move {
            let o = Opts {
                merge_until: until,
                ..Opts::default()
            };
            let out = evaluate(&gh, s, None, REPO, 7, HEAD, &o, true, None, None)
                .await
                .unwrap();
            (out, gh)
        };
        let pending = "{\"status\":\"pending\",\"details\":{\"uuid\":\"u-9\"}}";
        // A transient poll failure, then merged; the pull request lags one read behind.
        let gh = fake_with(
            FakeGh::default()
                .on("merge-async --input", pending)
                .push_fail_once("merge-async/u-9", "gh: Server Error (HTTP 502)")
                .on("merge-async/u-9", MERGED_ASYNC),
            &["pr-open.json", "pr-open.json", "pr-merged.json"],
        );
        let (o, gh) = run(gh, None).await;
        assert_eq!(o.kind, Kind::Merged, "{}", o.line);
        let polls = gh.calls().iter().filter(|c| c.contains("u-9")).count();
        assert_eq!(polls, 2);
        // A landing whose gate waited past the lease's max age (30 min) before the merge:
        // the bound comes from its deadline at the merge, so the poll still runs.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(50 * 60);
        tokio::time::advance(std::time::Duration::from_secs(40 * 60)).await;
        let gh = fake_with(
            FakeGh::default()
                .on("merge-async --input", pending)
                .once("merge-async/u-9", pending)
                .on("merge-async/u-9", MERGED_ASYNC),
            &["pr-open.json", "pr-merged.json"],
        );
        let (o, gh) = run(gh, Some(deadline)).await;
        assert_eq!(o.kind, Kind::Merged, "{}", o.line);
        let polls = gh.calls().iter().filter(|c| c.contains("u-9")).count();
        assert_eq!(polls, 2);
        // Still pending at the deadline: a wait that a retry may change, not a refusal.
        let gh = fake_with(
            FakeGh::default().on("merge-async", pending),
            &["pr-open.json"],
        );
        let until = tokio::time::Instant::now() + std::time::Duration::from_secs(7);
        let (o, _) = run(gh, Some(until)).await;
        assert_eq!((o.kind, o.retry), (Kind::Wait, true), "{}", o.line);
        assert!(
            o.line.contains("merge accepted, not done yet"),
            "{}",
            o.line
        );
        assert_eq!(
            tokio::time::Instant::now(),
            until,
            "polled up to the deadline"
        );
        // Failed: refused.
        let gh = fake_with(
            FakeGh::default().on(
                "merge-async",
                "{\"status\":\"failed\",\"details\":{\"message\":\"conflict\"}}",
            ),
            &["pr-open.json"],
        );
        let (o, _) = run(gh, None).await;
        assert_eq!(o.kind, Kind::Blocked, "{}", o.line);
        assert!(
            o.line
                .contains("merge refused (asynchronous merge failed: conflict)")
        );
    }

    #[tokio::test]
    async fn evaluate_brings_up_to_date() {
        let s = without_receipts(settings(), &[REPO]);
        let gh = FakeGh::default()
            .on(
                "update-branch",
                "{\"message\":\"Updating pull request branch.\"}",
            )
            .on("pulls/7", &fixture("pr-open.json"))
            .on("issues/7/comments", &fixture("comments-clean.json"))
            .on(&format!("compare/{HEAD}...main"), &fixture("compare.json"))
            .on(&format!("compare/main...{HEAD}"), &fixture("compare.json"))
            .on(
                "git/ref/heads/main",
                &format!("{{\"object\":{{\"sha\":\"{MAIN}\"}}}}"),
            )
            .on(
                "graphql",
                &fixture("threads-page2.json").replace("false", "true"),
            );
        let o = evaluate(
            &gh,
            &s,
            None,
            REPO,
            7,
            HEAD,
            &Opts::default(),
            true,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(o.kind, Kind::Updated, "{}", o.line);
        assert!(o.retry);
        let up = gh
            .calls()
            .into_iter()
            .find(|c| c.contains("update-branch"))
            .unwrap();
        assert!(up.contains(&format!("expected_head_sha={HEAD}")));
        assert!(!gh.calls().iter().any(|c| c.contains("/merge")));
    }
}
