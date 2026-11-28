//! An in-memory bus for the loop tests: per-seat queues, redelivery after `nak`, leases,
//! and a record of every ack, nak and in-progress.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hgl_ai_bus::{BusError, Result};
use hgl_ai_proto::{HostRecord, Note, Seat, TaskKey, TreeRecord, ended_key};
use hgl_ai_tree::Registry;
use tokio::sync::Notify;

use crate::bus::{BusApi, DeliveryApi, InboxApi, LeaseApi};
use crate::disk::DiskProbe;

/// JetStream's `max_deliver` of an inbox (docs/protocol.md §3): unlimited.
const MAX_DELIVER: u64 = u64::MAX;

#[derive(Debug, Clone, PartialEq)]
pub enum Ev {
    Ack,
    Nak(Duration),
    Term,
    Progress,
}

/// (seq, delivery count, note)
type Queued = (u64, u64, Note);

#[derive(Default)]
struct Inner {
    queues: Mutex<HashMap<Seat, VecDeque<Queued>>>,
    notify: Notify,
    sent: Mutex<Vec<Note>>,
    state: Mutex<HashMap<String, String>>,
    hosts: Mutex<Vec<HostRecord>>,
    deleted_hosts: Mutex<Vec<String>>,
    trees: Mutex<BTreeMap<String, TreeRecord>>,
    events: Mutex<Vec<(u64, Ev)>>,
    seq: AtomicU64,
    /// name -> (holder, revision)
    leases: Mutex<HashMap<String, (String, u64)>>,
    /// name -> when its holder's last write (take or refresh) was confirmed.
    confirmed: Mutex<HashMap<String, std::time::Instant>>,
    revision: AtomicU64,
    /// `HGL_ENDED`: key -> seq
    ended: Mutex<HashMap<String, u64>>,
    /// Make the `HGL_ENDED` reads, or writes, fail.
    ended_reads_down: AtomicBool,
    ended_writes_down: AtomicBool,
    /// The next inbox fetch panics.
    inbox_broken: AtomicBool,
    /// Lease refreshes and takes fail as if the hub did not answer.
    refresh_down: AtomicBool,
    /// Lease takes refused while `refresh_down`.
    takes_refused: AtomicU64,
    /// `HGL_LOCKS` max_age in milliseconds; 0 is the protocol's.
    lease_max_age_ms: AtomicU64,
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

    /// Deliver again a note sent earlier as `seq` (JetStream redelivering it).
    pub fn redeliver(&self, to: &str, seq: u64, note: Note) {
        self.enqueue(Seat::new(to).unwrap(), seq, 2, note);
    }

    /// Who holds the lease `name`.
    pub fn lease_holder(&self, name: &str) -> Option<String> {
        g(&self.0.leases).get(name).map(|(h, _)| h.clone())
    }

    /// When the last write of the lease `name` by its holder was confirmed: the lease expires
    /// `max_age` after it unless refreshed.
    pub fn lease_confirmed_at(&self, name: &str) -> Option<std::time::Instant> {
        g(&self.0.confirmed).get(name).copied()
    }

    /// The lease `name` goes (its holder released it, or it expired).
    pub fn drop_lease(&self, name: &str) {
        g(&self.0.leases).remove(name);
    }

    /// Another holder takes the lease `name` (as after its expiry): ours is lost.
    pub fn steal_lease(&self, name: &str, holder: &str) {
        let rev = self.0.revision.fetch_add(1, Ordering::SeqCst) + 1;
        g(&self.0.leases).insert(name.into(), (holder.into(), rev));
    }

