//! One turn: where it runs, preparing its tree, running the adapter, judging the result.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hgl_ai_proto::{Kind, Note, Role, TaskClass, TaskKey, TreeRecord};
use hgl_ai_tree::{Create, Refresh, TASK_FILE};
use tokio::sync::watch;

use crate::adapter::{self, Parser, TurnInput};
use crate::bus::BusApi;
use crate::lock::{self, FileLock};
use crate::proc::{self, Ended, Limits, Stop, TreeId, TreeSlot};
use crate::{Bridge, IssueTitle, SeatRt, prompt};

/// Where a turn runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    /// `<work_root>/<seat>/_inbox`: notes that belong to no tree of the seat.
    Inbox,
    Tree {
        task: TaskKey,
        review: bool,
    },
}

impl Target {
    pub fn task(&self) -> Option<&TaskKey> {
        match self {
            Target::Inbox => None,
            Target::Tree { task, .. } => Some(task),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Inbox => f.write_str("the inbox directory"),
            Target::Tree {
                task,
                review: false,
            } => write!(f, "{}#{}", task.repo, task.issue),
            Target::Tree { task, review: true } => {
                write!(f, "the review tree of {}#{}", task.repo, task.issue)
            }
        }
    }
}

/// A note's target, and the tree it asks to create.
pub struct Plan {
    pub target: Target,
    pub create: Option<Create>,
}

/// Of a group's creates, by the stream sequence of the note asking, whatever order they
/// arrived in (a naked note comes back after later ones): the review sent last (the latest
/// head), else the implement sent first.
pub fn choose_create(creates: Vec<(u64, Option<Create>)>) -> Option<(u64, Create)> {
    let mut review: Option<(u64, Create)> = None;
    let mut implement: Option<(u64, Create)> = None;
    for (seq, c) in creates {
        match c {
            Some(c @ Create::Review { .. }) if review.as_ref().is_none_or(|(s, _)| seq > *s) => {
                review = Some((seq, c))
            }
            Some(c @ Create::Implement { .. })
                if implement.as_ref().is_none_or(|(s, _)| seq < *s) =>
            {
                implement = Some((seq, c))
            }
            _ => {}
        }
    }
    review.or(implement)
}

/// A task's class and the stream sequence of the note that set it (PROTO-21).
pub type Classed = (Option<TaskClass>, Option<u64>);

/// The task's class after these notes (docs/roles.md §3), in stream order: an `assign`
/// sets it (triage may reclass a task by assigning it again); a `sendback` only moves it up
/// (ROLE-15). A note no newer than the one that last set it (an older note delivered again,
/// in this turn or a later one) changes nothing. The note at `made_by` made the tree of a
/// task that moved here (a `sendback --to` this seat): its class sets the class, as an
/// `assign` would, over the record it moved from.
pub fn task_class(recorded: Classed, notes: &[(u64, u64, Note)], made_by: Option<u64>) -> Classed {
    let mut by_seq: Vec<&(u64, u64, Note)> = notes.iter().collect();
    by_seq.sort_by_key(|(seq, _, _)| *seq);
    by_seq.iter().fold(recorded, |(c, at), (seq, _, n)| {
        if at.is_some_and(|a| *seq <= a) {
            return (c, at);
        }
        match (n.kind, n.class) {
            (_, Some(new)) if made_by == Some(*seq) => (Some(new), Some(*seq)),
            (Kind::Assign, Some(new)) => (Some(new), Some(*seq)),
            (Kind::Sendback, Some(new)) if c.is_none_or(|c| new > c) => (Some(new), Some(*seq)),
            _ => (c, at),
        }
    })
}

/// The task's class in these `HGL_TREES` records: of its implementor trees on any host and
/// seat, the one a note set last, by stream sequence (as `TreeRecord::merge_held` orders it).
pub fn newest_class(recs: &[TreeRecord], task: &TaskKey) -> Classed {
    recs.iter()
        .filter(|r| &r.task == task && !r.review && r.class.is_some())
        .max_by_key(|r| (r.class_seq, r.created_at.clone()))
        .map_or((None, None), |r| (r.class, r.class_seq))
}

