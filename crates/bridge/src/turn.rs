//! One turn: where it runs, preparing its tree, running the adapter, judging the result.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hgl_ai_proto::{Kind, Note, Role, TaskKey};
use hgl_ai_tree::{Create, TASK_FILE};
use tokio::sync::watch;

use crate::adapter::{self, Parser, TurnInput};
use crate::bus::BusApi;
use crate::lock::{self, FileLock};
use crate::proc::{self, Ended, Limits, Stop, TreeId, TreeSlot};
use crate::{Bridge, SeatRt, prompt};

/// Where a turn runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    /// `<work_root>/<seat>/_inbox`: notes that belong to no tree of the seat.
    Inbox,
    Tree {
        task: TaskKey,
        review: bool,
    },
}

impl Target {
    pub fn task(&self) -> Option<&TaskKey> {
        match self {
            Target::Inbox => None,
            Target::Tree { task, .. } => Some(task),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Inbox => f.write_str("the inbox directory"),
            Target::Tree {
                task,
                review: false,
            } => write!(f, "{}#{}", task.repo, task.issue),
            Target::Tree { task, review: true } => {
                write!(f, "the review tree of {}#{}", task.repo, task.issue)
            }
        }
    }
}

/// A note's target, and the tree it asks to create.
pub struct Plan {
    pub target: Target,
    pub create: Option<Create>,
}

/// Of a group's creates: the last review (latest head), else the first implement.
pub fn choose_create(creates: Vec<Option<Create>>) -> Option<Create> {
    let (mut review, mut implement) = (None, None);
    for c in creates.into_iter().flatten() {
        match c {
            Create::Review { .. } => review = Some(c),
            Create::Implement { .. } if implement.is_none() => implement = Some(c),
            Create::Implement { .. } => {}
        }
    }
    review.or(implement)
}

pub struct Job {
    pub seat: Arc<SeatRt>,
    pub target: Target,
    pub create: Option<Create>,
    /// (seq, delivery count, note)
    pub notes: Vec<(u64, u64, Note)>,
    pub hold: Option<String>,
    pub mode: Option<String>,
    pub binding_idx: usize,
    pub stop: watch::Receiver<Stop>,
    pub slot: TreeSlot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Clean,
    Quota(String),
    Failed(String),
    /// Stopped because its task ended.
    Stopped,
    Shutdown,
    /// Another process holds the turn lock (who, as far as known).
    Busy(String),
    PrepFailed(String),
}

#[derive(Debug)]
pub struct TurnOut {
    pub outcome: Outcome,
    pub binding_idx: usize,
}

fn first_line(body: &str) -> String {
    body.lines().next().unwrap_or("").to_string()
}

impl<B: BusApi> Bridge<B> {
    pub(crate) fn plan(&self, seat: &SeatRt, n: &Note) -> Plan {
        let inbox = Plan {
            target: Target::Inbox,
            create: None,
        };
        let Some(task) = n.task() else { return inbox };
        let tree = |review| Target::Tree {
            task: task.clone(),
            review,
        };
        match n.kind {
            Kind::Assign => Plan {
                target: tree(false),
                create: Some(Create::Implement {
                    title: first_line(&n.body),
                    base: n.base.clone(),
                }),
            },
            Kind::Review => match &n.head {
                Some(h) => Plan {
                    target: tree(true),
                    create: Some(Create::Review {
                        head: h.clone(),
                        base: n.base.clone(),
                    }),
                },
                None => inbox,
            },
            _ if self.trees.existing(seat.name(), &task, true).is_some() => Plan {
                target: tree(true),
                create: None,
            },
            _ if self.trees.existing(seat.name(), &task, false).is_some() => Plan {
                target: tree(false),
                create: None,
            },
            // A sendback or approval for a tree this host no longer has (a moved seat):
            // recreate it; the pushed branch is resumed.
            Kind::Sendback | Kind::Approved if seat.cfg.role == Role::Implementor => Plan {
                target: tree(false),
                create: Some(Create::Implement {
                    title: first_line(&n.body),
                    base: n.base.clone(),
                }),
            },
            _ => inbox,
        }
    }

    pub(crate) async fn turn(self: Arc<Self>, job: Job) -> TurnOut {
        let binding_idx = job.binding_idx;
        let outcome = self.turn_inner(&job).await;
        TurnOut {
            outcome,
            binding_idx,
        }
    }

