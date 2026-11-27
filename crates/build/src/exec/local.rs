//! This host: steps run as `sh -c RUN` under `hgl-ai cap` (memory and time), checkouts
//! under `<work_root>/builder/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hgl_ai_cap::{CapCommand, CapSpec};
use hgl_ai_proto::build::Platform;
use hgl_ai_tree::Git;

use super::{
    ARTIFACT_FILES, ARTIFACT_LIMIT, Cancel, Checkout, Executor, HostError, HostOps, Out, Prep,
    Probe, StepRun, StepSpec, Workspace, checkout, expand_vars, remove_job, wildcard,
};

pub struct Local {
    platform: Platform,
    /// `<work_root>/builder`: clones and job directories.
    root: PathBuf,
    sdk: BTreeMap<String, String>,
    git: Git,
}

impl Local {
    pub fn new(root: PathBuf, sdk: BTreeMap<String, String>, git: Git) -> Local {
        Local {
            platform: Platform::current().unwrap_or(Platform::Linux),
            root,
            sdk,
            git,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl HostOps for Local {
    fn lock_clone(&self, clone: &str) -> Result<super::CloneGuard, HostError> {
        // A file beside the clone: across processes (a builder and `build shell`).
        let l = crate::lock::FileLock::lock(Path::new(&format!("{clone}.lock")))
            .map_err(|e| HostError::Failed(format!("locking {clone}: {e}")))?;
        Ok(Box::new(l))
    }

    fn git(&self, cwd: &str, args: &[&str]) -> Result<Out, HostError> {
        let o = self
            .git
            .output(Path::new(cwd), args)
            .map_err(|e| HostError::Failed(e.to_string()))?;
        Ok(Out {
            ok: o.ok,
            stdout: o.stdout,
            stderr: o.stderr,
        })
    }
    fn exists(&self, path: &str) -> Result<bool, HostError> {
        Ok(std::fs::symlink_metadata(path).is_ok())
    }
    fn mkdirs(&self, path: &str) -> Result<(), HostError> {
        std::fs::create_dir_all(path).map_err(|e| HostError::Failed(format!("{path}: {e}")))
    }
    fn remove(&self, path: &str) -> Result<(), HostError> {
        match std::fs::symlink_metadata(path) {
            Err(_) => Ok(()),
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(path)
                .map_err(|e| HostError::Failed(format!("removing {path}: {e}"))),
            Ok(_) => std::fs::remove_file(path)
                .map_err(|e| HostError::Failed(format!("removing {path}: {e}"))),
        }
    }
}

/// User and system time of reaped children so far.
fn children_cpu() -> Duration {
    // SAFETY: `ru` is a valid rusage to write into.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) } != 0 {
        return Duration::ZERO;
    }
    let tv = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec.max(0) as u64) + Duration::from_micros(t.tv_usec.max(0) as u64)
    };
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// Run `cmd` under `spec` with stdout and stderr into `log`. The shared runner of local
/// steps and of `build shell`'s probes.
pub fn run_capped(cmd: &Command, spec: CapSpec, log: &Path, cancel: &Cancel) -> StepRun {
    let started = Instant::now();
    let mut r = StepRun {
        cap: Some(spec.mem_bytes),
        ..StepRun::default()
    };
    let file = match std::fs::File::create(log) {
        Ok(f) => f,
        Err(e) => {
            r.error = Some(format!("{}: {e}", log.display()));
            return r;
        }
    };
    let err = match file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            r.error = Some(format!("{}: {e}", log.display()));
            return r;
        }
    };
    let before = children_cpu();
    let child = CapCommand::new(spec, cmd)
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(err)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => {
            r.error = Some(format!("starting the step: {e}"));
            return r;
        }
    };
    let stopper = child.stopper();
    let _hook = cancel.hook(move || stopper.stop());
    match child.wait() {
        Ok(o) => {
            r.exit = Some(o.status);
            r.peak = o.peak_bytes;
            r.stopped = o.stopped;
            r.limit = if o.memory_limit {
                Some("memory")
            } else if o.time_limit {
                Some("time")
            } else {
                None
            };
            if !o.started {
                r.error = Some(o.report(&spec).join("; "));
            }
        }
        Err(e) => r.error = Some(format!("waiting for the step: {e}")),
    }
    r.cpu = Some(children_cpu().saturating_sub(before));
    r.wall = started.elapsed();
    r
}

