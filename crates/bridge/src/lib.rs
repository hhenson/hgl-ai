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
mod cleanup;
pub mod disk;
mod ended;
mod failures;
pub mod holder;
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
use hgl_ai_proto::{Family, Kind, Note, Role, Runtime, Seat, TaskKey, seat_lock_key};
use hgl_ai_tree::{Create, IsClosed, Trees};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;

pub use bus::{BusApi, DeliveryApi, InboxApi, LeaseApi};
pub use disk::{DiskProbe, Statvfs};
pub use hgl_ai_cap::Method as CapMethod;

use crate::cleanup::Cleanup;
use crate::ended::EndedTasks;
use crate::failures::Failures;
use crate::lock::FileLock;
use crate::proc::{Stop, TreeSlot};
use crate::turn::{Outcome, Target, TurnOut};

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
    /// PROTO-8: the delay of a nak after a real failure (a crash, a memory or time limit).
    pub nak_delay: Duration,
    /// Real failures of a note before it is dead-lettered to the controller (PROTO-8).
    pub max_failures: u32,
    /// First retry of a tree that could not be prepared; it doubles, to 16 times this.
    pub prep_retry: Duration,
    /// Retry of a seat refused for quota with no binding left to move to.
    pub quota_retry: Duration,
    /// Retry of a note whose end the hub could not answer or record (`HGL_ENDED`).
    pub hub_retry: Duration,
    /// How long a seat stays on a fallback binding before it tries its primary again.
    pub failover_cooldown: Duration,
    /// Retry of a tree whose removal on `landed` or `closed` failed.
    pub cleanup_retry: Duration,
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
            max_failures: 5,
            prep_retry: Duration::from_secs(60),
            quota_retry: Duration::from_secs(600),
            hub_retry: Duration::from_secs(15),
            failover_cooldown: cfg.failover_cooldown(),
            cleanup_retry: Duration::from_secs(300),
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

/// `assign` gate state (ROLE-18, ROLE-19).
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
    /// This host's id in lease holder strings ([`holder`]).
    host_id: String,
    /// How the seat leases are kept, from the hub's `max_age` ([`LeaseTiming`]).
    lease_timing: Mutex<LeaseTiming>,
}

/// The seat leases' refresh, from `HGL_LOCKS`' `max_age` (PROTO-6): written every `every`
/// (the heartbeat, or a sixth of `max_age` when that is shorter), each attempt bounded by
/// `every`. A lease not refreshed for `deadline` (half of `max_age`) stops its seat, so the
/// seat has stopped before the lease can expire and another host take it.
#[derive(Debug, Clone, Copy)]
struct LeaseTiming {
    every: Duration,
    deadline: Duration,
}

impl LeaseTiming {
    fn of(max_age: Duration, heartbeat: Duration) -> LeaseTiming {
        LeaseTiming {
            every: heartbeat.min(max_age / 6).max(Duration::from_millis(50)),
            deadline: max_age / 2,
        }
    }
}

/// Turns started by this process, for [`Bridge::next_marker`]: counted per process, not per
/// bridge, so two bridges of one process (tests) never share a marker and reap each other.
static TURNS: AtomicU64 = AtomicU64::new(0);

struct HeldLease<L> {
    lease: L,
    seat: Arc<SeatRt>,
    /// The last write of the lease that the hub confirmed.
    ok_at: Instant,
}

/// One seat at run time: its configuration and current binding.
struct SeatRt {
    cfg: SeatConfig,
    /// 0 is the primary binding, `i` is `fallback[i - 1]` (PROTO-17).
    binding: Mutex<usize>,
    exhausted_told: AtomicBool,
    busy_told: Mutex<HashSet<Target>>,
    /// Targets whose failed preparation the controller was told of (once until it works).
    prep_told: Mutex<HashSet<Target>>,
    /// Raised when a refresh finds the seat lease no longer ours, or could not refresh it
    /// within the deadline; why, in `lost_why`.
    lost: watch::Sender<bool>,
    lost_why: Mutex<String>,
    ended: Mutex<EndedTasks>,
    failures: Mutex<Failures>,
    /// Tree removals that failed, tried again until they work.
    cleanup: Mutex<Cleanup>,
    /// No turn starts before this: a quota refusal with no binding left (PROTO-17).
    quota_until: Mutex<Option<Instant>>,
    /// When the seat last moved to a fallback binding; it returns to the primary after the
    /// cool-down.
    failed_over_at: Mutex<Option<Instant>>,
}