    /// Take the lease `name` when it is free, or held by `stale`; under one guard of the
    /// leases, which `refresh_down` is set under: refused while the hub does not answer.
    fn take_lease(&self, name: &str, holder: &str, stale: Option<&str>) -> Result<FakeLease> {
        let mut leases = g(&self.0.leases);
        if self.0.refresh_down.load(Ordering::SeqCst) {
            self.0.takes_refused.fetch_add(1, Ordering::SeqCst);
            return Err(no_responders());
        }
        if let Some((h, _)) = leases.get(name)
            && Some(h.as_str()) != stale
        {
            return Err(BusError::Held {
                name: name.into(),
                holder: h.clone(),
                since: "earlier".into(),
            });
        }
        let rev = self.0.revision.fetch_add(1, Ordering::SeqCst) + 1;
        leases.insert(name.into(), (holder.into(), rev));
        g(&self.0.confirmed).insert(name.into(), std::time::Instant::now());
        Ok(FakeLease {
            bus: self.clone(),
            name: name.into(),
            rev,
        })
    }

    fn enqueue(&self, seat: Seat, seq: u64, delivered: u64, note: Note) {
        g(&self.0.queues)
            .entry(seat)
            .or_default()
            .push_back((seq, delivered, note));
        self.0.notify.notify_waiters();
    }

    pub fn events(&self, seq: u64) -> Vec<Ev> {
        g(&self.0.events)
            .iter()
            .filter(|(s, _)| *s == seq)
            .map(|(_, e)| e.clone())
            .filter(|e| *e != Ev::Progress)
            .collect()
    }

    pub fn progressed(&self, seq: u64) -> bool {
        g(&self.0.events)
            .iter()
            .any(|(s, e)| *s == seq && *e == Ev::Progress)
    }

    pub fn sent(&self) -> Vec<Note> {
        g(&self.0.sent).clone()
    }

