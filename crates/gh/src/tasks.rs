//! `HGL_STATE/tasks`: the last note about each task that has a worktree, kept by the
//! coordinator so the console's page shows a task's stage from the hub's buckets alone
//! (docs/owner.md §3). One line per task, sorted:
//! `REPO<TAB>ISSUE<TAB>KIND<TAB>SENDER<TAB>RECIPIENT<TAB>AT`. Written only when it changes.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use hgl_ai_bus::Bus;
use hgl_ai_proto::{Kind, Note, STREAM_NOTES, TreeRecord};

use crate::{state, store};

/// Between readings of the notes stream.
pub const TASKS_EVERY: Duration = Duration::from_secs(10);
/// Notes read at start: the bus keeps 14 days.
const NOTES_WINDOW_DAYS: i64 = 14;
const NOTES_MAX: usize = 20_000;

/// The last note about one task.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    by_task: BTreeMap<(String, u64), LastNote>,
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
            .unwrap_or_else(|| Utc::now() - chrono::Duration::days(NOTES_WINDOW_DAYS));
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

/// The value of `tasks`: the tasks of `trees` that have a note.
pub fn render(idx: &NotesIndex, trees: &[TreeRecord]) -> String {
    let tasks: BTreeSet<(&str, u64)> = trees
        .iter()
        .map(|t| (t.task.repo.as_str(), t.task.issue))
        .collect();
    let mut s = String::new();
    for (repo, issue) in tasks {
        if let Some(l) = idx.get(repo, issue) {
            s.push_str(&format!(
                "{repo}\t{issue}\t{}\t{}\t{}\t{}\n",
                l.kind.as_str(),
                l.sender,
                l.recipient,
                l.at
            ));
        }
    }
    s
}

/// Keep `tasks` current, every [`TASKS_EVERY`], until the process ends.
pub async fn run(bus: &Bus) {
    let mut idx = NotesIndex::default();
    let mut written: Option<String> = None;
    loop {
        if let Err(e) = keep(bus, &mut idx, &mut written).await {
            tracing::warn!("{}: {e}", state::KEY_TASKS);
        }
        tokio::time::sleep(TASKS_EVERY).await;
    }
}

/// What a round does with `tasks`, given what it wrote last and what the key holds.
#[derive(Debug, PartialEq, Eq)]
enum Write {
    /// The same as last time: nothing.
    Keep,
    /// Put the new text.
    Put,
    /// No task: delete what the key holds (an empty value is a delete on the bus).
    Clear(String),
    /// No task, and the key holds nothing: nothing, but remember it.
    Absent,
}

fn decide(text: &str, written: Option<&str>, held: Option<&str>) -> Write {
    match (text.is_empty(), written, held) {
        (_, Some(w), _) if w == text => Write::Keep,
        (false, ..) => Write::Put,
        (true, _, Some(h)) if !h.is_empty() => Write::Clear(h.to_string()),
        (true, ..) => Write::Absent,
    }
}

pub(crate) async fn keep(
    bus: &Bus,
    idx: &mut NotesIndex,
    written: &mut Option<String>,
) -> crate::Result<()> {
    idx.refresh(bus).await?;
    let text = render(idx, &bus.trees().await?);
    if written.as_deref() == Some(text.as_str()) {
        return Ok(());
    }
    // Read only when there is nothing to put: whether an earlier text is left to clear.
    let held = if text.is_empty() {
        bus.state_get(state::KEY_TASKS).await?.map(|e| e.value)
    } else {
        None
    };
    match decide(&text, written.as_deref(), held.as_deref()) {
        Write::Keep | Write::Absent => {}
        Write::Put => {
            store::put_latest(bus, state::KEY_TASKS, &text).await?;
        }
        Write::Clear(h) => {
            store::clear_if(bus, state::KEY_TASKS, &h).await?;
        }
    }
    *written = Some(text);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hgl_ai_proto::Seat;

    /// No task: the key is never created empty (on the bus, an empty write of a key that
    /// is not there fails its compare-and-set every round).
    #[test]
    fn no_task_writes_nothing_or_clears() {
        assert_eq!(decide("", None, None), Write::Absent);
        assert_eq!(
            decide("", Some("a\n"), Some("a\n")),
            Write::Clear("a\n".into())
        );
        assert_eq!(decide("", None, Some("a\n")), Write::Clear("a\n".into()));
        assert_eq!(decide("", Some(""), None), Write::Keep);
        assert_eq!(decide("b\n", None, None), Write::Put);
        assert_eq!(decide("b\n", Some("a\n"), None), Write::Put);
        assert_eq!(decide("b\n", Some("b\n"), None), Write::Keep);
    }

    #[test]
    fn last_note_per_task_with_a_tree() {
        let mut idx = NotesIndex::default();
        let mut n = Note::new(
            Seat::new("controller").unwrap(),
            Seat::new("w1").unwrap(),
            Kind::Assign,
            "x",
        );
        n.repo = Some("alpha".into());
        n.issue = 7;
        idx.add(1, &n);
        assert_eq!(idx.get("alpha", 7).unwrap().kind, Kind::Assign);
        n.kind = Kind::Sendback;
        n.head = Some("1".repeat(40));
        idx.add(3, &n);
        n.kind = Kind::Ready;
        idx.add(2, &n);
        assert_eq!(
            idx.get("alpha", 7).unwrap().kind,
            Kind::Sendback,
            "older notes ignored"
        );
        n.issue = 8;
        idx.add(4, &n);
        let tree = |issue: u64, seat: &str| -> TreeRecord {
            serde_json::from_value(serde_json::json!({
                "host": "lab1", "seat": seat, "task": {"repo": "alpha", "issue": issue},
                "path": "/w", "created_at": "20261128T000000Z",
            }))
            .unwrap()
        };
        // Issue 7 twice (an implementor and a reviewer), 9 without a note; 8 has no tree.
        let text = render(&idx, &[tree(7, "w1"), tree(7, "r1"), tree(9, "w2")]);
        assert_eq!(
            text,
            format!("alpha\t7\tsendback\tcontroller\tw1\t{}\n", n.created_at)
        );
    }
}
