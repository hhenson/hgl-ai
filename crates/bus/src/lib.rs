//! NATS JetStream access (docs/protocol.md §3, §8, §9).
//!
//! The public API below is the contract other crates build against:
//!
//! - [`Bus::connect`] / [`Bus::connect_url`] open the hub connection. Every
//!   connection failure, a refused token included, is [`BusError::Unreachable`]
//!   (the command line maps it to exit status 10).
//! - [`Bus::provision`] creates or updates the stream and buckets of §3.
//! - [`Bus::send`] validates and publishes a [`Note`] with its `Nats-Msg-Id`.
//! - [`Bus::inbox`] opens a seat's durable consumer; [`Inbox::fetch`] returns
//!   [`Delivery`] values that the receiver must [`ack`](Delivery::ack),
//!   [`nak`](Delivery::nak) or keep alive with
//!   [`in_progress`](Delivery::in_progress) (PROTO-8).
//! - `state_*`, `host*` and `tree*` read and write the three KV buckets.
//!   Writers of shared keys (`prs`, `main`, `landed.log`) use the
//!   compare-and-set calls [`Bus::state_create`], [`Bus::state_update`] and
//!   [`Bus::append_line`]; [`Bus::state_put`] is the explicit blind write.
//! - Every read is bounded: a note payload over [`MAX_NOTE_BYTES`] is refused
//!   on send and terminated on receipt; buckets refuse values over
//!   [`MAX_VALUE_BYTES`].
//!
//! Nothing here starts a hub; the operations other than `connect*` and
//! `provision` fail with a hint to run `hgl-ai init` when the stream or a
//! bucket is missing.

use std::path::Path;
use std::time::{Duration, Instant};

use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy, pull};
use async_nats::jetstream::{self, AckKind, context::Publish, kv, stream};
use futures::StreamExt;
use hgl_ai_config::Config;
use hgl_ai_proto::{
    BROADCAST, HostRecord, KV_HOSTS, KV_STATE, KV_TREES, Note, STREAM_NOTES, SUBJECT_PREFIX, Seat,
    TreeRecord,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::OnceCell;

#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("hub unreachable at {url}: {reason}")]
    Unreachable { url: String, reason: String },
    #[error("reading the hub token {path}: {source}")]
    Token {
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    Proto(#[from] hgl_ai_proto::ProtoError),
    #[error(transparent)]
    Config(#[from] hgl_ai_config::ConfigError),
    /// A compare-and-set lost: the key is not at the expected revision
    /// (`expected == 0`: the key already exists).
    #[error("{}", conflict_text(key, *expected))]
    Conflict { key: String, expected: u64 },
    #[error("{0}")]
    Nats(String),
}

fn conflict_text(key: &str, expected: u64) -> String {
    if expected == 0 {
        format!("state {key} already exists")
    } else {
        format!("state {key} is no longer at revision {expected}")
    }
}

pub type Result<T> = std::result::Result<T, BusError>;

/// `HGL_NOTES` keeps notes 14 days (PROTO-2).
pub const NOTES_MAX_AGE: Duration = Duration::from_secs(14 * 86_400);
/// `Nats-Msg-Id` deduplication window (PROTO-9).
pub const NOTES_DUPLICATE_WINDOW: Duration = Duration::from_secs(86_400);
/// Revisions kept per `HGL_STATE` key.
pub const STATE_HISTORY: i64 = 5;
/// A host record expires this long after its last write (§9).
pub const HOSTS_TTL: Duration = Duration::from_secs(120);
/// Revisions kept per `HGL_TREES` key.
pub const TREES_HISTORY: i64 = 1;
/// Inbox `ack_wait` (§3); the bridge extends it with `in_progress` (PROTO-8).
pub const INBOX_ACK_WAIT: Duration = Duration::from_secs(300);
/// Inbox `max_deliver` (§3).
pub const INBOX_MAX_DELIVER: i64 = 5;
/// Largest note payload (the encoded JSON) that is sent or accepted.
pub const MAX_NOTE_BYTES: usize = 1 << 20;
/// Largest value a bucket holds.
pub const MAX_VALUE_BYTES: usize = 1 << 20;
/// Attempts [`Bus::append_line`] makes before giving up with [`BusError::Conflict`].
const APPEND_ATTEMPTS: u32 = 200;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest single pull request; longer waits are split so a reconnect is noticed.
const MAX_PULL: Duration = Duration::from_secs(30);
/// Shortest pull expiry worth sending to the server.
const MIN_PULL: Duration = Duration::from_millis(10);
/// JetStream API error code for a missing stream.
const STREAM_NOT_FOUND: &str = "error code 10059";

/// What a send returned: the stream sequence (the bus receipt id) and whether
/// JetStream dropped it as a duplicate (PROTO-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendReceipt {
    pub seq: u64,
    pub duplicate: bool,
}

/// One `HGL_STATE` value with its revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateEntry {
    pub value: String,
    pub revision: u64,
}

/// Inbox consumer settings. [`Default`] is the protocol's (§3); other values
/// are for tests that need a short `ack_wait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboxOptions {
    pub ack_wait: Duration,
    pub max_deliver: i64,
}

