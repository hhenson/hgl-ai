//! Real failures of a seat's notes, by stream sequence (PROTO-8): a turn that crashed, hit
//! its memory or time limit, or could not be started this time. Administrative holds (a
//! quota refusal, a tree that cannot be prepared yet, the hub unreadable, the gates) are not
//! failures and are not counted. A note that failed [`Options::max_failures`] times is
//! dead-lettered: terminated and sent to the controller with the reason.
//!
//! Kept in `<data_dir>/bridge/failures/<seat>.json`, so a restarted bridge keeps the count;
//! a seat moved to another host starts again there. Entries go when their note is
//! acknowledged or terminated, and after [`KEEP`].
//!
//! [`Options::max_failures`]: crate::Options::max_failures

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long a count is kept: the stream's `max_age` and a day.
pub const KEEP: Duration = Duration::from_secs(hgl_ai_bus::NOTES_MAX_AGE.as_secs() + 86_400);
/// Most entries kept; the oldest go first.
const MAX_ENTRIES: usize = 10_000;
/// Largest file read back; a larger one is ignored.
const MAX_FILE: u64 = 16 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    seq: u64,
    count: u32,
    /// The last failure, one line.
    last: String,
    /// Unix seconds of the last failure.
    at: i64,
}

#[derive(Debug, Default)]
pub struct Failures {
    path: Option<PathBuf>,
    map: BTreeMap<u64, Entry>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Failures {
    /// The counts recorded at `path`; none when the file is missing or unreadable (logged).
    pub fn load(path: PathBuf) -> Failures {
        let mut f = Failures {
            path: Some(path),
            map: BTreeMap::new(),
        };
        let path = f.path.as_deref().expect("set");
        match read(path) {
            Ok(entries) => f.map = entries.into_iter().map(|e| (e.seq, e)).collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %path.display(), "ignoring the failure counts: {e}"),
        }
        f.prune(now());
        f
    }

    /// Count one more real failure of the note at `seq`; returns the count now.
    pub fn record(&mut self, seq: u64, why: &str) -> u32 {
        let now = now();
        let last: String = why.lines().next().unwrap_or("").chars().take(300).collect();
        let e = self.map.entry(seq).or_insert(Entry {
            seq,
            count: 0,
            last: String::new(),
            at: now,
        });
        e.count += 1;
        e.last = last;
        e.at = now;
        let n = e.count;
        self.prune(now);
        self.save_logged();
        n
    }

    /// Forget the note at `seq` (acknowledged or terminated).
    pub fn clear(&mut self, seq: u64) {
        if self.map.remove(&seq).is_some() {
            self.save_logged();
        }
    }

    #[cfg(test)]
    pub fn count(&self, seq: u64) -> u32 {
        self.map.get(&seq).map_or(0, |e| e.count)
    }

    fn prune(&mut self, now: i64) {
        let keep = KEEP.as_secs() as i64;
        self.map.retain(|_, e| now - e.at <= keep);
        while self.map.len() > MAX_ENTRIES {
            let oldest = self
                .map
                .values()
                .min_by_key(|e| e.at)
                .map(|e| e.seq)
                .expect("not empty");
            self.map.remove(&oldest);
        }
    }

    fn save_logged(&self) {
        if let Err(e) = self.save() {
            tracing::warn!("saving the failure counts: {e}");
        }
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let entries: Vec<&Entry> = self.map.values().collect();
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
        return Err(std::io::Error::other("over the size limit"));
    }
    serde_json::from_slice(&buf).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_survive_a_restart_and_clear() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("failures/w1.json");
        let mut f = Failures::load(p.clone());
        assert_eq!(f.record(7, "exit 2: boom\nmore"), 1);
        assert_eq!(f.record(7, "exit 2"), 2);
        assert_eq!(f.record(9, "time limit"), 1);
        let mut g = Failures::load(p.clone());
        assert_eq!((g.count(7), g.count(9)), (2, 1));
        assert_eq!(g.record(7, "x"), 3);
        g.clear(7);
        assert_eq!(Failures::load(p).count(7), 0);
    }
}
