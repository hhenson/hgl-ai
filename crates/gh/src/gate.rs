//! The merge gate, `gh merge-if-clean REPO PR HEAD`, ported from the shell gate with every
//! check it made. A pull request is merged only when all hold:
//!
//! - its head is the `HEAD` asked for, or the merge of that head with the present `main`
//!   that bringing it up to date made;
//! - it targets `main`, is open, not a draft, has no conflict and no unresolved thread;
//! - the review bot reviewed the head clean, or the head's first parent where the head only
//!   merges the present `main` into it, or (`--triaged`) reviewed the head with findings the
//!   controller triaged;
//! - where the repository names local build verdicts, each passed for this head on this
//!   `main`; then GitHub's checks are not waited for (one that failed still blocks) and the
//!   branch is not brought up to date;
//! - it is not behind `main`, or `main` moved only in files it does not touch and in none of
//!   the global paths; otherwise it is brought up to date (and not merged: CI runs first);
//! - GitHub reports it `CLEAN` (all checks green), or it changes only the relaxed paths and
//!   the required checks passed while the optional ones have not failed;
//! - no build output among its files and no local path among its added lines.
//!
//! The decision is [`decide`], a pure function over [`Facts`] that names the next fact it
//! needs; [`evaluate`] fetches facts until it decides, and acts unless it is a dry run.

use std::collections::BTreeMap;

use hgl_ai_bus::Bus;
use serde_json::Value;

use crate::bot::{BotView, Comment, Review};
use crate::exec::{self, Gh};
use crate::lock::LandLock;
use crate::{Error, GateRules, Result, Settings, short, state};

/// The pull request fields the gate reads (REST `pulls/N`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pr {
    pub number: u64,
    pub state: String,
    pub merged: bool,
    /// `mergeable_state`, upper case (`CLEAN`, `UNSTABLE`, `DIRTY`, `BLOCKED`, `BEHIND`, `UNKNOWN`, ...).
    pub mergeable_state: String,
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
    pub fn failed(&self) -> bool {
        matches!(
            self.conclusion.as_deref(),
            Some("failure" | "cancelled" | "timed_out")
        )
    }
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

/// One local build verdict: its name and, for this head on this main, pass or fail.
pub type VerdictResult = (String, Option<(bool, String)>);

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
}

/// Switches the controller gives: `TRIAGED`, `HYGIENE_READ` and `NOUPDATE` of the shell gate,
/// bound to the head asked for.
#[derive(Debug, Clone, Copy, Default)]
pub struct Opts {
    pub triaged: bool,
    pub hygiene_read: bool,
    pub no_update: bool,
}

