//! Running one turn's command under the cap (`hgl-ai-cap`).
//!
//! The agent's stdin is the turn lock's open file (the lock file is empty), so the lock
//! stays held while the agent lives, even if the bridge itself died: a second turn in
//! that directory cannot start. Stdout is read line by line under a hard per-line bound.
//! Whatever ends the turn (exit, time or memory limit, stop, shutdown, a panic in the
//! bridge) the cap's [`Stopper`] ends the tree, and [`run`] then kills any leftover and
//! returns only once none is left, so a note is never naked while an agent of the turn
//! still runs.
//!
//! The leftovers of a turn (Linux) are the processes of its scope cgroup, those holding
//! the turn lock, and those carrying its environment marker `HGL_AI_TURN`.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write as _};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hgl_ai_cap::{CapCommand, CapSpec, Method, Stopper};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::lock::FileLock;

/// The environment marker of a turn's processes.
pub const MARKER: &str = "HGL_AI_TURN";

/// A command to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
}

/// A request to stop a running turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    None,
    /// The task ended (`landed`, `closed`): stop now.
    Now,
    /// Shutdown: let it finish within the grace, then stop it.
    Graceful(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The command exited by itself (128 + N when signal N ended it).
    Exited(i32),
    TimedOut,
    /// The cap's memory limit was reached.
    MemoryCap,
    /// Stopped because its task ended.
    Stopped,
    /// Still running when the shutdown grace ran out.
    ShutdownKilled,
    SpawnFailed(String),
}

impl Ended {
    pub fn success(&self) -> bool {
        *self == Ended::Exited(0)
    }
}

#[derive(Debug)]
pub struct Run {
    pub ended: Ended,
    /// The last bytes of stderr.
    pub stderr_tail: String,
    /// Stdout lines longer than the bound, skipped.
    pub long_lines: usize,
    /// `false` if some process of the turn outlived every kill (logged).
    pub reaped: bool,
}

/// Limits of one run.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub mem_bytes: u64,
    pub timeout: Duration,
    /// TERM to KILL.
    pub kill_grace: Duration,
    /// Longest stdout line kept; longer lines are skipped.
    pub max_line: usize,
    /// The cap method; `None` detects it.
    pub method: Option<Method>,
}

/// How to find the processes of one turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeId {
    /// The cap's scope cgroup directory.
    pub cgroup: Option<PathBuf>,
    /// Device and inode of the turn lock.
    pub lock: Option<(u64, u64)>,
    /// The value of [`MARKER`] in the turn's environment.
    pub marker: Option<String>,
}

/// Where a running turn publishes its [`TreeId`], so a caller can reap it after a panic.
pub type TreeSlot = Arc<Mutex<Option<TreeId>>>;

const STDERR_TAIL: usize = 16 * 1024;
const DRAIN: Duration = Duration::from_secs(2);
const REAP_WAIT: Duration = Duration::from_secs(10);

