//! A builder's Cargo intermediate cache. Each slot has a stable checkout path and
//! one owner from before checkout through cleanup. An interrupted slot is discarded
//! instead of being lent to a build that could overlap a surviving process.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use hgl_ai_proto::build::{builder_inbox, check_build_id, new_build_id};
use serde::{Deserialize, Serialize};

use crate::exec::{Executor, HostError};

pub(crate) struct Cache {
    // The builder's service lease excludes other processes from this index.
    index: PathBuf,
    namespace: String,
    limit: u64,
    live: Mutex<BTreeSet<String>>,
    pruning: Mutex<()>,
    retry: AtomicBool,
}

#[derive(Deserialize, Serialize)]
struct Record {
    repo: String,
    bytes: Option<u64>,
    /// Checked out, abandoned, or being deleted. Keep its last known size until
    /// deletion succeeds. Older records with no byte count are also unusable.
    #[serde(default)]
    discard: bool,
}

impl Record {
    fn reusable(&self) -> bool {
        !self.discard && self.bytes.is_some()
    }
}

struct Entry {
    id: String,
    modified: SystemTime,
    record: Option<Record>,
}

pub(crate) struct Lease {
    cache: Arc<Cache>,
    id: String,
    repo: String,
    previous_bytes: Option<u64>,
    released: bool,
    pub path: String,
}

impl Cache {
    pub fn new(index_root: &Path, namespace: String, gib: u32) -> Self {
        Self {
            index: index_root.join(&namespace),
            namespace,
            limit: u64::from(gib) << 30,
            live: Mutex::default(),
            pruning: Mutex::default(),
            retry: AtomicBool::default(),
        }
    }

    pub fn reserve(self: &Arc<Self>, repo: &str, ex: &dyn Executor) -> io::Result<Option<Lease>> {
        if self.limit == 0 || self.needs_retry() {
            return Ok(None);
        }
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let entries = self.entries()?;
        let warm = entries.iter().rev().find(|e| {
            !live.contains(&e.id)
                && e.record
                    .as_ref()
                    .is_some_and(|r| r.repo == repo && r.reusable())
        });
        let id = warm.map_or_else(
            || loop {
                let id = new_build_id();
                if entries.iter().all(|e| e.id != id) {
                    break id;
                }
            },
            |e| e.id.clone(),
        );
        let previous_bytes = warm.and_then(|e| e.record.as_ref()?.bytes);
        let path = ex.cargo_cache_path(&self.slot_name(&id));
        self.write(&id, repo, previous_bytes, true)?;
        live.insert(id.clone());
        Ok(Some(Lease {
            cache: self.clone(),
            id,
            repo: repo.to_string(),
            previous_bytes,
            released: false,
            path,
        }))
    }

