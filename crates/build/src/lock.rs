//! Exclusive `flock` locks on files: one builder per label on a host, one git
//! operation at a time on a shared clone. Released when dropped (the file closes).

use std::os::unix::io::AsRawFd;
use std::path::Path;

pub struct FileLock(#[allow(dead_code)] std::fs::File);

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
}
