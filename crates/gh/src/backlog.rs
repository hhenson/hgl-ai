//! `HGL_STATE/backlog` and the daily `HGL_METRICS` samples (docs/owner.md §3): one batched
//! GraphQL read of the open issues of every configured repository, every 15 minutes, rendered as
//!
//! ```text
//! # read <UTC>
//! # open <repo> <count>
//! # label <repo> <label> <count>
//! # priority <repo> <repo> ...
//! REPO<TAB>NUMBER<TAB>codex-priority|codex<TAB>CREATED<TAB>TITLE
//! ```
//!
//! One `open` line per repository; one `label` line per repository and label with open
//! issues, sorted (a label may contain spaces: the repository is the first word, the count the
//! last). The rows are the claimable queue in order (docs/work.md WORK-1, WORK-3): every issue
//! labelled `codex-priority`, then those labelled `codex`; within each, the repositories of
//! `github.queue_priority` first, in its order, then the rest; then oldest first (ties in the
//! configured order of repositories, then by number). The `priority` line, present when that
//! list is not empty, names it. An issue also
//! labelled `codex-claimed`, `codex-blocked`, `needs-owner` or `codex-ready` is left out.
//! Times are UTC in the compact form of a note's `created_at`. A title has tabs, CR and LF as
//! spaces and at most 200 characters.
//!
//! The same read gives each repository's sample of the UTC day, `HGL_METRICS/<repo>.<YYYYMMDD>`:
//! open issues, issues opened and closed that day, open issues per label. Every round rewrites
//! it, so the day's last round is the day's sample.

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use hgl_ai_bus::Bus;
use hgl_ai_proto::{KV_METRICS, MetricsSample, utc_stamp};
use regex::Regex;
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
    /// `github.queue_priority`: the repositories whose issues come first, in order.
    pub priority: Vec<String>,
}

/// Tabs, CR and LF as spaces.
fn one_line(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ")
}

/// A title as a row keeps it: one line, at most [`TITLE_MAX`] characters.
pub fn clean_title(t: &str) -> String {
    one_line(t).chars().take(TITLE_MAX).collect()
}

/// A key word after which anything may be a secret: the title is cut there. Any case, any
/// separator, inside a longer word too (`GH_TOKEN`, `apiKey`, `X-Auth`); the longest word
/// first, so the one kept is whole (`Authorization`, not `Auth`).
static SENSITIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)authorization|credentials?|password|passwd|session|cookie|secret|bearer|apikey|token|basic|auth|seed|pwd|key",
    )
    .unwrap()
});
/// Token shapes that may stand alone, with no key word before them: GitHub, GitLab, npm,
/// OpenAI-style, Slack and AWS keys, JWTs, NATS seeds.
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|glpat-[A-Za-z0-9_-]{20,}|npm_[A-Za-z0-9]{30,}|sk-[A-Za-z0-9_-]{20,}|xox[abprs]-[A-Za-z0-9-]{10,}|A[KS]IA[0-9A-Z]{16}|eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}|S[UOANCAX][A-Z2-7]{50,})\b",
    )
    .unwrap()
});
/// A word long enough to be a key, whatever its shape.
static LONG_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9_+=.-]{32,}").unwrap());
static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}").unwrap()
});

/// Whether a word may be part of a path: it holds `/`, `\`, `~` or `file:`.
fn pathlike(w: &str) -> bool {
    w.contains(['/', '\\', '~']) || w.to_ascii_lowercase().contains("file:")
}

/// Every word that may be part of a path as `[path]`, and with it the rest of a quote the
/// word opens (`"C:\Program Files\x"`, `{"p":"/a b/c"}`), through the word that closes it
/// or the end of the title. Fails closed: a URL or `docs/work.md` goes too.
fn redact_paths(t: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    // The quote a path word left open, if any.
    let mut open: Option<char> = None;
    for w in t.split(' ') {
        if let Some(q) = open {
            if w.contains(q) {
                open = None;
            }
            continue;
        }
        if !pathlike(w) {
            out.push(w);
            continue;
        }
        out.push("[path]");
        open = ['"', '\'', '`']
            .into_iter()
            .find(|q| w.matches(*q).count() % 2 == 1);
    }
    out.join(" ")
}

