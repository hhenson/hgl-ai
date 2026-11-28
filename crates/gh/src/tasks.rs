//! `HGL_STATE/tasks`: the worktrees and the last note about each task that has one, kept by
//! the coordinator so the console's page shows them from viewer-readable keys alone, without
//! the trees' paths (docs/owner.md §3). One line per task, sorted:
//! `REPO<TAB>ISSUE<TAB>KIND<TAB>SENDER<TAB>RECIPIENT<TAB>AT`; then one per tree:
//! `# tree<TAB>HOST<TAB>SEAT<TAB>REPO<TAB>ISSUE<TAB>implement|review<TAB>CREATED<TAB>KIND<TAB>SENDER<TAB>RECIPIENT<TAB>AT`,
//! the last four the newest note about the task that the tree's seat sent or got (or that
//! went to `all`), `-` with none; then, per task whose title is known,
//! `# title<TAB>REPO<TAB>ISSUE<TAB>TITLE`. Written only when it changes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use hgl_ai_bus::Bus;
use hgl_ai_proto::{Kind, Note, STREAM_NOTES, Seat, TreeRecord};

use crate::backlog::{clean_title, public_title};
use crate::exec::{self, Gh};
use crate::{Settings, state, store, table};

/// Between readings of the notes stream.
pub const TASKS_EVERY: Duration = Duration::from_secs(10);
/// Between rounds of title reads.
pub const TITLES_EVERY: Duration = Duration::from_secs(30);
/// Notes read at start: the bus keeps 14 days.
const NOTES_WINDOW_DAYS: i64 = 14;
const NOTES_MAX: usize = 20_000;

/// The last note about one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastNote {
    /// The note's stream sequence.
    pub seq: u64,
    pub kind: Kind,
    pub sender: String,
    pub recipient: String,
    pub at: String,
}

/// The notes stream, folded to the last note per task and per task and seat, read
/// incrementally.
#[derive(Default)]
pub struct NotesIndex {
    last_seq: u64,
    last_time: Option<DateTime<Utc>>,
    by_task: BTreeMap<(String, u64), LastNote>,
    /// By task and seat (sender or recipient; `all` for a broadcast).
    by_seat: BTreeMap<(String, u64, String), LastNote>,
}