/// Where a kept implementor tree's class starts from when a turn brings it newer handoffs
/// (`handoffs`, hgl-ai#55), and the note that moved the task here, if one did. The start is
/// the newer, by stream sequence, of its own class (`own`) and the task's in `recs`, so the
/// class follows the newest handoff and its sequence never goes back. When another tree of
/// the task took a handoff after this tree's last (`own_handoff`) and before the newest of
/// `handoffs`, the task was there in between: the first of `handoffs` after the latest such
/// handoff moved it back, and its class sets the class, as for a tree made anew (the first
/// by sequence, as [`choose_create`] picks, and as turns one note at a time would).
pub fn reused_start(
    own: Classed,
    own_handoff: Option<u64>,
    own_key: &str,
    recs: &[TreeRecord],
    task: &TaskKey,
    handoffs: &[u64],
) -> (Classed, Option<u64>) {
    let newest = newest_class(recs, task);
    let from = if newest.1 > own.1 { newest } else { own };
    let Some(&last) = handoffs.iter().max() else {
        return (from, None);
    };
    let between = recs
        .iter()
        .filter(|r| &r.task == task && !r.review && r.key() != own_key)
        .filter_map(|r| r.handoff_seq.or(r.seq))
        .filter(|h| *h < last && own_handoff.is_none_or(|o| *h > o))
        .max();
    let made_by = between.and_then(|b| handoffs.iter().copied().filter(|h| *h > b).min());
    (from, made_by)
}

/// The stream sequence of the newest `assign` or `sendback` among these notes: a handoff
/// that moved the task to this seat (PROTO-21).
pub fn newest_handoff(notes: &[(u64, u64, Note)]) -> Option<u64> {
    handoffs(notes).max()
}

