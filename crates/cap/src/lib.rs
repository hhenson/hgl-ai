//! Memory and time caps for a command tree (docs/design.md §1, docs/roles.md ROLE-17).
//!
//! Two methods, chosen by [`Method::detect`]:
//!
//! - [`Method::Scope`] (Linux with a reachable user systemd): the command runs
//!   in a transient scope, `systemd-run --user --scope -p MemoryMax=SIZE
//!   -p MemorySwapMax=0 -p OOMPolicy=stop`, so the whole tree shares one
//!   limit. At the limit the kernel kills the largest process and systemd
//!   stops the rest; the kill count and peak come from the scope's cgroup. A
//!   SIGKILL death at the peak counts as the limit too, for a kill the
//!   counters do not show yet.
//! - [`Method::Rlimit`] (macOS, a sandbox, a bare ssh command): every process
//!   gets an address-space limit of SIZE (`setrlimit(RLIMIT_AS)`, unless
//!   [`CapSpec::address_limit`] is off, as sanitizer builds need), and a
//!   watchdog kills the tree when its sampled resident total passes SIZE.
//!
//! Nesting (Linux scopes): a bridge turn runs in a transient slice of its own
//! ([`CapCommand::slice`], `app-hgl_ai_turn_<id>.slice`, `MemoryMax=SIZE`), and the
//! command's environment names it in [`SLICE_ENV`]. A cap started inside the turn
//! (`hgl-ai cap` run by the agent) puts its scope in that slice, so its own limit and
//! the turn's both hold and the turn's reaper sees its processes. A scope's report file
//! goes to the command's `TMPDIR` (the task's `tmp/`), never to the bridge's.
//!
//! [`CapCommand::process_group`] starts the command as the leader of a process group of
//! its own; signals to the tree also go to the group, which reaches processes that left
//! the parent chain where nothing else finds them (macOS).
//!
//! A time limit works like `timeout -k GRACE N`: TERM to the whole tree, then
//! KILL after the grace. A run that reached a limit ends with status 137 or
//! 143, never with a status that passes for a finished run, and
//! [`CapOutcome::report`] says what happened.
//!
//! [`run`] is the blocking entry point of `hgl-ai cap`. Other crates wrap a
//! [`std::process::Command`] in a [`CapCommand`], [`spawn`](CapCommand::spawn)
//! it, and call the blocking [`CapChild::wait`] (from async code, inside
//! `spawn_blocking`); [`Stopper::stop`] ends a run early from any thread.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Prefix of every line this crate prints or reports.
pub const PREFIX: &str = "hgl-ai cap";
/// Exit status of a tree stopped with TERM (128 + 15).
pub const STATUS_TERM: i32 = 143;
/// Exit status of a tree stopped with KILL (128 + 9).
pub const STATUS_KILL: i32 = 137;

const POLL: Duration = Duration::from_millis(50);
#[cfg(target_os = "linux")]
const SAMPLE: Duration = Duration::from_millis(250);
#[cfg(not(target_os = "linux"))]
const SAMPLE: Duration = Duration::from_millis(500);
/// How long the scope's inner shell may outlive the KILL of its command, to write its report.
const SHELL_GRACE: Duration = Duration::from_secs(2);
const REPORT_ENV: &str = "HGL_AI_CAP_REPORT";
/// Names the turn slice a nested cap puts its scope in ([`CapCommand::slice`]).
pub const SLICE_ENV: &str = "HGL_AI_SLICE";
/// Longest wait for one `systemctl` call about a slice.
const SYSTEMCTL_WAIT: Duration = Duration::from_secs(10);
/// Bound on any small file read here (`/proc` entries, the scope report).
const SMALL_READ: u64 = 64 << 10;
/// Bound on a process listing (`cgroup.procs`, `ps` output).
const LIST_READ: u64 = 16 << 20;

/// Read at most `limit` bytes of a file as text; a longer file is cut off, never buffered whole.
fn read_limited(path: impl AsRef<Path>, limit: u64) -> io::Result<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)?
        .take(limit)
        .read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CapError {
    #[error("invalid size {0:?}: use a number and a unit, such as 512M or 16G")]
    Size(String),
}

/// A memory and time cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapSpec {
    /// Memory limit of the whole tree, in bytes.
    pub mem_bytes: u64,
    /// Time limit from the start; `None` for no limit.
    pub time: Option<Duration>,
    /// Grace between TERM and KILL when the time limit is reached or the run is stopped.
    pub kill_after: Duration,
    /// [`Method::Rlimit`]: also give each process an address-space limit of `mem_bytes`.
    /// Off for sanitizer builds (ASan, TSan reserve terabytes of address space); the
    /// resident-memory watchdog still limits the tree.
    pub address_limit: bool,
}

impl CapSpec {
    pub const DEFAULT_KILL_AFTER: Duration = Duration::from_secs(10);

    /// A memory cap with no time limit and the default grace.
    pub fn new(mem_bytes: u64) -> CapSpec {
        CapSpec {
            mem_bytes,
            time: None,
            kill_after: CapSpec::DEFAULT_KILL_AFTER,
            address_limit: true,
        }
    }

    pub fn with_address_limit(self, address_limit: bool) -> CapSpec {
        CapSpec {
            address_limit,
            ..self
        }
    }

    pub fn with_time(self, time: Duration) -> CapSpec {
        CapSpec {
            time: Some(time),
            ..self
        }
    }

    pub fn with_kill_after(self, kill_after: Duration) -> CapSpec {
        CapSpec { kill_after, ..self }
    }
}

