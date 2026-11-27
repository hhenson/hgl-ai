//! An in-memory bus for the loop tests: per-seat queues, redelivery after `nak`, and a
//! record of every ack, nak and in-progress.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hgl_ai_bus::{BusError, Result};
use hgl_ai_proto::{HostRecord, Note, Seat, TreeRecord};
use hgl_ai_tree::Registry;
use tokio::sync::Notify;

use crate::bus::{BusApi, DeliveryApi, InboxApi};
use crate::disk::DiskProbe;

/// JetStream's `max_deliver` of an inbox (docs/protocol.md §3).
const MAX_DELIVER: u64 = 5;

#[derive(Debug, Clone, PartialEq)]
pub enum Ev {
    Ack,
    Nak(Duration),
    Progress,
}

#[derive(Default)]
struct Inner {
    queues: Mutex<HashMap<Seat, VecDeque<(u64, u64, Note)>>>,
    notify: Notify,
    sent: Mutex<Vec<Note>>,
    state: Mutex<HashMap<String, String>>,
    hosts: Mutex<Vec<HostRecord>>,
    trees: Mutex<BTreeMap<String, TreeRecord>>,
    events: Mutex<Vec<(u64, Ev)>>,
    seq: AtomicU64,
}

#[derive(Clone, Default)]
pub struct FakeBus(Arc<Inner>);

fn g<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap()
}

impl FakeBus {
    /// Queue `note` for `to`; returns its stream sequence.
    pub fn push(&self, to: &str, note: Note) -> u64 {
        note.validate().expect("a valid note");
        let seq = self.0.seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.enqueue(Seat::new(to).unwrap(), seq, 1, note);
        seq
    }

    fn enqueue(&self, seat: Seat, seq: u64, delivered: u64, note: Note) {
        g(&self.0.queues).entry(seat).or_default().push_back((seq, delivered, note));
        self.0.notify.notify_waiters();
    }

    pub fn events(&self, seq: u64) -> Vec<Ev> {
        g(&self.0.events).iter().filter(|(s, _)| *s == seq).map(|(_, e)| e.clone()).filter(|e| *e != Ev::Progress).collect()
    }

    pub fn progressed(&self, seq: u64) -> bool {
        g(&self.0.events).iter().any(|(s, e)| *s == seq && *e == Ev::Progress)
    }

    pub fn sent(&self) -> Vec<Note> {
        g(&self.0.sent).clone()
    }

    pub fn sent_containing(&self, text: &str) -> Vec<Note> {
        self.sent().into_iter().filter(|n| n.body.contains(text)).collect()
    }

    pub fn set_state(&self, k: &str, v: &str) {
        g(&self.0.state).insert(k.into(), v.into());
    }

    pub fn trees(&self) -> BTreeMap<String, TreeRecord> {
        g(&self.0.trees).clone()
    }

    pub fn last_host(&self) -> Option<HostRecord> {
        g(&self.0.hosts).last().cloned()
    }
}

impl Registry for FakeBus {
    async fn tree_put(&self, rec: &TreeRecord) -> Result<()> {
        g(&self.0.trees).insert(rec.key(), rec.clone());
        Ok(())
    }
    async fn tree_del(&self, key: &str) -> Result<()> {
        g(&self.0.trees).remove(key);
        Ok(())
    }
    async fn trees(&self) -> Result<Vec<TreeRecord>> {
        Ok(g(&self.0.trees).values().cloned().collect())
    }
}

impl BusApi for FakeBus {
    type Inbox = FakeInbox;
    type Delivery = FakeDelivery;

    async fn inbox(&self, seat: &Seat) -> Result<FakeInbox> {
        Ok(FakeInbox { bus: self.clone(), seat: seat.clone() })
    }
    async fn send(&self, note: &Note) -> Result<()> {
        note.validate().map_err(BusError::Proto)?;
        g(&self.0.sent).push(note.clone());
        Ok(())
    }
    async fn state_get(&self, key: &str) -> Result<Option<String>> {
        Ok(g(&self.0.state).get(key).cloned())
    }
    async fn host_put(&self, rec: &HostRecord) -> Result<()> {
        g(&self.0.hosts).push(rec.clone());
        Ok(())
    }
}

pub struct FakeInbox {
    bus: FakeBus,
    seat: Seat,
}

impl InboxApi for FakeInbox {
    type Delivery = FakeDelivery;
    async fn fetch(&mut self, max: usize, wait: Duration) -> Result<Vec<FakeDelivery>> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.bus.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut q = g(&self.bus.0.queues);
                let q = q.entry(self.seat.clone()).or_default();
                if !q.is_empty() {
                    let n = q.len().min(max);
                    return Ok(q
                        .drain(..n)
                        .map(|(seq, delivered, note)| FakeDelivery { bus: self.bus.clone(), seat: self.seat.clone(), seq, delivered, note })
                        .collect());
                }
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep_until(deadline) => return Ok(Vec::new()),
            }
        }
    }
}

pub struct FakeDelivery {
    bus: FakeBus,
    seat: Seat,
    seq: u64,
    delivered: u64,
    note: Note,
}

impl FakeDelivery {
    fn record(&self, e: Ev) {
        g(&self.bus.0.events).push((self.seq, e));
    }
}

impl DeliveryApi for FakeDelivery {
    fn note(&self) -> &Note {
        &self.note
    }
    fn seq(&self) -> u64 {
        self.seq
    }
    fn delivered(&self) -> u64 {
        self.delivered
    }
    async fn ack(&self) -> Result<()> {
        self.record(Ev::Ack);
        Ok(())
    }
    async fn nak(&self, delay: Duration) -> Result<()> {
        self.record(Ev::Nak(delay));
        if self.delivered < MAX_DELIVER {
            let (bus, seat, seq, n, note) = (self.bus.clone(), self.seat.clone(), self.seq, self.delivered + 1, self.note.clone());
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                bus.enqueue(seat, seq, n, note);
            });
        }
        Ok(())
    }
    async fn in_progress(&self) -> Result<()> {
        self.record(Ev::Progress);
        Ok(())
    }
}

/// A disk with a settable amount of free space.
pub struct FixedDisk(pub AtomicU64);

impl DiskProbe for FixedDisk {
    fn free_bytes(&self, _: &Path) -> std::io::Result<u64> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}