/// The stream sequences of the `assign` and `sendback` notes among these.
fn handoffs(notes: &[(u64, u64, Note)]) -> impl Iterator<Item = u64> + '_ {
    notes
        .iter()
        .filter(|(_, _, n)| matches!(n.kind, Kind::Assign | Kind::Sendback))
        .map(|(seq, _, _)| *seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_handoff_is_an_assign_or_sendback() {
        let n = |seq, kind| classed(seq, kind, None);
        assert_eq!(newest_handoff(&[]), None);
        assert_eq!(
            newest_handoff(&[
                n(3, Kind::Assign),
                n(9, Kind::Approved),
                n(5, Kind::Sendback)
            ]),
            Some(5)
        );
        assert_eq!(newest_handoff(&[n(9, Kind::Question)]), None);
    }

    /// hgl-ai#55: a kept tree's class follows the newest record, and the handoff that moved
    /// the task back from another tree sets it.
    #[test]
    fn a_kept_tree_starts_from_the_newest_class() {
        use TaskClass::*;
        let task = TaskKey {
            repo: "demo".into(),
            issue: 41,
        };
        let rec = |who: &str, class, class_seq, handoff_seq| TreeRecord {
            host: "build-1".into(),
            seat: hgl_ai_proto::Seat::new(who).unwrap(),
            task: task.clone(),
            review: false,
            path: String::new(),
            branch: None,
            created_at: "20261128T000000Z".into(),
            size_bytes: 0,
            families: Vec::new(),
            class,
            class_seq,
            seq: handoff_seq,
            handoff_seq,
        };
        let w1 = rec("w1", Some(Simple), Some(3), Some(3));
        let w2 = rec("w2", Some(Complex), Some(7), Some(7));
        let own = (w1.class, w1.class_seq);
        let start =
            |recs: &[TreeRecord], h| reused_start(own, Some(3), &w1.key(), recs, &task, &[h]);
        // Moved to w2 at 7, back at 9: w2's class, and the note at 9 moved it.
        let both = [w1.clone(), w2.clone()];
        assert_eq!(start(&both, 9), ((Some(Complex), Some(7)), Some(9)));
        let sb = |seq, c| classed(seq, Kind::Sendback, c);
        let (from, made_by) = start(&both, 9);
        assert_eq!(
            task_class(from, &[sb(9, None)], made_by),
            (Some(Complex), Some(7)),
            "no class: the interim tree's"
        );
        assert_eq!(
            task_class(from, &[sb(9, Some(Standard))], made_by),
            (Some(Standard), Some(9)),
            "the move's class sets it"
        );
        // Never moved: its own, and a lower sendback leaves it.
        assert_eq!(start(std::slice::from_ref(&w1), 9), (own, None));
        assert_eq!(start(&[], 9), (own, None), "the hub unread");
        // A note older than the other tree's handoff moved nothing, and the class keeps
        // the newer sequence, so the old note changes nothing.
        let (from, made_by) = start(&both, 5);
        assert_eq!((from, made_by), ((Some(Complex), Some(7)), None));
        assert_eq!(task_class(from, &[sb(5, Some(Standard))], made_by), from);
        // The own record newer than the other's: kept.
        let w2_old = rec("w2", Some(Complex), Some(2), Some(2));
        assert_eq!(start(&[w1.clone(), w2_old], 9), (own, None));
        // r1's batches on PR 75: the first handoff after w2's moved it back, as one note at a
        // time would, whatever came after it in the batch.
        let batch = |hs: &[u64]| reused_start(own, Some(3), &w1.key(), &both, &task, hs);
        let (from, made_by) = batch(&[11, 9]);
        assert_eq!(made_by, Some(9));
        assert_eq!(
            task_class(from, &[sb(9, None), sb(11, Some(Simple))], made_by),
            (Some(Complex), Some(7)),
            "a lower sendback after the move does not move it down"
        );
        let (from, made_by) = batch(&[9, 11]);
        assert_eq!(
            task_class(from, &[sb(9, Some(Standard)), sb(11, None)], made_by),
            (Some(Standard), Some(9)),
            "the move's own class"
        );
        assert_eq!(batch(&[]), ((Some(Complex), Some(7)), None));
        // r1's minor 3: a tree whose first turn never finished has no handoff yet; the note
        // that made it, delivered again, still moved the task.
        assert_eq!(
            reused_start(own, None, &w1.key(), &both, &task, &[9]).1,
            Some(9)
        );
        // Its own record again in the listing is not another tree.
        assert_eq!(
            start(&[rec("w1", Some(Simple), Some(3), Some(8))], 9),
            (own, None)
        );
    }

    fn classed(seq: u64, kind: Kind, class: Option<TaskClass>) -> (u64, u64, Note) {
        let s = |x: &str| hgl_ai_proto::Seat::new(x).unwrap();
        let mut n = Note::new(s("claude"), s("w1"), kind, "x");
        n.class = class;
        (seq, 1, n)
    }

    #[test]
    fn assign_sets_the_class_and_a_sendback_only_moves_it_up() {
        use TaskClass::*;
        let a = |seq, c| classed(seq, Kind::Assign, Some(c));
        let sb = |seq, c| classed(seq, Kind::Sendback, c);
        let tc = |rec: Option<TaskClass>, notes: &[(u64, u64, Note)]| {
            task_class((rec, None), notes, None).0
        };
        assert_eq!(tc(None, &[a(1, Simple)]), Some(Simple));
        assert_eq!(tc(Some(Standard), &[sb(5, None)]), Some(Standard), "kept");
        assert_eq!(tc(None, &[sb(5, None)]), None, "an unclassed task stays so");
        assert_eq!(tc(Some(Simple), &[sb(5, Some(Standard))]), Some(Standard));
        assert_eq!(
            tc(Some(Complex), &[sb(5, Some(Simple))]),
            Some(Complex),
            "never down"
        );
        assert_eq!(tc(None, &[sb(5, Some(Standard))]), Some(Standard));
        assert_eq!(
            tc(Some(Complex), &[a(9, Simple)]),
            Some(Simple),
            "a new assign reclasses"
        );
        // Stream order, whatever order the batch arrived in.
        assert_eq!(
            tc(None, &[sb(7, Some(Complex)), a(3, Simple)]),
            Some(Complex)
        );
        let note = classed(2, Kind::Question, None);
        assert_eq!(tc(Some(Simple), &[note]), Some(Simple));
        // r3's finding 2: the sequence that set it is kept, and older notes change nothing.
        assert_eq!(
            task_class((None, None), &[a(3, Simple)], None),
            (Some(Simple), Some(3))
        );
        let up = (Some(Complex), Some(5));
        assert_eq!(
            task_class(up, &[a(3, Simple)], None),
            up,
            "an older assign again"
        );
        assert_eq!(
            task_class(up, &[a(5, Simple)], None),
            up,
            "the same note again"
        );
        assert_eq!(
            task_class(up, &[a(6, Simple)], None),
            (Some(Simple), Some(6)),
            "a newer one"
        );
        assert_eq!(
            task_class(up, &[sb(8, Some(Simple))], None),
            up,
            "a lower sendback"
        );
        // The sendback that made the tree of a task moved here sets its class.
        let moved = (Some(Complex), Some(5));
        assert_eq!(
            task_class(moved, &[sb(7, Some(Standard))], Some(7)),
            (Some(Standard), Some(7))
        );
        assert_eq!(task_class(moved, &[sb(7, Some(Standard))], None), moved);
        assert_eq!(
            task_class(moved, &[sb(7, None)], Some(7)),
            moved,
            "no class, no change"
        );
        assert_eq!(Simple.up(), Standard);
        assert_eq!(Standard.up(), Complex);
        assert_eq!(Complex.up(), Complex);
    }

    fn review(h: char) -> Option<Create> {
        Some(Create::Review {
            head: h.to_string().repeat(40),
            base: None,
        })
    }

    fn implement(t: &str) -> Option<Create> {
        Some(Create::Implement {
            title: t.into(),
            base: None,
        })
    }

    /// The bot's case (hgl-ai PR 1): an older review naked and delivered again after a
    /// newer one; the newer head is reviewed.
    #[test]
    fn the_latest_review_and_the_first_implement_by_sequence() {
        let head = |c: Option<(u64, Create)>| match c {
            Some((s, Create::Review { head, .. })) => (s, head),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            head(choose_create(vec![(20, review('b')), (10, review('a'))])),
            (20, "b".repeat(40))
        );
        assert_eq!(
            head(choose_create(vec![
                (10, review('a')),
                (20, review('b')),
                (15, None)
            ])),
            (20, "b".repeat(40))
        );
        match choose_create(vec![(20, implement("late")), (10, implement("first"))]) {
            Some((10, Create::Implement { title, .. })) => assert_eq!(title, "first"),
            other => panic!("{other:?}"),
        }
        assert!(choose_create(vec![(1, None)]).is_none());
    }
}