impl Executor for Local {
    fn kind(&self) -> &'static str {
        "local"
    }

    fn platform(&self) -> Platform {
        self.platform
    }

    fn sdk(&self, name: &str) -> Option<String> {
        self.sdk.get(name).map(|p| {
            hgl_ai_config::expand(p)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| p.clone())
        })
    }

    fn has_tool(&self, tool: &str) -> Result<bool, HostError> {
        Ok(find_on_path(tool).is_some())
    }

    fn prepare(&self, c: &Checkout) -> Result<Workspace, Prep> {
        checkout(self, &self.root.display().to_string(), c)
    }

    fn run_step(&self, ws: &Workspace, step: &StepSpec, log: &Path, cancel: &Cancel) -> StepRun {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(&step.script)
            .current_dir(Path::new(&ws.src).join(&step.cwd));
        for (k, v) in &step.env {
            cmd.env(k, v);
        }
        let spec = CapSpec::new(step.mem_bytes)
            .with_time(step.time)
            .with_address_limit(step.address_limit);
        run_capped(&cmd, spec, log, cancel)
    }

    fn collect(
        &self,
        ws: &Workspace,
        cwd: &str,
        patterns: &[String],
        env: &[(String, String)],
        dest: &Path,
        cancel: &Cancel,
    ) -> Vec<PathBuf> {
        let base = Path::new(&ws.src).join(cwd);
        let mut out = Vec::new();
        let mut total = 0u64;
        for p in patterns {
            let p = expand_vars(p, env);
            let full = if Path::new(&p).is_absolute() {
                PathBuf::from(&p)
            } else {
                base.join(&p)
            };
            let (dir, pat) = match (full.parent(), full.file_name()) {
                (Some(d), Some(f)) => (d.to_path_buf(), f.to_string_lossy().into_owned()),
                _ => continue,
            };
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut names: Vec<_> = rd
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| wildcard(&pat, n))
                .collect();
            names.sort();
            for n in names {
                if out.len() >= ARTIFACT_FILES || cancel.is_cancelled() {
                    return out;
                }
                let from = dir.join(&n);
                let len = std::fs::metadata(&from).map(|m| m.len()).unwrap_or(0);
                if total + len > ARTIFACT_LIMIT {
                    continue;
                }
                if std::fs::create_dir_all(dest).is_err() {
                    return out;
                }
                let to = dest.join(&n);
                if std::fs::copy(&from, &to).is_ok() {
                    total += len;
                    out.push(to);
                }
            }
        }
        out
    }

    fn cleanup(&self, ws: &Workspace) -> Result<(), HostError> {
        remove_job(self, ws)
    }

    fn clean_all(&self) -> Result<(), HostError> {
        let jobs = self.root.join("jobs");
        let Ok(rd) = std::fs::read_dir(&jobs) else {
            return Ok(());
        };
        for e in rd.flatten() {
            let ws = Workspace {
                src: e.path().join("src").display().to_string(),
                build: String::new(),
                tmp: String::new(),
                job: Some(e.path().display().to_string()),
                clone: None,
            };
            self.remove(ws.job.as_deref().unwrap_or_default())?;
        }
        // Worktrees of removed job directories.
        if let Ok(rd) = std::fs::read_dir(self.root.join("clones")) {
            for e in rd.flatten() {
                let _ = self.git(&e.path().display().to_string(), &["worktree", "prune"]);
            }
        }
        Ok(())
    }

    fn probe(&self, tools: &[String]) -> Result<Probe, HostError> {
        let mut p = Probe {
            free_gb: free_gb(&self.root),
            ..Probe::default()
        };
        for t in tools {
            if let Some(v) = version(t) {
                p.toolchains.insert(t.clone(), v);
            }
        }
        Ok(p)
    }

    fn shell(&self, ws: &Workspace, env: &[(String, String)]) -> std::io::Result<i32> {
        let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut c = Command::new(sh);
        c.arg("-i").current_dir(&ws.src);
        for (k, v) in env {
            c.env(k, v);
        }
        Ok(c.status()?.code().unwrap_or(1))
    }
}

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// The first line of `tool --version`.
fn version(tool: &str) -> Option<String> {
    find_on_path(tool)?;
    let o = Command::new(tool)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let text = if o.stdout.is_empty() {
        o.stderr
    } else {
        o.stdout
    };
    String::from_utf8_lossy(&text)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(120).collect())
}

/// Free space of the file system holding `p` (or its nearest existing parent), in GiB.
pub fn free_gb(p: &Path) -> Option<u64> {
    let mut p = p.to_path_buf();
    while !p.exists() {
        p = p.parent()?.to_path_buf();
    }
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: `c` is a valid C string and `s` a valid statvfs to write into.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    let bytes = (s.f_bavail as u64).saturating_mul(s.f_frsize as u64);
    Some(bytes >> 30)
}