impl Default for InboxOptions {
    fn default() -> Self {
        InboxOptions {
            ack_wait: INBOX_ACK_WAIT,
            max_deliver: INBOX_MAX_DELIVER,
        }
    }
}

/// Read a hub token file: the contents with surrounding whitespace trimmed,
/// or `None` when the file is empty.
pub fn read_token(path: &Path) -> Result<Option<String>> {
    let text = std::fs::read_to_string(path).map_err(|source| BusError::Token {
        path: path.display().to_string(),
        source,
    })?;
    let token = text.trim();
    Ok((!token.is_empty()).then(|| token.to_string()))
}

/// A connection to the hub.
pub struct Bus {
    client: async_nats::Client,
    js: jetstream::Context,
    url: String,
    state: OnceCell<kv::Store>,
    hosts: OnceCell<kv::Store>,
    trees: OnceCell<kv::Store>,
}

impl Bus {
    /// Connect with the host configuration (`hub.url`, `hub.token_file`).
    pub async fn connect(cfg: &Config) -> Result<Bus> {
        let token = read_token(&cfg.token_file()?)?;
        Bus::open(&cfg.hub.url, token, format!("hgl-ai {}", cfg.host)).await
    }

    /// Connect to `url`, with an optional token.
    pub async fn connect_url(url: &str, token: Option<String>) -> Result<Bus> {
        Bus::open(url, token, "hgl-ai".to_string()).await
    }

    async fn open(url: &str, token: Option<String>, name: String) -> Result<Bus> {
        let mut opts = async_nats::ConnectOptions::new()
            .name(name)
            .connection_timeout(CONNECT_TIMEOUT);
        if let Some(t) = token {
            opts = opts.token(t);
        }
        let client = opts.connect(url).await.map_err(|e| BusError::Unreachable {
            url: url.to_string(),
            reason: e.to_string(),
        })?;
        let js = jetstream::new(client.clone());
        Ok(Bus {
            client,
            js,
            url: url.to_string(),
            state: OnceCell::new(),
            hosts: OnceCell::new(),
            trees: OnceCell::new(),
        })
    }

    /// The hub URL this connection was opened with.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Wait until everything published on this connection reached the hub.
    pub async fn flush(&self) -> Result<()> {
        self.client
            .flush()
            .await
            .map_err(|e| nats("flushing the connection", e))
    }