impl NotesIndex {
    pub fn add(&mut self, seq: u64, n: &Note) {
        if seq <= self.last_seq {
            return;
        }
        self.last_seq = seq;
        if let Some(t) = n.task() {
            let l = LastNote {
                seq,
                kind: n.kind,
                sender: n.sender.to_string(),
                recipient: n.recipient.to_string(),
                at: n.created_at.clone(),
            };
            for seat in [&l.sender, &l.recipient] {
                self.by_seat
                    .insert((t.repo.clone(), t.issue, seat.clone()), l.clone());
            }
            self.by_task.insert((t.repo, t.issue), l);
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

    /// The newest note about the task that `seat` sent or got, or that went to `all`.
    pub fn get_seat(&self, repo: &str, issue: u64, seat: &str) -> Option<&LastNote> {
        let at = |s: &str| self.by_seat.get(&(repo.to_string(), issue, s.to_string()));
        let all = Seat::broadcast();
        match (at(seat), at(all.as_str())) {
            (Some(a), Some(b)) => Some(if a.seq >= b.seq { a } else { b }),
            (a, b) => a.or(b),
        }
    }
}

/// The tasks of `trees`, once each.
fn tasks_of(trees: &[TreeRecord]) -> BTreeSet<(&str, u64)> {
    trees
        .iter()
        .map(|t| (t.task.repo.as_str(), t.task.issue))
        .collect()
}

/// The value of `tasks`: the tasks of `trees` that have a note, each tree with its seat's
/// last note, then the titles known of the tasks of `trees`.
pub fn render(idx: &NotesIndex, trees: &[TreeRecord], titles: &Titles) -> String {
    let tasks = tasks_of(trees);
    let mut s = String::new();
    for &(repo, issue) in &tasks {
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
    let mut sorted: Vec<&TreeRecord> = trees.iter().collect();
    sorted.sort_by_key(|t| t.key());
    for t in sorted {
        let (repo, issue) = (t.task.repo.as_str(), t.task.issue);
        let last = match idx.get_seat(repo, issue, t.seat.as_str()) {
            Some(l) => format!(
                "{}\t{}\t{}\t{}",
                l.kind.as_str(),
                l.sender,
                l.recipient,
                l.at
            ),
            None => "-\t-\t-\t-".into(),
        };
        s.push_str(&format!(
            "# tree\t{}\t{}\t{repo}\t{issue}\t{}\t{}\t{last}\n",
            t.host,
            t.seat,
            if t.review { "review" } else { "implement" },
            t.created_at
        ));
    }
    for &(repo, issue) in &tasks {
        if let Some(t) = titles.get(repo, issue) {
            s.push_str(&format!("# title\t{repo}\t{issue}\t{t}\n"));
        }
    }
    s
}

/// Between tries of a title GitHub did not give.
const TITLE_RETRY: Duration = Duration::from_secs(300);
/// Titles asked for in one round: each is one GraphQL call.
const TITLES_PER_ROUND: usize = 5;

/// Issue titles of the tasks with a worktree, read from GitHub once each (a task's title
/// rarely changes; the page links to the issue).
#[derive(Default, Clone)]
pub struct Titles {
    known: BTreeMap<(String, u64), String>,
    tried: BTreeMap<(String, u64), Instant>,
}

impl Titles {
    pub fn get(&self, repo: &str, issue: u64) -> Option<&str> {
        self.known
            .get(&(repo.to_string(), issue))
            .map(String::as_str)
            .filter(|t| !t.is_empty())
    }

    /// Forget every title of a repository `keep` does not keep (one no longer public).
    pub fn retain_repos(&mut self, keep: impl Fn(&str) -> bool) {
        self.known.retain(|(r, _), _| keep(r));
    }

    pub fn insert(&mut self, repo: &str, issue: u64, title: &str) {
        self.known
            .insert((repo.to_string(), issue), clean_title(title).trim().into());
    }

    /// The tasks of `trees` in a configured repository with no title yet, not tried in
    /// the last [`TITLE_RETRY`]; at most [`TITLES_PER_ROUND`]. Forgets tasks gone from
    /// `trees`.
    fn wanted(&mut self, trees: &[TreeRecord], s: &Settings, now: Instant) -> Vec<(String, u64)> {
        let tasks: BTreeSet<(String, u64)> = tasks_of(trees)
            .into_iter()
            .map(|(r, i)| (r.to_string(), i))
            .collect();
        self.known.retain(|k, _| tasks.contains(k));
        self.tried.retain(|k, _| tasks.contains(k));
        tasks
            .into_iter()
            .filter(|k| s.repos.contains(&k.0) && !self.known.contains_key(k))
            .filter(|k| self.tried.get(k).is_none_or(|t| now - *t >= TITLE_RETRY))
            .take(TITLES_PER_ROUND)
            .collect()
    }
}

/// Read the titles [`Titles::wanted`] names; none while `hold` holds. The lock is never
/// held across a GitHub call.
async fn fill<G: Gh>(
    titles: &Mutex<Titles>,
    gh: &G,
    s: &Settings,
    bus: &Bus,
    trees: &[TreeRecord],
) {
    let now = Instant::now();
    let want = lock(titles).wanted(trees, s, now);
    if want.is_empty() {
        return;
    }
    match bus.state_get(state::KEY_HOLD).await {
        Ok(Some(h)) if table::hold_active(&h.value, chrono::Utc::now()) => return,
        Ok(_) => {}
        Err(e) => return tracing::warn!("reading {}: {e}", state::KEY_HOLD),
    }
    for (repo, issue) in want {
        lock(titles).tried.insert((repo.clone(), issue), now);
        match title(gh, s, &repo, issue).await {
            Ok(t) => lock(titles).insert(&repo, issue, &t),
            Err(e) => {
                tracing::warn!("the title of {repo}#{issue}: {e}");
                if matches!(e, crate::Error::RateLimited(_) | crate::Error::Auth(_)) {
                    return;
                }
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The title of issue or pull request `repo#issue`.
pub(crate) async fn title<G: Gh>(
    gh: &G,
    s: &Settings,
    repo: &str,
    issue: u64,
) -> crate::Result<String> {
    let q = format!(
        "query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ \
         isPrivate issueOrPullRequest(number: {issue}) {{ ... on Issue {{ title }} ... on PullRequest {{ title }} }} }} }}"
    );
    let vars = [("owner", s.owner.clone()), ("name", repo.to_string())];
    let pages = exec::graphql(gh, &q, &vars, false).await?;
    let at = |path: &str| pages.first().and_then(|p| p.pointer(path)).cloned();
    let private = at("/data/repository/isPrivate").and_then(|v| v.as_bool());
    let title = at("/data/repository/issueOrPullRequest/title");
    Ok(public_title(
        private,
        title.as_ref().and_then(|v| v.as_str()).unwrap_or(""),
    ))
}

/// Keep `tasks` current, every [`TASKS_EVERY`], until the process ends; titles from
/// GitHub in a loop of their own, every [`TITLES_EVERY`], so a slow GitHub never holds the
/// stages back.
/// A title shows only for a repository `vis` holds as public (the coordinator's newest
/// round; [`crate::migrate::Visibility`]).
pub async fn run<G: Gh>(bus: &Bus, gh: &G, s: &Settings, vis: &crate::migrate::Visibility) {
    let titles = Mutex::new(Titles::default());
    let stages = async {
        let mut idx = NotesIndex::default();
        let mut written: Option<String> = None;
        loop {
            if let Err(e) = round(bus, &mut idx, &titles, &mut written, vis).await {
                tracing::warn!("{}: {e}", state::KEY_TASKS);
            }
            tokio::time::sleep(TASKS_EVERY).await;
        }
    };
    let reads = async {
        loop {
            match bus.trees().await {
                Ok(trees) => fill(&titles, gh, s, bus, &trees).await,
                Err(e) => tracing::warn!("titles: reading the trees: {e}"),
            }
            tokio::time::sleep(TITLES_EVERY).await;
        }
    };
    tokio::join!(stages, reads);
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

/// One round without titles (the live tests).
#[cfg(test)]
pub(crate) async fn keep(
    bus: &Bus,
    idx: &mut NotesIndex,
    written: &mut Option<String>,
) -> crate::Result<()> {
    round(bus, idx, &Mutex::default(), written, &Default::default()).await
}

/// Read the notes and trees and write `tasks` if it changed; titles as known now.
/// The visibility is held read from the render to the write: a repository turning private
/// is never overtaken by a write of its title made with the older visibility.
pub(crate) async fn round(
    bus: &Bus,
    idx: &mut NotesIndex,
    titles: &Mutex<Titles>,
    written: &mut Option<String>,
    vis: &crate::migrate::Visibility,
) -> crate::Result<()> {
    idx.refresh(bus).await?;
    let trees = bus.trees().await?;
    let vis = vis.read().await;
    let public = |r: &str| crate::migrate::is_public(&vis, r);
    let shown = {
        let mut t = lock(titles);
        t.retain_repos(public);
        t.clone()
    };
    let text = render(idx, &trees, &shown);
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

    fn tree(repo: &str, issue: u64) -> TreeRecord {
        serde_json::from_value(serde_json::json!({
            "host": "lab1", "seat": "w1", "task": {"repo": repo, "issue": issue},
            "path": "/w", "created_at": "20261128T000000Z",
        }))
        .unwrap()
    }

    /// Titles wanted: configured repositories only, once each, not again before the retry,
    /// at most a round's worth; a task gone is forgotten.
    #[test]
    fn titles_wanted() {
        let mut s = crate::testutil::settings();
        s.repos = vec!["alpha".into()];
        let mut t = Titles::default();
        let now = Instant::now();
        let trees: Vec<TreeRecord> = (1..=7)
            .map(|i| tree("alpha", i))
            .chain([tree("elsewhere", 1), tree("alpha", 1)])
            .collect();
        let w = t.wanted(&trees, &s, now);
        assert_eq!(w.len(), TITLES_PER_ROUND);
        assert!(w.iter().all(|(r, _)| r == "alpha"));
        for (r, i) in &w {
            t.tried.insert((r.clone(), *i), now);
        }
        t.insert("alpha", 1, "One");
        let w = t.wanted(&trees, &s, now + Duration::from_secs(1));
        assert_eq!(w, [("alpha".into(), 6), ("alpha".into(), 7)]);
        let w = t.wanted(&trees, &s, now + TITLE_RETRY);
        assert_eq!(w.len(), TITLES_PER_ROUND, "tried again after the retry");
        assert!(!w.contains(&("alpha".into(), 1)), "known");
        t.wanted(&trees[1..2], &s, now);
        assert_eq!(t.get("alpha", 1), None, "forgotten with its tree");
    }

    #[tokio::test]
    async fn title_of_an_issue_or_pull_request() {
        let s = crate::testutil::settings();
        let gh = crate::exec::fake::FakeGh::default().on(
            "issueOrPullRequest(number: 12)",
            r#"{"data":{"repository":{"isPrivate":false,"issueOrPullRequest":{"title":"Twelve"}}}}"#,
        );
        assert_eq!(title(&gh, &s, "alpha", 12).await.unwrap(), "Twelve");
        assert!(gh.calls()[0].contains("name=alpha"), "{:?}", gh.calls());
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
        let trees = [tree(7, "w1"), tree(7, "r1"), tree(9, "w2")];
        let mut titles = Titles::default();
        let at = &n.created_at;
        let text = render(&idx, &trees, &titles);
        assert_eq!(
            text,
            format!(
                "alpha\t7\tsendback\tcontroller\tw1\t{at}\n\
                 # tree\tlab1\tr1\talpha\t7\timplement\t20261128T000000Z\t-\t-\t-\t-\n\
                 # tree\tlab1\tw1\talpha\t7\timplement\t20261128T000000Z\tsendback\tcontroller\tw1\t{at}\n\
                 # tree\tlab1\tw2\talpha\t9\timplement\t20261128T000000Z\t-\t-\t-\t-\n"
            )
        );
        assert!(!text.contains("/w"), "no path: {text}");
        // Titles follow, for every task with a tree, a note or not.
        titles.insert("alpha", 7, "Fix\tthe parser\n");
        titles.insert("alpha", 9, "No note, by owner@example.com");
        let text = render(&idx, &trees, &titles);
        assert!(
            text.ends_with(
                "\n# title\talpha\t7\tFix the parser\n# title\talpha\t9\tNo note, by owner@example.com\n"
            ),
            "{text}"
        );
    }

    /// After a handoff each seat's tree shows its own last note; a note to `all` (`landed`)
    /// reaches every seat's.
    #[test]
    fn each_seat_its_own_last_note() {
        let mut idx = NotesIndex::default();
        let note = |from: &str, to: &str, kind: Kind| {
            let mut n = Note::new(Seat::new(from).unwrap(), Seat::new(to).unwrap(), kind, "x");
            n.repo = Some("alpha".into());
            n.issue = 7;
            n
        };
        idx.add(1, &note("controller", "w1", Kind::Assign));
        idx.add(2, &note("w1", "controller", Kind::Blocked));
        idx.add(3, &note("controller", "w2", Kind::Assign));
        assert_eq!(idx.get_seat("alpha", 7, "w1").unwrap().kind, Kind::Blocked);
        assert_eq!(idx.get_seat("alpha", 7, "w2").unwrap().kind, Kind::Assign);
        assert_eq!(idx.get("alpha", 7).unwrap().seq, 3);
        assert!(idx.get_seat("alpha", 7, "r1").is_none());
        idx.add(4, &note("controller", "all", Kind::Landed));
        for seat in ["w1", "w2", "r1"] {
            assert_eq!(
                idx.get_seat("alpha", 7, seat).unwrap().kind,
                Kind::Landed,
                "{seat}"
            );
        }
        idx.add(5, &note("controller", "w2", Kind::Note));
        assert_eq!(idx.get_seat("alpha", 7, "w2").unwrap().kind, Kind::Note);
        assert_eq!(idx.get_seat("alpha", 7, "w1").unwrap().kind, Kind::Landed);
    }
}
