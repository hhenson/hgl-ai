//! A builder's Cargo intermediate cache. Each slot has a stable checkout path and
//! one owner from before checkout through cleanup. An interrupted slot is discarded
//! instead of being lent to a build that could overlap a surviving process.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use hgl_ai_proto::build::{check_build_id, new_build_id};
use serde::{Deserialize, Serialize};

use crate::exec::Executor;

pub(crate) struct Cache {
    // The builder's service lease excludes other processes from this index.
    index: PathBuf,
    namespace: String,
    limit: u64,
    live: Mutex<BTreeSet<String>>,
}

/// An index file with no byte count was checked out or abandoned. A byte count is
/// published only once the build has passed and checkout cleanup has completed.
#[derive(Deserialize, Serialize)]
struct Record {
    repo: String,
    bytes: Option<u64>,
}

struct Entry {
    id: String,
    path: PathBuf,
    modified: SystemTime,
    record: Option<Record>,
}

pub(crate) struct Lease {
    cache: Arc<Cache>,
    id: String,
    repo: String,
    pub path: String,
}

impl Cache {
    pub fn new(index_root: &Path, namespace: String, gib: u32) -> Self {
        Self {
            index: index_root.join(&namespace),
            namespace,
            limit: u64::from(gib) << 30,
            live: Mutex::default(),
        }
    }

    pub fn reserve(self: &Arc<Self>, repo: &str, ex: &dyn Executor) -> io::Result<Option<Lease>> {
        if self.limit == 0 {
            return Ok(None);
        }
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let entries = self.entries()?;
        let id = entries
            .iter()
            .rev()
            .find(|e| {
                !live.contains(&e.id)
                    && e.record
                        .as_ref()
                        .is_some_and(|r| r.repo == repo && r.bytes.is_some())
            })
            .map_or_else(
                || loop {
                    let id = new_build_id();
                    if entries.iter().all(|e| e.id != id) {
                        break id;
                    }
                },
                |e| e.id.clone(),
            );
        let path = ex.cargo_cache_path(&self.slot_name(&id));
        self.write(&id, repo, None)?;
        live.insert(id.clone());
        Ok(Some(Lease {
            cache: self.clone(),
            id,
            repo: repo.to_string(),
            path,
        }))
    }

    fn slot_name(&self, id: &str) -> String {
        format!("cache-{}-{id}", self.namespace)
    }

    fn entries(&self) -> io::Result<Vec<Entry>> {
        let dirs = match fs::read_dir(&self.index) {
            Ok(dirs) => dirs,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut entries = Vec::new();
        for item in dirs {
            let item = item?;
            let path = item.path();
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if path.extension().is_none_or(|ext| ext != "json")
                || check_build_id(id).is_err()
                || !item.file_type()?.is_file()
            {
                continue;
            }
            entries.push(Entry {
                id: id.into(),
                modified: item.metadata()?.modified()?,
                record: serde_json::from_slice(&fs::read(&path)?).ok(),
                path,
            });
        }
        entries.sort_by(|a, b| (a.modified, &a.id).cmp(&(b.modified, &b.id)));
        Ok(entries)
    }

    fn write(&self, id: &str, repo: &str, bytes: Option<u64>) -> io::Result<()> {
        fs::create_dir_all(&self.index)?;
        let path = self.index.join(id).with_extension("json");
        let tmp = path.with_extension("tmp");
        let record = Record {
            repo: repo.to_string(),
            bytes,
        };
        fs::write(&tmp, serde_json::to_vec(&record)?)?;
        fs::rename(tmp, path)
    }

    /// Remove incomplete slots and evict least recently used idle slots to the cap.
    /// A failed deletion leaves its record for the next pass.
    pub fn prune(&self, ex: &dyn Executor) {
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let entries = match self.entries() {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("reading the Cargo cache index: {e}");
                return;
            }
        };
        let bytes = |e: &Entry| e.record.as_ref().and_then(|r| r.bytes);
        let mut used = entries
            .iter()
            .filter_map(bytes)
            .fold(0u64, u64::saturating_add);
        for e in &entries {
            if live.contains(&e.id) || (bytes(e).is_some() && used <= self.limit) {
                continue;
            }
            let path = ex.cargo_cache_path(&self.slot_name(&e.id));
            // Invalidate before deletion so a partial or interrupted deletion
            // cannot leave an eligible slot.
            if let Some(r) = &e.record
                && let Err(err) = self.write(&e.id, &r.repo, None)
            {
                tracing::warn!("invalidating Cargo cache {path}: {err}");
                continue;
            }
            match ex.remove_cargo_cache(&path) {
                Ok(()) => {
                    used = used.saturating_sub(bytes(e).unwrap_or(0));
                    remove_index_file(&e.path);
                }
                Err(err) => tracing::warn!("evicting Cargo cache {path}: {err:?}"),
            }
        }
    }
}

