//! A remote driven over ssh by a proxy builder (docs/build.md §2a): the proxy owns one
//! OpenSSH master connection per remote ([`Conn`], a control socket under its data
//! directory), runs git and each step over it, streams the output back into a local log
//! for the same parsers, enforces the time limit itself (on expiry or cancel it kills the
//! remote process tree, then the session), and removes the remote job directory.
//!
//! Commands are written for the remote's shell ([`Dialect`]): PowerShell on Windows (each
//! script passed as `-EncodedCommand`, so no quoting passes through `cmd.exe`), or POSIX
//! `sh`. There is no memory cap on a remote.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use hgl_ai_proto::build::Platform;
use sha2::{Digest, Sha256};

use super::{
    ARTIFACT_FILES, ARTIFACT_LIMIT, Cancel, Checkout, Executor, HostError, HostOps, Out, Prep,
    Probe, StepRun, StepSpec, Workspace, checkout, expand_vars, remove_job,
};

/// Longest command line sent; `cmd.exe`, Windows OpenSSH's default shell, takes 8191.
pub const MAX_COMMAND: usize = 8000;
/// ssh's own exit status for a connection error.
const SSH_FAILED: i32 = 255;
const GIT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const SHORT: Duration = Duration::from_secs(30);
const KILL_GRACE: Duration = Duration::from_secs(10);
/// Longest transfer of one artifact.
const ARTIFACT_TIMEOUT: Duration = Duration::from_secs(600);
const PID_MARK: &[u8] = b"hgl-ai-pid ";

/// The remote's shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    PowerShell,
    Posix,
}

impl Dialect {
    pub fn parse(s: &str) -> Option<Dialect> {
        match s {
            "powershell" => Some(Dialect::PowerShell),
            "sh" => Some(Dialect::Posix),
            _ => None,
        }
    }

    pub fn quote(self, s: &str) -> String {
        match self {
            Dialect::PowerShell => format!("'{}'", s.replace('\'', "''")),
            Dialect::Posix => format!("'{}'", s.replace('\'', r"'\''")),
        }
    }

    /// The command line ssh sends to run `script`.
    pub fn command(self, script: &str) -> Result<String, String> {
        let c = match self {
            Dialect::PowerShell => format!(
                "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {}",
                encode_ps(&format!(
                    "$ProgressPreference = 'SilentlyContinue'\n{script}"
                ))
            ),
            Dialect::Posix => format!("sh -c {}", self.quote(script)),
        };
        if c.len() > MAX_COMMAND {
            Err(format!(
                "the command is {} bytes for the remote shell; the limit is {MAX_COMMAND}",
                c.len()
            ))
        } else {
            Ok(c)
        }
    }

    pub fn git(self, cwd: &str, args: &[&str]) -> String {
        let mut s = format!("git -C {}", self.quote(cwd));
        for a in args {
            s.push(' ');
            s.push_str(&self.quote(a));
        }
        if self == Dialect::PowerShell {
            s.push_str("\nexit $LASTEXITCODE");
        }
        s
    }

    /// A step: print the shell's process id first (the tree to kill), set the
    /// environment, enter `dir`, run `run`, exit with its status.
    pub fn step(self, dir: &str, env: &[(String, String)], run: &str) -> String {
        match self {
            Dialect::PowerShell => {
                let mut s = String::from(
                    "$ErrorActionPreference = 'Continue'\nWrite-Output ('hgl-ai-pid ' + $PID)\n",
                );
                for (k, v) in env {
                    s.push_str(&format!("$env:{k} = {}\n", self.quote(v)));
                }
                s.push_str(&format!(
                    "Set-Location -LiteralPath {} -ErrorAction Stop\n$global:LASTEXITCODE = 0\n{run}\nif (-not $?) {{ if ($LASTEXITCODE) {{ exit $LASTEXITCODE }} else {{ exit 1 }} }}\nexit $LASTEXITCODE",
                    self.quote(dir)
                ));
                s
            }
            Dialect::Posix => {
                let mut s = String::from("echo \"hgl-ai-pid $$\"\n");
                for (k, v) in env {
                    s.push_str(&format!("export {k}={}\n", self.quote(v)));
                }
                s.push_str(&format!("cd {} || exit 1\n{run}", self.quote(dir)));
                s
            }
        }
    }

    /// The command line that kills process `pid` and its descendants.
    pub fn kill_tree(self, pid: u32) -> String {
        match self {
            Dialect::PowerShell => format!("taskkill /T /F /PID {pid}"),
            Dialect::Posix => format!(
                "sh -c {}",
                self.quote(&format!(
                    "k() {{ kill -STOP \"$1\" 2>/dev/null; for c in $(pgrep -P \"$1\"); do k \"$c\"; done; kill -KILL \"$1\" 2>/dev/null; }}; k {pid}"
                ))
            ),
        }
    }

