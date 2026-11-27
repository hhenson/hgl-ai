//! Removals a seat still owes (PROTO-14): a tree whose removal on `landed` or `closed`
//! failed (a salvage patch that could not be written, a directory that would not go). The
//! end stays recorded in `HGL_ENDED`, so the task's older notes stay moot; the removal is
//! kept here, apart from it, and tried again until it works, then reported.
//!
//! Kept in `<data_dir>/bridge/cleanup/<seat>.json`, so a restarted bridge goes on trying.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use hgl_ai_proto::TaskKey;
use serde::{Deserialize, Serialize};

/// Largest file read back; a larger one is ignored.
const MAX_FILE: u64 = 16 << 20;

/// One removal owed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owed {
    pub repo: String,
    pub issue: u64,
    pub review: bool,
    /// Stream sequence of the ending note: a tree made for a later note is not this one's.
    pub end: u64,
    /// The last error, one line.
    pub error: String,
    /// Tries so far.
    pub tries: u32,
}

impl Owed {
    pub fn task(&self) -> TaskKey {
        TaskKey {
            repo: self.repo.clone(),
            issue: self.issue,
        }
    }
}

#[derive(Debug, Default)]
pub struct Cleanup {
    path: Option<PathBuf>,
    owed: Vec<Owed>,
}

impl Cleanup {
    /// The removals recorded at `path`; none when the file is missing or unreadable (logged).
    pub fn load(path: PathBuf) -> Cleanup {
        let owed = match read(&path) {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!(path = %path.display(), "ignoring the removals owed: {e}");
                Vec::new()
            }
        };
        Cleanup {
            path: Some(path),
            owed,
        }
    }

    pub fn list(&self) -> Vec<Owed> {
        self.owed.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.owed.is_empty()
    }

    /// Record (or update) a failed removal of `task`'s tree.
    pub fn owe(&mut self, task: &TaskKey, review: bool, end: u64, error: &str) {
        let error: String = error
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(300)
            .collect();
        match self
            .owed
            .iter_mut()
            .find(|o| o.task() == *task && o.review == review)
        {
            Some(o) => {
                o.end = o.end.max(end);
                o.error = error;
                o.tries += 1;
            }
            None => self.owed.push(Owed {
                repo: task.repo.clone(),
                issue: task.issue,
                review,
                end,
                error,
                tries: 1,
            }),
        }
        self.save_logged();
    }

    /// The removal of `task`'s tree is done, or no longer owed.
    pub fn settle(&mut self, task: &TaskKey, review: bool) {
        let before = self.owed.len();
        self.owed
            .retain(|o| !(o.task() == *task && o.review == review));
        if self.owed.len() != before {
            self.save_logged();
        }
    }

    fn save_logged(&self) {
        if let Err(e) = self.save() {
            tracing::warn!("saving the removals owed: {e}");
        }
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(&self.owed)?)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }
}

fn read(path: &Path) -> std::io::Result<Vec<Owed>> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_FILE + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_FILE {
        return Err(std::io::Error::other("over the size limit"));
    }
    serde_json::from_slice(&buf).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owed_removals_survive_a_restart() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("cleanup/w1.json");
        let t = TaskKey {
            repo: "demo".into(),
            issue: 7,
        };
        let mut c = Cleanup::load(p.clone());
        c.owe(&t, false, 12, "salvage: No space left on device\nmore");
        c.owe(&t, false, 12, "again");
        let again = Cleanup::load(p.clone());
        let owed = again.list();
        assert_eq!(owed.len(), 1);
        assert_eq!((owed[0].tries, owed[0].error.as_str()), (2, "again"));
        let mut c = again;
        c.settle(&t, false);
        assert!(Cleanup::load(p).is_empty());
    }
}