    /// Create the stream and buckets, or update them to match §3 (idempotent).
    pub async fn provision(&self) -> Result<()> {
        self.js
            .create_or_update_stream(stream::Config {
                name: STREAM_NOTES.to_string(),
                description: Some("hgl-ai notes".to_string()),
                subjects: vec![format!("{SUBJECT_PREFIX}.>")],
                retention: stream::RetentionPolicy::Limits,
                discard: stream::DiscardPolicy::Old,
                storage: stream::StorageType::File,
                max_age: NOTES_MAX_AGE,
                duplicate_window: NOTES_DUPLICATE_WINDOW,
                // Room for the headers on top of the largest payload.
                max_message_size: (MAX_NOTE_BYTES + (64 << 10)) as i32,
                ..Default::default()
            })
            .await
            .map_err(|e| nats(&format!("provisioning stream {STREAM_NOTES}"), e))?;
        let buckets = [
            (
                KV_STATE,
                "hgl-ai shared state",
                stream::StorageType::File,
                STATE_HISTORY,
                Duration::ZERO,
            ),
            (
                KV_HOSTS,
                "hgl-ai live hosts",
                stream::StorageType::Memory,
                1,
                HOSTS_TTL,
            ),
            (
                KV_TREES,
                "hgl-ai task worktrees",
                stream::StorageType::File,
                TREES_HISTORY,
                Duration::ZERO,
            ),
        ];
        for (bucket, description, storage, history, max_age) in buckets {
            self.js
                .create_or_update_key_value(kv::Config {
                    bucket: bucket.to_string(),
                    description: description.to_string(),
                    storage,
                    history,
                    max_age,
                    max_value_size: MAX_VALUE_BYTES as i32,
                    ..Default::default()
                })
                .await
                .map_err(|e| nats(&format!("provisioning bucket {bucket}"), e))?;
        }
        Ok(())
    }

    /// Validate and publish a note to `hgl.notes.<recipient>` with its message id,
    /// and wait for the hub's acknowledgement.
    pub async fn send(&self, note: &Note) -> Result<SendReceipt> {
        note.validate()?;
        let subject = note.recipient.subject();
        let payload = serde_json::to_vec(note).map_err(|e| nats("encoding the note", e))?;
        if payload.len() > MAX_NOTE_BYTES {
            return Err(BusError::Nats(format!(
                "the note is {} bytes encoded; the limit is {MAX_NOTE_BYTES}",
                payload.len()
            )));
        }
        let publish = Publish::build()
            .payload(payload.into())
            .message_id(note.msg_id());
        let ack = self
            .js
            .send_publish(subject.clone(), publish)
            .await
            .map_err(|e| publish_error(&subject, e))?
            .await
            .map_err(|e| publish_error(&subject, e))?;
        Ok(SendReceipt {
            seq: ack.sequence,
            duplicate: ack.duplicate,
        })
    }

    /// The durable inbox of `seat`, created on first use.
    pub async fn inbox(&self, seat: &Seat) -> Result<Inbox> {
        self.inbox_with(seat, InboxOptions::default()).await
    }

    /// The durable inbox of `seat` with explicit consumer settings. The consumer
    /// is created, or updated to these settings if it exists.
    pub async fn inbox_with(&self, seat: &Seat, opts: InboxOptions) -> Result<Inbox> {
        if seat.is_broadcast() {
            return Err(BusError::Nats(format!(
                "`{BROADCAST}` is the broadcast address; it has no inbox"
            )));
        }
        let stream = self
            .js
            .get_stream(STREAM_NOTES)
            .await
            .map_err(|e| missing(&format!("stream {STREAM_NOTES}"), e))?;
        let consumer = stream
            .create_consumer(pull::Config {
                durable_name: Some(seat.consumer()),
                description: Some(format!("inbox of seat {seat}")),
                deliver_policy: DeliverPolicy::All,
                ack_policy: AckPolicy::Explicit,
                ack_wait: opts.ack_wait,
                max_deliver: opts.max_deliver,
                filter_subjects: vec![seat.subject(), Seat::broadcast().subject()],
                ..Default::default()
            })
            .await
            .map_err(|e| nats(&format!("opening the inbox of {seat}"), e))?;
        Ok(Inbox {
            seat: seat.clone(),
            consumer,
            client: self.client.clone(),
        })
    }

    pub async fn state_get(&self, key: &str) -> Result<Option<StateEntry>> {
        check_state_key(key)?;
        let store = self.store(&self.state, KV_STATE).await?;
        match live_entry(store, key).await? {
            Some(e) => {
                check_value_size(key, e.value.len())?;
                let value = String::from_utf8(e.value.to_vec())
                    .map_err(|_| BusError::Nats(format!("state {key} is not UTF-8")))?;
                Ok(Some(StateEntry {
                    value,
                    revision: e.revision,
                }))
            }
            None => Ok(None),
        }
    }