/// The last word of a gate line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Merged,
    AlreadyMerged,
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
            Kind::Merged | Kind::AlreadyMerged => "MERGED",
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
        matches!(self, Kind::Merged | Kind::AlreadyMerged | Kind::WouldMerge)
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
    fn new(kind: Kind, line: String, retry: bool, head: &str) -> Outcome {
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
    if bot.clean_on(head) {
        reviewed = "head";
    } else {
        let parents = need!(f.parents, Need::Parents);
        if parents.len() == 2 && parents[1] == main && bot.clean_on(&parents[0]) {
            reviewed = "parent";
        }
    }
    if reviewed == "no" && opts.triaged {
        let reviews = need!(f.bot_reviews, Need::BotReviews);
        if reviews.iter().any(|r| r.commit.as_deref() == Some(head)) {
            reviewed = "triaged";
        }
    }
    let mut ms = pr.mergeable_state.clone();
    let mut line = format!(
        "{prefix} {ms} head {} unresolved {unresolved} behind {behind} reviewed {reviewed}",
        short(head)
    );
    if reviewed == "no" {
        if bot.limited() {
            return done(
                Kind::BotLimit,
                format!("{line} review bot out of quota BOTLIMIT"),
                false,
            );
        }
        if bot.running_on(head) {
            return done(Kind::Wait, format!("{line} review-running WAIT"), true);
        }
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
    if !rules.verdicts.is_empty() {
        let v = need!(f.verdicts, Need::Verdicts);
        let failed: Vec<&str> = v
            .iter()
            .filter_map(|(n, r)| r.as_ref().filter(|r| !r.0).map(|r| (n, r)))
            .map(|(_, r)| r.1.as_str())
            .collect();
        if !failed.is_empty() {
            return done(
                Kind::Blocked,
                format!("{line} local build failed: {} BLOCKED", failed.join(";")),
                false,
            );
        }
        let missing: Vec<&str> = v
            .iter()
            .filter(|(_, r)| r.is_none())
            .map(|(n, _)| n.as_str())
            .collect();
        if !missing.is_empty() || v.len() < rules.verdicts.len() {
            return done(
                Kind::Wait,
                format!("{line} no-local-build-verdict ({}) WAIT", missing.join(",")),
                true,
            );
        }
        let checks = need!(f.checks, Need::Checks);
        let failed: Vec<&str> = checks
            .iter()
            .filter(|c| c.failed())
            .map(|c| c.name.as_str())
            .collect();
        if !failed.is_empty() {
            return done(
                Kind::Blocked,
                format!(
                    "{line} local build passed but GitHub failed: {} BLOCKED",
                    failed.join(",")
                ),
                false,
            );
        }
        line.push_str(&format!(" local-build-pass-on-main-{}", short(main)));
        behind = 0;
        ms = "CLEAN".into();
    }
    if behind != 0 {
        let mine = need!(f.mine, Need::CompareFiles);
        let theirs = need!(f.theirs, Need::CompareFiles);
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
            .filter(|t| rules.global_paths.iter().any(|g| t.starts_with(g.as_str())))
            .take(3)
            .map(|t| format!("{t} "))
            .collect();
        if mine.files.is_empty() || theirs.files.is_empty() || !mine.whole || !theirs.whole {
            overlap.push_str("(file lists not known whole) ");
        }
        if overlap.is_empty() && global.is_empty() {
            line.push_str(" behind-in-untouched-files-only");
            behind = 0;
        } else {
            line.push_str(&format!(
                " main-moved-in: {}",
                format!("{overlap}{global}").trim_end()
            ));
        }
    }
    if behind != 0 && opts.no_update {
        return done(Kind::Behind, format!("{line} BEHIND"), false);
    }
    if behind != 0 {
        return Step::Update(line);
    }
    if ms == "UNSTABLE" && rules.relax_configured() {
        let files = need!(f.pr_files, Need::PrFiles);
        let checks = need!(f.checks, Need::Checks);
        let inside = files
            .iter()
            .all(|f| rules.relax_paths.iter().any(|r| r.is_match(&f.name)));
        let required_pass = rules.relax_required.iter().all(|n| {
            checks
                .iter()
                .any(|c| &c.name == n && c.conclusion.as_deref() == Some("success"))
        });
        let optional_ok = checks
            .iter()
            .filter(|c| rules.relax_optional.iter().any(|r| r.is_match(&c.name)))
            .all(|c| matches!(c.conclusion.as_deref(), None | Some("") | Some("success")));
        if inside && required_pass && optional_ok {
            line.push_str(" relaxed-paths-only:not-waiting-for-optional-checks");
            ms = "CLEAN".into();
        }
    }
    if ms != "CLEAN" {
        let checks = need!(f.checks, Need::Checks);
        let failed: Vec<&str> = checks
            .iter()
            .filter(|c| c.failed())
            .map(|c| c.name.as_str())
            .collect();
        if !failed.is_empty() {
            return done(
                Kind::Blocked,
                format!("{line} checks failed: {} BLOCKED", failed.join(",")),
                false,
            );
        }
        return done(Kind::Wait, format!("{line} WAIT"), true);
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

/// The head of `main` of a repository.
pub async fn main_head<G: Gh>(gh: &G, s: &Settings, repo: &str) -> Result<String> {
    let v = exec::get(gh, &format!("repos/{}/git/ref/heads/main", s.full(repo))).await?;
    v.pointer("/object/sha")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| resp("git/ref/heads/main", "no object.sha"))
}

async fn fill<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: Option<&Bus>,
    repo: &str,
    f: &mut Facts,
    need: Need,
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
                let value = match bus {
                    Some(b) => b
                        .state_get(&state::verdict_key(repo, n, name))
                        .await?
                        .map(|e| e.value),
                    None => None,
                };
                out.push((
                    name.clone(),
                    state::verdict_for(value.as_deref(), &head, &main),
                ));
            }
            f.verdicts = Some(out);
        }
        Need::Checks => {
            let pages = exec::get_pages(
                gh,
                &format!("repos/{r}/commits/{head}/check-runs?per_page=100&filter=all"),
            )
            .await?;
            f.checks = Some(latest_checks(&pages));
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
    }
    Ok(())
}