    async fn turn_inner(&self, job: &Job) -> Outcome {
        let seat = &job.seat;
        let name = seat.name().clone();
        // The directory, its tree, and its turn lock.
        let (dir, held, file) = match &job.target {
            Target::Inbox => {
                let trees = self.trees.clone();
                let n = name.clone();
                let dir = match tokio::task::spawn_blocking(move || trees.ensure_inbox(&n)).await {
                    Ok(Ok(d)) => d,
                    Ok(Err(e)) => return Outcome::PrepFailed(e.to_string()),
                    Err(e) => return Outcome::PrepFailed(e.to_string()),
                };
                match take_lock(&dir).await {
                    Ok(l) => (dir, l, None),
                    Err(o) => return o,
                }
            }
            Target::Tree { task, review } => {
                let dir = match self.trees.layout().task_dir(&name, task, *review) {
                    Ok(d) => d,
                    Err(e) => return Outcome::PrepFailed(e.to_string()),
                };
                let mut held = None;
                if dir.exists() {
                    match take_lock(&dir).await {
                        Ok(l) => held = Some(l),
                        Err(o) => return o,
                    }
                }
                if *review
                    && matches!(job.create, Some(Create::Review { .. }))
                    && dir.join(TASK_FILE).exists()
                {
                    // Each review starts from a fresh tree and session.
                    let trees = self.trees.clone();
                    let (s, t) = (name.clone(), task.clone());
                    match tokio::task::spawn_blocking(move || trees.remove(&s, &t, true)).await {
                        Ok(Ok(Some(rm))) => {
                            let _ = self.bus.tree_del(&rm.key).await;
                        }
                        Ok(Ok(None)) => {}
                        Ok(Err(e)) => {
                            return Outcome::PrepFailed(format!(
                                "removing the previous review tree: {e}"
                            ));
                        }
                        Err(e) => return Outcome::PrepFailed(e.to_string()),
                    }
                }
                let file = match &job.create {
                    Some(c) => {
                        let trees = self.trees.clone();
                        let (s, t, c) = (name.clone(), task.clone(), c.clone());
                        match tokio::task::spawn_blocking(move || trees.create(&s, &t, &c)).await {
                            Ok(Ok(created)) => {
                                if !created.reused
                                    && let Err(e) = self.bus.tree_put(&created.record).await
                                {
                                    tracing::warn!(key = %created.record.key(), "registering: {e}");
                                }
                                created.file
                            }
                            Ok(Err(e)) => return Outcome::PrepFailed(e.to_string()),
                            Err(e) => return Outcome::PrepFailed(e.to_string()),
                        }
                    }
                    None => match hgl_ai_tree::read_task(&dir) {
                        Ok(f) => f,
                        Err(e) => return Outcome::PrepFailed(e.to_string()),
                    },
                };
                // The tree may be new (or recreated): hold the lock of the file now there.
                let held = match held.filter(|l| same_file(l, &dir.join(lock::TURN_LOCK))) {
                    Some(l) => l,
                    None => match take_lock(&dir).await {
                        Ok(l) => l,
                        Err(o) => return o,
                    },
                };
                (dir, held, Some(file))
            }
        };
        match *job.stop.borrow() {
            Stop::Now => return Outcome::Stopped,
            Stop::Graceful(_) => return Outcome::Shutdown,
            Stop::None => {}
        }
        let binding = seat.binding_at(job.binding_idx);
        let r = match self.cfg.resolve(&binding) {
            Ok(r) => r,
            Err(e) => return Outcome::PrepFailed(format!("seat binding: {e}")),
        };
        let (cwd, writable) = match &job.target {
            Target::Inbox => (dir.clone(), vec![dir.clone()]),
            Target::Tree { task, .. } => {
                let mut w = vec![dir.clone()];
                if let Ok(c) = self.trees.layout().clone_dir(&task.repo) {
                    w.push(c);
                }
                (dir.join("src"), w)
            }
        };
        let resume = file
            .as_ref()
            .and_then(|f| f.sessions.get(r.runtime.as_str()))
            .cloned();
        let items: Vec<prompt::Item> = job.notes.iter().map(|(s, d, n)| (*s, *d, n)).collect();
        let text = prompt::build(
            &prompt::Context {
                host: &self.cfg.host,
                seat: &name,
                role: seat.cfg.role,
                runtime: r.runtime,
                family: r.family,
                model: &r.model,
                task: file.as_ref(),
                hold: job.hold.as_deref(),
                mode: job.mode.as_deref(),
            },
            &items,
        );
        let program = self
            .opts
            .bins
            .get(&r.runtime)
            .cloned()
            .unwrap_or_else(|| PathBuf::from(r.runtime.as_str()));
        let input = TurnInput {
            prompt: &text,
            resolved: &r,
            resume: resume.as_deref(),
            cwd: &cwd,
            writable: &writable,
        };
        let mut spec = adapter::command(&program, &input);
        let env: [(&str, OsString); 5] = [
            ("TMPDIR", dir.join("tmp").into()),
            ("CARGO_TARGET_DIR", dir.join("build").into()),
            ("HGL_AI_TASK_DIR", dir.clone().into()),
            ("HGL_AI_SEAT", name.as_str().into()),
            ("HGL_AI_ROLE", seat.cfg.role.as_str().into()),
        ];
        spec.env.extend(env.into_iter().map(|(k, v)| (k.into(), v)));
        let mem_gb = seat.cfg.memcap_gb.unwrap_or(self.cfg.limits.memcap_gb);
        let (out_log, err_log) =
            open_logs(&dir.join("log"), r.runtime.as_str(), self.opts.keep_logs);
        tracing::info!(seat = %name, target = %job.target, runtime = %r.runtime, model = %r.model, resume = resume.is_some(), "turn starting");
        let mut parser = Parser::new(r.runtime);
        let limits = Limits {
            mem_bytes: mem_gb.saturating_mul(1 << 30),
            timeout: self.opts.turn_timeout,
            kill_grace: self.opts.kill_grace,
            max_line: self.opts.max_line,
            method: self.opts.cap_method,
        };
        let marker = self.next_marker(&name);
        let run = proc::run(
            &spec,
            limits,
            job.stop.clone(),
            Some(&held),
            &marker,
            Some(&job.slot),
            |l| parser.line(l),
            out_log,
            err_log,
        )
        .await;
        if run.long_lines > 0 {
            tracing::warn!(seat = %name, lines = run.long_lines, "over-long output lines were skipped");
        }
        let exited_ok = run.ended.success();
        let parsed = parser.finish(exited_ok, &run.stderr_tail);
        if let (Some(id), Target::Tree { .. }) = (&parsed.session_id, &job.target)
            && let Err(e) = save_session(&dir, r.runtime.as_str(), id)
        {
            tracing::warn!(dir = %dir.display(), "saving the session id: {e}");
        }
        if !run.reaped {
            tracing::error!(seat = %name, "a process of the turn survived; the turn lock stays held by it");
        }
        drop(held);
        let err = parsed.error.clone().unwrap_or_default();
        match run.ended {
            Ended::Stopped => Outcome::Stopped,
            Ended::ShutdownKilled => Outcome::Shutdown,
            Ended::TimedOut => Outcome::Failed(format!("time limit {:?}", self.opts.turn_timeout)),
            Ended::MemoryCap => Outcome::Failed(format!("memory cap {mem_gb} GiB reached")),
            Ended::SpawnFailed(e) => Outcome::Failed(e),
            Ended::Exited(_) if parsed.success(exited_ok) => Outcome::Clean,
            Ended::Exited(_) if parsed.quota => Outcome::Quota(if err.is_empty() {
                "quota refusal".into()
            } else {
                err
            }),
            Ended::Exited(code) => {
                let tail = run
                    .stderr_tail
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string();
                Outcome::Failed(format!("exit {code}: {err} {tail}").trim().to_string())
            }
        }
    }
}

