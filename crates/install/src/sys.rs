//! The installer's only access to the host: files, commands, ports.
//!
//! [`RealSystem`] acts; [`DryRun`] reads and queries through another system
//! but only reports what it would write or run.

use std::fmt;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::env::Env;

/// A command line with optional environment, directory and time limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub timeout: Option<Duration>,
}

impl Cmd {
    pub fn new(program: impl Into<String>) -> Self {
        Cmd {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            timeout: None,
        }
    }
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(it.into_iter().map(Into::into));
        self
    }
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }
    pub fn cwd(mut self, d: impl Into<PathBuf>) -> Self {
        self.cwd = Some(d.into());
        self
    }
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }
    /// Program and arguments.
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }
    /// The program and arguments joined with single spaces (test matching).
    pub fn line(&self) -> String {
        self.argv().join(" ")
    }
}

/// Quote one word for a POSIX shell when it needs it.
pub fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%^".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let words: Vec<String> = self.argv().iter().map(|w| shell_quote(w)).collect();
        f.write_str(&words.join(" "))
    }
}

/// What a command printed and how it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// Exit code; `None` when killed by a signal or the time limit.
    pub code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(stdout: impl Into<String>) -> Self {
        Output {
            code: Some(0),
            timed_out: false,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }
    pub fn failure(code: i32, stderr: impl Into<String>) -> Self {
        Output {
            code: Some(code),
            timed_out: false,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
    pub fn status_text(&self) -> String {
        match (self.code, self.timed_out) {
            (_, true) => "timed out".into(),
            (Some(c), _) => format!("exit {c}"),
            (None, _) => "killed".into(),
        }
    }
    /// The last non-empty line of stderr, else of stdout.
    pub fn detail(&self) -> String {
        let last = |s: &str| {
            s.lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
        };
        last(&self.stderr)
            .or_else(|| last(&self.stdout))
            .unwrap_or_default()
    }
}

/// Host access. Reads and `query` are side-effect free; everything else changes the host.
pub trait System {
    fn env(&self) -> &Env;
    /// `true` when writes and `run` are only reported.
    fn dry_run(&self) -> bool {
        false
    }
    /// One line of user-facing progress.
    fn say(&self, line: &str);

    fn read(&self, p: &Path) -> io::Result<Vec<u8>>;
    fn read_to_string(&self, p: &Path) -> io::Result<String> {
        String::from_utf8(self.read(p)?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
    fn exists(&self, p: &Path) -> bool;
    /// Permission bits (`0o777` mask).
    fn mode(&self, p: &Path) -> io::Result<u32>;
    /// Find an executable on the environment's `PATH`.
    fn which(&self, name: &str) -> Option<PathBuf>;
    /// Find an executable on another `PATH` (another user's).
    fn which_on(&self, path_var: &str, name: &str) -> Option<PathBuf> {
        which_in(path_var, name)
    }
    /// Run a command that changes nothing (status, version, listing).
    fn query(&self, cmd: &Cmd) -> io::Result<Output>;
    fn port_open(&self, host: &str, port: u16) -> bool;

    /// Write atomically with `mode`, creating parent directories.
    fn write(&self, p: &Path, data: &[u8], mode: u32) -> io::Result<()>;
    fn mkdir(&self, p: &Path) -> io::Result<()>;
    fn chmod(&self, p: &Path, mode: u32) -> io::Result<()>;
    /// Remove a file; absent is fine.
    fn remove_file(&self, p: &Path) -> io::Result<()>;
    /// Remove a directory tree; absent is fine.
    fn remove_dir_all(&self, p: &Path) -> io::Result<()>;
    /// Run a command that changes the host.
    fn run(&self, cmd: &Cmd) -> io::Result<Output>;
    /// Ask the person at the terminal; `false` without one.
    fn confirm(&self, _question: &str) -> bool {
        false
    }
    /// Whether this process may read `p` (a probe; nothing is read).
    fn can_read(&self, p: &Path) -> bool {
        std::fs::File::open(p).is_ok()
    }
    /// The numeric owner of `p`.
    fn owner_uid(&self, p: &Path) -> io::Result<u32> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            std::fs::metadata(p).map(|m| m.uid())
        }
        #[cfg(not(unix))]
        {
            let _ = p;
            Ok(0)
        }
    }
}

/// The host itself.
pub struct RealSystem {
    env: Env,
}

impl RealSystem {
    pub fn new(env: Env) -> Self {
        RealSystem { env }
    }
}

impl System for RealSystem {
    fn env(&self) -> &Env {
        &self.env
    }
    fn say(&self, line: &str) {
        println!("{line}");
    }
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(p)
    }
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }
    fn mode(&self, p: &Path) -> io::Result<u32> {
        file_mode(p)
    }
    fn which(&self, name: &str) -> Option<PathBuf> {
        which_in(&self.env.path_var, name)
    }
    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        exec(cmd)
    }
    fn port_open(&self, host: &str, port: u16) -> bool {
        use std::net::{TcpStream, ToSocketAddrs};
        let Ok(addrs) = (host, port).to_socket_addrs() else {
            return false;
        };
        addrs
            .into_iter()
            .any(|a| TcpStream::connect_timeout(&a, Duration::from_secs(1)).is_ok())
    }
    fn write(&self, p: &Path, data: &[u8], mode: u32) -> io::Result<()> {
        write_atomic(p, data, mode)
    }
    fn mkdir(&self, p: &Path) -> io::Result<()> {
        std::fs::create_dir_all(p)
    }
    fn chmod(&self, p: &Path, mode: u32) -> io::Result<()> {
        set_mode(p, mode)
    }
    fn remove_file(&self, p: &Path) -> io::Result<()> {
        match std::fs::remove_file(p) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }
    fn remove_dir_all(&self, p: &Path) -> io::Result<()> {
        match std::fs::remove_dir_all(p) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }
    fn run(&self, cmd: &Cmd) -> io::Result<Output> {
        exec(cmd)
    }
    fn confirm(&self, question: &str) -> bool {
        use std::io::{BufRead as _, IsTerminal as _, Write as _};
        if !std::io::stdin().is_terminal() {
            println!("{question} [y/N] no (no terminal; --print-root-steps prints them)");
            return false;
        }
        print!("{question} [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        matches!(line.trim(), "y" | "Y" | "yes")
    }
}

