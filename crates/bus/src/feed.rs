//! The owner channel's primitives (docs/owner.md §4): events on `HGL_OWNER`, a durable
//! feed of them for the console, reading a stream's recent history, and the
//! `HGL_THREADS` and `HGL_METRICS` buckets. The meaning of the events is the `owner`
//! crate's; this module moves bytes.

use std::time::Duration;

use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy, pull};
use async_nats::jetstream::{self, AckKind, context::Publish};
use futures::StreamExt;
use hgl_ai_proto::{
    CONSOLE_PREFIX, KV_CONSOLE, KV_METRICS, KV_THREADS, MetricsSample, OWNER_PREFIX, STREAM_OWNER,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{
    Bus, BusError, MAX_NOTE_BYTES, Result, SendReceipt, check_kv_key, list_json, live_entry,
    missing, nats, publish_error, put_json,
};

/// One message of a stream, as a feed or a history read returns it.
pub struct StreamMsg {
    pub seq: u64,
    pub subject: String,
    /// When the hub stored it.
    pub time: chrono::DateTime<chrono::Utc>,
    pub payload: Vec<u8>,
    handle: Option<jetstream::Message>,
}

impl StreamMsg {
    /// A message not bound to the hub, for tests.
    pub fn detached(seq: u64, subject: &str, payload: Vec<u8>) -> StreamMsg {
        StreamMsg {
            seq,
            subject: subject.to_string(),
            time: chrono::Utc::now(),
            payload,
            handle: None,
        }
    }

    pub async fn ack(&self) -> Result<()> {
        match &self.handle {
            Some(m) => m
                .double_ack()
                .await
                .map_err(|e| nats(&format!("acknowledging event {}", self.seq), e)),
            None => Ok(()),
        }
    }

    pub async fn nak(&self, delay: Duration) -> Result<()> {
        match &self.handle {
            Some(m) => m
                .ack_with(AckKind::Nak(Some(delay)))
                .await
                .map_err(|e| nats(&format!("returning event {}", self.seq), e)),
            None => Ok(()),
        }
    }
}

fn to_msg(m: jetstream::Message, keep: bool) -> Result<StreamMsg> {
    let info = m.info().map_err(|e| nats("reading message metadata", e))?;
    let seq = info.stream_sequence;
    let time = chrono::DateTime::from_timestamp(
        info.published.unix_timestamp(),
        info.published.nanosecond(),
    )
    .unwrap_or_else(chrono::Utc::now);
    Ok(StreamMsg {
        seq,
        subject: m.subject.to_string(),
        time,
        payload: m.payload.to_vec(),
        handle: keep.then_some(m),
    })
}

/// A durable consumer of `HGL_OWNER` (the console's): every event once, in order, one
/// in flight at a time.
pub struct Feed {
    consumer: jetstream::consumer::PullConsumer,
}

impl Feed {
    /// Wait up to `wait` for events; return at most `max`.
    pub async fn fetch(&mut self, max: usize, wait: Duration) -> Result<Vec<StreamMsg>> {
        let mut batch = self
            .consumer
            .fetch()
            .max_messages(max.max(1))
            .expires(wait.max(Duration::from_millis(10)))
            .messages()
            .await
            .map_err(|e| nats("reading the owner feed", e))?;
        let mut out = Vec::new();
        while let Some(m) = batch.next().await {
            match m {
                Ok(m) => out.push(to_msg(m, true)?),
                Err(e) if !out.is_empty() => {
                    tracing::warn!("owner feed read stopped early: {e}");
                    break;
                }
                Err(e) => return Err(nats("reading the owner feed", e)),
            }
        }
        Ok(out)
    }
}

impl Bus {
    /// Publish one owner-channel event to `subject` (under `hgl.owner.` or `hgl.console.`)
    /// with a message id, so a retry is dropped. The hub refuses a subject the
    /// connection's hub user may not publish (OWN-5).
    pub async fn publish_event(
        &self,
        subject: &str,
        payload: &[u8],
        msg_id: &str,
    ) -> Result<SendReceipt> {
        let ok = [OWNER_PREFIX, CONSOLE_PREFIX]
            .iter()
            .any(|p| subject.strip_prefix(p).is_some_and(|r| r.starts_with('.')));
        if !ok {
            return Err(BusError::Nats(format!(
                "{subject} is not an owner-channel subject"
            )));
        }
        if payload.len() > MAX_NOTE_BYTES {
            return Err(BusError::Nats(format!(
                "the event is {} bytes; the limit is {MAX_NOTE_BYTES}",
                payload.len()
            )));
        }
        let publish = Publish::build()
            .payload(payload.to_vec().into())
            .message_id(msg_id);
        let ack = self
            .guarded(subject, async {
                self.js
                    .send_publish(subject.to_string(), publish)
                    .await
                    .map_err(|e| publish_error(subject, e))?
                    .await
                    .map_err(|e| publish_error(subject, e))
            })
            .await?;
        Ok(SendReceipt {
            seq: ack.sequence,
            duplicate: ack.duplicate,
        })
    }

    /// The durable feed `name` over `HGL_OWNER` with these subject filters, created or
    /// updated on first use.
    pub async fn owner_feed(&self, name: &str, filters: Vec<String>) -> Result<Feed> {
        let stream = self
            .js
            .get_stream(STREAM_OWNER)
            .await
            .map_err(|e| missing(&format!("stream {STREAM_OWNER}"), e))?;
        let consumer = stream
            .create_consumer(pull::Config {
                durable_name: Some(name.to_string()),
                description: Some(format!("owner feed {name}")),
                deliver_policy: DeliverPolicy::All,
                ack_policy: AckPolicy::Explicit,
                ack_wait: Duration::from_secs(60),
                // One event at a time, so a thread's events fold in order: a later one is
                // never delivered while an earlier one waits for redelivery.
                max_ack_pending: 1,
                filter_subjects: filters,
                ..Default::default()
            })
            .await
            .map_err(|e| nats(&format!("opening the owner feed {name}"), e))?;
        Ok(Feed { consumer })
    }

    /// The messages of `stream` on `filter` stored since `since`, oldest first, at most
    /// `max` (the newest `max` when there are more). A short-lived consumer, acked by none.
    pub async fn history(
        &self,
        stream: &str,
        filter: &str,
        since: chrono::DateTime<chrono::Utc>,
        max: usize,
    ) -> Result<Vec<StreamMsg>> {
        let s = self
            .js
            .get_stream(stream)
            .await
            .map_err(|e| missing(&format!("stream {stream}"), e))?;
        let start = time::OffsetDateTime::from_unix_timestamp(since.timestamp())
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        let consumer = s
            .create_consumer(pull::Config {
                deliver_policy: DeliverPolicy::ByStartTime { start_time: start },
                ack_policy: AckPolicy::None,
                filter_subject: filter.to_string(),
                inactive_threshold: Duration::from_secs(30),
                memory_storage: true,
                ..Default::default()
            })
            .await
            .map_err(|e| nats(&format!("reading {stream}"), e))?;
        let mut out: std::collections::VecDeque<StreamMsg> = std::collections::VecDeque::new();
        loop {
            let mut batch = consumer
                .fetch()
                .max_messages(256)
                .expires(Duration::from_millis(500))
                .messages()
                .await
                .map_err(|e| nats(&format!("reading {stream}"), e))?;
            let mut got = 0;
            let mut pending = 0;
            while let Some(m) = batch.next().await {
                let m = m.map_err(|e| nats(&format!("reading {stream}"), e))?;
                pending = m.info().map(|i| i.pending).unwrap_or(0);
                got += 1;
                out.push_back(to_msg(m, false)?);
                if out.len() > max {
                    out.pop_front();
                }
            }
            if got == 0 || pending == 0 {
                break;
            }
        }
        Ok(out.into())
    }

    /// One thread record and its revision.
    pub async fn thread_get<T: DeserializeOwned>(&self, id: &str) -> Result<Option<(T, u64)>> {
        check_kv_key(id)?;
        let store = self.store(&self.threads, KV_THREADS).await?;
        match live_entry(store, id).await? {
            None => Ok(None),
            Some(e) => serde_json::from_slice(&e.value)
                .map(|t| Some((t, e.revision)))
                .map_err(|err| BusError::Nats(format!("thread {id} is unreadable: {err}"))),
        }
    }

    /// Write a thread record if it is still at `expected_revision` (0: only if absent).
    pub async fn thread_put<T: Serialize>(
        &self,
        id: &str,
        rec: &T,
        expected_revision: u64,
    ) -> Result<u64> {
        check_kv_key(id)?;
        let store = self.store(&self.threads, KV_THREADS).await?;
        let value = serde_json::to_vec(rec).map_err(|e| nats(&format!("encoding {id}"), e))?;
        let conflict = || BusError::Conflict {
            key: id.to_string(),
            expected: expected_revision,
        };
        let subject = format!("$KV.{KV_THREADS}.{id}");
        self.guarded(&subject, async {
            if expected_revision == 0 {
                store
                    .create(id, value.into())
                    .await
                    .map_err(|e| match e.kind() {
                        async_nats::jetstream::kv::CreateErrorKind::AlreadyExists => conflict(),
                        _ => nats(&format!("writing thread {id}"), e),
                    })
            } else {
                store
                    .update(id, value.into(), expected_revision)
                    .await
                    .map_err(|e| match e.kind() {
                        async_nats::jetstream::kv::UpdateErrorKind::WrongLastRevision => conflict(),
                        _ => nats(&format!("writing thread {id}"), e),
                    })
            }
        })
        .await
    }

    /// Every thread record, ordered by id.
    pub async fn threads<T: DeserializeOwned>(&self) -> Result<Vec<T>> {
        let store = self.store(&self.threads, KV_THREADS).await?;
        list_json(store).await
    }

    /// Write `HGL_METRICS/<repo>.<date>` (the coordinator's daily sample).
    pub async fn metrics_put(&self, s: &MetricsSample) -> Result<()> {
        let store = self.store(&self.metrics, KV_METRICS).await?;
        self.guarded(
            &format!("$KV.{KV_METRICS}.{}", s.key()),
            put_json(store, &s.key(), s),
        )
        .await
    }

    /// Write `HGL_CONSOLE/<key>` (the console's own state).
    pub async fn console_put<T: Serialize>(&self, key: &str, v: &T) -> Result<()> {
        let store = self.store(&self.console, KV_CONSOLE).await?;
        self.guarded(&format!("$KV.{KV_CONSOLE}.{key}"), put_json(store, key, v))
            .await
    }

    /// Delete `HGL_CONSOLE/<key>`; a missing key is fine.
    pub async fn console_del(&self, key: &str) -> Result<()> {
        check_kv_key(key)?;
        let store = self.store(&self.console, KV_CONSOLE).await?;
        store
            .purge(key)
            .await
            .map_err(|e| nats(&format!("deleting {key}"), e))
    }

    /// `HGL_CONSOLE/<key>`, if present.
    pub async fn console_get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        check_kv_key(key)?;
        let store = self.store(&self.console, KV_CONSOLE).await?;
        match live_entry(store, key).await? {
            None => Ok(None),
            Some(e) => serde_json::from_slice(&e.value)
                .map(Some)
                .map_err(|err| BusError::Nats(format!("{key} is unreadable: {err}"))),
        }
    }

    /// Every `HGL_CONSOLE` value whose key starts with `prefix`.
    pub async fn console_list<T: DeserializeOwned>(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, T)>> {
        let store = self.store(&self.console, KV_CONSOLE).await?;
        let mut keys = store.keys().await.map_err(|e| nats("listing keys", e))?;
        let mut names = Vec::new();
        while let Some(k) = keys.next().await {
            let k = k.map_err(|e| nats("listing keys", e))?;
            if k.starts_with(prefix) {
                names.push(k);
            }
        }
        names.sort();
        names.dedup();
        let mut out = Vec::new();
        for k in names {
            let Some(e) = live_entry(store, &k).await? else {
                continue;
            };
            match serde_json::from_slice(&e.value) {
                Ok(v) => out.push((k, v)),
                Err(err) => tracing::warn!(key = %k, "skipping an unreadable value: {err}"),
            }
        }
        Ok(out)
    }

    /// Every metrics sample, ordered by key (repository, then day).
    pub async fn metrics(&self) -> Result<Vec<MetricsSample>> {
        let store = self.store(&self.metrics, KV_METRICS).await?;
        list_json(store).await
    }
}