    /// A command line that only checks the remote answers.
    pub fn noop(self) -> &'static str {
        match self {
            Dialect::PowerShell => "exit 0",
            Dialect::Posix => "true",
        }
    }

    pub fn exists(self, p: &str) -> String {
        match self {
            Dialect::PowerShell => format!(
                "if (Test-Path -LiteralPath {}) {{ exit 0 }} else {{ exit 1 }}",
                self.quote(p)
            ),
            Dialect::Posix => format!("test -e {}", self.quote(p)),
        }
    }

    pub fn mkdirs(self, p: &str) -> String {
        match self {
            Dialect::PowerShell => format!(
                "New-Item -ItemType Directory -Force -Path {} | Out-Null",
                self.quote(p)
            ),
            Dialect::Posix => format!("mkdir -p {}", self.quote(p)),
        }
    }

    pub fn remove(self, p: &str) -> String {
        match self {
            Dialect::PowerShell => format!(
                "if (Test-Path -LiteralPath {0}) {{ Remove-Item -LiteralPath {0} -Recurse -Force -ErrorAction Stop }}",
                self.quote(p)
            ),
            Dialect::Posix => format!("rm -rf {}", self.quote(p)),
        }
    }

    pub fn has_tool(self, t: &str) -> String {
        match self {
            Dialect::PowerShell => format!(
                "if (Get-Command {} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}",
                self.quote(t)
            ),
            Dialect::Posix => format!("command -v {} >/dev/null 2>&1", self.quote(t)),
        }
    }

    pub fn version(self, t: &str) -> String {
        match self {
            Dialect::PowerShell => format!(
                "if (Get-Command {0} -ErrorAction SilentlyContinue) {{ & {0} --version 2>&1 | Select-Object -First 1 }}",
                self.quote(t)
            ),
            Dialect::Posix => format!(
                "command -v {0} >/dev/null 2>&1 && {0} --version 2>&1 | head -n 1",
                self.quote(t)
            ),
        }
    }

    /// Prints the free bytes of the volume holding `p`.
    pub fn free_bytes(self, p: &str) -> String {
        match self {
            Dialect::PowerShell => format!(
                "$d = (Get-Item -LiteralPath {}).PSDrive.Name\n(Get-PSDrive -Name $d).Free",
                self.quote(p)
            ),
            Dialect::Posix => format!(
                "df -Pk {} | awk 'NR==2 {{ print $4 * 1024 }}'",
                self.quote(p)
            ),
        }
    }

    /// Prints `SIZE PATH` for each file in `dir` matching the one-segment `pattern`.
    pub fn list(self, dir: &str, pattern: &str) -> String {
        match self {
            Dialect::PowerShell => format!(
                "Get-ChildItem -LiteralPath {} -File -Filter {} -ErrorAction SilentlyContinue | ForEach-Object {{ \"$($_.Length) $($_.FullName)\" }}",
                self.quote(dir),
                self.quote(pattern)
            ),
            // The pattern is left unquoted for the shell to expand; recipes refuse quotes
            // and separators in artifacts.
            Dialect::Posix => format!(
                "cd {} 2>/dev/null && for f in {pattern}; do [ -f \"$f\" ] && printf '%s %s/%s\\n' \"$(wc -c < \"$f\" | tr -d ' ')\" \"$(pwd)\" \"$f\"; done; true",
                self.quote(dir)
            ),
        }
    }

    /// Writes file `p` to stdout, byte for byte, streamed (never read whole).
    pub fn cat(self, p: &str) -> String {
        match self {
            Dialect::PowerShell => format!(
                "$f = [IO.File]::OpenRead({})\n$o = [Console]::OpenStandardOutput()\n$f.CopyTo($o)\n$o.Flush()\n$f.Close()",
                self.quote(p)
            ),
            Dialect::Posix => format!("cat {}", self.quote(p)),
        }
    }

    /// The command line of an interactive shell in `dir` with `env` (ssh with `-t`).
    pub fn interactive(self, dir: &str, env: &[(String, String)]) -> String {
        match self {
            Dialect::PowerShell => {
                let mut s = String::new();
                for (k, v) in env {
                    s.push_str(&format!("$env:{k} = {}\n", self.quote(v)));
                }
                s.push_str(&format!("Set-Location -LiteralPath {}", self.quote(dir)));
                format!(
                    "powershell -NoProfile -ExecutionPolicy Bypass -NoExit -EncodedCommand {}",
                    encode_ps(&s)
                )
            }
            Dialect::Posix => {
                let mut s = format!("cd {} || exit 1\n", self.quote(dir));
                for (k, v) in env {
                    s.push_str(&format!("export {k}={}\n", self.quote(v)));
                }
                s.push_str("exec \"${SHELL:-/bin/sh}\" -i");
                format!("sh -c {}", self.quote(&s))
            }
        }
    }
}

/// PowerShell's `-EncodedCommand`: base64 of the script in UTF-16LE.
pub fn encode_ps(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

/// Run a program with a timeout, collecting stdout and stderr; `None` status when it
/// was killed for the timeout.
fn run_with_timeout(
    mut cmd: Command,
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> std::io::Result<(Option<i32>, Vec<u8>, Vec<u8>)> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    if let (Some(data), Some(mut w)) = (stdin, child.stdin.take()) {
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = w.write_all(&data);
        });
    }
    let out = child.stdout.take().map(reader);
    let err = child.stderr.take().map(reader);
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s.code().or(Some(128 + 9));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let collect = |h: Option<std::sync::mpsc::Receiver<Vec<u8>>>| {
        h.and_then(|r| r.recv_timeout(Duration::from_secs(5)).ok())
            .unwrap_or_default()
    };
    Ok((status, collect(out), collect(err)))
}

/// Read a pipe to its end on a thread of its own; the result arrives on the channel.
fn reader(mut r: impl Read + Send + 'static) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = r.read_to_end(&mut b);
        let _ = tx.send(b);
    });
    rx
}

fn last_line(b: &[u8]) -> String {
    String::from_utf8_lossy(b)
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("no message")
        .to_string()
}

struct ConnState {
    master: Option<Child>,
    reachable: bool,
    reason: Option<String>,
}

