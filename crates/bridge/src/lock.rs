//! Exclusive `flock(2)` locks on files.
//!
//! - Turn lock `<task_dir>/turn.lock` ([`FileLock::turn`]): held for a turn. Its open
//!   file is the agent's stdin, holding the turn's input ([`FileLock::input`]), so the
//!   lock stays held while any process of the turn keeps it, even if the bridge itself
//!   died; a second turn in that directory cannot start.
//! - Seat lock `<data_dir>/seats/<seat>.lock` ([`FileLock::try_exclusive`]): held by
//!   whoever serves the seat on this host, the bridge or an interactive session
//!   (PROTO-6), never both. It records the holder's pid. Dropping it unlocks the open
//!   file description explicitly (`LOCK_UN`): a child forked by another thread holds a
//!   copy of the descriptor until it execs, and closing ours alone would leave the seat
//!   held by that copy. A turn lock is not unlocked on drop: the turn's processes keep
//!   it on purpose.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

pub const TURN_LOCK: &str = "turn.lock";

#[derive(Debug)]
pub struct FileLock {
    file: File,
    path: PathBuf,
    /// `LOCK_UN` on drop: a seat lock, not a turn lock.
    unlock: bool,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.unlock {
            use std::os::fd::AsRawFd;
            // SAFETY: flock(2) on our own open descriptor. Unlocks the open file
            // description, whoever else holds a copy of it.
            unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

impl FileLock {
    /// Take the lock without waiting; `Ok(None)` when another holder has it. The holder's
    /// pid is written into the file for diagnostics.
    pub fn try_exclusive(path: &Path) -> std::io::Result<Option<FileLock>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        if !try_flock(&file)? {
            return Ok(None);
        }
        file.set_len(0)?;
        file.rewind()?;
        let _ = writeln!(file, "{}", std::process::id());
        Ok(Some(FileLock {
            file,
            path: path.to_path_buf(),
            unlock: true,
        }))
    }

    /// A turn lock: the file is created if missing and opened read-only; taking the lock
    /// empties it (the previous turn's input).
    pub fn turn(path: &Path) -> std::io::Result<Option<FileLock>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let w = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let file = File::open(path)?;
        if !try_flock(&file)? {
            return Ok(None);
        }
        if file.metadata()?.len() > 0 {
            w.set_len(0)?;
        }
        Ok(Some(FileLock {
            file,
            path: path.to_path_buf(),
            unlock: false,
        }))
    }

    /// [`FileLock::turn`], retried a few times over a quarter second: a fork elsewhere
    /// in this process holds a copy of a just-closed descriptor until its exec.
    pub async fn turn_patiently(path: &Path) -> std::io::Result<Option<FileLock>> {
        for _ in 0..5 {
            if let Some(l) = Self::turn(path)? {
                return Ok(Some(l));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Self::turn(path)
    }

    /// A second descriptor of the same open file (and so of the lock), for a child.
    pub fn share(&self) -> std::io::Result<File> {
        self.file.try_clone()
    }

    /// Make `input` the content of the lock file, and return a descriptor of the lock that
    /// reads it from the start: a child's stdin that keeps the lock held.
    pub fn input(&self, input: &[u8]) -> std::io::Result<File> {
        let mut w = OpenOptions::new().write(true).open(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m = w.metadata()?;
            if self.id() != Some((m.dev(), m.ino())) {
                return Err(std::io::Error::other(format!(
                    "{} is no longer the locked file",
                    self.path.display()
                )));
            }
        }
        w.set_len(0)?;
        w.write_all(input)?;
        let mut f = self.file.try_clone()?;
        f.rewind()?;
        Ok(f)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The descriptor, for a child to inherit.
    #[cfg(unix)]
    pub fn fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.file)
    }

    /// Device and inode of the lock file, to find the processes that hold it.
    #[cfg(unix)]
    pub fn id(&self) -> Option<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        self.file.metadata().ok().map(|m| (m.dev(), m.ino()))
    }
}

/// The pid last written into a lock file, if any.
pub fn holder(path: &Path) -> Option<u32> {
    let mut s = String::new();
    File::open(path).ok()?.read_to_string(&mut s).ok()?;
    s.trim().parse().ok()
}

