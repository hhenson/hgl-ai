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
//! - `state_*`, `host*` and `tree*` read and write the `HGL_STATE`, `HGL_HOSTS`
//!   and `HGL_TREES` buckets. Writers of shared keys (`prs`, `main`,
//!   `landed.log`) use the compare-and-set calls [`Bus::state_create`],
//!   [`Bus::state_update`] and [`Bus::append_line`]; [`Bus::state_put`] is the
//!   explicit blind write.
//! - [`Bus::lock_acquire`] takes a lease in `HGL_LOCKS` (create-if-absent, so
//!   one holder on any host); the holder keeps it with [`Lease::refresh`] and
//!   gives it up with [`Lease::release`]. A lease not refreshed within the
//!   bucket's `max_age` expires, so a dead holder does not keep it.
//!   [`Bus::lock_get`] names the present holder; [`Bus::lock_take_over`] takes
//!   a lease from a holder known to be dead.
//! - [`Bus::ended_put`] records in `HGL_ENDED` that a task ended for a seat, at
//!   the stream sequence of the ending note; [`Bus::ended_get`] reads it back,
//!   on any host (PROTO-14).
//! - Every read is bounded: a note payload over [`MAX_NOTE_BYTES`] is refused
//!   on send and terminated on receipt; buckets refuse values over
//!   [`MAX_VALUE_BYTES`].
//!
//! - [`Bus::publish_event`], [`Bus::owner_feed`], [`Bus::history`] and the `thread_*` and
//!   `metrics*` calls carry the owner channel (docs/owner.md §4); [`cred`] reads a hub
//!   credential: the legacy shared token or a hub user's NKey seed (OWN-5).
//! - [`Bus::usage_put`] keeps the newest usage record per login in `HGL_USAGE`
//!   (compare-and-set on `observed_at`) and publishes it to `HGL_USAGE_LOG`;
//!   [`Bus::usage`] lists them with their labels (PROTO-23, PROTO-24).
//! - A publish the connection's hub user may not make fails with [`BusError::Denied`].
//!
//! Nothing here starts a hub; the operations other than `connect*` and
//! `provision` fail with a hint to run `hgl-ai init` when the stream or a
//! bucket is missing.

pub mod cred;
mod feed;
mod kv_read;
mod usage;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use cred::{Credential, parse_credential, read_credential};
pub use feed::{Feed, StreamMsg};
pub use usage::UsagePut;

use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy, pull};
use async_nats::jetstream::{self, AckKind, context::Publish, kv, stream};
use futures::StreamExt;
use hgl_ai_config::Config;
use hgl_ai_proto::{
    BROADCAST, CANARY_PROBE, CANARY_PROBE_VALUE, CONSOLE_PREFIX, EndedRecord, HUB_API_PREFIX,
    HostRecord, KV_CANARY, KV_CONSOLE, KV_ENDED, KV_HOSTS, KV_LOCKS, KV_METRICS, KV_SLACK,
    KV_STATE, KV_THREADS, KV_TREES, KV_USAGE, LockRecord, Note, OWNER_PREFIX, STREAM_NOTES,
    STREAM_OWNER, STREAM_USAGE, SUBJECT_PREFIX, Seat, TaskKey, TreeRecord, USAGE_PREFIX, ended_key,
    utc_now_stamp,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use tokio::sync::OnceCell;

use kv_read::keys;

mod build;
pub use build::BUILD_HISTORY;

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
    /// [`Bus::lock_acquire`]: someone else holds the lease.
    #[error("{name} is held by {holder} since {since}")]
    Held {
        name: String,
        holder: String,
        since: String,
    },
    /// [`Lease::refresh`]: the lease expired or another holder took it.
    #[error("the lease {name} is no longer ours (it expired or was taken)")]
    LeaseLost { name: String },
    /// The hub refused a publish this connection's hub user may not make (OWN-5).
    #[error("the hub refused {subject}: this hub user ({who}) may not publish there")]
    Denied { subject: String, who: String },
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
/// Revisions kept per `HGL_THREADS` record.
pub const THREADS_HISTORY: i64 = 5;
/// `HGL_OWNER` drops a resent event within this window.
pub const OWNER_DUPLICATE_WINDOW: Duration = Duration::from_secs(86_400);
/// A lease in `HGL_LOCKS` expires this long after its last write (§3).
pub const LOCKS_MAX_AGE: Duration = Duration::from_secs(30 * 60);
/// An `HGL_ENDED` record outlives every note sent before it: the stream's `max_age` and a day.
pub const ENDED_MAX_AGE: Duration = Duration::from_secs(NOTES_MAX_AGE.as_secs() + 86_400);
/// Inbox `ack_wait` (§3); the bridge extends it with `in_progress` (PROTO-8).
pub const INBOX_ACK_WAIT: Duration = Duration::from_secs(300);
/// Inbox `max_deliver` (§3): unlimited. JetStream never drops a note on its own; the bridge
/// counts a note's real failures and dead-letters it to the controller (PROTO-8).
pub const INBOX_MAX_DELIVER: i64 = -1;
/// Largest note payload (the encoded JSON) that is sent or accepted.
pub const MAX_NOTE_BYTES: usize = 1 << 20;
/// Largest value a bucket holds.
pub const MAX_VALUE_BYTES: usize = 1 << 20;
/// `HGL_USAGE_LOG` keeps usage records 35 days, the burn-down history (PROTO-23).
pub const USAGE_MAX_AGE: Duration = Duration::from_secs(35 * 86_400);
/// Attempts [`Bus::append_line`] makes before giving up with [`BusError::Conflict`].
const APPEND_ATTEMPTS: u32 = 200;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_API_PREFIX: &str = "$JS.API";
/// Longest single pull request; longer waits are split so a reconnect is noticed.
const MAX_PULL: Duration = Duration::from_secs(30);
/// Shortest pull expiry worth sending to the server.
const MIN_PULL: Duration = Duration::from_millis(10);
/// JetStream API error code for a missing stream.
const STREAM_NOT_FOUND: &str = "error code 10059";

/// Longest wait for the hub to confirm an ack, nak, term or in-progress.
const SIGNAL_WAIT: Duration = Duration::from_secs(10);
/// Largest batch one stream read asks for.
const READ_BATCH: usize = 256;
/// How long an ephemeral reader outlives its last use, should it not be deleted.
const READER_IDLE: Duration = Duration::from_secs(30);

/// A note read from the stream without an inbox delivery ([`Bus::read_notes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamNote {
    pub seq: u64,
    pub note: Note,
}

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
    /// Unix seconds of the write, by the hub's clock.
    pub created: i64,
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