/// The proxy's connection to one remote: an OpenSSH master (`ControlMaster`) that this
/// process starts, checks and closes; every command goes through its control socket.
pub struct Conn {
    pub label: String,
    alias: String,
    program: PathBuf,
    socket: PathBuf,
    dir: PathBuf,
    log: PathBuf,
    dialect: Dialect,
    /// `false` for a borrower (`build shell`): it uses the proxy's master when there is
    /// one and never starts or stops it.
    owned: bool,
    /// Counts masters started and drops seen: a session that began under another
    /// generation lost its master, even if a new one answers now.
    generation: std::sync::atomic::AtomicU64,
    state: Mutex<ConnState>,
    reachable: tokio::sync::watch::Sender<bool>,
}

impl Conn {
    /// `dir` holds the master's log and, when short enough, its control socket.
    pub fn new(
        label: &str,
        alias: &str,
        program: PathBuf,
        dir: &Path,
        dialect: Dialect,
    ) -> std::io::Result<Arc<Conn>> {
        Self::make(label, alias, program, dir, dialect, true)
    }

    fn make(
        label: &str,
        alias: &str,
        program: PathBuf,
        dir: &Path,
        dialect: Dialect,
        owned: bool,
    ) -> std::io::Result<Arc<Conn>> {
        std::fs::create_dir_all(dir)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR");
        let socket = if !owned
            && runtime.is_none()
            && dir.join(format!("{label}.sock")).as_os_str().len() > socket_limit()
        {
            PathBuf::new()
        } else {
            control_socket(dir, label, runtime.as_deref().map(Path::new))?
        };
        let (tx, _) = tokio::sync::watch::channel(false);
        Ok(Arc::new(Conn {
            label: label.to_string(),
            alias: alias.to_string(),
            program,
            log: dir.join(format!("{label}.log")),
            socket,
            dir: dir.to_path_buf(),
            dialect,
            owned,
            generation: std::sync::atomic::AtomicU64::new(0),
            state: Mutex::new(ConnState {
                master: None,
                reachable: false,
                reason: Some("not connected yet".into()),
            }),
            reachable: tx,
        }))
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// A connection that borrows the proxy's master if it runs, else connects directly.
    pub fn borrowed(
        label: &str,
        alias: &str,
        program: PathBuf,
        dir: &Path,
        dialect: Dialect,
    ) -> std::io::Result<Arc<Conn>> {
        Self::make(label, alias, program, dir, dialect, false)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ConnState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Options of every command that uses the master.
    pub fn opts(&self) -> Vec<OsString> {
        let mut v: Vec<OsString> = Vec::new();
        let path = if self.owned || self.socket.exists() {
            self.socket.display().to_string()
        } else {
            "none".into()
        };
        for o in [
            format!("ControlPath={path}"),
            "ControlMaster=no".into(),
            "BatchMode=yes".into(),
            "ConnectTimeout=15".into(),
            "ServerAliveInterval=5".into(),
            "ServerAliveCountMax=3".into(),
        ] {
            v.push("-o".into());
            v.push(o.into());
        }
        v
    }

    pub fn command(&self, tty: bool) -> Command {
        let mut c = Command::new(&self.program);
        c.args(self.opts());
        if tty {
            c.arg("-t");
        } else {
            c.arg("-T");
        }
        c.arg(&self.alias);
        c
    }

    /// Run `cmdline` on the remote. `Ok((status, out, err))`; status `None` on a timeout.
    pub fn run(
        &self,
        cmdline: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> std::io::Result<(Option<i32>, Vec<u8>, Vec<u8>)> {
        let mut c = self.command(false);
        c.arg(cmdline);
        run_with_timeout(c, stdin, timeout)
    }

    fn control(&self, op: &str) -> bool {
        let mut c = Command::new(&self.program);
        c.arg("-o")
            .arg(format!("ControlPath={}", self.socket.display()))
            .args(["-O", op])
            .arg(&self.alias);
        matches!(
            run_with_timeout(c, None, Duration::from_secs(10)),
            Ok((Some(0), _, _))
        )
    }

    /// The remote answers a no-op through the master.
    pub fn answers(&self) -> bool {
        matches!(
            self.run(self.dialect.noop(), None, Duration::from_secs(20)),
            Ok((Some(0), _, _))
        )
    }

    pub fn is_reachable(&self) -> bool {
        self.lock().reachable
    }

    pub fn reason(&self) -> Option<String> {
        self.lock().reason.clone()
    }

    pub fn watch(&self) -> tokio::sync::watch::Receiver<bool> {
        self.reachable.subscribe()
    }

    fn set(&self, reachable: bool, reason: Option<String>) {
        let mut s = self.lock();
        s.reachable = reachable;
        s.reason = reason;
        drop(s);
        self.reachable.send_replace(reachable);
    }

    /// The master generation now; see [`Conn::lost_since`].
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A session begun at generation `g` ended with ssh's own failure (255): it lost its
    /// connection if the master changed since (a reconnect may already answer) or the
    /// remote does not answer now.
    pub fn lost_since(&self, g: u64) -> bool {
        self.generation() != g || (self.owned && !self.master_alive()) || !self.answers()
    }

    fn master_alive(&self) -> bool {
        let mut s = self.lock();
        match s.master.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Mark the remote unreachable (a command saw the connection drop).
    pub fn mark_down(&self, reason: impl Into<String>) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.set(false, Some(reason.into()));
    }

    /// The master runs and the remote answers.
    pub fn check(&self) -> bool {
        let alive = {
            let mut s = self.lock();
            match s.master.as_mut() {
                Some(c) => matches!(c.try_wait(), Ok(None)),
                None => false,
            }
        };
        let ok = alive && self.control("check") && self.answers();
        if !ok && self.is_reachable() {
            self.mark_down("the connection dropped");
        } else if ok && !self.is_reachable() {
            self.set(true, None);
        }
        ok
    }

    /// Start the master (after closing a stale one) and wait until it answers.
    pub fn connect(&self) -> Result<(), String> {
        if !self.owned {
            return if self.answers() {
                self.set(true, None);
                Ok(())
            } else {
                self.mark_down("the remote does not answer");
                Err("the remote does not answer".into())
            };
        }
        if self.check() {
            self.set(true, None);
            return Ok(());
        }
        self.stop_master();
        let log = std::fs::File::create(&self.log).map_err(|e| e.to_string())?;
        let mut c = Command::new(&self.program);
        for o in [
            format!("ControlPath={}", self.socket.display()),
            "ControlMaster=yes".into(),
            "ControlPersist=no".into(),
            "BatchMode=yes".into(),
            "ConnectTimeout=15".into(),
            "ServerAliveInterval=5".into(),
            "ServerAliveCountMax=3".into(),
        ] {
            c.arg("-o").arg(o);
        }
        c.args(["-N", "-T"])
            .arg(&self.alias)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        let child = c.spawn().map_err(|e| format!("starting ssh: {e}"))?;
        self.lock().master = Some(child);
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let exited = {
                let mut s = self.lock();
                matches!(
                    s.master.as_mut().map(|c| c.try_wait()),
                    Some(Ok(Some(_))) | None
                )
            };
            if exited {
                self.lock().master = None;
                let why = std::fs::read(&self.log)
                    .map(|b| last_line(&b))
                    .unwrap_or_else(|_| "ssh exited".into());
                self.mark_down(why.clone());
                return Err(why);
            }
            if self.control("check") && self.answers() {
                self.set(true, None);
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.stop_master();
                self.mark_down("the connection did not come up within 20s");
                return Err("the connection did not come up within 20s".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn stop_master(&self) {
        if self.socket.exists() || self.lock().master.is_some() {
            let _ = self.control("exit");
        }
        if let Some(mut c) = self.lock().master.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_file(&self.socket);
    }

    /// Close the master connection.
    pub fn close(&self) {
        if !self.owned {
            return;
        }
        self.stop_master();
        self.set(false, Some("closed".into()));
    }
}

fn control_socket(dir: &Path, label: &str, runtime: Option<&Path>) -> std::io::Result<PathBuf> {
    let socket = dir.join(format!("{label}.sock"));
    if socket.as_os_str().len() <= socket_limit() {
        return Ok(socket);
    }
    let runtime = runtime.filter(|p| p.is_absolute()).ok_or_else(|| {
        std::io::Error::other("control socket path is too long and XDG_RUNTIME_DIR is unavailable")
    })?;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = std::fs::metadata(runtime)?;
    // SAFETY: geteuid has no preconditions.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(std::io::Error::other(
            "XDG_RUNTIME_DIR must be a private directory owned by the current user",
        ));
    }
    let sockets = runtime.join("hgl-ai");
    let mut builder = std::fs::DirBuilder::new();
    use std::os::unix::fs::DirBuilderExt;
    builder.mode(0o700);
    match builder.create(&sockets) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e),
    }
    let metadata = std::fs::symlink_metadata(&sockets)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(std::io::Error::other(
            "control socket directory is not private",
        ));
    }
    let socket = fallback_socket(&sockets, &std::fs::canonicalize(dir)?, label);
    if socket.as_os_str().len() > socket_limit() {
        return Err(std::io::Error::other(
            "XDG_RUNTIME_DIR is too long for a control socket",
        ));
    }
    Ok(socket)
}