    /// Put `value` whatever the key holds; an empty value deletes the key (§8).
    /// Returns the new revision (0 on delete). A blind write: writers of shared
    /// keys use [`Bus::state_create`], [`Bus::state_update`] or [`Bus::append_line`].
    pub async fn state_put(&self, key: &str, value: &str) -> Result<u64> {
        if value.is_empty() {
            self.state_del(key).await?;
            return Ok(0);
        }
        check_state_key(key)?;
        check_value_size(key, value.len())?;
        let store = self.store(&self.state, KV_STATE).await?;
        store
            .put(key, value.as_bytes().to_vec().into())
            .await
            .map_err(|e| nats(&format!("writing state {key}"), e))
    }

    /// Create `key` only if it is absent (a deleted key counts as absent).
    /// Fails with [`BusError::Conflict`] (`expected: 0`) when it exists.
    pub async fn state_create(&self, key: &str, value: &str) -> Result<u64> {
        check_state_key(key)?;
        if value.is_empty() {
            return Err(BusError::Nats(format!(
                "state {key}: an empty value deletes; nothing to create"
            )));
        }
        check_value_size(key, value.len())?;
        let store = self.store(&self.state, KV_STATE).await?;
        store
            .create(key, value.as_bytes().to_vec().into())
            .await
            .map_err(|e| match e.kind() {
                kv::CreateErrorKind::AlreadyExists => BusError::Conflict {
                    key: key.to_string(),
                    expected: 0,
                },
                _ => nats(&format!("creating state {key}"), e),
            })
    }

    /// Replace `key` only if it is still at `expected_revision` (compare-and-set);
    /// `expected_revision == 0` means "only if absent" ([`Bus::state_create`]).
    /// An empty value deletes, under the same condition, and returns 0.
    /// Fails with [`BusError::Conflict`] when another writer got there first.
    pub async fn state_update(
        &self,
        key: &str,
        value: &str,
        expected_revision: u64,
    ) -> Result<u64> {
        if expected_revision == 0 {
            return self.state_create(key, value).await;
        }
        check_state_key(key)?;
        check_value_size(key, value.len())?;
        let store = self.store(&self.state, KV_STATE).await?;
        let conflict = |e: kv::UpdateError| match e.kind() {
            kv::UpdateErrorKind::WrongLastRevision => BusError::Conflict {
                key: key.to_string(),
                expected: expected_revision,
            },
            _ => nats(&format!("writing state {key}"), e),
        };
        if value.is_empty() {
            store
                .purge_expect_revision(key, Some(expected_revision))
                .await
                .map_err(conflict)?;
            return Ok(0);
        }
        store
            .update(key, value.as_bytes().to_vec().into(), expected_revision)
            .await
            .map_err(conflict)
    }