/// Provisioning settings. [`Default`] is the protocol's (§3); a short
/// `locks_max_age` is for tests of lease expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProvisionOptions {
    pub locks_max_age: Duration,
}

impl Default for ProvisionOptions {
    fn default() -> Self {
        ProvisionOptions {
            locks_max_age: LOCKS_MAX_AGE,
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
    api_prefix: &'static str,
    url: String,
    who: String,
    /// The hub's permissions violations on this connection, newest last (a few kept).
    denied: Arc<Mutex<Vec<String>>>,
    /// Woken on each violation, so a refused publish fails at once.
    denied_tick: Arc<tokio::sync::Notify>,
    state: OnceCell<kv::Store>,
    hosts: OnceCell<kv::Store>,
    trees: OnceCell<kv::Store>,
    locks: OnceCell<kv::Store>,
    ended: OnceCell<kv::Store>,
    threads: OnceCell<kv::Store>,
    metrics: OnceCell<kv::Store>,
    console: OnceCell<kv::Store>,
    slack: OnceCell<kv::Store>,
    usage: OnceCell<kv::Store>,
    build: OnceCell<kv::Store>,
}

impl Bus {
    /// Connect with the host configuration: `hub.url` and the credential in
    /// `hub.token_file` (or `HGL_AI_HUB_CREDENTIAL`, see [`Config::token_file`]).
    pub async fn connect(cfg: &Config) -> Result<Bus> {
        let cred = read_credential(&cfg.token_file()?)?;
        Bus::open(&cfg.hub.url, cred, format!("hgl-ai {}", cfg.host)).await
    }

    /// Connect to `url`, with an optional token.
    pub async fn connect_url(url: &str, token: Option<String>) -> Result<Bus> {
        Bus::open(url, token.map(Credential::Token), "hgl-ai".to_string()).await
    }

    /// Connect to `url` with a credential.
    pub async fn connect_with(url: &str, cred: Option<Credential>) -> Result<Bus> {
        Bus::open(url, cred, "hgl-ai".to_string()).await
    }

    async fn open(url: &str, cred: Option<Credential>, name: String) -> Result<Bus> {
        let denied: Arc<Mutex<Vec<String>>> = Arc::default();
        let denied_tick = Arc::new(tokio::sync::Notify::new());
        let seen = denied.clone();
        let tick = denied_tick.clone();
        let probe = format!("{HUB_API_PREFIX}.INFO");
        let quoted_probe = format!("\"{probe}\"");
        let mut opts = async_nats::ConnectOptions::new()
            .name(name)
            .connection_timeout(CONNECT_TIMEOUT)
            .event_callback(move |ev| {
                let seen = seen.clone();
                let tick = tick.clone();
                let quoted_probe = quoted_probe.clone();
                async move {
                    if let async_nats::Event::ServerError(async_nats::ServerError::Other(m)) = &ev
                        && m.to_ascii_lowercase().contains("permissions violation")
                    {
                        if !m.contains(&quoted_probe) {
                            tracing::warn!("hub: {m}");
                        }
                        if let Ok(mut d) = seen.lock() {
                            d.push(m.clone());
                            let n = d.len();
                            if n > 16 {
                                d.drain(..n - 16);
                            }
                        }
                        tick.notify_waiters();
                    }
                }
            });
        let who = cred
            .as_ref()
            .map_or_else(|| "no credential".to_string(), Credential::describe);
        // A hub user may subscribe to its own inbox prefix only (hgl-ai#111).
        if let Some(p) = cred.as_ref().and_then(Credential::inbox_prefix) {
            opts = opts.custom_inbox_prefix(p);
        }
        match cred {
            Some(Credential::Token(t)) => opts = opts.token(t),
            Some(Credential::Nkey { seed, .. }) => opts = opts.nkey(seed),
            None => {}
        }
        let client = opts.connect(url).await.map_err(|e| BusError::Unreachable {
            url: url.to_string(),
            reason: e.to_string(),
        })?;
        let mut bus = Bus {
            js: jetstream::new(client.clone()),
            client,
            api_prefix: DEFAULT_API_PREFIX,
            url: url.to_string(),
            who,
            denied,
            denied_tick,
            state: OnceCell::new(),
            hosts: OnceCell::new(),
            trees: OnceCell::new(),
            locks: OnceCell::new(),
            ended: OnceCell::new(),
            threads: OnceCell::new(),
            metrics: OnceCell::new(),
            console: OnceCell::new(),
            slack: OnceCell::new(),
            usage: OnceCell::new(),
            build: OnceCell::new(),
        };
        // A legacy user may be refused the probe, and an admin in HUB has no import.
        let imported = jetstream::with_prefix(bus.client.clone(), HUB_API_PREFIX);
        let answered = bus
            .guarded(&probe, async {
                tokio::time::timeout(CONNECT_TIMEOUT, imported.query_account())
                    .await
                    .map_err(|e| nats("probing the hub API", e))?
                    .map_err(|e| nats("probing the hub API", e))
            })
            .await;
        if answered.is_ok() {
            bus.js = imported;
            bus.api_prefix = HUB_API_PREFIX;
        }
        Ok(bus)
    }

    fn api_subject(&self, suffix: &str) -> String {
        format!("{}.{suffix}", self.api_prefix)
    }

    fn kv_subject(&self, bucket: &str, key: &str) -> String {
        let subject = format!("$KV.{bucket}.{key}");
        if self.api_prefix == HUB_API_PREFIX {
            self.api_subject(&subject)
        } else {
            subject
        }
    }

    fn legacy_kv_pull_denied(&self, error: &BusError) -> bool {
        self.api_prefix == DEFAULT_API_PREFIX
            && matches!(error, BusError::Denied { subject, .. }
                if subject.starts_with("$JS.API.CONSUMER.MSG.NEXT.KV_"))
    }

    /// Who this connection is: `hub user NAME (ROLE)` or `shared token`.
    pub fn who(&self) -> &str {
        &self.who
    }

    /// Take a recorded violation for `subject`, if the hub reported one.
    fn take_denied(&self, subject: &str) -> bool {
        let Ok(mut d) = self.denied.lock() else {
            return false;
        };
        let quoted = format!("\"{subject}\"");
        match d.iter().position(|m| m.contains(&quoted)) {
            Some(i) => {
                d.remove(i);
                true
            }
            None => false,
        }
    }

    /// `err`, unless the hub reported a permissions violation for `subject`: then
    /// [`BusError::Denied`].
    pub(crate) fn denied_or(&self, subject: &str, err: BusError) -> BusError {
        if self.take_denied(subject) {
            self.denial(subject)
        } else {
            err
        }
    }

    fn denial(&self, subject: &str) -> BusError {
        BusError::Denied {
            subject: subject.to_string(),
            who: self.who.clone(),
        }
    }

    /// Run a publish to `subject`, failing at once with [`BusError::Denied`] when the hub
    /// reports a permissions violation for it, instead of waiting out the reply timeout.
    pub(crate) async fn guarded<T>(
        &self,
        subject: &str,
        fut: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let refused = async {
            loop {
                let woken = self.denied_tick.notified();
                if self.take_denied(subject) {
                    return;
                }
                woken.await;
            }
        };
        tokio::select! {
            r = fut => r.map_err(|e| self.denied_or(subject, e)),
            () = refused => Err(self.denial(subject)),
        }
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
        self.provision_with(ProvisionOptions::default()).await
    }

    /// [`Bus::provision`] with explicit settings.
    pub async fn provision_with(&self, opts: ProvisionOptions) -> Result<()> {
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
        self.js
            .create_or_update_stream(stream::Config {
                name: STREAM_OWNER.to_string(),
                description: Some("hgl-ai owner channel".to_string()),
                subjects: vec![format!("{OWNER_PREFIX}.>"), format!("{CONSOLE_PREFIX}.>")],
                retention: stream::RetentionPolicy::Limits,
                discard: stream::DiscardPolicy::Old,
                storage: stream::StorageType::File,
                duplicate_window: OWNER_DUPLICATE_WINDOW,
                max_message_size: (MAX_NOTE_BYTES + (64 << 10)) as i32,
                ..Default::default()
            })
            .await
            .map_err(|e| nats(&format!("provisioning stream {STREAM_OWNER}"), e))?;
        self.js
            .create_or_update_stream(stream::Config {
                name: STREAM_USAGE.to_string(),
                description: Some("hgl-ai subscription usage history".to_string()),
                subjects: vec![format!("{USAGE_PREFIX}.>")],
                retention: stream::RetentionPolicy::Limits,
                discard: stream::DiscardPolicy::Old,
                storage: stream::StorageType::File,
                max_age: USAGE_MAX_AGE,
                duplicate_window: NOTES_DUPLICATE_WINDOW,
                max_message_size: (MAX_VALUE_BYTES + (64 << 10)) as i32,
                ..Default::default()
            })
            .await
            .map_err(|e| nats(&format!("provisioning stream {STREAM_USAGE}"), e))?;
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
            (
                KV_LOCKS,
                "hgl-ai leases",
                stream::StorageType::File,
                1,
                opts.locks_max_age,
            ),
            (
                KV_ENDED,
                "hgl-ai tasks ended per seat",
                stream::StorageType::File,
                1,
                ENDED_MAX_AGE,
            ),
            (
                KV_THREADS,
                "hgl-ai owner threads",
                stream::StorageType::File,
                THREADS_HISTORY,
                Duration::ZERO,
            ),
            (
                KV_METRICS,
                "hgl-ai daily metrics",
                stream::StorageType::File,
                1,
                Duration::ZERO,
            ),
            (
                KV_CONSOLE,
                "hgl-ai console state",
                stream::StorageType::File,
                1,
                Duration::ZERO,
            ),
            (
                KV_SLACK,
                "hgl-ai Slack delivery state",
                stream::StorageType::File,
                1,
                Duration::ZERO,
            ),
            (
                KV_USAGE,
                "hgl-ai subscription usage per login",
                stream::StorageType::File,
                1,
                Duration::ZERO,
            ),
            build::BUCKET,
            // Holds only its probe while the hub enforces role boundaries (`doctor`,
            // `hub boundary`). No age limit: the probe must not expire. A write that
            // arrives (one account, about 100 bytes a doctor run) stays until purged.
            (
                KV_CANARY,
                "hgl-ai role boundary canary; no role writes it",
                stream::StorageType::File,
                1,
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
        // The canary's fixed probe message (`doctor`, `hub boundary`).
        self.js
            .get_key_value(KV_CANARY)
            .await
            .map_err(|e| missing(&format!("bucket {KV_CANARY}"), e))?
            .put(CANARY_PROBE, CANARY_PROBE_VALUE.into())
            .await
            .map_err(|e| nats(&format!("writing {KV_CANARY}/{CANARY_PROBE}"), e))?;
        Ok(())
    }

    /// Validate and publish a note to its subject ([`Note::subject`]: `hgl.notes.<recipient>`,
    /// or `hgl.notes.owner.<recipient>` for the owner's) with its message id, and wait for
    /// the hub's acknowledgement.
    pub async fn send(&self, note: &Note) -> Result<SendReceipt> {
        self.send_with_id(note, &note.msg_id()).await
    }

    /// Send a note keyed by its source event when the note contents alone are not unique.
    pub async fn send_with_id(&self, note: &Note, msg_id: &str) -> Result<SendReceipt> {
        note.validate()?;
        let subject = note.subject();
        let payload = serde_json::to_vec(note).map_err(|e| nats("encoding the note", e))?;
        if payload.len() > MAX_NOTE_BYTES {
            return Err(BusError::Nats(format!(
                "the note is {} bytes encoded; the limit is {MAX_NOTE_BYTES}",
                payload.len()
            )));
        }
        let publish = Publish::build().payload(payload.into()).message_id(msg_id);
        let ack = self
            .guarded(&subject, async {
                self.js
                    .send_publish(subject.clone(), publish)
                    .await
                    .map_err(|e| publish_error(&subject, e))?
                    .await
                    .map_err(|e| publish_error(&subject, e))
            })
            .await?;
        Ok(SendReceipt {
            seq: ack.sequence,
            duplicate: ack.duplicate,
        })
    }

    /// The durable inbox of `seat`, created on first use. Only broadcasts published
    /// from its creation onward are delivered; any retained unicast is eligible.
    pub async fn inbox(&self, seat: &Seat) -> Result<Inbox> {
        self.inbox_with(seat, InboxOptions::default()).await
    }

    /// The durable inbox of `seat` with explicit consumer settings. The consumer
    /// is created, or updated to these settings if it exists.
    pub async fn inbox_with(&self, seat: &Seat, opts: InboxOptions) -> Result<Inbox> {
        check_inbox_seat(seat)?;
        let stream = self
            .js
            .get_stream(STREAM_NOTES)
            .await
            .map_err(|e| missing(&format!("stream {STREAM_NOTES}"), e))?;
        let mut consumer = stream
            .create_consumer(pull::Config {
                durable_name: Some(seat.consumer()),
                description: Some(format!("inbox of seat {seat}")),
                deliver_policy: DeliverPolicy::All,
                ack_policy: AckPolicy::Explicit,
                ack_wait: opts.ack_wait,
                max_deliver: opts.max_deliver,
                filter_subjects: vec![
                    seat.subject(),
                    Seat::broadcast().subject(),
                    seat.owner_subject(),
                ],
                ..Default::default()
            })
            .await
            .map_err(|e| nats(&format!("opening the inbox of {seat}"), e))?;
        // After a hub restart, CREATE can report the reload time as `created`.
        // INFO refreshes the cached creation time used to skip old broadcasts.
        consumer
            .info()
            .await
            .map_err(|e| nats(&format!("reading the inbox of {seat}"), e))?;
        Ok(Inbox {
            seat: seat.clone(),
            consumer,
            client: self.client.clone(),
        })
    }

    /// The notes for `seats` from stream sequence `from` on, oldest first, at most `max`;
    /// waits up to `wait` for the first when none is there. Read through an ephemeral
    /// consumer without acknowledgements, so no inbox delivery is spent and nothing is
    /// acknowledged (`note peek`, the merge gate's receipts). A message that is not a valid
    /// note is skipped.
    pub async fn read_notes(
        &self,
        seats: &[Seat],
        from: u64,
        max: usize,
        wait: Duration,
    ) -> Result<Vec<StreamNote>> {
        let subjects = seats.iter().map(Seat::subject).collect();
        Ok(self.read(subjects, from, max, wait, None).await?.0)
    }

    /// One page of a whole-history read: at most `max` messages for `seats` from stream
    /// sequence `from` on, without waiting, and the sequence of the last message read
    /// (`None`: there was none), so a reader goes on from after it whatever it skipped.
    pub async fn read_page(
        &self,
        seats: &[Seat],
        from: u64,
        max: usize,
    ) -> Result<(Vec<StreamNote>, Option<u64>)> {
        let subjects = seats.iter().map(Seat::subject).collect();
        self.read(subjects, from, max, Duration::ZERO, None).await
    }

    async fn read(
        &self,
        subjects: Vec<String>,
        from: u64,
        max: usize,
        wait: Duration,
        broadcast_start: Option<OffsetDateTime>,
    ) -> Result<(Vec<StreamNote>, Option<u64>)> {
        let mut out = Vec::new();
        if max == 0 || subjects.is_empty() {
            return Ok((out, None));
        }
        let stream = self
            .js
            .get_stream(STREAM_NOTES)
            .await
            .map_err(|e| missing(&format!("stream {STREAM_NOTES}"), e))?;
        let reader = stream
            .create_consumer(pull::Config {
                description: Some("hgl-ai reader".to_string()),
                deliver_policy: DeliverPolicy::ByStartSequence {
                    start_sequence: from.max(1),
                },
                ack_policy: AckPolicy::None,
                filter_subjects: subjects,
                inactive_threshold: READER_IDLE,
                ..Default::default()
            })
            .await
            .map_err(|e| nats("opening a stream reader", e))?;
        let mut last = None;
        let read = Self::read_with(&reader, max, wait, &mut out, &mut last, broadcast_start).await;
        if self.api_prefix == DEFAULT_API_PREFIX {
            let name = &reader.cached_info().name;
            let subject = self.api_subject(&format!("CONSUMER.DELETE.{STREAM_NOTES}.{name}"));
            if let Err(e) = self
                .guarded(&subject, async {
                    stream
                        .delete_consumer(name)
                        .await
                        .map_err(|e| nats("deleting a stream reader", e))
                })
                .await
            {
                tracing::debug!("deleting the stream reader {name}: {e}");
            }
        }
        read.map(|()| (out, last))
    }

    /// Read up to `max` eligible messages (malformed ones count, and are skipped);
    /// `last` is the stream sequence of the last eligible message read.
    async fn read_with(
        reader: &jetstream::consumer::PullConsumer,
        max: usize,
        wait: Duration,
        out: &mut Vec<StreamNote>,
        last: &mut Option<u64>,
        broadcast_start: Option<OffsetDateTime>,
    ) -> Result<()> {
        let deadline = Instant::now().checked_add(wait);
        let mut seen = 0;
        // Wait for the first, as an inbox fetch does; then read what is there without waiting.
        loop {
            let remaining = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => MAX_PULL,
            };
            if remaining < MIN_PULL {
                break;
            }
            let batch = reader
                .batch()
                .max_messages(1)
                .expires(remaining.min(MAX_PULL))
                .messages()
                .await
                .map_err(|e| nats("reading the stream", e))?;
            let (_, n) = take_read(batch, out, last, broadcast_start).await?;
            seen += n;
            if n > 0 {
                break;
            }
        }
        while seen < max {
            let batch = reader
                .fetch()
                .max_messages((max - seen).min(READ_BATCH))
                .messages()
                .await
                .map_err(|e| nats("reading the stream", e))?;
            let (received, n) = take_read(batch, out, last, broadcast_start).await?;
            if !received {
                break;
            }
            seen += n;
        }
        Ok(())
    }

    /// What `seat`'s inbox has not acknowledged, oldest first, without a delivery: the notes
    /// after its acknowledgement floor (one acknowledged out of order may be among them).
    /// Until the inbox is created, only unicast is eligible.
    pub async fn peek(&self, seat: &Seat, max: usize, wait: Duration) -> Result<Vec<StreamNote>> {
        check_inbox_seat(seat)?;
        let stream = self
            .js
            .get_stream(STREAM_NOTES)
            .await
            .map_err(|e| missing(&format!("stream {STREAM_NOTES}"), e))?;
        let mut subjects = vec![seat.subject(), seat.owner_subject()];
        let (from, broadcast_start) = match stream.consumer_info(seat.consumer()).await {
            Ok(i) => {
                subjects.push(Seat::broadcast().subject());
                (i.ack_floor.stream_sequence + 1, Some(i.created))
            }
            Err(e) if e.kind() == jetstream::context::ConsumerInfoErrorKind::NotFound => (1, None),
            Err(e) => return Err(nats(&format!("reading the inbox of {seat}"), e)),
        };
        Ok(self
            .read(subjects, from, max, wait, broadcast_start)
            .await?
            .0)
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
                    created: e.created.unix_timestamp(),
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
        self.guarded(&self.kv_subject(KV_STATE, key), async {
            store
                .put(key, value.as_bytes().to_vec().into())
                .await
                .map_err(|e| nats(&format!("writing state {key}"), e))
        })
        .await
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
        self.guarded(&self.kv_subject(KV_STATE, key), async {
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
        })
        .await
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
        self.guarded(&self.kv_subject(KV_STATE, key), async {
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
        })
        .await
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
        self.guarded(&self.kv_subject(KV_STATE, key), async {
            store
                .purge(key)
                .await
                .map_err(|e| nats(&format!("deleting state {key}"), e))
        })
        .await
    }

    /// The live keys of `HGL_STATE` that start with `prefix`, sorted.
    pub async fn state_keys(&self, prefix: &str) -> Result<Vec<String>> {
        let store = self.store(&self.state, KV_STATE).await?;
        keys(self, store, |key| key.starts_with(prefix)).await
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

    /// Every live host record, ordered by host label (builder records are apart:
    /// [`Bus::builders`]).
    pub async fn hosts(&self) -> Result<Vec<HostRecord>> {
        let store = self.store(&self.hosts, KV_HOSTS).await?;
        list_json_where(self, store, |k| !build::is_builder_key(k)).await
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

    /// Register `rec` without losing a fresher record a concurrent writer put under its key
    /// (hgl-ai#51): a compare-and-set of `rec` merged with the record held
    /// ([`TreeRecord::merge_held`]), retried on a lost race. Returns the record now held.
    pub async fn tree_merge(&self, rec: &TreeRecord) -> Result<TreeRecord> {
        let key = rec.key();
        check_kv_key(&key)?;
        let store = self.store(&self.trees, KV_TREES).await?;
        for attempt in 0..APPEND_ATTEMPTS {
            let entry = live_entry(store, &key).await?;
            let held = entry
                .as_ref()
                .and_then(|e| serde_json::from_slice::<TreeRecord>(&e.value).ok());
            let mut merged = rec.clone();
            if let Some(h) = &held {
                merged.merge_held(h);
                if &merged == h {
                    return Ok(merged);
                }
            }
            let value: bytes::Bytes = serde_json::to_vec(&merged)
                .map_err(|e| nats(&format!("encoding tree {key}"), e))?
                .into();
            let done = match &entry {
                Some(e) => store
                    .update(&key, value, e.revision)
                    .await
                    .map(drop)
                    .map_err(|e| {
                        (
                            e.kind() == kv::UpdateErrorKind::WrongLastRevision,
                            e.to_string(),
                        )
                    }),
                None => store.create(&key, value).await.map(drop).map_err(|e| {
                    (
                        e.kind() == kv::CreateErrorKind::AlreadyExists,
                        e.to_string(),
                    )
                }),
            };
            match done {
                Ok(()) => return Ok(merged),
                Err((true, _)) => tokio::time::sleep(backoff(attempt)).await,
                Err((false, e)) => return Err(nats(&format!("writing tree {key}"), e)),
            }
        }
        Err(BusError::Conflict { key, expected: 0 })
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
        list_json(self, store).await
    }

    /// Take the lease `name` for `holder` if nobody holds it (create-if-absent: of
    /// concurrent callers on any host, one wins). Fails with [`BusError::Held`], naming
    /// the holder, when it is held. The caller refreshes it well within the bucket's
    /// `max_age` and releases it when done.
    pub async fn lock_acquire(&self, name: &str, holder: &str) -> Result<Lease> {
        check_kv_key(name)?;
        let store = self.store(&self.locks, KV_LOCKS).await?;
        let record = LockRecord {
            holder: holder.to_string(),
            acquired_at: utc_now_stamp(),
            token: lease_token(),
        };
        let value: bytes::Bytes = serde_json::to_vec(&record)
            .map_err(|e| nats(&format!("encoding lease {name}"), e))?
            .into();
        match store.create(name, value.clone()).await {
            Ok(revision) => Ok(Lease {
                store: store.clone(),
                name: name.to_string(),
                record,
                value,
                revision,
            }),
            Err(e) if e.kind() == kv::CreateErrorKind::AlreadyExists => {
                let held = live_entry(store, name)
                    .await?
                    .and_then(|e| serde_json::from_slice::<LockRecord>(&e.value).ok());
                Err(BusError::Held {
                    name: name.to_string(),
                    holder: held
                        .as_ref()
                        .map_or_else(|| "another holder".to_string(), |r| r.holder.clone()),
                    since: held.map_or_else(|| "just now".to_string(), |r| r.acquired_at),
                })
            }
            Err(e) => Err(nats(&format!("taking lease {name}"), e)),
        }
    }

    /// Take over the lease `name` from `stale`, a holder the caller knows is dead (a
    /// compare-and-set at the revision it holds, so of concurrent callers one wins). Fails
    /// with [`BusError::Held`] when someone other than `stale` holds it; a lease that went
    /// in the meantime is taken as by [`Bus::lock_acquire`].
    pub async fn lock_take_over(&self, name: &str, holder: &str, stale: &str) -> Result<Lease> {
        check_kv_key(name)?;
        let store = self.store(&self.locks, KV_LOCKS).await?;
        let Some(entry) = live_entry(store, name).await? else {
            return self.lock_acquire(name, holder).await;
        };
        let held = serde_json::from_slice::<LockRecord>(&entry.value).ok();
        let held_by = |r: &Option<LockRecord>| BusError::Held {
            name: name.to_string(),
            holder: r
                .as_ref()
                .map_or_else(|| "another holder".to_string(), |r| r.holder.clone()),
            since: r
                .as_ref()
                .map_or_else(|| "just now".to_string(), |r| r.acquired_at.clone()),
        };
        if held.as_ref().map(|r| r.holder.as_str()) != Some(stale) {
            return Err(held_by(&held));
        }
        let record = LockRecord {
            holder: holder.to_string(),
            acquired_at: utc_now_stamp(),
            token: lease_token(),
        };
        let value: bytes::Bytes = serde_json::to_vec(&record)
            .map_err(|e| nats(&format!("encoding lease {name}"), e))?
            .into();
        match store.update(name, value.clone(), entry.revision).await {
            Ok(revision) => Ok(Lease {
                store: store.clone(),
                name: name.to_string(),
                record,
                value,
                revision,
            }),
            Err(e) if e.kind() == kv::UpdateErrorKind::WrongLastRevision => {
                let now = live_entry(store, name)
                    .await?
                    .and_then(|e| serde_json::from_slice::<LockRecord>(&e.value).ok());
                Err(held_by(&now))
            }
            Err(e) => Err(nats(&format!("taking over lease {name}"), e)),
        }
    }

    /// How long a lease in `HGL_LOCKS` lives without a refresh: the bucket's `max_age`.
    pub async fn locks_max_age(&self) -> Result<Duration> {
        let store = self.store(&self.locks, KV_LOCKS).await?;
        let status = store
            .status()
            .await
            .map_err(|e| nats(&format!("reading bucket {KV_LOCKS}"), e))?;
        Ok(status.info.config.max_age)
    }

    /// Who holds the lease `name` now, if anyone.
    pub async fn lock_get(&self, name: &str) -> Result<Option<LockRecord>> {
        check_kv_key(name)?;
        let store = self.store(&self.locks, KV_LOCKS).await?;
        Ok(live_entry(store, name).await?.map(|e| {
            serde_json::from_slice(&e.value).unwrap_or_else(|_| LockRecord {
                holder: "another holder".into(),
                acquired_at: "an unknown time".into(),
                token: String::new(),
            })
        }))
    }

    /// The end of `task` for `seat` recorded in `HGL_ENDED`, if any (within the bucket's
    /// `max_age`).
    pub async fn ended_get(&self, seat: &Seat, task: &TaskKey) -> Result<Option<EndedRecord>> {
        let key = ended_key(seat, task);
        check_kv_key(&key)?;
        let store = self.store(&self.ended, KV_ENDED).await?;
        match live_entry(store, &key).await? {
            Some(e) => serde_json::from_slice(&e.value)
                .map(Some)
                .map_err(|e| nats(&format!("reading ended task {key}"), e)),
            None => Ok(None),
        }
    }

    /// Record in `HGL_ENDED` that `task` ended for `seat` with the note at stream sequence
    /// `seq` (compare-and-set: an end at a later sequence already recorded stands). Returns
    /// the sequence recorded now.
    pub async fn ended_put(&self, seat: &Seat, task: &TaskKey, seq: u64) -> Result<u64> {
        let key = ended_key(seat, task);
        check_kv_key(&key)?;
        let store = self.store(&self.ended, KV_ENDED).await?;
        let record = EndedRecord {
            seq,
            ended_at: utc_now_stamp(),
        };
        let value: bytes::Bytes = serde_json::to_vec(&record)
            .map_err(|e| nats(&format!("encoding ended task {key}"), e))?
            .into();
        for attempt in 0..APPEND_ATTEMPTS {
            let entry = live_entry(store, &key).await?;
            let held = entry
                .as_ref()
                .and_then(|e| serde_json::from_slice::<EndedRecord>(&e.value).ok());
            if let Some(h) = held.filter(|h| h.seq >= seq) {
                return Ok(h.seq);
            }
            let done = match &entry {
                Some(e) => store
                    .update(&key, value.clone(), e.revision)
                    .await
                    .map(drop)
                    .map_err(|e| {
                        (
                            e.kind() == kv::UpdateErrorKind::WrongLastRevision,
                            e.to_string(),
                        )
                    }),
                None => store
                    .create(&key, value.clone())
                    .await
                    .map(drop)
                    .map_err(|e| {
                        (
                            e.kind() == kv::CreateErrorKind::AlreadyExists,
                            e.to_string(),
                        )
                    }),
            };
            match done {
                Ok(()) => return Ok(seq),
                Err((true, _)) => tokio::time::sleep(backoff(attempt)).await,
                Err((false, e)) => return Err(nats(&format!("recording ended task {key}"), e)),
            }
        }
        Err(BusError::Conflict { key, expected: 0 })
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
        while out.len() < max {
            let remaining = max - out.len();
            let batch = self
                .consumer
                .fetch()
                .max_messages(remaining)
                .messages()
                .await
                .map_err(|e| nats(&format!("reading the inbox of {}", self.seat), e))?;
            if self.take(batch, &mut out).await? < remaining {
                break;
            }
        }
        Ok(out)
    }

    async fn take(&mut self, mut batch: pull::Batch, out: &mut Vec<Delivery>) -> Result<usize> {
        let mut received = 0;
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
            received += 1;
            if let Some(d) = self.accept(msg).await? {
                out.push(d);
            }
        }
        Ok(received)
    }

    /// Turn one message into a delivery, skipping old broadcasts and terminating malformed notes.
    async fn accept(&self, msg: jetstream::Message) -> Result<Option<Delivery>> {
        let info = msg
            .info()
            .map_err(|e| nats("reading message metadata", e))?;
        let seq = info.stream_sequence;
        if broadcast_before(&msg, &info, self.consumer.cached_info().created) {
            msg.double_ack()
                .await
                .map_err(|e| nats(&format!("skipping old broadcast {seq}"), e))?;
            return Ok(None);
        }
        let delivered = u64::try_from(info.delivered).unwrap_or(0);
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

/// Collect a stream reader batch. Return whether it held any messages and how many were eligible.
async fn take_read(
    mut batch: pull::Batch,
    out: &mut Vec<StreamNote>,
    last: &mut Option<u64>,
    broadcast_start: Option<OffsetDateTime>,
) -> Result<(bool, usize)> {
    let mut received = false;
    let mut n = 0;
    while let Some(m) = batch.next().await {
        let msg = m.map_err(|e| nats("reading the stream", e))?;
        received = true;
        let info = msg
            .info()
            .map_err(|e| nats("reading message metadata", e))?;
        if broadcast_start.is_some_and(|start| broadcast_before(&msg, &info, start)) {
            continue;
        }
        n += 1;
        let seq = info.stream_sequence;
        *last = Some(last.map_or(seq, |l| l.max(seq)));
        match parse_note(msg.subject.as_str(), &msg.payload) {
            Ok(note) => out.push(StreamNote { seq, note }),
            Err(why) => tracing::debug!(seq, "skipping a malformed note: {why}"),
        }
    }
    Ok((received, n))
}

fn check_inbox_seat(seat: &Seat) -> Result<()> {
    if seat.is_broadcast() {
        return Err(BusError::Nats(format!(
            "`{BROADCAST}` is the broadcast address; it has no inbox"
        )));
    }
    Ok(())
}

/// Use the hub's stored publish time and consumer creation time, independent of sender clocks.
fn broadcast_before(
    msg: &jetstream::Message,
    info: &jetstream::message::Info<'_>,
    start: OffsetDateTime,
) -> bool {
    info.published < start
        && msg.subject.as_str().rsplit_once('.') == Some((SUBJECT_PREFIX, BROADCAST))
}

/// Parse and check one inbox payload; the error says why it is rejected. The subject is
/// the one the hub stored it under, which only a publisher allowed on it can choose: a note
/// with sender `owner` is accepted only from `hgl.notes.owner.<recipient>`, and a note
/// there only with sender `owner` (OWN-5).
fn parse_note(subject: &str, payload: &[u8]) -> std::result::Result<Note, String> {
    if payload.len() > MAX_NOTE_BYTES {
        return Err(format!(
            "{} bytes, over the {MAX_NOTE_BYTES} byte limit",
            payload.len()
        ));
    }
    let note: Note = serde_json::from_slice(payload).map_err(|e| format!("not a note: {e}"))?;
    note.validate().map_err(|e| e.to_string())?;
    if subject != note.subject() {
        if note.sender.is_owner() || subject == note.recipient.owner_subject() {
            return Err(format!(
                "sender {} on {subject}: owner notes come only from the console on {}",
                note.sender,
                note.recipient.owner_subject()
            ));
        }
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
        // Bounded: a hub that does not answer must not hold the caller.
        match tokio::time::timeout(SIGNAL_WAIT, h.client.flush()).await {
            Ok(r) => r.map_err(|e| nats(&ctx(), e)),
            Err(_) => Err(BusError::Nats(format!(
                "{}: the hub did not answer within {SIGNAL_WAIT:?}",
                ctx()
            ))),
        }
    }
}

/// A lease in `HGL_LOCKS`, from [`Bus::lock_acquire`]. Dropping it releases nothing:
/// call [`Lease::release`], or the lease expires after the bucket's `max_age`.
pub struct Lease {
    store: kv::Store,
    name: String,
    record: LockRecord,
    value: bytes::Bytes,
    revision: u64,
}

impl Lease {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn record(&self) -> &LockRecord {
        &self.record
    }

    /// The revision of the lease's last write.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Write the lease again at its revision (compare-and-set), so it lives another
    /// `max_age`. Fails with [`BusError::LeaseLost`] when it expired or was taken.
    ///
    /// Cancellation-safe: a refresh dropped (a caller's timeout) or failed after the hub
    /// committed its write leaves the key a revision ahead of ours. A later refresh that
    /// finds the key moved re-reads it, and when the latest value is still ours (the same
    /// record, token included, which no other lease writes) takes that revision and
    /// writes again; only a value that is not ours, or none, is a lost lease.
    pub async fn refresh(&mut self) -> Result<()> {
        let mut adopted = false;
        loop {
            match self
                .store
                .update(&self.name, self.value.clone(), self.revision)
                .await
            {
                Ok(revision) => {
                    self.revision = revision;
                    return Ok(());
                }
                Err(e) if e.kind() == kv::UpdateErrorKind::WrongLastRevision && !adopted => {
                    // Our own write whose answer we never saw: keep the lease.
                    self.revision = self
                        .own_later_revision()
                        .await?
                        .ok_or_else(|| self.lost())?;
                    adopted = true;
                }
                Err(e) if e.kind() == kv::UpdateErrorKind::WrongLastRevision => {
                    return Err(self.lost());
                }
                Err(e) => return Err(nats(&format!("refreshing lease {}", self.name), e)),
            }
        }
    }

    /// The key's latest revision when it is later than ours and still holds our record
    /// (a write of ours whose answer we never saw).
    async fn own_later_revision(&self) -> Result<Option<u64>> {
        let ours = live_entry(&self.store, &self.name).await?.filter(|e| {
            e.revision > self.revision
                && serde_json::from_slice::<LockRecord>(&e.value).ok().as_ref()
                    == Some(&self.record)
        });
        Ok(ours.map(|e| e.revision))
    }

    fn lost(&self) -> BusError {
        BusError::LeaseLost {
            name: self.name.clone(),
        }
    }

    /// Delete the lease if it is still ours. `false`: it was no longer ours (it expired,
    /// and perhaps another holder took it), so nothing was deleted. Like
    /// [`Lease::refresh`], it takes the later revision of a write of ours whose answer it
    /// never saw, once.
    pub async fn release(self) -> Result<bool> {
        let mut revision = self.revision;
        let mut adopted = false;
        loop {
            match self
                .store
                .purge_expect_revision(&self.name, Some(revision))
                .await
            {
                Ok(()) => return Ok(true),
                Err(e) if e.kind() == kv::UpdateErrorKind::WrongLastRevision && !adopted => {
                    match self.own_later_revision().await? {
                        Some(r) => revision = r,
                        None => return Ok(false),
                    }
                    adopted = true;
                }
                Err(e) if e.kind() == kv::UpdateErrorKind::WrongLastRevision => return Ok(false),
                Err(e) => return Err(nats(&format!("releasing lease {}", self.name), e)),
            }
        }
    }
}

/// A fresh lease token: 32 hex digits, unique per acquisition. From the standard
/// library's randomly keyed hasher (seeded by the OS once per process) over the clock,
/// the process and a counter, so it needs no random device of its own.
fn lease_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let half = |salt: u64| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(salt);
        h.write_u64(n);
        h.write_u128(t);
        h.write_u32(std::process::id());
        h.finish()
    };
    format!("{:016x}{:016x}", half(0), half(1))
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
async fn list_json<T: DeserializeOwned>(bus: &Bus, store: &kv::Store) -> Result<Vec<T>> {
    list_json_where(bus, store, |_| true).await
}

/// [`list_json`] of the keys `keep` accepts.
async fn list_json_where<T: DeserializeOwned>(
    bus: &Bus,
    store: &kv::Store,
    keep: impl Fn(&str) -> bool,
) -> Result<Vec<T>> {
    let names = keys(bus, store, keep).await?;
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
    fn owner_notes_only_on_the_owner_subject() {
        let mut n = note("claude");
        n.sender = Seat::owner();
        n.kind = Kind::Answer;
        let bytes = serde_json::to_vec(&n).unwrap();
        assert_eq!(
            parse_note("hgl.notes.owner.claude", &bytes).unwrap().sender,
            Seat::owner()
        );
        let err = parse_note("hgl.notes.claude", &bytes).unwrap_err();
        assert!(err.contains("only from the console"), "{err}");
        let agent = serde_json::to_vec(&note("claude")).unwrap();
        let err = parse_note("hgl.notes.owner.claude", &agent).unwrap_err();
        assert!(err.contains("only from the console"), "{err}");
        let err = parse_note("hgl.notes.owner.w2", &bytes).unwrap_err();
        assert!(err.contains("only from the console"), "{err}");
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
