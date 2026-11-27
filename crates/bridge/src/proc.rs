//! Running one turn's command.
//!
//! The agent starts in a new session (`setsid`), inherits the turn lock's descriptor,
//! and its stdout is read line by line under a hard per-line bound. Whatever ends the
//! turn (exit, time limit, stop, shutdown, a panic in the bridge) the whole process
//! tree is killed, and [`run`] returns only after it is gone, so a note is never naked
//! while an agent of the turn still runs.
//!
//! The tree is every process in the turn's session or process group and, on Linux,
//! every process holding the turn lock (a helper that left the session but kept the
//! inherited descriptor).

use std::ffi::OsString;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::lock::FileLock;

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
    /// The process exited by itself: its code, or the signal that ended it.
    Exited { code: Option<i32>, signal: Option<i32> },
    TimedOut,
    /// Stopped because its task ended.
    Stopped,
    /// Still running when the shutdown grace ran out.
    ShutdownKilled,
    SpawnFailed(String),
}

impl Ended {
    pub fn success(&self) -> bool {
        matches!(self, Ended::Exited { code: Some(0), .. })
    }
}

#[derive(Debug)]
pub struct Run {
    pub ended: Ended,
    /// The last bytes of stderr.
    pub stderr_tail: String,
    /// Stdout lines longer than the bound, skipped.
    pub long_lines: usize,
    /// `false` if some process of the tree outlived every kill (logged).
    pub reaped: bool,
}

/// Limits of one run.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub timeout: Duration,
    /// SIGTERM to SIGKILL.
    pub kill_grace: Duration,
    /// Longest stdout line kept; longer lines are skipped.
    pub max_line: usize,
}

/// The processes of one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeId {
    /// Session and process group id (the leader's pid).
    pub sid: i32,
    /// Device and inode of the turn lock.
    pub lock: Option<(u64, u64)>,
}

/// Where a running turn publishes its [`TreeId`], so a caller can reap it after a panic.
pub type TreeSlot = Arc<Mutex<Option<TreeId>>>;

const STDERR_TAIL: usize = 16 * 1024;
const DRAIN: Duration = Duration::from_secs(2);
const REAP_WAIT: Duration = Duration::from_secs(10);

/// Run `spec`, calling `on_line` for each stdout line and copying stdout and stderr to the
/// optional logs; see the module notes for how the tree is ended.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    spec: &CmdSpec,
    limits: Limits,
    mut stop: watch::Receiver<Stop>,
    lock: Option<&FileLock>,
    slot: Option<&TreeSlot>,
    mut on_line: impl FnMut(&str) + Send,
    mut out_log: Option<std::fs::File>,
    err_log: Option<std::fs::File>,
) -> Run {
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    let lock_id = {
        let fd = lock.map(|l| l.fd());
        // SAFETY: setsid(2) and fcntl(2) are async-signal-safe; nothing else runs between
        // fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(fd) = fd {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags >= 0 {
                        libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                    }
                }
                Ok(())
            });
        }
        lock.and_then(FileLock::id)
    };
    #[cfg(not(unix))]
    let lock_id = {
        let _ = lock;
        None
    };
    let mut child = match cmd.spawn() {
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
    let tree = child.id().and_then(|p| i32::try_from(p).ok()).map(|sid| TreeId { sid, lock: lock_id });
    if let (Some(slot), Some(t)) = (slot, tree) {
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
    }
    let mut guard = Guard(tree);
    let mut lines = Lines::new(child.stdout.take().expect("piped"), limits.max_line);
    let err_task = tokio::spawn(read_tail(child.stderr.take().expect("piped"), err_log));

    let mut deadline = Instant::now() + limits.timeout;
    let mut on_deadline = Ended::TimedOut;
    let mut reason: Option<Ended> = None;
    let mut killing = false;
    let mut out_open = true;
    let mut stop_open = true;
    let mut long_lines = 0;
    let mut on = |l: Line| match l {
        Line::Text(l) => {
            if let Some(f) = out_log.as_mut() {
                let _ = writeln!(f, "{l}");
            }
            on_line(&l);
        }
        Line::TooLong(n) => {
            long_lines += 1;
            tracing::warn!(bytes = n, max = limits.max_line, "skipped an over-long line of agent output");
        }
    };
    let status = loop {
        tokio::select! {
            l = lines.next(), if out_open => match l {
                Ok(Some(l)) => on(l),
                _ => out_open = false,
            },
            st = child.wait() => break st,
            _ = tokio::time::sleep_until(deadline) => {
                if killing {
                    signal_tree(tree, Sig::Kill);
                    deadline = Instant::now() + Duration::from_secs(3600);
                } else {
                    reason.get_or_insert(on_deadline.clone());
                    signal_tree(tree, Sig::Term);
                    killing = true;
                    deadline = Instant::now() + limits.kill_grace;
                }
            }
            r = stop.changed(), if stop_open => {
                if r.is_err() {
                    stop_open = false;
                    continue;
                }
                let s = *stop.borrow_and_update();
                match s {
                    Stop::Now if !killing => {
                        reason = Some(Ended::Stopped);
                        signal_tree(tree, Sig::Term);
                        killing = true;
                        deadline = Instant::now() + limits.kill_grace;
                    }
                    Stop::Graceful(g) if !killing => {
                        on_deadline = Ended::ShutdownKilled;
                        deadline = deadline.min(Instant::now() + g);
                    }
                    _ => {}
                }
            }
        }
    };
    // The rest of stdout; a helper still holding the pipe cannot hold the turn.
    let _ = tokio::time::timeout(DRAIN, async {
        while out_open {
            match lines.next().await {
                Ok(Some(l)) => on(l),
                _ => out_open = false,
            }
        }
    })
    .await;
    let reaped = match tree {
        Some(t) => reap(t, limits.kill_grace, REAP_WAIT).await,
        None => true,
    };
    if reaped {
        guard.0 = None;
    }
    let stderr_tail = match tokio::time::timeout(DRAIN, err_task).await {
        Ok(Ok(s)) => s,
        _ => String::new(),
    };
    let ended = match (reason, status) {
        (Some(r), _) => r,
        (None, Ok(st)) => Ended::Exited { code: st.code(), signal: exit_signal(&st) },
        (None, Err(e)) => Ended::SpawnFailed(format!("waiting: {e}")),
    };
    Run { ended, stderr_tail, long_lines, reaped }
}

