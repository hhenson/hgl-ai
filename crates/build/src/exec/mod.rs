//! Executors: where a build's checkout lives and its steps run. [`local::Local`] runs on
//! this host under `hgl-ai cap`; [`ssh::Ssh`] drives a remote over one owned ssh
//! connection (a proxy builder, docs/build.md §2a). Both check out with the same git
//! sequence ([`checkout`]) and leave nothing behind ([`Executor::cleanup`]).
//!
//! Everything here blocks; async callers use `spawn_blocking`.

pub mod local;
pub mod ssh;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hgl_ai_proto::build::Platform;

/// A request to stop: hooks run once, at [`Cancel::cancel`] or at once when added after it.
#[derive(Clone, Default)]
pub struct Cancel(Arc<Mutex<CancelState>>);

#[derive(Default)]
struct CancelState {
    cancelled: bool,
    next: u64,
    hooks: Vec<(u64, Box<dyn Fn() + Send>)>,
}

/// Removes its hook when dropped.
pub struct Hook {
    cancel: Cancel,
    id: u64,
}

impl Drop for Hook {
    fn drop(&mut self) {
        let mut s = self.cancel.0.lock().unwrap_or_else(|e| e.into_inner());
        s.hooks.retain(|(i, _)| *i != self.id);
    }
}

impl Cancel {
    pub fn cancel(&self) {
        let hooks = {
            let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if s.cancelled {
                return;
            }
            s.cancelled = true;
            std::mem::take(&mut s.hooks)
        };
        for (_, h) in hooks {
            h();
        }
    }

    /// The same request (a clone of it).
    pub fn same(&self, other: &Cancel) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).cancelled
    }

    pub fn hook(&self, f: impl Fn() + Send + 'static) -> Hook {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let id = s.next;
        s.next += 1;
        if s.cancelled {
            drop(s);
            f();
        } else {
            s.hooks.push((id, Box::new(f)));
        }
        Hook {
            cancel: self.clone(),
            id,
        }
    }
}

/// What to check out.
#[derive(Debug, Clone)]
pub struct Checkout {
    pub id: String,
    pub repo: String,
    pub url: String,
    pub head: String,
    pub base: Option<String>,
    pub issue: u64,
    /// A `build shell` checkout: under `shells/`, which a builder's startup cleanup of
    /// `jobs/` leaves alone.
    pub shell: bool,
}

/// Where a build runs: paths on the executor's host (`/`-separated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub src: String,
    pub build: String,
    pub tmp: String,
    /// The job directory [`Executor::cleanup`] removes; `None` for a tree that is not ours.
    pub job: Option<String>,
    /// The clone the checkout is a worktree of.
    pub clone: Option<String>,
    /// The commit the checkout merged `head` onto, where it had a base.
    pub base_sha: Option<String>,
}

/// Why a checkout did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prep {
    /// A prerequisite is missing (the head, a merge): the build is not run.
    NotRun(String),
    /// The remote did not answer.
    Unreachable(String),
}

/// One step to run.
#[derive(Debug, Clone)]
pub struct StepSpec {
    pub name: String,
    pub script: String,
    /// Relative to the checkout.
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub mem_bytes: u64,
    pub time: Duration,
    /// A per-process address-space limit where the cap uses one (off for sanitizers).
    pub address_limit: bool,
}

/// How a step ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepRun {
    pub exit: Option<i32>,
    pub wall: Duration,
    pub cpu: Option<Duration>,
    pub peak: Option<u64>,
    pub cap: Option<u64>,
    /// `memory` or `time`.
    pub limit: Option<&'static str>,
    pub stopped: bool,
    /// The connection dropped during the step: not a build failure.
    pub unreachable: bool,
    /// It could not start.
    pub error: Option<String>,
}

/// Health of an executor's host, for its builder record.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub free_gb: Option<u64>,
    pub toolchains: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    Unreachable(String),
    Failed(String),
}