/// Run the gate on `repo#number` for the head `asked`. With `act`, bring the branch up to
/// date or merge where the gate says so; without, say what it would do. Under a landing
/// lock, the lock is confirmed ours right before the merge ([`LandLock::confirm`]).
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
) -> Result<Outcome> {
    hgl_ai_proto::check_head(asked)?;
    let rules = s.gate_rules(repo);
    let mut f = Facts {
        pr: fetch_pr(gh, s, repo, number).await?,
        ..Facts::default()
    };
    loop {
        match decide(&f, repo, asked, &rules, opts) {
            Step::Need(n) => fill(gh, s, bus, repo, &mut f, n).await?,
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
                let path = format!("repos/{}/pulls/{number}/merge", s.full(repo));
                let res = exec::write(
                    gh,
                    "PUT",
                    &path,
                    &[("merge_method", "merge"), ("sha", &f.pr.head)],
                )
                .await;
                let after = fetch_pr(gh, s, repo, number).await?;
                if after.merged {
                    let m = after.merge_commit.clone().unwrap_or_default();
                    let mut o = Outcome::new(
                        Kind::Merged,
                        format!("{line} -> MERGED {} MERGED", short(&m)),
                        false,
                        &f.pr.head,
                    );
                    o.merge_commit = Some(m);
                    return Ok(o);
                }
                let why = res.err().map(|e| format!(" ({e})")).unwrap_or_default();
                return Ok(Outcome::new(
                    Kind::Blocked,
                    format!("{line} merge refused{why} BLOCKED"),
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
    use crate::testutil::{fixture, json, settings};

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
            checks: None,
            mine: None,
            theirs: None,
            pr_files: Some(pr_files(json("pr-files.json").as_array().unwrap())),
            diff: Some(Ok(fixture("pr.diff"))),
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
                    "alpha #7 codex/980-format-patterns CLEAN head 1f461fa7 unresolved 0 behind 0 reviewed head"
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
        // The update merge of the asked head onto the present main is the same head.
        f.parents = Some(vec![HEAD.into(), MAIN.into()]);
        f.bot = Some(bot_clean());
        match decide(&f, REPO, HEAD, &rules(), &Opts::default()) {
            Step::Merge(line) => assert!(line.ends_with("reviewed parent"), "{line}"),
            other => panic!("{other:?}"),
        }
        // Onto a main that has moved since, it is not.
        f.parents = Some(vec![HEAD.into(), "9".repeat(40)]);
        blocked(&f, "head moved");
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
            matches!(decide(&f, REPO, HEAD, &rules(), &opts), Step::Merge(l) if l.ends_with("reviewed triaged"))
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
        f.pr.mergeable_state = "UNKNOWN".into();
        f.checks = Some(vec![]);
        assert_eq!(outcome(&f, &rules(), &Opts::default()).kind, Kind::Wait);
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
        (
            name.into(),
            Some((
                pass,
                format!("{HEAD} {MAIN} {}", if pass { "pass" } else { "fail" }),
            )),
        )
    }

    #[test]
    fn local_build_verdicts() {
        let r = settings().gate_rules("hgl-compiler");
        let mut f = allowed();
        f.pr.mergeable_state = "BLOCKED".into();
        f.behind = Some(4);
        assert_eq!(
            decide(&f, REPO, HEAD, &r, &Opts::default()),
            Step::Need(Need::Verdicts)
        );
        f.verdicts = Some(vec![
            verdict("linux", true),
            ("mac".into(), None),
            verdict("windows", true),
        ]);
        let o = outcome(&f, &r, &Opts::default());
        assert_eq!(o.kind, Kind::Wait);
        assert!(
            o.line.contains("no-local-build-verdict (mac)"),
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
        // All three pass and nothing failed: merged without waiting and without an update.
        f.checks = Some(vec![CheckRun {
            id: 1,
            name: "lint".into(),
            status: "queued".into(),
            conclusion: None,
        }]);
        match decide(&f, REPO, HEAD, &r, &Opts::default()) {
            Step::Merge(l) => assert!(
                l.ends_with(&format!("local-build-pass-on-main-{}", short(MAIN))),
                "{l}"
            ),
            s => panic!("{s:?}"),
        }
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
        f.pr.mergeable_state = "UNSTABLE".into();
        f.pr_files = Some(vec![PrFile {
            name: "runtime/cpp/x.cpp".into(),
            status: "modified".into(),
        }]);
        let ok = |n: &str| CheckRun {
            id: 1,
            name: n.into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
        };
        let pending = |n: &str| CheckRun {
            id: 2,
            name: n.into(),
            status: "in_progress".into(),
            conclusion: None,
        };
        let mut checks: Vec<CheckRun> = ["lint", "test (ubuntu-latest)", "hgraph", "cpp"]
            .iter()
            .map(|n| ok(n))
            .collect();
        checks.push(pending("test (macos-latest)"));
        f.checks = Some(checks.clone());
        assert!(
            matches!(decide(&f, REPO, HEAD, &r, &Opts::default()), Step::Merge(l) if l.contains("relaxed-paths-only"))
        );
        // An optional check that failed, a required one missing, or a file outside: wait.
        let mut failed = checks.clone();
        failed[4].conclusion = Some("failure".into());
        f.checks = Some(failed);
        assert!(outcome(&f, &r, &Opts::default()).kind == Kind::Blocked);
        f.checks = Some(checks[1..].to_vec());
        assert_eq!(outcome(&f, &r, &Opts::default()).kind, Kind::Wait);
        f.checks = Some(checks);
        f.pr_files = Some(vec![PrFile {
            name: "crates/hgl_types/src/lib.rs".into(),
            status: "modified".into(),
        }]);
        assert_eq!(outcome(&f, &r, &Opts::default()).kind, Kind::Wait);
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

    fn fake() -> FakeGh {
        FakeGh::default()
            .on("pulls/7/merge", "{\"merged\":true}")
            .on("pulls/7/files", &fixture("pr-files.json"))
            .on("Accept: application/vnd.github.v3.diff", &fixture("pr.diff"))
            .on("issues/7/comments", &fixture("comments-clean.json"))
            .on("compare/", "{\"ahead_by\":0,\"files\":[]}")
            .on("git/ref/heads/main", &format!("{{\"object\":{{\"sha\":\"{MAIN}\"}}}}"))
            .on("graphql", "{\"data\":{\"repository\":{\"pullRequest\":{\"reviewThreads\":{\"nodes\":[{\"isResolved\":true}]}}}}}")
    }

    #[tokio::test]
    async fn evaluate_acts_only_when_asked() {
        let s = settings();
        // A dry run on a read-only runner: the decision, and no write.
        let gh = crate::exec::ReadOnly(fake().on("pulls/7", &fixture("pr-open.json")));
        let o = evaluate(&gh, &s, None, REPO, 7, HEAD, &Opts::default(), false, None)
            .await
            .unwrap();
        assert_eq!(o.kind, Kind::WouldMerge, "{}", o.line);
        assert!(gh.0.calls().iter().all(|c| !c.contains("-X PUT")));
        // Acting: merged at the head, and the merge commit read back.
        let gh = fake()
            .once("pulls/7", &fixture("pr-open.json"))
            .on("pulls/7", &fixture("pr-merged.json"));
        let o = evaluate(&gh, &s, None, REPO, 7, HEAD, &Opts::default(), true, None)
            .await
            .unwrap();
        assert_eq!(o.kind, Kind::Merged, "{}", o.line);
        assert_eq!(
            o.merge_commit.as_deref(),
            Some("01e99ea5e46457bac035ca8a2699b6b6575b32ed")
        );
        assert!(o.line.ends_with("-> MERGED 01e99ea5 MERGED"));
        let merge = gh
            .calls()
            .into_iter()
            .find(|c| c.contains("/merge"))
            .unwrap();
        assert!(
            merge.contains("-X PUT")
                && merge.contains(&format!("sha={HEAD}"))
                && merge.contains("merge_method=merge")
        );
        // A merged pull request is reported as such; a short head is refused before any call.
        let gh = fake().on("pulls/7", &fixture("pr-merged.json"));
        assert_eq!(
            evaluate(&gh, &s, None, REPO, 7, HEAD, &Opts::default(), true, None)
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
                None
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn evaluate_brings_up_to_date() {
        let s = settings();
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
        let o = evaluate(&gh, &s, None, REPO, 7, HEAD, &Opts::default(), true, None)
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
