//! The landing lock: one landing (or release watch) per repository at a time on the
//! controller's host. An advisory `flock` on `<data_dir>/gh/land-<repo>.lock`, so it is
//! released when the holder exits, however it exits.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{Error, Result, io_err};

pub struct Lock {
    _file: File,
    pub path: PathBuf,
}

#[cfg(unix)]
fn try_flock(f: &File) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: `flock` on a descriptor this process owns for the call's duration.
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
fn try_flock(_f: &File) -> std::io::Result<bool> {
    Ok(true)
}

impl Lock {
    /// Take the lock at `path`, waiting at most `wait`.
    pub async fn acquire(path: &Path, wait: Duration) -> Result<Lock> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io_err(format!("creating {}", dir.display())))?;
        }
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(io_err(format!("opening {}", path.display())))?;
        let start = Instant::now();
        loop {
            if try_flock(&file).map_err(io_err(format!("locking {}", path.display())))? {
                return Ok(Lock {
                    _file: file,
                    path: path.to_path_buf(),
                });
            }
            if start.elapsed() >= wait {
                return Err(Error::Invalid(format!(
                    "could not take the landing lock {} in {wait:?}",
                    path.display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_holder_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("gh/land-x.lock");
        let a = Lock::acquire(&p, Duration::ZERO).await.unwrap();
        #[cfg(unix)]
        assert!(Lock::acquire(&p, Duration::from_millis(600)).await.is_err());
        drop(a);
        assert!(Lock::acquire(&p, Duration::ZERO).await.is_ok());
    }
}
