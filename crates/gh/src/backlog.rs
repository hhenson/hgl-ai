//! `HGL_STATE/backlog` and the daily `HGL_METRICS` samples (docs/owner.md §3): one batched
//! GraphQL read of the open issues of every configured repository, every 15 minutes, rendered as
//!
//! ```text
//! # read <UTC>
//! # open <repo> <count>
//! # label <repo> <label> <count>
//! REPO<TAB>NUMBER<TAB>codex-priority|codex<TAB>CREATED<TAB>TITLE
//! ```
//!
//! One `open` line per repository; one `label` line per repository and label with open
//! issues, sorted (a label may contain spaces: the repository is the first word, the count the
//! last). The rows are the claimable queue in order (docs/work.md WORK-1, WORK-3): every issue
//! labelled `codex-priority`, then those labelled `codex`, each oldest first across the
//! repositories (ties in the configured order of repositories, then by number). An issue also
//! labelled `codex-claimed`, `codex-blocked`, `needs-owner` or `codex-ready` is left out.
//! Times are UTC in the compact form of a note's `created_at`. A title has tabs, CR and LF as
//! spaces and at most 200 characters.
//!
//! The same read gives each repository's sample of the UTC day, `HGL_METRICS/<repo>.<YYYYMMDD>`:
//! open issues, issues opened and closed that day, open issues per label. Every round rewrites
//! it, so the day's last round is the day's sample.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use hgl_ai_bus::Bus;
use hgl_ai_proto::{KV_METRICS, MetricsSample, utc_stamp};
use serde_json::Value;

use crate::exec::{self, Gh};
use crate::table::{self, RateLimit, nodes};
use crate::{Error, Result, Settings, state, store};

/// How often the backlog round runs (docs/owner.md §3).
pub const BACKLOG_EVERY: Duration = Duration::from_secs(15 * 60);

/// Claimable, and taken before `codex` (WORK-1).
pub const PRIORITY: &str = "codex-priority";
/// Claimable.
pub const CLAIMABLE: &str = "codex";
/// Labels that keep a claimable issue out of the queue.
pub const NOT_CLAIMABLE: [&str; 4] = [
    "codex-claimed",
    "codex-blocked",
    "needs-owner",
    "codex-ready",
];
/// The longest title a row keeps, in characters.
pub const TITLE_MAX: usize = 200;

/// One claimable issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRow {
    pub repo: String,
    pub number: u64,
    /// Labelled `codex-priority`; else `codex`.
    pub priority: bool,
    /// When the issue was opened, compact UTC.
    pub created: String,
    pub title: String,
}

impl QueueRow {
    /// The row's class column.
    pub fn class(&self) -> &'static str {
        if self.priority { PRIORITY } else { CLAIMABLE }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Backlog {
    pub read: String,
    /// Open issues per repository.
    pub open: BTreeMap<String, u64>,
    /// Open issues per repository and label; a label with none is absent.
    pub labels: BTreeMap<(String, String), u64>,
    /// The claimable queue, in the order it is taken.
    pub queue: Vec<QueueRow>,
}

/// Tabs, CR and LF as spaces.
fn one_line(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ")
}

/// A title as a row keeps it: one line, at most [`TITLE_MAX`] characters.
pub fn clean_title(t: &str) -> String {
    one_line(t).chars().take(TITLE_MAX).collect()
}

impl Backlog {
    pub fn render(&self) -> String {
        let mut s = format!("# read {}\n", self.read);
        for (repo, n) in &self.open {
            s.push_str(&format!("# open {repo} {n}\n"));
        }
        for ((repo, label), n) in self.labels.iter().filter(|(_, n)| **n > 0) {
            s.push_str(&format!("# label {repo} {} {n}\n", one_line(label)));
        }
        for r in &self.queue {
            s.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                r.repo,
                r.number,
                r.class(),
                r.created,
                clean_title(&r.title)
            ));
        }
        s
    }

    /// Parse a backlog written earlier; lines that do not parse are skipped.
    pub fn parse(text: &str) -> Backlog {
        let mut b = Backlog::default();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# read ") {
                b.read = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("# open ") {
                if let [repo, n] = rest.split_whitespace().collect::<Vec<_>>()[..]
                    && let Ok(n) = n.parse()
                {
                    b.open.insert(repo.to_string(), n);
                }
            } else if let Some(rest) = line.strip_prefix("# label ") {
                if let Some((repo, rest)) = rest.split_once(' ')
                    && let Some((label, n)) = rest.rsplit_once(' ')
                    && !repo.is_empty()
                    && !label.is_empty()
                    && let Ok(n) = n.parse()
                {
                    b.labels.insert((repo.to_string(), label.to_string()), n);
                }
            } else if let [repo, num, class, created, title] =
                line.splitn(5, '\t').collect::<Vec<_>>()[..]
            {
                let priority = match class {
                    PRIORITY => true,
                    CLAIMABLE => false,
                    _ => continue,
                };
                let Ok(number) = num.parse() else {
                    continue;
                };
                if repo.is_empty() || created.is_empty() {
                    continue;
                }
                b.queue.push(QueueRow {
                    repo: repo.to_string(),
                    number,
                    priority,
                    created: created.to_string(),
                    title: title.to_string(),
                });
            }
        }
        b
    }
}