/// A title the console's page may show (anything a viewer-readable key holds), failing
/// closed: emails out; token shapes and any word long enough to be a key out; everything
/// after a sensitive key word out (the word kept, the rest `[redacted]`); every word that
/// may be part of a path out, with the rest of a quote it opens. All of it over the
/// whole title, and only then cut as [`clean_title`] cuts it, so nothing crosses the cut
/// half-redacted. Over-redaction is the price: `docs/work.md`, a URL, `Keyboard` go too.
pub fn page_title(t: &str) -> String {
    let t = one_line(t);
    let t = EMAIL.replace_all(&t, "[email]");
    // Whole tokens first, so a key word inside one cannot leave half of it before the cut;
    // their mark holds no key word.
    let t = TOKEN.replace_all(&t, "[redacted]");
    let t = LONG_WORD.replace_all(&t, "[redacted]");
    let t = match SENSITIVE.find(&t) {
        Some(m) => format!("{} [redacted]", &t[..m.end()]),
        None => t.into_owned(),
    };
    clean_title(&redact_paths(&t))
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
        if !self.priority.is_empty() {
            s.push_str(&format!("# priority {}\n", self.priority.join(" ")));
        }
        for r in &self.queue {
            s.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                r.repo,
                r.number,
                r.class(),
                r.created,
                page_title(&r.title)
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
            } else if let Some(rest) = line.strip_prefix("# priority ") {
                b.priority = rest.split_whitespace().map(String::from).collect();
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
    /// Queue classes with more than one page: repository, label, the cursor after page one.
    pub more: Vec<(String, &'static str, String)>,
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
  priority: issues(states: OPEN, labels: [\"{PRIORITY}\"], first: 100, orderBy: {{field: CREATED_AT, direction: ASC}}) {{ ...Q }}
  claimable: issues(states: OPEN, labels: [\"{CLAIMABLE}\"], first: 100, orderBy: {{field: CREATED_AT, direction: ASC}}) {{ ...Q }}
}}
fragment Q on IssueConnection {{
  pageInfo {{ hasNextPage endCursor }}
  nodes {{ number title createdAt labels(first: 30) {{ nodes {{ name }} }} }}
}}"
    )
}

/// The later pages of one repository's issues with `label` (`gh api --paginate`).
pub fn page_query() -> String {
    "query Page($owner: String!, $name: String!, $label: String!, $endCursor: String) {
  repository(owner: $owner, name: $name) {
    issues(states: OPEN, labels: [$label], first: 100, after: $endCursor, orderBy: {field: CREATED_AT, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes { number title createdAt labels(first: 30) { nodes { name } } }
    }
  }
}"
    .to_string()
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
    // The whole title: `render` redacts it, then cuts it (`page_title`).
    Some((number, priority, created, one_line(title)))
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
    let mut more = Vec::new();
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
        for (alias, label) in [("priority", PRIORITY), ("claimable", CLAIMABLE)] {
            queue.extend(
                nodes(r, &[alias]).iter().filter_map(candidate).map(
                    |(number, priority, created, title)| (i, number, priority, created, title),
                ),
            );
            let page = r.get(alias).and_then(|c| c.get("pageInfo"));
            if page
                .and_then(|p| p.get("hasNextPage"))
                .and_then(Value::as_bool)
                == Some(true)
                && let Some(cur) = page
                    .and_then(|p| p.get("endCursor"))
                    .and_then(Value::as_str)
            {
                more.push((repo.clone(), label, cur.to_string()));
            }
        }
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
    b.queue = order(queue, repos);
    Ok(Reading {
        backlog: b,
        samples,
        rate,
        missing,
        more,
    })
}

type Candidate = (usize, u64, bool, DateTime<Utc>, String);

/// The claimable queue: `codex-priority` first, then `codex`, each oldest first across
/// the repositories (ties in configured order, then number); an issue in both classes
/// once, as priority.
fn order(mut queue: Vec<Candidate>, repos: &[String]) -> Vec<QueueRow> {
    queue.sort_by_key(|(i, number, priority, created, _)| (!*priority, *created, *i, *number));
    let mut seen = std::collections::BTreeSet::new();
    queue
        .into_iter()
        .filter(|(i, number, ..)| seen.insert((*i, *number)))
        .map(|(i, number, priority, created, title)| QueueRow {
            repo: repos[i].clone(),
            number,
            priority,
            created: utc_stamp(created),
            title,
        })
        .collect()
}

/// Add the later pages `read` fetched to the queue.
pub fn add_pages(r: &mut Reading, repos: &[String], pages: &[(String, Vec<Value>)]) {
    let index = |repo: &str| repos.iter().position(|x| x == repo);
    let mut queue: Vec<Candidate> = r
        .backlog
        .queue
        .iter()
        .filter_map(|q| {
            Some((
                index(&q.repo)?,
                q.number,
                q.priority,
                table::parse_time(&q.created).or_else(|| hgl_ai_proto::parse_stamp(&q.created))?,
                q.title.clone(),
            ))
        })
        .collect();
    for (repo, pp) in pages {
        let Some(i) = index(repo) else { continue };
        for p in pp {
            let conn = p.pointer("/data/repository/issues");
            for n in conn
                .and_then(|c| c.get("nodes"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some((number, priority, created, title)) = candidate(n) {
                    queue.push((i, number, priority, created, title));
                }
            }
        }
    }
    r.backlog.queue = order(queue, repos);
}

/// Read the backlog of every configured repository from GitHub, as at `now`.
pub async fn read<G: Gh>(gh: &G, s: &Settings, now: DateTime<Utc>) -> Result<Reading> {
    let q = batch_query(s.repos.len());
    let vars = variables(s, now);
    let vars: Vec<(&str, String)> = vars.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    let pages = exec::graphql(gh, &q, &vars, false).await?;
    let mut r = parse_batch(&pages[0], &s.repos, now)?;
    // A class with more than a page: the rest, page by page, from after the first.
    let mut later = Vec::new();
    for (repo, label, cursor) in r.more.clone() {
        let vars = [
            ("owner", s.owner.clone()),
            ("name", repo.clone()),
            ("label", label.to_string()),
            ("endCursor", cursor),
        ];
        later.push((repo, exec::graphql(gh, &page_query(), &vars, true).await?));
    }
    add_pages(&mut r, &s.repos, &later);
    prioritise(&mut r.backlog, &s.queue_priority);
    Ok(r)
}

/// Within each class, the issues of the repositories of `first` first, in its order; the
/// order is otherwise kept.
pub fn prioritise(b: &mut Backlog, first: &[String]) {
    let rank = |repo: &str| first.iter().position(|f| f == repo).unwrap_or(first.len());
    b.queue.sort_by_key(|q| (!q.priority, rank(&q.repo)));
    b.priority = first.to_vec();
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
        // Each class has its own connection: newer priority issues are never crowded
        // out by a hundred older `codex` ones.
        assert!(q.contains(
            "priority: issues(states: OPEN, labels: [\"codex-priority\"], first: 100, orderBy: {field: CREATED_AT, direction: ASC}) { ...Q }"
        ));
        assert!(q.contains(
            "claimable: issues(states: OPEN, labels: [\"codex\"], first: 100, orderBy: {field: CREATED_AT, direction: ASC}) { ...Q }"
        ));
        assert!(q.contains("pageInfo { hasNextPage endCursor }"));
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

    /// A class with a second page: the rest comes by `gh api --paginate` from after the
    /// first page's cursor, and joins the queue in order.
    #[tokio::test]
    async fn a_long_queue_is_read_to_the_end() {
        let mut first = json("backlog-batch.json");
        let alpha = first.pointer_mut("/data/r0/claimable/pageInfo").unwrap();
        *alpha = serde_json::json!({ "hasNextPage": true, "endCursor": "Y3Vyc29y" });
        let page2 = serde_json::json!({ "data": { "repository": { "issues": {
            "pageInfo": { "hasNextPage": false, "endCursor": null },
            "nodes": [{ "number": 999, "title": "Late", "createdAt": "2026-03-01T00:00:00Z",
                        "labels": { "nodes": [{ "name": "codex" }] } }]
        }}}});
        let mut s = settings();
        s.repos = ["alpha", "beta", "gamma"].map(String::from).to_vec();
        let gh = crate::exec::fake::FakeGh::default()
            .on("--paginate", &page2.to_string())
            .on("graphql", &first.to_string());
        let r = read(&gh, &s, chrono::Utc::now()).await.unwrap();
        assert_eq!(
            r.more,
            [("alpha".to_string(), CLAIMABLE, "Y3Vyc29y".to_string())]
        );
        let last = r.backlog.queue.last().unwrap();
        assert_eq!((last.repo.as_str(), last.number), ("alpha", 999));
        let calls = gh.calls();
        let page = calls.iter().find(|c| c.contains("--paginate")).unwrap();
        assert!(
            page.contains("-f endCursor=Y3Vyc29y") && page.contains("-f label=codex"),
            "{page}"
        );
        let n = r.backlog.queue.len();
        let mut seen = std::collections::BTreeSet::new();
        assert!(
            r.backlog
                .queue
                .iter()
                .all(|q| seen.insert((q.repo.clone(), q.number))),
            "no duplicates"
        );
        assert!(n > 1);
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

    /// `github.queue_priority` puts its repositories first within each class, and the
    /// backlog names it.
    #[test]
    fn the_queue_by_repository_priority() {
        let mut b = reading().backlog;
        prioritise(&mut b, &["gamma".into(), "beta".into()]);
        let q: Vec<(&str, u64)> = b
            .queue
            .iter()
            .map(|x| (x.repo.as_str(), x.number))
            .collect();
        assert_eq!(
            q,
            [
                ("beta", 8),
                ("alpha", 105),
                ("gamma", 21),
                ("beta", 3),
                ("alpha", 101),
                ("alpha", 107),
            ]
        );
        let text = b.render();
        assert!(text.contains("\n# priority gamma beta\n"), "{text}");
        assert_eq!(Backlog::parse(&text), b);
        prioritise(&mut b, &[]);
        assert!(!b.render().contains("# priority"));
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

    /// A title through every writer of a viewer-readable title: the backlog's queue (from a
    /// GraphQL node, as a round reads it), `prs`' `# pr` lines, `tasks`' `# title` lines and
    /// `landed.log`.
    fn every_writer(title: &str) -> [(&'static str, String); 4] {
        let node = serde_json::json!({
            "number": 1, "title": title, "createdAt": "2026-01-01T00:00:00Z",
            "labels": {"nodes": [{"name": CLAIMABLE}]}
        });
        let (number, priority, created, t) = candidate(&node).unwrap();
        let b = Backlog {
            queue: vec![QueueRow {
                repo: "alpha".into(),
                number,
                priority,
                created: utc_stamp(created),
                title: t,
            }],
            ..Backlog::default()
        };
        let mut pr = table::Table::parse("alpha\t1\th\topen\tnone\t0\tnone\tT\n");
        pr.rows[0].detail.title = title.to_string();
        let mut titles = crate::tasks::Titles::default();
        titles.insert("alpha", 1, title);
        let tree: hgl_ai_proto::TreeRecord = serde_json::from_value(serde_json::json!({
            "host": "h", "seat": "w1", "task": {"repo": "alpha", "issue": 1},
            "path": "/fixture/tree", "created_at": "20260101T000000Z",
        }))
        .unwrap();
        let tasks = crate::tasks::render(&crate::tasks::NotesIndex::default(), &[tree], &titles);
        [
            ("backlog", b.render()),
            ("prs", pr.render()),
            ("tasks", tasks),
            (
                "landed.log",
                crate::state::landed_line("S", "alpha", 1, "m", title),
            ),
        ]
    }

    /// r5's probe on #181 (sixteen shapes, each with its marker) and the leaks of the earlier
    /// rounds as regressions, alone and behind five prefix lengths that put them across the
    /// 200-character cut, through all four writers: no marker comes out, whole or cut.
    #[test]
    fn every_writer_redacts_then_cuts() {
        let seed = format!("SU{}", "Q".repeat(54));
        let ghp = format!("ghp_{}", "Q".repeat(36));
        let shapes: Vec<(&str, String, &str)> = vec![
            // r5's sixteen.
            ("YAML known", "GH_TOKEN: CANARY".into(), "CANARY"),
            (
                "quoted Windows",
                r#"run "C:\Program Files\CANARY\bin""#.into(),
                "CANARY",
            ),
            ("drive slash", "read C:/Users/CANARY/log".into(), "CANARY"),
            ("UNC", r"read \\CANARY\share".into(), "CANARY"),
            (
                "email crossing the cut",
                "CANARY@example.invalid".into(),
                "CAN",
            ),
            (
                "quoted POSIX",
                r#"read "/Users/CANARY/My File""#.into(),
                "CANARY",
            ),
            (
                "JSON quoted",
                r#"{"apiKey":"CANARY words"}"#.into(),
                "CANARY",
            ),
            (
                "YAML plain scalar",
                "password: yellow CANARY".into(),
                "CANARY",
            ),
            (
                "JSON escaped quote",
                r#"{"password":"yellow\"CANARY"}"#.into(),
                "CANARY",
            ),
            (
                "shell escaped space",
                r"export PASSWORD=yellow\ CANARY".into(),
                "CANARY",
            ),
            (
                "quoted root with a space",
                r#"read "/Shared Items/CANARY/report""#.into(),
                "CANARY",
            ),
            (
                "file URI",
                "read file:///Users/CANARY/report".into(),
                "CANARY",
            ),
            (
                "Basic credential",
                "Authorization: Basic Q0FOQVJZ".into(),
                "Q0FO",
            ),
            ("NATS seed", seed.clone(), "QQQQ"),
            (
                "AWS temporary key id",
                "ASIACANARYAAAAAAAAAA".into(),
                "CANARY",
            ),
            (
                "token after a flag",
                "retry --token CANARY".into(),
                "CANARY",
            ),
            // Earlier rounds'.
            ("shell export", "export GH_TOKEN=CANARY".into(), "CANARY"),
            ("YAML quoted", "password: \"CANARY words\"".into(), "CANARY"),
            ("dotted name", "db.password='CANARY'".into(), "CANARY"),
            (
                "AWS secret",
                "AWS_SECRET_ACCESS_KEY = CANARY".into(),
                "CANARY",
            ),
            ("client secret", "client secret: CANARY".into(), "CANARY"),
            (
                "seed and auth",
                "seed=CANARY and auth:CANARY".into(),
                "CANARY",
            ),
            (
                "bearer",
                "Authorization: Bearer CANARYCANARY".into(),
                "CANARY",
            ),
            ("cookie", "Cookie: sid=CANARY".into(), "CANARY"),
            ("session", "session id CANARY".into(), "CANARY"),
            ("GitHub token", format!("leaked {ghp}"), "QQQQ"),
            (
                "JWT",
                "jwt eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0.SflKxwRJCANARY".into(),
                "CANARY",
            ),
            (
                "long key",
                format!("value {}", "CANARY9".repeat(6)),
                "CANARY",
            ),
            ("email", "mail alice.b+x@mail.example.co.uk".into(), "alice"),
            (
                "quoted drive slash",
                "'C:/Users/CANARY/My Docs/x'".into(),
                "CANARY",
            ),
            ("drive backslash", r"Drive D:\work\CANARY".into(), "CANARY"),
            (
                "unquoted space",
                r"Unquoted C:\Program Files\CANARY".into(),
                "CANARY",
            ),
            ("POSIX", "Logs /home/CANARY/x".into(), "CANARY"),
            ("home", "Key ~/.ssh/id_CANARY".into(), "CANARY"),
            ("parenthesised", "(path:/var/lib/CANARY)".into(), "CANARY"),
            ("flag path", "--config=/etc/CANARY/x".into(), "CANARY"),
            (
                "escaped space path",
                r"open /home/a\ CANARY/x".into(),
                "CANARY",
            ),
            (
                "path in JSON with a space",
                r#"{"p":"/srv/a b CANARY/x"}"#.into(),
                "CANARY",
            ),
        ];
        let mut problems = Vec::new();
        for (label, shape, marker) in &shapes {
            for pad in [0, 160, 180, 190, 196] {
                let title = if pad == 0 {
                    shape.clone()
                } else {
                    format!("{} {shape}", "é".repeat(pad))
                };
                for (writer, out) in every_writer(&title) {
                    if out.contains(marker) {
                        problems.push(format!("{label} at {pad} through {writer}: {out}"));
                    }
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    /// The rule, exactly: what goes and what stays.
    #[test]
    fn page_titles_redact() {
        let cases = [
            ("Mail owner@example.com about it", "Mail [email] about it"),
            (
                "Logs in /home/alice/hgl-ai/work fill up",
                "Logs in [path] fill up",
            ),
            ("CI: GH_TOKEN: CANARY", "CI: GH_TOKEN [redacted]"),
            ("Authorization: Basic Q0FO", "Authorization [redacted]"),
            ("Fix the parser for tabs", "Fix the parser for tabs"),
            (r#"Run "C:\Program Files\x y" now"#, "Run [path] now"),
            (r#"Read "/a b/c" and "x y""#, r#"Read [path] and "x y""#),
            ("read file:///Users/x/report today", "read [path] today"),
            // Failing closed: over-redaction, as the rule takes it.
            ("Hub accounts 1/6", "Hub accounts [path]"),
            ("See docs/work.md", "See [path]"),
            ("Keyboard order", "Key [redacted]"),
            (
                &format!("Token ghp_{} seen", "a".repeat(36)),
                "Token [redacted]",
            ),
            (&format!("Seen {}", "Q".repeat(40)), "Seen [redacted]"),
        ];
        for (t, want) in cases {
            assert_eq!(page_title(t), want, "{t}");
        }
        assert_eq!(page_title("a\tb"), "a b");
        // Redacted, then cut: the cut falls on the redacted text.
        let long = format!("{} alice@example.com", "x".repeat(195));
        assert!(!page_title(&long).contains("alice"));
        assert_eq!(page_title(&"é".repeat(300)).chars().count(), TITLE_MAX);
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
