use std::path::{Path, PathBuf};

pub fn tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new().prefix("hbs").tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap();
    (dir, path)
}

pub fn fits(dir: &Path, label: &str) -> bool {
    // SAFETY: zero is valid for sockaddr_un; only the array length is read.
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    dir.join(format!("{label}.sock")).as_os_str().len() <= addr.sun_path.len() - 18
}
