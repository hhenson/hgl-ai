//! NATS JetStream access (docs/protocol.md §3, §8, §9). The public API below is the
//! contract other crates build against; bodies marked `todo!` are filled by the bus work.

use std::time::Duration;

use hgl_ai_config::Config;
use hgl_ai_proto::{HostRecord, Note, Seat, TreeRecord};

#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("hub unreachable at {url}: {reason}")]
    Unreachable { url: String, reason: String },
    #[error("reading the hub token {path}: {source}")]
    Token { path: String, source: std::io::Error },
    #[error(transparent)]
    Proto(#[from] hgl_ai_proto::ProtoError),
    #[error(transparent)]
    Config(#[from] hgl_ai_config::ConfigError),
    #[error("{0}")]
    Nats(String),
}

pub type Result<T> = std::result::Result<T, BusError>;

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

/// A connection to the hub.
pub struct Bus {
    _private: (),
}

impl Bus {
    /// Connect with the host configuration (`hub.url`, `hub.token_file`).
    pub async fn connect(cfg: &Config) -> Result<Bus> {
        let _ = cfg;
        todo!("bus work")
    }

    /// Connect to `url`, with an optional token.
    pub async fn connect_url(url: &str, token: Option<String>) -> Result<Bus> {
        let _ = (url, token);
        todo!("bus work")
    }

    /// Create the stream and buckets, or check they match (idempotent).
    pub async fn provision(&self) -> Result<()> {
        todo!("bus work")
    }

    /// Validate and publish a note to `hgl.notes.<recipient>` with its message id.
    pub async fn send(&self, note: &Note) -> Result<SendReceipt> {
        let _ = note;
        todo!("bus work")
    }

    /// The durable inbox of `seat`, created on first use.
    pub async fn inbox(&self, seat: &Seat) -> Result<Inbox> {
        let _ = seat;
        todo!("bus work")
    }

    pub async fn state_get(&self, key: &str) -> Result<Option<StateEntry>> {
        let _ = key;
        todo!("bus work")
    }

    /// Put `value`; an empty value deletes the key (§8). Returns the new revision (0 on delete).
    pub async fn state_put(&self, key: &str, value: &str) -> Result<u64> {
        let _ = (key, value);
        todo!("bus work")
    }

    pub async fn state_del(&self, key: &str) -> Result<()> {
        let _ = key;
        todo!("bus work")
    }

    pub async fn host_put(&self, rec: &HostRecord) -> Result<()> {
        let _ = rec;
        todo!("bus work")
    }

    pub async fn hosts(&self) -> Result<Vec<HostRecord>> {
        todo!("bus work")
    }

    pub async fn tree_put(&self, rec: &TreeRecord) -> Result<()> {
        let _ = rec;
        todo!("bus work")
    }

    pub async fn tree_del(&self, key: &str) -> Result<()> {
        let _ = key;
        todo!("bus work")
    }

    pub async fn trees(&self) -> Result<Vec<TreeRecord>> {
        todo!("bus work")
    }
}

/// A seat's durable inbox consumer (`inbox-<seat>`).
pub struct Inbox {
    _private: (),
}

impl Inbox {
    /// Wait up to `wait` for at least one note; return at most `max`.
    /// A malformed message is terminated and logged, never returned.
    pub async fn fetch(&mut self, max: usize, wait: Duration) -> Result<Vec<Delivery>> {
        let _ = (max, wait);
        todo!("bus work")
    }
}

/// One delivered note. The receiver must `ack`, `nak` or let it time out.
pub struct Delivery {
    pub seq: u64,
    /// How many times JetStream has delivered it, this one included.
    pub delivered: u64,
    pub note: Note,
}

impl Delivery {
    pub async fn ack(&self) -> Result<()> {
        todo!("bus work")
    }
    pub async fn nak(&self, delay: Duration) -> Result<()> {
        let _ = delay;
        todo!("bus work")
    }
    pub async fn in_progress(&self) -> Result<()> {
        todo!("bus work")
    }
}
