//! `HGL_BUILD` (recipes and run summaries), builder records in `HGL_HOSTS`, and the
//! replies of build jobs (docs/build.md, docs/protocol.md §3).

use std::time::{Duration, Instant};

use async_nats::jetstream::consumer::DeliverPolicy;
use async_nats::jetstream::stream::RawMessageErrorKind;
use async_nats::jetstream::{kv, stream};
use hgl_ai_proto::build::{BUILDER_PREFIX, BuilderRecord, KV_BUILD, builder_key};
use hgl_ai_proto::{KV_HOSTS, Note, STREAM_NOTES, Seat};

use super::{
    Bus, BusError, Result, check_kv_key, keys, kv_read, list_json_where, live_entry, missing, nats,
};

/// Revisions kept per `HGL_BUILD` key: a recipe's history (`recipe show --rev N`).
pub const BUILD_HISTORY: i64 = 64;

/// Provisioning entry of `HGL_BUILD`: file storage, history [`BUILD_HISTORY`], no age
/// limit (run summaries are trimmed by count by their writers).
pub(crate) const BUCKET: (&str, &str, stream::StorageType, i64, Duration) = (
    KV_BUILD,
    "hgl-ai build recipes and run summaries",
    stream::StorageType::File,
    BUILD_HISTORY,
    Duration::ZERO,
);

pub(crate) fn is_builder_key(k: &str) -> bool {
    k.starts_with(BUILDER_PREFIX)
}

/// One revision of an `HGL_BUILD` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildEntry {
    pub value: Vec<u8>,
    pub revision: u64,
    /// Unix seconds of the write.
    pub created: i64,
}

const POLL: Duration = Duration::from_millis(500);

impl Bus {
    async fn build_store(&self) -> Result<&kv::Store> {
        self.store(&self.build, KV_BUILD).await
    }

    /// The latest value of `key` in `HGL_BUILD`, unless deleted.
    pub async fn build_get(&self, key: &str) -> Result<Option<BuildEntry>> {
        check_kv_key(key)?;
        let store = self.build_store().await?;
        Ok(live_entry(store, key).await?.map(entry))
    }

    /// The value of `key` at `revision`, if that revision is still kept and is a write.
    pub async fn build_get_revision(&self, key: &str, revision: u64) -> Result<Option<BuildEntry>> {
        check_kv_key(key)?;
        let store = self.build_store().await?;
        let e = store
            .entry_for_revision(key, revision)
            .await
            .map_err(|e| nats(&format!("reading {key} at revision {revision}"), e))?;
        Ok(
            e.filter(|e| e.operation == kv::Operation::Put && e.key == key)
                .map(entry),
        )
    }

    /// Every kept revision of `key`, oldest first (deletes left out).
    pub async fn build_history(&self, key: &str) -> Result<Vec<BuildEntry>> {
        check_kv_key(key)?;
        let store = self.build_store().await?;
        let Some(latest) = live_entry(store, key).await? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        let read = kv_read::read(self, store, key, DeliverPolicy::All, false, |m| {
            if kv_read::is_put(m.headers.as_ref()) {
                let info = m
                    .info()
                    .map_err(|e| nats("reading build revision metadata", e))?;
                out.push(BuildEntry {
                    value: m.payload.to_vec(),
                    revision: info.stream_sequence,
                    created: info.published.unix_timestamp(),
                });
            }
            Ok(())
        })
        .await;
        match read {
            Err(e) if self.legacy_kv_pull_denied(&e) => {
                return legacy_history(store, key, latest.revision).await;
            }
            other => other?,
        }
        Ok(out)
    }

    /// Write `key`; returns the new revision.
    pub async fn build_put(&self, key: &str, value: &[u8]) -> Result<u64> {
        check_kv_key(key)?;
        super::check_value_size(key, value.len())?;
        let store = self.build_store().await?;
        self.guarded(&self.kv_subject(KV_BUILD, key), async {
            store
                .put(key, value.to_vec().into())
                .await
                .map_err(|e| nats(&format!("writing {key}"), e))
        })
        .await
    }

    /// Delete `key` and its history. A missing key is fine.
    pub async fn build_del(&self, key: &str) -> Result<()> {
        check_kv_key(key)?;
        let store = self.build_store().await?;
        self.guarded(&self.kv_subject(KV_BUILD, key), async {
            store
                .purge(key)
                .await
                .map_err(|e| nats(&format!("deleting {key}"), e))
        })
        .await
    }

