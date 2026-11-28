//! `hgl-ai server install` at a terminal (hgl-ai#90): the binary runs on a pseudo-terminal
//! with a `sudo` on `PATH` that records each step and whether its input is a terminal,
//! and runs only the server user's token writer. The coordinator's token is typed at
//! the prompt; nothing under `sudo` reads the terminal, and every step after it runs.
//! Nothing here touches a running hub or service: the "hub" is a listener of the test.
#![cfg(target_os = "linux")]

use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const TOKEN_PROMPT: &str = "GitHub token for the coordinator (Enter keeps the current one): ";
const CONFIRM: &str = "Run it? [y/N] ";
const SERVER_USER: &str = "hgl-ai-pty-test";

/// Records `tty|notty ARGS` per call; `-u U cat F` prints a credential, `-u U sh ...`
/// (the writers) runs as this user, the rest only succeeds.
const SUDO: &str = r#"#!/bin/sh
if [ -t 0 ]; then t=tty; else t=notty; fi
printf '%s %s\n' "$t" "$*" >>"$SUDO_LOG"
if [ "$1" = -u ]; then
  shift 2
  case "$1" in
    cat) printf 'seat-credential\n'; exit 0 ;;
    sh) exec "$@" ;;
  esac
fi
exit 0
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    hub: std::net::TcpListener,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("sudo"), SUDO).unwrap();
        std::fs::set_permissions(bin.join("sudo"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        // What the server user's own step (here only recorded) makes.
        std::fs::create_dir_all(root.join("srv/.config/hgl-ai")).unwrap();
        let hub = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        Fixture {
            _dir: dir,
            root,
            hub,
        }
    }

    fn token_file(&self) -> PathBuf {
        self.root.join("srv/.config/hgl-ai/github.token")
    }

    fn sudo_log(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("sudo.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Run the install on a pseudo-terminal, `y` at every confirmation and `keys` at
    /// the token prompt. Returns the exit status, everything the terminal showed, and
    /// whether the terminal echoes at the end.
    fn install(&self, keys: &str) -> (std::process::ExitStatus, String, bool) {
        let _ = std::fs::remove_file(self.root.join("sudo.log"));
        let (master, slave) = openpty();
        let port = self.hub.local_addr().unwrap().port();
        let fd = |f: &OwnedFd| Stdio::from(f.try_clone().unwrap());
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_hgl-ai"));
        cmd.args(["server", "install", "--server-user", SERVER_USER])
            .arg("--server-home")
            .arg(self.root.join("srv"))
            .args(["--listen", "127.0.0.1", "--port", &port.to_string()])
            .args(["--host", "ptyhost"])
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env(
                "HGL_AI_CONFIG",
                self.root.join("home/.config/hgl-ai/config.toml"),
            )
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("SUDO_LOG", self.root.join("sudo.log"))
            .env("TERM", "dumb")
            .stdin(fd(&slave))
            .stdout(fd(&slave))
            .stderr(fd(&slave));
        // SAFETY: setsid and ioctl only, between fork and exec: the terminal becomes
        // the child's controlling terminal, its /dev/tty.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let mut reader = std::fs::File::from(master.try_clone().unwrap());
        let mut writer = std::fs::File::from(master);
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            // EIO once the last process holding the terminal is gone.
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut shown = String::new();
        let (mut confirmed, mut asked) = (0, 0);
        let start = Instant::now();
        let status = loop {
            while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(20)) {
                shown.push_str(&String::from_utf8_lossy(&chunk));
            }
            // Answer each prompt once it shows, never ahead of it.
            while shown.matches(CONFIRM).count() > confirmed {
                confirmed += 1;
                writer.write_all(b"y\n").unwrap();
            }
            while shown.matches(TOKEN_PROMPT).count() > asked {
                asked += 1;
                writer.write_all(keys.as_bytes()).unwrap();
            }
            if let Some(st) = child.try_wait().unwrap() {
                while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(200)) {
                    shown.push_str(&String::from_utf8_lossy(&chunk));
                }
                break st;
            }
            if start.elapsed() > Duration::from_secs(60) {
                let _ = child.kill();
                panic!("the install hangs; the terminal shows:\n{shown}");
            }
        };
        assert_eq!(asked, 1, "one token prompt:\n{shown}");
        let mut t = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `slave` is open; tcgetattr fills `t` on success.
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), t.as_mut_ptr()) },
            0
        );
        // SAFETY: filled above.
        let echo = unsafe { t.assume_init() }.c_lflag & libc::ECHO != 0;
        (status, shown, echo)
    }
}