    /// Append `line` and a newline to `key` (creating it if absent), retrying
    /// the compare-and-set until it wins, as `landed.log` needs. A trailing
    /// newline of `line` is dropped; the old value gets one if it lacks it.
    /// Returns the new revision.
    pub async fn append_line(&self, key: &str, line: &str) -> Result<u64> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        for attempt in 0..APPEND_ATTEMPTS {
            let res = match self.state_get(key).await? {
                None => self.state_create(key, &format!("{line}\n")).await,
                Some(e) => {
                    let mut value = e.value;
                    if !value.is_empty() && !value.ends_with('\n') {
                        value.push('\n');
                    }
                    value.push_str(line);
                    value.push('\n');
                    self.state_update(key, &value, e.revision).await
                }
            };
            match res {
                Err(BusError::Conflict { .. }) => tokio::time::sleep(backoff(attempt)).await,
                other => return other,
            }
        }
        Err(BusError::Conflict {
            key: key.to_string(),
            expected: 0,
        })
    }

    /// Delete a key and its history. Deleting a missing key succeeds.
    pub async fn state_del(&self, key: &str) -> Result<()> {
        check_state_key(key)?;
        let store = self.store(&self.state, KV_STATE).await?;
        store
            .purge(key)
            .await
            .map_err(|e| nats(&format!("deleting state {key}"), e))
    }

    /// Write `HGL_HOSTS/<host>`; it expires [`HOSTS_TTL`] after the last write.
    pub async fn host_put(&self, rec: &HostRecord) -> Result<()> {
        let store = self.store(&self.hosts, KV_HOSTS).await?;
        put_json(store, &rec.host, rec).await
    }

    /// Remove a host record before it expires (a bridge shutting down).
    pub async fn host_del(&self, host: &str) -> Result<()> {
        check_kv_key(host)?;
        let store = self.store(&self.hosts, KV_HOSTS).await?;
        store
            .purge(host)
            .await
            .map_err(|e| nats(&format!("deleting host {host}"), e))
    }

    /// Every live host record, ordered by host label.
    pub async fn hosts(&self) -> Result<Vec<HostRecord>> {
        let store = self.store(&self.hosts, KV_HOSTS).await?;
        list_json(store).await
    }

    /// The live host whose bridge serves `seat`, if any. A seat is served by a
    /// bridge or interactively, never both (PROTO-6).
    pub async fn bridge_host(&self, seat: &Seat) -> Result<Option<String>> {
        Ok(self
            .hosts()
            .await?
            .into_iter()
            .find(|h| h.seats.iter().any(|s| &s.seat == seat))
            .map(|h| h.host))
    }

    /// Write `HGL_TREES/<TreeRecord::key()>` (PROTO-13).
    pub async fn tree_put(&self, rec: &TreeRecord) -> Result<()> {
        let store = self.store(&self.trees, KV_TREES).await?;
        put_json(store, &rec.key(), rec).await
    }

    /// Deregister a tree by its key ([`TreeRecord::key`]). Deleting a missing key succeeds.
    pub async fn tree_del(&self, key: &str) -> Result<()> {
        check_kv_key(key)?;
        let store = self.store(&self.trees, KV_TREES).await?;
        store
            .purge(key)
            .await
            .map_err(|e| nats(&format!("deleting tree {key}"), e))
    }

    /// Every registered tree, ordered by key.
    pub async fn trees(&self) -> Result<Vec<TreeRecord>> {
        let store = self.store(&self.trees, KV_TREES).await?;
        list_json(store).await
    }

    async fn store<'a>(
        &self,
        cell: &'a OnceCell<kv::Store>,
        bucket: &str,
    ) -> Result<&'a kv::Store> {
        cell.get_or_try_init(|| async {
            self.js
                .get_key_value(bucket)
                .await
                .map_err(|e| missing(&format!("bucket {bucket}"), e))
        })
        .await
    }
}

/// A seat's durable inbox consumer (`inbox-<seat>`).
pub struct Inbox {
    seat: Seat,
    consumer: jetstream::consumer::PullConsumer,
    client: async_nats::Client,
}

impl Inbox {
    pub fn seat(&self) -> &Seat {
        &self.seat
    }

