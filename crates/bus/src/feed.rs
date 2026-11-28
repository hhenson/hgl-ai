//! The owner channel's primitives (docs/owner.md §4): events on `HGL_OWNER`, a durable
//! feed of them for the console, reading a stream's recent history, and the
//! `HGL_THREADS` and `HGL_METRICS` buckets. The meaning of the events is the `owner`
//! crate's; this module moves bytes.

use std::time::Duration;

use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy, pull};
use async_nats::jetstream::{
    self, AckKind,
    context::{Publish, PublishErrorKind},
    message::StreamMessage,
    stream::{DirectGetErrorKind, RawMessageErrorKind, Stream},
};
use futures::StreamExt;
use hgl_ai_proto::{
    CONSOLE_PREFIX, KV_CONSOLE, KV_METRICS, KV_SLACK, KV_THREADS, MetricsSample, OWNER_PREFIX,
    STREAM_OWNER,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{
    Bus, BusError, HUB_API_PREFIX, MAX_NOTE_BYTES, Result, SendReceipt, check_kv_key, keys,
    kv_read, list_json, live_entry, missing, nats, publish_error, put_json,
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

fn raw_msg(m: StreamMessage) -> StreamMsg {
    StreamMsg {
        seq: m.sequence,
        subject: m.subject.to_string(),
        time: chrono::DateTime::from_timestamp(m.time.unix_timestamp(), m.time.nanosecond())
            .unwrap_or(chrono::DateTime::UNIX_EPOCH),
        payload: m.payload.to_vec(),
        handle: None,
    }
}

fn subject_matches(filter: &str, subject: &str) -> bool {
    let mut tokens = subject.split('.');
    for part in filter.split('.') {
        match tokens.next() {
            Some(_) if part == ">" => return true,
            Some(t) if part == "*" || part == t => {}
            _ => return false,
        }
    }
    tokens.next().is_none()
}

/// A durable consumer of `HGL_OWNER` (the console's): every event once, in order, one
/// in flight at a time.
pub struct Feed {
    consumer: jetstream::consumer::PullConsumer,
}

impl Feed {
    /// Wait up to `wait` for the next event. The feed holds one event in flight, so this
    /// asks for one: a request for more would wait out `wait` for events that cannot come
    /// before this one is acknowledged.
    pub async fn next(&mut self, wait: Duration) -> Result<Option<StreamMsg>> {
        let mut batch = self
            .consumer
            .fetch()
            .max_messages(1)
            .expires(wait.max(Duration::from_millis(10)))
            .messages()
            .await
            .map_err(|e| nats("reading the owner feed", e))?;
        match batch.next().await {
            Some(Ok(m)) => Ok(Some(to_msg(m, true)?)),
            Some(Err(e)) => Err(nats("reading the owner feed", e)),
            None => Ok(None),
        }
    }
}

impl Bus {
    /// Read an immutable owner event by its stream sequence.
    pub async fn owner_event_at(&self, seq: u64) -> Result<StreamMsg> {
        let stream = self
            .js
            .get_stream_no_info(STREAM_OWNER)
            .await
            .map_err(|e| nats("opening the owner stream", e))?;
        self.owner_raw_at(&stream, seq)
            .await?
            .map(raw_msg)
            .ok_or_else(|| {
                nats(
                    "reading an owner event",
                    format!("no message at sequence {seq}"),
                )
            })
    }

    async fn owner_raw_at<I>(&self, stream: &Stream<I>, seq: u64) -> Result<Option<StreamMessage>> {
        let subject = self.api_subject(&format!("STREAM.MSG.GET.{STREAM_OWNER}"));
        self.guarded(&subject, async {
            match stream.get_raw_message(seq).await {
                Ok(m) => Ok(Some(m)),
                Err(e) if e.kind() == RawMessageErrorKind::NoMessageFound => Ok(None),
                Err(e) => Err(nats(&format!("reading owner event {seq}"), e)),
            }
        })
        .await
    }

    /// Publish one owner-channel event to `subject` (under `hgl.owner.` or `hgl.console.`)
    /// with a message id, so a retry is dropped. The hub refuses a subject the
    /// connection's hub user may not publish (OWN-5).
    pub async fn publish_event(
        &self,
        subject: &str,
        payload: &[u8],
        msg_id: &str,
    ) -> Result<SendReceipt> {
        self.publish_owner_event(subject, payload, Some(msg_id))
            .await
    }

    /// Publish an owner event once on a subject unique to its source reply or action.
    /// Returns false if the subject has already been used, even after the dedup window.
    pub async fn publish_slack_event(&self, subject: &str, payload: &[u8]) -> Result<bool> {
        match self.publish_owner_event(subject, payload, None).await {
            Ok(_) => Ok(true),
            Err(BusError::Conflict { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn publish_owner_event(
        &self,
        subject: &str,
        payload: &[u8],
        msg_id: Option<&str>,
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
        let publish = Publish::build().payload(payload.to_vec().into());
        let publish = match msg_id {
            Some(id) => publish.message_id(id),
            None => publish.expected_last_subject_sequence(0),
        };
        let ack = self
            .guarded(subject, async {
                self.js
                    .send_publish(subject.to_string(), publish)
                    .await
                    .map_err(|e| publish_error(subject, e))?
                    .await
                    .map_err(|e| {
                        if msg_id.is_none() && e.kind() == PublishErrorKind::WrongLastSequence {
                            return BusError::Conflict {
                                key: subject.into(),
                                expected: 0,
                            };
                        }
                        publish_error(subject, e)
                    })
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
    /// `max` (the newest `max` when there are more). Nothing is acknowledged.
    pub async fn history(
        &self,
        stream: &str,
        filter: &str,
        since: chrono::DateTime<chrono::Utc>,
        max: usize,
    ) -> Result<Vec<StreamMsg>> {
        self.history_filtered(stream, vec![filter.into()], since, max)
            .await
    }

    /// Read a stream's history across subject filters, in stream sequence order.
    pub async fn history_filtered(
        &self,
        stream: &str,
        filters: Vec<String>,
        since: chrono::DateTime<chrono::Utc>,
        max: usize,
    ) -> Result<Vec<StreamMsg>> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let s = self
            .js
            .get_stream(stream)
            .await
            .map_err(|e| missing(&format!("stream {stream}"), e))?;
        if stream == STREAM_OWNER {
            return self.owner_history(&s, &filters, since, max).await;
        }
        let start = time::OffsetDateTime::from_unix_timestamp(since.timestamp())
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        let mut config = pull::Config {
            deliver_policy: DeliverPolicy::ByStartTime { start_time: start },
            ack_policy: AckPolicy::None,
            inactive_threshold: Duration::from_secs(30),
            memory_storage: true,
            ..Default::default()
        };
        match filters.as_slice() {
            [filter] => config.filter_subject = filter.clone(),
            _ => config.filter_subjects = filters,
        }
        let consumer = s
            .create_consumer(config)
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

    async fn owner_history(
        &self,
        stream: &Stream,
        filters: &[String],
        since: chrono::DateTime<chrono::Utc>,
        max: usize,
    ) -> Result<Vec<StreamMsg>> {
        let state = &stream.cached_info().state;
        let mut out = Vec::new();
        if state.messages == 0 {
            return Ok(out);
        }
        // Walk the retained interval newest first, with a bounded number of requests
        // in flight. A deleted sequence is a hole, and new writes lie past the interval.
        let since = (since.timestamp(), since.timestamp_subsec_nanos());
        let mut messages =
            futures::stream::iter((state.first_sequence..=state.last_sequence).rev())
                .map(|seq| self.owner_raw_at(stream, seq))
                .buffered(16);
        while let Some(message) = messages.next().await {
            let Some(m) = message? else { continue };
            if (m.time.unix_timestamp(), m.time.nanosecond()) < since {
                break;
            }
            if filters.is_empty() || filters.iter().any(|f| subject_matches(f, &m.subject)) {
                out.push(raw_msg(m));
                if out.len() == max {
                    break;
                }
            }
        }
        out.reverse();
        Ok(out)
    }

    /// One thread record and its revision.
    pub async fn thread_get<T: DeserializeOwned>(&self, id: &str) -> Result<Option<(T, u64)>> {
        let store = self.store(&self.threads, KV_THREADS).await?;
        get_record(store, id).await
    }

    /// A Slack delivery record or reverse thread mapping, with its revision.
    pub async fn slack_get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<(T, u64)>> {
        if self.api_prefix == HUB_API_PREFIX && key.starts_with("links.") {
            return self.slack_link(key).await;
        }
        let store = self.store(&self.slack, KV_SLACK).await?;
        get_record(store, key).await
    }

    async fn slack_link<T: DeserializeOwned>(&self, key: &str) -> Result<Option<(T, u64)>> {
        check_kv_key(key)?;
        // The console can see link keys, but has no STREAM.INFO for the Slack bucket.
        let stream_name = format!("KV_{KV_SLACK}");
        let stream = self
            .js
            .get_stream_no_info(&stream_name)
            .await
            .map_err(|e| nats("opening the Slack link index", e))?;
        let subject = format!("$KV.{KV_SLACK}.{key}");
        let api = self.api_subject(&format!("DIRECT.GET.{stream_name}.{subject}"));
        self.guarded(&api, async {
            match stream.direct_get_last_for_subject(&subject).await {
                Ok(m) if kv_read::is_put(Some(&m.headers)) => {
                    decode_record(key, &m.payload, m.sequence).map(Some)
                }
                Ok(_) => Ok(None),
                Err(e) if e.kind() == DirectGetErrorKind::NotFound => Ok(None),
                Err(e) => Err(nats(&format!("reading {key}"), e)),
            }
        })
        .await
    }

    /// Change a Slack delivery record or create a reverse thread mapping.
    pub async fn slack_put<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        revision: u64,
    ) -> Result<u64> {
        let store = self.store(&self.slack, KV_SLACK).await?;
        self.put_record(store, KV_SLACK, key, value, revision).await
    }

    /// Write a thread record if it is still at `expected_revision` (0: only if absent).
    pub async fn thread_put<T: Serialize>(
        &self,
        id: &str,
        rec: &T,
        expected_revision: u64,
    ) -> Result<u64> {
        let store = self.store(&self.threads, KV_THREADS).await?;
        self.put_record(store, KV_THREADS, id, rec, expected_revision)
            .await
    }

    async fn put_record<T: Serialize>(
        &self,
        store: &jetstream::kv::Store,
        bucket: &str,
        id: &str,
        rec: &T,
        expected_revision: u64,
    ) -> Result<u64> {
        check_kv_key(id)?;
        let value = serde_json::to_vec(rec).map_err(|e| nats(&format!("encoding {id}"), e))?;
        let conflict = || BusError::Conflict {
            key: id.to_string(),
            expected: expected_revision,
        };
        let subject = self.kv_subject(bucket, id);
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
        list_json(self, store).await
    }

    /// Write `HGL_METRICS/<repo>.<date>` (the coordinator's daily sample).
    pub async fn metrics_put(&self, s: &MetricsSample) -> Result<()> {
        let store = self.store(&self.metrics, KV_METRICS).await?;
        self.guarded(
            &self.kv_subject(KV_METRICS, &s.key()),
            put_json(store, &s.key(), s),
        )
        .await
    }

    /// Write `HGL_CONSOLE/<key>` (the console's own state).
    pub async fn console_put<T: Serialize>(&self, key: &str, v: &T) -> Result<()> {
        let store = self.store(&self.console, KV_CONSOLE).await?;
        self.guarded(&self.kv_subject(KV_CONSOLE, key), put_json(store, key, v))
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
        let names = keys(self, store, |key| key.starts_with(prefix)).await?;
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
        list_json(self, store).await
    }
}

async fn get_record<T: DeserializeOwned>(
    store: &jetstream::kv::Store,
    key: &str,
) -> Result<Option<(T, u64)>> {
    check_kv_key(key)?;
    match live_entry(store, key).await? {
        None => Ok(None),
        Some(entry) => decode_record(key, &entry.value, entry.revision).map(Some),
    }
}

fn decode_record<T: DeserializeOwned>(key: &str, value: &[u8], revision: u64) -> Result<(T, u64)> {
    serde_json::from_slice(value)
        .map(|value| (value, revision))
        .map_err(|error| BusError::Nats(format!("{key} is unreadable: {error}")))
}