fn socket_limit() -> usize {
    // OpenSSH appends a dot and 16 random characters before bind, plus NUL.
    // SAFETY: zero is a valid sockaddr_un value; only the array length is read.
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_path.len() - 18
}

fn fallback_socket(sockets: &Path, dir: &Path, label: &str) -> PathBuf {
    let mut hash = Sha256::new();
    use std::os::unix::ffi::OsStrExt;
    hash.update(dir.as_os_str().as_bytes());
    hash.update([0]);
    hash.update(label.as_bytes());
    sockets.join(format!("{}.sock", hex::encode(&hash.finalize()[..16])))
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(mut c) = self.lock().master.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Keep `conn` up until `stop`: check it every `every`, and when it is down reconnect
/// with backoff (2 s doubling to 5 min). `kick` asks for a check now.
pub async fn supervise(
    conn: Arc<Conn>,
    every: Duration,
    kick: Arc<tokio::sync::Notify>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(2);
    loop {
        if *stop.borrow() {
            break;
        }
        let c = conn.clone();
        let up = tokio::task::spawn_blocking(move || c.check())
            .await
            .unwrap_or(false);
        let wait = if up {
            backoff = Duration::from_secs(2);
            every
        } else {
            let c = conn.clone();
            match tokio::task::spawn_blocking(move || c.connect()).await {
                Ok(Ok(())) => {
                    tracing::info!(remote = %conn.label, "connected");
                    backoff = Duration::from_secs(2);
                    every
                }
                Ok(Err(e)) => {
                    tracing::warn!(remote = %conn.label, "unreachable: {e}; retrying in {backoff:?}");
                    let w = backoff;
                    backoff = (backoff * 2).min(Duration::from_secs(300));
                    w
                }
                Err(_) => backoff,
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = kick.notified() => {}
            _ = stop.changed() => {}
        }
    }
    let c = conn.clone();
    let _ = tokio::task::spawn_blocking(move || c.close()).await;
}

/// A remote host built over ssh.
pub struct Ssh {
    platform: Platform,
    dialect: Dialect,
    work_dir: String,
    sdk: BTreeMap<String, String>,
    conn: Arc<Conn>,
    kick: Arc<tokio::sync::Notify>,
    /// Remote step shells whose session dropped: killed at the next cleanup.
    orphans: Mutex<Vec<u32>>,
    /// Bytes of artifacts one step may bring back.
    artifact_limit: u64,
}

impl Ssh {
    pub fn new(
        platform: Platform,
        dialect: Dialect,
        work_dir: &str,
        sdk: BTreeMap<String, String>,
        conn: Arc<Conn>,
    ) -> Ssh {
        Ssh {
            platform,
            dialect,
            work_dir: work_dir.trim_end_matches(['/', '\\']).to_string(),
            sdk,
            conn,
            kick: Arc::new(tokio::sync::Notify::new()),
            orphans: Mutex::new(Vec::new()),
            artifact_limit: ARTIFACT_LIMIT,
        }
    }

    /// A smaller artifact budget than [`ARTIFACT_LIMIT`] (tests).
    pub fn with_artifact_limit(mut self, bytes: u64) -> Ssh {
        self.artifact_limit = bytes;
        self
    }

    pub fn conn(&self) -> &Arc<Conn> {
        &self.conn
    }

    /// Notified when a command sees the connection drop, for [`supervise`].
    pub fn kick(&self) -> Arc<tokio::sync::Notify> {
        self.kick.clone()
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// Run `script` on the remote.
    fn exec(&self, script: &str, timeout: Duration) -> Result<Out, HostError> {
        let cmdline = self.dialect.command(script).map_err(HostError::Failed)?;
        self.exec_line(&cmdline, timeout)
    }

    fn exec_line(&self, cmdline: &str, timeout: Duration) -> Result<Out, HostError> {
        let g = self.conn.generation();
        let (status, out, err) = self
            .conn
            .run(cmdline, None, timeout)
            .map_err(|e| HostError::Failed(format!("running ssh: {e}")))?;
        match status {
            // A command that never ended: the connection, unless the remote answers.
            None if !self.conn.answers() => {
                let why = format!("no answer within {timeout:?}");
                self.conn.mark_down(why.clone());
                self.kick.notify_one();
                Err(HostError::Unreachable(why))
            }
            None => Err(HostError::Failed(format!(
                "a remote command took longer than {timeout:?}"
            ))),
            Some(SSH_FAILED) if self.conn.lost_since(g) => {
                let why = last_line(&err);
                self.conn.mark_down(why.clone());
                self.kick.notify_one();
                Err(HostError::Unreachable(why))
            }
            Some(code) => Ok(Out {
                ok: code == 0,
                stdout: String::from_utf8_lossy(&out).into_owned(),
                stderr: String::from_utf8_lossy(&err).into_owned(),
            }),
        }
    }

    fn reachable(&self) -> Result<(), HostError> {
        if self.conn.is_reachable() {
            Ok(())
        } else {
            Err(HostError::Unreachable(
                self.conn.reason().unwrap_or_else(|| "unreachable".into()),
            ))
        }
    }

    fn kill_orphans(&self) -> Result<(), HostError> {
        let pids = std::mem::take(&mut *self.orphans.lock().unwrap_or_else(|e| e.into_inner()));
        for (n, pid) in pids.iter().enumerate() {
            if let Err(e) = self.exec_line(&self.dialect.kill_tree(*pid), SHORT) {
                self.orphans
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(&pids[n..]);
                return Err(e);
            }
        }
        Ok(())
    }

    /// Stream remote file `from` into local `to`, at most `budget` bytes: a file that
    /// grows past it is cut off and dropped (`None`). Returns the bytes written.
    /// The reader runs on a thread of its own, so `timeout` and `cancel` end a transfer
    /// that stalls: the session is killed and reaped, and the reader sees its pipe close.
    fn fetch(
        &self,
        from: &str,
        to: &Path,
        budget: u64,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Option<u64> {
        let cmdline = self.dialect.command(&self.dialect.cat(from)).ok()?;
        let mut c = self.conn.command(false);
        c.arg(&cmdline)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = c.spawn().ok()?;
        let mut out = child.stdout.take()?;
        let mut file = match std::fs::File::create(to) {
            Ok(f) => f,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        };
        let (tx, rx) = std::sync::mpsc::channel::<Option<u64>>();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 << 10];
            let mut n = 0u64;
            let res = loop {
                match out.read(&mut buf) {
                    Ok(0) => break Some(n),
                    Ok(k) => {
                        n += k as u64;
                        if n > budget || file.write_all(&buf[..k]).is_err() {
                            break None;
                        }
                    }
                    Err(_) => break None,
                }
            };
            let _ = tx.send(res);
        });
        let deadline = Instant::now() + timeout;
        let read = loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(r) => break r,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break None,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            if cancel.is_cancelled() || Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                // The session's pipe is closed now and the reader ends with it; it is not
                // waited for long, in case something else still holds the pipe.
                let _ = rx.recv_timeout(Duration::from_secs(1));
                return None;
            }
        };
        if read.is_none() {
            let _ = child.kill();
        }
        // The session ends after its output; a few seconds more, never unbounded.
        let until = Instant::now() + Duration::from_secs(10);
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Some(st),
                Ok(None) if Instant::now() < until && !cancel.is_cancelled() => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        read.filter(|_| status.is_some_and(|s| s.success()))
    }

    fn is_absolute(&self, p: &str) -> bool {
        p.starts_with('/') || p.starts_with('\\') || p.as_bytes().get(1) == Some(&b':')
    }
}