pub struct Job {
    pub seat: Arc<SeatRt>,
    pub target: Target,
    /// The tree to create, with the stream sequence of the note asking for it.
    pub create: Option<(u64, Create)>,
    /// (seq, delivery count, note)
    pub notes: Vec<(u64, u64, Note)>,
    pub hold: Option<String>,
    pub mode: Option<String>,
    pub binding_idx: usize,
    pub stop: watch::Receiver<Stop>,
    pub slot: TreeSlot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Clean,
    Quota(String),
    Failed(String),
    /// Stopped because its task ended.
    Stopped,
    Shutdown,
    /// The tree cannot be brought to the task's pushed branch without losing work
    /// (hgl-ai#55): the notes are held as for `PrepFailed`, and the controller told once
    /// with a `blocked` note.
    Blocked(String),
    /// Another process holds the turn lock (who, as far as known).
    Busy(String),
    PrepFailed(String),
    /// The agent can never start as given (`proc::Ended::CannotStart`).
    CannotStart(String),
}

#[derive(Debug)]
pub struct TurnOut {
    pub outcome: Outcome,
    pub binding_idx: usize,
}

/// The title naming a new branch of `task` (WORK §2 step 3), trimmed; `None`, with the
/// reason logged, names it from the note's line.
pub(crate) fn new_title(
    lookup: Option<&(dyn IssueTitle + Send + Sync)>,
    task: &TaskKey,
) -> Option<String> {
    let Some(lookup) = lookup else {
        tracing::info!(%task, "hold: naming the new branch from the note");
        return None;
    };
    match lookup.issue_title(&task.repo, task.issue) {
        Ok(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
        Ok(_) => {
            tracing::warn!(%task, "the issue title is empty; naming the new branch from the note");
            None
        }
        Err(why) => {
            tracing::warn!(%task, %why, "issue title unknown; naming the new branch from the note");
            None
        }
    }
}

fn first_line(body: &str) -> String {
    body.lines().next().unwrap_or("").to_string()
}

impl<B: BusApi> Bridge<B> {
    pub(crate) fn plan(&self, seat: &SeatRt, n: &Note) -> Plan {
        let inbox = Plan {
            target: Target::Inbox,
            create: None,
        };
        let Some(task) = n.task() else { return inbox };
        let tree = |review| Target::Tree {
            task: task.clone(),
            review,
        };
        match n.kind {
            Kind::Assign => Plan {
                target: tree(false),
                create: Some(Create::Implement {
                    title: first_line(&n.body),
                    base: n.base.clone(),
                }),
            },
            Kind::Review => match &n.head {
                Some(h) => Plan {
                    target: tree(true),
                    create: Some(Create::Review {
                        head: h.clone(),
                        base: n.base.clone(),
                    }),
                },
                None => inbox,
            },
            _ if self.trees.existing(seat.name(), &task, true).is_some() => Plan {
                target: tree(true),
                create: None,
            },
            _ if self.trees.existing(seat.name(), &task, false).is_some() => Plan {
                target: tree(false),
                create: None,
            },
            // A sendback or approval for a tree this host no longer has (a moved seat):
            // recreate it; the pushed branch is resumed.
            Kind::Sendback | Kind::Approved if seat.cfg.role == Role::Implementor => Plan {
                target: tree(false),
                create: Some(Create::Implement {
                    title: first_line(&n.body),
                    base: n.base.clone(),
                }),
            },
            _ => inbox,
        }
    }

    /// The `HGL_TREES` records, for the task's class (a task moves with its seat, or to
    /// another seat); none when the hub cannot be read.
    async fn tree_records(&self, task: &TaskKey) -> Vec<TreeRecord> {
        self.bus.trees().await.unwrap_or_else(|e| {
            tracing::warn!(%task, "reading HGL_TREES for the task's class: {e}");
            Vec::new()
        })
    }

    pub(crate) async fn turn(self: Arc<Self>, job: Job) -> TurnOut {
        let binding_idx = job.binding_idx;
        let outcome = self.turn_inner(&job).await;
        TurnOut {
            outcome,
            binding_idx,
        }
    }

    async fn turn_inner(&self, job: &Job) -> Outcome {
        let seat = &job.seat;
        let name = seat.name().clone();
        // The directory, its tree, and its turn lock.
        let (dir, held, file, start) = match &job.target {
            Target::Inbox => {
                let trees = self.trees.clone();
                let n = name.clone();
                let dir = match tokio::task::spawn_blocking(move || trees.ensure_inbox(&n)).await {
                    Ok(Ok(d)) => d,
                    Ok(Err(e)) => return Outcome::PrepFailed(e.to_string()),
                    Err(e) => return Outcome::PrepFailed(e.to_string()),
                };
                match take_lock(&dir).await {
                    Ok(l) => (dir, l, None, None),
                    Err(o) => return o,
                }
            }
            Target::Tree { task, review } => {
                let dir = match self.trees.layout().task_dir(&name, task, *review) {
                    Ok(d) => d,
                    Err(e) => return Outcome::PrepFailed(e.to_string()),
                };
                let mut held = None;
                if dir.exists() {
                    match take_lock(&dir).await {
                        Ok(l) => held = Some(l),
                        Err(o) => return o,
                    }
                }
                if *review
                    && matches!(job.create, Some((_, Create::Review { .. })))
                    && dir.join(TASK_FILE).exists()
                {
                    // Each review starts from a fresh tree and session.
                    let trees = self.trees.clone();
                    let (s, t) = (name.clone(), task.clone());
                    match tokio::task::spawn_blocking(move || trees.remove(&s, &t, true)).await {
                        Ok(Ok(Some(rm))) => {
                            let _reg = self.registry_lock(&rm.key).await;
                            let _ = self.bus.tree_del(&rm.key).await;
                        }
                        Ok(Ok(None)) => {}
                        Ok(Err(e)) => {
                            return Outcome::PrepFailed(format!(
                                "removing the previous review tree: {e}"
                            ));
                        }
                        Err(e) => return Outcome::PrepFailed(e.to_string()),
                    }
                }
                // A tree made anew may continue a task begun on another host or seat: its
                // class is in that tree's record (PROTO-21). With the note that made it.
                let mut start: Option<(Classed, Option<u64>)> = None;
                let mut made = false;
                let file = match &job.create {
                    Some((seq, c)) => {
                        let trees = self.trees.clone();
                        let (s, t, c, seq) = (name.clone(), task.clone(), c.clone(), *seq);
                        // No GitHub call during a hold (ROLE-19).
                        let lookup = job.hold.is_none().then(|| self.deps.title.clone());
                        match tokio::task::spawn_blocking(move || {
                            trees.create_for(&s, &t, &c, seq, &|| new_title(lookup.as_deref(), &t))
                        })
                        .await
                        {
                            Ok(Ok(created)) => {
                                made = !created.reused;
                                // Read before this tree's own record replaces the old one.
                                if made && !*review && created.file.class.is_none() {
                                    let recs = self.tree_records(task).await;
                                    start = Some((newest_class(&recs, task), Some(seq)));
                                }
                                if !created.reused
                                    && let Err(e) = self.bus.tree_put(&created.record).await
                                {
                                    tracing::warn!(key = %created.record.key(), "registering: {e}");
                                }
                                created.file
                            }
                            Ok(Err(e)) => return Outcome::PrepFailed(e.to_string()),
                            Err(e) => return Outcome::PrepFailed(e.to_string()),
                        }
                    }
                    None => match hgl_ai_tree::read_task(&dir) {
                        Ok(f) => f,
                        Err(e) => return Outcome::PrepFailed(e.to_string()),
                    },
                };
                // The tree may be new (or recreated): hold the lock of the file now there.
                let held = match held.filter(|l| same_file(l, &dir.join(lock::TURN_LOCK))) {
                    Some(l) => l,
                    None => match take_lock(&dir).await {
                        Ok(l) => l,
                        Err(o) => return o,
                    },
                };
                // A kept implementor tree a newer handoff reaches (hgl-ai#55): the task may
                // have been elsewhere since its last, so its class follows the newest
                // record, and its branch the commits pushed there.
                // A tree whose first turn never finished has no handoff yet: the note that
                // made it is one of these.
                let newer: Vec<u64> = if made || *review {
                    Vec::new()
                } else {
                    handoffs(&job.notes)
                        .filter(|n| file.handoff_seq.is_none_or(|h| *n > h))
                        .collect()
                };
                if !newer.is_empty() {
                    let recs = self.tree_records(task).await;
                    let own = hgl_ai_proto::tree_key(self.trees.host(), &name, task, false);
                    start = Some(reused_start(
                        (file.class, file.class_seq),
                        file.handoff_seq,
                        &own,
                        &recs,
                        task,
                        &newer,
                    ));
                    // Never a turn on a stale tree: one that cannot be brought to the pushed
                    // branch holds its notes and tells the controller.
                    let (trees, d, f) = (self.trees.clone(), dir.clone(), file.clone());
                    match tokio::task::spawn_blocking(move || trees.refresh_branch(&d, &f)).await {
                        Ok(Ok(Refresh::Current)) => {}
                        Ok(Ok(Refresh::Forwarded(head))) => {
                            tracing::info!(seat = %name, %task, %head, "fast-forwarded to the pushed branch");
                        }
                        Ok(Ok(Refresh::Blocked { why, files })) => {
                            return Outcome::Blocked(format!(
                                "{} cannot be brought to its pushed branch: {why} ({})",
                                dir.display(),
                                if files.is_empty() {
                                    "no file changed on both sides".to_string()
                                } else {
                                    files.join(", ")
                                }
                            ));
                        }
                        Ok(Err(e)) => {
                            return Outcome::PrepFailed(format!("fetching the branch: {e}"));
                        }
                        Err(e) => return Outcome::PrepFailed(e.to_string()),
                    }
                }
                (dir, held, Some(file), start)
            }
        };
        match *job.stop.borrow() {
            Stop::Now => return Outcome::Stopped,
            Stop::Graceful(_) => return Outcome::Shutdown,
            Stop::None => {}
        }
        // The task's class (docs/roles.md §3), recorded in its tree; an inbox turn has none.
        let mut file = file;
        let (class, class_seq) = match (file.as_ref(), start) {
            (Some(_), Some((from, made_by))) => task_class(from, &job.notes, made_by),
            (Some(f), None) => task_class((f.class, f.class_seq), &job.notes, None),
            (None, _) => (None, None),
        };
        // The class picks the seat, not the model (PROTO-21): the seat's binding runs it.
        let binding = seat.binding_at(job.binding_idx);
        let r = match self.cfg.resolve(&binding) {
            Ok(r) => r,
            Err(e) => return Outcome::PrepFailed(format!("seat binding: {e}")),
        };
        let mut added_author = false;
        if let Some(f) = file.as_mut() {
            let implementor = !matches!(job.target, Target::Tree { review: true, .. });
            added_author = implementor && !f.families.contains(&r.family);
            if added_author {
                f.families.push(r.family);
            }
            // The newest handoff this tree ran, even one reusing it after another seat's
            // tree was made: verdicts go to that tree's seat (PROTO-21).
            let handoff = implementor
                .then(|| newest_handoff(&job.notes))
                .flatten()
                .filter(|n| f.handoff_seq.is_none_or(|h| *n > h));
            if let Some(n) = handoff {
                f.handoff_seq = Some(n);
            }
            let reclassed = (f.class, f.class_seq) != (class, class_seq);
            if reclassed {
                tracing::info!(seat = %name, target = %job.target, from = ?f.class, to = ?class, ?class_seq, "task class");
                f.class = class;
                f.class_seq = class_seq;
            }
            if (added_author || reclassed || handoff.is_some())
                && let Err(e) = hgl_ai_tree::write_task(&dir, f)
            {
                return Outcome::PrepFailed(format!("recording the family and class: {e}"));
            }
            // Publish on every attempt: a previous attempt may have saved task.json but
            // failed to publish it, and the gate must see the author before the turn.
            let d = dir.clone();
            let size = tokio::task::spawn_blocking(move || hgl_ai_tree::dir_size(&d))
                .await
                .unwrap_or(0);
            let rec = self.trees.record(&dir, f, size);
            if let Err(e) = self.bus.tree_put(&rec).await {
                return Outcome::PrepFailed(format!(
                    "registering the task's families and class: {e}"
                ));
            }
        }
        let (cwd, writable) = match &job.target {
            Target::Inbox => (dir.clone(), vec![dir.clone()]),
            Target::Tree { task, .. } => {
                let mut w = vec![dir.clone()];
                if let Ok(c) = self.trees.layout().clone_dir(&task.repo) {
                    w.push(c);
                }
                (dir.join("src"), w)
            }
        };
        let resume = file
            .as_ref()
            .and_then(|f| f.sessions.get(r.runtime.as_str()))
            .cloned();
        let items: Vec<prompt::Item> = job.notes.iter().map(|(s, d, n)| (*s, *d, n)).collect();
        let controller = self.cfg.controller().ok();
        let text = prompt::build(
            &prompt::Context {
                host: &self.cfg.host,
                seat: &name,
                controller: controller.as_ref(),
                role: seat.cfg.role,
                runtime: r.runtime,
                family: r.family,
                model: &r.model,
                class,
                task: file.as_ref(),
                hold: job.hold.as_deref(),
                mode: job.mode.as_deref(),
            },
            &items,
        );
        let program = self
            .opts
            .bins
            .get(&r.runtime)
            .cloned()
            .unwrap_or_else(|| PathBuf::from(r.runtime.as_str()));
        let input = TurnInput {
            prompt: &text,
            resolved: &r,
            resume: resume.as_deref(),
            cwd: &cwd,
            writable: &writable,
        };
        let mut spec = adapter::command(&program, &input);
        let env: [(&str, OsString); 5] = [
            ("TMPDIR", dir.join("tmp").into()),
            ("CARGO_TARGET_DIR", dir.join("build").into()),
            ("HGL_AI_TASK_DIR", dir.clone().into()),
            ("HGL_AI_SEAT", name.as_str().into()),
            ("HGL_AI_ROLE", seat.cfg.role.as_str().into()),
        ];
        spec.env.extend(env.into_iter().map(|(k, v)| (k.into(), v)));
        let mem_gb = seat.cfg.memcap_gb.unwrap_or(self.cfg.limits.memcap_gb);
        let stem = format!("{}-{}", hgl_ai_proto::utc_now_stamp(), r.runtime);
        let (out_log, err_log) = open_logs(&dir.join("log"), &stem, self.opts.keep_logs);
        if let Err(e) = record_prompt(&dir.join("prompts"), &stem, &text, self.opts.keep_logs) {
            tracing::warn!(dir = %dir.display(), "recording the prompt: {e}");
        }
        tracing::info!(seat = %name, target = %job.target, runtime = %r.runtime, model = %r.model, resume = resume.is_some(), "turn starting");
        let mut parser = Parser::new(r.runtime);
        let limits = Limits {
            mem_bytes: mem_gb.saturating_mul(1 << 30),
            timeout: self.opts.turn_timeout,
            kill_grace: self.opts.kill_grace,
            max_line: self.opts.max_line,
            method: self.opts.cap_method,
        };
        let marker = self.next_marker(&name);
        let before = if added_author { clean_head(&cwd) } else { None };
        let run = proc::run(
            &spec,
            limits,
            job.stop.clone(),
            Some(&held),
            &marker,
            Some(&job.slot),
            |l| parser.line(l),
            out_log,
            err_log,
        )
        .await;
        if run.long_lines > 0 {
            tracing::warn!(seat = %name, lines = run.long_lines, "over-long output lines were skipped");
        }
        let exited_ok = run.ended.success();
        let parsed = parser.finish(exited_ok, &run.stderr_tail);
        // Only remove a newly recorded author when the stream and checkout both prove
        // that this refusal did no work. Unknown state retains the author.
        let mut rollback_error = None;
        if added_author
            && matches!(run.ended, Ended::Exited(_))
            && parsed.quota
            && !parsed.activity
            && run.long_lines == 0
            && before.is_some()
            && before == clean_head(&cwd)
            && let Some(f) = file.as_mut()
        {
            let rec = self.trees.record(&dir, f, 0);
            let _reg = self.registry_lock(&rec.key()).await;
            f.families.retain(|family| *family != r.family);
            if let Err(e) = hgl_ai_tree::write_task(&dir, f) {
                rollback_error = Some(format!("rolling back the family: {e}"));
            } else {
                let d = dir.clone();
                let size = tokio::task::spawn_blocking(move || hgl_ai_tree::dir_size(&d))
                    .await
                    .unwrap_or(0);
                let rec = self.trees.record(&dir, f, size);
                if let Err(e) = self.bus.tree_put(&rec).await {
                    rollback_error = Some(format!("registering the family rollback: {e}"));
                    // Restore the conservative local record if publication failed.
                    f.families.push(r.family);
                    if let Err(e) = hgl_ai_tree::write_task(&dir, f) {
                        tracing::error!("restoring the family after failed rollback: {e}");
                    }
                }
            }
        }
        if let (Some(id), Target::Tree { .. }) = (&parsed.session_id, &job.target)
            && let Err(e) = save_session(&dir, r.runtime.as_str(), id)
        {
            tracing::warn!(dir = %dir.display(), "saving the session id: {e}");
        }
        if !run.reaped {
            tracing::error!(seat = %name, "a process of the turn survived; the turn lock stays held by it");
        }
        drop(held);
        let quota =
            matches!(run.ended, Ended::Exited(_)) && parsed.quota && !parsed.success(exited_ok);
        if !matches!(
            run.ended,
            Ended::CannotStart(_) | Ended::SpawnFailed(_) | Ended::ShutdownKilled
        ) {
            let refusal = quota.then(|| crate::usage::RefusedTurn {
                seat: name,
                family: r.family,
                model: r.model,
                at: hgl_ai_proto::utc_now_stamp(),
                id: marker,
            });
            seat.usage[&r.runtime].after_turn(r.runtime, refusal);
        }
        let err = parsed.error.clone().unwrap_or_default();
        match run.ended {
            Ended::Stopped => Outcome::Stopped,
            Ended::ShutdownKilled => Outcome::Shutdown,
            Ended::TimedOut => Outcome::Failed(format!("time limit {:?}", self.opts.turn_timeout)),
            Ended::MemoryCap => Outcome::Failed(format!("memory cap {mem_gb} GiB reached")),
            Ended::SpawnFailed(e) => Outcome::Failed(e),
            Ended::CannotStart(e) => Outcome::CannotStart(e),
            Ended::Exited(_) if rollback_error.is_some() => {
                Outcome::PrepFailed(rollback_error.unwrap())
            }
            Ended::Exited(_) if parsed.success(exited_ok) => Outcome::Clean,
            Ended::Exited(_) if quota => Outcome::Quota(if err.is_empty() {
                "quota refusal".into()
            } else {
                err
            }),
            Ended::Exited(code) => {
                let tail = run
                    .stderr_tail
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string();
                Outcome::Failed(format!("exit {code}: {err} {tail}").trim().to_string())
            }
        }
    }
}

// A dirty or unreadable checkout cannot prove that the turn did no work.
fn clean_head(cwd: &Path) -> Option<Vec<u8>> {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout)
    };
    let status = git(&[
        "status",
        "--porcelain=v1",
        "--untracked-files=all",
        "--ignored=matching",
    ])?;
    if !status.is_empty() {
        return None;
    }
    git(&["rev-parse", "HEAD"])
}

