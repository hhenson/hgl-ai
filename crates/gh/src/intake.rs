//! ROLE-3 intake. GitHub supplies the latest opening of each open item. The hub
//! keeps the scan cursor, a short overlap of seen events, and the pending note.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use hgl_ai_bus::{Bus, BusError, NOTES_DUPLICATE_WINDOW};
use hgl_ai_proto::{Kind, Note, STREAM_NOTES, Seat, TaskKey, TreeRecord, parse_stamp};
use serde::{Deserialize, Serialize};

use crate::backlog::{BACKLOG_EVERY, CLAIMABLE, NOT_CLAIMABLE, PRIORITY, clean_title};
use crate::exec::{self, Gh};
use crate::table::{self, RateLimit};
use crate::{Error, Result, Settings};

pub(crate) const KEY: &str = "coord.intake";
const LOOKBACK: TimeDelta = TimeDelta::seconds(BACKLOG_EVERY.as_secs() as i64);
const ASSIGNMENT_HISTORY_MAX: usize = 20_000;

#[derive(Default, Serialize, Deserialize)]
struct Cursor {
    through: Option<DateTime<Utc>>,
    seen: BTreeMap<String, DateTime<Utc>>,
    pending: Option<Note>,
}

pub(crate) struct Round {
    cursor: Cursor,
    revision: u64,
    until: DateTime<Utc>,
}

impl Round {
    pub(crate) async fn load(bus: Option<&Bus>, until: DateTime<Utc>) -> Result<Self> {
        let mut round = Self {
            cursor: Cursor::default(),
            revision: 0,
            until,
        };
        if let Some(bus) = bus
            && let Some(entry) = bus.state_get(KEY).await?
        {
            round.cursor = serde_json::from_str(&entry.value)
                .map_err(|e| crate::invalid(format!("reading {KEY}: {e}")))?;
            round.revision = entry.revision;
        }
        Ok(round)
    }

    pub(crate) fn from(&self) -> Option<DateTime<Utc>> {
        let through = self.cursor.through.unwrap_or(self.until);
        if self.cursor.pending.is_some() || through > self.until {
            return None;
        }
        Some(through - LOOKBACK)
    }

    /// Commit the outbox and cursor together before publishing. A concurrent
    /// round that lost the revision check leaves delivery to the winner.
    pub(crate) async fn finish(
        mut self,
        bus: Option<&Bus>,
        s: &Settings,
        mut items: Vec<Item>,
        out: &mut impl Write,
    ) -> Result<()> {
        let from = self.from().expect("a scan was started");
        items.retain(|item| {
            let event = item.event();
            event.at >= from && event.at < self.until && !self.cursor.seen.contains_key(&event.id)
        });
        let (trees, assigned) = match (bus, items.iter().map(|i| i.event().at).min()) {
            (Some(bus), Some(since)) => (bus.trees().await?, assignments(bus, since).await?),
            _ => Default::default(),
        };
        let mut body = String::from("[coord] Intake (ROLE-3)\n");
        let mut count = 0;
        for item in items {
            let event = item.event();
            if self
                .cursor
                .seen
                .insert(event.id.clone(), event.at)
                .is_some()
            {
                continue;
            }
            if item.handled(&trees) || assigned.get(&item.task()).is_some_and(|at| *at >= event.at)
            {
                continue;
            }
            item.write_line(s, &trees, &mut body);
            count += 1;
        }
        if count > 0 {
            let note = Note::new(s.coord_seat.clone(), Seat::new("triage")?, Kind::Note, body);
            note.validate()?;
            self.cursor.pending = Some(note);
        }
        self.cursor.through = Some(self.until);
        self.cursor
            .seen
            .retain(|_, at| *at >= self.until - LOOKBACK);
        match bus {
            Some(bus) if self.save(bus).await? => self.deliver(bus, false).await,
            None => {
                if let Some(note) = self.cursor.pending {
                    write!(out, "would send to {}\n{}", note.recipient, note.body)
                        .map_err(crate::io_err("printing intake"))?;
                }
                Ok(())
            }
            Some(_) => Ok(()),
        }
    }

    pub(crate) async fn retry(self, bus: &Bus) -> Result<()> {
        self.deliver(bus, true).await
    }