/// Parse a size such as `16G`, `512M`, `1.5G`, `64GiB` (powers of 1024). A unit is required.
pub fn parse_size(s: &str) -> Result<u64, CapError> {
    let err = || CapError::Size(s.to_string());
    let t = s.trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let upper = unit.to_ascii_uppercase();
    let shift = match upper.as_str() {
        "B" => 0,
        "K" | "KB" | "KIB" => 10,
        "M" | "MB" | "MIB" => 20,
        "G" | "GB" | "GIB" => 30,
        "T" | "TB" | "TIB" => 40,
        "P" | "PB" | "PIB" => 50,
        _ => return Err(err()),
    };
    let (int, frac) = num.split_once('.').unwrap_or((num, ""));
    if int.is_empty() && frac.is_empty() {
        return Err(err());
    }
    let whole: u64 = if int.is_empty() {
        0
    } else {
        int.parse().map_err(|_| err())?
    };
    let mut bytes = whole
        .checked_shl(shift)
        .filter(|b| b >> shift == whole)
        .ok_or_else(err)?;
    if !frac.is_empty() {
        let digits: u32 = frac.len().try_into().map_err(|_| err())?;
        let f: u128 = frac.parse().map_err(|_| err())?;
        let scale = 10u128.checked_pow(digits).ok_or_else(err)?;
        let part = (f << shift) / scale;
        bytes = bytes
            .checked_add(u64::try_from(part).map_err(|_| err())?)
            .ok_or_else(err)?;
    }
    if bytes == 0 { Err(err()) } else { Ok(bytes) }
}

/// Format bytes like `numfmt --to=iec`: `16G`, `512M`, `1.5G`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["K", "M", "G", "T", "P"];
    let mut unit = None;
    let mut value = bytes as f64;
    for u in UNITS {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = Some(u);
    }
    let Some(u) = unit else {
        return bytes.to_string();
    };
    if value.fract() == 0.0 {
        format!("{value:.0}{u}")
    } else if value < 10.0 {
        format!("{:.1}{u}", (value * 10.0).ceil() / 10.0)
    } else {
        format!("{:.0}{u}", value.ceil())
    }
}

/// Format a duration as `90s`, `4m`, `2h`, or `1.5s`.
pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    if d.subsec_nanos() != 0 {
        format!("{:.1}s", d.as_secs_f64())
    } else if s != 0 && s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else if s != 0 && s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// How the memory cap is enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// A systemd user scope: one cgroup limit for the whole tree (Linux).
    Scope,
    /// A per-process address-space limit and a sampled watchdog over the tree.
    Rlimit,
}

impl Method {
    /// [`Method::Scope`] when `systemd-run --user --scope` works here, else
    /// [`Method::Rlimit`]. Probed once per process.
    pub fn detect() -> Method {
        if scope_probe().is_some() {
            Method::Scope
        } else {
            Method::Rlimit
        }
    }

    /// The one-line warning to print before a run with this method, if any.
    pub fn warning(self, spec: &CapSpec) -> Option<String> {
        match self {
            Method::Scope => None,
            Method::Rlimit => Some(format!(
                "{PREFIX}: {}; using {}a sampled watchdog over the command tree instead",
                if cfg!(target_os = "linux") {
                    "the user's systemd is not reachable here"
                } else {
                    "no cgroup memory limit on this system"
                },
                if spec.address_limit {
                    format!(
                        "a per-process address-space limit of {} and ",
                        format_size(spec.mem_bytes)
                    )
                } else {
                    String::new()
                }
            )),
        }
    }
}

fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        // SAFETY: getuid has no preconditions.
        _ => PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })),
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// How to start a scope here: `None` when it cannot be done, else the extra
/// `systemd-run` arguments. Since systemd 254, `systemd-run --scope` expands
/// `$VAR`, `${VAR}` and `$$` in the command's arguments unless told not to;
/// older versions neither expand nor know the option. Probed once per process.
fn scope_probe() -> Option<&'static [&'static str]> {
    static PROBED: OnceLock<Option<&'static [&'static str]>> = OnceLock::new();
    *PROBED.get_or_init(|| {
        if !cfg!(target_os = "linux") || find_on_path("systemd-run").is_none() {
            return None;
        }
        use std::os::unix::fs::FileTypeExt;
        if !std::fs::metadata(runtime_dir().join("bus")).is_ok_and(|m| m.file_type().is_socket()) {
            return None;
        }
        const NO_EXPAND: &[&str] = &["--expand-environment=no"];
        [NO_EXPAND, &[]].into_iter().find(|extra| scope_runs(extra))
    })
}