    /// The live keys of `HGL_BUILD` that start with `prefix`, sorted.
    pub async fn build_keys(&self, prefix: &str) -> Result<Vec<String>> {
        let store = self.build_store().await?;
        keys(self, store, |key| key.starts_with(prefix)).await
    }

    /// Write `HGL_HOSTS/builder.<label>`; it expires like a host record.
    pub async fn builder_put(&self, rec: &BuilderRecord) -> Result<()> {
        let store = self.store(&self.hosts, KV_HOSTS).await?;
        super::put_json(store, &builder_key(&rec.label), rec).await
    }

    /// Remove a builder record (a builder stopping or retiring).
    pub async fn builder_del(&self, label: &str) -> Result<()> {
        let key = builder_key(label);
        check_kv_key(&key)?;
        let store = self.store(&self.hosts, KV_HOSTS).await?;
        store
            .purge(&key)
            .await
            .map_err(|e| nats(&format!("deleting {key}"), e))
    }

    /// Every live builder record, ordered by label.
    pub async fn builders(&self) -> Result<Vec<BuilderRecord>> {
        let store = self.store(&self.hosts, KV_HOSTS).await?;
        list_json_where(self, store, is_builder_key).await
    }

    /// The last note sent to `address` (an address no inbox serves, such as a build's
    /// reply address), waiting up to `wait` for one. A note that does not parse is an error.
    pub async fn reply_wait(&self, address: &Seat, wait: Duration) -> Result<Option<Note>> {
        Ok(self
            .reply_wait_after(address, 0, wait)
            .await?
            .map(|(_, n)| n))
    }

    /// [`Bus::reply_wait`] for a note after stream sequence `after` (one already read and
    /// passed over), with its sequence; polling at the same pace either way.
    pub async fn reply_wait_after(
        &self,
        address: &Seat,
        after: u64,
        wait: Duration,
    ) -> Result<Option<(u64, Note)>> {
        let stream = self
            .js
            .get_stream(STREAM_NOTES)
            .await
            .map_err(|e| missing(&format!("stream {STREAM_NOTES}"), e))?;
        let subject = address.subject();
        let deadline = Instant::now().checked_add(wait);
        loop {
            match stream.get_last_raw_message_by_subject(&subject).await {
                Ok(m) if m.sequence > after => {
                    let note = super::parse_note(m.subject.as_str(), &m.payload)
                        .map_err(|r| BusError::Nats(format!("the reply to {address}: {r}")))?;
                    return Ok(Some((m.sequence, note)));
                }
                // Already read: wait for a newer one.
                Ok(_) => {}
                Err(e)
                    if e.kind()
                        == async_nats::jetstream::stream::LastRawMessageErrorKind::NoMessageFound => {}
                Err(e) => return Err(nats(&format!("reading {subject}"), e)),
            }
            let left = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => POLL,
            };
            if left.is_zero() {
                return Ok(None);
            }
            tokio::time::sleep(left.min(POLL)).await;
        }
    }
}

/// Older single-account grants allow reads by sequence but no KV pulls.
async fn legacy_history(store: &kv::Store, key: &str, latest: u64) -> Result<Vec<BuildEntry>> {
    let subject = format!("{}{key}", store.prefix);
    let mut out = Vec::new();
    let mut next = 1;
    while next <= latest {
        let m = match store
            .stream
            .get_first_raw_message_by_subject(&subject, next)
            .await
        {
            Ok(m) if m.sequence <= latest => m,
            Ok(_) => break,
            Err(e) if e.kind() == RawMessageErrorKind::NoMessageFound => break,
            Err(e) => return Err(nats(&format!("reading the history of {key}"), e)),
        };
        if kv_read::is_put(Some(&m.headers)) {
            out.push(BuildEntry {
                value: m.payload.to_vec(),
                revision: m.sequence,
                created: m.time.unix_timestamp(),
            });
        }
        if m.sequence == latest {
            break;
        }
        next = m.sequence + 1;
    }
    Ok(out)
}

fn entry(e: kv::Entry) -> BuildEntry {
    BuildEntry {
        value: e.value.to_vec(),
        revision: e.revision,
        created: e.created.unix_timestamp(),
    }
}
