//! `HGL_STATE/prs` (docs/protocol.md §8): one batched GraphQL read of every open pull request
//! of the configured repositories, rendered as
//!
//! ```text
//! # read <UTC>
//! # cost <n> remaining <n> resetAt <ISO>
//! REPO<TAB>NUMBER<TAB>HEAD<TAB>draft|open<TAB>BOT<TAB>UNRESOLVED<TAB>CHECKS<TAB>UPDATED
//! ```
//!
//! UPDATED is the UTC time of the reading, in the compact form of a note's `created_at`.
//! Also here: `hold` from the GraphQL allowance, and the alerts a round raises.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;

use crate::bot::{
    BotState, BotView, Comment, NoteState, Review, by_note_authors, note_state, ready_mark_across,
};
use crate::{Error, Result, short};

/// `CHECKS` of a row: the head commit's status check rollup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checks {
    Pass,
    Fail,
    Pending,
    None,
}

impl Checks {
    pub fn as_str(self) -> &'static str {
        match self {
            Checks::Pass => "pass",
            Checks::Fail => "fail",
            Checks::Pending => "pending",
            Checks::None => "none",
        }
    }
    pub fn parse(s: &str) -> Option<Checks> {
        Some(match s {
            "pass" => Checks::Pass,
            "fail" => Checks::Fail,
            "pending" => Checks::Pending,
            "none" => Checks::None,
            _ => return None,
        })
    }
    /// From GraphQL's `StatusCheckRollup.state`; no rollup is `none`.
    pub fn from_rollup(state: Option<&str>) -> Checks {
        match state {
            None => Checks::None,
            Some("SUCCESS") => Checks::Pass,
            Some("FAILURE") | Some("ERROR") => Checks::Fail,
            Some(_) => Checks::Pending,
        }
    }
}

/// One row of the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub repo: String,
    pub number: u64,
    pub head: String,
    pub draft: bool,
    pub bot: BotState,
    pub unresolved: u64,
    pub checks: Checks,
    pub updated: String,
}

/// GraphQL's own `rateLimit` (the REST figure for GraphQL is not reliable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimit {
    pub cost: u64,
    pub remaining: u64,
    pub reset_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Table {
    pub read: String,
    pub rate: Option<RateLimit>,
    pub rows: Vec<Row>,
}

impl Table {
    pub fn render(&self) -> String {
        let mut s = format!("# read {}\n", self.read);
        if let Some(r) = &self.rate {
            s.push_str(&format!(
                "# cost {} remaining {} resetAt {}\n",
                r.cost, r.remaining, r.reset_at
            ));
        }
        for r in &self.rows {
            s.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                r.repo,
                r.number,
                r.head,
                if r.draft { "draft" } else { "open" },
                r.bot.as_str(),
                r.unresolved,
                r.checks.as_str(),
                r.updated
            ));
        }
        s
    }

    /// Parse a table written earlier; lines that do not parse are skipped.
    pub fn parse(text: &str) -> Table {
        let mut t = Table::default();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# read ") {
                t.read = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("# cost ") {
                let w: Vec<&str> = rest.split_whitespace().collect();
                if let [cost, "remaining", rem, "resetAt", reset] = w[..]
                    && let (Ok(cost), Ok(remaining)) = (cost.parse(), rem.parse())
                {
                    t.rate = Some(RateLimit {
                        cost,
                        remaining,
                        reset_at: reset.to_string(),
                    });
                }
            } else if let [repo, num, head, kind, bot, unres, checks, updated] =
                line.split('\t').collect::<Vec<_>>()[..]
            {
                let (Ok(number), Ok(unresolved), Some(bot), Some(checks)) = (
                    num.parse(),
                    unres.parse(),
                    BotState::parse(bot),
                    Checks::parse(checks),
                ) else {
                    continue;
                };
                t.rows.push(Row {
                    repo: repo.into(),
                    number,
                    head: head.into(),
                    draft: kind == "draft",
                    bot,
                    unresolved,
                    checks,
                    updated: updated.into(),
                });
            }
        }
        t
    }
}