/// Take `<dir>/turn.lock`, or the `Busy` outcome when another process holds it.
async fn take_lock(dir: &Path) -> Result<FileLock, Outcome> {
    let p = dir.join(lock::TURN_LOCK);
    match FileLock::turn_patiently(&p).await {
        Ok(Some(l)) => Ok(l),
        Ok(None) => Err(Outcome::Busy(holders(&p))),
        Err(e) => Err(Outcome::PrepFailed(format!("{}: {e}", p.display()))),
    }
}

fn same_file(l: &FileLock, p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(p).is_ok_and(|m| l.id() == Some((m.dev(), m.ino())))
    }
    #[cfg(not(unix))]
    {
        let _ = (l, p);
        true
    }
}

fn save_session(dir: &Path, runtime: &str, id: &str) -> hgl_ai_tree::Result<()> {
    let mut f = hgl_ai_tree::read_task(dir)?;
    if f.sessions.get(runtime).map(String::as_str) != Some(id) {
        f.sessions.insert(runtime.to_string(), id.to_string());
        hgl_ai_tree::write_task(dir, &f)?;
    }
    Ok(())
}

/// `<dir>/<UTC>-<runtime>.out|.err`, keeping the newest `keep` turns.
fn open_logs(
    dir: &Path,
    runtime: &str,
    keep: usize,
) -> (Option<std::fs::File>, Option<std::fs::File>) {
    if std::fs::create_dir_all(dir).is_err() {
        return (None, None);
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        names.sort();
        let excess = names.len().saturating_sub(keep.saturating_sub(1) * 2);
        for p in names.into_iter().take(excess) {
            let _ = std::fs::remove_file(p);
        }
    }
    let stem = format!("{}-{runtime}", hgl_ai_proto::utc_now_stamp());
    let open = |ext: &str| std::fs::File::create(dir.join(format!("{stem}.{ext}"))).ok();
    (open("out"), open("err"))
}

/// Who holds a turn lock: the live holders where they can be found, else the pid the
/// lock file names (the bridge that started that turn).
fn holders(p: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(m) = std::fs::metadata(p) {
            let pids = proc::members(&TreeId {
                lock: Some((m.dev(), m.ino())),
                ..TreeId::default()
            });
            if !pids.is_empty() {
                return format!("pids {pids:?}");
            }
        }
    }
    match lock::holder(p) {
        Some(pid) => format!("the lock file names pid {pid}"),
        None => "unknown".into(),
    }
}

/// Kill whatever still holds a turn lock (a stray of an earlier turn). `true` when none is left.
pub async fn reap_lock_holders(path: &Path, grace: Duration) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(m) = std::fs::metadata(path) else {
            return true;
        };
        let t = TreeId {
            lock: Some((m.dev(), m.ino())),
            ..TreeId::default()
        };
        tracing::warn!(lock = %path.display(), holders = ?proc::members(&t), "killing the holders of a turn lock");
        proc::reap(&t, grace, Duration::from_secs(10)).await
    }
    #[cfg(not(unix))]
    {
        let _ = (path, grace);
        true
    }
}
