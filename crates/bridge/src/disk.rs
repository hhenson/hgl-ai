//! Free space of `work_root` (ROLE-18).

use std::path::Path;

pub const GB: u64 = 1_000_000_000;

pub trait DiskProbe: Send + Sync {
    /// Bytes available to this user on the filesystem holding `path`.
    fn free_bytes(&self, path: &Path) -> std::io::Result<u64>;
}

/// `statvfs(3)`.
pub struct Statvfs;

impl DiskProbe for Statvfs {
    #[cfg(unix)]
    fn free_bytes(&self, path: &Path) -> std::io::Result<u64> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        // SAFETY: `c` is a valid NUL-terminated path and `st` a properly sized out-parameter.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        #[allow(clippy::unnecessary_cast)]
        Ok(st.f_bavail as u64 * st.f_frsize as u64)
    }

    #[cfg(not(unix))]
    fn free_bytes(&self, _: &Path) -> std::io::Result<u64> {
        Err(std::io::Error::other("free space is measured on Unix only"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statvfs_reports_something() {
        let d = tempfile::tempdir().unwrap();
        assert!(Statvfs.free_bytes(d.path()).unwrap() > 0);
        assert!(Statvfs.free_bytes(Path::new("/nonexistent/x")).is_err());
    }
}