/// Run `spec` under the cap; see the module notes. `marker` tags the turn's processes.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    spec: &CmdSpec,
    limits: Limits,
    mut stop: watch::Receiver<Stop>,
    lock: Option<&FileLock>,
    marker: &str,
    slot: Option<&TreeSlot>,
    mut on_line: impl FnMut(&str) + Send,
    mut out_log: Option<std::fs::File>,
    err_log: Option<std::fs::File>,
) -> Run {
    let mut cmd = std::process::Command::new(&spec.program);
    cmd.args(&spec.args).current_dir(&spec.cwd);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd.env(MARKER, marker);
    let stdin = match lock.map(FileLock::share) {
        Some(Ok(f)) => Stdio::from(f),
        _ => Stdio::null(),
    };
    let cap = CapSpec::new(limits.mem_bytes)
        .with_time(limits.timeout)
        .with_kill_after(limits.kill_grace);
    let mut c = CapCommand::new(cap, &cmd)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(m) = limits.method {
        c = c.method(m);
    }
    let mut tree = TreeId {
        cgroup: None,
        lock: lock.and_then(FileLock::id),
        marker: Some(marker.to_string()),
    };
    publish(slot, &tree);
    let mut child = match c.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Run {
                ended: Ended::SpawnFailed(format!("{}: {e}", spec.program.display())),
                stderr_tail: String::new(),
                long_lines: 0,
                reaped: true,
            };
        }
    };
    let root = child.id();
    let scoped = child.method() == Method::Scope;
    let stopper = child.stopper();
    let mut guard = Guard {
        stopper: Some(stopper.clone()),
        tree: tree.clone(),
    };
    let stdout = child.take_stdout().expect("piped");
    let stderr = child.take_stderr().expect("piped");
    let (ltx, mut lrx) = mpsc::channel::<Line>(64);
    let max = limits.max_line;
    let _reader = tokio::task::spawn_blocking(move || read_lines(stdout, max, ltx));
    let err_task = tokio::task::spawn_blocking(move || read_tail(stderr, err_log));
    let mut waiter = tokio::task::spawn_blocking(move || child.wait());

    let mut grace: Option<Instant> = None;
    let mut reason: Option<Ended> = None;
    let mut out_open = true;
    let mut stop_open = true;
    let mut long_lines = 0;
    let mut cg_tick = tokio::time::interval(Duration::from_millis(100));
    let mut on = |l: Line| match l {
        Line::Text(l) => {
            if let Some(f) = out_log.as_mut() {
                let _ = writeln!(f, "{l}");
            }
            on_line(&l);
        }
        Line::TooLong(n) => {
            long_lines += 1;
            tracing::warn!(
                bytes = n,
                max = limits.max_line,
                "skipped an over-long line of agent output"
            );
        }
    };
    let waited = loop {
        tokio::select! {
            l = lrx.recv(), if out_open => match l {
                Some(l) => on(l),
                None => out_open = false,
            },
            r = &mut waiter => break r,
            _ = tokio::time::sleep_until(grace.unwrap_or_else(Instant::now)), if grace.is_some() && reason.is_none() => {
                reason = Some(Ended::ShutdownKilled);
                stopper.stop();
            }
            r = stop.changed(), if stop_open => {
                if r.is_err() {
                    stop_open = false;
                    continue;
                }
                let s = *stop.borrow_and_update();
                match s {
                    Stop::Now if reason.is_none() => {
                        reason = Some(Ended::Stopped);
                        stopper.stop();
                    }
                    Stop::Graceful(g) if grace.is_none() => grace = Some(Instant::now() + g),
                    _ => {}
                }
            }
            _ = cg_tick.tick(), if scoped && tree.cgroup.is_none() => {
                if let Some(cg) = scope_cgroup(root) {
                    tree.cgroup = Some(cg);
                    publish(slot, &tree);
                    guard.tree = tree.clone();
                }
            }
        }
    };
    // The rest of stdout; a helper still holding the pipe cannot hold the turn.
    let _ = tokio::time::timeout(DRAIN, async {
        while out_open {
            match lrx.recv().await {
                Some(l) => on(l),
                None => out_open = false,
            }
        }
    })
    .await;
    drop(lrx);
    let reaped = reap(&tree, limits.kill_grace, REAP_WAIT).await;
    if reaped {
        guard.disarm();
    }
    let stderr_tail = match tokio::time::timeout(DRAIN, err_task).await {
        Ok(Ok(s)) => s,
        _ => String::new(),
    };
    let ended = match waited {
        Ok(Ok(o)) => match reason {
            Some(r) if o.stopped || !o.limit_reached() => r,
            _ if o.memory_limit => Ended::MemoryCap,
            _ if o.time_limit => Ended::TimedOut,
            _ if !o.started => Ended::SpawnFailed(format!(
                "the cap did not start the command (status {})",
                o.status
            )),
            _ => Ended::Exited(o.command_status),
        },
        Ok(Err(e)) => Ended::SpawnFailed(format!("waiting: {e}")),
        Err(e) => Ended::SpawnFailed(format!("waiting: {e}")),
    };
    Run {
        ended,
        stderr_tail,
        long_lines,
        reaped,
    }
}

