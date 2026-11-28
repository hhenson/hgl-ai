//! The builder service (BUILD-6, `hgl-ai builder run`): one lane per builder this host
//! runs (its own platform, and each remote it drives as a proxy). A lane takes `build`
//! jobs from its inbox `builder-<label>`, checks out the requested head in a job
//! directory of its own, runs the recipe, keeps the summary in `HGL_BUILD/run.<id>`,
//! sends the `receipt`, and removes the checkout. It advertises itself in
//! `HGL_HOSTS/builder.<label>` with its health, and honours `drain` and `retire`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hgl_ai_bus::Bus;
use hgl_ai_proto::build::{
    BuildJob, BuildStatus, BuilderRecord, StepStatus, Summary, verdict_platform,
};
use hgl_ai_proto::{Kind, Seat, utc_now_stamp};
use tokio::sync::{Notify, watch};

use crate::driver::{self, Meta, Options};
use crate::exec::{Cancel, Checkout, Executor, Prep, Probe, Workspace, ssh};
use crate::store;

/// Added to a gate build's step time limits to bound its `running` verdict.
const RUNNING_MARGIN: Duration = Duration::from_secs(15 * 60);
/// Heartbeats between sweeps of the verdict keys of pull requests that closed without a
/// `landed` note (about an hour).
const VERDICT_SWEEP_EVERY: u32 = 120;
/// Verdict keys the hub wrote longer ago than this are swept (the notes' history and a
/// day).
const VERDICT_KEEP_DAYS: i64 = 15;
/// How often a lane writes its record and checks its gate.
pub const HEARTBEAT: Duration = Duration::from_secs(30);
const KEEPALIVE: Duration = Duration::from_secs(60);
/// Builds' log directories kept per lane.
pub const LOGS_KEPT: usize = 100;
const PROBE_EVERY: u32 = 20;

/// Whether a lane takes jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Active,
    Draining,
    Retired,
}

impl Gate {
    pub fn as_str(self) -> &'static str {
        match self {
            Gate::Active => "active",
            Gate::Draining => "draining",
            Gate::Retired => "retired",
        }
    }
}

/// `HGL_LOCKS/builder.<label>`: held by the one process that serves the builder.
pub fn lease_key(label: &str) -> String {
    format!("builder.{label}")
}

/// `HGL_STATE/builder.<label>`: `drain` or `retire` a builder (empty: active).
pub fn gate_key(label: &str) -> String {
    format!("builder.{label}")
}

/// One builder.
pub struct Lane {
    pub label: String,
    pub inbox: Seat,
    pub exec: Arc<dyn Executor>,
    pub slots: u32,
    /// The host label of the proxy, for a remote.
    pub proxy: Option<String>,
    pub conn: Option<Arc<ssh::Conn>>,
    pub kick: Option<Arc<Notify>>,
    /// Local directory of this lane's build logs.
    pub logs: PathBuf,
    busy: AtomicU32,
    /// The startup cleanup of leftover job directories is done; no job is taken before.
    cleaned: AtomicBool,
    gate: Mutex<Gate>,
    probe: Mutex<Probe>,
    /// Workspaces whose cleanup waits for the remote to come back.
    stale: Mutex<Vec<Workspace>>,
    /// One stale cleanup pass at a time.
    stale_pass: tokio::sync::Mutex<()>,
    /// Job directories of builds running now; no cleanup touches them.
    live: Mutex<std::collections::BTreeSet<String>>,
    /// This host's id (`<data_dir>/host-id`), in the lease's holder.
    pub host_id: String,
}

impl Lane {
    pub fn new(
        label: String,
        inbox: Seat,
        exec: Arc<dyn Executor>,
        slots: u32,
        logs: PathBuf,
    ) -> Lane {
        Lane {
            label,
            inbox,
            exec,
            slots,
            proxy: None,
            conn: None,
            kick: None,
            logs,
            busy: AtomicU32::new(0),
            cleaned: AtomicBool::new(false),
            gate: Mutex::new(Gate::Active),
            probe: Mutex::new(Probe::default()),
            stale: Mutex::new(Vec::new()),
            stale_pass: tokio::sync::Mutex::new(()),
            live: Mutex::new(std::collections::BTreeSet::new()),
            host_id: String::new(),
        }
    }

    pub fn with_host_id(mut self, id: String) -> Lane {
        self.host_id = id;
        self
    }

    /// A lane for a remote driven over `conn` by the proxy `proxy`.
    pub fn remote(mut self, proxy: &str, conn: Arc<ssh::Conn>, kick: Arc<Notify>) -> Lane {
        self.proxy = Some(proxy.to_string());
        self.conn = Some(conn);
        self.kick = Some(kick);
        self
    }

    pub fn reachable(&self) -> bool {
        self.conn.as_ref().is_none_or(|c| c.is_reachable())
    }