/// Kills the tree if [`run`] is dropped or unwinds before reaping it.
struct Guard(Option<TreeId>);

impl Drop for Guard {
    fn drop(&mut self) {
        signal_tree(self.0, Sig::Kill);
    }
}

/// SIGTERM the tree, wait up to `grace`, SIGKILL what is left, wait up to `wait`.
/// `true` when nothing of it is left.
pub async fn reap(t: TreeId, grace: Duration, wait: Duration) -> bool {
    if members(t).is_empty() {
        return true;
    }
    signal_tree(Some(t), Sig::Term);
    if gone_within(t, grace).await {
        return true;
    }
    signal_tree(Some(t), Sig::Kill);
    if gone_within(t, wait).await {
        return true;
    }
    tracing::error!(sid = t.sid, left = ?members(t), "processes of a turn survived SIGKILL");
    false
}

async fn gone_within(t: TreeId, d: Duration) -> bool {
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
pub fn members(t: TreeId) -> Vec<i32> {
    imp::members(t)
}

enum Sig {
    Term,
    Kill,
}

fn signal_tree(t: Option<TreeId>, sig: Sig) {
    if let Some(t) = t {
        imp::signal(t, sig);
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{Sig, TreeId};
    use std::os::unix::fs::MetadataExt;

    pub fn members(t: TreeId) -> Vec<i32> {
        let me = std::process::id() as i32;
        let my_exe = std::fs::read_link("/proc/self/exe").ok();
        let Ok(rd) = std::fs::read_dir("/proc") else { return Vec::new() };
        let mut out = Vec::new();
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
            if pid == me || pid <= 1 {
                continue;
            }
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
            // `pid (comm) state ppid pgrp session ...`; comm may hold spaces and parens.
            let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else { continue };
            let f: Vec<&str> = rest.split_whitespace().collect();
            if f.len() < 4 || f[0] == "Z" || f[0] == "X" {
                continue;
            }
            let (ppid, pgrp, session) =
                (f[1].parse::<i32>().unwrap_or(0), f[2].parse::<i32>().unwrap_or(0), f[3].parse::<i32>().unwrap_or(0));
            let in_session = t.sid > 1 && (pgrp == t.sid || session == t.sid);
            // A child of this process between fork and exec holds a copy of every descriptor,
            // the lock's included; it is not a holder (a turn's own children are in its session).
            let forking = ppid == me && my_exe.is_some() && std::fs::read_link(format!("/proc/{pid}/exe")).ok() == my_exe;
            if in_session || (!forking && t.lock.is_some_and(|l| holds(pid, l))) {
                out.push(pid);
            }
        }
        out
    }

    fn holds(pid: i32, lock: (u64, u64)) -> bool {
        let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { return false };
        rd.flatten().any(|e| std::fs::metadata(e.path()).is_ok_and(|m| (m.dev(), m.ino()) == lock))
    }

    pub fn signal(t: TreeId, sig: Sig) {
        let sig = match sig {
            Sig::Term => libc::SIGTERM,
            Sig::Kill => libc::SIGKILL,
        };
        // SAFETY: kill(2) only sends a signal; no memory is shared.
        unsafe {
            if t.sid > 1 {
                libc::kill(-t.sid, sig);
            }
            for pid in members(t) {
                libc::kill(pid, sig);
            }
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
mod imp {
    use super::{Sig, TreeId};

    pub fn members(t: TreeId) -> Vec<i32> {
        // SAFETY: signal 0 only checks that the process group exists.
        let alive = t.sid > 1 && unsafe { libc::kill(-t.sid, 0) } == 0;
        if alive { vec![-t.sid] } else { Vec::new() }
    }

    pub fn signal(t: TreeId, sig: Sig) {
        let sig = match sig {
            Sig::Term => libc::SIGTERM,
            Sig::Kill => libc::SIGKILL,
        };
        if t.sid > 1 {
            // SAFETY: kill(2) only sends a signal; no memory is shared.
            unsafe {
                libc::kill(-t.sid, sig);
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{Sig, TreeId};
    pub fn members(_: TreeId) -> Vec<i32> {
        Vec::new()
    }
    pub fn signal(_: TreeId, _: Sig) {}
}

#[cfg(unix)]
fn exit_signal(st: &std::process::ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(st)
}

#[cfg(not(unix))]
fn exit_signal(_: &std::process::ExitStatus) -> Option<i32> {
    None
}

enum Line {
    Text(String),
    /// A line over the bound, skipped; its length.
    TooLong(usize),
}

/// Stdout lines, each at most `max` bytes; memory stays bounded whatever the agent prints.
/// Cancel-safe: the only await is `fill_buf`, and state lives in `self`.
struct Lines<R> {
    r: BufReader<R>,
    buf: Vec<u8>,
    over: bool,
    skipped: usize,
    max: usize,
}

impl<R: AsyncRead + Unpin> Lines<R> {
    fn new(r: R, max: usize) -> Self {
        Lines { r: BufReader::with_capacity(64 * 1024, r), buf: Vec::new(), over: false, skipped: 0, max }
    }

    async fn next(&mut self) -> std::io::Result<Option<Line>> {
        loop {
            let avail = self.r.fill_buf().await?;
            if avail.is_empty() {
                return Ok(self.take());
            }
            let nl = avail.iter().position(|b| *b == b'\n');
            let n = nl.unwrap_or(avail.len());
            if self.over {
                self.skipped += n;
            } else if self.buf.len() + n > self.max {
                self.over = true;
                self.skipped = self.buf.len() + n;
                self.buf = Vec::new();
            } else {
                self.buf.extend_from_slice(&avail[..n]);
            }
            self.r.consume(if nl.is_some() { n + 1 } else { n });
            if nl.is_some() {
                return Ok(Some(self.take().unwrap_or(Line::Text(String::new()))));
            }
        }
    }

    fn take(&mut self) -> Option<Line> {
        if self.over {
            self.over = false;
            return Some(Line::TooLong(std::mem::take(&mut self.skipped)));
        }
        if self.buf.is_empty() {
            return None;
        }
        let l = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        if self.buf.capacity() > 1 << 20 {
            self.buf = Vec::new();
        }
        Some(Line::Text(l))
    }
}

async fn read_tail(mut r: impl AsyncRead + Unpin, mut log: Option<std::fs::File>) -> String {
    let mut tail: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
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
        CmdSpec { program: "sh".into(), args: vec!["-c".into(), script.into()], env: vec![], cwd: cwd.to_path_buf() }
    }

    fn limits(timeout_ms: u64) -> Limits {
        Limits { timeout: Duration::from_millis(timeout_ms), kill_grace: Duration::from_millis(500), max_line: 1024 }
    }

    fn no_stop() -> watch::Receiver<Stop> {
        watch::channel(Stop::None).1
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 checks existence only.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[tokio::test]
    async fn lines_exit_and_stderr() {
        let d = tempfile::tempdir().unwrap();
        let mut got = Vec::new();
        let r = run(&sh("echo a; printf 'b\\nc'; echo oops >&2; exit 3", d.path()), limits(10_000), no_stop(), None, None, |l| got.push(l.to_string()), None, None).await;
        assert_eq!(got, ["a", "b", "c"]);
        assert_eq!(r.ended, Ended::Exited { code: Some(3), signal: None });
        assert_eq!(r.stderr_tail.trim(), "oops");
        assert!(r.reaped);
    }

    #[tokio::test]
    async fn an_over_long_line_is_skipped_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        let mut got = Vec::new();
        let script = "echo before; head -c 5000 /dev/zero | tr '\\0' x; echo; echo after";
        let r = run(&sh(script, d.path()), limits(10_000), no_stop(), None, None, |l| got.push(l.to_string()), None, None).await;
        assert_eq!(got, ["before", "after"]);
        assert_eq!(r.long_lines, 1);
        assert!(r.ended.success());
    }

    #[tokio::test]
    async fn time_limit_kills_the_whole_tree_before_returning() {
        let d = tempfile::tempdir().unwrap();
        let pids = d.path().join("pids");
        // A helper in its own process group and session still holding the lock descriptor.
        let script = format!(
            "sleep 60 & echo $! >> {p}; setsid sleep 60 & echo $! >> {p}; trap '' TERM; sleep 60",
            p = pids.display()
        );
        let lock = FileLock::try_exclusive(&d.path().join("turn.lock")).unwrap().unwrap();
        let t = std::time::Instant::now();
        let r = run(&sh(&script, d.path()), limits(300), no_stop(), Some(&lock), None, |_| {}, None, None).await;
        assert_eq!(r.ended, Ended::TimedOut);
        assert!(r.reaped);
        assert!(t.elapsed() < Duration::from_secs(8));
        let ids: Vec<i32> = std::fs::read_to_string(&pids).unwrap().lines().map(|l| l.parse().unwrap()).collect();
        assert_eq!(ids.len(), 2);
        for pid in ids {
            assert!(!alive(pid), "{pid} outlived the turn");
        }
    }

    #[tokio::test]
    async fn helpers_left_after_a_clean_exit_are_reaped() {
        let d = tempfile::tempdir().unwrap();
        let pids = d.path().join("pids");
        let script = format!("sleep 60 & echo $! > {}; exit 0", pids.display());
        let r = run(&sh(&script, d.path()), limits(10_000), no_stop(), None, None, |_| {}, None, None).await;
        assert!(r.ended.success() && r.reaped);
        let pid: i32 = std::fs::read_to_string(&pids).unwrap().trim().parse().unwrap();
        assert!(!alive(pid));
    }

    #[tokio::test]
    async fn stop_now_and_graceful() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().to_path_buf();
        let (tx, rx) = watch::channel(Stop::None);
        let q = p.clone();
        let h = tokio::spawn(async move { run(&sh("sleep 30", &q), limits(60_000), rx, None, None, |_| {}, None, None).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send(Stop::Now).unwrap();
        assert_eq!(h.await.unwrap().ended, Ended::Stopped);

        let (tx, rx) = watch::channel(Stop::None);
        let q = p.clone();
        let h = tokio::spawn(async move { run(&sh("sleep 0.3; exit 0", &q), limits(60_000), rx, None, None, |_| {}, None, None).await });
        tx.send(Stop::Graceful(Duration::from_secs(5))).unwrap();
        assert!(h.await.unwrap().ended.success(), "finished within the grace");

        let (tx, rx) = watch::channel(Stop::None);
        let h = tokio::spawn(async move { run(&sh("sleep 30", &p), limits(60_000), rx, None, None, |_| {}, None, None).await });
        tx.send(Stop::Graceful(Duration::from_millis(100))).unwrap();
        assert_eq!(h.await.unwrap().ended, Ended::ShutdownKilled);
    }

    #[tokio::test]
    async fn a_dropped_run_kills_its_tree() {
        let d = tempfile::tempdir().unwrap();
        let slot: TreeSlot = Arc::default();
        let s2 = slot.clone();
        let p = d.path().to_path_buf();
        let h = tokio::spawn(async move { run(&sh("sleep 30", &p), limits(60_000), no_stop(), None, Some(&s2), |_| {}, None, None).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        h.abort();
        let _ = h.await;
        let t = slot.lock().unwrap().expect("published");
        assert!(reap(t, Duration::from_millis(100), Duration::from_secs(5)).await);
        assert!(members(t).is_empty());
    }

    #[tokio::test]
    async fn spawn_failure() {
        let d = tempfile::tempdir().unwrap();
        let spec = CmdSpec { program: "/nonexistent/agent".into(), args: vec![], env: vec![], cwd: d.path().into() };
        let r = run(&spec, limits(1000), no_stop(), None, None, |_| {}, None, None).await;
        assert!(matches!(r.ended, Ended::SpawnFailed(_)));
    }
}