/// One round's read: the backlog, each repository's sample of the day, the allowance, and
/// the repositories the response had nothing for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub backlog: Backlog,
    pub samples: Vec<MetricsSample>,
    pub rate: RateLimit,
    pub missing: Vec<String>,
}

impl Reading {
    /// What a dry run prints: the backlog, then a line per sample.
    pub fn dry_run(&self) -> String {
        let mut s = self.backlog.render();
        for m in &self.samples {
            s.push_str(&format!(
                "would write {KV_METRICS}/{}: open {} opened {} closed {}\n",
                m.key(),
                m.open,
                m.opened,
                m.closed
            ));
        }
        s
    }
}

/// The backlog query: one request for every repository (aliases `r0`, `r1`, ...), with the
/// day's issue searches beside each (`opened0`, `closed0`, ...).
pub fn batch_query(n_repos: usize) -> String {
    let vars: String = (0..n_repos)
        .map(|i| format!(", $r{i}: String!, $opened{i}: String!, $closed{i}: String!"))
        .collect();
    let fields: String = (0..n_repos)
        .map(|i| {
            format!(
                "  r{i}: repository(owner: $owner, name: $r{i}) {{ ...B }}
  opened{i}: search(query: $opened{i}, type: ISSUE, first: 1) {{ issueCount }}
  closed{i}: search(query: $closed{i}, type: ISSUE, first: 1) {{ issueCount }}
"
            )
        })
        .collect();
    format!(
        "query Backlog($owner: String!{vars}) {{
  rateLimit {{ cost remaining resetAt limit }}
{fields}}}
fragment B on Repository {{
  open: issues(states: OPEN) {{ totalCount }}
  labels(first: 100) {{ nodes {{ name issues(states: OPEN) {{ totalCount }} }} }}
  queue: issues(states: OPEN, labels: [\"{PRIORITY}\", \"{CLAIMABLE}\"], first: 100, orderBy: {{field: CREATED_AT, direction: ASC}}) {{
    nodes {{ number title createdAt labels(first: 30) {{ nodes {{ name }} }} }}
  }}
}}"
    )
}

/// The query's variables for the UTC day of `now`.
pub fn variables(s: &Settings, now: DateTime<Utc>) -> Vec<(String, String)> {
    let day = now.format("%Y-%m-%d");
    let mut vars = vec![("owner".to_string(), s.owner.clone())];
    for (i, r) in s.repos.iter().enumerate() {
        let full = s.full(r);
        vars.push((format!("r{i}"), r.clone()));
        vars.push((
            format!("opened{i}"),
            format!("repo:{full} is:issue created:{day}"),
        ));
        vars.push((
            format!("closed{i}"),
            format!("repo:{full} is:issue closed:{day}"),
        ));
    }
    vars
}

fn resp(msg: impl Into<String>) -> Error {
    Error::Response {
        what: "graphql backlog".into(),
        msg: msg.into(),
    }
}