    fn slot_name(&self, id: &str) -> String {
        format!("cache-{}-{id}", self.namespace)
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.index.join(id).with_extension("json")
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
            });
        }
        entries.sort_by(|a, b| (a.modified, &a.id).cmp(&(b.modified, &b.id)));
        Ok(entries)
    }

    fn write(&self, id: &str, repo: &str, bytes: Option<u64>, discard: bool) -> io::Result<()> {
        fs::create_dir_all(&self.index)?;
        let path = self.record_path(id);
        let tmp = path.with_extension("tmp");
        let record = Record {
            repo: repo.to_string(),
            bytes,
            discard,
        };
        fs::write(&tmp, serde_json::to_vec(&record)?)?;
        fs::rename(tmp, path)
    }

    /// Reconcile the executor with the local index before the lane takes jobs.
    /// The executor's work directory can outlive its proxy or builder label.
    pub fn clean(&self, ex: &dyn Executor) -> Result<(), HostError> {
        let indexed: BTreeSet<_> = self
            .entries()
            .map_err(index_error)?
            .into_iter()
            .map(|e| self.slot_name(&e.id))
            .collect();
        let platform_suffix = format!(".{}-", ex.platform());
        for slot in ex.cargo_cache_slots()? {
            if let Some((label, id)) = slot
                .strip_prefix("cache-")
                .and_then(|s| s.rsplit_once(&platform_suffix))
                && builder_inbox(label).is_ok()
                && check_build_id(id).is_ok()
                && !indexed.contains(&slot)
            {
                ex.remove_cargo_cache(&ex.cargo_cache_path(&slot))?;
            }
        }
        self.prune(ex)
    }

    pub fn needs_retry(&self) -> bool {
        self.retry.load(Ordering::Relaxed)
    }

    /// Remove incomplete slots and evict least recently used idle slots to the cap.
    /// Serialize pruning passes, but allow leases to be reserved and released while
    /// deletion runs. Each selection uses the current set of idle slots.
    pub fn prune(&self, ex: &dyn Executor) -> Result<(), HostError> {
        self.prune_idle(false, |slot| {
            ex.remove_cargo_cache(&ex.cargo_cache_path(slot))
        })
    }

    fn prune_idle(
        &self,
        over_limit_only: bool,
        remove: impl Fn(&str) -> Result<(), HostError>,
    ) -> Result<(), HostError> {
        let _pruning = self.pruning.lock().unwrap_or_else(|e| e.into_inner());
        let result = self.evict(over_limit_only, remove);
        // A budget-only pass can leave abandoned slots for a pending full retry.
        if !over_limit_only || result.is_err() {
            self.retry.store(result.is_err(), Ordering::Relaxed);
        }
        result
    }

    fn evict(
        &self,
        over_limit_only: bool,
        remove: impl Fn(&str) -> Result<(), HostError>,
    ) -> Result<(), HostError> {
        let mut failed = BTreeSet::new();
        let mut error = None;
        while let Some(id) = self
            .next_eviction(&failed, over_limit_only)
            .map_err(index_error)?
        {
            let slot = self.slot_name(&id);
            let result = remove(&slot).and_then(|()| {
                let _live = self.live.lock().unwrap_or_else(|e| e.into_inner());
                fs::remove_file(self.record_path(&id)).map_err(index_error)
            });
            match result {
                Ok(()) => (),
                Err(err @ HostError::Unreachable(_)) => return Err(err),
                Err(err @ HostError::Failed(_)) => {
                    tracing::warn!("evicting Cargo cache slot {slot}: {err:?}");
                    failed.insert(id);
                    error = Some(err);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn next_eviction(
        &self,
        failed: &BTreeSet<String>,
        over_limit_only: bool,
    ) -> io::Result<Option<String>> {
        let live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let entries = self.entries()?;
        let idle = entries.iter().filter(|e| !live.contains(&e.id));
        let used = idle
            .clone()
            .filter_map(|e| e.record.as_ref()?.bytes)
            .fold(0u64, u64::saturating_add);
        if over_limit_only && used <= self.limit {
            return Ok(None);
        }
        let mut candidates = idle.filter(|e| !failed.contains(&e.id));
        let mut candidate = candidates
            .clone()
            .find(|e| !e.record.as_ref().is_some_and(Record::reusable));
        if candidate.is_none() && used > self.limit {
            candidate = candidates.next();
        }
        if let Some(entry) = candidate
            && let Some(r) = &entry.record
            && r.reusable()
        {
            // A partial or interrupted deletion must never leave a reusable slot.
            self.write(&entry.id, &r.repo, r.bytes, true)?;
        }
        Ok(candidate.map(|e| e.id.clone()))
    }
}

fn index_error(e: io::Error) -> HostError {
    HostError::Failed(format!("updating the Cargo cache index: {e}"))
}

impl Lease {
    pub fn checkout_id(&self) -> String {
        self.cache.slot_name(&self.id)
    }

    pub fn finish(mut self, ex: &dyn Executor, keep: bool) {
        let bytes = if keep {
            match ex.cargo_cache_size(&self.path) {
                Ok(size) => Some(size),
                Err(e) => {
                    tracing::warn!("measuring Cargo cache {}: {e:?}", self.path);
                    None
                }
            }
        } else {
            None
        };
        let reusable = bytes.is_some_and(|size| size > 0 && size <= self.cache.limit);
        self.release(bytes, reusable);
        if let Err(e) = self.cache.prune(ex) {
            tracing::warn!("pruning the Cargo cache: {e:?}");
        }
    }

    /// Checkout failed before any recipe step could touch the Cargo directory.
    pub fn checkout_failed(mut self, ex: &dyn Executor) {
        self.release(self.previous_bytes, true);
        if self.previous_bytes.is_some()
            && let Err(e) = self.cache.prune_idle(true, |slot| {
                ex.remove_cargo_cache(&ex.cargo_cache_path(slot))
            })
        {
            tracing::warn!("pruning the Cargo cache after checkout failed: {e:?}");
        }
    }

    fn release(&mut self, bytes: Option<u64>, reusable: bool) {
        let mut live = self.cache.live.lock().unwrap_or_else(|e| e.into_inner());
        let update = match bytes {
            Some(_) => self.cache.write(&self.id, &self.repo, bytes, !reusable),
            // A checkout failure on a fresh slot never created a Cargo directory.
            None if reusable => fs::remove_file(self.cache.record_path(&self.id)),
            None => Ok(()),
        };
        if let Err(e) = update {
            tracing::warn!("recording Cargo cache {}: {e}", self.path);
        }
        live.remove(&self.id);
        self.released = true;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.released {
            self.cache
                .live
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.id);
        }
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
        cache.prune(&ex).unwrap();
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
        disabled.prune(&ex).unwrap();
        assert!(!Path::new(&b_path).exists());
        assert!(disabled.reserve("b", &ex).unwrap().is_none());
    }

    #[test]
    fn failed_deletions_keep_their_size_and_a_disconnect_stops_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let ex = executor(dir.path());
        let mut cache = Cache::new(dir.path(), "index".into(), 1);
        cache.limit = 10;
        let ids = [
            "20261127-090000-000001",
            "20261127-090000-000002",
            "20261127-090000-000003",
        ];
        for (i, id) in ids.iter().enumerate() {
            cache.write(id, "demo", Some(4), false).unwrap();
            fs::File::open(cache.record_path(id))
                .unwrap()
                .set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(i as u64))
                .unwrap();
        }
        let first = cache.slot_name(ids[0]);
        assert!(matches!(
            cache.prune_idle(true, |slot| {
                assert_eq!(slot, first);
                Err(HostError::Unreachable("disconnected".into()))
            }),
            Err(HostError::Unreachable(_))
        ));
        assert!(cache.needs_retry());
        let record = |id| -> Record {
            serde_json::from_slice(&fs::read(cache.record_path(id)).unwrap()).unwrap()
        };
        assert_eq!(record(ids[0]).bytes, Some(4));
        assert!(!record(ids[0]).reusable());
        assert!(record(ids[1]).reusable());
        assert!(record(ids[2]).reusable());

        assert!(matches!(
            cache.prune_idle(false, |slot| {
                assert!(
                    cache.live.try_lock().is_ok(),
                    "deletion holds the live lock"
                );
                if slot == first {
                    Err(HostError::Failed("locked directory".into()))
                } else {
                    ex.remove_cargo_cache(&ex.cargo_cache_path(slot))
                }
            }),
            Err(HostError::Failed(_))
        ));
        assert_eq!(record(ids[0]).bytes, Some(4));
        assert!(
            !cache.record_path(ids[1]).exists(),
            "stuck bytes count toward the cap"
        );
        assert!(record(ids[2]).reusable());
        cache
            .prune_idle(true, |_| panic!("idle bytes are within the cap"))
            .unwrap();
        assert!(cache.needs_retry(), "an abandoned slot still needs cleanup");
        cache.prune(&ex).unwrap();
        assert!(!cache.needs_retry());
    }

    #[test]
    fn failed_checkout_preserves_a_warm_slot() {
        let dir = tempfile::tempdir().unwrap();
        let ex = executor(dir.path());
        let cache = Arc::new(Cache::new(dir.path(), "index".into(), 1));
        let lease = cache.reserve("demo", &ex).unwrap().unwrap();
        let path = lease.path.clone();
        fill(&lease, 1);
        lease.finish(&ex, true);

        let failed = cache.reserve("demo", &ex).unwrap().unwrap();
        assert_eq!(failed.path, path);
        let abandoned = cache.reserve("abandoned", &ex).unwrap().unwrap();
        fill(&abandoned, 1);
        let abandoned_path = abandoned.path.clone();
        drop(abandoned);
        failed.checkout_failed(&ex);
        assert!(
            Path::new(&abandoned_path).exists(),
            "idle bytes are within the cap"
        );
        assert_eq!(cache.reserve("demo", &ex).unwrap().unwrap().path, path);
        let cold = cache.reserve("other", &ex).unwrap().unwrap();
        let cold_record = cache.record_path(&cold.id);
        cold.checkout_failed(&ex);
        assert!(!cold_record.exists());
    }

    #[test]
    fn failed_checkout_prunes_newly_idle_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let ex = executor(dir.path());
        let mut cache = Cache::new(dir.path(), "index".into(), 1);
        cache.limit = 10;
        let cache = Arc::new(cache);
        let warm = cache.reserve("demo", &ex).unwrap().unwrap();
        fill(&warm, 6);
        let warm_path = warm.path.clone();
        warm.finish(&ex, true);
        let checkout = cache.reserve("demo", &ex).unwrap().unwrap();
        assert_eq!(checkout.path, warm_path);

        let parallel = cache.reserve("demo", &ex).unwrap().unwrap();
        fill(&parallel, 6);
        let parallel_path = parallel.path.clone();
        let parallel_record = cache.record_path(&parallel.id);
        parallel.finish(&ex, true);
        fs::File::open(parallel_record)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        assert!(Path::new(&parallel_path).exists(), "one slot is still live");

        checkout.checkout_failed(&ex);
        assert!(Path::new(&warm_path).exists());
        assert!(
            !Path::new(&parallel_path).exists(),
            "idle bytes exceeded the cap"
        );
    }

    #[test]
    fn startup_removes_unindexed_slots_even_with_caching_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let ex = executor(dir.path());
        let cache = Cache::new(dir.path(), format!("new.{}", ex.platform()), 0);
        let old = ex.cargo_cache_path(&format!(
            "cache-old-name.{}-20261127-090000-000001",
            ex.platform()
        ));
        let other = ex.cargo_cache_path("cache-unmanaged");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&other).unwrap();
        cache.clean(&ex).unwrap();
        assert!(!Path::new(&old).exists());
        assert!(Path::new(&other).exists());
    }
}