/// One open pull request as a round reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrInfo {
    pub row: Row,
    pub note: Option<NoteState>,
    /// `created_at` of the `Ready` comment at this head that still stands
    /// ([`crate::bot::ready_mark`]): its place in Ready order (WORK-6).
    pub ready_at: Option<String>,
    /// `created_at` of the latest note comment that cleared Ready at this head.
    pub ready_cleared: Option<String>,
    pub url: String,
    pub review_url: Option<String>,
}

/// The coordinator's query: one request for every repository (aliases `r0`, `r1`, ...).
/// Comments are read from both ends, so the bot's early summary comment and its latest
/// word are both seen on a long pull request.
pub fn batch_query(n_repos: usize) -> String {
    let vars: String = (0..n_repos).map(|i| format!(", $r{i}: String!")).collect();
    let repos: String = (0..n_repos)
        .map(|i| format!("  r{i}: repository(owner: $owner, name: $r{i}) {{ ...R }}\n"))
        .collect();
    format!(
        "query($owner: String!{vars}) {{
  rateLimit {{ cost remaining resetAt limit }}
{repos}}}
fragment R on Repository {{
  name
  pullRequests(states: OPEN, first: 50, orderBy: {{field: CREATED_AT, direction: ASC}}) {{
    totalCount
    nodes {{
      number isDraft headRefOid headRefName baseRefName updatedAt url
      reviewThreads(first: 100) {{ totalCount nodes {{ isResolved }} }}
      early: comments(first: {EARLY}) {{ totalCount nodes {{ databaseId author {{ login }} body createdAt updatedAt }} }}
      late: comments(last: {LATE}) {{ nodes {{ databaseId author {{ login }} body createdAt updatedAt }} }}
      reviews(last: 30) {{ nodes {{ author {{ login }} commit {{ oid }} state url }} }}
      commits(last: 1) {{ nodes {{ commit {{ oid statusCheckRollup {{ state }} }} }} }}
    }}
  }}
}}"
    )
}

/// The `nodes` of the connection at `path`; none when absent.
pub(crate) fn nodes<'a>(v: &'a Value, path: &[&str]) -> &'a [Value] {
    let mut v = v;
    for p in path {
        match v.get(p) {
            Some(x) => v = x,
            None => return &[],
        }
    }
    v.get("nodes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn resp(msg: impl Into<String>) -> Error {
    Error::Response {
        what: "graphql batch".into(),
        msg: msg.into(),
    }
}

/// Rows of one batched response, in the order of `repos` then by number, the rate limit,
/// and the repositories the response had nothing for.
pub fn parse_batch(
    v: &Value,
    repos: &[String],
    bot: &str,
    note_authors: &[String],
    read: &str,
) -> Result<(Vec<PrInfo>, RateLimit, Vec<String>)> {
    let data = v.get("data").ok_or_else(|| resp("no data"))?;
    let rate = parse_rate(data)?;
    let mut out = Vec::new();
    let mut missing = Vec::new();
    for (i, repo) in repos.iter().enumerate() {
        let Some(r) = data.get(format!("r{i}")).filter(|r| !r.is_null()) else {
            missing.push(repo.clone());
            continue;
        };
        let mut infos: Vec<PrInfo> = nodes(r, &["pullRequests"])
            .iter()
            .filter_map(|p| pr_info(repo, p, bot, note_authors, read))
            .collect();
        infos.sort_by_key(|i| i.row.number);
        out.extend(infos);
    }
    Ok((out, rate, missing))
}