    /// Wait up to `wait` for at least one note; return at most `max`.
    /// A zero `wait` returns what is pending without waiting; `Duration::MAX`
    /// waits without end. A malformed message is terminated and logged, never
    /// returned.
    pub async fn fetch(&mut self, max: usize, wait: Duration) -> Result<Vec<Delivery>> {
        let mut out = Vec::new();
        if max == 0 {
            return Ok(out);
        }
        // `None`: no deadline.
        let deadline = Instant::now().checked_add(wait);
        loop {
            let remaining = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => MAX_PULL,
            };
            if remaining < MIN_PULL {
                break;
            }
            // Hold one pull request open until a note arrives; a larger batch would
            // keep waiting for more after the first.
            let batch = self
                .consumer
                .batch()
                .max_messages(1)
                .expires(remaining.min(MAX_PULL))
                .messages()
                .await
                .map_err(|e| nats(&format!("waiting on the inbox of {}", self.seat), e))?;
            self.take(batch, &mut out).await?;
            if !out.is_empty() {
                break;
            }
        }
        // Then whatever else is pending, without waiting.
        if out.len() < max {
            self.take_pending(max - out.len(), &mut out).await?;
        }
        Ok(out)
    }

    async fn take_pending(&mut self, max: usize, out: &mut Vec<Delivery>) -> Result<()> {
        let batch = self
            .consumer
            .fetch()
            .max_messages(max)
            .messages()
            .await
            .map_err(|e| nats(&format!("reading the inbox of {}", self.seat), e))?;
        self.take(batch, out).await
    }

    async fn take(&mut self, mut batch: pull::Batch, out: &mut Vec<Delivery>) -> Result<()> {
        while let Some(m) = batch.next().await {
            let msg = match m {
                Ok(msg) => msg,
                // Keep what already arrived; the caller sees an error only when nothing did.
                Err(e) if !out.is_empty() => {
                    tracing::warn!(seat = %self.seat, "inbox read stopped early: {e}");
                    break;
                }
                Err(e) => return Err(nats(&format!("reading the inbox of {}", self.seat), e)),
            };
            if let Some(d) = self.accept(msg).await? {
                out.push(d);
            }
        }
        Ok(())
    }

    /// Turn one message into a delivery, or terminate it when it is not a valid note.
    async fn accept(&self, msg: jetstream::Message) -> Result<Option<Delivery>> {
        let (seq, delivered) = match msg.info() {
            Ok(i) => (i.stream_sequence, u64::try_from(i.delivered).unwrap_or(0)),
            Err(e) => return Err(nats("reading message metadata", e)),
        };
        match parse_note(msg.subject.as_str(), &msg.payload) {
            Ok(note) => Ok(Some(Delivery {
                seq,
                delivered,
                note,
                handle: Some(Handle {
                    msg,
                    client: self.client.clone(),
                }),
            })),
            Err(reason) => {
                tracing::warn!(seat = %self.seat, seq, subject = %msg.subject, "terminating a malformed note: {reason}");
                msg.ack_with(AckKind::Term)
                    .await
                    .map_err(|e| nats("terminating a malformed note", e))?;
                self.client
                    .flush()
                    .await
                    .map_err(|e| nats("terminating a malformed note", e))?;
                Ok(None)
            }
        }
    }
}

/// Parse and check one inbox payload; the error says why it is rejected.
fn parse_note(subject: &str, payload: &[u8]) -> std::result::Result<Note, String> {
    if payload.len() > MAX_NOTE_BYTES {
        return Err(format!(
            "{} bytes, over the {MAX_NOTE_BYTES} byte limit",
            payload.len()
        ));
    }
    let note: Note = serde_json::from_slice(payload).map_err(|e| format!("not a note: {e}"))?;
    note.validate().map_err(|e| e.to_string())?;
    if subject != note.recipient.subject() {
        return Err(format!(
            "recipient {} does not match subject {subject}",
            note.recipient
        ));
    }
    Ok(note)
}

struct Handle {
    msg: jetstream::Message,
    client: async_nats::Client,
}

/// One delivered note. The receiver must `ack`, `nak` or let it time out.
pub struct Delivery {
    pub seq: u64,
    /// How many times JetStream has delivered it, this one included.
    pub delivered: u64,
    pub note: Note,
    handle: Option<Handle>,
}

impl Delivery {
    /// A delivery not bound to the hub, for tests of code that consumes deliveries.
    /// Its `ack`, `nak`, `in_progress` and `term` succeed and do nothing.
    pub fn detached(seq: u64, delivered: u64, note: Note) -> Delivery {
        Delivery {
            seq,
            delivered,
            note,
            handle: None,
        }
    }

    /// Acknowledge, and wait for the hub to confirm it.
    pub async fn ack(&self) -> Result<()> {
        match &self.handle {
            Some(h) => h
                .msg
                .double_ack()
                .await
                .map_err(|e| nats(&format!("acknowledging note {}", self.seq), e)),
            None => Ok(()),
        }
    }

    /// Ask for redelivery after `delay` (zero: at once). Counts toward `max_deliver`.
    pub async fn nak(&self, delay: Duration) -> Result<()> {
        let kind = if delay.is_zero() {
            AckKind::Nak(None)
        } else {
            AckKind::Nak(Some(delay))
        };
        self.signal(kind, "returning").await
    }

    /// Restart the `ack_wait` timer while work on the note continues (PROTO-8).
    pub async fn in_progress(&self) -> Result<()> {
        self.signal(AckKind::Progress, "extending").await
    }