fn publish(slot: Option<&TreeSlot>, t: &TreeId) {
    if let Some(s) = slot {
        *s.lock().unwrap_or_else(|e| e.into_inner()) = Some(t.clone());
    }
}

/// Ends the tree if [`run`] is dropped or unwinds before reaping it.
struct Guard {
    stopper: Option<Stopper>,
    tree: TreeId,
}

impl Guard {
    fn disarm(&mut self) {
        self.stopper = None;
        self.tree = TreeId::default();
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(s) = &self.stopper {
            s.stop();
        }
        imp::signal(&self.tree, libc::SIGKILL);
    }
}

/// SIGTERM what is left of the tree, wait up to `grace`, SIGKILL, wait up to `wait`.
/// `true` when nothing of it is left.
pub async fn reap(t: &TreeId, grace: Duration, wait: Duration) -> bool {
    if members(t).is_empty() {
        return true;
    }
    imp::signal(t, libc::SIGTERM);
    if gone_within(t, grace).await {
        return true;
    }
    imp::signal(t, libc::SIGKILL);
    if gone_within(t, wait).await {
        return true;
    }
    tracing::error!(left = ?members(t), "processes of a turn survived SIGKILL");
    false
}

async fn gone_within(t: &TreeId, d: Duration) -> bool {
    let end = Instant::now() + d;
    loop {
        if members(t).is_empty() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Live (non-zombie) processes of the tree, never this process.
pub fn members(t: &TreeId) -> Vec<i32> {
    imp::members(t)
}

/// The scope cgroup the cap moved `pid` into, once it has.
fn scope_cgroup(pid: u32) -> Option<PathBuf> {
    let of = |p: &str| {
        let text = std::fs::read_to_string(format!("/proc/{p}/cgroup")).ok()?;
        text.lines()
            .find_map(|l| l.strip_prefix("0::"))
            .map(str::to_string)
    };
    let theirs = of(&pid.to_string())?;
    (theirs.ends_with(".scope") && Some(&theirs) != of("self").as_ref())
        .then(|| PathBuf::from(format!("/sys/fs/cgroup{theirs}")))
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{MARKER, TreeId};
    use std::collections::BTreeSet;
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt;

    pub fn members(t: &TreeId) -> Vec<i32> {
        if t.cgroup.is_none() && t.lock.is_none() && t.marker.is_none() {
            return Vec::new();
        }
        let me = std::process::id() as i32;
        let my_exe = std::fs::read_link("/proc/self/exe").ok();
        let mut out = BTreeSet::new();
        if let Some(cg) = &t.cgroup
            && let Ok(text) = std::fs::read_to_string(cg.join("cgroup.procs"))
        {
            out.extend(text.lines().filter_map(|l| l.trim().parse::<i32>().ok()));
        }
        let marker = t
            .marker
            .as_ref()
            .map(|m| format!("{MARKER}={m}").into_bytes());
        if let Ok(rd) = std::fs::read_dir("/proc") {
            for e in rd.flatten() {
                let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                    continue;
                };
                if pid == me || out.contains(&pid) {
                    continue;
                }
                let Some((_, ppid)) = stat(pid) else { continue };
                // A child of this process between fork and exec holds a copy of every
                // descriptor and this process's environment; it is not of the turn.
                let forking = ppid == me
                    && my_exe.is_some()
                    && std::fs::read_link(format!("/proc/{pid}/exe")).ok() == my_exe;
                if forking {
                    continue;
                }
                if t.lock.is_some_and(|l| holds(pid, l))
                    || marker.as_ref().is_some_and(|m| has_env(pid, m))
                {
                    out.insert(pid);
                }
            }
        }
        out.into_iter()
            .filter(|p| *p != me && *p > 1 && stat(*p).is_some_and(|(s, _)| s != 'Z' && s != 'X'))
            .collect()
    }

    /// `(state, ppid)`.
    fn stat(pid: i32) -> Option<(char, i32)> {
        let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // `pid (comm) state ppid ...`; comm may hold spaces and parens.
        let rest = &s[s.rfind(')')? + 1..];
        let mut f = rest.split_whitespace();
        let state = f.next()?.chars().next()?;
        let ppid = f.next()?.parse().ok()?;
        Some((state, ppid))
    }

    fn holds(pid: i32, lock: (u64, u64)) -> bool {
        let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            return false;
        };
        rd.flatten()
            .any(|e| std::fs::metadata(e.path()).is_ok_and(|m| (m.dev(), m.ino()) == lock))
    }

    fn has_env(pid: i32, needle: &[u8]) -> bool {
        let Ok(f) = std::fs::File::open(format!("/proc/{pid}/environ")) else {
            return false;
        };
        let mut buf = Vec::new();
        if f.take(1 << 20).read_to_end(&mut buf).is_err() {
            return false;
        }
        buf.split(|b| *b == 0).any(|kv| kv == needle)
    }

    pub fn signal(t: &TreeId, sig: libc::c_int) {
        for pid in members(t) {
            // SAFETY: kill(2) only sends a signal; no memory is shared.
            unsafe {
                libc::kill(pid, sig);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    //! Without `/proc` the cap's own tree handling (descendants of the command) is all.
    use super::TreeId;
    pub fn members(_: &TreeId) -> Vec<i32> {
        Vec::new()
    }
    pub fn signal(_: &TreeId, _: libc::c_int) {}
}

enum Line {
    Text(String),
    /// A line over the bound, skipped; its length.
    TooLong(usize),
}

/// Send stdout lines of at most `max` bytes; memory stays bounded whatever the agent
/// prints. Runs on a blocking thread until EOF or until the receiver is gone.
fn read_lines(r: impl Read, max: usize, tx: mpsc::Sender<Line>) {
    let mut r = BufReader::with_capacity(64 * 1024, r);
    let mut buf: Vec<u8> = Vec::new();
    let mut over: Option<usize> = None;
    loop {
        let avail = match r.fill_buf() {
            Ok(a) => a,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if avail.is_empty() {
            break;
        }
        let nl = avail.iter().position(|b| *b == b'\n');
        let n = nl.unwrap_or(avail.len());
        match &mut over {
            Some(skipped) => *skipped += n,
            None if buf.len() + n > max => {
                over = Some(buf.len() + n);
                buf = Vec::new();
            }
            None => buf.extend_from_slice(&avail[..n]),
        }
        r.consume(if nl.is_some() { n + 1 } else { n });
        if nl.is_some() {
            let line = match over.take() {
                Some(skipped) => Line::TooLong(skipped),
                None => Line::Text(String::from_utf8_lossy(&std::mem::take(&mut buf)).into_owned()),
            };
            if tx.blocking_send(line).is_err() {
                return;
            }
        }
    }
    let last = match over {
        Some(skipped) => Some(Line::TooLong(skipped)),
        None if !buf.is_empty() => Some(Line::Text(String::from_utf8_lossy(&buf).into_owned())),
        None => None,
    };
    if let Some(l) = last {
        let _ = tx.blocking_send(l);
    }
}

fn read_tail(mut r: impl Read, mut log: Option<std::fs::File>) -> String {
    let mut tail: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
            Ok(n) => {
                if let Some(f) = log.as_mut() {
                    let _ = f.write_all(&buf[..n]);
                }
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > 2 * STDERR_TAIL {
                    tail.drain(..tail.len() - STDERR_TAIL);
                }
            }
        }
    }
    if tail.len() > STDERR_TAIL {
        tail.drain(..tail.len() - STDERR_TAIL);
    }
    String::from_utf8_lossy(&tail).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, cwd: &std::path::Path) -> CmdSpec {
        CmdSpec {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
            cwd: cwd.to_path_buf(),
        }
    }

    fn limits(timeout_ms: u64, method: Method) -> Limits {
        Limits {
            mem_bytes: 4 << 30,
            timeout: Duration::from_millis(timeout_ms),
            kill_grace: Duration::from_millis(500),
            max_line: 1024,
            method: Some(method),
        }
    }

    fn no_stop() -> watch::Receiver<Stop> {
        watch::channel(Stop::None).1
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 checks existence only; a zombie does not count.
        let exists = unsafe { libc::kill(pid, 0) == 0 };
        exists
            && std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|s| !s.contains(") Z "))
    }

    fn pids(p: &std::path::Path) -> Vec<i32> {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    fn methods() -> Vec<Method> {
        let mut m = vec![Method::Rlimit];
        if Method::detect() == Method::Scope {
            m.push(Method::Scope);
        }
        m
    }

    #[tokio::test]
    async fn lines_exit_and_stderr() {
        let d = tempfile::tempdir().unwrap();
        let mut got = Vec::new();
        let script = "echo a; printf 'b\\nc'; echo oops >&2; exit 3";
        let r = run(
            &sh(script, d.path()),
            limits(10_000, Method::Rlimit),
            no_stop(),
            None,
            "t1",
            None,
            |l| got.push(l.to_string()),
            None,
            None,
        )
        .await;
        assert_eq!(got, ["a", "b", "c"]);
        assert_eq!(r.ended, Ended::Exited(3));
        assert_eq!(r.stderr_tail.trim(), "oops");
        assert!(r.reaped);
    }

    #[tokio::test]
    async fn an_over_long_line_is_skipped_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        let mut got = Vec::new();
        let script = "echo before; head -c 50000 /dev/zero | tr '\\0' x; echo; echo after; head -c 5000 /dev/zero | tr '\\0' y";
        let r = run(
            &sh(script, d.path()),
            limits(10_000, Method::Rlimit),
            no_stop(),
            None,
            "t2",
            None,
            |l| got.push(l.to_string()),
            None,
            None,
        )
        .await;
        assert_eq!(got, ["before", "after"]);
        assert_eq!(r.long_lines, 2);
        assert!(r.ended.success());
    }

    #[tokio::test]
    async fn time_limit_and_leftovers_die_before_run_returns() {
        for m in methods() {
            let d = tempfile::tempdir().unwrap();
            let f = d.path().join("pids");
            // A helper in its own session, one that ignores TERM, and the command itself.
            let script = format!(
                "setsid sleep 60 </dev/null & echo $! >> {p}; (trap '' TERM; exec sleep 60) & echo $! >> {p}; echo $$ >> {p}; sleep 60",
                p = f.display()
            );
            let lock = FileLock::turn(&d.path().join("turn.lock"))
                .unwrap()
                .unwrap();
            let t = std::time::Instant::now();
            let r = run(
                &sh(&script, d.path()),
                limits(400, m),
                no_stop(),
                Some(&lock),
                &format!("t3-{m:?}"),
                None,
                |_| {},
                None,
                None,
            )
            .await;
            assert_eq!(r.ended, Ended::TimedOut, "{m:?}");
            assert!(r.reaped, "{m:?}");
            assert!(t.elapsed() < Duration::from_secs(8));
            let ps = pids(&f);
            assert_eq!(ps.len(), 3);
            for p in ps {
                assert!(!alive(p), "{m:?}: {p} outlived the turn");
            }
        }
    }

    #[tokio::test]
    async fn helpers_left_after_a_clean_exit_are_reaped() {
        for m in methods() {
            let d = tempfile::tempdir().unwrap();
            let f = d.path().join("pids");
            let script = format!(
                "sleep 60 </dev/null >/dev/null 2>&1 & echo $! > {}; exit 0",
                f.display()
            );
            let r = run(
                &sh(&script, d.path()),
                limits(10_000, m),
                no_stop(),
                None,
                &format!("t4-{m:?}"),
                None,
                |_| {},
                None,
                None,
            )
            .await;
            assert!(r.ended.success() && r.reaped, "{m:?}");
            assert!(!alive(pids(&f)[0]), "{m:?}");
        }
    }

    #[tokio::test]
    async fn the_agent_holds_the_turn_lock_through_its_stdin() {
        let d = tempfile::tempdir().unwrap();
        let lp = d.path().join("turn.lock");
        let lock = FileLock::turn(&lp).unwrap().unwrap();
        let (tx, rx) = watch::channel(Stop::None);
        let p = d.path().to_path_buf();
        let h = tokio::spawn(async move {
            let r = run(
                &sh("cat; sleep 30", &p),
                limits(60_000, Method::Rlimit),
                rx,
                Some(&lock),
                "t5",
                None,
                |_| {},
                None,
                None,
            )
            .await;
            drop(lock);
            r
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(FileLock::turn(&lp).unwrap().is_none());
        tx.send(Stop::Now).unwrap();
        assert_eq!(h.await.unwrap().ended, Ended::Stopped);
        assert!(
            FileLock::turn_patiently(&lp).await.unwrap().is_some(),
            "released with the tree"
        );
    }

    #[tokio::test]
    async fn stop_now_and_graceful() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().to_path_buf();
        let go = |script: &'static str, rx| {
            let p = p.clone();
            tokio::spawn(async move {
                run(
                    &sh(script, &p),
                    limits(60_000, Method::Rlimit),
                    rx,
                    None,
                    "t6",
                    None,
                    |_| {},
                    None,
                    None,
                )
                .await
            })
        };
        let (tx, rx) = watch::channel(Stop::None);
        let h = go("sleep 30", rx);
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send(Stop::Now).unwrap();
        assert_eq!(h.await.unwrap().ended, Ended::Stopped);

        let (tx, rx) = watch::channel(Stop::None);
        let h = go("sleep 0.3; exit 0", rx);
        tx.send(Stop::Graceful(Duration::from_secs(5))).unwrap();
        assert!(
            h.await.unwrap().ended.success(),
            "finished within the grace"
        );

        let (tx, rx) = watch::channel(Stop::None);
        let h = go("sleep 30", rx);
        tx.send(Stop::Graceful(Duration::from_millis(100))).unwrap();
        assert_eq!(h.await.unwrap().ended, Ended::ShutdownKilled);
    }

    #[tokio::test]
    async fn a_dropped_run_kills_its_tree() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("pid");
        let script = format!("echo $$ > {}; exec sleep 30", f.display());
        let p = d.path().to_path_buf();
        let slot: TreeSlot = Arc::default();
        let s2 = slot.clone();
        let h = tokio::spawn(async move {
            run(
                &sh(&script, &p),
                limits(60_000, Method::Rlimit),
                no_stop(),
                None,
                "t7",
                Some(&s2),
                |_| {},
                None,
                None,
            )
            .await
        });
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while pids(&f).is_empty() && std::time::Instant::now() < end {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        h.abort();
        let _ = h.await;
        let t = slot.lock().unwrap().clone().expect("published");
        assert!(reap(&t, Duration::from_millis(100), Duration::from_secs(5)).await);
        let pid = pids(&f)[0];
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while alive(pid) && std::time::Instant::now() < end {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive(pid));
    }

    #[tokio::test]
    async fn spawn_failure() {
        let d = tempfile::tempdir().unwrap();
        let spec = CmdSpec {
            program: "/nonexistent/agent".into(),
            args: vec![],
            env: vec![],
            cwd: d.path().into(),
        };
        let r = run(
            &spec,
            limits(1000, Method::Rlimit),
            no_stop(),
            None,
            "t8",
            None,
            |_| {},
            None,
            None,
        )
        .await;
        assert!(matches!(r.ended, Ended::SpawnFailed(_)), "{:?}", r.ended);
    }
}