fn scope_runs(extra: &[&str]) -> bool {
    let probe = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect"])
        .args(extra)
        .args(["--", "true"])
        .env("XDG_RUNTIME_DIR", runtime_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = probe else { return false };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// The transient slice of one bridge turn: `app-hgl_ai_turn_<id>.slice`, under `app.slice`
/// where systemd puts user scopes. `id` is reduced to `[a-z0-9_]` (a dash in a slice name
/// is a level of the hierarchy).
pub fn turn_slice(id: &str) -> String {
    let id: String = id
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    format!("app-hgl_ai_turn_{id}.slice")
}

/// Remove the turn slices of bridges that are gone: those `turn_slice` named after a
/// process id (its last but one part) that no longer runs, left by a bridge that died
/// before it could stop them. Returns the slices removed.
pub fn remove_stale_turn_slices() -> Vec<String> {
    let dir = runtime_dir().join("systemd").join("user.control");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(slice) = name.strip_suffix(".d") else {
            continue;
        };
        let Some(id) = slice
            .strip_prefix("app-hgl_ai_turn_")
            .and_then(|r| r.strip_suffix(".slice"))
        else {
            continue;
        };
        let mut parts = id.rsplit('_');
        let (Some(_n), Some(pid)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(pid) = pid.parse::<libc::pid_t>() else {
            continue;
        };
        // SAFETY: signal 0 checks existence only.
        let gone = unsafe { libc::kill(pid, 0) } != 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone && valid_slice(slice) {
            remove_slice(slice);
            out.push(slice.to_string());
        }
    }
    out
}

/// A slice unit name as [`SLICE_ENV`] may give it.
fn valid_slice(name: &str) -> bool {
    name.len() <= 255
        && name.len() > ".slice".len()
        && name.ends_with(".slice")
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

/// Run a quiet command, waiting at most `wait`; whether it exited 0.
fn run_quiet(cmd: &mut Command, wait: Duration) -> bool {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    let deadline = Instant::now() + wait;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn systemctl(args: &[&str]) -> bool {
    run_quiet(
        Command::new("systemctl")
            .arg("--user")
            .args(args)
            .env("XDG_RUNTIME_DIR", runtime_dir()),
        SYSTEMCTL_WAIT,
    )
}

/// Give the slice `name` the turn's limit (a runtime drop-in; the slice is loaded on
/// first use). Whether it worked.
fn make_slice(name: &str, spec: &CapSpec) -> bool {
    systemctl(&[
        "set-property",
        "--runtime",
        name,
        &format!("MemoryMax={}", spec.mem_bytes),
        "MemorySwapMax=0",
    ])
}

/// Stop the slice and remove its drop-in. `--no-block`: a process still in it is the
/// caller's to reap ([`drain`]); systemd's own stop would wait for it.
fn remove_slice(name: &str) {
    systemctl(&["stop", "--no-block", name]);
    systemctl(&["revert", name]);
}

/// TERM every process left in the cgroup, KILL them after `grace`, and wait a moment for
/// them to go.
fn drain(cg: &Path, grace: Duration) {
    let left = |sig| {
        let pids = cgroup_procs(cg);
        for &p in &pids {
            send(p, sig);
        }
        !pids.is_empty()
    };
    if !left(libc::SIGTERM) {
        return;
    }
    let end = Instant::now() + grace;
    while Instant::now() < end && !cgroup_procs(cg).is_empty() {
        std::thread::sleep(POLL);
    }
    let end = Instant::now() + SHELL_GRACE;
    while left(libc::SIGKILL) && Instant::now() < end {
        std::thread::sleep(POLL);
    }
}

/// The slice a nested cap joins: [`SLICE_ENV`] as the command sets it, else as this
/// process has it.
fn inherited_slice(envs: &[(OsString, Option<OsString>)]) -> Option<String> {
    let v = match envs.iter().rev().find(|(k, _)| k == SLICE_ENV) {
        Some((_, v)) => v.clone()?,
        None => std::env::var_os(SLICE_ENV)?,
    };
    let v = v.into_string().ok()?;
    valid_slice(&v).then_some(v)
}

/// Where a scope's report file goes: the command's `TMPDIR` when it sets one (a turn's
/// `tmp/`), else this process's.
fn report_dir(envs: &[(OsString, Option<OsString>)]) -> PathBuf {
    envs.iter()
        .rev()
        .find(|(k, _)| k == "TMPDIR")
        .and_then(|(_, v)| v.clone())
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// Runs in the scope: keeps the command in the foreground (so a terminal and
/// Ctrl-C work as without the cap), then records the scope's counters before
/// the cgroup goes away.
const INNER: &str = r#"r=$HGL_AI_CAP_REPORT
unset HGL_AI_CAP_REPORT
echo started > "$r"
trap : INT TERM HUP
"$@"
rc=$?
cg=
while IFS= read -r line; do
  case $line in 0::*) cg=/sys/fs/cgroup${line#0::} ;; esac
done < /proc/self/cgroup
{
  echo "rc $rc"
  cat "$cg/memory.events" 2>/dev/null
  { while IFS= read -r line; do echo "local $line"; done < "$cg/memory.events.local"; } 2>/dev/null
  printf 'peak '
  cat "$cg/memory.peak" 2>/dev/null || echo
} >> "$r"
exit $rc
"#;

/// A command prepared to run under a cap.
///
/// The wrapped [`Command`]'s program, arguments, environment changes and
/// working directory are carried over; its stdio settings cannot be read back,
/// so set them here. `env_clear`, `pre_exec`, user and group settings are not
/// carried over.
pub struct CapCommand {
    spec: CapSpec,
    method: Method,
    program: OsString,
    args: Vec<OsString>,
    envs: Vec<(OsString, Option<OsString>)>,
    cwd: Option<PathBuf>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
    slice: Option<String>,
    group: bool,
}

impl CapCommand {
    /// Wrap `cmd` with the detected [`Method`].
    pub fn new(spec: CapSpec, cmd: &Command) -> CapCommand {
        CapCommand {
            spec,
            method: Method::detect(),
            program: cmd.get_program().to_owned(),
            args: cmd.get_args().map(OsStr::to_owned).collect(),
            envs: cmd
                .get_envs()
                .map(|(k, v)| (k.to_owned(), v.map(OsStr::to_owned)))
                .collect(),
            cwd: cmd.get_current_dir().map(Path::to_path_buf),
            stdin: None,
            stdout: None,
            stderr: None,
            slice: None,
            group: false,
        }
    }

    /// [`Method::Scope`]: run in the transient slice `name` (see [`turn_slice`]), limited to
    /// the spec's memory, which the command's tree joins with its own caps ([`SLICE_ENV`]).
    /// The slice is stopped when the run ends. Ignored by [`Method::Rlimit`].
    pub fn slice(mut self, name: impl Into<String>) -> CapCommand {
        self.slice = Some(name.into());
        self
    }

    /// Start the command as the leader of a new process group; the tree's signals also go
    /// to the group.
    pub fn process_group(mut self) -> CapCommand {
        self.group = true;
        self
    }

    /// Use `method` instead of the detected one.
    pub fn method(mut self, method: Method) -> CapCommand {
        self.method = method;
        self
    }

    pub fn stdin(mut self, s: impl Into<Stdio>) -> CapCommand {
        self.stdin = Some(s.into());
        self
    }

    pub fn stdout(mut self, s: impl Into<Stdio>) -> CapCommand {
        self.stdout = Some(s.into());
        self
    }

    pub fn stderr(mut self, s: impl Into<Stdio>) -> CapCommand {
        self.stderr = Some(s.into());
        self
    }

    pub fn spec(&self) -> &CapSpec {
        &self.spec
    }

    pub fn get_method(&self) -> Method {
        self.method
    }

    /// Start the command. The caller then calls [`CapChild::wait`], which
    /// enforces the time limit and, for [`Method::Rlimit`], the watchdog.
    pub fn spawn(self) -> io::Result<CapChild> {
        let mut report = None;
        let mut own_slice = None;
        let mut cmd = match self.method {
            Method::Scope => {
                let file = tempfile::Builder::new()
                    .prefix("hgl-ai-cap-")
                    .tempfile_in(report_dir(&self.envs))?;
                let join_slice = match &self.slice {
                    Some(s) if valid_slice(s) && make_slice(s, &self.spec) => {
                        own_slice = Some(s.clone());
                        Some(s.clone())
                    }
                    Some(s) => {
                        eprintln!(
                            "{PREFIX}: could not make the slice {s}; the command runs in a scope of its own, and caps inside it are not held to its limit"
                        );
                        None
                    }
                    None => inherited_slice(&self.envs),
                };
                let mut c = Command::new("systemd-run");
                c.args(["--user", "--scope", "--quiet", "--collect"])
                    .args(scope_probe().unwrap_or_default());
                if let Some(s) = &join_slice {
                    c.arg(format!("--slice={s}"));
                }
                c.arg(format!("--property=MemoryMax={}", self.spec.mem_bytes))
                    .args([
                        "--property=MemorySwapMax=0",
                        "--property=OOMPolicy=stop",
                        "--",
                        "/bin/sh",
                        "-c",
                        INNER,
                    ])
                    .arg(PREFIX)
                    .arg(&self.program)
                    .args(&self.args)
                    .env("XDG_RUNTIME_DIR", runtime_dir())
                    .env(REPORT_ENV, file.path());
                report = Some(file);
                c
            }
            Method::Rlimit => {
                let mut c = Command::new(&self.program);
                c.args(&self.args);
                let limit = self.spec.mem_bytes;
                if self.spec.address_limit {
                    // SAFETY: the closure only calls getrlimit/setrlimit, which are async-signal-safe.
                    unsafe {
                        c.pre_exec(move || limit_address_space(limit));
                    }
                }
                c
            }
        };
        for (k, v) in &self.envs {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        if let Some(s) = &own_slice {
            // The tree's own caps join the turn's slice.
            cmd.env(SLICE_ENV, s);
        }
        if self.group {
            cmd.process_group(0);
        }
        if let Some(d) = &self.cwd {
            cmd.current_dir(d);
        }
        if let Some(s) = self.stdin {
            cmd.stdin(s);
        }
        if let Some(s) = self.stdout {
            cmd.stdout(s);
        }
        if let Some(s) = self.stderr {
            cmd.stderr(s);
        }
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                if let Some(s) = &own_slice {
                    remove_slice(s);
                }
                return Err(e);
            }
        };
        let pgid = self.group.then(|| child.id());
        Ok(CapChild {
            child,
            spec: self.spec,
            method: self.method,
            report,
            started: Instant::now(),
            stop: Arc::new(AtomicBool::new(false)),
            slice: own_slice,
            pgid,
        })
    }
}

fn limit_address_space(bytes: u64) -> io::Result<()> {
    let mut cur = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `cur` is a valid rlimit to write into.
    if unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut cur) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let want = libc::rlim_t::try_from(bytes).unwrap_or(libc::RLIM_INFINITY);
    let v = if cur.rlim_max != libc::RLIM_INFINITY && cur.rlim_max < want {
        cur.rlim_max
    } else {
        want
    };
    let new = libc::rlimit {
        rlim_cur: v,
        rlim_max: v,
    };
    // SAFETY: `new` is a valid rlimit.
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &new) } != 0 && cfg!(target_os = "linux") {
        // macOS accepts but does not enforce RLIMIT_AS on some versions; the watchdog covers it.
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Ends a capped run early from another thread: TERM to the tree, KILL after the grace.
#[derive(Debug, Clone)]
pub struct Stopper(Arc<AtomicBool>);

impl Stopper {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A running capped command.
pub struct CapChild {
    child: Child,
    spec: CapSpec,
    method: Method,
    report: Option<tempfile::NamedTempFile>,
    started: Instant,
    stop: Arc<AtomicBool>,
    /// The turn slice this run made, stopped when it ends.
    slice: Option<String>,
    pgid: Option<u32>,
}

/// How a capped run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapOutcome {
    /// The status to report: the command's own, or 137/143 when a limit or a stop ended it.
    pub status: i32,
    /// The command's own status (128 + signal when a signal ended it).
    pub command_status: i32,
    pub method: Method,
    pub memory_limit: bool,
    pub time_limit: bool,
    /// Ended by [`Stopper::stop`].
    pub stopped: bool,
    /// KILL was needed after TERM and the grace.
    pub killed: bool,
    /// Processes killed for memory: by the kernel (scope) or the watchdog (rlimit). In a
    /// scope it is 0 when the limit is known only from a SIGKILL at the peak.
    pub oom_kills: u64,
    /// Peak memory of the tree, when measured: the cgroup's `memory.peak`, or the
    /// watchdog's highest sample.
    pub peak_bytes: Option<u64>,
    /// `false` when `systemd-run` failed before the command started.
    pub started: bool,
}

impl CapOutcome {
    pub fn limit_reached(&self) -> bool {
        self.memory_limit || self.time_limit
    }

    /// The stderr lines that describe a reached limit (empty for a normal run).
    pub fn report(&self, spec: &CapSpec) -> Vec<String> {
        let mut out = Vec::new();
        let size = format_size(spec.mem_bytes);
        if self.memory_limit {
            out.push(match self.method {
                Method::Scope if self.oom_kills > 0 => format!(
                    "{PREFIX}: the {size} memory limit was reached; the kernel killed {} process(es) of this command (exit status {})",
                    self.oom_kills, self.status
                ),
                Method::Scope => format!(
                    "{PREFIX}: the {size} memory limit was reached; the kernel killed this command at a peak of {} (exit status {})",
                    self.peak_bytes.map(format_size).unwrap_or_else(|| "unknown".into()),
                    self.status
                ),
                Method::Rlimit => format!(
                    "{PREFIX}: the {size} memory limit was reached (sampled resident total {}); killed {} process(es) of this command (exit status {})",
                    self.peak_bytes.map(format_size).unwrap_or_else(|| "unknown".into()),
                    self.oom_kills,
                    self.status
                ),
            });
        }
        if self.time_limit {
            let t = spec.time.map(format_duration).unwrap_or_default();
            out.push(if self.killed {
                format!(
                    "{PREFIX}: the {t} time limit was reached; the command did not stop within {} of TERM and was killed (exit status {})",
                    format_duration(spec.kill_after),
                    self.status
                )
            } else {
                format!("{PREFIX}: the {t} time limit was reached; sent TERM to the command (exit status {})", self.status)
            });
        }
        if !self.started {
            out.push(format!(
                "{PREFIX}: systemd-run did not start the command (exit status {})",
                self.status
            ));
        }
        out
    }

    /// `peak N of SIZE`, when the peak was measured.
    pub fn peak_line(&self, spec: &CapSpec) -> Option<String> {
        let sampled = if self.method == Method::Rlimit {
            " (sampled)"
        } else {
            ""
        };
        self.peak_bytes.map(|p| {
            format!(
                "{PREFIX}: peak {} of {}{sampled}",
                format_size(p),
                format_size(spec.mem_bytes)
            )
        })
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Running,
    Term(Instant),
    Kill(Instant),
}

impl CapChild {
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn method(&self) -> Method {
        self.method
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    pub fn stopper(&self) -> Stopper {
        Stopper(self.stop.clone())
    }

    /// The process group the command leads ([`CapCommand::process_group`]).
    pub fn pgid(&self) -> Option<u32> {
        self.pgid
    }

    /// The turn slice this run made ([`CapCommand::slice`]), if it could.
    pub fn slice(&self) -> Option<&str> {
        self.slice.as_deref()
    }

    /// Block until the command ends, enforcing the time limit, the stop request
    /// and (for [`Method::Rlimit`]) the memory watchdog.
    pub fn wait(self) -> io::Result<CapOutcome> {
        self.supervise(None)
    }

    fn supervise(mut self, signals: Option<&AtomicU32>) -> io::Result<CapOutcome> {
        let me = self.child.id();
        let mut phase = Phase::Running;
        let mut time_limit = false;
        let mut stopped = false;
        let mut memory_limit = false;
        let mut watchdog_kills = 0u64;
        let mut peak: Option<u64> = None;
        let mut next_sample = Instant::now();
        // The scope's cgroup, or the turn slice's that holds it and the tree's own caps.
        let mut scope_cg: Option<PathBuf> = None;
        let pgid = self.pgid;
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                break status;
            }
            let now = Instant::now();
            if self.method == Method::Scope && scope_cg.is_none() {
                scope_cg = scope_cgroup(me).map(|cg| match &self.slice {
                    Some(s) => slice_of(&cg, s).unwrap_or(cg),
                    None => cg,
                });
            }
            if let Some(flags) = signals {
                for sig in take_signals(flags) {
                    signal_tree(me, scope_cg.as_deref(), sig, None, pgid);
                }
            }
            match phase {
                Phase::Running => {
                    let over_time = self
                        .spec
                        .time
                        .is_some_and(|t| now.duration_since(self.started) >= t);
                    let stop = self.stop.load(Ordering::SeqCst);
                    if over_time || stop {
                        time_limit = over_time;
                        stopped = !over_time;
                        signal_tree(me, scope_cg.as_deref(), libc::SIGTERM, None, pgid);
                        phase = Phase::Term(now);
                    } else if self.method == Method::Rlimit && now >= next_sample {
                        next_sample = now + SAMPLE;
                        let (members, rss) = tree_rss(me);
                        if rss > 0 {
                            peak = Some(peak.unwrap_or(0).max(rss));
                        }
                        if rss > self.spec.mem_bytes {
                            // Like the kernel at a cgroup limit: KILL, no grace. The Kill
                            // phase repeats it until the tree is gone.
                            memory_limit = true;
                            watchdog_kills = members.len() as u64;
                            phase = Phase::Kill(now);
                        }
                    }
                }
                Phase::Term(since) => {
                    if now.duration_since(since) >= self.spec.kill_after {
                        phase = Phase::Kill(now);
                    }
                }
                Phase::Kill(_) => {}
            }
            if let Phase::Kill(since) = phase {
                // The scope's inner shell is spared for a moment so it can record the counters.
                let spare = (self.method == Method::Scope
                    && now.duration_since(since) < SHELL_GRACE)
                    .then_some(me);
                signal_tree(me, scope_cg.as_deref(), libc::SIGKILL, spare, pgid);
            }
            std::thread::sleep(POLL);
        };
        let command_status = status_code(status);
        let killed = matches!(phase, Phase::Kill(_)) && (time_limit || stopped);
        let mut outcome = CapOutcome {
            status: command_status,
            command_status,
            method: self.method,
            memory_limit,
            time_limit,
            stopped,
            killed,
            oom_kills: watchdog_kills,
            peak_bytes: peak,
            started: true,
        };
        if let Some(file) = &self.report {
            let text = read_limited(file.path(), SMALL_READ).unwrap_or_default();
            let r = parse_report(&text);
            outcome.started = r.started;
            outcome.peak_bytes = r.peak;
            if scope_memory_limit(&r, &outcome, self.spec.mem_bytes) {
                outcome.memory_limit = true;
                outcome.oom_kills = r.oom_kills;
            }
        }
        if let (Some(_), Some(cg)) = (&self.slice, &scope_cg) {
            // The turn's peak counts the caps it ran too.
            let slice_peak = read_limited(cg.join("memory.peak"), SMALL_READ)
                .ok()
                .and_then(|t| t.trim().parse::<u64>().ok());
            outcome.peak_bytes = outcome.peak_bytes.max(slice_peak);
            // What is left in the slice goes as at a stop: TERM, the grace, KILL.
            drain(cg, self.spec.kill_after);
        }
        if let Some(s) = &self.slice {
            remove_slice(s);
        }
        outcome.status = final_status(&outcome);
        Ok(outcome)
    }
}

fn final_status(o: &CapOutcome) -> i32 {
    if o.memory_limit {
        if [STATUS_KILL, STATUS_TERM].contains(&o.command_status) {
            o.command_status
        } else {
            STATUS_KILL
        }
    } else if o.time_limit || o.stopped {
        if o.killed { STATUS_KILL } else { STATUS_TERM }
    } else {
        o.command_status
    }
}

fn status_code(s: ExitStatus) -> i32 {
    s.code()
        .or_else(|| s.signal().map(|n| 128 + n))
        .unwrap_or(1)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Report {
    started: bool,
    /// Processes the kernel killed for memory: `oom_kill`, the larger of `memory.events`
    /// and `memory.events.local`.
    oom_kills: u64,
    /// `oom_group_kill`, likewise.
    group_kills: u64,
    peak: Option<u64>,
}

/// Parse what the scope's inner shell wrote: `started`, `rc N`, the cgroup's
/// `memory.events` lines, the same of `memory.events.local` prefixed `local `, and
/// `peak N`.
fn parse_report(text: &str) -> Report {
    let mut r = Report::default();
    for line in text.lines() {
        let line = line.strip_prefix("local ").unwrap_or(line);
        let (k, v) = line.split_once(' ').unwrap_or((line, ""));
        let n = || v.trim().parse::<u64>().unwrap_or(0);
        match k {
            "started" => r.started = true,
            "oom_kill" => r.oom_kills = r.oom_kills.max(n()),
            "oom_group_kill" => r.group_kills = r.group_kills.max(n()),
            "peak" => r.peak = v.trim().parse().ok(),
            _ => {}
        }
    }
    r
}

/// How far below the limit a peak may be and still count as reaching it: the kernel
/// charges and fails in pages and huge pages, so `memory.peak` of a tree killed at the
/// limit can sit just under `MemoryMax`.
fn peak_slack(limit: u64) -> u64 {
    (limit / 100).clamp(2 << 20, 64 << 20)
}

/// Whether a scope run reached its memory limit: the kernel counted an OOM kill (of a
/// process or a group), or the command died of SIGKILL, not sent by the cap, with its
/// peak at the limit (the kill is counted in `memory.events` after the victim is
/// signalled, so the inner shell can read the counters before the count lands).
fn scope_memory_limit(r: &Report, o: &CapOutcome, limit: u64) -> bool {
    if r.oom_kills > 0 || r.group_kills > 0 {
        return true;
    }
    let killed_by_us = o.time_limit || o.stopped;
    o.command_status == STATUS_KILL
        && !killed_by_us
        && r.peak
            .is_some_and(|p| p.saturating_add(peak_slack(limit)) >= limit)
}

fn cgroup_of(pid: &str) -> Option<String> {
    let text = read_limited(format!("/proc/{pid}/cgroup"), SMALL_READ).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(str::to_string)
}

/// The scope cgroup directory of the child, once `systemd-run` has moved it there.
fn scope_cgroup(pid: u32) -> Option<PathBuf> {
    let theirs = cgroup_of(&pid.to_string())?;
    let ours = cgroup_of("self");
    (theirs.ends_with(".scope") && Some(&theirs) != ours.as_ref())
        .then(|| PathBuf::from(format!("/sys/fs/cgroup{theirs}")))
}

/// The slice directory `name` that holds the scope cgroup `scope`.
fn slice_of(scope: &Path, name: &str) -> Option<PathBuf> {
    let parent = scope.parent()?;
    (parent.file_name()? == name).then(|| parent.to_path_buf())
}

/// The processes of a cgroup and of every cgroup under it (a slice holds none itself).
fn cgroup_procs(dir: &Path) -> Vec<u32> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut seen = 0;
    while let Some(d) = stack.pop() {
        seen += 1;
        if seen > 4096 {
            break;
        }
        if let Ok(text) = read_limited(d.join("cgroup.procs"), LIST_READ) {
            out.extend(text.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
        }
        if let Ok(rd) = std::fs::read_dir(&d) {
            stack.extend(
                rd.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path()),
            );
        }
    }
    out
}

fn send(pid: u32, sig: libc::c_int) {
    if let Ok(p) = libc::pid_t::try_from(pid) {
        // SAFETY: kill has no memory-safety preconditions.
        unsafe {
            libc::kill(p, sig);
        }
    }
}

/// Signal every process of the tree: the cgroup's members (the scope, or the turn slice
/// with every scope in it), the child's descendants, and its process group. `spare` is
/// left alone (and with it the group, which it leads).
fn signal_tree(
    root: u32,
    cgroup: Option<&Path>,
    sig: libc::c_int,
    spare: Option<u32>,
    pgid: Option<u32>,
) {
    let mut pids: HashSet<u32> = descendants(root).into_iter().collect();
    if let Some(dir) = cgroup {
        pids.extend(cgroup_procs(dir));
    }
    for pid in pids {
        if Some(pid) != spare {
            send(pid, sig);
        }
    }
    if spare.is_none()
        && let Some(g) = pgid.and_then(|g| libc::pid_t::try_from(g).ok())
    {
        // SAFETY: killpg has no memory-safety preconditions.
        unsafe {
            libc::killpg(g, sig);
        }
    }
}

/// `pid -> (ppid, resident bytes)` for every process visible here.
fn process_table() -> HashMap<u32, (u32, u64)> {
    if cfg!(target_os = "linux") {
        proc_table()
    } else {
        ps_table()
    }
}

/// From `/proc` (Linux).
fn proc_table() -> HashMap<u32, (u32, u64)> {
    let mut t = HashMap::new();
    // SAFETY: sysconf has no preconditions.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096);
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return t;
    };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = read_limited(format!("/proc/{pid}/stat"), SMALL_READ) else {
            continue;
        };
        // Fields after the command name, which is in parentheses and may hold spaces.
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            continue;
        };
        let f: Vec<&str> = rest.split_whitespace().collect();
        let ppid = f.get(1).and_then(|s| s.parse().ok());
        let rss = f.get(21).and_then(|s| s.parse::<u64>().ok());
        if let (Some(ppid), Some(rss)) = (ppid, rss) {
            t.insert(pid, (ppid, rss * page));
        }
    }
    t
}

