//! Paths under `work_root` (docs/design.md §5) and the rule that nothing escapes it.

use std::path::{Component, Path, PathBuf};

use hgl_ai_proto::{Seat, TaskKey};

use crate::{Result, TreeError};

/// Top-level directories of `work_root` that are not seats.
pub const CLONES: &str = "clones";
pub const SALVAGE: &str = "salvage";
/// The directory of a seat's turns that belong to no task.
pub const INBOX: &str = "_inbox";

/// The directory layout of one host's `work_root`.
#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// `root` must be absolute; it is created if missing and resolved through symlinks.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(TreeError::Invalid(format!(
                "work_root {} is not absolute",
                root.display()
            )));
        }
        std::fs::create_dir_all(&root).map_err(|e| TreeError::io(&root, e))?;
        let root = root.canonicalize().map_err(|e| TreeError::io(&root, e))?;
        Ok(Layout { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<work_root>/<seat>`; seats named like a top-level directory are refused.
    pub fn seat_dir(&self, seat: &Seat) -> Result<PathBuf> {
        if [CLONES, SALVAGE].contains(&seat.as_str()) {
            return Err(TreeError::Invalid(format!(
                "seat name {seat} is reserved under work_root"
            )));
        }
        self.confine(&self.root.join(seat.as_str()))
    }

    /// `<work_root>/<seat>/<repo>-<issue>` or `.../<repo>-<pr>-review`.
    pub fn task_dir(&self, seat: &Seat, task: &TaskKey, review: bool) -> Result<PathBuf> {
        check_repo(&task.repo)?;
        if task.issue == 0 {
            return Err(TreeError::Invalid("issue number 0".into()));
        }
        self.confine(&self.seat_dir(seat)?.join(task_dir_name(task, review)))
    }

    /// `<work_root>/<seat>/_inbox`.
    pub fn inbox_dir(&self, seat: &Seat) -> Result<PathBuf> {
        self.confine(&self.seat_dir(seat)?.join(INBOX))
    }

    /// `<work_root>/clones/<repo>`.
    pub fn clone_dir(&self, repo: &str) -> Result<PathBuf> {
        check_repo(repo)?;
        self.confine(&self.root.join(CLONES).join(repo))
    }

    /// `<work_root>/salvage`.
    pub fn salvage_dir(&self) -> PathBuf {
        self.root.join(SALVAGE)
    }

    /// Lexically normalise `p` and require it to lie strictly inside `work_root`.
    pub fn confine(&self, p: &Path) -> Result<PathBuf> {
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        let norm = normalize(&joined);
        if norm != self.root && norm.starts_with(&self.root) {
            Ok(norm)
        } else {
            Err(TreeError::Escape(p.to_path_buf()))
        }
    }

    /// Like [`Layout::confine`], and also resolve symlinks of an existing path, so a
    /// link inside `work_root` that points elsewhere is refused.
    pub fn confine_existing(&self, p: &Path) -> Result<PathBuf> {
        let p = self.confine(p)?;
        let real = p.canonicalize().map_err(|e| TreeError::io(&p, e))?;
        if real != self.root && real.starts_with(&self.root) {
            Ok(real)
        } else {
            Err(TreeError::Escape(p))
        }
    }
}

/// `<repo>-<issue>` with `-review` for a reviewer's tree.
pub fn task_dir_name(task: &TaskKey, review: bool) -> String {
    let suffix = if review { "-review" } else { "" };
    format!("{}-{}{suffix}", task.repo, task.issue)
}

/// Parse a task directory name back into its task and review flag.
pub fn parse_task_dir_name(name: &str) -> Option<(TaskKey, bool)> {
    let (rest, review) = match name.strip_suffix("-review") {
        Some(r) => (r, true),
        None => (name, false),
    };
    let (repo, issue) = rest.rsplit_once('-')?;
    let issue: u64 = issue.parse().ok().filter(|n| *n > 0)?;
    check_repo(repo).ok()?;
    Some((
        TaskKey {
            repo: repo.to_string(),
            issue,
        },
        review,
    ))
}

/// A GitHub repository name: `[A-Za-z0-9._-]`, at most 100, with no leading, trailing or
/// doubled dot. Names go into bucket keys whose dot-separated parts must not be empty
/// (`HGL_TREES`, `HGL_ENDED`), so `.github` is refused here rather than at registration.
pub fn check_repo(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(TreeError::Invalid(format!("repository name {name:?}")))
    }
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(s: &str) -> Seat {
        Seat::new(s).unwrap()
    }

    #[test]
    fn paths_follow_the_layout() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout::new(d.path()).unwrap();
        let t = TaskKey {
            repo: "hgl-compiler".into(),
            issue: 980,
        };
        let root = l.root().to_path_buf();
        assert_eq!(
            l.task_dir(&seat("w1"), &t, false).unwrap(),
            root.join("w1/hgl-compiler-980")
        );
        assert_eq!(
            l.task_dir(&seat("r1"), &t, true).unwrap(),
            root.join("r1/hgl-compiler-980-review")
        );
        assert_eq!(
            l.clone_dir("hgl_spec").unwrap(),
            root.join("clones/hgl_spec")
        );
        assert_eq!(l.inbox_dir(&seat("w1")).unwrap(), root.join("w1/_inbox"));
        assert_eq!(
            parse_task_dir_name("hgl-compiler-980-review"),
            Some((t.clone(), true))
        );
        assert_eq!(parse_task_dir_name("hgl-compiler-980"), Some((t, false)));
        assert_eq!(parse_task_dir_name("_inbox"), None);
        assert!(check_repo("hgl.github.io").is_ok());
    }

    #[test]
    fn nothing_escapes_work_root() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout::new(d.path().join("work")).unwrap();
        for bad in ["..", ".", "a/b", "../x", "x y", "", ".github", "x.", "a..b"] {
            let t = TaskKey {
                repo: bad.into(),
                issue: 1,
            };
            assert!(l.task_dir(&seat("w1"), &t, false).is_err(), "{bad:?}");
            assert!(l.clone_dir(bad).is_err(), "{bad:?}");
        }
        assert!(l.confine(Path::new("../outside")).is_err());
        assert!(l.confine(&l.root().join("w1/../../outside")).is_err());
        assert!(
            l.confine(l.root()).is_err(),
            "work_root itself is not a task path"
        );
        assert!(l.confine(Path::new("/etc")).is_err());
        assert!(l.confine(Path::new("w1/x/../y")).is_ok());
        assert!(l.seat_dir(&seat("clones")).is_err());
        assert!(l.seat_dir(&seat("salvage")).is_err());

        // A symlink inside work_root that points outside is refused once resolved.
        let outside = d.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(l.root().join("w1")).unwrap();
        let link = l.root().join("w1/repo-1");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(l.confine(&link).is_ok());
        assert!(matches!(
            l.confine_existing(&link),
            Err(TreeError::Escape(_))
        ));
    }
}
