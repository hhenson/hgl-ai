//! The telemetry page (docs/owner.md §3): one snapshot of the hub, read every few
//! seconds, turned into tables of rows (see [`crate::grid`]) that the page shows in tabs
//! and the live feed sends as row diffs.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Duration, Utc};
use hgl_ai_bus::Bus;
use hgl_ai_gh::backlog::{Backlog, QueueRow};
use hgl_ai_gh::table::Table;
use hgl_ai_owner::{Priority, Thread, ThreadKind};
use hgl_ai_proto::build::{BuilderRecord, Summary};
use hgl_ai_proto::usage::UsageRecord;
use hgl_ai_proto::{HostRecord, Kind, MetricsSample, Note, STREAM_NOTES, TreeRecord};
use serde::Serialize;
use serde_json::{Value, json};

use crate::grid::{self, Meta, Tables, row};
use crate::html::when;

/// Notes read for the tasks panel: the bus keeps 14 days.
const NOTES_WINDOW_DAYS: i64 = 14;
const NOTES_MAX: usize = 20_000;
/// Days of the burndown.
pub const BURNDOWN_DAYS: usize = 14;
/// `prs` older than this is stale (docs/protocol.md §8).
const PRS_STALE_MIN: i64 = 15;
/// Build runs shown.
const RUNS_MAX: usize = 40;
/// Landings shown.
const LANDINGS_MAX: usize = 50;

/// The last note about one task.
#[derive(Debug, Clone, Serialize)]
pub struct LastNote {
    pub kind: Kind,
    pub sender: String,
    pub recipient: String,
    pub at: String,
}

/// The notes stream, folded to the last note per task, read incrementally.
#[derive(Default)]
pub struct NotesIndex {
    last_seq: u64,
    last_time: Option<DateTime<Utc>>,
    by_task: HashMap<(String, u64), LastNote>,
}

impl NotesIndex {
    pub fn add(&mut self, seq: u64, n: &Note) {
        if seq <= self.last_seq {
            return;
        }
        self.last_seq = seq;
        if let Some(t) = n.task() {
            self.by_task.insert(
                (t.repo, t.issue),
                LastNote {
                    kind: n.kind,
                    sender: n.sender.to_string(),
                    recipient: n.recipient.to_string(),
                    at: n.created_at.clone(),
                },
            );
        }
    }

    pub async fn refresh(&mut self, bus: &Bus) -> hgl_ai_bus::Result<()> {
        let since = self
            .last_time
            .unwrap_or_else(|| Utc::now() - Duration::days(NOTES_WINDOW_DAYS));
        let msgs = bus
            .history(STREAM_NOTES, "hgl.notes.>", since, NOTES_MAX)
            .await?;
        for m in msgs {
            self.last_time = Some(m.time);
            if let Ok(n) = serde_json::from_slice::<Note>(&m.payload) {
                self.add(m.seq, &n);
            }
        }
        Ok(())
    }

    pub fn get(&self, repo: &str, issue: u64) -> Option<&LastNote> {
        self.by_task.get(&(repo.to_string(), issue))
    }
}