    fn gate(&self) -> Gate {
        *self.gate.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn busy(&self) -> u32 {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn record(&self) -> BuilderRecord {
        let probe = self.probe.lock().unwrap_or_else(|e| e.into_inner()).clone();
        BuilderRecord {
            label: self.label.clone(),
            inbox: self.inbox.clone(),
            platform: self.exec.platform(),
            arch: if self.conn.is_none() {
                std::env::consts::ARCH.to_string()
            } else {
                String::new()
            },
            executor: self.exec.kind().to_string(),
            proxy: self.proxy.clone(),
            reachable: self.reachable(),
            reason: if self.reachable() {
                None
            } else {
                self.conn.as_ref().and_then(|c| c.reason())
            },
            state: self.gate().as_str().to_string(),
            slots: self.slots,
            busy: self.busy(),
            free_gb: probe.free_gb,
            toolchains: probe.toolchains,
            updated_at: utc_now_stamp(),
        }
    }
}

/// What the service needs besides its lanes.
#[derive(Clone)]
pub struct Settings {
    /// Clone URL of a repository.
    pub clone_url: Arc<dyn Fn(&str) -> String + Send + Sync>,
    pub toolchains: Vec<String>,
    pub heartbeat: Duration,
    /// This service's hub user may write the local build verdicts (a `builder` user, or
    /// a hub with one shared token); else gate builds are not run (BUILD-15).
    pub verdicts: bool,
    /// Asks GitHub whether a pull request is merged or closed, so its verdict keys may go;
    /// `None`: they go only by age.
    pub pr_ended: Option<store::PrEnded>,
}

/// Run the lanes until `shutdown` turns true; then cancel running builds (their remote
/// trees killed, job directories removed, jobs returned to the inbox), close the remote
/// connections and remove the records.
pub async fn serve(
    bus: Arc<Bus>,
    lanes: Vec<Arc<Lane>>,
    settings: Settings,
    shutdown: watch::Receiver<bool>,
) -> Result<(), ServiceError> {
    // One lane that cannot serve ends them all, with its error, so that the service
    // manager restarts the whole builder (no lane left dead beside the others).
    let (stop_all, all_rx) = watch::channel(false);
    let forward = {
        let (mut outer, tx) = (shutdown, stop_all.clone());
        tokio::spawn(async move {
            while !*outer.borrow() {
                if outer.changed().await.is_err() {
                    break;
                }
            }
            let _ = tx.send(true);
        })
    };
    let mut tasks = tokio::task::JoinSet::new();
    for lane in lanes {
        let (bus, settings, rx) = (bus.clone(), settings.clone(), all_rx.clone());
        tasks.spawn(async move { lane_loop(bus, lane, settings, rx).await });
    }
    let mut first_err = None;
    while let Some(t) = tasks.join_next().await {
        let err = match t {
            Ok(Ok(())) => continue,
            Ok(Err(e)) => e,
            Err(e) => ServiceError::Other(format!("a lane stopped: {e}")),
        };
        if first_err.is_none() {
            tracing::error!("{err}; stopping every lane");
            let _ = stop_all.send(true);
            first_err = Some(err);
        }
    }
    forward.abort();
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Bus(#[from] hgl_ai_bus::BusError),
    #[error("{0}")]
    Other(String),
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f)
        .await
        .expect("a blocking task panicked")
}

async fn lane_loop(
    bus: Arc<Bus>,
    lane: Arc<Lane>,
    settings: Settings,
    outer: watch::Receiver<bool>,
) -> Result<(), ServiceError> {
    // One builder per label, before anything is cleaned, connected or fetched: a `flock`
    // on this host and the hub lease `HGL_LOCKS/builder.<label>` across hosts.
    let lock_path = lane.logs.with_extension("lock");
    let _flock = match crate::lock::FileLock::try_lock(&lock_path) {
        Ok(Some(l)) => l,
        Ok(None) => {
            return Err(ServiceError::Other(format!(
                "builder {} already runs on this host (it holds {})",
                lane.label,
                lock_path.display()
            )));
        }
        Err(e) => {
            return Err(ServiceError::Other(format!(
                "builder {}: locking {}: {e}",
                lane.label,
                lock_path.display()
            )));
        }
    };
    let host = lane.proxy.clone().unwrap_or_else(|| lane.label.clone());
    let lease_name = lease_key(&lane.label);
    // `<host> pid <pid> (builder) id <host-id>` (PROTO-6): a host that shares the label
    // by mistake has another id and is never taken over.
    let holder = hgl_ai_bridge::holder::holder(&host, &lane.host_id, "builder");
    let lease = match bus.lock_acquire(&lease_name, &holder).await {
        Ok(l) => l,
        // A builder of this host that is gone: this process holds the host's lock.
        Err(hgl_ai_bus::BusError::Held { holder: old, .. })
            if hgl_ai_bridge::holder::dead_own(&old, &host, &lane.host_id) =>
        {
            bus.lock_take_over(&lease_name, &holder, &old).await?
        }
        Err(hgl_ai_bus::BusError::Held {
            holder: old, since, ..
        }) => {
            return Err(ServiceError::Other(format!(
                "builder {} is served by {old} since {since}",
                lane.label
            )));
        }
        Err(e) => return Err(e.into()),
    };
    // Refreshed on its own cadence; stopping the lane when it is lost, or when no refresh
    // has been confirmed for half the bucket's `max_age` (a partition: it could expire and
    // another builder take the label while this one still builds).
    let max_age = bus
        .locks_max_age()
        .await
        .unwrap_or(hgl_ai_bus::LOCKS_MAX_AGE);
    let every = settings
        .heartbeat
        .min(max_age / 6)
        .max(Duration::from_millis(50));
    let deadline = max_age / 2;
    let lease = Arc::new(tokio::sync::Mutex::new(Some(lease)));
    let lost: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // Stops this lane: the service's shutdown, or the lease lost.
    let (stop_tx, mut shutdown) = watch::channel(false);
    let forward = {
        let (mut outer, tx) = (outer, stop_tx.clone());
        tokio::spawn(async move {
            while !*outer.borrow() {
                if outer.changed().await.is_err() {
                    break;
                }
            }
            let _ = tx.send(true);
        })
    };
    let refresher = {
        let (lease, lost, stop_tx, mut stop, label) = (
            lease.clone(),
            lost.clone(),
            stop_tx.clone(),
            shutdown.clone(),
            lane.label.clone(),
        );
        tokio::spawn(async move {
            let mut ok_at = std::time::Instant::now();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(every) => {}
                    _ = stop.changed() => break,
                }
                let mut g = lease.lock().await;
                let Some(l) = g.as_mut() else { break };
                let why = match tokio::time::timeout(every, l.refresh()).await {
                    Ok(Ok(())) => {
                        ok_at = std::time::Instant::now();
                        continue;
                    }
                    Ok(Err(e @ hgl_ai_bus::BusError::LeaseLost { .. })) => Some(e.to_string()),
                    Ok(Err(e)) => (ok_at.elapsed() >= deadline)
                        .then(|| format!("was not refreshed for {:?} ({e})", ok_at.elapsed())),
                    Err(_) => (ok_at.elapsed() >= deadline).then(|| {
                        format!("was not refreshed for {:?} (no answer)", ok_at.elapsed())
                    }),
                };
                if let Some(why) = why {
                    tracing::error!(builder = %label, "the lease {why}; stopping this builder");
                    // Not ours any more: nothing to release.
                    g.take();
                    *lost.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
                    let _ = stop_tx.send(true);
                    break;
                }
                tracing::warn!(builder = %label, "refreshing the lease: no confirmation for {:?} of {deadline:?}", ok_at.elapsed());
            }
        })
    };
    let (conn_stop_tx, conn_stop) = watch::channel(false);
    let supervisor = match (&lane.conn, &lane.kick) {
        (Some(c), Some(k)) => Some(tokio::spawn(ssh::supervise(
            c.clone(),
            settings.heartbeat,
            k.clone(),
            conn_stop,
        ))),
        _ => None,
    };
    // Heartbeat: gate, probe, record.
    let hb = {
        let (bus, lane, settings, mut stop, lost) = (
            bus.clone(),
            lane.clone(),
            settings.clone(),
            shutdown.clone(),
            lost.clone(),
        );
        tokio::spawn(async move {
            let mut beat = 0u32;
            let mut was_reachable = false;
            loop {
                let gate = read_gate(&bus, &lane.label).await;
                *lane.gate.lock().unwrap_or_else(|e| e.into_inner()) = gate;
                let reachable = lane.reachable();
                if reachable && !lane.cleaned.load(Ordering::SeqCst) {
                    // Leftovers of an earlier run of this builder, before any job is taken.
                    let ex = lane.exec.clone();
                    match blocking(move || ex.clean_all()).await {
                        Ok(()) => lane.cleaned.store(true, Ordering::SeqCst),
                        Err(e) => {
                            tracing::warn!(builder = %lane.label, "startup cleanup: {e:?}; no job is taken until it is done")
                        }
                    }
                }
                if reachable {
                    retry_stale(&lane).await;
                }
                if reachable && (beat.is_multiple_of(PROBE_EVERY) || !was_reachable) {
                    let (ex, tools) = (lane.exec.clone(), settings.toolchains.clone());
                    if let Ok(p) = blocking(move || ex.probe(&tools)).await {
                        *lane.probe.lock().unwrap_or_else(|e| e.into_inner()) = p;
                    }
                }
                if settings.verdicts && beat.is_multiple_of(VERDICT_SWEEP_EVERY) {
                    let now = chrono::Utc::now().timestamp();
                    let age = VERDICT_KEEP_DAYS * 86_400;
                    match store::verdicts_sweep(&bus, now, age, settings.pr_ended.as_ref()).await {
                        Ok(0) => {}
                        Ok(n) => tracing::info!(builder = %lane.label, "{n} verdict keys swept"),
                        Err(e) => {
                            tracing::warn!(builder = %lane.label, "sweeping verdict keys: {e}")
                        }
                    }
                }
                was_reachable = reachable;
                beat = beat.wrapping_add(1);
                if lost.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                    // The record is the new holder's now.
                    break;
                }
                let res = if gate == Gate::Retired && lane.busy() == 0 {
                    bus.builder_del(&lane.label).await
                } else {
                    bus.builder_put(&lane.record()).await
                };
                if let Err(e) = res {
                    tracing::warn!(builder = %lane.label, "writing the builder record: {e}");
                }
                let mut rx = lane.conn.as_ref().map(|c| c.watch());
                tokio::select! {
                    _ = tokio::time::sleep(settings.heartbeat) => {}
                    _ = stop.changed() => break,
                    _ = async { match rx.as_mut() { Some(r) => { let _ = r.changed().await; } None => std::future::pending().await } } => {}
                }
            }
        })
    };
    // From here every way out goes through the cleanup below: the tasks above stop and
    // the lease is given back.
    let (mut inbox, mut fatal) = match bus.inbox(&lane.inbox).await {
        Ok(i) => (Some(i), None),
        Err(e) => (None, Some(ServiceError::from(e))),
    };
    let log_error = Arc::new(Mutex::new(None));
    let log_server = inbox.as_ref().map(|_| {
        let (bus, lane, stop, stop_tx, log_error) = (
            bus.clone(),
            lane.clone(),
            shutdown.clone(),
            stop_tx.clone(),
            log_error.clone(),
        );
        tokio::spawn(async move {
            if let Err(e) = crate::log::serve(bus, lane, stop).await {
                *log_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                let _ = stop_tx.send(true);
            }
        })
    });
    let running: Arc<Mutex<Vec<Cancel>>> = Arc::new(Mutex::new(Vec::new()));
    let mut jobs = tokio::task::JoinSet::new();
    let idle = Arc::new(Notify::new());
    while let Some(inbox) = inbox.as_mut() {
        if *shutdown.borrow() {
            break;
        }
        while jobs.try_join_next().is_some() {}
        let open = lane.cleaned.load(Ordering::SeqCst)
            && lane.gate() == Gate::Active
            && lane.reachable()
            && lane.busy() < lane.slots;
        if !open {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                _ = idle.notified() => {}
                _ = shutdown.changed() => {}
            }
            continue;
        }
        let got = tokio::select! {
            r = inbox.fetch(1, Duration::from_secs(2)) => r,
            _ = shutdown.changed() => continue,
        };
        let deliveries = match got {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(builder = %lane.label, "reading the inbox: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        for d in deliveries {
            if d.note.kind != Kind::Build {
                // `landed` or `closed`: a hint, never proof (notes are not authenticated,
                // hgl-ai#16). Its verdicts go only if GitHub says the pull request ended.
                if d.note.kind.ends_task()
                    && settings.verdicts
                    && let (Some(ended), Some(repo)) = (&settings.pr_ended, d.note.repo.as_deref())
                    && d.note.issue > 0
                {
                    match store::verdicts_purge(&bus, ended, repo, d.note.issue).await {
                        Ok(None | Some(0)) => {}
                        Ok(Some(n)) => {
                            tracing::info!(builder = %lane.label, "{repo} #{}: ended on GitHub; {n} verdict keys purged", d.note.issue)
                        }
                        Err(e) => {
                            tracing::warn!(builder = %lane.label, "purging the verdicts of {repo} #{}: {e}", d.note.issue)
                        }
                    }
                }
                let _ = d.ack().await;
                continue;
            }
            let job: BuildJob = match serde_json::from_str(d.note.body.trim()) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!(builder = %lane.label, seq = d.seq, "terminating a build note that is no job: {e}");
                    let _ = d.term().await;
                    continue;
                }
            };
            // Before anything touches a path or the hub with it.
            let claimed = job
                .check(&d.note)
                .and_then(|()| check_verdict(&lane, &job))
                .and_then(|()| claim(&lane, &job));
            let claim = match claimed {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(builder = %lane.label, seq = d.seq, from = %d.note.sender, "terminating a build job: {e}");
                    let _ = d.term().await;
                    continue;
                }
            };
            lane.busy.fetch_add(1, Ordering::SeqCst);
            let cancel = Cancel::default();
            running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(cancel.clone());
            let (bus, lane, settings, running, idle) = (
                bus.clone(),
                lane.clone(),
                settings.clone(),
                running.clone(),
                idle.clone(),
            );
            jobs.spawn(async move {
                let d = Arc::new(d);
                let keep = {
                    let d = d.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(KEEPALIVE).await;
                            let _ = d.in_progress().await;
                        }
                    })
                };
                // A redelivered job that ended before (its receipt not yet published):
                // publish that result, build nothing again.
                let out = match claim {
                    Claim::Saved(s) => Some(*s),
                    Claim::Run => run_job(&bus, &lane, &settings, &job, &cancel).await,
                };
                // Acknowledged only once the result is published; until then the job
                // stays ours (kept alive), and a crash redelivers it.
                let published = match &out {
                    Some(s) => finish(&bus, &lane, &job, s, &cancel, settings.verdicts).await,
                    None => false,
                };
                keep.abort();
                // Bounded: a lane stopping for a partition must not wait on the hub.
                let bounded = Duration::from_secs(10);
                if published {
                    let _ = tokio::time::timeout(bounded, d.ack()).await;
                } else {
                    // Stopped by a shutdown: back to the inbox.
                    let _ = tokio::time::timeout(bounded, d.nak(Duration::from_secs(15))).await;
                }
                running
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|c| !c.same(&cancel));
                lane.busy.fetch_sub(1, Ordering::SeqCst);
                idle.notify_one();
            });
        }
    }
    // Shutdown: stop running builds; they clean up and return their jobs.
    for c in running.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        c.cancel();
    }
    while jobs.join_next().await.is_some() {}
    if let Some(log_server) = log_server {
        log_server.abort();
        let _ = log_server.await;
    }
    hb.abort();
    let _ = hb.await;
    let _ = conn_stop_tx.send(true);
    if let Some(s) = supervisor {
        let _ = s.await;
    }
    refresher.abort();
    let _ = refresher.await;
    // Bounded: the hub may not answer (a partition ended this lane). The record and the
    // lease are this lane's only while it holds the lease: after a loss they are the new
    // holder's.
    let bounded = Duration::from_secs(10);
    if let Some(l) = lease.lock().await.take() {
        match tokio::time::timeout(bounded, bus.builder_del(&lane.label)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(builder = %lane.label, "removing the builder record: {e}")
            }
            Err(_) => {
                tracing::warn!(builder = %lane.label, "removing the builder record: no answer")
            }
        }
        let _ = tokio::time::timeout(bounded, l.release()).await;
    }
    forward.abort();
    if let Some(why) = lost.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(ServiceError::Other(format!(
            "builder {}: the lease {why}",
            lane.label
        )));
    }
    if let Some(e) = log_error.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(ServiceError::Other(format!(
            "builder {}: log service failed: {e}",
            lane.label
        )));
    }
    match fatal.take() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

