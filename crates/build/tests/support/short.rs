use std::path::{Path, PathBuf};

pub fn relative(path: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
    let path = path.canonicalize().unwrap();
    let a: Vec<_> = cwd.components().collect();
    let b: Vec<_> = path.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut result = PathBuf::new();
    for _ in common..a.len() {
        result.push("..");
    }
    for part in &b[common..] {
        result.push(part.as_os_str());
    }
    result
}

pub fn tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new().prefix("s").tempdir().unwrap();
    let path = relative(dir.path());
    assert!(
        path.join("data/builder/ssh/win1.sock").as_os_str().len() <= 86,
        "test socket path is too long"
    );
    (dir, path)
}