    /// Stop redelivery without acknowledging (a note that can never be handled).
    pub async fn term(&self) -> Result<()> {
        self.signal(AckKind::Term, "terminating").await
    }

    async fn signal(&self, kind: AckKind, what: &str) -> Result<()> {
        let Some(h) = &self.handle else { return Ok(()) };
        let ctx = || format!("{what} note {}", self.seq);
        h.msg.ack_with(kind).await.map_err(|e| nats(&ctx(), e))?;
        h.client.flush().await.map_err(|e| nats(&ctx(), e))
    }
}

/// The latest value of `key`, unless it was deleted.
async fn live_entry(store: &kv::Store, key: &str) -> Result<Option<kv::Entry>> {
    let entry = store
        .entry(key)
        .await
        .map_err(|e| nats(&format!("reading {key}"), e))?;
    Ok(entry.filter(|e| e.operation == kv::Operation::Put))
}

async fn put_json<T: Serialize>(store: &kv::Store, key: &str, rec: &T) -> Result<()> {
    check_kv_key(key)?;
    let value = serde_json::to_vec(rec).map_err(|e| nats(&format!("encoding {key}"), e))?;
    store
        .put(key, value.into())
        .await
        .map_err(|e| nats(&format!("writing {key}"), e))?;
    Ok(())
}

/// Every live value of a JSON bucket, ordered by key. A value that does not
/// parse is logged and skipped.
async fn list_json<T: DeserializeOwned>(store: &kv::Store) -> Result<Vec<T>> {
    let mut keys = store.keys().await.map_err(|e| nats("listing keys", e))?;
    let mut names = Vec::new();
    while let Some(k) = keys.next().await {
        names.push(k.map_err(|e| nats("listing keys", e))?);
    }
    names.sort();
    names.dedup();
    let mut out = Vec::with_capacity(names.len());
    for key in names {
        let Some(e) = live_entry(store, &key).await? else {
            continue;
        };
        match serde_json::from_slice(&e.value) {
            Ok(v) => out.push(v),
            Err(err) => tracing::warn!(key, "skipping an unreadable record: {err}"),
        }
    }
    Ok(out)
}

fn check_value_size(key: &str, len: usize) -> Result<()> {
    if len > MAX_VALUE_BYTES {
        return Err(BusError::Nats(format!(
            "state {key} would be {len} bytes; the limit is {MAX_VALUE_BYTES}"
        )));
    }
    Ok(())
}

/// A short, varying pause between compare-and-set attempts, so racing writers spread out.
fn backoff(attempt: u32) -> Duration {
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() % 7)
        .unwrap_or(0);
    Duration::from_millis(u64::from(attempt.min(10) + jitter))
}

/// A state key must satisfy §8 and also be a valid KV key.
fn check_state_key(key: &str) -> Result<()> {
    hgl_ai_proto::check_state_key(key)?;
    check_kv_key(key)
}

/// NATS KV key rules: `[-/_=.a-zA-Z0-9]`, no empty dot-separated token.
fn check_kv_key(key: &str) -> Result<()> {
    let chars_ok = key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-/_=.".contains(c));
    if chars_ok && !key.is_empty() && key.split('.').all(|t| !t.is_empty()) {
        Ok(())
    } else {
        Err(BusError::Nats(format!("invalid bucket key {key:?}")))
    }
}

fn nats(what: &str, e: impl std::fmt::Display) -> BusError {
    BusError::Nats(format!("{what}: {e}"))
}

/// A lookup error, with a hint when the stream or bucket was never provisioned.
fn missing(what: &str, e: impl std::fmt::Display) -> BusError {
    let text = e.to_string();
    if text.contains(STREAM_NOT_FOUND) {
        BusError::Nats(format!("{what} not found on the hub; run `hgl-ai init`"))
    } else {
        BusError::Nats(format!("opening {what}: {text}"))
    }
}

