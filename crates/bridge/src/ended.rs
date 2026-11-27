//! This host's cache of the tasks a seat saw end (`landed`, `closed`), by `repo#issue`, with
//! the stream sequence of the ending note. The hub's `HGL_ENDED` is the record (PROTO-14), so
//! a seat moved between hosts keeps its ends; this cache answers without the hub for ends
//! seen here. Kept in `<data_dir>/bridge/ended/<seat>.json` for [`KEEP`], longer than the
//! stream keeps notes (PROTO-2). A note of the task sent before its end and delivered again
//! after it is acknowledged without a turn; one sent after the end (a later sequence) is new
//! intent and runs.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hgl_ai_proto::TaskKey;
use serde::{Deserialize, Serialize};

/// How long an end is remembered: the stream's `max_age` and a day.
pub const KEEP: Duration = Duration::from_secs(hgl_ai_bus::NOTES_MAX_AGE.as_secs() + 86_400);
/// Largest file read back; a larger one is ignored.
const MAX_FILE: u64 = 16 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    repo: String,
    issue: u64,
    /// Stream sequence of the ending note.
    seq: u64,
    /// Unix seconds.
    ended_at: i64,
}

#[derive(Debug, Default)]
pub struct EndedTasks {
    path: Option<PathBuf>,
    /// task -> (seq, ended_at)
    map: HashMap<TaskKey, (u64, i64)>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl EndedTasks {
    /// The ends recorded at `path`; none when the file is missing or unreadable (logged).
    pub fn load(path: PathBuf) -> EndedTasks {
        let mut t = EndedTasks {
            path: Some(path),
            map: HashMap::new(),
        };
        let path = t.path.as_deref().expect("set");
        match read(path) {
            Ok(entries) => {
                for e in entries {
                    t.insert(
                        TaskKey {
                            repo: e.repo,
                            issue: e.issue,
                        },
                        e.seq,
                        e.ended_at,
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %path.display(), "ignoring the ended tasks: {e}"),
        }
        t.prune(now());
        t
    }

    fn insert(&mut self, task: TaskKey, seq: u64, at: i64) {
        let e = self.map.entry(task).or_insert((seq, at));
        if seq >= e.0 {
            *e = (seq, at);
        }
    }

    fn prune(&mut self, now: i64) {
        let keep = KEEP.as_secs() as i64;
        self.map.retain(|_, (_, at)| now - *at <= keep);
    }

    /// Whether a note of `task` at stream sequence `seq` was sent before the task ended.
    pub fn moot(&self, task: &TaskKey, seq: u64) -> bool {
        self.map.get(task).is_some_and(|(end, _)| seq < *end)
    }

    /// Record that `task` ended with the note at `seq`, and save.
    pub fn end(&mut self, task: TaskKey, seq: u64) -> std::io::Result<()> {
        let now = now();
        self.insert(task, seq, now);
        self.prune(now);
        self.save()
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut entries: Vec<Entry> = self
            .map
            .iter()
            .map(|(t, (seq, at))| Entry {
                repo: t.repo.clone(),
                issue: t.issue,
                seq: *seq,
                ended_at: *at,
            })
            .collect();
        entries.sort_by(|a, b| (&a.repo, a.issue).cmp(&(&b.repo, b.issue)));
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(&entries)?)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }
}

fn read(path: &Path) -> std::io::Result<Vec<Entry>> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_FILE + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_FILE {
        return Err(std::io::Error::other("over 16 MiB"));
    }
    Ok(serde_json::from_slice(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(n: u64) -> TaskKey {
        TaskKey {
            repo: "demo".into(),
            issue: n,
        }
    }

    #[test]
    fn ends_persist_and_expire() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("ended/w1.json");
        let mut t = EndedTasks::load(p.clone());
        assert!(!t.moot(&task(1), 5));
        t.end(task(1), 10).unwrap();
        t.end(task(1), 8).unwrap();
        assert!(t.moot(&task(1), 9), "the later end stands");
        assert!(!t.moot(&task(1), 10) && !t.moot(&task(1), 11));
        assert!(!t.moot(&task(2), 1));
        let back = EndedTasks::load(p.clone());
        assert!(back.moot(&task(1), 9) && !back.moot(&task(1), 11));
        let old = now() - KEEP.as_secs() as i64 - 1;
        let text = format!(
            "[{{\"repo\":\"demo\",\"issue\":3,\"seq\":4,\"ended_at\":{old}}},{{\"repo\":\"demo\",\"issue\":1,\"seq\":10,\"ended_at\":{}}}]",
            now()
        );
        std::fs::write(&p, text).unwrap();
        let t = EndedTasks::load(p.clone());
        assert!(!t.moot(&task(3), 1), "older than the stream keeps notes");
        assert!(t.moot(&task(1), 9));
        std::fs::write(&p, "not json").unwrap();
        assert!(!EndedTasks::load(p).moot(&task(1), 9));
    }
}