/// From `ps` (macOS and other systems without `/proc`).
fn ps_table() -> HashMap<u32, (u32, u64)> {
    use std::io::Read;
    let mut t = HashMap::new();
    let ps = Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid=", "-o", "rss="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut ps) = ps else { return t };
    let mut buf = Vec::new();
    if let Some(out) = ps.stdout.take() {
        let _ = out.take(LIST_READ).read_to_end(&mut buf);
    }
    let _ = ps.kill();
    let _ = ps.wait();
    for line in String::from_utf8_lossy(&buf).lines() {
        let f: Vec<u64> = line
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if let [pid, ppid, rss_kib] = f[..]
            && let (Ok(pid), Ok(ppid)) = (u32::try_from(pid), u32::try_from(ppid))
        {
            t.insert(pid, (ppid, rss_kib * 1024));
        }
    }
    t
}

fn descendants_in(table: &HashMap<u32, (u32, u64)>, root: u32) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &(ppid, _)) in table {
        children.entry(ppid).or_default().push(pid);
    }
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        if let Some(c) = children.get(&out[i]) {
            out.extend(c.iter().copied().filter(|p| *p != root));
        }
        i += 1;
    }
    out
}

fn descendants(root: u32) -> Vec<u32> {
    descendants_in(&process_table(), root)
}

