//! `HGL_STATE/prs` (docs/protocol.md §8): one batched GraphQL read of every open pull request
//! of the configured repositories, rendered as
//!
//! ```text
//! # read <UTC>
//! # cost <n> remaining <n> resetAt <ISO>
//! REPO<TAB>NUMBER<TAB>HEAD<TAB>draft|open<TAB>BOT<TAB>UNRESOLVED<TAB>CHECKS<TAB>UPDATED
//! # pr<TAB>REPO<TAB>NUMBER<TAB>PASSED<TAB>TOTAL<TAB>TITLE
//! ```
//!
//! UPDATED is the UTC time of the reading, in the compact form of a note's `created_at`.
//! The `# pr` line after a row gives its checks passed and in all (`-` when GitHub gave no
//! rollup) and its title, for the console's page; a reader of the rows alone skips it.
//! Also here: `hold` from the GraphQL allowance, and the alerts a round raises.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;

use crate::backlog::clean_title;
use crate::bot::{BotState, BotView, Comment, NoteState, Review, note_state};
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
    /// The `# pr` line: the head's checks passed and in all, and the title.
    pub detail: PrDetail,
}

/// What the `# pr` line after a row adds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrDetail {
    /// Check runs and status contexts passed (success, neutral, skipped) and in all.
    pub counts: Option<(u64, u64)>,
    pub title: String,
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
            let d = &r.detail;
            if d.counts.is_some() || !d.title.is_empty() {
                let (p, n) = d.counts.map_or(("-".into(), "-".into()), |(p, n)| {
                    (p.to_string(), n.to_string())
                });
                s.push_str(&format!(
                    "# pr\t{}\t{}\t{p}\t{n}\t{}\n",
                    r.repo,
                    r.number,
                    clean_title(&d.title)
                ));
            }
        }
        s
    }

    /// Parse a table written earlier; lines that do not parse are skipped.
    pub fn parse(text: &str) -> Table {
        let mut t = Table::default();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# pr\t") {
                if let [repo, num, p, n, title] = rest.splitn(5, '\t').collect::<Vec<_>>()[..]
                    && let Some(r) = t
                        .rows
                        .iter_mut()
                        .rev()
                        .find(|r| r.repo == repo && num.parse() == Ok(r.number))
                {
                    r.detail = PrDetail {
                        counts: p.parse().ok().zip(n.parse().ok()),
                        title: title.to_string(),
                    };
                }
            } else if let Some(rest) = line.strip_prefix("# read ") {
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
                    detail: PrDetail::default(),
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
      number title isDraft headRefOid headRefName baseRefName updatedAt url
      reviewThreads(first: 100) {{ totalCount nodes {{ isResolved }} }}
      early: comments(first: 10) {{ nodes {{ databaseId author {{ login }} body createdAt updatedAt }} }}
      late: comments(last: 30) {{ nodes {{ databaseId author {{ login }} body createdAt updatedAt }} }}
      reviews(last: 30) {{ nodes {{ author {{ login }} commit {{ oid }} state url }} }}
      commits(last: 1) {{ nodes {{ commit {{ oid statusCheckRollup {{ state contexts(first: 0) {{
        checkRunCount checkRunCountsByState {{ state count }}
        statusContextCount statusContextCountsByState {{ state count }}
      }} }} }} }} }}
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
            .filter_map(|p| pr_info(repo, p, bot, read))
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

fn pr_info(repo: &str, p: &Value, bot: &str, read: &str) -> Option<PrInfo> {
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
    let rollup = nodes(p, &["commits"])
        .first()
        .and_then(|c| c.get("commit"))
        .filter(|c| c.get("oid").and_then(Value::as_str) == Some(head.as_str()))
        .and_then(|c| c.get("statusCheckRollup"));
    let state = rollup.and_then(|r| r.get("state")).and_then(Value::as_str);
    Some(PrInfo {
        row: Row {
            repo: repo.to_string(),
            number,
            draft: p.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
            bot: view.state_at(&head),
            unresolved,
            checks: Checks::from_rollup(state),
            updated: read.to_string(),
            head: head.clone(),
            detail: PrDetail {
                counts: rollup.and_then(check_counts),
                title: p.get("title").and_then(Value::as_str).unwrap_or("").into(),
            },
        },
        note: note_state(&comments),
        url: p
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        review_url: view.review_url(&head).map(String::from),
    })
}

/// Checks passed and in all of a `statusCheckRollup`: check runs that succeeded, were
/// neutral or skipped, and status contexts that succeeded.
fn check_counts(rollup: &Value) -> Option<(u64, u64)> {
    let c = rollup.get("contexts")?;
    let n = |k: &str| c.get(k).and_then(Value::as_u64);
    let passed = |k: &str, ok: &[&str]| -> u64 {
        c.get(k)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|s| {
                s.get("state")
                    .and_then(Value::as_str)
                    .is_some_and(|s| ok.contains(&s))
            })
            .filter_map(|s| s.get("count").and_then(Value::as_u64))
            .sum()
    };
    let total = n("checkRunCount")? + n("statusContextCount").unwrap_or(0);
    let ok = passed("checkRunCountsByState", &["SUCCESS", "NEUTRAL", "SKIPPED"])
        + passed("statusContextCountsByState", &["SUCCESS"]);
    Some((ok, total))
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
        parse_batch(&json("coord-batch.json"), &repos(), BOT, READ).unwrap()
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
    }

    #[test]
    fn table_round_trip() {
        let text = fixture("coord-batch.prs");
        let t = Table::parse(&text);
        assert_eq!(t.render(), text);
        assert_eq!(t.rows.len(), 6);
        assert!(Table::parse("garbage\nalpha\tx\n").rows.is_empty());
        let r = &t.rows[0];
        assert_eq!(r.detail.counts, Some((3, 3)));
        assert_eq!(r.detail.title, "Parser: tabs in titles");
        // No rollup, a line before its row or for no row: nothing, and nothing written.
        let row = "alpha\t9\t9999\topen\tnone\t0\tnone\tT\n";
        let t = Table::parse(&format!(
            "# pr\talpha\t9\t1\t1\tEarly\n{row}# pr\tbeta\t9\t1\t1\tOther\n"
        ));
        assert_eq!(t.rows[0].detail, PrDetail::default());
        assert_eq!(t.render(), format!("# read \n{row}"));
        let t = Table::parse(&format!("{row}# pr\talpha\t9\t-\t-\tTitle only\n"));
        assert_eq!(t.rows[0].detail.counts, None);
        assert_eq!(
            t.render(),
            format!("# read \n{row}# pr\talpha\t9\t-\t-\tTitle only\n")
        );
    }

    #[test]
    fn check_counts_of_a_rollup() {
        let v = serde_json::json!({"state": "FAILURE", "contexts": {
            "checkRunCount": 4,
            "checkRunCountsByState": [
                {"state": "SUCCESS", "count": 1}, {"state": "NEUTRAL", "count": 1},
                {"state": "SKIPPED", "count": 1}, {"state": "FAILURE", "count": 1}],
            "statusContextCount": 2,
            "statusContextCountsByState": [{"state": "SUCCESS", "count": 1}, {"state": "PENDING", "count": 1}]
        }});
        assert_eq!(check_counts(&v), Some((4, 6)));
        assert_eq!(check_counts(&serde_json::json!({"state": "SUCCESS"})), None);
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
