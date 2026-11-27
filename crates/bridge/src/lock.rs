//! Exclusive `flock(2)` locks on files.
//!
//! - Turn lock `<task_dir>/turn.lock` ([`FileLock::turn`]): held for a turn. The file
//!   stays empty and its open file is the agent's stdin, so the lock stays held while
//!   any process of the turn keeps it, even if the bridge itself died; a second turn in
//!   that directory cannot start.
//! - Seat lock `<data_dir>/seats/<seat>.lock` ([`FileLock::try_exclusive`]): held by
//!   whoever serves the seat on this host, the bridge or an interactive session
//!   (PROTO-6), never both. It records the holder's pid.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

pub const TURN_LOCK: &str = "turn.lock";

#[derive(Debug)]
pub struct FileLock {
    file: File,
    path: PathBuf,
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
        }))
    }

    /// A turn lock: the file is created empty and opened read-only, and stays empty.
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
            "a turn lock stays empty: it is the agent's stdin"
        );
        assert!(a.id().is_some());
        let shared = a.share().unwrap();
        drop(a);
        assert!(
            FileLock::turn(&t).unwrap().is_none(),
            "the shared descriptor still holds it"
        );
        drop(shared);
        assert!(FileLock::turn_patiently(&t).await.unwrap().is_some());
    }
}