/// Take `<dir>/turn.lock`, or the `Busy` outcome when another process holds it.
async fn take_lock(dir: &Path) -> Result<FileLock, Outcome> {
    let p = dir.join(lock::TURN_LOCK);
    match FileLock::turn_patiently(&p).await {
        Ok(Some(l)) => Ok(l),
        Ok(None) => Err(Outcome::Busy(holders(&p))),
        Err(e) => Err(Outcome::PrepFailed(format!("{}: {e}", p.display()))),
    }
}

fn same_file(l: &FileLock, p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(p).is_ok_and(|m| l.id() == Some((m.dev(), m.ino())))
    }
    #[cfg(not(unix))]
    {
        let _ = (l, p);
        true
    }
}

fn save_session(dir: &Path, runtime: &str, id: &str) -> hgl_ai_tree::Result<()> {
    let mut f = hgl_ai_tree::read_task(dir)?;
    if f.sessions.get(runtime).map(String::as_str) != Some(id) {
        f.sessions.insert(runtime.to_string(), id.to_string());
        hgl_ai_tree::write_task(dir, &f)?;
    }
    Ok(())
}

/// `<dir>/<stem>.out|.err` (`<UTC>-<runtime>`), keeping the newest `keep` turns.
fn open_logs(
    dir: &Path,
    stem: &str,
    keep: usize,
) -> (Option<std::fs::File>, Option<std::fs::File>) {
    if std::fs::create_dir_all(dir).is_err() {
        return (None, None);
    }
    prune(dir, keep.saturating_sub(1) * 2);
    let open = |ext: &str| std::fs::File::create(dir.join(format!("{stem}.{ext}"))).ok();
    (open("out"), open("err"))
}

