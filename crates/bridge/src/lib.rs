//! The hgl-ai bridge daemon (docs/protocol.md §2, §3, §5, §6; docs/design.md §4).
//!
//! Per served seat: hold the seat lock and the hub's seat lease, fetch the inbox, and per
//! note either create the task's tree and run one headless agent turn in it (`assign`,
//! `review`, and every other note), or remove the task's trees and send a cleanup receipt
//! (`landed`, `closed`). One turn runs per seat at a time, and one per task directory,
//! enforced by the directory's turn lock. Beside the seats: a heartbeat (`HGL_HOSTS`, and
//! the seat leases' refresh) and a daily report of stale trees and disk use.

pub mod adapter;
pub mod bus;
pub mod disk;
mod ended;
pub mod lock;
pub mod proc;
pub mod prompt;
mod service;
mod turn;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hgl_ai_bus::BusError;
use hgl_ai_config::{Binding, Config, SeatConfig};
use hgl_ai_proto::{Kind, Role, Runtime, Seat, TaskKey, seat_lock_key};
use hgl_ai_tree::{IsClosed, Trees};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;

pub use bus::{BusApi, DeliveryApi, InboxApi, LeaseApi};
pub use disk::{DiskProbe, Statvfs};
pub use hgl_ai_cap::Method as CapMethod;

use crate::ended::EndedTasks;
use crate::lock::FileLock;
use crate::proc::{Stop, TreeSlot};
use crate::turn::{Outcome, Target, TurnOut};

