//! The builder service (BUILD-6, `hgl-ai builder run`): one lane per builder this host
//! runs (its own platform, and each remote it drives as a proxy). A lane takes `build`
//! jobs from its inbox `builder-<label>`, checks out the requested head in a job
//! directory of its own, runs the recipe, keeps the summary in `HGL_BUILD/run.<id>`,
//! sends the `receipt`, and removes the checkout. It advertises itself in
//! `HGL_HOSTS/builder.<label>` with its health, and honours `drain` and `retire`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hgl_ai_bus::Bus;
use hgl_ai_proto::build::{BuildJob, BuildStatus, BuilderRecord, Summary};
use hgl_ai_proto::{Kind, Seat, utc_now_stamp};
use tokio::sync::{Notify, watch};

use crate::driver::{self, Meta, Options};
use crate::exec::{Cancel, Checkout, Executor, Prep, Probe, Workspace, ssh};
use crate::store;

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
    gate: Mutex<Gate>,
    probe: Mutex<Probe>,
    /// Workspaces whose cleanup waits for the remote to come back.
    stale: Mutex<Vec<Workspace>>,
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
            gate: Mutex::new(Gate::Active),
            probe: Mutex::new(Probe::default()),
            stale: Mutex::new(Vec::new()),
        }
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
    let mut tasks = Vec::new();
    for lane in lanes {
        let (bus, settings, shutdown) = (bus.clone(), settings.clone(), shutdown.clone());
        tasks.push(tokio::spawn(async move {
            lane_loop(bus, lane, settings, shutdown).await
        }));
    }
    let mut first_err = None;
    for t in tasks {
        match t.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                first_err.get_or_insert(e);
            }
            Err(e) => {
                first_err.get_or_insert(ServiceError::Other(format!("a lane stopped: {e}")));
            }
        }
    }
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
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ServiceError> {
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
        let (bus, lane, settings, mut stop) = (
            bus.clone(),
            lane.clone(),
            settings.clone(),
            shutdown.clone(),
        );
        tokio::spawn(async move {
            let mut beat = 0u32;
            let mut cleaned = false;
            let mut was_reachable = false;
            loop {
                let gate = read_gate(&bus, &lane.label).await;
                *lane.gate.lock().unwrap_or_else(|e| e.into_inner()) = gate;
                let reachable = lane.reachable();
                if reachable && !cleaned {
                    // Leftovers of an earlier run of this builder.
                    let ex = lane.exec.clone();
                    if blocking(move || ex.clean_all()).await.is_ok() {
                        cleaned = true;
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
                was_reachable = reachable;
                beat = beat.wrapping_add(1);
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
    let mut inbox = bus.inbox(&lane.inbox).await?;
    let running: Arc<Mutex<Vec<Cancel>>> = Arc::new(Mutex::new(Vec::new()));
    let mut jobs = tokio::task::JoinSet::new();
    let idle = Arc::new(Notify::new());
    loop {
        if *shutdown.borrow() {
            break;
        }
        while jobs.try_join_next().is_some() {}
        let open = lane.gate() == Gate::Active && lane.reachable() && lane.busy() < lane.slots;
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
                let out = run_job(&bus, &lane, &settings, &job, &cancel).await;
                keep.abort();
                match out {
                    Some(s) => {
                        finish(&bus, &lane, &job, &s).await;
                        let _ = d.ack().await;
                    }
                    None => {
                        // Stopped by a shutdown: back to the inbox.
                        let _ = d.nak(Duration::from_secs(15)).await;
                    }
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
    hb.abort();
    let _ = hb.await;
    let _ = conn_stop_tx.send(true);
    if let Some(s) = supervisor {
        let _ = s.await;
    }
    if let Err(e) = bus.builder_del(&lane.label).await {
        tracing::warn!(builder = %lane.label, "removing the builder record: {e}");
    }
    Ok(())
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

async fn retry_stale(lane: &Arc<Lane>) {
    let stale: Vec<Workspace> =
        std::mem::take(&mut *lane.stale.lock().unwrap_or_else(|e| e.into_inner()));
    for ws in stale {
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
        dirty: false,
        host: lane.label.clone(),
        proxy: lane.proxy.clone(),
        requester: Some(job.requester.clone()),
        recipe_rev: job.recipe_rev,
    };
    let log_dir = lane.logs.join(&job.id);
    let mut s0 = driver::start(&meta, lane.exec.as_ref(), &log_dir, BuildStatus::Running);
    if hgl_ai_proto::build::check_build_id(&job.id).is_err()
        || hgl_ai_proto::check_head(&job.head).is_err()
    {
        return Some(driver::not_run(s0, "the job's id or head is malformed"));
    }
    let _ = store::run_put(bus, &s0).await;
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
    };
    let mut retried = false;
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        let (ex, c) = (lane.exec.clone(), checkout.clone());
        let ws = match blocking(move || ex.prepare(&c)).await {
            Ok(ws) => ws,
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
        if cancel.is_cancelled() && driven.summary.status == BuildStatus::Stopped {
            let mut s = driven.summary;
            s.reason = Some("stopped by the builder's shutdown; the job is queued again".into());
            let _ = store::run_put(bus, &s).await;
            return None;
        }
        return Some(driven.summary);
    }
}

/// Keep the summary (log directory and hub), send the receipts, prune old logs.
async fn finish(bus: &Bus, lane: &Lane, job: &BuildJob, s: &Summary) {
    let dir = lane.logs.join(&job.id);
    if let Err(e) = store::write_final(&dir, s) {
        tracing::warn!("writing {}: {e}", dir.display());
    }
    if let Err(e) = store::run_put(bus, s).await {
        tracing::warn!("writing run {}: {e}", s.id);
    }
    let mut to = vec![job.reply_to.clone()];
    to.extend(job.notify.clone());
    for seat in to {
        if let Err(e) = store::send_receipt(bus, &lane.inbox, &seat, s, job.issue).await {
            tracing::warn!("sending the receipt of {} to {seat}: {e}", s.id);
        }
    }
    prune_logs(&lane.logs, LOGS_KEPT);
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
    use hgl_ai_proto::build::builder_inbox;
    let err = |e: &dyn std::fmt::Display| ServiceError::Other(e.to_string());
    let work_root = cfg.work_root().map_err(|e| err(&e))?;
    let data_dir = cfg.data_dir().map_err(|e| err(&e))?;
    let root = work_root.join("builder");
    let logs = root.join("logs");
    let mut out = Vec::new();
    if cfg.builder.local {
        let exec = crate::exec::local::Local::new(
            root.clone(),
            cfg.builder.sdk.clone(),
            hgl_ai_tree::Git::default(),
        );
        out.push(Arc::new(Lane::new(
            cfg.host.clone(),
            builder_inbox(&cfg.host).map_err(|e| err(&e))?,
            Arc::new(exec),
            cfg.builder.concurrency,
            logs.join(&cfg.host),
        )));
    }
    for r in &cfg.builder.remote {
        let label = r.label(&cfg.host);
        let dialect = ssh::Dialect::parse(&r.shell)
            .ok_or_else(|| ServiceError::Other(format!("shell {:?}", r.shell)))?;
        let conn = ssh::Conn::new(
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
            .remote(&cfg.host, conn, kick),
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