async fn read_gate(bus: &Bus, label: &str) -> Gate {
    let mode = bus.state_get("mode").await.ok().flatten();
    let own = bus.state_get(&gate_key(label)).await.ok().flatten();
    match own.as_ref().map(|e| e.value.trim()) {
        Some("retire") => Gate::Retired,
        Some("drain") => Gate::Draining,
        _ if mode.as_ref().is_some_and(|m| m.value.trim() == "drain") => Gate::Draining,
        _ => Gate::Active,
    }
}

/// Remove the workspaces whose cleanup waited for the remote, one pass at a time (the
/// heartbeat and a retrying job both call this), never one a job is using.
async fn retry_stale(lane: &Arc<Lane>) {
    let _one = lane.stale_pass.lock().await;
    let stale: Vec<Workspace> =
        std::mem::take(&mut *lane.stale.lock().unwrap_or_else(|e| e.into_inner()));
    for ws in stale {
        let live = ws.job.as_ref().is_some_and(|j| {
            lane.live
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(j)
        });
        if live {
            continue;
        }
        let ex = lane.exec.clone();
        let w = ws.clone();
        if blocking(move || ex.cleanup(&w)).await.is_err() {
            lane.stale
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(ws);
        }
    }
}

/// Until the lane's remote is reachable; `false` when cancelled first.
async fn wait_reachable(lane: &Lane, cancel: &Cancel) -> bool {
    let Some(conn) = &lane.conn else { return true };
    let mut rx = conn.watch();
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        if conn.is_reachable() {
            return true;
        }
        if let Some(k) = &lane.kick {
            k.notify_one();
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), rx.changed()).await;
    }
}