/// The turn's whole prompt in `<dir>/<stem>.md`, beside its logs; the newest `keep` kept.
fn record_prompt(dir: &Path, stem: &str, prompt: &str, keep: usize) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    prune(dir, keep.saturating_sub(1));
    std::fs::write(dir.join(format!("{stem}.md")), prompt)
}

/// Remove the oldest files of `dir` (by name) until `keep` are left.
fn prune(dir: &Path, keep: usize) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        names.sort();
        let excess = names.len().saturating_sub(keep);
        for p in names.into_iter().take(excess) {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Who holds a turn lock: the live holders where they can be found, else the pid the
/// lock file names (the bridge that started that turn).
fn holders(p: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(m) = std::fs::metadata(p) {
            let pids = proc::members(&TreeId {
                lock: Some((m.dev(), m.ino())),
                lock_path: Some(p.to_path_buf()),
                ..TreeId::default()
            });
            if !pids.is_empty() {
                return format!("pids {pids:?}");
            }
        }
    }
    match lock::holder(p) {
        Some(pid) => format!("the lock file names pid {pid}"),
        None => "unknown".into(),
    }
}

/// Kill whatever still holds a turn lock (a stray of an earlier turn). `true` when none is left.
pub async fn reap_lock_holders(path: &Path, grace: Duration) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(m) = std::fs::metadata(path) else {
            return true;
        };
        let t = TreeId {
            lock: Some((m.dev(), m.ino())),
            lock_path: Some(path.to_path_buf()),
            ..TreeId::default()
        };
        tracing::warn!(lock = %path.display(), holders = ?proc::members(&t), "killing the holders of a turn lock");
        proc::reap(&t, grace, Duration::from_secs(10)).await
    }
    #[cfg(not(unix))]
    {
        let _ = (path, grace);
        true
    }
}