/// A task's stage from its last note.
pub fn stage(last: Option<&LastNote>, review: bool) -> &'static str {
    match last.map(|l| l.kind) {
        _ if review => "review",
        Some(Kind::Assign) => "assigned",
        Some(Kind::Ready) => "ready",
        Some(Kind::Review | Kind::Receipt) => "review",
        Some(Kind::Sendback) => "sendback",
        Some(Kind::Approved) => "approved",
        Some(Kind::Blocked) => "blocked",
        _ => "working",
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub host: String,
    pub seat: String,
    pub task: String,
    pub review: bool,
    pub stage: &'static str,
    pub last: Option<LastNote>,
    pub created_at: String,
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Day {
    pub date: String,
    pub open: u64,
    pub opened: u64,
    pub closed: u64,
    pub net: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub read_at: String,
    pub error: Option<String>,
    pub hosts: Vec<HostRecord>,
    pub tasks: Vec<Task>,
    pub prs_read: String,
    pub prs_stale: bool,
    pub prs: Vec<PrRow>,
    pub backlog_read: String,
    pub queue: Vec<QueueRowOut>,
    pub labels: BTreeMap<String, BTreeMap<String, u64>>,
    pub burndown: BTreeMap<String, Vec<Day>>,
    pub landings: Vec<String>,
    pub mode: String,
    pub hold: Option<String>,
    pub stale_trees: usize,
    pub open_questions: BTreeMap<String, usize>,
    /// The newest build runs, newest first (`HGL_BUILD/run.*`).
    pub runs: Vec<RunRow>,
    pub builders: Vec<BuilderRecord>,
    pub usage: Vec<UsageRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrRow {
    pub repo: String,
    pub number: u64,
    pub head: String,
    pub draft: bool,
    pub bot: String,
    pub unresolved: u64,
    pub checks: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueRowOut {
    pub repo: String,
    pub number: u64,
    pub class: String,
    pub created: String,
    pub title: String,
}

impl From<&QueueRow> for QueueRowOut {
    fn from(q: &QueueRow) -> Self {
        QueueRowOut {
            repo: q.repo.clone(),
            number: q.number,
            class: q.class().to_string(),
            created: q.created.clone(),
            title: q.title.clone(),
        }
    }
}

/// One build run, as the builds tab shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunRow {
    pub id: String,
    pub repo: String,
    pub head: String,
    pub host: String,
    pub platform: String,
    pub status: String,
    pub started_at: String,
    pub wall_ms: u64,
    pub errors: u64,
    pub passed: u64,
    pub failed: u64,
}

impl From<&Summary> for RunRow {
    fn from(s: &Summary) -> Self {
        RunRow {
            id: s.id.clone(),
            repo: s.repo.clone(),
            head: s
                .head
                .as_deref()
                .map(|h| h.chars().take(8).collect())
                .unwrap_or_default(),
            host: s.host.clone(),
            platform: s.platform.to_string(),
            status: s.status.to_string(),
            started_at: s.started_at.clone(),
            wall_ms: s.wall_ms,
            errors: s.errors_total,
            passed: s.tests.passed,
            failed: s.tests.failed,
        }
    }
}

fn parse_stamp(s: &str) -> Option<DateTime<Utc>> {
    hgl_ai_proto::parse_stamp(s)
}

/// Per repository, the last [`BURNDOWN_DAYS`] samples, oldest first.
pub fn burndown(samples: &[MetricsSample]) -> BTreeMap<String, Vec<Day>> {
    let mut by: BTreeMap<String, Vec<&MetricsSample>> = BTreeMap::new();
    for s in samples {
        by.entry(s.repo.clone()).or_default().push(s);
    }
    by.into_iter()
        .map(|(repo, mut v)| {
            v.sort_by(|a, b| a.date.cmp(&b.date));
            let skip = v.len().saturating_sub(BURNDOWN_DAYS);
            let days = v[skip..]
                .iter()
                .map(|s| Day {
                    date: s.date.clone(),
                    open: s.open,
                    opened: s.opened,
                    closed: s.closed,
                    net: s.opened as i64 - s.closed as i64,
                })
                .collect();
            (repo, days)
        })
        .collect()
}

/// What one reading keeps for the next: the notes read so far and the finished runs.
#[derive(Default)]
pub struct Cache {
    pub notes: NotesIndex,
    runs: HashMap<String, RunRow>,
}

/// The newest [`RUNS_MAX`] runs, newest first; a finished run is read once.
async fn read_runs(bus: &Bus, cache: &mut Cache) -> hgl_ai_bus::Result<Vec<RunRow>> {
    let keys = bus.build_keys("run.").await?;
    let newest: Vec<&String> = keys.iter().rev().take(RUNS_MAX).collect();
    let mut out = Vec::new();
    for k in &newest {
        if let Some(r) = cache.runs.get(*k) {
            out.push(r.clone());
            continue;
        }
        let Some(e) = bus.build_get(k).await? else {
            continue;
        };
        let Ok(s) = serde_json::from_slice::<Summary>(&e.value) else {
            continue;
        };
        let r = RunRow::from(&s);
        if s.status.is_final() {
            cache.runs.insert((*k).clone(), r.clone());
        }
        out.push(r);
    }
    cache.runs.retain(|k, _| newest.contains(&k));
    Ok(out)
}

/// Read everything the page shows.
pub async fn read(
    bus: &Bus,
    cache: &mut Cache,
    stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> Snapshot {
    let mut s = Snapshot {
        read_at: hgl_ai_proto::utc_stamp(now),
        ..Snapshot::default()
    };
    let mut errors = Vec::new();
    macro_rules! get {
        ($what:literal, $e:expr) => {
            match $e.await {
                Ok(v) => Some(v),
                Err(e) => {
                    errors.push(format!("{}: {e}", $what));
                    None
                }
            }
        };
    }
    if let Err(e) = cache.notes.refresh(bus).await {
        errors.push(format!("notes: {e}"));
    }
    s.hosts = get!("hosts", bus.hosts()).unwrap_or_default();
    let trees: Vec<TreeRecord> = get!("trees", bus.trees()).unwrap_or_default();
    let stale = Duration::from_std(stale_after).unwrap_or(Duration::days(3));
    for t in &trees {
        let last = cache.notes.get(&t.task.repo, t.task.issue).cloned();
        let old = parse_stamp(&t.created_at).is_some_and(|c| now - c > stale);
        s.stale_trees += usize::from(old);
        s.tasks.push(Task {
            host: t.host.clone(),
            seat: t.seat.to_string(),
            task: format!("{}#{}", t.task.repo, t.task.issue),
            review: t.review,
            stage: stage(last.as_ref(), t.review),
            last,
            created_at: t.created_at.clone(),
            stale: old,
        });
    }
    let state = |k: &'static str| async move { bus.state_get(k).await.map(|e| e.map(|e| e.value)) };
    if let Some(Some(v)) = get!("prs", state("prs")) {
        let t = Table::parse(&v);
        s.prs_stale =
            parse_stamp(&t.read).is_none_or(|r| now - r > Duration::minutes(PRS_STALE_MIN));
        s.prs_read = t.read.clone();
        s.prs = t
            .rows
            .iter()
            .map(|r| PrRow {
                repo: r.repo.clone(),
                number: r.number,
                head: r.head.chars().take(8).collect(),
                draft: r.draft,
                bot: r.bot.as_str().to_string(),
                unresolved: r.unresolved,
                checks: r.checks.as_str().to_string(),
            })
            .collect();
    }
    if let Some(Some(v)) = get!("backlog", state("backlog")) {
        let b = Backlog::parse(&v);
        s.backlog_read = b.read.clone();
        s.queue = b.queue.iter().map(QueueRowOut::from).collect();
        for ((repo, label), n) in &b.labels {
            s.labels
                .entry(repo.clone())
                .or_default()
                .insert(label.clone(), *n);
        }
    }
    if let Some(m) = get!("metrics", bus.metrics()) {
        s.burndown = burndown(&m);
    }
    if let Some(Some(v)) = get!("landed.log", state("landed.log")) {
        let lines: Vec<String> = v
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(String::from)
            .collect();
        let skip = lines.len().saturating_sub(LANDINGS_MAX);
        s.landings = lines[skip..].iter().rev().cloned().collect();
    }
    s.mode = get!("mode", state("mode"))
        .flatten()
        .map(|m| m.trim().to_string())
        .unwrap_or_else(|| "normal".into());
    s.hold = get!("hold", state("hold"))
        .flatten()
        .map(|h| h.trim().to_string());
    if let Some(threads) = get!("threads", bus.threads::<Thread>()) {
        for p in Priority::ALL {
            let n = threads
                .iter()
                .filter(|t| t.kind == ThreadKind::Question && t.is_open() && t.priority == Some(*p))
                .count();
            s.open_questions.insert(p.to_string(), n);
        }
    }
    s.runs = get!("build runs", read_runs(bus, cache)).unwrap_or_default();
    s.builders = get!("builders", bus.builders()).unwrap_or_default();
    s.usage = get!("usage", bus.usage()).unwrap_or_default();
    if !errors.is_empty() {
        s.error = Some(errors.join("; "));
    }
    s
}

fn minutes(stamp: &str, now: DateTime<Utc>) -> Value {
    parse_stamp(stamp).map_or(Value::Null, |t| json!((now - t).num_minutes().max(0)))
}

fn stamp(s: &str) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        json!(when(s))
    }
}

/// `YYYYMMDD` → `YYYY-MM-DD`.
fn day(d: &str) -> String {
    match (d.get(0..4), d.get(4..6), d.get(6..8)) {
        (Some(y), Some(m), Some(dd)) if d.len() == 8 => format!("{y}-{m}-{dd}"),
        _ => d.to_string(),
    }
}

/// Every table's rows, keyed by a stable id (see [`crate::grid`]).
pub fn tables(s: &Snapshot) -> Tables {
    let now = parse_stamp(&s.read_at).unwrap_or_else(Utc::now);
    let mut t = Tables::new();

    let mut rows = Vec::new();
    for h in &s.hosts {
        let host = [
            ("host", json!(h.host)),
            ("memcap_gb", json!(h.memcap_gb)),
            ("free_gb", json!(h.work_root_free_gb)),
        ];
        if h.seats.is_empty() {
            rows.push(row(h.host.clone(), host.clone()));
        }
        for st in &h.seats {
            let mut r = row(format!("{}/{}", h.host, st.seat), host.clone());
            r.extend([
                ("seat".into(), json!(st.seat.as_str())),
                ("role".into(), json!(st.role.as_str())),
                ("runtime".into(), json!(st.runtime.as_str())),
                ("model".into(), json!(st.model)),
            ]);
            rows.push(r);
        }
    }
    t.insert(grid::HOSTS.id, rows);

    let rows = s
        .tasks
        .iter()
        .map(|k| {
            let review = if k.review { "review" } else { "implement" };
            row(
                format!("{}/{}/{}/{review}", k.host, k.seat, k.task),
                [
                    ("seat", json!(k.seat)),
                    ("task", json!(k.task)),
                    ("work", json!(review)),
                    ("stage", json!(k.stage)),
                    (
                        "last",
                        k.last.as_ref().map_or(Value::Null, |l| {
                            json!(format!("{} {}→{}", l.kind.as_str(), l.sender, l.recipient))
                        }),
                    ),
                    (
                        "last_at",
                        k.last.as_ref().map_or(Value::Null, |l| stamp(&l.at)),
                    ),
                    ("host", json!(k.host)),
                    ("age", minutes(&k.created_at, now)),
                    ("stale", if k.stale { json!("stale") } else { Value::Null }),
                ],
            )
        })
        .collect();
    t.insert(grid::TASKS.id, rows);

    let rows = s
        .prs
        .iter()
        .map(|p| {
            let pr = format!("{}#{}", p.repo, p.number);
            row(
                pr.clone(),
                [
                    ("pr", json!(pr)),
                    ("head", json!(p.head)),
                    ("state", json!(if p.draft { "draft" } else { "open" })),
                    ("bot", json!(p.bot)),
                    ("threads", json!(p.unresolved)),
                    ("checks", json!(p.checks)),
                ],
            )
        })
        .collect();
    t.insert(grid::PRS.id, rows);

    let rows = s
        .queue
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let issue = format!("{}#{}", q.repo, q.number);
            row(
                issue.clone(),
                [
                    ("pos", json!(i + 1)),
                    ("issue", json!(issue)),
                    ("class", json!(q.class)),
                    ("opened", stamp(&q.created)),
                    ("title", json!(q.title)),
                ],
            )
        })
        .collect();
    t.insert(grid::QUEUE.id, rows);

    let mut rows = Vec::new();
    for (repo, labels) in &s.labels {
        for (l, n) in labels {
            rows.push(row(
                format!("{repo}/{l}"),
                [
                    ("repo", json!(repo)),
                    ("label", json!(l)),
                    ("open", json!(n)),
                ],
            ));
        }
    }
    t.insert(grid::LABELS.id, rows);

    let mut rows = Vec::new();
    for (repo, days) in &s.burndown {
        for d in days.iter().rev() {
            rows.push(row(
                format!("{repo}/{}", d.date),
                [
                    ("repo", json!(repo)),
                    ("day", json!(day(&d.date))),
                    ("open", json!(d.open)),
                    ("opened", json!(d.opened)),
                    ("closed", json!(d.closed)),
                    ("net", json!(d.net)),
                ],
            ));
        }
    }
    t.insert(grid::BURNDOWN.id, rows);

    let mut seen: HashMap<&str, usize> = HashMap::new();
    let rows = s
        .landings
        .iter()
        .map(|l| {
            // The same line twice (a landing run again) is two rows.
            let n = seen.entry(l.as_str()).or_default();
            *n += 1;
            let id = if *n == 1 {
                l.clone()
            } else {
                format!("{l} ({n})")
            };
            let w: Vec<&str> = l.split_whitespace().collect();
            match w[..] {
                [at, repo, pr, sha, ..] => row(
                    id,
                    [
                        ("when", json!(when(at))),
                        ("pr", json!(format!("{repo} {pr}"))),
                        ("merge", json!(sha.chars().take(8).collect::<String>())),
                    ],
                ),
                _ => row(id, [("when", json!(l))]),
            }
        })
        .collect();
    t.insert(grid::LANDINGS.id, rows);

    let q = |p: &str| s.open_questions.get(p).copied().unwrap_or(0);
    let fleet = [
        ("mode", "Mode", s.mode.clone()),
        (
            "hold",
            "Hold",
            s.hold.clone().unwrap_or_else(|| "none".into()),
        ),
        ("stale", "Stale trees", s.stale_trees.to_string()),
        ("p1", "Open questions, p1", q("p1").to_string()),
        ("p2", "Open questions, p2", q("p2").to_string()),
        ("p3", "Open questions, p3", q("p3").to_string()),
    ];
    let rows = fleet
        .into_iter()
        .map(|(id, item, v)| row(id, [("item", json!(item)), ("value", json!(v))]))
        .collect();
    t.insert(grid::FLEET.id, rows);

    let rows = s
        .runs
        .iter()
        .map(|r| {
            row(
                r.id.clone(),
                [
                    ("run", json!(r.id)),
                    ("repo", json!(r.repo)),
                    ("head", json!(r.head)),
                    ("host", json!(r.host)),
                    ("platform", json!(r.platform)),
                    ("status", json!(r.status)),
                    ("started", stamp(&r.started_at)),
                    ("wall_s", json!(r.wall_ms / 1000)),
                    ("errors", json!(r.errors)),
                    ("passed", json!(r.passed)),
                    ("failed", json!(r.failed)),
                ],
            )
        })
        .collect();
    t.insert(grid::RUNS.id, rows);

    let rows = s
        .builders
        .iter()
        .map(|b| {
            let reachable = match (&b.reachable, &b.reason) {
                (true, _) => "yes".to_string(),
                (false, Some(r)) => format!("no: {r}"),
                (false, None) => "no".to_string(),
            };
            let tools: Vec<String> = b
                .toolchains
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect();
            row(
                b.label.clone(),
                [
                    ("builder", json!(b.label)),
                    ("platform", json!(b.platform.to_string())),
                    ("arch", json!(b.arch)),
                    (
                        "executor",
                        json!(match &b.proxy {
                            Some(p) => format!("{} via {p}", b.executor),
                            None => b.executor.clone(),
                        }),
                    ),
                    ("state", json!(b.state)),
                    ("reachable", json!(reachable)),
                    ("busy", json!(b.busy)),
                    ("slots", json!(b.slots)),
                    ("free_gb", json!(b.free_gb)),
                    ("toolchains", json!(tools.join(", "))),
                    ("updated", stamp(&b.updated_at)),
                ],
            )
        })
        .collect();
    t.insert(grid::BUILDERS.id, rows);

    let mut rows = Vec::new();
    for u in &s.usage {
        let login = u.label.clone().unwrap_or_else(|| u.account.clone());
        let base = [
            ("login", json!(login)),
            ("runtime", json!(u.runtime.as_str())),
            ("host", json!(u.host)),
            ("plan", json!(u.plan)),
            ("observed", stamp(&u.observed_at)),
            ("limit", json!(u.limit_reached)),
        ];
        if u.buckets.is_empty() {
            rows.push(row(u.key(), base.clone()));
        }
        for b in &u.buckets {
            let mut r = row(
                format!("{}/{}/{}", u.key(), b.scope, b.window),
                base.clone(),
            );
            r.extend([
                ("scope".into(), json!(b.scope)),
                ("window".into(), json!(b.window)),
                ("used_pct".into(), json!((b.used_pct * 10.0).round() / 10.0)),
                (
                    "resets".into(),
                    b.resets_at.as_deref().map_or(Value::Null, stamp),
                ),
            ]);
            rows.push(r);
        }
    }
    t.insert(grid::USAGE.id, rows);
    t
}

