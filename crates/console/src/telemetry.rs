//! The telemetry page (docs/owner.md §3): one snapshot of the hub, read every few
//! seconds, rendered as HTML panels and pushed to open pages by server-sent events.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use chrono::{DateTime, Duration, Utc};
use hgl_ai_bus::Bus;
use hgl_ai_gh::backlog::{Backlog, QueueRow};
use hgl_ai_gh::table::Table;
use hgl_ai_owner::{Priority, Thread, ThreadKind};
use hgl_ai_proto::{HostRecord, Kind, MetricsSample, Note, STREAM_NOTES, TreeRecord};
use serde::Serialize;

use crate::html::{esc, time, when};

/// Notes read for the tasks panel: the bus keeps 14 days.
const NOTES_WINDOW_DAYS: i64 = 14;
const NOTES_MAX: usize = 20_000;
/// Days of the burndown.
pub const BURNDOWN_DAYS: usize = 14;
/// `prs` older than this is stale (docs/protocol.md §8).
const PRS_STALE_MIN: i64 = 15;

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

fn parse_stamp(s: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y%m%dT%H%M%SZ")
        .ok()
        .map(|n| n.and_utc())
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

/// Read everything the page shows.
pub async fn read(
    bus: &Bus,
    notes: &mut NotesIndex,
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
    if let Err(e) = notes.refresh(bus).await {
        errors.push(format!("notes: {e}"));
    }
    s.hosts = get!("hosts", bus.hosts()).unwrap_or_default();
    let trees: Vec<TreeRecord> = get!("trees", bus.trees()).unwrap_or_default();
    let stale = Duration::from_std(stale_after).unwrap_or(Duration::days(3));
    for t in &trees {
        let last = notes.get(&t.task.repo, t.task.issue).cloned();
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
        let skip = lines.len().saturating_sub(20);
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
    if !errors.is_empty() {
        s.error = Some(errors.join("; "));
    }
    s
}

fn table(caption: &str, head: &[&str], rows: Vec<Vec<String>>, empty: &str) -> String {
    let mut s = format!("<table><caption>{}</caption><thead><tr>", esc(caption));
    for h in head {
        let _ = write!(s, "<th scope=\"col\">{}</th>", esc(h));
    }
    s.push_str("</tr></thead><tbody>");
    if rows.is_empty() {
        let _ = write!(
            s,
            "<tr><td colspan=\"{}\">{}</td></tr>",
            head.len(),
            esc(empty)
        );
    }
    for r in rows {
        s.push_str("<tr>");
        for c in r {
            let _ = write!(s, "<td>{c}</td>");
        }
        s.push_str("</tr>");
    }
    s.push_str("</tbody></table>");
    s
}

fn age(stamp: &str, now: DateTime<Utc>) -> String {
    match parse_stamp(stamp) {
        Some(t) => {
            let d = now - t;
            if d.num_days() > 0 {
                format!("{}d", d.num_days())
            } else if d.num_hours() > 0 {
                format!("{}h", d.num_hours())
            } else {
                format!("{}m", d.num_minutes().max(0))
            }
        }
        None => "?".into(),
    }
}

/// The panels, as HTML (the page body and each server-sent update).
pub fn render(s: &Snapshot) -> String {
    let now = parse_stamp(&s.read_at).unwrap_or_else(Utc::now);
    let mut o = String::new();
    let _ = write!(o, "<p class=\"meta\">Read {}", time(&s.read_at));
    if let Some(e) = &s.error {
        let _ = write!(o, " · <span class=\"warn\">{}</span>", esc(e));
    }
    o.push_str("</p>");

    // Fleet.
    let q = |p: &str| s.open_questions.get(p).copied().unwrap_or(0);
    let _ = write!(
        o,
        "<section aria-labelledby=\"t-fleet\"><h2 id=\"t-fleet\">Fleet</h2><dl class=\"facts\">\
         <dt>Mode</dt><dd>{}</dd><dt>Hold</dt><dd>{}</dd><dt>Stale trees</dt><dd>{}</dd>\
         <dt>Open questions</dt><dd>p1 {} · p2 {} · p3 {}</dd></dl></section>",
        esc(&s.mode),
        s.hold.as_deref().map(esc).unwrap_or_else(|| "none".into()),
        s.stale_trees,
        q("p1"),
        q("p2"),
        q("p3")
    );

    // Hosts and seats.
    let mut rows = Vec::new();
    for h in &s.hosts {
        if h.seats.is_empty() {
            rows.push(vec![
                esc(&h.host),
                "—".into(),
                "".into(),
                "".into(),
                "".into(),
                format!("{} GB", h.memcap_gb),
                format!("{} GB", h.work_root_free_gb),
            ]);
        }
        for st in &h.seats {
            rows.push(vec![
                esc(&h.host),
                esc(st.seat.as_str()),
                esc(st.role.as_str()),
                esc(st.runtime.as_str()),
                esc(&st.model),
                format!("{} GB", h.memcap_gb),
                format!("{} GB", h.work_root_free_gb),
            ]);
        }
    }
    let _ = write!(
        o,
        "<section aria-labelledby=\"t-hosts\"><h2 id=\"t-hosts\">Hosts and seats</h2>{}</section>",
        table(
            "Live hosts (a host record lapses two minutes after its bridge stops)",
            &[
                "Host",
                "Seat",
                "Role",
                "Runtime",
                "Model",
                "Memory cap",
                "Free disk"
            ],
            rows,
            "No live host."
        )
    );

    // Tasks in flight.
    let rows = s
        .tasks
        .iter()
        .map(|t| {
            let last = t
                .last
                .as_ref()
                .map(|l| {
                    format!(
                        "{} {}→{} {}",
                        esc(l.kind.as_str()),
                        esc(&l.sender),
                        esc(&l.recipient),
                        time(&l.at)
                    )
                })
                .unwrap_or_else(|| "—".into());
            vec![
                esc(&t.seat),
                format!(
                    "{}{}",
                    esc(&t.task),
                    if t.review { " (review)" } else { "" }
                ),
                esc(t.stage),
                last,
                format!(
                    "{}{}",
                    age(&t.created_at, now),
                    if t.stale {
                        " <span class=\"warn\">stale</span>"
                    } else {
                        ""
                    }
                ),
            ]
        })
        .collect();
    let _ = write!(
        o,
        "<section aria-labelledby=\"t-tasks\"><h2 id=\"t-tasks\">Tasks in flight</h2>{}</section>",
        table(
            "Registered worktrees",
            &["Seat", "Issue", "Stage", "Last note", "Age"],
            rows,
            "No task in flight."
        )
    );

    // Pull requests.
    let rows = s
        .prs
        .iter()
        .map(|p| {
            vec![
                format!("{}#{}", esc(&p.repo), p.number),
                format!("<code>{}</code>", esc(&p.head)),
                if p.draft {
                    "draft".into()
                } else {
                    "open".into()
                },
                esc(&p.bot),
                p.unresolved.to_string(),
                esc(&p.checks),
            ]
        })
        .collect();
    let stale = if s.prs_stale && !s.prs_read.is_empty() {
        " <span class=\"warn\">stale</span>"
    } else {
        ""
    };
    let _ = write!(
        o,
        "<section aria-labelledby=\"t-prs\"><h2 id=\"t-prs\">Pull requests</h2><p class=\"meta\">Read {}{stale}</p>{}</section>",
        if s.prs_read.is_empty() {
            "never".into()
        } else {
            time(&s.prs_read)
        },
        table(
            "Open pull requests",
            &["Pull request", "Head", "State", "Bot", "Threads", "Checks"],
            rows,
            "None open."
        )
    );

    // Queue.
    let rows = s
        .queue
        .iter()
        .enumerate()
        .map(|(i, q)| {
            vec![
                (i + 1).to_string(),
                format!("{}#{}", esc(&q.repo), q.number),
                esc(&q.class),
                esc(&when(&q.created)),
                esc(&q.title),
            ]
        })
        .collect();
    let _ = write!(
        o,
        "<section aria-labelledby=\"t-queue\"><h2 id=\"t-queue\">Queue</h2><p class=\"meta\">Read {}</p>{}</section>",
        if s.backlog_read.is_empty() {
            "never".into()
        } else {
            time(&s.backlog_read)
        },
        table(
            "Claimable issues in order, priority first",
            &["#", "Issue", "Class", "Opened", "Title"],
            rows,
            "Nothing claimable."
        )
    );

    // Backlog by label, and the burndown.
    let mut rows = Vec::new();
    for (repo, labels) in &s.labels {
        for (l, n) in labels {
            rows.push(vec![esc(repo), esc(l), n.to_string()]);
        }
    }
    let _ = write!(
        o,
        "<section aria-labelledby=\"t-backlog\"><h2 id=\"t-backlog\">Backlog</h2>{}",
        table(
            "Open issues by label and repository",
            &["Repository", "Label", "Open"],
            rows,
            "No backlog reading yet."
        )
    );
    for (repo, days) in &s.burndown {
        let rows = days
            .iter()
            .map(|d| {
                vec![
                    esc(&format!(
                        "{}-{}-{}",
                        d.date.get(0..4).unwrap_or(""),
                        d.date.get(4..6).unwrap_or(""),
                        d.date.get(6..8).unwrap_or("")
                    )),
                    d.open.to_string(),
                    d.opened.to_string(),
                    d.closed.to_string(),
                    format!("{:+}", d.net),
                ]
            })
            .collect();
        o.push_str(&table(
            &format!("Burndown of {repo}"),
            &["Day", "Open", "Opened", "Closed", "Net"],
            rows,
            "No samples.",
        ));
    }
    o.push_str("</section>");

    // Landings.
    let rows = s
        .landings
        .iter()
        .map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            match w[..] {
                [at, repo, pr, sha, ..] => vec![
                    esc(&when(at)),
                    format!("{} {}", esc(repo), esc(pr)),
                    format!(
                        "<code>{}</code>",
                        esc(&sha.chars().take(8).collect::<String>())
                    ),
                ],
                _ => vec![esc(l), String::new(), String::new()],
            }
        })
        .collect();
    let _ = write!(
        o,
        "<section aria-labelledby=\"t-landed\"><h2 id=\"t-landed\">Landings and releases</h2>{}</section>",
        table(
            "The newest landings first",
            &["When", "Pull request", "Merge"],
            rows,
            "Nothing landed yet."
        )
    );
    o
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

    #[test]
    fn render_escapes() {
        let s = Snapshot {
            read_at: "20261127T100000Z".into(),
            queue: vec![QueueRowOut {
                repo: "alpha".into(),
                number: 3,
                class: "codex".into(),
                created: "20261120T000000Z".into(),
                title: "<script>x</script>".into(),
            }],
            mode: "normal".into(),
            ..Snapshot::default()
        };
        let h = render(&s);
        assert!(h.contains("&lt;script&gt;x&lt;/script&gt;"));
        assert!(!h.contains("<script>"));
        assert!(h.contains("<caption>Claimable issues in order, priority first</caption>"));
    }
}