impl From<HostError> for Prep {
    fn from(e: HostError) -> Prep {
        match e {
            HostError::Unreachable(r) => Prep::Unreachable(r),
            HostError::Failed(r) => Prep::NotRun(r),
        }
    }
}

pub trait Executor: Send + Sync {
    /// `local` or `ssh`.
    fn kind(&self) -> &'static str;
    fn platform(&self) -> Platform;
    /// The prefix of SDK `name` on the executor's host, if configured.
    fn sdk(&self, name: &str) -> Option<String>;
    /// Every SDK configured on the executor's host, by name: steps see them all
    /// (`HGL_SDK_<NAME>`); `needs` only requires one.
    fn sdk_names(&self) -> Vec<String>;
    fn has_tool(&self, tool: &str) -> Result<bool, HostError>;
    /// Check out `c` in a job directory of its own.
    fn prepare(&self, c: &Checkout) -> Result<Workspace, Prep>;
    /// Run one step, its stdout and stderr together in `log` (a local file).
    fn run_step(&self, ws: &Workspace, step: &StepSpec, log: &Path, cancel: &Cancel) -> StepRun;
    /// Copy the files matching `patterns` (relative to the step's `cwd`; `$VAR` from
    /// `env`) into `dest`, stopping at `cancel`; returns the copies.
    fn collect(
        &self,
        ws: &Workspace,
        cwd: &str,
        patterns: &[String],
        env: &[(String, String)],
        dest: &Path,
        cancel: &Cancel,
    ) -> Vec<PathBuf>;
    /// Remove the job directory and its worktree. Always called after `prepare` succeeded.
    fn cleanup(&self, ws: &Workspace) -> Result<(), HostError>;
    /// Remove every job directory left by an earlier run of this builder.
    fn clean_all(&self) -> Result<(), HostError>;
    /// Existing Cargo slot directory names in the executor's platform directory.
    fn cargo_cache_slots(&self) -> Result<Vec<String>, HostError>;
    /// Absolute directory for a slot's Cargo intermediates on this executor's host.
    fn cargo_cache_path(&self, slot: &str) -> String;
    /// Size of a slot in bytes, or zero when Cargo did not create it.
    fn cargo_cache_size(&self, path: &str) -> Result<u64, HostError>;
    fn remove_cargo_cache(&self, path: &str) -> Result<(), HostError>;
    fn probe(&self, tools: &[String]) -> Result<Probe, HostError>;
    /// An interactive shell in the checkout, with the build's environment.
    fn shell(&self, ws: &Workspace, env: &[(String, String)]) -> std::io::Result<i32>;
}

/// What a finished command printed.
#[derive(Debug, Clone, Default)]
pub struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Held while git works on a shared clone; released when dropped.
pub(crate) type CloneGuard = Box<dyn std::any::Any + Send>;

/// File and git operations on an executor's host, for [`checkout`] and [`remove_job`].
pub(crate) trait HostOps {
    /// Exclusive use of the shared clone `clone` (its config, refs and worktrees) for a
    /// checkout or a removal.
    fn lock_clone(&self, clone: &str) -> Result<CloneGuard, HostError>;
    fn git(&self, cwd: &str, args: &[&str]) -> Result<Out, HostError>;
    fn exists(&self, path: &str) -> Result<bool, HostError>;
    fn mkdirs(&self, path: &str) -> Result<(), HostError>;
    fn remove(&self, path: &str) -> Result<(), HostError>;
}

const MERGE_IDENTITY: [&str; 6] = [
    "-c",
    "user.name=hgl-ai builder",
    "-c",
    "user.email=builder@hgl-ai.invalid",
    "-c",
    "commit.gpgsign=false",
];

fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no message")
}

/// The job directory of build `id` under `root`.
pub fn job_dir(root: &str, id: &str) -> String {
    format!("{root}/jobs/{id}")
}