    pub fn sent_containing(&self, text: &str) -> Vec<Note> {
        self.sent()
            .into_iter()
            .filter(|n| n.body.contains(text))
            .collect()
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

    /// Another host's record, as its bridge would write it.
    pub fn put_host(&self, rec: HostRecord) {
        g(&self.0.hosts).push(rec);
    }

    pub fn deleted_hosts(&self) -> Vec<String> {
        g(&self.0.deleted_hosts).clone()
    }

    /// The end recorded in `HGL_ENDED` for `seat` and `repo#issue`.
    pub fn ended(&self, seat: &str, repo: &str, issue: u64) -> Option<u64> {
        let task = TaskKey {
            repo: repo.into(),
            issue,
        };
        g(&self.0.ended)
            .get(&ended_key(&Seat::new(seat).unwrap(), &task))
            .copied()
    }

    /// `HGL_ENDED` reads, and writes, fail (`true`) or work.
    pub fn ended_down(&self, reads: bool, writes: bool) {
        self.0.ended_reads_down.store(reads, Ordering::SeqCst);
        self.0.ended_writes_down.store(writes, Ordering::SeqCst);
    }

    /// Lease refreshes and takes fail without an answer (`true`), as in a partition, or
    /// work. Set under the leases' lock, which a refresh or take checks it under: once this
    /// returns none started before it can still succeed.
    pub fn refresh_down(&self, down: bool) {
        let _leases = g(&self.0.leases);
        self.0.refresh_down.store(down, Ordering::SeqCst);
    }

    /// How many lease takes were refused while the hub did not answer.
    pub fn lease_takes_refused(&self) -> u64 {
        self.0.takes_refused.load(Ordering::SeqCst)
    }

    /// The `max_age` of `HGL_LOCKS` the bridge reads.
    pub fn set_lease_max_age(&self, d: Duration) {
        self.0
            .lease_max_age_ms
            .store(d.as_millis() as u64, Ordering::SeqCst);
    }

    /// Make the next inbox fetch panic, ending the bridge's fetcher.
    pub fn break_inbox(&self) {
        self.0.inbox_broken.store(true, Ordering::SeqCst);
    }

    fn ended_up(&self, flag: &AtomicBool) -> Result<()> {
        if flag.load(Ordering::SeqCst) {
            return Err(BusError::Nats("HGL_ENDED: no responders".into()));
        }
        Ok(())
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
    type Lease = FakeLease;

    async fn inbox(&self, seat: &Seat) -> Result<FakeInbox> {
        Ok(FakeInbox {
            bus: self.clone(),
            seat: seat.clone(),
        })
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
    async fn host_del(&self, host: &str) -> Result<()> {
        g(&self.0.deleted_hosts).push(host.to_string());
        Ok(())
    }
    async fn bridge_host(&self, seat: &Seat) -> Result<Option<String>> {
        let hosts = g(&self.0.hosts);
        let mut latest: BTreeMap<&str, &HostRecord> = BTreeMap::new();
        for h in hosts.iter() {
            latest.insert(&h.host, h);
        }
        Ok(latest
            .values()
            .find(|h| h.seats.iter().any(|s| &s.seat == seat))
            .map(|h| h.host.clone()))
    }
    async fn lease_max_age(&self) -> Result<Duration> {
        Ok(match self.0.lease_max_age_ms.load(Ordering::SeqCst) {
            0 => hgl_ai_bus::LOCKS_MAX_AGE,
            ms => Duration::from_millis(ms),
        })
    }
    async fn hosts(&self) -> Result<Vec<HostRecord>> {
        let hosts = g(&self.0.hosts);
        let mut latest: BTreeMap<String, HostRecord> = BTreeMap::new();
        for h in hosts.iter() {
            latest.insert(h.host.clone(), h.clone());
        }
        Ok(latest.into_values().collect())
    }
    async fn lock_acquire(&self, name: &str, holder: &str) -> Result<FakeLease> {
        self.take_lease(name, holder, None)
    }
    async fn lock_take_over(&self, name: &str, holder: &str, stale: &str) -> Result<FakeLease> {
        self.take_lease(name, holder, Some(stale))
    }
    async fn ended_get(&self, seat: &Seat, task: &TaskKey) -> Result<Option<u64>> {
        self.ended_up(&self.0.ended_reads_down)?;
        Ok(g(&self.0.ended).get(&ended_key(seat, task)).copied())
    }
    async fn ended_put(&self, seat: &Seat, task: &TaskKey, seq: u64) -> Result<()> {
        self.ended_up(&self.0.ended_writes_down)?;
        let mut e = g(&self.0.ended);
        let v = e.entry(ended_key(seat, task)).or_insert(seq);
        *v = (*v).max(seq);
        Ok(())
    }
}

fn no_responders() -> BusError {
    BusError::Nats("HGL_LOCKS: no responders".into())
}

pub struct FakeLease {
    bus: FakeBus,
    name: String,
    rev: u64,
}

impl LeaseApi for FakeLease {
    async fn refresh(&mut self) -> Result<()> {
        let mut leases = g(&self.bus.0.leases);
        if self.bus.0.refresh_down.load(Ordering::SeqCst) {
            return Err(no_responders());
        }
        match leases.get_mut(&self.name) {
            Some((_, rev)) if *rev == self.rev => {
                *rev = self.bus.0.revision.fetch_add(1, Ordering::SeqCst) + 1;
                self.rev = *rev;
                g(&self.bus.0.confirmed).insert(self.name.clone(), std::time::Instant::now());
                Ok(())
            }
            _ => Err(BusError::LeaseLost {
                name: self.name.clone(),
            }),
        }
    }
    async fn release(self) -> Result<bool> {
        let mut leases = g(&self.bus.0.leases);
        if leases
            .get(&self.name)
            .is_some_and(|(_, rev)| *rev == self.rev)
        {
            leases.remove(&self.name);
            return Ok(true);
        }
        Ok(false)
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
            assert!(
                !self.bus.0.inbox_broken.swap(false, Ordering::SeqCst),
                "the inbox broke"
            );
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
                        .map(|(seq, delivered, note)| FakeDelivery {
                            bus: self.bus.clone(),
                            seat: self.seat.clone(),
                            seq,
                            delivered,
                            note,
                        })
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
            let (bus, seat, seq, n, note) = (
                self.bus.clone(),
                self.seat.clone(),
                self.seq,
                self.delivered + 1,
                self.note.clone(),
            );
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                bus.enqueue(seat, seq, n, note);
            });
        }
        Ok(())
    }
    async fn term(&self) -> Result<()> {
        self.record(Ev::Term);
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