/// The tree's members and their summed resident memory.
fn tree_rss(root: u32) -> (Vec<u32>, u64) {
    let table = process_table();
    let members = descendants_in(&table, root);
    let rss = members
        .iter()
        .filter_map(|p| table.get(p))
        .map(|(_, r)| r)
        .sum();
    (members, rss)
}

/// Signals received by [`run`] while the command runs, as a bit per signal number.
static RECEIVED: AtomicU32 = AtomicU32::new(0);
const FORWARDED: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

extern "C" fn on_signal(sig: libc::c_int) {
    if (0..32).contains(&sig) {
        RECEIVED.fetch_or(1 << sig, Ordering::SeqCst);
    }
}

fn take_signals(flags: &AtomicU32) -> Vec<libc::c_int> {
    let bits = flags.swap(0, Ordering::SeqCst);
    FORWARDED
        .iter()
        .copied()
        .filter(|s| bits & (1 << s) != 0)
        .collect()
}

/// Catch INT, TERM and HUP for the life of the value, to forward them to the tree.
struct Forwarding(Vec<(libc::c_int, libc::sigaction)>);

impl Forwarding {
    fn install() -> Forwarding {
        let mut old = Vec::new();
        for sig in FORWARDED {
            // SAFETY: a zeroed sigaction is valid; the handler only touches an atomic.
            unsafe {
                let mut act: libc::sigaction = std::mem::zeroed();
                act.sa_sigaction = on_signal as *const () as libc::sighandler_t;
                libc::sigemptyset(&mut act.sa_mask);
                let mut prev: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, &act, &mut prev) == 0 {
                    old.push((sig, prev));
                }
            }
        }
        Forwarding(old)
    }
}