/// The controller's seat (ROLE-2).
pub const CONTROLLER: &str = "claude";

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error(transparent)]
    Bus(#[from] hgl_ai_bus::BusError),
    #[error(transparent)]
    Tree(#[from] hgl_ai_tree::TreeError),
    #[error(transparent)]
    Config(#[from] hgl_ai_config::ConfigError),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, BridgeError>;

/// Timings and limits; [`Options::from_config`] gives the daemon's.
#[derive(Debug, Clone)]
pub struct Options {
    pub fetch_max: usize,
    pub fetch_wait: Duration,
    /// PROTO-8.
    pub nak_delay: Duration,
    /// Retry of a task whose turn lock another process holds.
    pub busy_retry: Duration,
    /// Also how often held `assign` notes are looked at again.
    pub in_progress_every: Duration,
    /// Also the refresh of the seat leases.
    pub heartbeat_every: Duration,
    pub report_every: Duration,
    /// Retry of a seat whose seat lock or lease another process holds.
    pub seat_retry: Duration,
    pub turn_timeout: Duration,
    pub shutdown_grace: Duration,
    pub kill_grace: Duration,
    /// Longest line of agent output kept.
    pub max_line: usize,
    /// Turn logs kept per task directory.
    pub keep_logs: usize,
    pub bins: BTreeMap<Runtime, PathBuf>,
    /// How turns are capped; `None` detects it (`hgl_ai_cap::Method::detect`).
    pub cap_method: Option<CapMethod>,
}

impl Options {
    pub fn from_config(cfg: &Config) -> Self {
        Options {
            fetch_max: 8,
            fetch_wait: Duration::from_secs(30),
            nak_delay: Duration::from_secs(15),
            busy_retry: Duration::from_secs(30),
            in_progress_every: Duration::from_secs(60),
            heartbeat_every: Duration::from_secs(30),
            report_every: Duration::from_secs(86400),
            seat_retry: Duration::from_secs(30),
            turn_timeout: cfg.turn_timeout(),
            shutdown_grace: Duration::from_secs(30),
            kill_grace: Duration::from_secs(10),
            max_line: 16 << 20,
            keep_logs: 10,
            bins: Runtime::ALL
                .iter()
                .map(|r| (*r, PathBuf::from(r.as_str())))
                .collect(),
            cap_method: None,
        }
    }
}

/// What the bridge needs from outside: GitHub's closed-ness and free space.
pub struct Deps {
    pub closed: Arc<dyn IsClosed + Send + Sync>,
    pub disk: Arc<dyn DiskProbe>,
}

/// `assign` gate state (ROLE-16, ROLE-17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Open,
    Drain,
    LowDisk,
}

pub struct Bridge<B: BusApi> {
    cfg: Config,
    bus: B,
    trees: Arc<Trees>,
    deps: Deps,
    opts: Options,
    gate: Mutex<Gate>,
    /// Seats this bridge serves now (it holds their seat lock and lease).
    served: Mutex<BTreeMap<Seat, Arc<SeatRt>>>,
    /// The seat leases held (PROTO-6), refreshed with the heartbeat.
    leases: tokio::sync::Mutex<BTreeMap<Seat, HeldLease<B::Lease>>>,
    turns: AtomicU64,
}

struct HeldLease<L> {
    lease: L,
    seat: Arc<SeatRt>,
}

/// One seat at run time: its configuration and current binding.
struct SeatRt {
    cfg: SeatConfig,
    /// 0 is the primary binding, `i` is `fallback[i - 1]` (PROTO-17).
    binding: Mutex<usize>,
    exhausted_told: AtomicBool,
    busy_told: Mutex<HashSet<Target>>,
    /// Raised when a refresh finds the seat lease no longer ours.
    lost: watch::Sender<bool>,
    ended: Mutex<EndedTasks>,
}

impl SeatRt {
    fn new(cfg: SeatConfig, ended: EndedTasks) -> Self {
        SeatRt {
            cfg,
            binding: Mutex::new(0),
            exhausted_told: AtomicBool::new(false),
            busy_told: Mutex::default(),
            lost: watch::Sender::new(false),
            ended: Mutex::new(ended),
        }
    }
    fn name(&self) -> &Seat {
        &self.cfg.name
    }
    fn binding_at(&self, i: usize) -> Binding {
        if i == 0 {
            self.cfg.binding.clone()
        } else {
            self.cfg.fallback[i - 1].clone()
        }
    }
    fn current(&self) -> (usize, Binding) {
        let i = *lock(&self.binding);
        (i, self.binding_at(i))
    }
}

/// How a seat loop ended.
struct LoopEnd {
    /// The seat lease was lost; whether a turn was running then.
    lost: Option<bool>,
}

/// A turn in flight in a seat loop.
struct Running<D> {
    target: Target,
    deliveries: Arc<Mutex<Vec<Arc<D>>>>,
    stop: watch::Sender<Stop>,
    handle: JoinHandle<TurnOut>,
    slot: TreeSlot,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl<B: BusApi> Bridge<B> {
    pub fn new(cfg: Config, bus: B, trees: Trees, deps: Deps, opts: Options) -> Arc<Self> {
        Arc::new(Bridge {
            cfg,
            bus,
            trees: Arc::new(trees),
            deps,
            opts,
            gate: Mutex::new(Gate::Open),
            served: Mutex::default(),
            leases: tokio::sync::Mutex::default(),
            turns: AtomicU64::new(0),
        })
    }

    pub fn bus(&self) -> &B {
        &self.bus
    }

    /// A value of `HGL_AI_TURN` no other turn of any bridge has.
    fn next_marker(&self, seat: &Seat) -> String {
        let n = self.turns.fetch_add(1, Ordering::Relaxed);
        format!("{}-{seat}-{}-{n}", self.cfg.host, std::process::id())
    }

    /// The seats to serve: the named ones, or every configured seat but controllers
    /// (a controller is interactive, PROTO-6, unless named).
    pub fn select(&self, only: &[Seat]) -> Result<Vec<SeatConfig>> {
        if only.is_empty() {
            return Ok(self
                .cfg
                .seats
                .iter()
                .filter(|s| s.role != Role::Controller)
                .cloned()
                .collect());
        }
        only.iter()
            .map(|n| {
                self.cfg.seat(n.as_str()).cloned().ok_or_else(|| {
                    BridgeError::Invalid(format!("no seat {n} in the configuration"))
                })
            })
            .collect()
    }

    /// Serve `only` (or the default seats) until `shutdown` turns true.
    pub async fn run(
        self: Arc<Self>,
        only: &[Seat],
        shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let seats = self.select(only)?;
        if seats.is_empty() {
            return Err(BridgeError::Invalid(
                "no seats to serve; add one with `hgl-ai seat add`".into(),
            ));
        }
        tracing::info!(host = %self.cfg.host, seats = ?seats.iter().map(|s| s.name.to_string()).collect::<Vec<_>>(), "bridge starting");
        let data = self.cfg.data_dir()?;
        let mut set = tokio::task::JoinSet::new();
        for s in seats {
            let ended = EndedTasks::load(
                data.join("bridge")
                    .join("ended")
                    .join(format!("{}.json", s.name)),
            );
            set.spawn(
                self.clone()
                    .seat_main(Arc::new(SeatRt::new(s, ended)), shutdown.clone()),
            );
        }
        set.spawn(self.clone().heartbeat_loop(shutdown.clone()));
        set.spawn(self.clone().report_loop(shutdown.clone()));
        while let Some(r) = set.join_next().await {
            if let Err(e) = r {
                tracing::error!("bridge task failed: {e}");
            }
        }
        // No seat is served any more: let an interactive session take one at once.
        if let Err(e) = self.bus.host_del(&self.cfg.host).await {
            tracing::warn!("removing the host record: {e}");
        }
        tracing::info!("bridge stopped");
        Ok(())
    }

    /// Serve the seat while this bridge holds it (its seat lock and lease, [`Self::take_seat`]);
    /// after a lost lease, wait to take it again.
    async fn seat_main(self: Arc<Self>, seat: Arc<SeatRt>, shutdown: watch::Receiver<bool>) {
        let path = match self.cfg.seat_lock_path(seat.name()) {
            Ok(p) => p,
            Err(e) => return tracing::error!(seat = %seat.name(), "seat lock path: {e}"),
        };
        let mut down_rx = shutdown.clone();
        loop {
            let Some(held) = self.take_seat(&seat, &path, &mut down_rx).await else {
                return;
            };
            // Advertise the seat before taking notes, so `note wait` refuses it (PROTO-6).
            lock(&self.served).insert(seat.name().clone(), seat.clone());
            self.beat().await;
            let ended = self.clone().seat_loop(&seat, shutdown.clone()).await;
            lock(&self.served).remove(seat.name());
            self.beat().await;
            // Released once the host record no longer lists the seat.
            let lease = self.leases.lock().await.remove(seat.name());
            if let Some(l) = lease {
                match l.lease.release().await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(seat = %seat.name(), "the seat lease was no longer ours at release")
                    }
                    Err(e) => {
                        tracing::warn!(seat = %seat.name(), "releasing the seat lease: {e}; it expires on its own")
                    }
                }
            }
            drop(held);
            let Some(stopped) = ended.lost else { return };
            let body = format!(
                "[bridge] {}/{}: the seat lease {} is no longer ours (it expired or another bridge took it); stopped serving the seat{}, naked its notes, and wait to take it again.",
                self.cfg.host,
                seat.name(),
                seat_lock_key(seat.name()),
                if stopped {
                    ", stopped its running turn"
                } else {
                    ""
                }
            );
            self.tell(seat.name(), Kind::Note, None, body).await;
        }
    }

    /// Take the seat: the seat lock on this host, then the hub lease `seat.<seat>` with no
    /// other host's bridge advertising the seat (PROTO-6). Waits, saying why once, while
    /// another holder has either; `None` at shutdown.
    async fn take_seat(
        &self,
        seat: &Arc<SeatRt>,
        path: &std::path::Path,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Option<FileLock> {
        let key = seat_lock_key(seat.name());
        let holder = format!("{} pid {} (bridge)", self.cfg.host, std::process::id());
        let mut told: Option<String> = None;
        loop {
            if *shutdown.borrow() {
                return None;
            }
            let why = match FileLock::try_exclusive(path) {
                Ok(Some(l)) => match self.acquire_seat_lease(&key, &holder).await {
                    Ok(lease) => match self.bus.bridge_host(seat.name()).await {
                        Ok(Some(h)) if h != self.cfg.host => {
                            if let Err(e) = lease.release().await {
                                tracing::warn!(seat = %seat.name(), "releasing the seat lease: {e}");
                            }
                            format!("the bridge of host {h} still advertises this seat")
                        }
                        _ => {
                            seat.lost.send_replace(false);
                            self.leases.lock().await.insert(
                                seat.name().clone(),
                                HeldLease {
                                    lease,
                                    seat: seat.clone(),
                                },
                            );
                            if told.is_some() {
                                tracing::info!(seat = %seat.name(), "seat lock and lease taken; serving");
                            }
                            return Some(l);
                        }
                    },
                    Err(BusError::Held { holder, since, .. }) => {
                        format!("the seat lease {key} is held by {holder} since {since}")
                    }
                    Err(e) => format!("taking the seat lease {key}: {e}"),
                },
                Ok(None) => format!(
                    "seat is served elsewhere on this host (holder {:?}; interactive session?)",
                    lock::holder(path)
                ),
                Err(e) => format!("seat lock {}: {e}", path.display()),
            };
            if told.as_deref() != Some(why.as_str()) {
                tracing::warn!(seat = %seat.name(), "{why}; waiting");
                told = Some(why);
            }
            tokio::select! {
                _ = tokio::time::sleep(self.opts.seat_retry) => {}
                _ = down(shutdown) => return None,
            }
        }
    }

    /// Take the seat lease, under the seat lock. A lease that names a bridge of this host
    /// whose process is gone (a crash; or this process, after a refresh whose answer was
    /// lost) is taken over at once rather than left to expire: no live bridge of this host
    /// can serve the seat without the seat lock held here.
    async fn acquire_seat_lease(&self, key: &str, holder: &str) -> hgl_ai_bus::Result<B::Lease> {
        match self.bus.lock_acquire(key, holder).await {
            Err(BusError::Held { holder: h, .. }) if self.dead_own_bridge(&h) => {
                tracing::warn!(lease = key, stale = %h, "taking over the seat lease of a bridge of this host that is gone");
                self.bus.lock_take_over(key, holder, &h).await
            }
            r => r,
        }
    }

    /// `<host> pid <pid> (bridge)` of this host, with `pid` this process or a dead one.
    fn dead_own_bridge(&self, holder: &str) -> bool {
        let pid = holder
            .strip_prefix(&format!("{} pid ", self.cfg.host))
            .and_then(|r| r.strip_suffix(" (bridge)"))
            .and_then(|p| p.parse::<u32>().ok());
        match pid {
            Some(p) if p == std::process::id() => true,
            Some(p) => !proc_alive(p),
            None => false,
        }
    }

    /// Refresh every held seat lease (with the heartbeat); a lease no longer ours stops
    /// its seat.
    pub(crate) async fn refresh_leases(&self) {
        let mut leases = self.leases.lock().await;
        let mut lost = Vec::new();
        for (seat, held) in leases.iter_mut() {
            match held.lease.refresh().await {
                Ok(()) => {}
                Err(BusError::LeaseLost { .. }) => lost.push(seat.clone()),
                Err(e) => tracing::warn!(%seat, "refreshing the seat lease: {e}; trying again"),
            }
        }
        for seat in lost {
            if let Some(held) = leases.remove(&seat) {
                tracing::error!(%seat, "the seat lease is no longer ours; stopping the seat");
                held.seat.lost.send_replace(true);
            }
        }
    }

    /// Serve until shutdown or a lost lease; the notes held at the end are naked.
    async fn seat_loop(
        self: Arc<Self>,
        seat: &Arc<SeatRt>,
        mut shutdown: watch::Receiver<bool>,
    ) -> LoopEnd {
        let mut lost = seat.lost.subscribe();
        let mut end = LoopEnd { lost: None };
        let (tx, mut rx) = mpsc::channel::<B::Delivery>(64);
        let fetcher = tokio::spawn(self.clone().fetch_loop(
            seat.name().clone(),
            tx,
            shutdown.clone(),
        ));
        let mut pending: Vec<Arc<B::Delivery>> = Vec::new();
        let mut running: Option<Running<B::Delivery>> = None;
        let mut busy: HashMap<Target, Instant> = HashMap::new();
        let mut stopping = false;
        let every = self.opts.in_progress_every;
        let mut tick = tokio::time::interval_at(Instant::now() + every, every);
        loop {
            if !stopping && running.is_none() {
                running = self.start_next(seat, &mut pending, &busy).await;
            }
            if stopping && running.is_none() {
                break;
            }
            let next_busy = busy.values().min().copied();
            tokio::select! {
                _ = down(&mut shutdown), if !stopping => {
                    stopping = true;
                    if let Some(r) = &running {
                        let _ = r.stop.send(Stop::Graceful(self.opts.shutdown_grace));
                    }
                }
                _ = down(&mut lost), if !stopping => {
                    // Another bridge may serve the seat now: start nothing more, stop the
                    // running turn at once, hand every note back.
                    stopping = true;
                    end.lost = Some(running.is_some());
                    if let Some(r) = &running {
                        let _ = r.stop.send(Stop::Graceful(Duration::ZERO));
                    }
                }
                d = rx.recv(), if !stopping => match d {
                    Some(d) => self.on_delivery(seat, Arc::new(d), &mut pending, &mut running, &mut busy).await,
                    None => stopping = true,
                },
                out = join(&mut running) => {
                    let r = running.take().expect("a running turn");
                    self.finish(seat, r, out, &mut pending, &mut busy).await;
                }
                _ = tick.tick() => self.keep_alive(&pending, running.as_ref()).await,
                _ = tokio::time::sleep_until(next_busy.unwrap_or_else(Instant::now)), if next_busy.is_some() && running.is_none() => {
                    let now = Instant::now();
                    busy.retain(|_, until| *until > now);
                }
            }
        }
        fetcher.abort();
        let _ = fetcher.await;
        while let Ok(d) = rx.try_recv() {
            pending.push(Arc::new(d));
        }
        for d in pending {
            let _ = d.nak(self.opts.nak_delay).await;
        }
        end
    }

    async fn fetch_loop(
        self: Arc<Self>,
        seat: Seat,
        tx: mpsc::Sender<B::Delivery>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let pause = Duration::from_secs(5);
        let mut inbox = loop {
            match self.bus.inbox(&seat).await {
                Ok(i) => break i,
                Err(e) => tracing::warn!(%seat, "opening the inbox: {e}"),
            }
            tokio::select! {
                _ = tokio::time::sleep(pause) => {}
                _ = down(&mut shutdown) => return,
            }
        };
        loop {
            let got = tokio::select! {
                r = inbox.fetch(self.opts.fetch_max, self.opts.fetch_wait) => r,
                _ = down(&mut shutdown) => return,
            };
            match got {
                Ok(ds) => {
                    for d in ds {
                        if tx.send(d).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(%seat, "fetching: {e}");
                    tokio::select! {
                        _ = tokio::time::sleep(pause) => {}
                        _ = down(&mut shutdown) => return,
                    }
                }
            }
        }
    }

    async fn on_delivery(
        &self,
        seat: &Arc<SeatRt>,
        d: Arc<B::Delivery>,
        pending: &mut Vec<Arc<B::Delivery>>,
        running: &mut Option<Running<B::Delivery>>,
        busy: &mut HashMap<Target, Instant>,
    ) {
        let seq = d.seq();
        // A redelivery of a note already held: keep the newest handle, run it once.
        if let Some(p) = pending.iter_mut().find(|p| p.seq() == seq) {
            *p = d;
            return;
        }
        if let Some(r) = running.as_ref() {
            let mut ds = lock(&r.deliveries);
            if ds.iter().any(|x| x.seq() == seq) {
                ds.push(d);
                return;
            }
        }
        tracing::info!(seat = %seat.name(), seq, kind = %d.note().kind, from = %d.note().sender, "note");
        if let Some(task) = d.note().task()
            && lock(&seat.ended).moot(&task, seq)
        {
            // Sent before its task ended, delivered after (hgl-ai#2): moot.
            tracing::info!(seat = %seat.name(), seq, kind = %d.note().kind, %task, "a note of an ended task sent before its end; acked");
            let _ = d.ack().await;
            return;
        }
        if d.note().kind.ends_task() {
            self.end_task(seat, d, pending, running, busy).await;
        } else {
            pending.push(d);
        }
    }

    /// `landed` / `closed` (PROTO-14): stop the task's turn, drop its queued notes, remove
    /// the seat's trees of it, send a cleanup receipt; no agent turn.
    async fn end_task(
        &self,
        seat: &Arc<SeatRt>,
        d: Arc<B::Delivery>,
        pending: &mut Vec<Arc<B::Delivery>>,
        running: &mut Option<Running<B::Delivery>>,
        busy: &mut HashMap<Target, Instant>,
    ) {
        let note = d.note().clone();
        let Some(task) = note.task() else {
            tracing::warn!(
                seq = d.seq(),
                "a `{}` note without a task; acked",
                note.kind
            );
            let _ = d.ack().await;
            return;
        };
        let mut stopped = false;
        if let Some(r) = running.as_mut()
            && r.target.task() == Some(&task)
        {
            let _ = r.stop.send(Stop::Now);
            let out = (&mut r.handle).await;
            let r = running.take().expect("checked");
            self.finish(seat, r, out, pending, busy).await;
            stopped = true;
        }
        // Notes of the task still queued (the seat was busy elsewhere) would recreate its
        // tree and run a turn after the removal: they are moot. Notes sent after this one
        // (a later seq) are newer intent and stay.
        let (moot, rest): (Vec<_>, Vec<_>) = std::mem::take(pending)
            .into_iter()
            .partition(|p| p.seq() < d.seq() && p.note().task().as_ref() == Some(&task));
        *pending = rest;
        for p in moot {
            tracing::info!(seat = %seat.name(), seq = p.seq(), kind = %p.note().kind, %task, "a queued note of an ended task; acked");
            let _ = p.ack().await;
        }
        busy.retain(|t, _| t.task() != Some(&task));
        lock(&seat.busy_told).retain(|t| t.task() != Some(&task));
        let mut removed = Vec::new();
        let mut errors = Vec::new();
        for review in [false, true] {
            if let Some(dir) = self
                .trees
                .layout()
                .task_dir(seat.name(), &task, review)
                .ok()
                .filter(|p| p.exists())
            {
                // A process that still holds the turn lock is a stray of an earlier turn.
                let lp = dir.join(lock::TURN_LOCK);
                if lp.exists()
                    && matches!(FileLock::turn_patiently(&lp).await, Ok(None))
                    && !turn::reap_lock_holders(&lp, self.opts.kill_grace).await
                {
                    errors.push(format!("a process still held {}", lp.display()));
                }
            }
            let trees = self.trees.clone();
            let (s, t) = (seat.name().clone(), task.clone());
            match tokio::task::spawn_blocking(move || trees.remove(&s, &t, review)).await {
                Ok(Ok(Some(rm))) => {
                    if let Err(e) = self.bus.tree_del(&rm.key).await {
                        tracing::warn!(key = %rm.key, "deregistering: {e}");
                    }
                    removed.push(rm);
                }
                Ok(Ok(None)) => {}
                Ok(Err(e)) => errors.push(e.to_string()),
                Err(e) => errors.push(format!("removal panicked: {e}")),
            }
        }
        if let Err(e) = lock(&seat.ended).end(task.clone(), d.seq()) {
            tracing::warn!(seat = %seat.name(), %task, "recording the end of the task: {e}");
        }
        if !removed.is_empty() || stopped || !errors.is_empty() {
            let body = service::cleanup_body(
                &self.cfg.host,
                seat.name(),
                &note,
                &task,
                &removed,
                stopped,
                &errors,
                self.trees.layout().root(),
            );
            self.tell(seat.name(), Kind::Done, Some(&task), body).await;
        }
        let _ = d.ack().await;
    }

    /// Start the next turn from `pending`: the first group of notes sharing a target that
    /// is not busy. While the `assign` gate is closed (ROLE-16, ROLE-17), an `assign` that
    /// would start a new task stays held in `pending`, kept alive, until the gate opens.
    async fn start_next(
        self: &Arc<Self>,
        seat: &Arc<SeatRt>,
        pending: &mut Vec<Arc<B::Delivery>>,
        busy: &HashMap<Target, Instant>,
    ) -> Option<Running<B::Delivery>> {
        let now = Instant::now();
        let plans: Vec<turn::Plan> = pending.iter().map(|d| self.plan(seat, d.note())).collect();
        let mut gate = None;
        let mut held = Vec::with_capacity(plans.len());
        for (d, p) in pending.iter().zip(&plans) {
            let new_task = matches!(&p.target, Target::Tree { task, review: false } if self.trees.existing(seat.name(), task, false).is_none());
            let gated = if d.note().kind == Kind::Assign && new_task {
                let g = match gate {
                    Some(g) => g,
                    None => *gate.insert(self.gate_check().await),
                };
                g != Gate::Open
            } else {
                false
            };
            held.push(gated);
        }
        let idx = (0..plans.len())
            .find(|&i| !held[i] && busy.get(&plans[i].target).is_none_or(|u| *u <= now))?;
        let target = plans[idx].target.clone();
        let mut group = Vec::new();
        let mut creates = Vec::new();
        let mut rest = Vec::new();
        for ((d, p), h) in pending.drain(..).zip(plans).zip(held) {
            if p.target == target && !h {
                group.push(d);
                creates.push(p.create);
            } else {
                rest.push(d);
            }
        }
        *pending = rest;
        let create = turn::choose_create(creates);
        let hold = self.bus.state_get("hold").await.ok().flatten();
        let mode = self.bus.state_get("mode").await.ok().flatten();
        let (binding_idx, _) = seat.current();
        let notes = group
            .iter()
            .map(|d| (d.seq(), d.delivered(), d.note().clone()))
            .collect();
        let (stop_tx, stop_rx) = watch::channel(Stop::None);
        let slot = TreeSlot::default();
        let job = turn::Job {
            seat: seat.clone(),
            target: target.clone(),
            create,
            notes,
            hold,
            mode,
            binding_idx,
            stop: stop_rx,
            slot: slot.clone(),
        };
        let handle = tokio::spawn(self.clone().turn(job));
        Some(Running {
            target,
            deliveries: Arc::new(Mutex::new(group)),
            stop: stop_tx,
            handle,
            slot,
        })
    }

    async fn finish(
        &self,
        seat: &Arc<SeatRt>,
        r: Running<B::Delivery>,
        out: std::result::Result<TurnOut, JoinError>,
        pending: &mut Vec<Arc<B::Delivery>>,
        busy: &mut HashMap<Target, Instant>,
    ) {
        let ds: Vec<Arc<B::Delivery>> = lock(&r.deliveries).clone();
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                // The turn task panicked or was cancelled: kill its tree, then nak.
                let tree = lock(&r.slot).clone();
                if let Some(t) = tree {
                    proc::reap(&t, self.opts.kill_grace, Duration::from_secs(10)).await;
                }
                tracing::error!(seat = %seat.name(), "turn task failed: {e}");
                return nak_all(&ds, self.opts.nak_delay).await;
            }
        };
        let seqs: Vec<u64> = ds.iter().map(|d| d.seq()).collect();
        tracing::info!(seat = %seat.name(), ?seqs, target = %r.target, outcome = ?out.outcome, "turn ended");
        if !matches!(out.outcome, Outcome::Busy(_)) {
            lock(&seat.busy_told).remove(&r.target);
        }
        match out.outcome {
            Outcome::Clean => {
                seat.exhausted_told.store(false, Ordering::Relaxed);
                for d in ds {
                    let _ = d.ack().await;
                }
            }
            Outcome::Stopped => {
                // Its task ended (`landed`, `closed`); the notes are moot.
                for d in ds {
                    let _ = d.ack().await;
                }
            }
            Outcome::Quota(msg) => {
                nak_all(&ds, self.opts.nak_delay).await;
                self.failover(seat, out.binding_idx, &msg).await;
            }
            Outcome::PrepFailed(msg) => {
                nak_all(&ds, self.opts.nak_delay).await;
                let body = format!(
                    "[bridge] {}/{}: could not prepare {} for a turn: {msg}; notes naked.",
                    self.cfg.host,
                    seat.name(),
                    r.target
                );
                self.tell(seat.name(), Kind::Note, r.target.task(), body)
                    .await;
            }
            Outcome::Failed(_) | Outcome::Shutdown => nak_all(&ds, self.opts.nak_delay).await,
            Outcome::Busy(who) => {
                // Another process holds the task's turn lock: wait, do not start a second turn.
                busy.insert(r.target.clone(), Instant::now() + self.opts.busy_retry);
                pending.splice(0..0, ds);
                if lock(&seat.busy_told).insert(r.target.clone()) {
                    let body = format!(
                        "[bridge] {}/{}: another process ({who}) still holds the turn lock of {}; its notes wait.",
                        self.cfg.host,
                        seat.name(),
                        r.target
                    );
                    self.tell(seat.name(), Kind::Note, r.target.task(), body)
                        .await;
                }
            }
        }
    }

    /// PROTO-17: move the seat to its next binding after a quota refusal on `used`.
    async fn failover(&self, seat: &Arc<SeatRt>, used: usize, msg: &str) {
        let moved = {
            let mut b = lock(&seat.binding);
            if *b != used {
                return;
            }
            if used < seat.cfg.fallback.len() {
                *b = used + 1;
                true
            } else {
                false
            }
        };
        let from = self.describe(&seat.binding_at(used));
        let msg: String = msg.chars().take(200).collect();
        if moved {
            let to = self.describe(&seat.binding_at(used + 1));
            tracing::warn!(seat = %seat.name(), %from, %to, "quota refusal; failing over");
            let body = format!(
                "[bridge] Failover: seat {} on {} moved from {from} to {to} after a quota refusal ({msg}). Later turns use {to}; ROLE-9 applies to the new family.",
                seat.name(),
                self.cfg.host
            );
            self.tell(seat.name(), Kind::Note, None, body).await;
        } else if !seat.exhausted_told.swap(true, Ordering::Relaxed) {
            let body = format!(
                "[bridge] Quota: seat {} on {} was refused on {from} ({msg}) and has no fallback left; its notes are naked and retried.",
                seat.name(),
                self.cfg.host
            );
            self.tell(seat.name(), Kind::Note, None, body).await;
        }
    }

    /// `runtime family/model`.
    fn describe(&self, b: &Binding) -> String {
        match self.cfg.resolve(b) {
            Ok(r) => format!("{} {}/{}", r.runtime, r.family, r.model),
            Err(e) => format!("{} ({e})", b.runtime),
        }
    }

    async fn keep_alive(
        &self,
        pending: &[Arc<B::Delivery>],
        running: Option<&Running<B::Delivery>>,
    ) {
        let mut all: Vec<Arc<B::Delivery>> = pending.to_vec();
        if let Some(r) = running {
            all.extend(lock(&r.deliveries).iter().cloned());
        }
        for d in all {
            if let Err(e) = d.in_progress().await {
                tracing::warn!(seq = d.seq(), "in_progress: {e}");
            }
        }
    }

    /// Send a note to the controller; failures are logged.
    async fn tell(&self, from: &Seat, kind: Kind, task: Option<&TaskKey>, body: String) {
        let Ok(to) = Seat::new(CONTROLLER) else {
            return;
        };
        let mut n = hgl_ai_proto::Note::new(from.clone(), to, kind, body);
        if let Some(t) = task {
            n.repo = Some(t.repo.clone());
            n.issue = t.issue;
        }
        if let Err(e) = self.bus.send(&n).await {
            tracing::warn!(kind = %kind, "sending to {CONTROLLER}: {e}");
        }
    }
}

/// Whether process `pid` exists on this host (`EPERM`: it exists, someone else's).
#[cfg(unix)]
fn proc_alive(pid: u32) -> bool {
    let Ok(p) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 checks existence only.
    let r = unsafe { libc::kill(p, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn proc_alive(_: u32) -> bool {
    true
}

/// Resolves once `shutdown` is true (the guard of `wait_for` is not held past it).
pub(crate) async fn down(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|d| *d).await;
}

async fn nak_all<D: DeliveryApi>(ds: &[Arc<D>], delay: Duration) {
    for d in ds {
        let _ = d.nak(delay).await;
    }
}

async fn join<D>(r: &mut Option<Running<D>>) -> std::result::Result<TurnOut, JoinError> {
    match r {
        Some(r) => (&mut r.handle).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;