fn openpty() -> (OwnedFd, OwnedFd) {
    let (mut m, mut s) = (-1, -1);
    // SAFETY: openpty writes two descriptors, which are then owned here.
    let r = unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(r, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: fresh descriptors from openpty.
    unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) }
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_token_is_typed_at_the_terminal_and_the_install_goes_on() {
    let fx = Fixture::new();
    let (status, shown, echo) = fx.install("ghp_ptytest\n");
    assert!(status.success(), "{status:?}:\n{shown}");
    assert!(echo, "echo back on");
    assert!(!shown.contains("ghp_ptytest"), "the token echoed:\n{shown}");
    assert_eq!(
        std::fs::read_to_string(fx.token_file()).unwrap(),
        "ghp_ptytest\n"
    );
    assert_eq!(mode(&fx.token_file()), 0o600);
    let log = fx.sudo_log();
    let writer = format!("notty -u {SERVER_USER} sh -c umask 077; f=$1;");
    let w = log
        .iter()
        .position(|l| l.starts_with(&writer))
        .unwrap_or_else(|| panic!("no token writer: {log:#?}"));
    // Every other step had the terminal, for sudo's password; none was handed the token.
    for (i, l) in log.iter().enumerate() {
        assert!(i == w || l.starts_with("tty "), "{l}");
        assert!(!l.contains("ghp_ptytest") && !l.contains("/dev/tty"), "{l}");
    }
    // The steps after it ran: the coordinator, this host's credential.
    let restart = log
        .iter()
        .position(|l| l == "tty systemctl restart hgl-ai-coord.service")
        .unwrap_or_else(|| panic!("{log:#?}"));
    assert!(w < restart, "{log:#?}");
    let cred = fx.root.join("home/.config/hgl-ai/hub.token");
    assert_eq!(std::fs::read_to_string(&cred).unwrap(), "seat-credential\n");
    assert!(
        shown.contains("Next, on this host, as yourself:"),
        "{shown}"
    );

    // A rerun, Enter at the prompt: the token stays, nothing writes it, the rest runs.
    let (status, shown, _) = fx.install("\n");
    assert!(status.success(), "{status:?}:\n{shown}");
    assert!(shown.contains("kept the current one"), "{shown}");
    assert_eq!(
        std::fs::read_to_string(fx.token_file()).unwrap(),
        "ghp_ptytest\n"
    );
    let log = fx.sudo_log();
    assert!(
        !log.iter().any(|l| l.contains("umask 077; f=$1;")),
        "{log:#?}"
    );
    assert!(
        log.iter()
            .any(|l| l == "tty systemctl restart hgl-ai-coord.service"),
        "{log:#?}"
    );
}

/// Ctrl-C at the token prompt: the install stops there, and echo is back on.
#[test]
fn an_interrupted_token_prompt_turns_echo_back_on() {
    use std::os::unix::process::ExitStatusExt as _;
    let fx = Fixture::new();
    let (status, shown, echo) = fx.install("\x03");
    assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}:\n{shown}");
    assert!(echo, "echo stays off:\n{shown}");
    assert!(!fx.token_file().exists());
    let log = fx.sudo_log();
    assert!(
        !log.iter().any(|l| l.contains("restart hgl-ai-coord")),
        "{log:#?}"
    );
}