fn publish_error(subject: &str, e: jetstream::context::PublishError) -> BusError {
    match e.kind() {
        jetstream::context::PublishErrorKind::StreamNotFound => {
            BusError::Nats(format!("no stream takes {subject}; run `hgl-ai init`"))
        }
        _ => nats(&format!("publishing to {subject}"), e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hgl_ai_proto::Kind;

    fn note(to: &str) -> Note {
        Note::new(
            Seat::new("w1").unwrap(),
            Seat::new(to).unwrap(),
            Kind::Note,
            "hello",
        )
    }

    #[test]
    fn parse_accepts_a_valid_note() {
        let n = note("claude");
        let got = parse_note("hgl.notes.claude", &serde_json::to_vec(&n).unwrap()).unwrap();
        assert_eq!(got, n);
        let b = note("all");
        assert!(parse_note("hgl.notes.all", &serde_json::to_vec(&b).unwrap()).is_ok());
    }

    #[test]
    fn parse_rejects_malformed_payloads() {
        assert!(
            parse_note("hgl.notes.claude", b"not json")
                .unwrap_err()
                .starts_with("not a note")
        );
        assert!(parse_note("hgl.notes.claude", br#"{"v":1}"#).is_err());
        let mut n = note("claude");
        n.body = " \n".into();
        assert!(
            parse_note("hgl.notes.claude", &serde_json::to_vec(&n).unwrap())
                .unwrap_err()
                .contains("empty body")
        );
        let mut r = note("claude");
        r.kind = Kind::Ready;
        assert!(
            parse_note("hgl.notes.claude", &serde_json::to_vec(&r).unwrap())
                .unwrap_err()
                .contains("head")
        );
        let n = note("claude");
        let err = parse_note("hgl.notes.w2", &serde_json::to_vec(&n).unwrap()).unwrap_err();
        assert!(err.contains("does not match"), "{err}");
        let mut big = note("claude");
        big.body = "x".repeat(MAX_NOTE_BYTES);
        let err = parse_note("hgl.notes.claude", &serde_json::to_vec(&big).unwrap()).unwrap_err();
        assert!(err.contains("byte limit"), "{err}");
    }

    #[test]
    fn conflict_messages() {
        let e = BusError::Conflict {
            key: "prs".into(),
            expected: 0,
        };
        assert_eq!(e.to_string(), "state prs already exists");
        let e = BusError::Conflict {
            key: "prs".into(),
            expected: 7,
        };
        assert_eq!(e.to_string(), "state prs is no longer at revision 7");
    }

    #[test]
    fn kv_keys() {
        for ok in [
            "build-1",
            "build-1.w1.hgl-compiler-992",
            "landed.log",
            "a/b=c",
            "x_y",
        ] {
            assert!(check_kv_key(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".a", "a.", "a..b", "a b", "a*", "a>"] {
            assert!(check_kv_key(bad).is_err(), "{bad}");
        }
        assert!(check_state_key("prs").is_ok());
        assert!(
            check_state_key(".prs").is_err(),
            "valid for §8, not for NATS"
        );
        assert!(matches!(check_state_key("a b"), Err(BusError::Proto(_))));
    }

    #[test]
    fn token_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hub.token");
        std::fs::write(&p, "  s3cret\n").unwrap();
        assert_eq!(read_token(&p).unwrap().as_deref(), Some("s3cret"));
        std::fs::write(&p, "\n").unwrap();
        assert_eq!(read_token(&p).unwrap(), None);
        assert!(matches!(
            read_token(&dir.path().join("none")),
            Err(BusError::Token { .. })
        ));
    }

    #[test]
    fn missing_hint() {
        let e = missing(
            "stream HGL_NOTES",
            "stream not found (code 404, error code 10059)",
        );
        assert!(e.to_string().contains("hgl-ai init"));
        let e = missing("stream HGL_NOTES", "timed out");
        assert!(!e.to_string().contains("hgl-ai init"));
    }

    #[tokio::test]
    async fn detached_delivery_is_inert() {
        let d = Delivery::detached(3, 1, note("claude"));
        d.ack().await.unwrap();
        d.nak(Duration::from_secs(15)).await.unwrap();
        d.in_progress().await.unwrap();
        d.term().await.unwrap();
        assert_eq!((d.seq, d.delivered), (3, 1));
    }
}