/// Reports changes instead of making them; reads and queries go to `inner`.
pub struct DryRun<'a> {
    inner: &'a dyn System,
}

impl<'a> DryRun<'a> {
    pub fn new(inner: &'a dyn System) -> Self {
        DryRun { inner }
    }
}

impl System for DryRun<'_> {
    fn env(&self) -> &Env {
        self.inner.env()
    }
    fn dry_run(&self) -> bool {
        true
    }
    fn say(&self, line: &str) {
        self.inner.say(line)
    }
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(p)
    }
    fn exists(&self, p: &Path) -> bool {
        self.inner.exists(p)
    }
    fn mode(&self, p: &Path) -> io::Result<u32> {
        self.inner.mode(p)
    }
    fn which(&self, name: &str) -> Option<PathBuf> {
        self.inner.which(name)
    }
    fn which_on(&self, path_var: &str, name: &str) -> Option<PathBuf> {
        self.inner.which_on(path_var, name)
    }
    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        self.inner.query(cmd)
    }
    fn port_open(&self, host: &str, port: u16) -> bool {
        self.inner.port_open(host, port)
    }
    fn write(&self, p: &Path, _data: &[u8], mode: u32) -> io::Result<()> {
        self.say(&format!("would write {} (mode {mode:o})", p.display()));
        Ok(())
    }
    fn mkdir(&self, p: &Path) -> io::Result<()> {
        if !self.inner.exists(p) {
            self.say(&format!("would create {}", p.display()));
        }
        Ok(())
    }
    fn chmod(&self, p: &Path, mode: u32) -> io::Result<()> {
        self.say(&format!("would chmod {mode:o} {}", p.display()));
        Ok(())
    }
    fn remove_file(&self, p: &Path) -> io::Result<()> {
        if self.inner.exists(p) {
            self.say(&format!("would remove {}", p.display()));
        }
        Ok(())
    }
    fn remove_dir_all(&self, p: &Path) -> io::Result<()> {
        if self.inner.exists(p) {
            self.say(&format!("would remove {}/", p.display()));
        }
        Ok(())
    }
    fn owner_uid(&self, p: &Path) -> io::Result<u32> {
        self.inner.owner_uid(p)
    }
    fn confirm(&self, question: &str) -> bool {
        self.say(&format!("would ask: {question}"));
        true
    }
    fn can_read(&self, p: &Path) -> bool {
        self.inner.can_read(p)
    }
    fn run(&self, cmd: &Cmd) -> io::Result<Output> {
        self.say(&format!("would run: {cmd}"));
        Ok(Output::success(""))
    }
}

/// Search a `PATH` value for an executable file.
pub fn which_in(path_var: &str, name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_executable(&p).then_some(p);
    }
    path_var
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    match std::fs::metadata(p) {
        Ok(m) if m.is_file() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                m.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        _ => false,
    }
}

pub(crate) fn file_mode(p: &Path) -> io::Result<u32> {
    let m = std::fs::metadata(p)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Ok(m.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        Ok(if m.permissions().readonly() {
            0o444
        } else {
            0o644
        })
    }
}

fn set_mode(p: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (p, mode);
        Ok(())
    }
}