impl HostOps for Ssh {
    /// A `flock` on this host in the configured data directory: the proxy's builder
    /// and its `build shell`s, in their own processes, take turns on the remote clone.
    fn lock_clone(&self, clone: &str) -> Result<super::CloneGuard, HostError> {
        let repo = clone.rsplit(['/', '\\']).next().unwrap_or(clone);
        let path = self
            .conn
            .dir
            .join(format!("{}.clone.{repo}.lock", self.conn.label));
        let l = crate::lock::FileLock::lock(&path)
            .map_err(|e| HostError::Failed(format!("locking {}: {e}", path.display())))?;
        Ok(Box::new(l))
    }

    fn git(&self, cwd: &str, args: &[&str]) -> Result<Out, HostError> {
        self.exec(&self.dialect.git(cwd, args), GIT_TIMEOUT)
    }
    fn exists(&self, path: &str) -> Result<bool, HostError> {
        Ok(self.exec(&self.dialect.exists(path), SHORT)?.ok)
    }
    fn mkdirs(&self, path: &str) -> Result<(), HostError> {
        let o = self.exec(&self.dialect.mkdirs(path), SHORT)?;
        if o.ok {
            Ok(())
        } else {
            Err(HostError::Failed(format!(
                "mkdir {path}: {}",
                last_line(o.stderr.as_bytes())
            )))
        }
    }
    fn remove(&self, path: &str) -> Result<(), HostError> {
        let o = self.exec(&self.dialect.remove(path), Duration::from_secs(600))?;
        if o.ok {
            Ok(())
        } else {
            Err(HostError::Failed(format!(
                "removing {path}: {}",
                last_line(o.stderr.as_bytes())
            )))
        }
    }
}