/// A queue candidate: (number, priority, created, title), or `None` when not claimable.
fn candidate(n: &Value) -> Option<(u64, bool, DateTime<Utc>, String)> {
    let number = n.get("number")?.as_u64()?;
    let labels: Vec<&str> = nodes(n, &["labels"])
        .iter()
        .filter_map(|l| l.get("name").and_then(Value::as_str))
        .collect();
    let has = |l: &str| labels.iter().any(|x| x.eq_ignore_ascii_case(l));
    if NOT_CLAIMABLE.iter().any(|l| has(l)) {
        return None;
    }
    let priority = has(PRIORITY);
    if !priority && !has(CLAIMABLE) {
        return None;
    }
    let created = table::parse_time(n.get("createdAt")?.as_str()?)?;
    let title = n.get("title").and_then(Value::as_str).unwrap_or("");
    Some((number, priority, created, clean_title(title)))
}

fn count(v: &Value, path: &[&str]) -> Option<u64> {
    let mut v = v;
    for p in path {
        v = v.get(p)?;
    }
    v.as_u64()
}

/// The reading of one batched response at `now` (the `# read` stamp, the samples' day and
/// `taken_at`). A repository counts as missing unless its repository and both searches came;
/// a response with none fails (an empty queue would say nothing is claimable).
pub fn parse_batch(v: &Value, repos: &[String], now: DateTime<Utc>) -> Result<Reading> {
    let data = v.get("data").ok_or_else(|| resp("no data"))?;
    let rate = table::parse_rate(data)?;
    let read = utc_stamp(now);
    let date = now.format("%Y%m%d").to_string();
    let mut b = Backlog {
        read: read.clone(),
        ..Backlog::default()
    };
    let mut samples = Vec::new();
    let mut missing = Vec::new();
    let mut queue = Vec::new();
    for (i, repo) in repos.iter().enumerate() {
        let r = data.get(format!("r{i}")).filter(|r| !r.is_null());
        let got = r.and_then(|r| {
            Some((
                r,
                count(r, &["open", "totalCount"])?,
                count(data, &[&format!("opened{i}"), "issueCount"])?,
                count(data, &[&format!("closed{i}"), "issueCount"])?,
            ))
        });
        let Some((r, open, opened, closed)) = got else {
            missing.push(repo.clone());
            continue;
        };
        let mut labels = BTreeMap::new();
        for l in nodes(r, &["labels"]) {
            if let (Some(name), Some(n)) = (
                l.get("name").and_then(Value::as_str),
                count(l, &["issues", "totalCount"]),
            ) && n > 0
            {
                labels.insert(name.to_string(), n);
                b.labels.insert((repo.clone(), name.to_string()), n);
            }
        }
        b.open.insert(repo.clone(), open);
        queue.extend(
            nodes(r, &["queue"])
                .iter()
                .filter_map(candidate)
                .map(|(number, priority, created, title)| (i, number, priority, created, title)),
        );
        samples.push(MetricsSample {
            repo: repo.clone(),
            date: date.clone(),
            open,
            opened,
            closed,
            labels,
            taken_at: read.clone(),
        });
    }
    if !repos.is_empty() && missing.len() == repos.len() {
        return Err(resp("no repository answered"));
    }
    queue.sort_by_key(|(i, number, priority, created, _)| (!*priority, *created, *i, *number));
    b.queue = queue
        .into_iter()
        .map(|(i, number, priority, created, title)| QueueRow {
            repo: repos[i].clone(),
            number,
            priority,
            created: utc_stamp(created),
            title,
        })
        .collect();
    Ok(Reading {
        backlog: b,
        samples,
        rate,
        missing,
    })
}

/// Read the backlog of every configured repository from GitHub, as at `now`.
pub async fn read<G: Gh>(gh: &G, s: &Settings, now: DateTime<Utc>) -> Result<Reading> {
    let q = batch_query(s.repos.len());
    let vars = variables(s, now);
    let vars: Vec<(&str, String)> = vars.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    let pages = exec::graphql(gh, &q, &vars, false).await?;
    parse_batch(&pages[0], &s.repos, now)
}

/// Write a reading to the hub: `backlog`, then each repository's sample of the day.
pub async fn publish(b: &Bus, r: &Reading) -> Result<()> {
    store::put_latest(b, state::KEY_BACKLOG, &r.backlog.render()).await?;
    for m in &r.samples {
        b.metrics_put(m).await?;
    }
    Ok(())
}