/// The page's text outside the tables.
pub fn meta(s: &Snapshot) -> Meta {
    let mut m = Meta {
        read: if s.read_at.is_empty() {
            "Not read yet.".into()
        } else {
            format!("Read {}", when(&s.read_at))
        },
        error: s.error.clone(),
        ..Meta::default()
    };
    let read = |r: &str| {
        if r.is_empty() {
            "Read never".to_string()
        } else {
            format!("Read {}", when(r))
        }
    };
    let mut prs = read(&s.prs_read);
    if s.prs_stale && !s.prs_read.is_empty() {
        prs.push_str(" · stale");
    }
    m.notes.insert("prs", prs);
    m.notes.insert("queue", read(&s.backlog_read));
    m.notes.insert("backlog", read(&s.backlog_read));
    m.counts.insert("tasks", s.tasks.len());
    m.counts.insert("prs", s.prs.len());
    m.counts.insert("queue", s.queue.len());
    m
}

/// The tabs and plain tables of the page.
pub fn render(s: &Snapshot) -> String {
    grid::html_tabs(&meta(s), &tables(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hgl_ai_proto::Seat;

    #[test]
    fn stages_and_burndown() {
        let mut idx = NotesIndex::default();
        let mut n = Note::new(
            Seat::new("claude").unwrap(),
            Seat::new("w1").unwrap(),
            Kind::Assign,
            "x",
        );
        n.repo = Some("alpha".into());
        n.issue = 7;
        idx.add(1, &n);
        assert_eq!(stage(idx.get("alpha", 7), false), "assigned");
        n.kind = Kind::Sendback;
        n.head = Some("1".repeat(40));
        idx.add(3, &n);
        n.kind = Kind::Ready;
        idx.add(2, &n);
        assert_eq!(
            stage(idx.get("alpha", 7), false),
            "sendback",
            "older notes ignored"
        );
        assert_eq!(stage(None, true), "review");
        assert_eq!(stage(None, false), "working");
        let s = |d: &str, opened, closed| MetricsSample {
            repo: "alpha".into(),
            date: d.into(),
            open: 10,
            opened,
            closed,
            labels: Default::default(),
            taken_at: "x".into(),
        };
        let b = burndown(&[s("20261127", 1, 3), s("20261126", 4, 1)]);
        let days = &b["alpha"];
        assert_eq!(days[0].date, "20261126");
        assert_eq!((days[1].net, days[0].net), (-2, 3));
    }

    fn sample() -> Snapshot {
        Snapshot {
            read_at: "20261127T100000Z".into(),
            queue: vec![QueueRowOut {
                repo: "alpha".into(),
                number: 3,
                class: "codex".into(),
                created: "20261120T000000Z".into(),
                title: "<script>x</script>".into(),
            }],
            tasks: vec![Task {
                host: "lab1".into(),
                seat: "w1".into(),
                task: "alpha#7".into(),
                review: false,
                stage: "assigned",
                last: None,
                created_at: "20261127T080000Z".into(),
                stale: false,
            }],
            landings: vec![
                "20261127T090000Z alpha #5 0123456789abcdef".into(),
                "20261127T090000Z alpha #5 0123456789abcdef".into(),
            ],
            mode: "normal".into(),
            ..Snapshot::default()
        }
    }

    #[test]
    fn rows_have_stable_ids() {
        let s = sample();
        let t = tables(&s);
        assert_eq!(t["queue"][0]["id"], "alpha#3");
        assert_eq!(t["queue"][0]["opened"], "2026-11-20 00:00 UTC");
        assert_eq!(t["tasks"][0]["id"], "lab1/w1/alpha#7/implement");
        assert_eq!(t["tasks"][0]["age"], 120);
        assert_eq!(t["landings"][1]["id"], format!("{} (2)", s.landings[0]));
        assert_eq!(t["fleet"][1]["value"], "none");
        // Every table has a row list, and ids are unique within each.
        for tab in grid::TABS {
            for d in tab.tables {
                let rows = &t[d.id];
                let ids: std::collections::HashSet<_> =
                    rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
                assert_eq!(ids.len(), rows.len(), "{}", d.id);
            }
        }
        // A later reading changes only what changed: the task ages, the queue grows.
        let mut later = s.clone();
        later.read_at = "20261127T101000Z".into();
        later.queue.push(QueueRowOut {
            repo: "alpha".into(),
            number: 4,
            class: "codex".into(),
            created: "20261121T000000Z".into(),
            title: "Next".into(),
        });
        let d = grid::diff(&t, &tables(&later));
        assert_eq!(d.keys().copied().collect::<Vec<_>>(), ["queue", "tasks"]);
        assert_eq!(d["queue"].add.len(), 1);
        assert!(d["queue"].update.is_empty() && d["queue"].remove.is_empty());
        assert_eq!(d["tasks"].update[0]["age"], 130);
    }

    #[test]
    fn render_escapes() {
        let h = render(&sample());
        assert!(h.contains("&lt;script&gt;x&lt;/script&gt;"));
        assert!(!h.contains("<script>"));
        assert!(h.contains("<caption>Claimable issues in order, priority first</caption>"));
        assert!(h.contains("<a href=\"#queue\" id=\"tab-queue\" data-tab=\"queue\">Queue <span class=\"count\">1</span></a>"));
        assert!(h.contains("<section class=\"panel\" id=\"usage\""));
        assert!(h.contains("<td>2h</td>"), "age as text");
    }
}