impl Executor for Ssh {
    fn kind(&self) -> &'static str {
        "ssh"
    }

    fn platform(&self) -> Platform {
        self.platform
    }

    fn sdk(&self, name: &str) -> Option<String> {
        self.sdk.get(name).cloned()
    }

    fn sdk_names(&self) -> Vec<String> {
        self.sdk.keys().cloned().collect()
    }

    fn has_tool(&self, tool: &str) -> Result<bool, HostError> {
        Ok(self.exec(&self.dialect.has_tool(tool), SHORT)?.ok)
    }

    fn prepare(&self, c: &Checkout) -> Result<Workspace, Prep> {
        self.reachable()?;
        checkout(self, &self.work_dir, c)
    }

    fn run_step(&self, ws: &Workspace, step: &StepSpec, log: &Path, cancel: &Cancel) -> StepRun {
        let started = Instant::now();
        let mut r = StepRun::default();
        if let Err(HostError::Unreachable(_)) = self.reachable() {
            r.unreachable = true;
            return r;
        }
        let dir = format!("{}/{}", ws.src, step.cwd);
        let mut env = step.env.clone();
        if self.platform == Platform::Windows {
            env.push(("TMP".into(), ws.tmp.clone()));
            env.push(("TEMP".into(), ws.tmp.clone()));
        }
        let cmdline = match self
            .dialect
            .command(&self.dialect.step(&dir, &env, &step.script))
        {
            Ok(c) => c,
            Err(e) => {
                r.error = Some(e);
                return r;
            }
        };
        let file = match std::fs::File::create(log) {
            Ok(f) => Arc::new(Mutex::new(f)),
            Err(e) => {
                r.error = Some(format!("{}: {e}", log.display()));
                return r;
            }
        };
        let mut c = self.conn.command(false);
        c.arg(&cmdline)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let generation = self.conn.generation();
        let mut child = match c.spawn() {
            Ok(c) => c,
            Err(e) => {
                r.error = Some(format!("running ssh: {e}"));
                return r;
            }
        };
        let pid = Arc::new(AtomicU32::new(0));
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        if let Some(out) = child.stdout.take() {
            let (file, pid, done) = (file.clone(), pid.clone(), done_tx.clone());
            std::thread::spawn(move || {
                let mut rd = BufReader::new(out);
                let mut first = Vec::new();
                if rd.read_until(b'\n', &mut first).is_ok() {
                    match first.strip_prefix(PID_MARK) {
                        Some(rest) => {
                            let n = String::from_utf8_lossy(rest).trim().parse().unwrap_or(0);
                            pid.store(n, Ordering::SeqCst);
                        }
                        None => {
                            let _ = file.lock().map(|mut f| f.write_all(&first));
                        }
                    }
                }
                copy(rd, &file);
                let _ = done.send(());
            });
        }
        if let Some(err) = child.stderr.take() {
            let (file, done) = (file.clone(), done_tx.clone());
            std::thread::spawn(move || {
                copy(err, &file);
                let _ = done.send(());
            });
        }
        drop(done_tx);
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let _hook = cancel.hook(move || s2.store(true, Ordering::SeqCst));
        let deadline = started + step.time;
        let mut killing: Option<Instant> = None;
        // The remote tree's kill was sent (or handed to cleanup); its pid may arrive after
        // the stop, so it is looked for through the grace.
        let mut remote_killed = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s.code(),
                Ok(None) => {}
                Err(e) => {
                    r.error = Some(format!("waiting for ssh: {e}"));
                    break None;
                }
            }
            let now = Instant::now();
            if killing.is_none() && (stop.load(Ordering::SeqCst) || now >= deadline) {
                if stop.load(Ordering::SeqCst) {
                    r.stopped = true;
                } else {
                    r.limit = Some("time");
                }
                killing = Some(now);
            }
            if killing.is_some() && !remote_killed {
                let p = pid.load(Ordering::SeqCst);
                if p > 0 {
                    if self.exec_line(&self.dialect.kill_tree(p), SHORT).is_err() {
                        // Not reachable now: the next cleanup kills it.
                        self.orphans
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(p);
                    }
                    remote_killed = true;
                }
            }
            if let Some(k) = killing
                && now >= k + KILL_GRACE
            {
                let _ = child.kill();
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        if killing.is_some() && !remote_killed {
            // The pid came with the last output, after the session ended.
            let p = pid.load(Ordering::SeqCst);
            if p > 0 && self.exec_line(&self.dialect.kill_tree(p), SHORT).is_err() {
                self.orphans
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(p);
            }
        }
        // Ask at once whether the session dropped, before the remote can come back.
        let dropped =
            killing.is_none() && status == Some(SSH_FAILED) && self.conn.lost_since(generation);
        if dropped {
            self.conn.mark_down("the connection dropped during a step");
            self.kick.notify_one();
        }
        // The readers end with the session; a session that dropped may leave the remote
        // writing, so they get a few seconds.
        let wait_until = Instant::now() + Duration::from_secs(5);
        for _ in 0..2 {
            let left = wait_until.saturating_duration_since(Instant::now());
            if done_rx.recv_timeout(left).is_err() {
                break;
            }
        }
        r.wall = started.elapsed();
        r.exit = Some(status.unwrap_or(128 + 9));
        if killing.is_some() {
            r.exit = Some(if r.limit.is_some() { 124 } else { 143 });
        } else if dropped {
            r.unreachable = true;
            let p = pid.load(Ordering::SeqCst);
            if p > 0 {
                self.orphans
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(p);
            }
        }
        r
    }

    fn collect(
        &self,
        ws: &Workspace,
        cwd: &str,
        patterns: &[String],
        env: &[(String, String)],
        dest: &Path,
        cancel: &Cancel,
    ) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut total = 0u64;
        for p in patterns {
            let p = expand_vars(p, env).replace('\\', "/");
            let full = if self.is_absolute(&p) {
                p
            } else {
                format!("{}/{cwd}/{p}", ws.src)
            };
            let Some((dir, pat)) = full.rsplit_once('/') else {
                continue;
            };
            let Ok(o) = self.exec(&self.dialect.list(dir, pat), SHORT) else {
                continue;
            };
            for line in o.stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
                if out.len() >= ARTIFACT_FILES || total >= self.artifact_limit {
                    return out;
                }
                let Some((size, f)) = line
                    .split_once(' ')
                    .and_then(|(n, f)| Some((n.parse::<u64>().ok()?, f)))
                else {
                    continue;
                };
                let budget = self.artifact_limit - total;
                if size > budget || std::fs::create_dir_all(dest).is_err() {
                    tracing::warn!(
                        "artifact {f} ({size} bytes) is over the {budget} bytes left; skipped"
                    );
                    continue;
                }
                let name = f.rsplit(['/', '\\']).next().unwrap_or(f).to_string();
                let to = dest.join(&name);
                match self.fetch(f, &to, budget, ARTIFACT_TIMEOUT, cancel) {
                    Some(n) => {
                        total += n;
                        out.push(to);
                    }
                    None => {
                        let _ = std::fs::remove_file(&to);
                    }
                }
            }
        }
        out
    }

    fn cleanup(&self, ws: &Workspace) -> Result<(), HostError> {
        self.kill_orphans()?;
        remove_job(self, ws)
    }

    fn clean_all(&self) -> Result<(), HostError> {
        self.reachable()?;
        self.kill_orphans()?;
        self.remove(&format!("{}/jobs", self.work_dir))
    }

    fn probe(&self, tools: &[String]) -> Result<Probe, HostError> {
        self.reachable()?;
        self.mkdirs(&self.work_dir)?;
        let mut p = Probe::default();
        let o = self.exec(&self.dialect.free_bytes(&self.work_dir), SHORT)?;
        p.free_gb = o
            .stdout
            .trim()
            .parse::<f64>()
            .ok()
            .map(|b| b as u64 / 1_000_000_000);
        for t in tools {
            let o = self.exec(&self.dialect.version(t), SHORT)?;
            if let Some(l) = o.stdout.lines().map(str::trim).find(|l| !l.is_empty()) {
                p.toolchains
                    .insert(t.clone(), l.chars().take(120).collect());
            }
        }
        Ok(p)
    }

    fn shell(&self, ws: &Workspace, env: &[(String, String)]) -> std::io::Result<i32> {
        let mut c = self.conn.command(true);
        c.arg(self.dialect.interactive(&ws.src, env));
        Ok(c.status()?.code().unwrap_or(1))
    }
}