/// Run one job; `None` when a shutdown stopped it (the job goes back to the inbox).
async fn run_job(
    bus: &Arc<Bus>,
    lane: &Arc<Lane>,
    settings: &Settings,
    job: &BuildJob,
    cancel: &Cancel,
) -> Option<Summary> {
    let meta = Meta {
        id: job.id.clone(),
        repo: job.repo.clone(),
        head: Some(job.head.clone()),
        base: job.base.clone(),
        base_sha: None,
        dirty: false,
        host: lane.label.clone(),
        proxy: lane.proxy.clone(),
        requester: Some(job.requester.clone()),
        recipe_rev: job.recipe_rev,
    };
    let log_dir = lane.logs.join(&job.id);
    let mut s0 = driver::start(&meta, lane.exec.as_ref(), &log_dir, BuildStatus::Running);
    let _ = store::run_put(bus, &s0).await;
    if job.verdict.is_some() && !settings.verdicts {
        return Some(driver::not_run(
            s0,
            "a gate build needs a builder hub user: this builder connects as a seat \
             ([builder] credential, docs/install.md)",
        ));
    }
    let stored = match store::recipe_get(bus, &job.repo, job.recipe_rev).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Some(driver::not_run(
                s0,
                format!(
                    "no recipe for {} on the hub; add one with `hgl-ai build recipe import`",
                    job.repo
                ),
            ));
        }
        Err(e) => return Some(driver::not_run(s0, format!("reading the recipe: {e}"))),
    };
    s0.recipe_rev = Some(stored.revision);
    let meta = Meta {
        recipe_rev: Some(stored.revision),
        ..meta
    };
    let checkout = Checkout {
        id: job.id.clone(),
        repo: job.repo.clone(),
        url: (settings.clone_url)(&job.repo),
        head: job.head.clone(),
        base: job.base.clone(),
        issue: job.issue,
        shell: false,
    };
    let mut retried = false;
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        // A retry checks out in a directory of its own (`<id>-r1`): the first attempt's
        // may still be waiting for its cleanup.
        let mut c = checkout.clone();
        if retried {
            c.id = format!("{}-r1", job.id);
        }
        let ex = lane.exec.clone();
        let ws = match blocking(move || ex.prepare(&c)).await {
            Ok(ws) => {
                if let Some(j) = &ws.job {
                    lane.live
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(j.clone());
                }
                ws
            }
            Err(Prep::NotRun(r)) => return Some(driver::not_run(s0, r)),
            Err(Prep::Unreachable(r)) => {
                if retried {
                    return Some(driver::not_run(s0, format!("remote unreachable: {r}")));
                }
                retried = true;
                if !wait_reachable(lane, cancel).await {
                    return None;
                }
                continue;
            }
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Summary>();
        let fwd = {
            let (bus, dir) = (bus.clone(), log_dir.clone());
            tokio::spawn(async move {
                while let Some(s) = rx.recv().await {
                    let _ = store::write_status(&dir, &s);
                    if let Err(e) = store::run_put(&bus, &s).await {
                        tracing::warn!("writing run {}: {e}", s.id);
                    }
                }
            })
        };
        // What the head was merged onto: the verdict binds it (BUILD-15).
        let meta = Meta {
            base_sha: ws.base_sha.clone(),
            ..meta.clone()
        };
        if job.verdict.is_some() {
            let mut s = s0.clone();
            s.base_sha = ws.base_sha.clone();
            // Bounded: every step's time limit and a margin. A builder not done by then
            // is presumed gone, and the gate asks another (BUILD-15).
            let budget: Duration = stored
                .recipe
                .plan(lane.exec.platform(), None)
                .map(|p| p.iter().map(|st| st.time_limit()).sum::<Duration>())
                .unwrap_or_default()
                + RUNNING_MARGIN;
            let until = hgl_ai_proto::utc_stamp(
                chrono::Utc::now() + chrono::Duration::from_std(budget).unwrap_or_default(),
            );
            match store::verdict_put(bus, job, &s, Some(until)).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::info!(builder = %lane.label, "{}: a later gate build holds the verdict", job.id)
                }
                Err(e) => {
                    tracing::warn!(builder = %lane.label, "writing the verdict of {}: {e}", job.id)
                }
            }
        }
        let opts = Options {
            only: job.steps.clone(),
            keep_going: job.keep_going,
            env: driver::build_env(&job.id, &ws),
            log_dir: log_dir.clone(),
        };
        let (ex, w, recipe, m, c) = (
            lane.exec.clone(),
            ws.clone(),
            stored.recipe.clone(),
            meta.clone(),
            cancel.clone(),
        );
        let driven = blocking(move || {
            driver::drive(ex.as_ref(), &w, &recipe, &m, &opts, &c, &mut |s| {
                let _ = tx.send(s.clone());
            })
        })
        .await;
        let _ = fwd.await;
        // Cleanup is part of done.
        if let Some(j) = &ws.job {
            lane.live
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(j);
        }
        let (ex, w) = (lane.exec.clone(), ws.clone());
        if let Err(e) = blocking(move || ex.cleanup(&w)).await {
            tracing::warn!(builder = %lane.label, "cleaning up {}: {e:?}; retrying when the remote answers", job.id);
            lane.stale
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(ws);
        }
        if driven.unreachable {
            if retried {
                return Some(driver::not_run(
                    driven.summary,
                    "remote unreachable: the connection dropped twice",
                ));
            }
            retried = true;
            if !wait_reachable(lane, cancel).await {
                return None;
            }
            retry_stale(lane).await;
            continue;
        }
        // Cancelled with work left undone (a stopped step), whatever the other steps did.
        let unfinished = driven
            .summary
            .steps
            .iter()
            .any(|x| x.status == StepStatus::Stopped);
        if cancel.is_cancelled() && unfinished {
            // Not the partial aggregate (a failed step before it under --keep-going): the
            // job runs again, so what waiters see is that it is queued.
            let mut s = driven.summary;
            s.status = BuildStatus::Queued;
            s.ended_at = None;
            s.reason = Some("stopped by the builder's shutdown; the job is queued again".into());
            let _ = store::run_put(bus, &s).await;
            return None;
        }
        return Some(driven.summary);
    }
}