impl SeatRt {
    fn new(cfg: SeatConfig, ended: EndedTasks, failures: Failures, cleanup: Cleanup) -> Self {
        SeatRt {
            lost_why: Mutex::default(),
            cleanup: Mutex::new(cleanup),
            cfg,
            binding: Mutex::new(0),
            exhausted_told: AtomicBool::new(false),
            busy_told: Mutex::default(),
            prep_told: Mutex::default(),
            lost: watch::Sender::new(false),
            ended: Mutex::new(ended),
            failures: Mutex::new(failures),
            quota_until: Mutex::new(None),
            failed_over_at: Mutex::new(None),
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

/// What a seat loop holds between deliveries.
struct SeatState<D> {
    /// Notes waiting for a turn: queued behind another target, held by a gate, a quota
    /// refusal or a failed preparation (kept alive with `in_progress`, PROTO-8).
    pending: Vec<Arc<D>>,
    running: Option<Running<D>>,
    /// Targets not started before the time: a turn lock held elsewhere, a tree that could
    /// not be prepared.
    busy: HashMap<Target, Instant>,
    /// Notes whose end the hub could not answer or record, looked at again then.
    deferred: Vec<(Arc<D>, Instant)>,
    /// Failed preparations in a row, per target: the retry backs off.
    prep_failures: HashMap<Target, u32>,
}

impl<D> SeatState<D> {
    fn new() -> Self {
        SeatState {
            pending: Vec::new(),
            running: None,
            busy: HashMap::new(),
            deferred: Vec::new(),
            prep_failures: HashMap::new(),
        }
    }
}

/// A turn in flight in a seat loop.
struct Running<D> {
    target: Target,
    /// The stream sequence of the note that made the tree the turn runs in (`task.json`
    /// `seq`, or this turn's creating note); `None` when unknown.
    start: Option<u64>,
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
        let host_id = match cfg.data_dir() {
            Ok(d) => holder::host_id(&d),
            Err(e) => {
                tracing::warn!("no data directory for the host id: {e}");
                format!("{:016x}", std::process::id())
            }
        };
        let lease_timing = Mutex::new(LeaseTiming::of(
            hgl_ai_bus::LOCKS_MAX_AGE,
            opts.heartbeat_every,
        ));
        Arc::new(Bridge {
            host_id,
            lease_timing,
            cfg,
            bus,
            trees: Arc::new(trees),
            deps,
            opts,
            gate: Mutex::new(Gate::Open),
            served: Mutex::default(),
            leases: tokio::sync::Mutex::default(),
        })
    }

    pub fn bus(&self) -> &B {
        &self.bus
    }

    /// A value of `HGL_AI_TURN` no other turn of any bridge has.
    fn next_marker(&self, seat: &Seat) -> String {
        let n = TURNS.fetch_add(1, Ordering::Relaxed);
        format!("{}-{seat}-{}-{n}", self.cfg.host, std::process::id())
    }

    /// The seats to serve: the named ones, or every configured seat but interactive
    /// controllers (PROTO-6); a controller with `headless = true` is served (OWN-7).
    pub fn select(&self, only: &[Seat]) -> Result<Vec<SeatConfig>> {
        if only.is_empty() {
            return Ok(self
                .cfg
                .seats
                .iter()
                .filter(|s| s.role != Role::Controller || s.headless)
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
        if self.opts.cap_method.unwrap_or_else(CapMethod::detect) == CapMethod::Scope {
            // Turn slices a bridge of this host left when it died.
            let gone = tokio::task::spawn_blocking(hgl_ai_cap::remove_stale_turn_slices)
                .await
                .unwrap_or_default();
            if !gone.is_empty() {
                tracing::warn!(slices = ?gone, "removed turn slices of bridges that are gone");
            }
        }
        let data = self.cfg.data_dir()?;
        match self.bus.lease_max_age().await {
            Ok(max_age) => {
                let t = LeaseTiming::of(max_age, self.opts.heartbeat_every);
                tracing::info!(?max_age, refresh = ?t.every, deadline = ?t.deadline, "seat leases");
                *lock(&self.lease_timing) = t;
            }
            Err(e) => tracing::warn!(
                "reading the leases' max_age: {e}; assuming {:?}",
                hgl_ai_bus::LOCKS_MAX_AGE
            ),
        }
        let mut set = tokio::task::JoinSet::new();
        for s in seats {
            let file = format!("{}.json", s.name);
            let ended = EndedTasks::load(data.join("bridge").join("ended").join(&file));
            let failures = Failures::load(data.join("bridge").join("failures").join(&file));
            let cleanup = Cleanup::load(data.join("bridge").join("cleanup").join(&file));
            set.spawn(self.clone().seat_main(
                Arc::new(SeatRt::new(s, ended, failures, cleanup)),
                shutdown.clone(),
            ));
        }
        set.spawn(self.clone().heartbeat_loop(shutdown.clone()));
        set.spawn(self.clone().lease_loop(shutdown.clone()));
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
            let why = lock(&seat.lost_why).clone();
            let body = format!(
                "[bridge] {}/{}: the seat lease {} {why}; stopped serving the seat{}, naked its notes, and wait to take it again.",
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
        let holder = holder::holder(&self.cfg.host, &self.host_id, "bridge");
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
                                    ok_at: Instant::now(),
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

    /// Take the seat lease, under the seat lock. A lease that names a holder of this host
    /// (its label and id, [`holder`]) whose process is gone (a crashed bridge or `note wait`;
    /// or this process, after a refresh whose answer was lost) is taken over at once rather
    /// than left to expire.
    async fn acquire_seat_lease(&self, key: &str, holder: &str) -> hgl_ai_bus::Result<B::Lease> {
        match self.bus.lock_acquire(key, holder).await {
            Err(BusError::Held { holder: h, .. })
                if holder::dead_own(&h, &self.cfg.host, &self.host_id) =>
            {
                tracing::warn!(lease = key, stale = %h, "taking over the seat lease of a holder of this host that is gone");
                self.bus.lock_take_over(key, holder, &h).await
            }
            r => r,
        }
    }

    /// Refresh every held seat lease (with the heartbeat). A lease no longer ours stops its
    /// seat; so does one the hub has not confirmed for the deadline ([`LeaseTiming`]): in a
    /// partition no answer says it is lost, and it expires on its own while the seat would
    /// still run.
    pub(crate) async fn refresh_leases(&self) {
        let t = *lock(&self.lease_timing);
        let mut leases = self.leases.lock().await;
        let mut lost = Vec::new();
        for (seat, held) in leases.iter_mut() {
            let r = tokio::time::timeout(t.every, held.lease.refresh()).await;
            let failed = match r {
                Ok(Ok(())) => {
                    held.ok_at = Instant::now();
                    continue;
                }
                Ok(Err(BusError::LeaseLost { .. })) => {
                    lost.push((
                        seat.clone(),
                        "is no longer ours (it expired or another bridge took it)".to_string(),
                    ));
                    continue;
                }
                Ok(Err(e)) => e.to_string(),
                Err(_) => format!("no answer within {:?}", t.every),
            };
            let since = held.ok_at.elapsed();
            if since >= t.deadline {
                lost.push((
                    seat.clone(),
                    format!(
                        "was not refreshed for {since:?} ({failed}), so it could expire and another host take the seat"
                    ),
                ));
            } else {
                tracing::warn!(%seat, "refreshing the seat lease: {failed}; trying again (unrefreshed for {since:?} of {:?})", t.deadline);
            }
        }
        for (seat, why) in lost {
            if let Some(held) = leases.remove(&seat) {
                tracing::error!(%seat, "the seat lease {why}; stopping the seat");
                *lock(&held.seat.lost_why) = why;
                held.seat.lost.send_replace(true);
            }
        }
    }

    /// How often the seat leases are refreshed.
    pub(crate) fn lease_every(&self) -> Duration {
        lock(&self.lease_timing).every
    }

    /// Serve until shutdown or a lost lease; the notes held at the end are naked. This loop
    /// stops the inbox fetcher when it starts stopping: the fetcher does not watch the
    /// shutdown, so its channel cannot close first and leave a running turn unstopped.
    async fn seat_loop(
        self: Arc<Self>,
        seat: &Arc<SeatRt>,
        mut shutdown: watch::Receiver<bool>,
    ) -> LoopEnd {
        let mut lost = seat.lost.subscribe();
        let mut end = LoopEnd { lost: None };
        let (tx, mut rx) = mpsc::channel::<B::Delivery>(64);
        let fetcher = tokio::spawn(self.clone().fetch_loop(seat.name().clone(), tx));
        let mut st: SeatState<B::Delivery> = SeatState::new();
        let mut stopping = false;
        // Removals owed from before (a restart) are tried at once.
        let mut cleanup_at = Instant::now();
        let every = self.opts.in_progress_every;
        let mut tick = tokio::time::interval_at(Instant::now() + every, every);
        loop {
            if !stopping && st.running.is_none() {
                st.running = self.start_next(seat, &mut st).await;
            }
            if stopping && st.running.is_none() {
                break;
            }
            // When an idle seat may start something it holds: a target no longer busy, the
            // end of a quota hold.
            let wake = if st.running.is_none() && !st.pending.is_empty() {
                let quota = *lock(&seat.quota_until);
                st.busy.values().copied().chain(quota).min()
            } else {
                None
            };
            let due = st.deferred.iter().map(|(_, t)| *t).min();
            let owed = !lock(&seat.cleanup).is_empty();
            tokio::select! {
                _ = down(&mut shutdown), if !stopping => {
                    stopping = true;
                    fetcher.abort();
                    stop_turn(&st.running, Stop::Graceful(self.opts.shutdown_grace));
                }
                _ = down(&mut lost), if !stopping => {
                    // Another bridge may serve the seat now: start nothing more, stop the
                    // running turn at once, hand every note back.
                    stopping = true;
                    fetcher.abort();
                    end.lost = Some(st.running.is_some());
                    stop_turn(&st.running, Stop::Graceful(Duration::ZERO));
                }
                d = rx.recv(), if !stopping => match d {
                    Some(d) => self.on_delivery(seat, Arc::new(d), &mut st).await,
                    None => {
                        // The fetcher failed: stop as at shutdown.
                        tracing::error!(seat = %seat.name(), "the inbox fetcher ended; stopping the seat");
                        stopping = true;
                        stop_turn(&st.running, Stop::Graceful(self.opts.shutdown_grace));
                    }
                },
                out = join(&mut st.running) => {
                    let r = st.running.take().expect("a running turn");
                    self.finish(seat, r, out, &mut st, None).await;
                }
                _ = tick.tick() => self.keep_alive(&st).await,
                _ = tokio::time::sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {
                    let now = Instant::now();
                    st.busy.retain(|_, until| *until > now);
                }
                _ = tokio::time::sleep_until(due.unwrap_or_else(Instant::now)), if due.is_some() && !stopping => {
                    let now = Instant::now();
                    let (ready, later): (Vec<_>, Vec<_>) = std::mem::take(&mut st.deferred)
                        .into_iter()
                        .partition(|(_, at)| *at <= now);
                    st.deferred = later;
                    for (d, _) in ready {
                        self.on_delivery(seat, d, &mut st).await;
                    }
                }
                _ = tokio::time::sleep_until(cleanup_at), if owed && !stopping => {
                    self.retry_cleanup(seat, &mut st).await;
                    cleanup_at = Instant::now() + self.opts.cleanup_retry;
                }
            }
        }
        fetcher.abort();
        let _ = fetcher.await;
        while let Ok(d) = rx.try_recv() {
            st.pending.push(Arc::new(d));
        }
        let held = st
            .pending
            .drain(..)
            .chain(st.deferred.drain(..).map(|(d, _)| d));
        for d in held {
            let _ = d.nak(self.opts.nak_delay).await;
        }
        end
    }

    /// Fetch the seat's inbox into `tx` until the seat loop aborts it (or drops the receiver).
    /// It does not watch the shutdown itself: ending on its own, it would close the channel
    /// and race the seat loop's own shutdown branch, which stops the running turn.
    async fn fetch_loop(self: Arc<Self>, seat: Seat, tx: mpsc::Sender<B::Delivery>) {
        let pause = Duration::from_secs(5);
        let mut inbox = loop {
            match self.bus.inbox(&seat).await {
                Ok(i) => break i,
                Err(e) => tracing::warn!(%seat, "opening the inbox: {e}"),
            }
            tokio::time::sleep(pause).await;
        };
        loop {
            match inbox.fetch(self.opts.fetch_max, self.opts.fetch_wait).await {
                Ok(ds) => {
                    for d in ds {
                        if tx.send(d).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(%seat, "fetching: {e}");
                    tokio::time::sleep(pause).await;
                }
            }
        }
    }

    async fn on_delivery(
        &self,
        seat: &Arc<SeatRt>,
        d: Arc<B::Delivery>,
        st: &mut SeatState<B::Delivery>,
    ) {
        let seq = d.seq();
        // A redelivery of a note already held: keep the newest handle, run it once.
        if let Some(p) = st.pending.iter_mut().find(|p| p.seq() == seq) {
            *p = d;
            return;
        }
        if let Some((p, _)) = st.deferred.iter_mut().find(|(p, _)| p.seq() == seq) {
            *p = d;
            return;
        }
        if let Some(r) = st.running.as_ref() {
            let mut ds = lock(&r.deliveries);
            if ds.iter().any(|x| x.seq() == seq) {
                ds.push(d);
                return;
            }
        }
        tracing::info!(seat = %seat.name(), seq, kind = %d.note().kind, from = %d.note().sender, "note");
        if d.note().kind == Kind::Build {
            // Only builders take `build` jobs (docs/build.md BUILD-6, hgl-ai#15): no turn.
            self.refuse_build(seat.name(), &d).await;
            return;
        }
        // Every note passes here before it can start a turn.
        if let Some(task) = d.note().task() {
            match self.known_end(seat, &task, seq).await {
                Ok(Some(end)) if seq < end => {
                    // Sent before its task ended, delivered after (hgl-ai#2): moot.
                    tracing::info!(seat = %seat.name(), seq, kind = %d.note().kind, %task, "a note of an ended task sent before its end; acked");
                    let _ = d.ack().await;
                    lock(&seat.failures).clear(seq);
                    return;
                }
                Ok(Some(end)) if seq == end && d.note().kind.ends_task() => {
                    // The end it made is recorded: a redelivery after a lost ack. Whatever
                    // runs in the task now is a restart, newer than this note.
                    tracing::info!(seat = %seat.name(), seq, kind = %d.note().kind, %task, "an ending note whose end is recorded already; acked");
                    let _ = d.ack().await;
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    // Without the hub's answer the note may be moot: no turn; held, kept
                    // alive, and looked at again (no delivery spent, PROTO-8).
                    tracing::warn!(seat = %seat.name(), seq, %task, "reading {}: {e}; held, looked at again in {:?}", hgl_ai_proto::KV_ENDED, self.opts.hub_retry);
                    st.deferred.push((d, Instant::now() + self.opts.hub_retry));
                    return;
                }
            }
        }
        if d.note().kind.ends_task() {
            self.end_task(seat, d, st).await;
        } else {
            st.pending.push(d);
        }
    }

    /// The end of `task` for the seat that rules on its note at stream sequence `seq`: this
    /// host's cache of its own ends when that makes the note moot, else the hub's
    /// `HGL_ENDED`, the authority, so a seat moved from another host knows the ends seen there.
    async fn known_end(
        &self,
        seat: &SeatRt,
        task: &TaskKey,
        seq: u64,
    ) -> std::result::Result<Option<u64>, BusError> {
        let cached = lock(&seat.ended).get(task);
        if cached.is_some_and(|end| seq < end) {
            return Ok(cached);
        }
        self.bus.ended_get(seat.name(), task).await
    }

    /// `landed` / `closed` (PROTO-14): stop the task's turn, drop its queued notes, remove
    /// the seat's trees of it, record the end on the hub, send a cleanup receipt; no agent
    /// turn. The note is acked once the end is recorded, else held and done again later. A
    /// tree made for a note sent after this one (a restart of the task; this note redelivered
    /// before its end was recorded) is not this note's: it and its turn are left alone.
    async fn end_task(
        &self,
        seat: &Arc<SeatRt>,
        d: Arc<B::Delivery>,
        st: &mut SeatState<B::Delivery>,
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
        let end = d.seq();
        let ends = |start: Option<u64>| start.is_none_or(|s| s < end);
        let mut stopped = false;
        let mut kept = None;
        if let Some(r) = st.running.as_mut()
            && r.target.task() == Some(&task)
        {
            if ends(r.start) {
                let _ = r.stop.send(Stop::Now);
                let out = (&mut r.handle).await;
                let r = st.running.take().expect("checked");
                self.finish(seat, r, out, st, Some(end)).await;
                stopped = true;
            } else {
                kept = Some(r.target.clone());
            }
        }
        // Notes of the task still queued (the seat was busy elsewhere) would recreate its
        // tree and run a turn after the removal: they are moot. Notes sent after this one
        // (a later seq) are newer intent and stay.
        let (moot, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut st.pending)
            .into_iter()
            .partition(|p| p.seq() < d.seq() && p.note().task().as_ref() == Some(&task));
        st.pending = rest;
        for p in moot {
            tracing::info!(seat = %seat.name(), seq = p.seq(), kind = %p.note().kind, %task, "a queued note of an ended task; acked");
            let _ = p.ack().await;
            lock(&seat.failures).clear(p.seq());
        }
        let mut removed = Vec::new();
        let mut errors = Vec::new();
        let mut cleared = Vec::new();
        for review in [false, true] {
            let target = Target::Tree {
                task: task.clone(),
                review,
            };
            if kept.as_ref() == Some(&target) {
                tracing::info!(seat = %seat.name(), seq = end, %target, "the running turn is of a restart after this end; left alone");
                continue;
            }
            if let Some(dir) = self
                .trees
                .layout()
                .task_dir(seat.name(), &task, review)
                .ok()
                .filter(|p| p.exists())
            {
                let start = hgl_ai_tree::read_task(&dir).ok().and_then(|f| f.seq);
                if !ends(start) {
                    tracing::info!(seat = %seat.name(), seq = end, ?start, %target, "a tree made after this end; left alone");
                    continue;
                }
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
            let failed =
                match tokio::task::spawn_blocking(move || trees.remove(&s, &t, review)).await {
                    Ok(Ok(Some(rm))) => {
                        if let Err(e) = self.bus.tree_del(&rm.key).await {
                            tracing::warn!(key = %rm.key, "deregistering: {e}");
                        }
                        removed.push(rm);
                        None
                    }
                    Ok(Ok(None)) => None,
                    Ok(Err(e)) => Some(e.to_string()),
                    Err(e) => Some(format!("removal panicked: {e}")),
                };
            match failed {
                // Owed apart from the end, which stands: tried again until it works.
                Some(e) => {
                    lock(&seat.cleanup).owe(&task, review, end, &e);
                    errors.push(format!(
                        "{e} (the removal is tried again every {:?} until it works)",
                        self.opts.cleanup_retry
                    ));
                }
                None => lock(&seat.cleanup).settle(&task, review),
            }
            cleared.push(target);
        }
        st.busy.retain(|t, _| !cleared.contains(t));
        st.prep_failures.retain(|t, _| !cleared.contains(t));
        lock(&seat.busy_told).retain(|t| !cleared.contains(t));
        lock(&seat.prep_told).retain(|t| !cleared.contains(t));
        if let Err(e) = lock(&seat.ended).end(task.clone(), d.seq()) {
            tracing::warn!(seat = %seat.name(), %task, "caching the end of the task: {e}");
        }
        let recorded = self.bus.ended_put(seat.name(), &task, d.seq()).await;
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
        match recorded {
            Ok(()) => {
                let _ = d.ack().await;
                lock(&seat.failures).clear(d.seq());
            }
            Err(e) => {
                // Done again later, the note finds nothing left to remove and records the end.
                tracing::warn!(seat = %seat.name(), %task, "recording the end in {}: {e}; held, done again in {:?}", hgl_ai_proto::KV_ENDED, self.opts.hub_retry);
                st.deferred.push((d, Instant::now() + self.opts.hub_retry));
            }
        }
    }

    /// Try again the removals the seat owes (a failed removal on `landed` or `closed`): each
    /// whose tree no turn uses now. Reported when it works.
    async fn retry_cleanup(&self, seat: &Arc<SeatRt>, st: &mut SeatState<B::Delivery>) {
        let owed = lock(&seat.cleanup).list();
        for o in owed {
            let task = o.task();
            let target = Target::Tree {
                task: task.clone(),
                review: o.review,
            };
            if st.running.as_ref().is_some_and(|r| r.target == target) {
                continue;
            }
            if let Some(dir) = self
                .trees
                .layout()
                .task_dir(seat.name(), &task, o.review)
                .ok()
                .filter(|p| p.exists())
            {
                let lp = dir.join(lock::TURN_LOCK);
                if lp.exists()
                    && matches!(FileLock::turn_patiently(&lp).await, Ok(None))
                    && !turn::reap_lock_holders(&lp, self.opts.kill_grace).await
                {
                    lock(&seat.cleanup).owe(
                        &task,
                        o.review,
                        o.end,
                        "a process still holds its turn lock",
                    );
                    continue;
                }
            }
            let trees = self.trees.clone();
            let (s, t, review) = (seat.name().clone(), task.clone(), o.review);
            match tokio::task::spawn_blocking(move || trees.remove(&s, &t, review)).await {
                Ok(Ok(rm)) => {
                    lock(&seat.cleanup).settle(&task, o.review);
                    let what = match &rm {
                        Some(rm) => {
                            if let Err(e) = self.bus.tree_del(&rm.key).await {
                                tracing::warn!(key = %rm.key, "deregistering: {e}");
                            }
                            let salvage = rm
                                .salvage
                                .as_ref()
                                .and_then(|p| p.strip_prefix(self.trees.layout().root()).ok())
                                .map(|p| format!("; salvage: {}", p.display()))
                                .unwrap_or_default();
                            format!(
                                "removed {} at try {}, freed {}{salvage}",
                                hgl_ai_tree::layout::task_dir_name(&task, o.review),
                                o.tries + 1,
                                hgl_ai_tree::stale::human_bytes(rm.freed_bytes)
                            )
                        }
                        None => "nothing was left to remove".to_string(),
                    };
                    tracing::info!(seat = %seat.name(), %task, "{what}");
                    let body = format!(
                        "[bridge] {}/{}: Cleanup: {}#{}: {what} (the removal at its end had failed: {}).",
                        self.cfg.host,
                        seat.name(),
                        task.repo,
                        task.issue,
                        o.error
                    );
                    self.tell(seat.name(), Kind::Done, Some(&task), body).await;
                }
                Ok(Err(e)) => {
                    tracing::warn!(seat = %seat.name(), %task, "the owed removal failed again: {e}");
                    lock(&seat.cleanup).owe(&task, o.review, o.end, &e.to_string());
                }
                Err(e) => {
                    lock(&seat.cleanup).owe(
                        &task,
                        o.review,
                        o.end,
                        &format!("removal panicked: {e}"),
                    );
                }
            }
        }
    }

    /// Start the next turn from `pending`: the first group of notes sharing a target that
    /// is not busy. A note of a task whose tree a queued `assign` or `review` makes joins
    /// that tree's turn. While the gate is closed (ROLE-18, ROLE-19) a note that would make
    /// a new tree stays held in `pending`, kept alive, until it opens: an `assign` under
    /// `drain` or low disk, any new tree (a review, a moved task's tree) under low disk.
    /// Nothing starts during a quota hold.
    async fn start_next(
        self: &Arc<Self>,
        seat: &Arc<SeatRt>,
        st: &mut SeatState<B::Delivery>,
    ) -> Option<Running<B::Delivery>> {
        if st.pending.is_empty() {
            return None;
        }
        let now = Instant::now();
        self.back_to_primary(seat).await;
        if lock(&seat.quota_until).is_some_and(|t| t > now) {
            return None;
        }
        let mut plans: Vec<turn::Plan> = st
            .pending
            .iter()
            .map(|d| self.plan(seat, d.note()))
            .collect();
        // A note for no tree yet, of a task whose tree a queued note makes: into that turn.
        let makers: Vec<(TaskKey, Target)> = plans
            .iter()
            .filter(|p| p.create.is_some())
            .filter_map(|p| Some((p.target.task()?.clone(), p.target.clone())))
            .collect();
        for (d, p) in st.pending.iter().zip(plans.iter_mut()) {
            if p.target == Target::Inbox
                && let Some(t) = d.note().task()
                && let Some((_, target)) = makers.iter().find(|(k, _)| *k == t)
            {
                p.target = target.clone();
            }
        }
        let mut gate = None;
        let mut held = Vec::with_capacity(plans.len());
        for (d, p) in st.pending.iter().zip(&plans) {
            let new_tree = match &p.target {
                Target::Tree { task, review } => {
                    p.create.is_some() && self.trees.existing(seat.name(), task, *review).is_none()
                }
                Target::Inbox => false,
            };
            let gated = if new_tree {
                let g = match gate {
                    Some(g) => g,
                    None => *gate.insert(self.gate_check().await),
                };
                (d.note().kind == Kind::Assign && g != Gate::Open) || g == Gate::LowDisk
            } else {
                false
            };
            held.push(gated);
        }
        // A note that joins a held maker's tree is held with it.
        let held_targets: HashSet<Target> = plans
            .iter()
            .zip(&held)
            .filter(|(p, h)| **h && p.create.is_some())
            .map(|(p, _)| p.target.clone())
            .collect();
        for (p, h) in plans.iter().zip(held.iter_mut()) {
            if !*h && p.create.is_none() && held_targets.contains(&p.target) {
                *h = true;
            }
        }
        let idx = (0..plans.len())
            .find(|&i| !held[i] && st.busy.get(&plans[i].target).is_none_or(|u| *u <= now))?;
        let target = plans[idx].target.clone();
        let mut group = Vec::new();
        let mut creates = Vec::new();
        let mut rest = Vec::new();
        for ((d, p), h) in st.pending.drain(..).zip(plans).zip(held) {
            if p.target == target && !h {
                creates.push((d.seq(), p.create));
                group.push(d);
            } else {
                rest.push(d);
            }
        }
        st.pending = rest;
        if let Target::Tree { task, review } = &target {
            // A turn of the task after its end is a restart: the tree is its now, not a
            // removal still owed (older notes of the task are moot, so these are later).
            lock(&seat.cleanup).settle(task, *review);
        }
        let create = turn::choose_create(creates);
        // The tree the turn runs in: one kept from before (a review always gets a new one),
        // or the one its creating note makes.
        let start = match &target {
            Target::Inbox => None,
            Target::Tree { task, review } => {
                let fresh = matches!(create, Some((_, Create::Review { .. })));
                match self.trees.existing(seat.name(), task, *review) {
                    Some(dir) if !fresh => hgl_ai_tree::read_task(&dir).ok().and_then(|f| f.seq),
                    _ => create.as_ref().map(|(seq, _)| *seq),
                }
            }
        };
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
            start,
            deliveries: Arc::new(Mutex::new(group)),
            stop: stop_tx,
            handle,
            slot,
        })
    }

    /// Settle the notes of an ended turn by its outcome; `end` is the stream sequence of
    /// the ending note that stopped it, if one did. A real failure is counted and naked (or
    /// dead-lettered); an administrative hold keeps the notes in `pending`, alive, and spends
    /// no delivery (PROTO-8).
    async fn finish(
        &self,
        seat: &Arc<SeatRt>,
        r: Running<B::Delivery>,
        out: std::result::Result<TurnOut, JoinError>,
        st: &mut SeatState<B::Delivery>,
        end: Option<u64>,
    ) {
        let ds: Vec<Arc<B::Delivery>> = lock(&r.deliveries).clone();
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                // The turn task panicked or was cancelled: kill its tree, then count it.
                let tree = lock(&r.slot).clone();
                if let Some(t) = tree {
                    proc::reap(&t, self.opts.kill_grace, Duration::from_secs(10)).await;
                }
                tracing::error!(seat = %seat.name(), "turn task failed: {e}");
                return self
                    .failed(seat, &ds, &format!("the turn task failed: {e}"))
                    .await;
            }
        };
        let seqs: Vec<u64> = ds.iter().map(|d| d.seq()).collect();
        tracing::info!(seat = %seat.name(), ?seqs, target = %r.target, outcome = ?out.outcome, "turn ended");
        if !matches!(out.outcome, Outcome::Busy(_)) {
            lock(&seat.busy_told).remove(&r.target);
        }
        if !matches!(out.outcome, Outcome::PrepFailed(_)) {
            st.prep_failures.remove(&r.target);
            lock(&seat.prep_told).remove(&r.target);
        }
        match out.outcome {
            Outcome::Clean => {
                seat.exhausted_told.store(false, Ordering::Relaxed);
                for d in &ds {
                    let _ = d.ack().await;
                }
                let mut f = lock(&seat.failures);
                seqs.iter().for_each(|s| f.clear(*s));
            }
            Outcome::Stopped => {
                // Its task ended (`landed`, `closed`): the notes sent before the end are moot;
                // later ones are newer intent and run again.
                for d in ds {
                    if end.is_none_or(|e| d.seq() < e) {
                        let _ = d.ack().await;
                        lock(&seat.failures).clear(d.seq());
                    } else {
                        let _ = d.nak(self.opts.nak_delay).await;
                    }
                }
            }
            Outcome::Quota(msg) => {
                // Held, not naked: a quota refusal is no failure of the notes.
                if !self.failover(seat, out.binding_idx, &msg, &r.target).await {
                    *lock(&seat.quota_until) = Some(Instant::now() + self.opts.quota_retry);
                }
                st.pending.splice(0..0, newest(ds));
            }
            Outcome::PrepFailed(msg) => {
                // It may heal (the network, the hub, a fetch): held and tried again, backing off.
                let n = st.prep_failures.entry(r.target.clone()).or_insert(0);
                *n += 1;
                let wait = self.opts.prep_retry * (1u32 << (*n - 1).min(4));
                st.busy.insert(r.target.clone(), Instant::now() + wait);
                st.pending.splice(0..0, newest(ds));
                if lock(&seat.prep_told).insert(r.target.clone()) {
                    let body = format!(
                        "[bridge] {}/{}: could not prepare {} for a turn: {msg}. Its notes are held and tried again (backing off to {:?}); a `closed` note for the task drops them.",
                        self.cfg.host,
                        seat.name(),
                        r.target,
                        self.opts.prep_retry * 16
                    );
                    self.tell(seat.name(), Kind::Note, r.target.task(), body)
                        .await;
                }
            }
            Outcome::CannotStart(msg) => {
                // A retry fails alike: drop the notes at once and say which.
                for d in &ds {
                    let _ = d.term().await;
                    lock(&seat.failures).clear(d.seq());
                }
                let notes: Vec<String> = ds
                    .iter()
                    .map(|d| format!("seq {} {} from {}", d.seq(), d.note().kind, d.note().sender))
                    .collect();
                let body = format!(
                    "[bridge] {}/{}: cannot start a turn in {}: {msg}. A retry would fail alike, so its notes are dropped, not retried: {}. Fix the cause and send them again.",
                    self.cfg.host,
                    seat.name(),
                    r.target,
                    notes.join("; ")
                );
                self.tell(seat.name(), Kind::Note, r.target.task(), body)
                    .await;
            }
            Outcome::Failed(msg) => self.failed(seat, &ds, &msg).await,
            Outcome::Shutdown => nak_all(&ds, self.opts.nak_delay).await,
            Outcome::Busy(who) => {
                // Another process holds the task's turn lock: wait, do not start a second turn.
                st.busy
                    .insert(r.target.clone(), Instant::now() + self.opts.busy_retry);
                st.pending.splice(0..0, newest(ds));
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

    /// A real failure of a turn (PROTO-8): count it for each of its notes; nak a note below
    /// [`Options::max_failures`], dead-letter one that reached it.
    async fn failed(&self, seat: &Arc<SeatRt>, ds: &[Arc<B::Delivery>], why: &str) {
        let mut done: Vec<u64> = Vec::new();
        for d in newest(ds.to_vec()) {
            let n = lock(&seat.failures).record(d.seq(), why);
            if n < self.opts.max_failures {
                continue;
            }
            done.push(d.seq());
            self.dead_letter(seat, &d, n, why).await;
        }
        for d in ds {
            if !done.contains(&d.seq()) {
                let _ = d.nak(self.opts.nak_delay).await;
            }
        }
    }

    /// Terminate a note that failed `n` times and send it, with the reason, to the
    /// controller as a `note`.
    async fn dead_letter(&self, seat: &Arc<SeatRt>, d: &Arc<B::Delivery>, n: u32, why: &str) {
        let note = d.note();
        let seq = d.seq();
        let _ = d.term().await;
        lock(&seat.failures).clear(seq);
        tracing::error!(seat = %seat.name(), seq, kind = %note.kind, failures = n, "dead letter: {why}");
        if note.kind == Kind::Note && note.body.starts_with(DEAD_LETTER) {
            // Never a dead letter of a dead letter.
            return;
        }
        let task = note
            .task()
            .map(|t| format!(", {}#{}", t.repo, t.issue))
            .unwrap_or_default();
        let why: String = why.chars().take(500).collect();
        let head = format!(
            "{DEAD_LETTER} {}/{}: note seq {seq} ({} from {}{task}) failed {n} times and is dropped, not retried. Last failure: {why}. Fix the cause and send it again. The note:\n\n",
            self.cfg.host,
            seat.name(),
            note.kind,
            note.sender
        );
        // Room for the envelope and JSON escaping within the bus's note limit.
        let room = hgl_ai_bus::MAX_NOTE_BYTES / 2 - head.len();
        let mut quoted = describe_note(seq, note);
        if quoted.len() > room {
            let mut cut = room;
            while !quoted.is_char_boundary(cut) {
                cut -= 1;
            }
            quoted.truncate(cut);
            quoted.push_str("\n(cut off)\n");
        }
        let task = note.task();
        self.tell(seat.name(), Kind::Note, task.as_ref(), head + &quoted)
            .await;
    }

    /// The seat's binding after the cool-down: back to its primary (PROTO-17).
    async fn back_to_primary(&self, seat: &Arc<SeatRt>) {
        let since = *lock(&seat.failed_over_at);
        let Some(since) = since else { return };
        if since.elapsed() < self.opts.failover_cooldown {
            return;
        }
        let from = {
            let mut b = lock(&seat.binding);
            *lock(&seat.failed_over_at) = None;
            if *b == 0 {
                return;
            }
            std::mem::replace(&mut *b, 0)
        };
        *lock(&seat.quota_until) = None;
        seat.exhausted_told.store(false, Ordering::Relaxed);
        let from = self.describe(&seat.binding_at(from));
        let to = self.describe(&seat.binding_at(0));
        tracing::info!(seat = %seat.name(), %from, %to, "cool-down over; back to the primary binding");
        let body = format!(
            "[bridge] Seat {} on {} returns from {from} to its primary {to} after the {:?} cool-down.",
            seat.name(),
            self.cfg.host,
            self.opts.failover_cooldown
        );
        self.tell(seat.name(), Kind::Note, None, body).await;
    }

    /// PROTO-17: move the seat to its next binding after a quota refusal on `used`, skipping
    /// one whose family would break ROLE-11 for the refused turn's task. Whether it moved
    /// (or another refusal moved it already).
    async fn failover(&self, seat: &Arc<SeatRt>, used: usize, msg: &str, target: &Target) -> bool {
        let cur = self
            .cfg
            .resolve(&seat.binding_at(used))
            .ok()
            .map(|r| r.family);
        let mut skipped = Vec::new();
        let mut to = None;
        for i in used + 1..=seat.cfg.fallback.len() {
            let b = seat.binding_at(i);
            let family = match self.cfg.resolve(&b) {
                Ok(r) => r.family,
                Err(e) => {
                    skipped.push(format!("{} ({e})", b.runtime));
                    continue;
                }
            };
            match self.role9_block(seat, target, cur, family).await {
                None => {
                    to = Some(i);
                    break;
                }
                Some(why) => skipped.push(format!("{} ({why})", self.describe(&b))),
            }
        }
        {
            let mut b = lock(&seat.binding);
            if *b != used {
                return true;
            }
            if let Some(i) = to {
                *b = i;
                *lock(&seat.failed_over_at) = Some(Instant::now());
            }
        }
        let from = self.describe(&seat.binding_at(used));
        let msg: String = msg.chars().take(200).collect();
        let skipped = if skipped.is_empty() {
            String::new()
        } else {
            format!(" Skipped: {}.", skipped.join("; "))
        };
        match to {
            Some(i) => {
                let to = self.describe(&seat.binding_at(i));
                tracing::warn!(seat = %seat.name(), %from, %to, "quota refusal; failing over");
                let body = format!(
                    "[bridge] Failover: seat {} on {} moved from {from} to {to} after a quota refusal ({msg}). Later turns use {to}, and the seat returns to its primary after {:?}; ROLE-11 applies to the new family.{skipped}",
                    seat.name(),
                    self.cfg.host,
                    self.opts.failover_cooldown
                );
                self.tell(seat.name(), Kind::Note, None, body).await;
                true
            }
            None => {
                if !seat.exhausted_told.swap(true, Ordering::Relaxed) {
                    let body = format!(
                        "[bridge] Quota: seat {} on {} was refused on {from} ({msg}) and has no fallback left to move to.{skipped} Its notes are held and tried again every {:?}.",
                        seat.name(),
                        self.cfg.host,
                        self.opts.quota_retry
                    );
                    self.tell(seat.name(), Kind::Note, None, body).await;
                }
                false
            }
        }
    }

    /// Why moving the turn of `target` from family `cur` to `to` would break ROLE-11, if it
    /// would. A review keeps the family the controller gave it (it was chosen to differ from
    /// the author's). An implementor's task must leave a live reviewer family that is none of
    /// its authors'. Unknown (the hub unreadable, no reviewer live): allowed.
    async fn role9_block(
        &self,
        seat: &SeatRt,
        target: &Target,
        cur: Option<Family>,
        to: Family,
    ) -> Option<String> {
        if Some(to) == cur {
            return None;
        }
        let Target::Tree { task, review } = target else {
            return None;
        };
        if *review {
            return Some(format!(
                "a review keeps its family ({}), which differs from the author's",
                cur.map(|f| f.to_string())
                    .unwrap_or_else(|| "unknown".into())
            ));
        }
        let mut authors: HashSet<Family> = cur.into_iter().collect();
        if let Some(dir) = self.trees.existing(seat.name(), task, false)
            && let Ok(f) = hgl_ai_tree::read_task(&dir)
        {
            authors.extend(f.families);
        }
        authors.insert(to);
        let reviewers: HashSet<Family> = match self.bus.hosts().await {
            Ok(hosts) => hosts
                .iter()
                .flat_map(|h| h.seats.iter())
                .filter(|s| s.role == Role::Reviewer)
                .map(|s| s.family)
                .collect(),
            Err(e) => {
                tracing::warn!("reading the hosts for ROLE-11: {e}; not checked");
                return None;
            }
        };
        if reviewers.is_empty() || reviewers.iter().any(|f| !authors.contains(f)) {
            return None;
        }
        let mut names: Vec<String> = reviewers.iter().map(|f| f.to_string()).collect();
        names.sort();
        Some(format!(
            "every live reviewer family ({}) would then be among the task's authors",
            names.join(", ")
        ))
    }

    /// `runtime family/model`.
    fn describe(&self, b: &Binding) -> String {
        match self.cfg.resolve(b) {
            Ok(r) => format!("{} {}/{}", r.runtime, r.family, r.model),
            Err(e) => format!("{} ({e})", b.runtime),
        }
    }

    async fn keep_alive(&self, st: &SeatState<B::Delivery>) {
        let mut all: Vec<Arc<B::Delivery>> = st.pending.to_vec();
        all.extend(st.deferred.iter().map(|(d, _)| d.clone()));
        if let Some(r) = &st.running {
            all.extend(lock(&r.deliveries).iter().cloned());
        }
        // Bounded: with the hub gone, the seat loop must still see a lost lease.
        let sent = tokio::time::timeout(self.opts.in_progress_every / 2, async {
            for d in all {
                if let Err(e) = d.in_progress().await {
                    tracing::warn!(seq = d.seq(), "in_progress: {e}");
                }
            }
        })
        .await;
        if sent.is_err() {
            tracing::warn!("in_progress: the hub did not answer in time");
        }
    }

    /// Terminate a `build` note sent to an agent seat and tell its sender.
    async fn refuse_build(&self, seat: &Seat, d: &B::Delivery) {
        let n = d.note();
        tracing::warn!(%seat, seq = d.seq(), from = %n.sender, "a build job at an agent seat; terminated");
        if let Err(e) = d.term().await {
            tracing::warn!(%seat, seq = d.seq(), "terminating: {e}");
        }
        let body = format!(
            "[bridge] Not a builder: {seat} is an agent seat, so your `build` note (seq {}) was dropped. Builders take jobs at `builder-<label>`; use `hgl-ai build run --platform P` (`hgl-ai builder list`).",
            d.seq()
        );
        let mut r = hgl_ai_proto::Note::new(seat.clone(), n.sender.clone(), Kind::Note, body);
        r.repo = n.repo.clone();
        r.issue = n.issue;
        if let Err(e) = self.bus.send(&r).await {
            tracing::warn!(%seat, "telling {}: {e}", n.sender);
        }
    }

    /// Send a note to the controller (`[github] controller`); failures are logged.
    async fn tell(&self, from: &Seat, kind: Kind, task: Option<&TaskKey>, body: String) {
        let to = match self.cfg.controller() {
            Ok(s) => s,
            Err(e) => return tracing::warn!(kind = %kind, "not sent: {e}"),
        };
        let mut n = hgl_ai_proto::Note::new(from.clone(), to, kind, body);
        if let Some(t) = task {
            n.repo = Some(t.repo.clone());
            n.issue = t.issue;
        }
        if let Err(e) = self.bus.send(&n).await {
            tracing::warn!(kind = %kind, "sending to {}: {e}", n.recipient);
        }
    }
}

/// The first line of a dead letter's body.
const DEAD_LETTER: &str = "[bridge] Dead letter:";

/// One handle per note, the newest of a redelivered one, in order.
fn newest<D: DeliveryApi>(ds: Vec<Arc<D>>) -> Vec<Arc<D>> {
    let mut out: Vec<Arc<D>> = Vec::with_capacity(ds.len());
    for d in ds {
        match out.iter_mut().find(|o| o.seq() == d.seq()) {
            Some(o) => *o = d,
            None => out.push(d),
        }
    }
    out
}

/// A note as `note wait` prints it: a header line, then the body.
fn describe_note(seq: u64, n: &Note) -> String {
    let mut h = format!(
        "--- seq={seq} from={} to={} kind={}",
        n.sender, n.recipient, n.kind
    );
    if let Some(t) = n.task() {
        h.push_str(&format!(" {}#{}", t.repo, t.issue));
    }
    if let Some(x) = &n.head {
        h.push_str(&format!(" head={x}"));
    }
    if let Some(x) = &n.base {
        h.push_str(&format!(" base={x}"));
    }
    format!("{h} at {}\n{}", n.created_at, n.body)
}

/// Resolves once `shutdown` is true (the guard of `wait_for` is not held past it).
pub(crate) async fn down(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|d| *d).await;
}

/// Ask the running turn, if any, to stop.
fn stop_turn<D>(running: &Option<Running<D>>, how: Stop) {
    if let Some(r) = running {
        let _ = r.stop.send(how);
    }
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