fn remove_index_file(path: &Path) {
    if let Err(e) = fs::remove_file(path) {
        tracing::warn!("removing Cargo cache index record {}: {e}", path.display());
    }
}

impl Lease {
    pub fn checkout_id(&self) -> String {
        self.cache.slot_name(&self.id)
    }

    pub fn finish(self, ex: &dyn Executor, keep: bool) {
        if keep {
            match ex.cargo_cache_size(&self.path) {
                Ok(size) if size > 0 && size <= self.cache.limit => {
                    if let Err(e) = self.cache.write(&self.id, &self.repo, Some(size)) {
                        tracing::warn!("recording Cargo cache {}: {e}", self.path);
                    }
                }
                Ok(_) => (),
                Err(e) => tracing::warn!("measuring Cargo cache {}: {e:?}", self.path),
            }
        }
        let cache = self.cache.clone();
        drop(self);
        cache.prune(ex);
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.cache
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::local::Local;

    fn executor(dir: &Path) -> Local {
        Local::new(dir.join("builder"), Default::default(), Default::default())
    }

    fn fill(lease: &Lease, bytes: u64) {
        fs::create_dir_all(&lease.path).unwrap();
        fs::File::create(Path::new(&lease.path).join("artifact"))
            .unwrap()
            .set_len(bytes)
            .unwrap();
    }

    #[test]
    fn only_completed_slots_can_be_reused() {
        let dir = tempfile::tempdir().unwrap();
        let ex = executor(dir.path());
        let index = dir.path().join("index");
        let cache = Arc::new(Cache::new(&index, "test".into(), 1));
        let interrupted = cache.reserve("a", &ex).unwrap().unwrap();
        let successful = cache.reserve("a", &ex).unwrap().unwrap();
        assert_ne!(interrupted.path, successful.path);
        fill(&interrupted, 1);
        fill(&successful, 1);
        let abandoned_path = interrupted.path.clone();
        let ready_path = successful.path.clone();
        successful.finish(&ex, true);
        assert!(
            Path::new(&abandoned_path).exists(),
            "active slots are protected"
        );
        drop(interrupted);
        drop(cache);

        let cache = Arc::new(Cache::new(&index, "test".into(), 1));
        cache.prune(&ex);
        assert!(!Path::new(&abandoned_path).exists());
        assert_ne!(cache.reserve("b", &ex).unwrap().unwrap().path, ready_path);
        assert_eq!(cache.reserve("a", &ex).unwrap().unwrap().path, ready_path);
    }

    #[test]
    fn discards_oldest_idle_slots_over_budget() {
        let dir = tempfile::tempdir().unwrap();
        let ex = executor(dir.path());
        let mut cache = Cache::new(dir.path(), "index".into(), 1);
        cache.limit = 10;
        let cache = Arc::new(cache);
        let a = cache.reserve("a", &ex).unwrap().unwrap();
        let a_path = a.path.clone();
        let a_entry = cache.index.join(&a.id).with_extension("json");
        fill(&a, 6);
        a.finish(&ex, true);
        fs::File::open(a_entry)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        let b = cache.reserve("b", &ex).unwrap().unwrap();
        let b_path = b.path.clone();
        fill(&b, 6);
        b.finish(&ex, true);
        assert!(!Path::new(&a_path).exists());
        assert!(Path::new(&b_path).exists());

        let oversized = cache.reserve("c", &ex).unwrap().unwrap();
        let oversized_path = oversized.path.clone();
        fill(&oversized, 11);
        oversized.finish(&ex, true);
        assert!(!Path::new(&oversized_path).exists());
        assert!(Path::new(&b_path).exists());

        let disabled = Arc::new(Cache::new(dir.path(), "index".into(), 0));
        disabled.prune(&ex);
        assert!(!Path::new(&b_path).exists());
        assert!(disabled.reserve("b", &ex).unwrap().is_none());
    }
}