/// A gate build only on a builder of the platform its verdict names: no builder speaks
/// for another platform.
fn check_verdict(lane: &Lane, job: &BuildJob) -> Result<(), String> {
    let Some(v) = &job.verdict else {
        return Ok(());
    };
    let p = lane.exec.platform();
    if verdict_platform(v) != Some(p) {
        return Err(format!("verdict {v} is not this builder's platform ({p})"));
    }
    Ok(())
}

/// The request a log directory belongs to: the job as it came.
const REQUEST_FILE: &str = "request.json";

/// What to do with a checked job.
enum Claim {
    /// Build it (a new request, or one stopped before its end).
    Run,
    /// The same request ended here before (a redelivery): publish that result.
    Saved(Box<Summary>),
}

/// Bind the job's id to this request: a new id gets a log directory holding the request;
/// a known id is this request again only if the request is the same (a redelivery or a
/// retry), and is refused when it is another, so no result answers a request it was not
/// built for.
fn claim(lane: &Lane, job: &BuildJob) -> Result<Claim, String> {
    let dir = lane.logs.join(&job.id);
    let req = dir.join(REQUEST_FILE);
    match std::fs::read(&req) {
        Ok(b) => {
            let prev: BuildJob =
                serde_json::from_slice(&b).map_err(|e| format!("{}: {e}", req.display()))?;
            if prev != *job {
                return Err(format!("build id {} was used for another request", job.id));
            }
            Ok(saved_result(lane, &job.id)
                .map(|s| Claim::Saved(Box::new(s)))
                .unwrap_or(Claim::Run))
        }
        Err(_) if dir.exists() => Err(format!(
            "build id {} has a log directory of another request",
            job.id
        )),
        Err(_) => {
            let bytes = serde_json::to_vec(job).map_err(|e| e.to_string())?;
            std::fs::create_dir_all(&dir)
                .and_then(|()| std::fs::write(&req, bytes))
                .map_err(|e| format!("{}: {e}", req.display()))?;
            Ok(Claim::Run)
        }
    }
}