impl Drop for Forwarding {
    fn drop(&mut self) {
        for (sig, prev) in &self.0 {
            // SAFETY: restoring the action saved by `install`.
            unsafe {
                libc::sigaction(*sig, prev, std::ptr::null_mut());
            }
        }
    }
}

/// Run `argv` under `spec` with the detected method, inheriting stdio; print the
/// fallback warning, any reached limit and (with `MEMCAP_PEAK` set) the peak on
/// stderr; return the exit status. This is `hgl-ai cap`.
pub fn run(spec: &CapSpec, argv: &[OsString]) -> io::Result<i32> {
    run_with(spec, Method::detect(), argv)
}

/// [`run`] with an explicit method.
pub fn run_with(spec: &CapSpec, method: Method, argv: &[OsString]) -> io::Result<i32> {
    let Some((program, args)) = argv.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no command to run",
        ));
    };
    if let Some(w) = method.warning(spec) {
        eprintln!("{w}");
    }
    let mut base = Command::new(program);
    base.args(args);
    RECEIVED.store(0, Ordering::SeqCst);
    let forwarding = Forwarding::install();
    let child = match CapCommand::new(*spec, &base).method(method).spawn() {
        Ok(c) => c,
        Err(e) => {
            let (msg, code) = match e.kind() {
                io::ErrorKind::NotFound => ("command not found".to_string(), 127),
                io::ErrorKind::PermissionDenied => ("permission denied".to_string(), 126),
                _ => (e.to_string(), 126),
            };
            eprintln!("{PREFIX}: {}: {msg}", program.to_string_lossy());
            return Ok(code);
        }
    };
    let outcome = child.supervise(Some(&RECEIVED))?;
    drop(forwarding);
    if !outcome.started && method == Method::Scope {
        eprintln!("{PREFIX}: systemd-run did not start a scope; retrying without it");
        return run_with(spec, Method::Rlimit, argv);
    }
    for line in outcome.report(spec) {
        eprintln!("{line}");
    }
    if std::env::var_os("MEMCAP_PEAK").is_some_and(|v| !v.is_empty() && v != "0")
        && let Some(line) = outcome.peak_line(spec)
    {
        eprintln!("{line}");
    }
    Ok(outcome.status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("16G"), Ok(16 << 30));
        assert_eq!(parse_size("512M"), Ok(512 << 20));
        assert_eq!(parse_size("100m"), Ok(100 << 20));
        assert_eq!(parse_size("64GiB"), Ok(64 << 30));
        assert_eq!(parse_size("2GB"), Ok(2 << 30));
        assert_eq!(parse_size("1.5G"), Ok(3 << 29));
        assert_eq!(parse_size("4096B"), Ok(4096));
        assert_eq!(parse_size(" 1T "), Ok(1 << 40));
        for bad in [
            "",
            "16",
            "G",
            "0M",
            "1.2.3G",
            "16X",
            "-1G",
            "99999999999P",
            ".G",
            "1GG",
        ] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn formatting() {
        assert_eq!(format_size(16 << 30), "16G");
        assert_eq!(format_size(512 << 20), "512M");
        assert_eq!(format_size(3 << 29), "1.5G");
        assert_eq!(format_size(1000), "1000");
        assert_eq!(format_size((100 << 20) + 1), "101M");
        assert_eq!(format_duration(Duration::from_secs(240)), "4m");
        assert_eq!(format_duration(Duration::from_secs(7200)), "2h");
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
    }

    #[test]
    fn report_parsing() {
        let text = "started\nrc 137\nlow 0\nhigh 0\nmax 41\noom 1\noom_kill 1\noom_group_kill 0\npeak 104857600\n";
        assert_eq!(
            parse_report(text),
            Report {
                started: true,
                oom_kills: 1,
                group_kills: 0,
                peak: Some(100 << 20)
            }
        );
        assert_eq!(
            parse_report("started\nrc 0\npeak \n"),
            Report {
                started: true,
                oom_kills: 0,
                group_kills: 0,
                peak: None
            }
        );
        assert_eq!(parse_report(""), Report::default());
        let text = "started\nrc 137\noom_kill 0\noom_group_kill 0\nlocal oom_kill 2\nlocal oom_group_kill 1\npeak 5\n";
        assert_eq!(
            parse_report(text),
            Report {
                started: true,
                oom_kills: 2,
                group_kills: 1,
                peak: Some(5)
            }
        );
    }

    #[test]
    fn a_kill_at_the_peak_is_the_memory_limit() {
        let limit = 100 << 20;
        // The runner's case: KILL at exactly the limit, no `oom_kill` counted yet.
        let at = Report {
            started: true,
            peak: Some(limit),
            ..Report::default()
        };
        assert!(scope_memory_limit(&at, &outcome(137), limit));
        let near = Report {
            peak: Some(limit - (1 << 20)),
            ..at
        };
        assert!(scope_memory_limit(&near, &outcome(137), limit));
        let low = Report {
            peak: Some(limit / 2),
            ..Report::default()
        };
        assert!(!scope_memory_limit(&low, &outcome(137), limit));
        assert!(!scope_memory_limit(&at, &outcome(0), limit));
        assert!(!scope_memory_limit(&at, &outcome(143), limit));
        let timed = CapOutcome {
            time_limit: true,
            ..outcome(137)
        };
        assert!(
            !scope_memory_limit(&at, &timed, limit),
            "the cap's own KILL"
        );
        let no_peak = Report {
            peak: None,
            ..Report::default()
        };
        assert!(!scope_memory_limit(&no_peak, &outcome(137), limit));
        let group = Report {
            group_kills: 1,
            ..Report::default()
        };
        assert!(scope_memory_limit(&group, &outcome(0), limit));
        assert_eq!(peak_slack(100 << 20), 2 << 20);
        assert_eq!(peak_slack(64 << 30), 64 << 20);
    }

    fn outcome(command_status: i32) -> CapOutcome {
        CapOutcome {
            status: command_status,
            command_status,
            method: Method::Scope,
            memory_limit: false,
            time_limit: false,
            stopped: false,
            killed: false,
            oom_kills: 0,
            peak_bytes: None,
            started: true,
        }
    }

    #[test]
    fn a_reached_limit_never_passes_for_a_finished_run() {
        assert_eq!(final_status(&outcome(7)), 7);
        let mem = CapOutcome {
            memory_limit: true,
            ..outcome(0)
        };
        assert_eq!(final_status(&mem), STATUS_KILL);
        let mem = CapOutcome {
            memory_limit: true,
            ..outcome(143)
        };
        assert_eq!(final_status(&mem), STATUS_TERM);
        let time = CapOutcome {
            time_limit: true,
            ..outcome(0)
        };
        assert_eq!(final_status(&time), STATUS_TERM);
        let time = CapOutcome {
            time_limit: true,
            killed: true,
            ..outcome(1)
        };
        assert_eq!(final_status(&time), STATUS_KILL);
    }

    #[test]
    fn report_lines() {
        let spec = CapSpec::new(100 << 20).with_time(Duration::from_secs(240));
        assert!(outcome(0).report(&spec).is_empty());
        let mem = CapOutcome {
            memory_limit: true,
            oom_kills: 2,
            status: 137,
            ..outcome(137)
        };
        assert_eq!(
            mem.report(&spec),
            [
                "hgl-ai cap: the 100M memory limit was reached; the kernel killed 2 process(es) of this command (exit status 137)"
            ]
        );
        let at_peak = CapOutcome {
            memory_limit: true,
            peak_bytes: Some(100 << 20),
            status: 137,
            ..outcome(137)
        };
        assert_eq!(
            at_peak.report(&spec),
            [
                "hgl-ai cap: the 100M memory limit was reached; the kernel killed this command at a peak of 100M (exit status 137)"
            ]
        );
        let time = CapOutcome {
            time_limit: true,
            killed: true,
            status: 137,
            ..outcome(137)
        };
        assert_eq!(
            time.report(&spec),
            [
                "hgl-ai cap: the 4m time limit was reached; the command did not stop within 10s of TERM and was killed (exit status 137)"
            ]
        );
        let peak = CapOutcome {
            peak_bytes: Some(3 << 29),
            method: Method::Rlimit,
            ..outcome(0)
        };
        assert_eq!(
            peak.peak_line(&CapSpec::new(16 << 30)).unwrap(),
            "hgl-ai cap: peak 1.5G of 16G (sampled)"
        );
        assert!(
            Method::Rlimit
                .warning(&spec)
                .unwrap()
                .contains("per-process address-space limit of 100M")
        );
        assert_eq!(Method::Scope.warning(&spec), None);
    }

    #[test]
    fn turn_slices() {
        assert_eq!(
            turn_slice("build-1-W1-4242-7"),
            "app-hgl_ai_turn_build_1_w1_4242_7.slice"
        );
        assert!(valid_slice(&turn_slice(&"x".repeat(300))));
        for bad in [
            "",
            ".slice",
            "a.scope",
            "-a.slice",
            "a b.slice",
            "a/b.slice",
        ] {
            assert!(!valid_slice(bad), "{bad:?}");
        }
        let set = |v: Option<&str>| vec![(OsString::from(SLICE_ENV), v.map(OsString::from))];
        assert_eq!(
            inherited_slice(&set(Some("app-x.slice"))).as_deref(),
            Some("app-x.slice")
        );
        assert_eq!(inherited_slice(&set(Some("bad name"))), None);
        assert_eq!(inherited_slice(&set(None)), None, "removed by the command");
        let d = tempfile::tempdir().unwrap();
        let tmp = vec![(OsString::from("TMPDIR"), Some(d.path().as_os_str().into()))];
        assert_eq!(report_dir(&tmp), d.path());
        let gone = vec![(OsString::from("TMPDIR"), Some("/nonexistent/x".into()))];
        assert_eq!(report_dir(&gone), std::env::temp_dir());
    }

    #[test]
    fn tree_walk() {
        let t = HashMap::from([
            (1, (0, 10)),
            (2, (1, 20)),
            (3, (2, 30)),
            (4, (1, 40)),
            (5, (9, 50)),
        ]);
        let mut d = descendants_in(&t, 2);
        d.sort();
        assert_eq!(d, [2, 3]);
        let mut d = descendants_in(&t, 1);
        d.sort();
        assert_eq!(d, [1, 2, 3, 4]);
    }

    #[test]
    fn this_process_is_measured() {
        let (members, rss) = tree_rss(std::process::id());
        assert!(members.contains(&std::process::id()));
        assert!(rss > 0);
        // Both table sources see this process with its parent.
        let me = std::process::id();
        let ppid = std::os::unix::process::parent_id();
        assert_eq!(ps_table().get(&me).map(|e| e.0), Some(ppid));
        if cfg!(target_os = "linux") {
            assert_eq!(proc_table().get(&me).map(|e| e.0), Some(ppid));
        }
    }
}