    async fn deliver(mut self, bus: &Bus, retry: bool) -> Result<()> {
        let Some(note) = self.cursor.pending.take() else {
            return Ok(());
        };
        // The stored note keeps its message ID on retries. If a restart
        // outlasted the duplicate window, the retained inbox is also evidence
        // that the publish succeeded before its cursor update was interrupted.
        if !retry || !already_published(bus, &note).await? {
            let receipt = bus.send(&note).await?;
            tracing::info!(seq = receipt.seq, "intake delivered to {}", note.recipient);
        }
        self.save(bus).await?;
        Ok(())
    }

    async fn save(&mut self, bus: &Bus) -> Result<bool> {
        let value = serde_json::to_string(&self.cursor)
            .map_err(|e| crate::invalid(format!("encoding {KEY}: {e}")))?;
        match bus.state_update(KEY, &value, self.revision).await {
            Ok(rev) => {
                self.revision = rev;
                Ok(true)
            }
            Err(BusError::Conflict { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

/// Cover assignments already sent to a seat whose bridge has yet to register its tree.
async fn assignments(bus: &Bus, since: DateTime<Utc>) -> Result<BTreeMap<TaskKey, DateTime<Utc>>> {
    let messages = bus
        .history(STREAM_NOTES, "hgl.notes.>", since, ASSIGNMENT_HISTORY_MAX)
        .await?;
    let mut assigned = BTreeMap::new();
    for message in messages {
        let Ok(note) = serde_json::from_slice::<Note>(&message.payload) else {
            continue;
        };
        let Some(task) = note.task() else { continue };
        match note.kind {
            Kind::Assign | Kind::Review | Kind::Ready | Kind::Approved | Kind::Sendback => {
                assigned.insert(task, message.time);
            }
            Kind::Closed | Kind::Landed => {
                assigned.remove(&task);
            }
            _ => {}
        }
    }
    Ok(assigned)
}

async fn already_published(bus: &Bus, note: &Note) -> Result<bool> {
    let Some(created) = parse_stamp(&note.created_at) else {
        return Err(crate::invalid("invalid pending intake note timestamp"));
    };
    if (Utc::now() - created).to_std().unwrap_or(Duration::ZERO) < NOTES_DUPLICATE_WINDOW {
        return Ok(false);
    }
    let mut from = 1;
    loop {
        let (notes, last) = bus
            .read_page(std::slice::from_ref(&note.recipient), from, 256)
            .await?;
        if notes.iter().any(|n| n.note == *note) {
            return Ok(true);
        }
        match last {
            Some(seq) => from = seq + 1,
            None => return Ok(false),
        }
    }
}

#[derive(Clone, Copy)]
enum ItemKind {
    Issue,
    Pr,
}

impl ItemKind {
    fn connection(self) -> &'static str {
        match self {
            Self::Issue => "issues",
            Self::Pr => "pullRequests",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    id: String,
    #[serde(rename = "createdAt")]
    at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

#[derive(Deserialize)]
struct Author {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Count {
    total_count: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Node {
    #[serde(flatten)]
    created: Event,
    number: u64,
    title: String,
    updated_at: DateTime<Utc>,
    timeline_items: Nodes<Event>,
    labels: Nodes<Label>,
    assignees: Count,
    review_requests: Option<Count>,
    author: Option<Author>,
    #[serde(default)]
    is_cross_repository: bool,
    head_ref_name: Option<String>,
}

pub(crate) struct Item {
    repo: String,
    kind: ItemKind,
    node: Node,
}

impl Item {
    fn task(&self) -> TaskKey {
        TaskKey {
            repo: self.repo.clone(),
            issue: self.node.number,
        }
    }

    fn event(&self) -> &Event {
        self.node
            .timeline_items
            .nodes
            .last()
            .unwrap_or(&self.node.created)
    }

    fn handled(&self, trees: &[TreeRecord]) -> bool {
        self.node.assignees.total_count > 0
            || self
                .node
                .review_requests
                .as_ref()
                .is_some_and(|r| r.total_count > 0)
            || self.node.labels.nodes.iter().any(|label| {
                let name = label.name.as_str();
                name == CLAIMABLE || name == PRIORITY || NOT_CLAIMABLE.contains(&name)
            })
            || trees.iter().any(|tree| {
                tree.task.repo == self.repo
                    && tree.task.issue == self.node.number
                    && (!tree.review || matches!(self.kind, ItemKind::Pr))
            })
    }

    fn external_pr(&self, trees: &[TreeRecord]) -> bool {
        matches!(self.kind, ItemKind::Pr)
            && (self.node.is_cross_repository
                || !trees.iter().any(|tree| {
                    tree.task.repo == self.repo
                        && !tree.review
                        && tree.branch.is_some()
                        && tree.branch == self.node.head_ref_name
                }))
    }

    fn write_line(&self, s: &Settings, trees: &[TreeRecord], body: &mut String) {
        let event = if self.node.timeline_items.nodes.is_empty() {
            "new"
        } else {
            "reopened"
        };
        let kind = match self.kind {
            ItemKind::Issue => "issue",
            ItemKind::Pr => "PR",
        };
        let author = self.node.author.as_ref().map_or("unknown", |a| &a.login);
        let spec = if self
            .node
            .labels
            .nodes
            .iter()
            .any(|l| l.name == "spec-refine")
        {
            " [spec-refine]"
        } else {
            ""
        };
        let route = if self.external_pr(trees) {
            format!(
                " [external, {} routes review to another model family]",
                s.controller
            )
        } else {
            String::new()
        };
        let _ = writeln!(
            body,
            "- {}#{} [{event} {kind}] by {author}{spec}{route} — {}",
            self.repo,
            self.node.number,
            clean_title(&self.node.title)
        );
    }
}

struct Scan<'a> {
    repo: &'a str,
    kind: ItemKind,
    after: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    page_info: PageInfo,
    nodes: Vec<Node>,
}

fn query(scans: &[Scan<'_>]) -> String {
    let mut vars = String::from("$owner: String!");
    let mut fields = String::new();
    for (i, scan) in scans.iter().enumerate() {
        let _ = write!(vars, ", $repo{i}: String!");
        let mut after = String::new();
        if scan.after.is_some() {
            let _ = write!(vars, ", $after{i}: String!");
            after = format!(", after: $after{i}");
        }
        let _ = writeln!(
            fields,
            "  r{i}: repository(owner: $owner, name: $repo{i}) {{
    items: {}(states: OPEN, first: 50, orderBy: {{field: UPDATED_AT, direction: DESC}}{after}) {{
      pageInfo {{ hasNextPage endCursor }}
      nodes {{
        id number title createdAt updatedAt author {{ login }} {}
        assignees {{ totalCount }}
        labels(first: 100) {{ nodes {{ name }} }}
        timelineItems(last: 1, itemTypes: [REOPENED_EVENT]) {{ nodes {{ ... on ReopenedEvent {{ id createdAt }} }} }}
      }}
    }}
  }}",
            scan.kind.connection(),
            match scan.kind {
                ItemKind::Issue => "",
                ItemKind::Pr => "isCrossRepository headRefName reviewRequests { totalCount }",
            }
        );
    }
    format!("query Intake({vars}) {{\n  rateLimit {{ cost remaining resetAt }}\n{fields}}}")
}

pub(crate) struct Reading {
    pub(crate) rate: RateLimit,
    /// No complete scan when the allowance ran low mid-pagination.
    pub(crate) items: Option<Vec<Item>>,
}

/// Walk both connections down to the cursor, batching the first pages and any
/// overflow. The cursor advances only after every configured repo was read.
pub(crate) async fn read<G: Gh>(gh: &G, s: &Settings, from: DateTime<Utc>) -> Result<Reading> {
    let mut scans: Vec<_> = s
        .repos
        .iter()
        .flat_map(|repo| {
            [ItemKind::Issue, ItemKind::Pr].map(|kind| Scan {
                repo,
                kind,
                after: None,
            })
        })
        .collect();
    let mut items = BTreeMap::new();
    loop {
        let q = query(&scans);
        let mut vars = vec![("owner".to_string(), s.owner.clone())];
        for (i, scan) in scans.iter().enumerate() {
            vars.push((format!("repo{i}"), scan.repo.to_string()));
            if let Some(after) = &scan.after {
                vars.push((format!("after{i}"), after.clone()));
            }
        }
        let vars: Vec<_> = vars.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        let pages = exec::graphql(gh, &q, &vars, false).await?;
        let data = &pages[0]["data"];
        let rate = table::parse_rate(data)?;
        let mut next = Vec::new();
        for (i, mut scan) in scans.into_iter().enumerate() {
            let response = |msg: String| Error::Response {
                what: format!("intake {} {}", s.full(scan.repo), scan.kind.connection()),
                msg,
            };
            let page: Page = serde_json::from_value(data[format!("r{i}")]["items"].clone())
                .map_err(|e| response(e.to_string()))?;
            let recent = page.nodes.last().is_none_or(|n| n.updated_at >= from);
            for node in page.nodes.into_iter().filter(|n| n.updated_at >= from) {
                let key = (scan.repo, node.number);
                let item = Item {
                    repo: scan.repo.into(),
                    kind: scan.kind,
                    node,
                };
                items.entry(key).or_insert(item);
            }
            if recent && page.page_info.has_next_page {
                let after = page
                    .page_info
                    .end_cursor
                    .filter(|cursor| !cursor.is_empty() && scan.after.as_ref() != Some(cursor))
                    .ok_or_else(|| response("pagination did not advance".into()))?;
                scan.after = Some(after);
                next.push(scan);
            }
        }
        if next.is_empty() || rate.remaining <= s.hold_below {
            return Ok(Reading {
                rate,
                items: next.is_empty().then(|| items.into_values().collect()),
            });
        }
        scans = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::fake::FakeGh;
    use crate::testutil::{empty_intake, intake_node, settings};
    use serde_json::json;

    #[tokio::test]
    async fn follows_recent_pages_and_uses_open_and_reopen_events() {
        let mut s = settings();
        s.repos = vec!["alpha".into(), "beta".into()];
        let now = table::parse_time("2026-11-28T12:00:00Z").unwrap();
        let recent = now - TimeDelta::minutes(1);
        let old = now - TimeDelta::hours(1);
        let mut first = empty_intake(2);
        first["data"]["r0"]["items"] = json!({
            "pageInfo": { "hasNextPage": true, "endCursor": "second" },
            "nodes": [intake_node(10, old, recent)]
        });
        // No third page needed for this repository once we reach old updates.
        first["data"]["r2"]["items"] = json!({
            "pageInfo": { "hasNextPage": true, "endCursor": "old-issues" },
            "nodes": [intake_node(11, old, old)]
        });
        let mut reopened = intake_node(15, old, recent);
        reopened["timelineItems"]["nodes"] = json!([{ "id": "reopen", "createdAt": recent }]);
        first["data"]["r3"]["items"]["nodes"] = json!([reopened]);
        let mut second = empty_intake(1);
        second["data"]["r0"]["items"]["nodes"] = json!([intake_node(12, recent, recent)]);
        let gh = FakeGh::default()
            .on("after0=second", &second.to_string())
            .on("query Intake", &first.to_string());
        let round = Round::load(None, now).await.unwrap();
        let result = read(&gh, &s, round.from().unwrap()).await.unwrap();
        let mut out = Vec::new();
        round
            .finish(None, &s, result.items.unwrap(), &mut out)
            .await
            .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("would send to triage\n"));
        assert!(!out.contains("#10"), "an update alone is not an opening");
        assert!(!out.contains("#11"));
        assert!(out.contains("alpha#12 [new issue]"), "{out}");
        assert!(out.contains("beta#15 [reopened PR]"), "{out}");
        let calls = gh.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].contains("repo3=beta"));
        assert!(calls[1].contains("after0=second"));
        assert!(!calls[1].contains("old-issues"));

        first["data"]["rateLimit"]["remaining"] = json!(s.hold_below);
        let gh = FakeGh::default().on("query Intake", &first.to_string());
        let result = read(&gh, &s, now - LOOKBACK).await.unwrap();
        assert!(result.items.is_none());
        assert_eq!(gh.calls().len(), 1);
    }
}
