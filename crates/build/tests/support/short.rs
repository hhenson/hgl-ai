use std::os::fd::AsRawFd;
use std::path::PathBuf;

// A short absolute alias to a directory in the test's scratch tree. The open
// directory is inherited by ssh, so /dev/fd resolves in both processes.
pub struct ShortDir {
    _file: std::fs::File,
    _dir: tempfile::TempDir,
}

pub fn tempdir() -> (ShortDir, PathBuf) {
    let dir = tempfile::Builder::new().prefix("s").tempdir().unwrap();
    let file = std::fs::File::open(dir.path()).unwrap();
    let fd = file.as_raw_fd();
    // SAFETY: fd is live and owned by file. Preserve its flags except CLOEXEC.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
        0
    );
    let path = PathBuf::from(format!("/dev/fd/{fd}"));
    (
        ShortDir {
            _file: file,
            _dir: dir,
        },
        path,
    )
}