/// Write through a temporary file created with `mode`, then rename, so a
/// secret is never readable by others, not even briefly.
pub(crate) fn write_atomic(p: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = p.with_file_name(format!(".{name}.hgl-ai-tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(mode);
    }
    let res = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        set_mode(&tmp, mode)?;
        std::fs::rename(&tmp, p)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

fn exec(cmd: &Cmd) -> io::Result<Output> {
    let mut c = std::process::Command::new(&cmd.program);
    c.args(&cmd.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &cmd.env {
        c.env(k, v);
    }
    if let Some(d) = &cmd.cwd {
        c.current_dir(d);
    }
    // A command with a time limit gets its own process group, so the limit
    // stops everything it started.
    #[cfg(unix)]
    if cmd.timeout.is_some() {
        use std::os::unix::process::CommandExt as _;
        c.process_group(0);
    }
    let mut child = c.spawn()?;
    let reader = |r: Option<Box<dyn io::Read + Send>>| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            let _ = tx.send(buf);
        });
        rx
    };
    let out_rx = reader(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn io::Read + Send>),
    );
    let err_rx = reader(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn io::Read + Send>),
    );
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if cmd.timeout.is_some_and(|t| start.elapsed() >= t) {
            #[cfg(unix)]
            let _ = std::process::Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{}", child.id()))
                .stderr(Stdio::null())
                .status();
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // A grandchild may keep a pipe open; do not wait on it for long.
    let grace = if status.is_some() {
        Duration::from_secs(2)
    } else {
        Duration::from_millis(200)
    };
    let take = |rx: mpsc::Receiver<Vec<u8>>| {
        String::from_utf8_lossy(&rx.recv_timeout(grace).unwrap_or_default()).into_owned()
    };
    Ok(Output {
        code: status.and_then(|s| s.code()),
        timed_out: status.is_none(),
        stdout: take(out_rx),
        stderr: take(err_rx),
    })
}