/// Whether a backlog round is due at `now`: none has run, or `every` has passed since the last.
pub fn due(last: Option<Instant>, now: Instant, every: Duration) -> bool {
    last.is_none_or(|t| now.saturating_duration_since(t) >= every)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{fixture, json, settings};

    fn repos() -> Vec<String> {
        ["alpha", "beta", "gamma"].map(String::from).to_vec()
    }

    fn now() -> DateTime<Utc> {
        table::parse_time("2026-01-05T12:00:00Z").unwrap()
    }

    fn reading() -> Reading {
        parse_batch(&json("backlog-batch.json"), &repos(), now()).unwrap()
    }

    #[test]
    fn query_and_variables() {
        let q = batch_query(3);
        assert!(q.starts_with("query Backlog($owner: String!, $r0: String!, $opened0: String!"));
        assert!(q.contains("$r2: String!, $opened2: String!, $closed2: String!)"));
        assert!(!q.contains("$r3"));
        assert!(q.contains("r2: repository(owner: $owner, name: $r2) { ...B }"));
        assert!(
            q.contains("opened2: search(query: $opened2, type: ISSUE, first: 1) { issueCount }")
        );
        assert!(
            q.contains("closed2: search(query: $closed2, type: ISSUE, first: 1) { issueCount }")
        );
        assert!(q.contains("rateLimit { cost remaining resetAt limit }"));
        assert!(q.contains("open: issues(states: OPEN) { totalCount }"));
        assert!(q.contains(
            "queue: issues(states: OPEN, labels: [\"codex-priority\", \"codex\"], first: 100, orderBy: {field: CREATED_AT, direction: ASC})"
        ));
        assert!(!q.trim_start().starts_with("mutation"));

        let mut s = settings();
        s.repos = repos();
        let v = variables(&s, now());
        let get = |k: &str| v.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(v.len(), 1 + 3 * 3);
        assert_eq!(get("owner"), Some("hhenson"));
        assert_eq!(get("r1"), Some("beta"));
        assert_eq!(
            get("opened1"),
            Some("repo:hhenson/beta is:issue created:2026-01-05")
        );
        assert_eq!(
            get("closed2"),
            Some("repo:hhenson/gamma is:issue closed:2026-01-05")
        );
    }

    #[test]
    fn a_batch_parses() {
        let r = reading();
        assert_eq!(
            r.rate,
            RateLimit {
                cost: 10,
                remaining: 4930,
                reset_at: "2026-01-05T12:55:49Z".into()
            }
        );
        assert!(r.missing.is_empty());
        assert_eq!(r.backlog.render(), fixture("backlog-batch.backlog"));
        let open: Vec<(&str, u64)> = r
            .backlog
            .open
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect();
        assert_eq!(open, [("alpha", 9), ("beta", 5), ("gamma", 3)]);
        assert_eq!(
            r.backlog
                .labels
                .get(&("alpha".into(), "good first issue".into())),
            Some(&2)
        );
        assert!(
            !r.backlog
                .labels
                .contains_key(&("alpha".into(), "wontfix".into())),
            "no open issue"
        );
    }

    #[test]
    fn the_queue_in_order() {
        let r = reading();
        let q: Vec<(&str, u64, &str)> = r
            .backlog
            .queue
            .iter()
            .map(|x| (x.repo.as_str(), x.number, x.class()))
            .collect();
        // Priority first, oldest first; the tie at 2026-01-01T10:00 in the configured order.
        // Out: alpha#102 claimed, beta#4 blocked, beta#6 needs-owner, gamma#20 ready.
        assert_eq!(
            q,
            [
                ("alpha", 105, "codex-priority"),
                ("beta", 8, "codex-priority"),
                ("alpha", 101, "codex"),
                ("beta", 3, "codex"),
                ("gamma", 21, "codex"),
                ("alpha", 107, "codex"),
            ]
        );
        let first = &r.backlog.queue[2];
        assert_eq!(first.created, "20260101T100000Z");
        assert_eq!(first.title, "Fix the parser for tabs");
        assert_eq!(r.backlog.queue[4].title, "Line one  line two");
        // A tie in another configured order follows it.
        let swapped = ["beta", "alpha", "gamma"].map(String::from).to_vec();
        let mut v = json("backlog-batch.json");
        let d = v["data"].as_object_mut().unwrap();
        let (r0, r1) = (d["r0"].clone(), d["r1"].clone());
        d.insert("r0".into(), r1);
        d.insert("r1".into(), r0);
        let r = parse_batch(&v, &swapped, now()).unwrap();
        let tie: Vec<(&str, u64)> = r.backlog.queue[2..5]
            .iter()
            .map(|x| (x.repo.as_str(), x.number))
            .collect();
        assert_eq!(tie, [("beta", 3), ("alpha", 101), ("gamma", 21)]);
    }

    #[test]
    fn titles() {
        assert_eq!(clean_title("a\tb\r\nc"), "a b  c");
        let long = "é".repeat(TITLE_MAX + 50);
        let t = clean_title(&long);
        assert_eq!(t.chars().count(), TITLE_MAX);
        assert_eq!(t.len(), 2 * TITLE_MAX);
        assert_eq!(clean_title("short"), "short");
    }

    #[test]
    fn backlog_round_trip() {
        let text = fixture("backlog-batch.backlog");
        let b = Backlog::parse(&text);
        assert_eq!(b, reading().backlog);
        assert_eq!(b.render(), text);
        assert_eq!(b.queue.len(), 6);
        assert_eq!(b.read, "20260105T120000Z");
        let junk = "garbage\n# open alpha\n# label alpha 3\nalpha\tx\tcodex\tt\tx\nalpha\t1\tother\tt\tx\n";
        let j = Backlog::parse(junk);
        assert!(
            j.open.is_empty() && j.labels.is_empty() && j.queue.is_empty(),
            "{j:?}"
        );
        // A pull request table is not a backlog.
        assert!(Backlog::parse(&fixture("coord-batch.prs")).queue.is_empty());
    }

    #[test]
    fn metrics_samples() {
        let r = reading();
        let got: Vec<(String, u64, u64, u64)> = r
            .samples
            .iter()
            .map(|m| (m.key(), m.open, m.opened, m.closed))
            .collect();
        assert_eq!(
            got,
            [
                ("alpha.20260105".to_string(), 9, 2, 1),
                ("beta.20260105".to_string(), 5, 0, 3),
                ("gamma.20260105".to_string(), 3, 1, 0),
            ]
        );
        let alpha = &r.samples[0];
        assert_eq!(alpha.taken_at, "20260105T120000Z");
        assert_eq!(alpha.labels.get("codex"), Some(&4));
        assert_eq!(alpha.labels.get("good first issue"), Some(&2));
        assert!(!alpha.labels.contains_key("needs-owner"), "zero omitted");
        assert_eq!(
            r.dry_run().lines().last(),
            Some("would write HGL_METRICS/gamma.20260105: open 3 opened 1 closed 0")
        );
        assert!(r.dry_run().starts_with(&fixture("backlog-batch.backlog")));
    }

    #[test]
    fn a_repository_without_an_answer() {
        let mut v = json("backlog-batch.json");
        v["data"]["r1"] = Value::Null;
        v["data"].as_object_mut().unwrap().remove("closed2");
        let r = parse_batch(&v, &repos(), now()).unwrap();
        assert_eq!(r.missing, ["beta", "gamma"]);
        assert_eq!(r.samples.len(), 1);
        assert!(r.backlog.queue.iter().all(|q| q.repo == "alpha"));
        assert_eq!(r.backlog.open.keys().collect::<Vec<_>>(), ["alpha"]);
        assert!(parse_batch(&serde_json::json!({}), &repos(), now()).is_err());
        // A pull request batch answers none of it.
        assert!(parse_batch(&json("coord-batch.json"), &repos(), now()).is_err());
    }

    #[test]
    fn every_fifteen_minutes() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert!(due(None, t0, BACKLOG_EVERY), "the first round");
        assert!(!due(Some(t0), t0, BACKLOG_EVERY));
        assert!(!due(Some(t0), at(5 * 60), BACKLOG_EVERY));
        assert!(!due(Some(t0), at(15 * 60 - 1), BACKLOG_EVERY));
        assert!(due(Some(t0), at(15 * 60), BACKLOG_EVERY));
        assert!(due(Some(t0), at(16 * 60), BACKLOG_EVERY));
        assert!(!due(Some(at(60)), t0, BACKLOG_EVERY), "a clock behind");
        // Rounds every five minutes, each taking a few seconds: every third runs it.
        let mut last = None;
        let mut ran = Vec::new();
        for k in 0..7u64 {
            let t = at(k * (5 * 60 + 3));
            if due(last, t, BACKLOG_EVERY) {
                last = Some(t);
                ran.push(k);
            }
        }
        assert_eq!(ran, [0, 3, 6]);
    }
}
