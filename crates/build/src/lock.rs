//! Exclusive `flock` locks on files: one builder per label on a host, one git
//! operation at a time on a shared clone. Released when dropped, by an explicit
//! `LOCK_UN`: a `flock` belongs to the open file description, and a child forked by
//! any thread holds a copy of it until `exec`, so closing the file alone does not
//! release it then.

use std::os::unix::io::AsRawFd;
use std::path::Path;

pub struct FileLock(std::fs::File);

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: a valid descriptor. Unlocks the description, whoever else holds a copy.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn open(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
}

impl FileLock {
    /// Take it, or `None` when another process (or handle) holds it.
    pub fn try_lock(path: &Path) -> std::io::Result<Option<FileLock>> {
        let f = open(path)?;
        // SAFETY: a valid descriptor.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(FileLock(f)));
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(e)
        }
    }

    /// Wait for it.
    pub fn lock(path: &Path) -> std::io::Result<FileLock> {
        let f = open(path)?;
        loop {
            // SAFETY: a valid descriptor.
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(FileLock(f));
            }
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x/a.lock");
        let a = FileLock::try_lock(&p).unwrap().unwrap();
        assert!(FileLock::try_lock(&p).unwrap().is_none());
        drop(a);
        assert!(FileLock::try_lock(&p).unwrap().is_some());
        let _b = FileLock::lock(&p).unwrap();
    }

    /// A copy of the descriptor (what a child forked by another thread holds until
    /// `exec`) does not keep the lock past the drop.
    #[test]
    fn released_despite_a_copy() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.lock");
        let a = FileLock::try_lock(&p).unwrap().unwrap();
        // SAFETY: a valid descriptor; the copy is closed below.
        let copy = unsafe { libc::dup(a.0.as_raw_fd()) };
        assert!(copy >= 0);
        drop(a);
        let taken = FileLock::try_lock(&p).unwrap().is_some();
        // SAFETY: the copy made above.
        unsafe { libc::close(copy) };
        assert!(taken);
    }
}