/// A test system: real files, but only under a temporary root; commands are
/// recorded and answered by a closure.
#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;
    use crate::env::Os;

    pub fn env(home: &Path, os: Os) -> Env {
        Env {
            home: home.to_path_buf(),
            config_path: home.join(".config/hgl-ai/config.toml"),
            xdg_config_home: home.join(".config"),
            path_var: "/usr/local/bin:/usr/bin:/bin".into(),
            os,
            arch: "x86_64".into(),
            exe: home.join(".cargo/bin/hgl-ai"),
            in_tmux: false,
            runtime_root: None,
        }
    }

    type Responder = Box<dyn Fn(&Cmd) -> Output>;

    pub struct Fake {
        pub env: Env,
        pub root: PathBuf,
        pub tools: RefCell<BTreeMap<String, PathBuf>>,
        /// Every command, with `true` for `run` and `false` for `query`.
        pub cmds: RefCell<Vec<(bool, Cmd)>>,
        pub said: RefCell<Vec<String>>,
        pub respond: RefCell<Responder>,
        pub ports_open: std::cell::Cell<bool>,
        /// The answer to every confirmation.
        pub confirms: std::cell::Cell<bool>,
        /// Paths outside the fake's root this process may read.
        pub readable: RefCell<std::collections::BTreeSet<PathBuf>>,
    }

    impl Fake {
        /// A fake whose home is `<root>/home`.
        pub fn new(root: &Path, os: Os) -> Fake {
            let home = root.join("home");
            std::fs::create_dir_all(&home).unwrap();
            Fake {
                env: env(&home, os),
                root: root.to_path_buf(),
                tools: RefCell::new(BTreeMap::new()),
                cmds: RefCell::new(Vec::new()),
                said: RefCell::new(Vec::new()),
                respond: RefCell::new(Box::new(|_| Output::success(""))),
                ports_open: std::cell::Cell::new(true),
                confirms: std::cell::Cell::new(true),
                readable: RefCell::new(std::collections::BTreeSet::new()),
            }
        }
        pub fn tool(&self, name: &str) -> PathBuf {
            self.tool_at(name, Path::new("/usr/bin"))
        }
        /// `name` found in `dir`.
        pub fn tool_at(&self, name: &str, dir: &Path) -> PathBuf {
            let p = dir.join(name);
            self.tools.borrow_mut().insert(name.into(), p.clone());
            p
        }
        pub fn on(&self, f: impl Fn(&Cmd) -> Output + 'static) {
            *self.respond.borrow_mut() = Box::new(f);
        }
        /// `run` command lines, in order.
        pub fn runs(&self) -> Vec<String> {
            self.cmds
                .borrow()
                .iter()
                .filter(|(m, _)| *m)
                .map(|(_, c)| c.line())
                .collect()
        }
        pub fn output(&self) -> String {
            self.said.borrow().join("\n")
        }
        fn guard(&self, p: &Path) {
            assert!(
                p.starts_with(&self.root),
                "fake system touched {} outside {}",
                p.display(),
                self.root.display()
            );
        }
    }

    impl System for Fake {
        fn env(&self) -> &Env {
            &self.env
        }
        fn say(&self, line: &str) {
            self.said.borrow_mut().push(line.to_string());
        }
        fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
            self.guard(p);
            std::fs::read(p)
        }
        fn exists(&self, p: &Path) -> bool {
            p.starts_with(&self.root) && p.exists()
        }
        fn mode(&self, p: &Path) -> io::Result<u32> {
            self.guard(p);
            file_mode(p)
        }
        fn which(&self, name: &str) -> Option<PathBuf> {
            self.tools.borrow().get(name).cloned()
        }
        fn which_on(&self, path_var: &str, name: &str) -> Option<PathBuf> {
            self.tools.borrow().get(name).cloned().filter(|p| {
                path_var
                    .split(':')
                    .any(|d| !d.is_empty() && p.parent() == Some(Path::new(d)))
            })
        }
        fn query(&self, cmd: &Cmd) -> io::Result<Output> {
            self.cmds.borrow_mut().push((false, cmd.clone()));
            Ok((self.respond.borrow())(cmd))
        }
        fn port_open(&self, _host: &str, _port: u16) -> bool {
            self.ports_open.get()
        }
        fn write(&self, p: &Path, data: &[u8], mode: u32) -> io::Result<()> {
            self.guard(p);
            write_atomic(p, data, mode)
        }
        fn mkdir(&self, p: &Path) -> io::Result<()> {
            self.guard(p);
            std::fs::create_dir_all(p)
        }
        fn chmod(&self, p: &Path, mode: u32) -> io::Result<()> {
            self.guard(p);
            set_mode(p, mode)
        }
        fn remove_file(&self, p: &Path) -> io::Result<()> {
            self.guard(p);
            match std::fs::remove_file(p) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                r => r,
            }
        }
        fn remove_dir_all(&self, p: &Path) -> io::Result<()> {
            self.guard(p);
            match std::fs::remove_dir_all(p) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                r => r,
            }
        }
        fn confirm(&self, question: &str) -> bool {
            self.say(&format!("ask: {question}"));
            self.confirms.get()
        }
        fn can_read(&self, p: &Path) -> bool {
            if p.starts_with(&self.root) {
                std::fs::File::open(p).is_ok()
            } else {
                self.readable.borrow().contains(p)
            }
        }
        fn run(&self, cmd: &Cmd) -> io::Result<Output> {
            self.cmds.borrow_mut().push((true, cmd.clone()));
            Ok((self.respond.borrow())(cmd))
        }
    }

    /// Every file under `dir`, relative, sorted.
    pub fn files(dir: &Path) -> Vec<String> {
        fn walk(base: &Path, d: &Path, out: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(d) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(base, &p, out);
                } else {
                    out.push(p.strip_prefix(base).unwrap().display().to_string());
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("abc-1.2/x"), "abc-1.2/x");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        let c = Cmd::new("tmux")
            .args(["new-session", "-s", "hgl-w1"])
            .arg("a b");
        assert_eq!(c.to_string(), "tmux new-session -s hgl-w1 'a b'");
    }

    #[test]
    fn atomic_write_sets_mode() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a/b/secret");
        write_atomic(&p, b"x", 0o600).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"x");
        #[cfg(unix)]
        assert_eq!(file_mode(&p).unwrap(), 0o600);
        write_atomic(&p, b"y", 0o644).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"y");
        assert_eq!(
            std::fs::read_dir(p.parent().unwrap()).unwrap().count(),
            1,
            "no temporary left"
        );
    }

    #[cfg(unix)]
    #[test]
    fn exec_captures_and_times_out() {
        let o = exec(&Cmd::new("sh").args(["-c", "echo out; echo err >&2; exit 3"])).unwrap();
        assert_eq!(
            (o.code, o.stdout.as_str(), o.stderr.as_str()),
            (Some(3), "out\n", "err\n")
        );
        let t = Instant::now();
        let o = exec(
            &Cmd::new("sh")
                .args(["-c", "sleep 30; echo late"])
                .timeout(Duration::from_millis(100)),
        )
        .unwrap();
        assert!(o.timed_out && !o.ok());
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "the group was killed, not waited for"
        );
    }

    #[test]
    fn dry_run_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let f = fake::Fake::new(d.path(), crate::Os::Linux);
        let dry = DryRun::new(&f);
        let p = f.env.home.join("x/y");
        dry.write(&p, b"z", 0o600).unwrap();
        dry.mkdir(&f.env.home.join("dir")).unwrap();
        dry.run(&Cmd::new("systemctl").arg("--user")).unwrap();
        assert!(!p.exists() && !f.env.home.join("dir").exists());
        assert!(f.runs().is_empty());
        assert!(f.output().contains("would run: systemctl --user"));
    }
}