/// Worth trying again: the hub did not answer, not a refusal or a malformed value.
fn transient(e: &store::StoreError) -> bool {
    matches!(
        e,
        store::StoreError::Bus(
            hgl_ai_bus::BusError::Unreachable { .. } | hgl_ai_bus::BusError::Nats(_)
        )
    )
}

/// The saved result of a job that ended on this lane before: `summary.json` in its log
/// directory.
fn saved_result(lane: &Lane, id: &str) -> Option<Summary> {
    let b = std::fs::read(lane.logs.join(id).join(store::SUMMARY_FILE)).ok()?;
    serde_json::from_slice::<Summary>(&b)
        .ok()
        .filter(|s| s.id == id && s.status.is_final())
}

/// Keep the summary (log directory first, then a gate build's verdict, then the hub) and
/// send the receipts, retrying what fails until it is all published (`true`) or `cancel`
/// (`false`: the job is redelivered and its saved result published then).
async fn finish(
    bus: &Bus,
    lane: &Lane,
    job: &BuildJob,
    s: &Summary,
    cancel: &Cancel,
    verdicts: bool,
) -> bool {
    let dir = lane.logs.join(&job.id);
    if let Err(e) = store::write_final(&dir, s) {
        tracing::warn!("writing {}: {e}", dir.display());
    }
    let mut run_kept = false;
    // A hub user that may not write verdicts does not try (the run says why).
    let mut verdict_kept = job.verdict.is_none() || !verdicts;
    let mut to = vec![job.reply_to.clone()];
    to.extend(job.notify.clone());
    let mut pause = Duration::from_secs(1);
    loop {
        // The verdict before the final run: a requester that sees the run ended finds its
        // verdict written (or refused).
        if !verdict_kept {
            match store::verdict_put(bus, job, s, None).await {
                Ok(true) => verdict_kept = true,
                Ok(false) => {
                    tracing::info!("{}: a later gate build holds the verdict", s.id);
                    verdict_kept = true;
                }
                Err(e) if transient(&e) => {
                    tracing::warn!("writing the verdict of {}: {e}; retrying", s.id)
                }
                Err(e) => {
                    tracing::error!("writing the verdict of {}: {e}; not retried", s.id);
                    verdict_kept = true;
                }
            }
        }
        if !run_kept && verdict_kept {
            match store::run_put(bus, s).await {
                Ok(()) => run_kept = true,
                Err(e) if transient(&e) => tracing::warn!("writing run {}: {e}; retrying", s.id),
                // A refusal or a malformed value stays so: not retried.
                Err(e) => {
                    tracing::warn!("writing run {}: {e}; not retried", s.id);
                    run_kept = true;
                }
            }
        }
        let mut left = Vec::new();
        for seat in to {
            match store::send_receipt(bus, &lane.inbox, &seat, s, job.issue).await {
                Ok(()) => {}
                Err(e) if transient(&e) => {
                    tracing::warn!("sending the receipt of {} to {seat}: {e}; retrying", s.id);
                    left.push(seat);
                }
                Err(e) => tracing::warn!("the receipt of {} to {seat}: {e}; not retried", s.id),
            }
        }
        to = left;
        if run_kept && verdict_kept && to.is_empty() {
            prune_logs(&lane.logs, LOGS_KEPT);
            return true;
        }
        let deadline = std::time::Instant::now() + pause;
        while std::time::Instant::now() < deadline {
            if cancel.is_cancelled() {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        pause = (pause * 2).min(Duration::from_secs(30));
    }
}

/// Keep the newest `keep` build log directories under `root` (ids sort by start).
pub fn prune_logs(root: &std::path::Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| {
            e.file_type().is_ok_and(|t| t.is_dir())
                && hgl_ai_proto::build::check_build_id(&e.file_name().to_string_lossy()).is_ok()
                // A build that has not ended (with `concurrency > 1`) keeps its logs.
                && e.path().join(store::SUMMARY_FILE).is_file()
        })
        .map(|e| e.path())
        .collect();
    dirs.sort();
    let n = dirs.len().saturating_sub(keep);
    for d in &dirs[..n] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// The lanes `cfg` configures: this host's platform (unless `builder.local = false`) and
/// each `[[builder.remote]]`. `ssh` is the ssh program (`HGL_AI_SSH`, default `ssh`).
pub fn lanes(
    cfg: &hgl_ai_config::Config,
    ssh_program: PathBuf,
) -> Result<Vec<Arc<Lane>>, ServiceError> {
    lanes_with(cfg, ssh_program, ssh::Conn::new)
}

/// Construct lanes using the supplied connection factory.
pub fn lanes_with<F>(
    cfg: &hgl_ai_config::Config,
    ssh_program: PathBuf,
    connect: F,
) -> Result<Vec<Arc<Lane>>, ServiceError>
where
    F: Fn(&str, &str, PathBuf, &Path, ssh::Dialect) -> std::io::Result<Arc<ssh::Conn>>,
{
    use hgl_ai_proto::build::builder_inbox;
    let err = |e: &dyn std::fmt::Display| ServiceError::Other(e.to_string());
    let work_root = cfg.work_root().map_err(|e| err(&e))?;
    let data_dir = cfg.data_dir().map_err(|e| err(&e))?;
    let root = work_root.join("builder");
    let logs = root.join("logs");
    let host_id = hgl_ai_bridge::holder::host_id(&data_dir);
    let mut out = Vec::new();
    if cfg.builder.local {
        let exec = crate::exec::local::Local::new(
            root.clone(),
            cfg.builder.sdk.clone(),
            hgl_ai_tree::Git::default(),
        );
        out.push(Arc::new(
            Lane::new(
                cfg.host.clone(),
                builder_inbox(&cfg.host).map_err(|e| err(&e))?,
                Arc::new(exec),
                cfg.builder.concurrency,
                logs.join(&cfg.host),
            )
            .with_host_id(host_id.clone()),
        ));
    }
    for r in &cfg.builder.remote {
        let label = r.label(&cfg.host);
        let dialect = ssh::Dialect::parse(&r.shell)
            .ok_or_else(|| ServiceError::Other(format!("shell {:?}", r.shell)))?;
        let conn = connect(
            &label,
            &r.ssh,
            ssh_program.clone(),
            &data_dir.join("builder").join("ssh"),
            dialect,
        )
        .map_err(|e| err(&e))?;
        let exec = ssh::Ssh::new(
            r.platform,
            dialect,
            &r.work_dir,
            r.sdk.clone(),
            conn.clone(),
        );
        let kick = exec.kick();
        out.push(Arc::new(
            Lane::new(
                label.clone(),
                builder_inbox(&label).map_err(|e| err(&e))?,
                Arc::new(exec),
                r.concurrency,
                logs.join(&label),
            )
            .remote(&cfg.host, conn, kick)
            .with_host_id(host_id.clone()),
        ));
    }
    if out.is_empty() {
        return Err(ServiceError::Other(
            "no builder configured: builder.local is false and there is no [[builder.remote]]"
                .into(),
        ));
    }
    Ok(out)
}

/// The ssh program: `HGL_AI_SSH`, else `ssh`.
pub fn ssh_program() -> PathBuf {
    std::env::var_os("HGL_AI_SSH")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ssh"))
}