/// The `rateLimit` of a batched response's `data`.
pub fn parse_rate(data: &Value) -> Result<RateLimit> {
    let rl = data.get("rateLimit").ok_or_else(|| resp("no rateLimit"))?;
    Ok(RateLimit {
        cost: rl.get("cost").and_then(Value::as_u64).unwrap_or(0),
        remaining: rl
            .get("remaining")
            .and_then(Value::as_u64)
            .ok_or_else(|| resp("no rateLimit.remaining"))?,
        reset_at: rl
            .get("resetAt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// Comments read from each end of a pull request (`early`, `late` in [`batch_query`]).
const EARLY: usize = 10;
const LATE: usize = 30;

/// When the comments read from both ends do not meet, the `createdAt` of the first `late`
/// comment: the comments before it back to the `early` ones were not read. Without
/// `totalCount`, both full windows count as a gap. A gap with no `late` time to give is
/// `now` (fail closed: nothing before it is known).
fn unread_gap(p: &Value, read: usize, now: &str) -> Option<String> {
    let (early, late) = (nodes(p, &["early"]), nodes(p, &["late"]));
    let total = p
        .get("early")
        .and_then(|e| e.get("totalCount"))
        .and_then(Value::as_u64);
    let gap = match total {
        Some(t) => t > read as u64,
        None => early.len() >= EARLY && late.len() >= LATE,
    };
    if !gap {
        return None;
    }
    let first = late
        .iter()
        .filter_map(|c| c.get("createdAt").and_then(Value::as_str))
        .min();
    Some(first.unwrap_or(now).to_string())
}

fn pr_info(
    repo: &str,
    p: &Value,
    bot: &str,
    note_authors: &[String],
    read: &str,
) -> Option<PrInfo> {
    let number = p.get("number")?.as_u64()?;
    let head = p.get("headRefOid")?.as_str()?.to_string();
    let unresolved = nodes(p, &["reviewThreads"])
        .iter()
        .filter(|t| t.get("isResolved").and_then(Value::as_bool) == Some(false))
        .count() as u64;
    let mut seen = HashSet::new();
    let comments: Vec<Comment> = nodes(p, &["early"])
        .iter()
        .chain(nodes(p, &["late"]))
        .filter(|c| match c.get("databaseId").and_then(Value::as_u64) {
            Some(id) => seen.insert(id),
            None => true,
        })
        .map(Comment::from_graphql)
        .collect();
    let reviews: Vec<Review> = nodes(p, &["reviews"])
        .iter()
        .map(Review::from_graphql)
        .collect();
    let view = BotView::of(bot, &comments, &reviews);
    // Across an unread run only the late comments count: the run may hold a later note.
    let gap = unread_gap(p, comments.len(), read);
    let late: Vec<Comment>;
    let read_notes = match gap {
        Some(_) => {
            late = nodes(p, &["late"])
                .iter()
                .map(Comment::from_graphql)
                .collect();
            &late
        }
        None => &comments,
    };
    // Only the fleet's own comments change a note state (anyone can comment).
    let notes = by_note_authors(read_notes, note_authors);
    let mark = ready_mark_across(&notes, &head, gap.as_deref());
    let rollup = nodes(p, &["commits"])
        .first()
        .and_then(|c| c.get("commit"))
        .filter(|c| c.get("oid").and_then(Value::as_str) == Some(head.as_str()))
        .and_then(|c| c.get("statusCheckRollup"))
        .and_then(|r| r.get("state"))
        .and_then(Value::as_str);
    Some(PrInfo {
        row: Row {
            repo: repo.to_string(),
            number,
            draft: p.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
            bot: view.state_at(&head),
            unresolved,
            checks: Checks::from_rollup(rollup),
            updated: read.to_string(),
            head: head.clone(),
        },
        note: note_state(&notes),
        ready_at: mark.at,
        ready_cleared: mark.cleared,
        url: p
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        review_url: view.review_url(&head).map(String::from),
    })
}

/// A time as the protocol writes it: RFC 3339 (`2026-11-27T08:55:49Z`) or the compact
/// `20261127T085549Z`; minutes-only forms (`2026-11-27T08:55Z`) are read too.
pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    for f in ["%Y%m%dT%H%M%SZ", "%Y-%m-%dT%H:%MZ", "%Y-%m-%dT%H:%M:%SZ"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(s, f) {
            return Some(Utc.from_utc_datetime(&t));
        }
    }
    None
}

/// Whether a `hold` value still holds at `now`. A value that names no time holds.
pub fn hold_active(value: &str, now: DateTime<Utc>) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return false;
    }
    match parse_time(v.split_whitespace().next().unwrap_or(v)) {
        Some(t) => now < t,
        None => true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldChange {
    Set(String),
    Clear,
    Keep,
}

/// What a successful round does to `hold`: set it to the reset time when the allowance is
/// at or below `below`, clear one whose time has passed.
pub fn hold_after(
    rate: &RateLimit,
    below: u64,
    current: Option<&str>,
    now: DateTime<Utc>,
) -> HoldChange {
    if rate.remaining <= below && !rate.reset_at.is_empty() {
        if current.map(str::trim) == Some(rate.reset_at.as_str()) {
            HoldChange::Keep
        } else {
            HoldChange::Set(rate.reset_at.clone())
        }
    } else if current.is_some_and(|h| !hold_active(h, now)) {
        HoldChange::Clear
    } else {
        HoldChange::Keep
    }
}

/// The `hold` value after a rate-limited round: the reset time GitHub gave, else the last
/// known one when it is still ahead, else an hour from now.
pub fn hold_on_limit(
    hint: Option<DateTime<Utc>>,
    last: Option<&RateLimit>,
    now: DateTime<Utc>,
) -> String {
    let last = last
        .and_then(|r| parse_time(&r.reset_at))
        .filter(|t| *t > now);
    let t = hint
        .filter(|t| *t > now)
        .or(last)
        .unwrap_or(now + chrono::Duration::hours(1));
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// A change on a `ready` or `approved` pull request the controller must hear about now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub repo: String,
    pub number: u64,
    pub head: String,
    pub note: NoteState,
    pub finding: bool,
    pub failing: bool,
    pub url: String,
}

impl Alert {
    /// The note body (PROTO-11: the first line names the sender and the action).
    pub fn body(&self) -> String {
        let what = match (self.finding, self.failing) {
            (true, true) => "a review bot finding and a failing check",
            (true, false) => "a review bot finding",
            _ => "a failing check",
        };
        let state = match self.note {
            NoteState::Ready => "ready",
            _ => "approved",
        };
        format!(
            "[coord] Alert: {}#{} at {} ({state}) has {what} at this head.\n{}\n",
            self.repo,
            self.number,
            short(&self.head),
            self.url
        )
    }
}

/// Alerts for pull requests whose latest note is `ready` or `approved` and that gained a bot
/// finding or a failing check since the previous table (a new head counts as a change).
/// A pull request the previous table did not have raises nothing.
pub fn alerts(prev: &[Row], now: &[PrInfo]) -> Vec<Alert> {
    let before: BTreeMap<(&str, u64), &Row> = prev
        .iter()
        .map(|r| ((r.repo.as_str(), r.number), r))
        .collect();
    let mut out = Vec::new();
    for i in now {
        let Some(note) = i
            .note
            .filter(|n| matches!(n, NoteState::Ready | NoteState::Approved))
        else {
            continue;
        };
        let Some(p) = before.get(&(i.row.repo.as_str(), i.row.number)) else {
            continue;
        };
        let moved = p.head != i.row.head;
        let finding = i.row.bot == BotState::Findings && (moved || p.bot != BotState::Findings);
        let failing = i.row.checks == Checks::Fail && (moved || p.checks != Checks::Fail);
        if finding || failing {
            out.push(Alert {
                repo: i.row.repo.clone(),
                number: i.row.number,
                head: i.row.head.clone(),
                note,
                finding,
                failing,
                url: if finding {
                    i.review_url.clone().unwrap_or_else(|| i.url.clone())
                } else {
                    i.url.clone()
                },
            });
        }
    }
    out
}

/// A table older than this is not used to decide anything (docs/protocol.md §8).
pub const STALE_AFTER: chrono::Duration = chrono::Duration::minutes(15);

/// What a fresh table says about landing one pull request at one head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableWord {
    /// Read GitHub: the table is stale, has no row for it, names another head, or says it
    /// can land.
    Ask(String),
    /// The review bot reports its usage limit.
    Limit,
    /// Not yet: no GitHub call is worth making.
    NotYet(String),
}

/// The rule of the shell `land-when-ready`: land when the table shows no unresolved thread,
/// no failed check and the bot clean (or the findings triaged).
pub fn table_word(
    text: Option<&str>,
    repo: &str,
    number: u64,
    head: &str,
    triaged: bool,
    now: DateTime<Utc>,
) -> TableWord {
    let Some(text) = text else {
        return TableWord::Ask("no table".into());
    };
    let t = Table::parse(text);
    match parse_time(&t.read) {
        Some(read) if now - read <= STALE_AFTER => {}
        _ => return TableWord::Ask("the table is stale".into()),
    }
    let Some(r) = t.rows.iter().find(|r| r.repo == repo && r.number == number) else {
        return TableWord::Ask("no row for it".into());
    };
    if r.bot == BotState::Limit {
        return TableWord::Limit;
    }
    if r.head != head {
        return TableWord::Ask(format!("the table shows head {}", short(&r.head)));
    }
    let mut why = Vec::new();
    if r.unresolved != 0 {
        why.push(format!("{} unresolved", r.unresolved));
    }
    if r.checks == Checks::Fail {
        why.push("a check failed".to_string());
    }
    if r.bot != BotState::Clean && !triaged {
        why.push(format!("bot {}", r.bot.as_str()));
    }
    if why.is_empty() {
        TableWord::Ask("the table says it can land".into())
    } else {
        TableWord::NotYet(why.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{fixture, json};

    const BOT: &str = "chatgpt-codex-connector";
    const READ: &str = "20260105T120000Z";

    fn repos() -> Vec<String> {
        ["alpha", "beta", "gamma"].map(String::from).to_vec()
    }

    fn batch() -> (Vec<PrInfo>, RateLimit, Vec<String>) {
        parse_batch(&json("coord-batch.json"), &repos(), BOT, &fleet(), READ).unwrap()
    }

    /// The fixtures' fleet account.
    fn fleet() -> Vec<String> {
        vec!["octo".into()]
    }

    /// alpha#11 of the fixture with more late comments: `(author, body, created_at)`.
    fn alpha11_with(extra: &[(&str, &str, &str)]) -> PrInfo {
        let mut v = json("coord-batch.json");
        let late = v
            .pointer_mut("/data/r0/pullRequests/nodes")
            .and_then(Value::as_array_mut)
            .unwrap()
            .iter_mut()
            .find(|p| p["number"] == 11)
            .unwrap()
            .pointer_mut("/late/nodes")
            .and_then(Value::as_array_mut)
            .unwrap();
        for (i, (author, body, at)) in extra.iter().enumerate() {
            late.push(serde_json::json!({
                "databaseId": 900_000 + i, "author": {"login": author}, "body": body,
                "createdAt": at, "updatedAt": at,
            }));
        }
        let (infos, ..) = parse_batch(&v, &repos(), BOT, &fleet(), READ).unwrap();
        infos.into_iter().find(|i| i.row.number == 11).unwrap()
    }

    /// alpha#11 of the fixture as a round reads it when `early` reports `total` comments; its
    /// `Ready` (the first comment) only in `early` when `total` is over 2.
    fn alpha11_total(total: Option<u64>, extra: &[(&str, &str, &str)]) -> PrInfo {
        alpha11_shaped(total, extra, |_| {})
    }

    /// [`alpha11_total`], with `shape` applied to the pull request's JSON last.
    fn alpha11_shaped(
        total: Option<u64>,
        extra: &[(&str, &str, &str)],
        shape: impl Fn(&mut Value),
    ) -> PrInfo {
        let mut v = json("coord-batch.json");
        let pr = v
            .pointer_mut("/data/r0/pullRequests/nodes")
            .and_then(Value::as_array_mut)
            .unwrap()
            .iter_mut()
            .find(|p| p["number"] == 11)
            .unwrap();
        match total {
            Some(t) => pr["early"]["totalCount"] = t.into(),
            None => {
                pr["early"].as_object_mut().unwrap().remove("totalCount");
            }
        }
        let late = pr["late"]["nodes"].as_array_mut().unwrap();
        if total.is_some_and(|t| t > 2) {
            late.remove(0);
        }
        for (i, (author, body, at)) in extra.iter().enumerate() {
            late.push(serde_json::json!({
                "databaseId": 900_000 + i, "author": {"login": author}, "body": body,
                "createdAt": at, "updatedAt": at,
            }));
        }
        shape(pr);
        let (infos, ..) = parse_batch(&v, &repos(), BOT, &fleet(), READ).unwrap();
        infos.into_iter().find(|i| i.row.number == 11).unwrap()
    }

    #[test]
    fn across_a_gap_early_comments_are_not_read() {
        // The early Ready in the same second as the first late comment: still not read.
        let same = alpha11_shaped(Some(99), &[], |pr| {
            pr["early"]["nodes"][0]["createdAt"] = "2026-01-05T09:01:00Z".into();
        });
        assert_eq!((same.note, &same.ready_at), (None, &None));
        assert_eq!(same.ready_cleared.as_deref(), Some("2026-01-05T09:01:00Z"));
        // Fail closed: a gap with no late comment, or none with a time, clears at the read.
        for shape in [
            (|pr: &mut Value| pr["late"]["nodes"] = serde_json::json!([])) as fn(&mut Value),
            |pr: &mut Value| {
                for c in pr["late"]["nodes"].as_array_mut().unwrap() {
                    c.as_object_mut().unwrap().remove("createdAt");
                }
            },
        ] {
            let i = alpha11_shaped(Some(99), &[], shape);
            assert_eq!((i.note, &i.ready_at), (None, &None));
            assert_eq!(i.ready_cleared.as_deref(), Some(READ));
        }
    }

    #[test]
    fn an_unread_run_of_comments_clears_ready() {
        let read = alpha11_total(None, &[]);
        let n = 2; // the fixture's alpha#11 comments, each in both windows
        assert_eq!(read.ready_at.as_deref(), Some("2026-01-05T09:00:00Z"));
        assert_eq!(
            alpha11_total(Some(n), &[]).ready_at,
            read.ready_at,
            "all read"
        );
        // More comments than were read: the first late comment starts after a gap.
        let gapped = alpha11_total(Some(n + 1), &[]);
        assert_eq!(gapped.ready_at, None);
        assert_eq!(gapped.note, None, "the latest note may be in the gap");
        let first_late = "2026-01-05T09:01:00Z";
        assert_eq!(gapped.ready_cleared.as_deref(), Some(first_late));
        // An approval after the gap does not bring the early Ready back; a Ready does.
        let approved = [("octo", "[claude] Approved.", "2026-01-05T10:00:00Z")];
        let after = alpha11_total(Some(99), &approved);
        assert_eq!(
            (after.note, after.ready_at),
            (Some(NoteState::Approved), None)
        );
        let ready = [(
            "octo",
            "[codex] Ready: alpha#11 at 11111111",
            "2026-01-05T11:00:00Z",
        )];
        assert_eq!(
            alpha11_total(Some(99), &ready).ready_at.as_deref(),
            Some("2026-01-05T11:00:00Z")
        );
    }

    #[test]
    fn only_the_fleets_comments_are_notes() {
        const READY_AT: &str = "2026-01-05T09:00:00Z";
        const T10: &str = "2026-01-05T10:00:00Z";
        const T11: &str = "2026-01-05T11:00:00Z";
        const BACK: &str = "[claude] Sent back: no";
        const READY: &str = "[codex] Ready: alpha#11 at 11111111";
        // The fixture's Ready is by the fleet account; read with another fleet, it is not.
        let (infos, ..) = parse_batch(
            &json("coord-batch.json"),
            &repos(),
            BOT,
            &["x".into()],
            READ,
        )
        .unwrap();
        let alpha11 = infos.iter().find(|i| i.row.number == 11).unwrap();
        assert_eq!((alpha11.note, &alpha11.ready_at), (None, &None));
        // A stranger's later Sent back changes nothing; the fleet's clears Ready.
        let foreign = alpha11_with(&[("stranger", BACK, T10)]);
        assert_eq!(foreign.note, Some(NoteState::Ready));
        assert_eq!(foreign.ready_at.as_deref(), Some(READY_AT));
        assert_eq!(foreign.ready_cleared, None);
        let own = alpha11_with(&[("octo", BACK, T10)]);
        assert_eq!(own.note, Some(NoteState::Sendback));
        assert_eq!(
            (own.ready_at, own.ready_cleared.as_deref()),
            (None, Some(T10))
        );
        // After the fleet's Sent back, a stranger's Ready at the head gives no place back;
        // the fleet's does.
        let foreign = alpha11_with(&[("octo", BACK, T10), ("stranger", READY, T11)]);
        assert_eq!(foreign.note, Some(NoteState::Sendback));
        assert_eq!(foreign.ready_at, None);
        let own = alpha11_with(&[("octo", BACK, T10), ("octo", READY, T11)]);
        assert_eq!(own.note, Some(NoteState::Ready));
        assert_eq!(own.ready_at.as_deref(), Some(T11));
    }

    #[test]
    fn rows_from_a_batch() {
        let (infos, rate, missing) = batch();
        assert_eq!(
            rate,
            RateLimit {
                cost: 13,
                remaining: 4945,
                reset_at: "2026-01-05T12:55:49Z".into()
            }
        );
        assert_eq!(missing, ["gamma"]);
        let t = Table {
            read: READ.into(),
            rate: Some(rate),
            rows: infos.iter().map(|i| i.row.clone()).collect(),
        };
        assert_eq!(t.render(), fixture("coord-batch.prs"));
        let by = |n: u64| infos.iter().find(|i| i.row.number == n).unwrap();
        assert_eq!(by(11).note, Some(NoteState::Ready));
        assert_eq!(by(12).note, Some(NoteState::Approved));
        assert_eq!(
            by(12).review_url.as_deref(),
            Some("https://github.com/o/alpha/pull/12#pullrequestreview-9")
        );
        assert_eq!(by(13).note, None);
        // Ready order (WORK-6): the Ready comment at the head; an approval with no Ready
        // comment read gives no place.
        assert_eq!(by(11).ready_at.as_deref(), Some("2026-01-05T09:00:00Z"));
        assert_eq!(by(12).ready_at, None);
        assert_eq!(by(13).ready_at, None);
    }

    #[test]
    fn table_round_trip() {
        let text = fixture("coord-batch.prs");
        let t = Table::parse(&text);
        assert_eq!(t.render(), text);
        assert_eq!(t.rows.len(), 6);
        assert!(Table::parse("garbage\nalpha\tx\n").rows.is_empty());
    }

    #[test]
    fn query_names_every_repo() {
        let q = batch_query(3);
        assert!(
            q.contains("$r2: String!") && q.contains("r2: repository(owner: $owner, name: $r2)")
        );
        assert!(!q.contains("$r3"));
        assert!(q.contains("rateLimit { cost remaining resetAt limit }"));
    }

    #[test]
    fn rollup_states() {
        assert_eq!(Checks::from_rollup(None), Checks::None);
        assert_eq!(Checks::from_rollup(Some("SUCCESS")), Checks::Pass);
        assert_eq!(Checks::from_rollup(Some("ERROR")), Checks::Fail);
        assert_eq!(Checks::from_rollup(Some("EXPECTED")), Checks::Pending);
    }

    fn at(s: &str) -> DateTime<Utc> {
        parse_time(s).unwrap()
    }

    #[test]
    fn hold() {
        let now = at("2026-01-05T12:00:00Z");
        assert!(hold_active("2026-01-05T12:55:49Z", now));
        assert!(hold_active("20260105T125549Z", now));
        assert!(!hold_active("2026-01-05T11:59Z", now));
        assert!(
            hold_active("until the owner says", now),
            "a hold without a time holds"
        );
        assert!(!hold_active("", now));
        let low = RateLimit {
            cost: 13,
            remaining: 40,
            reset_at: "2026-01-05T12:55:49Z".into(),
        };
        let ok = RateLimit {
            remaining: 4000,
            ..low.clone()
        };
        assert_eq!(
            hold_after(&low, 100, None, now),
            HoldChange::Set("2026-01-05T12:55:49Z".into())
        );
        assert_eq!(
            hold_after(&low, 100, Some("2026-01-05T12:55:49Z"), now),
            HoldChange::Keep
        );
        assert_eq!(
            hold_after(&ok, 100, Some("2026-01-05T11:00:00Z"), now),
            HoldChange::Clear
        );
        assert_eq!(hold_after(&ok, 100, None, now), HoldChange::Keep);
        assert_eq!(
            hold_on_limit(Some(at("2026-01-05T12:30:00Z")), Some(&low), now),
            "2026-01-05T12:30:00Z"
        );
        assert_eq!(hold_on_limit(None, Some(&low), now), "2026-01-05T12:55:49Z");
        assert_eq!(hold_on_limit(None, None, now), "2026-01-05T13:00:00Z");
    }

    #[test]
    fn landing_from_the_table() {
        let text = fixture("coord-batch.prs");
        let now = at("2026-01-05T12:05:00Z");
        let h = |c: &str| c.repeat(40);
        let w = |n: u64, head: &str, triaged: bool| {
            table_word(Some(&text), "alpha", n, head, triaged, now)
        };
        assert_eq!(
            w(11, &h("1"), false),
            TableWord::NotYet("2 unresolved, bot findings".into())
        );
        assert_eq!(
            w(12, &h("2"), false),
            TableWord::NotYet("a check failed".into())
        );
        assert_eq!(
            w(13, &h("3"), true),
            TableWord::NotYet("1 unresolved".into())
        );
        assert!(
            matches!(w(14, &h("4"), false), TableWord::NotYet(_)),
            "a draft has no review"
        );
        assert!(
            matches!(w(12, &h("9"), false), TableWord::Ask(_)),
            "another head"
        );
        assert!(matches!(w(99, &h("9"), false), TableWord::Ask(_)), "no row");
        assert_eq!(
            table_word(Some(&text), "beta", 3, &"a3".repeat(20), false, now),
            TableWord::Limit
        );
        let late = at("2026-01-05T12:20:01Z");
        assert!(
            matches!(
                table_word(Some(&text), "alpha", 11, &h("1"), false, late),
                TableWord::Ask(_)
            ),
            "stale"
        );
        assert!(matches!(
            table_word(None, "alpha", 11, &h("1"), false, now),
            TableWord::Ask(_)
        ));
        let ready = text.replace("open\tclean\t0\tfail", "open\tclean\t0\tpass");
        assert!(
            matches!(table_word(Some(&ready), "alpha", 12, &h("2"), false, now), TableWord::Ask(w) if w.contains("can land"))
        );
    }

    #[test]
    fn alert_diffing() {
        let (infos, _, _) = batch();
        let prev = Table::parse(&fixture("coord-prev.prs")).rows;
        let a = alerts(&prev, &infos);
        // 11: ready, bot clean before, findings now. 12: approved, checks pass before, fail now.
        // 13: findings but no note. 14: draft, not in the previous table.
        let got: Vec<(u64, bool, bool)> =
            a.iter().map(|x| (x.number, x.finding, x.failing)).collect();
        assert_eq!(got, [(11, true, false), (12, false, true)]);
        assert!(
            a[0].body().starts_with(
                "[coord] Alert: alpha#11 at 11111111 (ready) has a review bot finding"
            )
        );
        assert!(a[1].body().contains("(approved) has a failing check"));
        // The same state again raises nothing.
        let same: Vec<Row> = infos.iter().map(|i| i.row.clone()).collect();
        assert!(alerts(&same, &infos).is_empty());
        // A new head with the same failing state does.
        let mut moved = same.clone();
        moved.iter_mut().find(|r| r.number == 12).unwrap().head = "f".repeat(40);
        assert_eq!(alerts(&moved, &infos).len(), 1);
    }
}