#[cfg(unix)]
fn try_flock(f: &File) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: flock(2) on a descriptor this function borrows; no memory is shared.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(e)
    }
}

#[cfg(not(unix))]
fn try_flock(_: &File) -> std::io::Result<bool> {
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exclusive_until_dropped() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x/seat.lock");
        let a = FileLock::try_exclusive(&p).unwrap().expect("free");
        assert!(
            FileLock::try_exclusive(&p).unwrap().is_none(),
            "a second open file description conflicts"
        );
        assert_eq!(holder(&p), Some(std::process::id()));
        drop(a);
        let t = d.path().join("y/turn.lock");
        std::fs::create_dir_all(t.parent().unwrap()).unwrap();
        std::fs::write(&t, "stale").unwrap();
        let a = FileLock::turn(&t).unwrap().expect("free");
        assert_eq!(
            std::fs::metadata(&t).unwrap().len(),
            0,
            "taking a turn lock empties it"
        );
        assert!(a.id().is_some());
        let mut stdin = a.input(b"the prompt\n").unwrap();
        let mut got = String::new();
        stdin.read_to_string(&mut got).unwrap();
        assert_eq!(got, "the prompt\n", "the descriptor reads the input");
        let shared = a.share().unwrap();
        drop(a);
        assert!(
            FileLock::turn(&t).unwrap().is_none(),
            "the shared descriptors still hold it"
        );
        drop((shared, stdin));
        assert!(FileLock::turn_patiently(&t).await.unwrap().is_some());
        assert_eq!(std::fs::metadata(&t).unwrap().len(), 0, "emptied again");
    }

    /// A forked child holding copies of this process's descriptors until it is told to
    /// exit: what a child forked by another thread is until its exec.
    #[cfg(unix)]
    struct Forked {
        pid: libc::pid_t,
        go: libc::c_int,
    }

    #[cfg(unix)]
    impl Forked {
        fn spawn() -> Forked {
            let mut p = [0 as libc::c_int; 2];
            // SAFETY: pipe2(2) into a two-element array; close-on-exec, so no other
            // test's exec'd child keeps it.
            assert_eq!(unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
            // SAFETY: the child calls only async-signal-safe functions (close, read,
            // _exit) before it exits.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0, "fork");
            if pid == 0 {
                let mut b = 0u8;
                // SAFETY: the child's copies of the pipe; read blocks until the parent
                // closes its end.
                unsafe {
                    libc::close(p[1]);
                    libc::read(p[0], (&raw mut b).cast(), 1);
                    libc::_exit(0);
                }
            }
            // SAFETY: the parent's read end, not used by it.
            unsafe { libc::close(p[0]) };
            Forked { pid, go: p[1] }
        }

        fn finish(self) {
            let mut st = 0;
            // SAFETY: our write end, then the child forked above.
            unsafe {
                libc::close(self.go);
                assert_eq!(libc::waitpid(self.pid, &mut st, 0), self.pid);
            }
            assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0);
        }
    }

    /// The seat lock is released when dropped, though a forked child still holds a copy
    /// of its descriptor (#68).
    #[cfg(unix)]
    #[test]
    fn seat_lock_released_despite_a_forked_copy() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("seat.lock");
        let a = FileLock::try_exclusive(&p).unwrap().expect("free");
        let child = Forked::spawn();
        drop(a);
        let again = FileLock::try_exclusive(&p).unwrap();
        child.finish();
        assert!(again.is_some(), "the forked copy does not keep the seat");
    }

    /// The turn lock is not unlocked on drop: a forked child (the turn's agent) keeps it
    /// after the parent drops its lock, until it exits.
    #[cfg(unix)]
    #[tokio::test]
    async fn turn_lock_held_by_a_forked_child_after_drop() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(TURN_LOCK);
        let a = FileLock::turn(&p).unwrap().expect("free");
        let child = Forked::spawn();
        drop(a);
        let during = FileLock::turn(&p).unwrap().is_none();
        child.finish();
        assert!(during, "the child still holds the turn lock");
        assert!(FileLock::turn_patiently(&p).await.unwrap().is_some());
    }
}