/// Check out `c` under `root`: a bare clone per repository (`clones/<repo>`, fetched),
/// and `jobs/<id>/src` a detached worktree at `head`, or at `base` with `head` merged.
pub(crate) fn checkout(h: &dyn HostOps, root: &str, c: &Checkout) -> Result<Workspace, Prep> {
    let clone = format!("{root}/clones/{}", c.repo);
    let job = if c.shell {
        format!("{root}/shells/{}", c.id)
    } else {
        job_dir(root, &c.id)
    };
    let src = format!("{job}/src");
    // One checkout or removal at a time on the shared clone: git locks its config and
    // refs and fails a second writer at once.
    let _guard = h.lock_clone(&clone)?;
    if !h.exists(&format!("{clone}/HEAD"))? {
        h.mkdirs(&clone)?;
        let o = h.git(&clone, &["init", "--quiet", "--bare"])?;
        if !o.ok {
            return Err(Prep::NotRun(format!("git init: {}", first_line(&o.stderr))));
        }
    }
    let origin = h.git(&clone, &["remote", "get-url", "origin"])?;
    let set = if origin.ok && origin.stdout.trim() == c.url {
        Out {
            ok: true,
            ..Out::default()
        }
    } else if origin.ok {
        h.git(&clone, &["remote", "set-url", "origin", &c.url])?
    } else {
        h.git(&clone, &["remote", "add", "origin", &c.url])?
    };
    if !set.ok {
        return Err(Prep::NotRun(format!(
            "git remote: {}",
            first_line(&set.stderr)
        )));
    }
    let f = h.git(&clone, &["fetch", "--quiet", "origin"])?;
    if !f.ok {
        return Err(Prep::NotRun(format!(
            "fetching {}: {}",
            c.repo,
            first_line(&f.stderr)
        )));
    }
    let has = |rev: &str| -> Result<bool, HostError> {
        Ok(
            h.git(&clone, &["cat-file", "-e", &format!("{rev}^{{commit}}")])?
                .ok,
        )
    };
    if !has(&c.head)? && c.issue > 0 {
        let r = format!("+refs/pull/{0}/head:refs/remotes/origin/pr/{0}", c.issue);
        h.git(&clone, &["fetch", "--quiet", "origin", &r])?;
    }
    if !has(&c.head)? {
        h.git(&clone, &["fetch", "--quiet", "origin", &c.head])?;
    }
    if !has(&c.head)? {
        return Err(Prep::NotRun(format!(
            "head {} is not on origin; push it first",
            &c.head[..c.head.len().min(12)]
        )));
    }
    let start = match &c.base {
        None => c.head.clone(),
        Some(b) => {
            let remote = format!("refs/remotes/origin/{b}");
            let o = h.git(
                &clone,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{remote}^{{commit}}"),
                ],
            )?;
            if o.ok {
                o.stdout.trim().to_string()
            } else if has(b)? {
                b.clone()
            } else {
                return Err(Prep::NotRun(format!(
                    "base {b:?} is neither a branch of origin nor a commit"
                )));
            }
        }
    };
    h.remove(&job)?;
    h.mkdirs(&format!("{job}/build"))?;
    h.mkdirs(&format!("{job}/tmp"))?;
    let _ = h.git(&clone, &["worktree", "prune"])?;
    let ws = Workspace {
        src: src.clone(),
        build: format!("{job}/build"),
        tmp: format!("{job}/tmp"),
        job: Some(job.clone()),
        clone: Some(clone.clone()),
        base_sha: c.base.as_ref().map(|_| start.clone()),
    };
    let add = h.git(
        &clone,
        &["worktree", "add", "--quiet", "--detach", &src, &start],
    )?;
    if !add.ok {
        let _ = remove_job_locked(h, &ws);
        return Err(Prep::NotRun(format!(
            "git worktree add: {}",
            first_line(&add.stderr)
        )));
    }
    if c.base.is_some() {
        let msg = format!(
            "Build merge of {} onto {}",
            &c.head[..12],
            &start[..start.len().min(12)]
        );
        let mut args: Vec<&str> = MERGE_IDENTITY.to_vec();
        args.extend(["merge", "--quiet", "--no-edit", "-m", &msg, &c.head]);
        let m = h.git(&src, &args)?;
        if !m.ok {
            let _ = h.git(&src, &["merge", "--abort"]);
            let _ = remove_job_locked(h, &ws);
            return Err(Prep::NotRun(format!(
                "head {} does not merge onto {}",
                &c.head[..12],
                c.base.as_deref().unwrap_or("base")
            )));
        }
    }
    if h.exists(&format!("{src}/.gitmodules"))? {
        let s = h.git(
            &src,
            &["submodule", "update", "--init", "--recursive", "--quiet"],
        )?;
        if !s.ok {
            let _ = remove_job_locked(h, &ws);
            return Err(Prep::NotRun(format!(
                "submodules: {}",
                first_line(&s.stderr)
            )));
        }
    }
    Ok(ws)
}