fn copy(mut r: impl Read, file: &Mutex<std::fs::File>) {
    let mut buf = vec![0u8; 64 << 10];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Ok(mut f) = file.lock() {
                    let _ = f.write_all(&buf[..n]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    mod short {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/short.rs"
        ));
    }

    use super::*;

    #[test]
    fn socket_limit_reserves_openssh_suffix() {
        let label = "r";
        let dir = PathBuf::from("x".repeat(socket_limit() - "/r.sock".len()));
        assert_eq!(
            control_socket(&dir, label, None).unwrap().as_os_str().len(),
            socket_limit()
        );
        let too_long = PathBuf::from("x".repeat(socket_limit() - "/r.sock".len() + 1));
        assert!(control_socket(&too_long, label, None).is_err());
    }

    #[test]
    fn borrowed_long_path_without_runtime_connects_directly() {
        if std::env::var_os("XDG_RUNTIME_DIR").is_some() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d".repeat(120));
        let conn = Conn::borrowed("r", "host", "ssh".into(), &dir, Dialect::Posix).unwrap();
        assert!(conn.opts().iter().any(|o| o == "ControlPath=none"));
    }

    #[test]
    fn clone_locks_stay_in_each_configured_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        for name in ["one", "two"] {
            let dir = tmp.path().join(name);
            let conn = Conn::new("r", "host", "ssh".into(), &dir, Dialect::Posix).unwrap();
            let mut conn = Arc::into_inner(conn).unwrap_or_else(|| panic!("unexpected sharing"));
            conn.socket = runtime.join("fallback.sock");
            let ex = Ssh::new(
                Platform::Linux,
                Dialect::Posix,
                "/work",
                BTreeMap::new(),
                Arc::new(conn),
            );
            let _guard = ex.lock_clone("/work/repo").unwrap();
            assert!(dir.join("r.clone.repo.lock").exists());
        }
        assert!(std::fs::read_dir(runtime).unwrap().next().is_none());
    }

    // What only a real Windows host checks: that PowerShell runs these scripts as
    // intended. Here: their text.
    #[test]
    fn powershell_text() {
        let d = Dialect::PowerShell;
        assert_eq!(d.quote("it's"), "'it''s'");
        let s = d.step(
            "C:/w/jobs/1/src/.",
            &[("HGL_AI_BUILD_DIR".into(), "C:/w/jobs/1/build".into())],
            "cargo test",
        );
        assert!(s.starts_with(
            "$ErrorActionPreference = 'Continue'\nWrite-Output ('hgl-ai-pid ' + $PID)\n"
        ));
        assert!(s.contains("$env:HGL_AI_BUILD_DIR = 'C:/w/jobs/1/build'\n"));
        assert!(s.contains(
            "Set-Location -LiteralPath 'C:/w/jobs/1/src/.' -ErrorAction Stop\n$global:LASTEXITCODE = 0\ncargo test\n"
        ));
        assert!(s.ends_with("exit $LASTEXITCODE"));
        let c = d.command("exit 0").unwrap();
        let b64 = c.rsplit(' ').next().unwrap();
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let back: Vec<u16> = raw
            .chunks(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect();
        assert_eq!(
            String::from_utf16(&back).unwrap(),
            "$ProgressPreference = 'SilentlyContinue'\nexit 0"
        );
        assert!(c.starts_with(
            "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand "
        ));
        assert_eq!(d.kill_tree(42), "taskkill /T /F /PID 42");
        assert_eq!(
            d.git("C:/w/clones/x", &["fetch", "origin"]),
            "git -C 'C:/w/clones/x' 'fetch' 'origin'\nexit $LASTEXITCODE"
        );
        assert!(d.command(&"x".repeat(MAX_COMMAND)).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn long_control_socket_uses_private_runtime_and_is_stable() {
        let tmp = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = Path::new("/proc/self/cwd").join(short::relative(tmp.path()));
        let dir = tmp.path().join("d".repeat(120));
        std::fs::create_dir(&dir).unwrap();
        let first = control_socket(&dir, "remote", Some(&runtime)).unwrap();
        assert!(first.as_os_str().len() <= socket_limit());
        assert_eq!(
            first,
            control_socket(&dir, "remote", Some(&runtime)).unwrap()
        );
        assert_ne!(
            first,
            control_socket(&dir, "other", Some(&runtime)).unwrap()
        );
        assert!(control_socket(&dir, "remote", None).is_err());
        std::os::unix::net::UnixListener::bind(&first).unwrap();
    }

    /// A transfer that stalls (a FIFO no one writes) ends at its deadline, or at a cancel,
    /// with the session killed.
    #[test]
    fn a_stalled_artifact_transfer_ends() {
        let tmp = tempfile::Builder::new().prefix("hbf").tempdir().unwrap();
        let (_socket_tmp, socket_path) = short::tempdir();
        let state = tmp.path().join("remote");
        std::fs::create_dir_all(&state).unwrap();
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-ssh");
        let conn = Conn::new(
            "r1",
            state.to_str().unwrap(),
            fake,
            &socket_path.join("ssh"),
            Dialect::Posix,
        )
        .unwrap();
        conn.connect().unwrap();
        let ex = Ssh::new(
            Platform::Linux,
            Dialect::Posix,
            tmp.path().to_str().unwrap(),
            BTreeMap::new(),
            conn.clone(),
        );
        let fifo = tmp.path().join("stall.xml");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a valid path and mode.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // Releases the remote readers still waiting on the FIFO, also on a panic.
        struct Release(std::path::PathBuf);
        impl Drop for Release {
            fn drop(&mut self) {
                use std::os::unix::fs::OpenOptionsExt;
                for _ in 0..8 {
                    if std::fs::OpenOptions::new()
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(&self.0)
                        .is_err()
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        let _release = Release(fifo.clone());
        let to = tmp.path().join("copy.xml");
        let t0 = Instant::now();
        let got = ex.fetch(
            fifo.to_str().unwrap(),
            &to,
            1 << 20,
            Duration::from_secs(1),
            &Cancel::default(),
        );
        assert_eq!(got, None);
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        let cancel = Cancel::default();
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            c2.cancel();
        });
        let t0 = Instant::now();
        let got = ex.fetch(
            fifo.to_str().unwrap(),
            &to,
            1 << 20,
            Duration::from_secs(600),
            &cancel,
        );
        assert_eq!(got, None);
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        conn.close();
    }

    #[test]
    fn posix_text() {
        let d = Dialect::Posix;
        assert_eq!(d.quote("it's"), r"'it'\''s'");
        assert_eq!(d.command("echo hi").unwrap(), "sh -c 'echo hi'");
        let s = d.step("/w/src/.", &[("A".into(), "b c".into())], "make");
        assert_eq!(
            s,
            "echo \"hgl-ai-pid $$\"\nexport A='b c'\ncd '/w/src/.' || exit 1\nmake"
        );
    }
}