/// Remove a job's worktree and directory.
pub(crate) fn remove_job(h: &dyn HostOps, ws: &Workspace) -> Result<(), HostError> {
    let _guard = match &ws.clone {
        Some(c) => Some(h.lock_clone(c)?),
        None => None,
    };
    remove_job_locked(h, ws)
}

/// [`remove_job`] with the clone already locked (inside [`checkout`]).
fn remove_job_locked(h: &dyn HostOps, ws: &Workspace) -> Result<(), HostError> {
    let Some(job) = &ws.job else { return Ok(()) };
    if let Some(clone) = &ws.clone
        && h.exists(&format!("{clone}/HEAD"))?
    {
        let _ = h.git(clone, &["worktree", "remove", "--force", &ws.src])?;
    }
    h.remove(job)?;
    if let Some(clone) = &ws.clone
        && h.exists(&format!("{clone}/HEAD"))?
    {
        let _ = h.git(clone, &["worktree", "prune"])?;
    }
    Ok(())
}

/// Expand `$VAR` and `${VAR}` from `env` (others are left as written).
pub fn expand_vars(s: &str, env: &[(String, String)]) -> String {
    let get = |k: &str| {
        env.iter()
            .rev()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
    };
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let (name, used) = if let Some(b) = after.strip_prefix('{') {
            match b.find('}') {
                Some(e) => (&b[..e], e + 2),
                None => ("", 0),
            }
        } else {
            let e = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after.len());
            (&after[..e], e)
        };
        match (name.is_empty(), get(name)) {
            (false, Some(v)) => {
                out.push_str(&v);
                rest = &after[used..];
            }
            _ => {
                out.push('$');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `*` and `?` within one path segment.
pub fn wildcard(pattern: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

/// Bound on the artifacts kept of one step.
pub const ARTIFACT_LIMIT: u64 = 256 << 20;
pub const ARTIFACT_FILES: usize = 100;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vars_and_wildcards() {
        let env = vec![("B".to_string(), "/b".to_string())];
        assert_eq!(expand_vars("$B/x.xml", &env), "/b/x.xml");
        assert_eq!(expand_vars("${B}x/$NONE", &env), "/bx/$NONE");
        assert_eq!(expand_vars("a$", &env), "a$");
        assert!(wildcard("*.xml", "ctest.xml"));
        assert!(wildcard("t?st-*.xml", "test-1.xml"));
        assert!(!wildcard("*.xml", "ctest.xml.bak"));
    }

    #[test]
    fn cancel_hooks() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let c = Cancel::default();
        let n = Arc::new(AtomicU32::new(0));
        let n2 = n.clone();
        let h = c.hook(move || {
            n2.fetch_add(1, Ordering::SeqCst);
        });
        let n3 = n.clone();
        let gone = c.hook(move || {
            n3.fetch_add(10, Ordering::SeqCst);
        });
        drop(gone);
        c.cancel();
        c.cancel();
        assert_eq!(n.load(Ordering::SeqCst), 1);
        let n4 = n.clone();
        let _late = c.hook(move || {
            n4.fetch_add(100, Ordering::SeqCst);
        });
        assert_eq!(n.load(Ordering::SeqCst), 101);
        drop(h);
    }
}
