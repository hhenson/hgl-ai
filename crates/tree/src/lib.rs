//! Worktree layout and lifecycle owned by the bridge (docs/protocol.md §5,
//! docs/design.md §5): one clone per repository and host, one git worktree per task,
//! registration in `HGL_TREES`, salvage and removal, stale listing.
//!
//! Git work is synchronous (it shells out to `git`); callers on an async runtime run it
//! on a blocking thread. Registration is async, through [`Registry`].

pub mod git;
pub mod layout;
pub mod slug;
pub mod stale;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hgl_ai_config::{Config, Github};
use hgl_ai_proto::{Seat, TaskKey, TreeRecord};
use serde::{Deserialize, Serialize};

pub use git::Git;
pub use layout::Layout;
pub use stale::{IsClosed, Stale};

#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error("path {0} escapes work_root")]
    Escape(PathBuf),
    #[error("{0}")]
    Invalid(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("git {args} failed ({code:?}): {stderr}")]
    Git {
        args: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("a tree already exists at {0}")]
    Exists(PathBuf),
    #[error("commit {0} is not in the clone after fetching")]
    MissingCommit(String),
    #[error(transparent)]
    Config(#[from] hgl_ai_config::ConfigError),
}

impl TreeError {
    pub fn io(path: &Path, source: std::io::Error) -> Self {
        TreeError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, TreeError>;

pub const TASK_FILE: &str = "task.json";
/// Identity of the bridge's own merge commit in a review tree.
const MERGE_IDENTITY: [&str; 6] = [
    "-c",
    "user.name=hgl-ai bridge",
    "-c",
    "user.email=bridge@hgl-ai.invalid",
    "-c",
    "commit.gpgsign=false",
];

/// `task.json`: what the bridge knows about one task directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFile {
    pub seat: Seat,
    pub repo: String,
    pub issue: u64,
    #[serde(default)]
    pub review: bool,
    /// The reviewed head (review trees).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The implementor's branch; `None` for a detached review tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The ref the tree started from (`origin/main` unless the note named a base).
    pub base: String,
    /// The commit `base` named at creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,
    /// `HEAD` right after creation; commits from here on are the task's own.
    pub start: String,
    pub created_at: String,
    /// Review trees: files that did not merge; empty when `head` merged onto `main`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    /// Agent session ids by runtime, from the task's previous turns.
    #[serde(default)]
    pub sessions: BTreeMap<String, String>,
    /// The stream sequence of the note whose turn made the tree (PROTO-14): an ending
    /// note with a lower one is older than the tree and leaves it alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Model families whose turns ran in this tree (implementor trees): the task's authors,
    /// which a review's family must differ from (ROLE-11).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub families: Vec<hgl_ai_proto::Family>,
    /// The task's class (docs/roles.md §3): from its `assign`, moved up by a `sendback`
    /// that carries a higher one (ROLE-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<hgl_ai_proto::TaskClass>,
    /// The stream sequence of the note that last set `class`: a class note with a lower one
    /// (an older note delivered again) changes nothing (PROTO-21).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_seq: Option<u64>,
    /// The stream sequence of the newest `assign` or `sendback` a turn in this tree ran
    /// (PROTO-21): verdicts go to the task's tree with the newest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_seq: Option<u64>,
    /// Implementor trees: the commit the tree's index and files were last brought to, at
    /// its creation, the end of a turn, or a refresh (hgl-ai#55). Another tree of this host
    /// sharing the branch can move the ref under them; [`Trees::refresh_branch`] catches up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<String>,
}

impl TaskFile {
    pub fn task(&self) -> TaskKey {
        TaskKey {
            repo: self.repo.clone(),
            issue: self.issue,
        }
    }
}

/// What to create.
#[derive(Debug, Clone)]
pub enum Create {
    /// An implementor tree from `base` (default `main`). A pushed or local
    /// `<prefix>q<issue>-*` branch is resumed whatever its slug; a new one is
    /// `<prefix>q<issue>-<slug>` of the issue's title ([`Trees::create_for`]), else of
    /// `title`, the note's first line ([`slug::note_slug`]).
    Implement { title: String, base: Option<String> },
    /// A reviewer tree: `head` merged onto `base` (default `main`), detached.
    Review { head: String, base: Option<String> },
}

/// What names a new implementor branch: the issue's title, asked for only then, else the
/// note's first line.
struct Naming<'a> {
    note_line: &'a str,
    issue_title: &'a dyn Fn() -> Option<String>,
}

/// A tree that exists after [`Trees::create`].
#[derive(Debug, Clone)]
pub struct Created {
    pub dir: PathBuf,
    pub file: TaskFile,
    pub record: TreeRecord,
    /// `true` when the tree already existed and was kept.
    pub reused: bool,
}

/// What [`Trees::remove`] did, for the cleanup receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub key: String,
    pub dir: PathBuf,
    pub branch: Option<String>,
    pub freed_bytes: u64,
    /// Patch of uncommitted changes and unpushed commits, when there were any.
    pub salvage: Option<PathBuf>,
}

/// A task directory found on disk.
#[derive(Debug, Clone)]
pub struct LocalTree {
    pub dir: PathBuf,
    pub record: TreeRecord,
    /// `None` when `task.json` is missing or unreadable (a partial tree).
    pub file: Option<TaskFile>,
}

/// `HGL_TREES` access (PROTO-13); implemented for the hub connection.
pub trait Registry: Send + Sync {
    fn tree_put(&self, rec: &TreeRecord) -> impl Future<Output = hgl_ai_bus::Result<()>> + Send;
    /// Put `rec` merged with the record held, never losing a fresher one (hgl-ai#51).
    fn tree_merge(
        &self,
        rec: &TreeRecord,
    ) -> impl Future<Output = hgl_ai_bus::Result<TreeRecord>> + Send;
    fn tree_del(&self, key: &str) -> impl Future<Output = hgl_ai_bus::Result<()>> + Send;
    fn trees(&self) -> impl Future<Output = hgl_ai_bus::Result<Vec<TreeRecord>>> + Send;
}

impl Registry for hgl_ai_bus::Bus {
    async fn tree_put(&self, rec: &TreeRecord) -> hgl_ai_bus::Result<()> {
        hgl_ai_bus::Bus::tree_put(self, rec).await
    }
    async fn tree_merge(&self, rec: &TreeRecord) -> hgl_ai_bus::Result<TreeRecord> {
        hgl_ai_bus::Bus::tree_merge(self, rec).await
    }
    async fn tree_del(&self, key: &str) -> hgl_ai_bus::Result<()> {
        hgl_ai_bus::Bus::tree_del(self, key).await
    }
    async fn trees(&self) -> hgl_ai_bus::Result<Vec<TreeRecord>> {
        hgl_ai_bus::Bus::trees(self).await
    }
}

/// The trees of one host.
#[derive(Debug)]
pub struct Trees {
    layout: Layout,
    host: String,
    github: Github,
    git: Git,
    /// Serialises git work on the shared clones within this process.
    lock: Mutex<()>,
}

impl Trees {
    pub fn new(cfg: &Config, git: Git) -> Result<Self> {
        Ok(Self::with(
            Layout::new(cfg.work_root()?)?,
            &cfg.host,
            &cfg.github,
            git,
        ))
    }

    pub fn with(layout: Layout, host: &str, github: &Github, git: Git) -> Self {
        Trees {
            layout,
            host: host.to_string(),
            github: github.clone(),
            git,
            lock: Mutex::new(()),
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn git(&self) -> &Git {
        &self.git
    }

    /// The task directory, if it holds a tree.
    pub fn existing(&self, seat: &Seat, task: &TaskKey, review: bool) -> Option<PathBuf> {
        let d = self.layout.task_dir(seat, task, review).ok()?;
        d.join(TASK_FILE).is_file().then_some(d)
    }

    /// `<work_root>/<seat>/_inbox` with its `tmp/` and `build/`.
    pub fn ensure_inbox(&self, seat: &Seat) -> Result<PathBuf> {
        let d = self.layout.inbox_dir(seat)?;
        for sub in ["tmp", "build"] {
            mkdir(&d.join(sub))?;
        }
        Ok(d)
    }

    /// Create the task's tree (PROTO-12). An implementor tree that exists is kept;
    /// a review tree that exists is an error (the caller removes it first).
    pub fn create(&self, seat: &Seat, task: &TaskKey, what: &Create) -> Result<Created> {
        self.make(seat, task, what, None, &|| None)
    }

    /// [`Trees::create`] for the note at stream sequence `seq`, recorded in a new tree's
    /// `task.json` ([`TaskFile::seq`]); a kept tree keeps its own. `title` is asked for
    /// the issue's title only when a new branch is made (WORK §2 step 3); `None` names it
    /// from the note's line.
    pub fn create_for(
        &self,
        seat: &Seat,
        task: &TaskKey,
        what: &Create,
        seq: u64,
        title: &dyn Fn() -> Option<String>,
    ) -> Result<Created> {
        self.make(seat, task, what, Some(seq), title)
    }

    fn make(
        &self,
        seat: &Seat,
        task: &TaskKey,
        what: &Create,
        seq: Option<u64>,
        issue_title: &dyn Fn() -> Option<String>,
    ) -> Result<Created> {
        let review = matches!(what, Create::Review { .. });
        let dir = self.layout.task_dir(seat, task, review)?;
        if dir.join(TASK_FILE).is_file() {
            if review {
                return Err(TreeError::Exists(dir));
            }
            let file = read_task(&dir)?;
            // A git killed in an earlier turn may have left its lock behind.
            clear_stale_index_lock(&dir.join("src"));
            let record = self.record(&dir, &file, dir_size(&dir));
            return Ok(Created {
                dir,
                file,
                record,
                reused: true,
            });
        }
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let clone = self.ensure_clone(&task.repo)?;
        if std::fs::symlink_metadata(&dir).is_ok() {
            // A partial tree from an interrupted create.
            self.remove_dir(&dir)?;
        }
        self.git.run(&clone, ["worktree", "prune"])?;
        for sub in ["tmp", "build"] {
            mkdir(&dir.join(sub))?;
        }
        let made = match what {
            Create::Implement { title, base } => self.add_implementor(
                &clone,
                &dir,
                seat,
                task,
                Naming {
                    note_line: title,
                    issue_title,
                },
                base.as_deref(),
            ),
            Create::Review { head, base } => {
                self.add_review(&clone, &dir, seat, task, head, base.as_deref())
            }
        };
        let mut file = match made {
            Ok(f) => f,
            Err(e) => {
                let _ = self.remove_dir(&dir);
                let _ = self.git.run(&clone, ["worktree", "prune"]);
                return Err(e);
            }
        };
        file.seq = seq;
        write_task(&dir, &file)?;
        let record = self.record(&dir, &file, dir_size(&dir));
        Ok(Created {
            dir,
            file,
            record,
            reused: false,
        })
    }

    fn add_implementor(
        &self,
        clone: &Path,
        dir: &Path,
        seat: &Seat,
        task: &TaskKey,
        naming: Naming<'_>,
        base: Option<&str>,
    ) -> Result<TaskFile> {
        let Naming {
            note_line,
            issue_title,
        } = naming;
        let src = dir.join("src");
        let prefix = &self.github.branch_prefix;
        let from_note = slug::branch_name(prefix, task.issue, &slug::note_slug(note_line));
        let starts = slug::branch_starts(prefix, task.issue);
        let ours = |h: &&String| starts.iter().any(|p| h.starts_with(p.as_str()));
        // The task's branch: on the remote (a sendback, a resumed or moved task), else one
        // made on this host and not pushed yet, else a new one. Of several, the note's name.
        let pick = |names: &[String]| {
            if names.contains(&from_note) {
                Some(from_note.clone())
            } else {
                names.iter().filter(ours).min().cloned()
            }
        };
        let on_remote = pick(&self.remote_heads(clone)?);
        let locals = self.local_heads(clone)?;
        // Only a new branch asks for the issue's title.
        let branch = match on_remote.clone().or_else(|| pick(&locals)) {
            Some(b) => b,
            None => match issue_title() {
                Some(t) => slug::branch_name(prefix, task.issue, &slug::slug(&t)),
                None => from_note,
            },
        };
        let local = locals.contains(&branch);
        let base_ref = self.resolve_base(clone, base)?;
        let src_os = src.as_os_str();
        if local {
            // Another seat of this host may still have the branch checked out (the task
            // moved to this seat, ROLE-15): share it rather than fail.
            let mut args: Vec<&std::ffi::OsStr> = vec!["worktree".as_ref(), "add".as_ref()];
            if let Some(other) = self.checked_out_at(clone, &branch)? {
                tracing::warn!(%branch, other = %other.display(), "the branch is checked out in another tree of this host; sharing it");
                args.push("--force".as_ref());
            }
            args.extend([src_os, branch.as_ref()]);
            self.git.run(clone, args)?;
        } else if on_remote.is_some() {
            // A sendback or a resumed task: continue the pushed branch.
            let tracking = format!("origin/{branch}");
            self.git.run(
                clone,
                [
                    "fetch",
                    "--quiet",
                    "origin",
                    &format!("+refs/heads/{branch}:refs/remotes/{tracking}"),
                ],
            )?;
            let args: [&std::ffi::OsStr; 7] = [
                "worktree".as_ref(),
                "add".as_ref(),
                "--track".as_ref(),
                "-b".as_ref(),
                branch.as_ref(),
                src_os,
                tracking.as_ref(),
            ];
            self.git.run(clone, args)?;
        } else {
            // `--no-track`, so a plain `git push` never targets the base branch.
            let args: [&std::ffi::OsStr; 7] = [
                "worktree".as_ref(),
                "add".as_ref(),
                "--no-track".as_ref(),
                "-b".as_ref(),
                branch.as_ref(),
                src_os,
                base_ref.as_ref(),
            ];
            self.git.run(clone, args)?;
        }
        let start = self.rev(&src, "HEAD")?;
        let main = self.rev(clone, &base_ref).ok();
        Ok(TaskFile {
            seat: seat.clone(),
            repo: task.repo.clone(),
            issue: task.issue,
            review: false,
            head: None,
            branch: Some(branch),
            base: base_ref,
            main,
            checkout: Some(start.clone()),
            start,
            created_at: hgl_ai_proto::utc_now_stamp(),
            conflicts: Vec::new(),
            sessions: BTreeMap::new(),
            seq: None,
            families: Vec::new(),
            class: None,
            class_seq: None,
            handoff_seq: None,
        })
    }

    fn add_review(
        &self,
        clone: &Path,
        dir: &Path,
        seat: &Seat,
        task: &TaskKey,
        head: &str,
        base: Option<&str>,
    ) -> Result<TaskFile> {
        hgl_ai_proto::check_head(head).map_err(|e| TreeError::Invalid(e.to_string()))?;
        let src = dir.join("src");
        if !self.has_commit(clone, head)? {
            let pr_ref = format!(
                "+refs/pull/{}/head:refs/remotes/origin/pr/{}",
                task.issue, task.issue
            );
            let _ = self
                .git
                .output(clone, ["fetch", "--quiet", "origin", &pr_ref])?;
            if !self.has_commit(clone, head)? {
                let _ = self
                    .git
                    .output(clone, ["fetch", "--quiet", "origin", head])?;
            }
            if !self.has_commit(clone, head)? {
                return Err(TreeError::MissingCommit(head.to_string()));
            }
        }
        let base_ref = self.resolve_base(clone, base)?;
        let main = self.rev(clone, &base_ref)?;
        let args: [&std::ffi::OsStr; 5] = [
            "worktree".as_ref(),
            "add".as_ref(),
            "--detach".as_ref(),
            src.as_os_str(),
            main.as_ref(),
        ];
        self.git.run(clone, args)?;
        let msg = format!("Review merge of {} onto {}", &head[..12], &main[..12]);
        let mut args: Vec<&str> = MERGE_IDENTITY.to_vec();
        args.extend(["merge", "--no-edit", "-m", &msg, head]);
        let merged = self.git.output(&src, &args)?;
        let mut conflicts = Vec::new();
        if !merged.ok {
            let unmerged = self
                .git
                .run(&src, ["diff", "--name-only", "--diff-filter=U"])
                .unwrap_or_default();
            conflicts = unmerged.lines().map(str::to_string).collect();
            if conflicts.is_empty() {
                let why = merged
                    .stderr
                    .lines()
                    .chain(merged.stdout.lines())
                    .find(|l| !l.trim().is_empty());
                conflicts.push(format!(
                    "(merge failed: {})",
                    why.unwrap_or("no message").trim()
                ));
            }
            let _ = self.git.output(&src, ["merge", "--abort"])?;
            self.git
                .run(&src, ["checkout", "--quiet", "--force", "--detach", head])?;
        }
        let start = self.rev(&src, "HEAD")?;
        Ok(TaskFile {
            seat: seat.clone(),
            repo: task.repo.clone(),
            issue: task.issue,
            review: true,
            head: Some(head.to_string()),
            branch: None,
            base: base_ref,
            main: Some(main),
            start,
            created_at: hgl_ai_proto::utc_now_stamp(),
            conflicts,
            sessions: BTreeMap::new(),
            seq: None,
            families: Vec::new(),
            class: None,
            class_seq: None,
            handoff_seq: None,
            checkout: None,
        })
    }

    /// Remove the task's tree (PROTO-14): salvage, worktree, branch, directory. The caller
    /// deregisters `Removed::key`. `None` when there is no such tree.
    pub fn remove(&self, seat: &Seat, task: &TaskKey, review: bool) -> Result<Option<Removed>> {
        let dir = self.layout.task_dir(seat, task, review)?;
        if std::fs::symlink_metadata(&dir).is_err() {
            return Ok(None);
        }
        let dir = self.layout.confine_existing(&dir)?;
        let file = read_task(&dir).ok();
        let freed_bytes = dir_size(&dir);
        let src = dir.join("src");
        let clone = self
            .layout
            .clone_dir(&task.repo)
            .ok()
            .filter(|c| c.is_dir());
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut salvage = None;
        if src.join(".git").exists() {
            // A git killed with the turn may have left its lock; salvage reads through an
            // index of its own either way.
            clear_stale_index_lock(&src);
            salvage = self.salvage(&src, seat, task, file.as_ref(), &dir)?;
            if let Some(clone) = &clone {
                let args: [&std::ffi::OsStr; 4] = [
                    "worktree".as_ref(),
                    "remove".as_ref(),
                    "--force".as_ref(),
                    src.as_os_str(),
                ];
                let _ = self.git.output(clone, args)?;
            }
        }
        self.remove_dir(&dir)?;
        let branch = file.as_ref().and_then(|f| f.branch.clone());
        if let Some(clone) = &clone {
            let _ = self.git.output(clone, ["worktree", "prune"])?;
            if let Some(b) = &branch {
                let _ = self.git.output(clone, ["branch", "-D", b])?;
            }
        }
        let key = hgl_ai_proto::tree_key(&self.host, seat, task, review);
        Ok(Some(Removed {
            key,
            dir,
            branch,
            freed_bytes,
            salvage,
        }))
    }

    /// Write uncommitted changes (untracked files included) and unpushed commits to
    /// `<work_root>/salvage/<seat>-<repo>-<issue>-<UTC>.patch`, if there are any.
    fn salvage(
        &self,
        src: &Path,
        seat: &Seat,
        task: &TaskKey,
        file: Option<&TaskFile>,
        dir: &Path,
    ) -> Result<Option<PathBuf>> {
        // Read-only on the tree's own index: no optional locks, and the untracked files are
        // added to a copy of it, so a held or stale `index.lock` cannot stop the salvage.
        let status = self
            .git
            .output(src, ["--no-optional-locks", "status", "--porcelain"])?;
        if !status.ok {
            tracing::warn!(src = %src.display(), "git status failed; nothing salvaged: {}", status.stderr.trim());
            return Ok(None);
        }
        let dirty = !status.stdout.trim().is_empty();
        let mut range: Vec<String> = vec!["HEAD".into(), "--not".into(), "--remotes=origin".into()];
        if let Some(start) = file.map(|f| f.start.as_str()).filter(|s| !s.is_empty())
            && self.has_commit(src, start)?
        {
            range.push(start.to_string());
        }
        let mut rl = vec!["rev-list".to_string()];
        rl.extend(range.iter().cloned());
        let unpushed: Vec<String> = self
            .git
            .run(src, &rl)?
            .lines()
            .map(str::to_string)
            .collect();
        if !dirty && unpushed.is_empty() {
            return Ok(None);
        }
        let now = hgl_ai_proto::utc_now_stamp();
        let head = self.rev(src, "HEAD").unwrap_or_default();
        let mut patch = format!(
            "# hgl-ai salvage of {seat} {}#{} at {now}\n# HEAD {head}; unpushed commits: {}; uncommitted changes: {}\n",
            task.repo,
            task.issue,
            unpushed.len(),
            if dirty { "yes" } else { "no" }
        );
        if !unpushed.is_empty() {
            let mut fp = vec![
                "format-patch".to_string(),
                "--stdout".into(),
                "--binary".into(),
            ];
            fp.extend(range);
            patch.push_str(&self.git.run(src, &fp)?);
        }
        if dirty {
            let index = dir.join("salvage.index");
            if let Some(own) = git_dir(src)
                .map(|g| g.join("index"))
                .filter(|i| i.is_file())
            {
                std::fs::copy(&own, &index).map_err(|e| TreeError::io(&own, e))?;
            }
            let env = [("GIT_INDEX_FILE", index.as_os_str())];
            let diffed = self
                .git
                .run_env(src, ["add", "--all", "--intent-to-add"], &env)
                .and_then(|_| self.git.run_env(src, ["diff", "HEAD", "--binary"], &env));
            let _ = std::fs::remove_file(&index);
            patch.push_str(&diffed?);
        }
        let dir = self.layout.salvage_dir();
        mkdir(&dir)?;
        let stem = format!("{seat}-{}-{}-{now}", task.repo, task.issue);
        let mut path = dir.join(format!("{stem}.patch"));
        let mut n = 2;
        while path.exists() {
            path = dir.join(format!("{stem}-{n}.patch"));
            n += 1;
        }
        std::fs::write(&path, patch).map_err(|e| TreeError::io(&path, e))?;
        Ok(Some(path))
    }

    /// Every task directory under `work_root`, with its measured size.
    pub fn list(&self) -> Result<Vec<LocalTree>> {
        let mut out = Vec::new();
        for seat_ent in read_dir(self.layout.root())? {
            let Ok(seat) = Seat::new(seat_ent.file_name().to_string_lossy().into_owned()) else {
                continue;
            };
            if !seat_ent.path().is_dir() || self.layout.seat_dir(&seat).is_err() {
                continue;
            }
            for ent in read_dir(&seat_ent.path())? {
                let name = ent.file_name().to_string_lossy().into_owned();
                let Some((task, review)) = layout::parse_task_dir_name(&name) else {
                    continue;
                };
                let dir = ent.path();
                if !dir.is_dir() {
                    continue;
                }
                let size = dir_size(&dir);
                let file = read_task(&dir).ok();
                let record = match &file {
                    Some(f) => self.record(&dir, f, size),
                    None => TreeRecord {
                        host: self.host.clone(),
                        seat: seat.clone(),
                        task,
                        review,
                        path: dir.display().to_string(),
                        branch: None,
                        created_at: mtime_stamp(&dir),
                        size_bytes: size,
                        families: Vec::new(),
                        class: None,
                        class_seq: None,
                        handoff_seq: None,
                        seq: None,
                    },
                };
                out.push(LocalTree { dir, record, file });
            }
        }
        out.sort_by_key(|t| t.record.key());
        Ok(out)
    }

    /// The `HGL_TREES` record of a task directory.
    pub fn record(&self, dir: &Path, f: &TaskFile, size_bytes: u64) -> TreeRecord {
        TreeRecord {
            host: self.host.clone(),
            seat: f.seat.clone(),
            task: f.task(),
            review: f.review,
            path: dir.display().to_string(),
            branch: f.branch.clone(),
            created_at: f.created_at.clone(),
            size_bytes,
            families: f.families.clone(),
            class: f.class,
            class_seq: f.class_seq,
            seq: f.seq,
            handoff_seq: f.handoff_seq,
        }
    }

    /// Bring a kept implementor tree to its branch's newest commit (hgl-ai#55), keeping its
    /// uncommitted changes: a seat the task moved to in between may have committed on it.
    /// First, a branch that another tree of this host moved under this one (the two share
    /// the ref) since [`TaskFile::checkout`] (else `start`): a two-way `read-tree -m -u` from
    /// that commit to HEAD, which takes the other tree's files and refuses to overwrite a
    /// change of this one. Then the pushed branch, fetched: a fast-forward when the tree is
    /// behind; a branch never pushed, or one that has diverged, is left. Records the commit
    /// reached in `f.checkout` and returns it when the index or files moved; an error leaves
    /// `checkout` as it was.
    pub fn refresh_branch(&self, dir: &Path, f: &mut TaskFile) -> Result<Option<String>> {
        let Some(branch) = f.branch.clone().filter(|_| !f.review) else {
            return Ok(None);
        };
        let src = dir.join("src");
        // A git killed in an earlier turn may have left its lock behind.
        clear_stale_index_lock(&src);
        let head = self.rev(&src, "HEAD")?;
        let old = f.checkout.clone().unwrap_or_else(|| f.start.clone());
        let mut moved = false;
        if old != head {
            let index = |t: &Self| {
                t.git
                    .run(&src, ["write-tree"])
                    .map(|o| o.trim().to_string())
            };
            let before = index(self)?;
            self.git
                .run(&src, ["read-tree", "-m", "-u", old.as_str(), head.as_str()])?;
            moved = index(self)? != before;
        }
        let tracking = format!("refs/remotes/origin/{branch}");
        let fetched = {
            let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            let clone = self.ensure_clone(&f.repo)?;
            self.git.output(
                &clone,
                [
                    "fetch",
                    "--quiet",
                    "origin",
                    &format!("+refs/heads/{branch}:{tracking}"),
                ],
            )?
        };
        if fetched.ok {
            let pushed = self.rev(&src, &tracking)?;
            if pushed != head
                && self
                    .git
                    .check(&src, ["merge-base", "--is-ancestor", &head, &pushed])?
            {
                self.git
                    .run(&src, ["merge", "--ff-only", "--quiet", &pushed])?;
                moved = true;
            }
        }
        let now = self.rev(&src, "HEAD")?;
        f.checkout = Some(now.clone());
        Ok(moved.then_some(now))
    }

    /// Record in `task.json` the commit an implementor tree's index and files are at after a
    /// turn ([`TaskFile::checkout`]).
    pub fn note_checkout(&self, dir: &Path) -> Result<()> {
        let mut f = read_task(dir)?;
        if f.review || f.branch.is_none() {
            return Ok(());
        }
        let head = self.rev(&dir.join("src"), "HEAD")?;
        if f.checkout.as_deref() != Some(head.as_str()) {
            f.checkout = Some(head);
            write_task(dir, &f)?;
        }
        Ok(())
    }

    /// `<work_root>/clones/<repo>`: a bare repository with `origin`, fetched now.
    fn ensure_clone(&self, repo: &str) -> Result<PathBuf> {
        let clone = self.layout.clone_dir(repo)?;
        let url = self.github.clone_url_for(repo);
        if !clone.join("HEAD").is_file() {
            mkdir(&clone)?;
            self.git.run(&clone, ["init", "--quiet", "--bare"])?;
        }
        if self.git.check(&clone, ["remote", "get-url", "origin"])? {
            self.git
                .run(&clone, ["remote", "set-url", "origin", &url])?;
        } else {
            self.git.run(&clone, ["remote", "add", "origin", &url])?;
        }
        // No `--prune`: a merged branch deleted upstream keeps its tracking ref, so its
        // pushed commits do not look unpushed at removal.
        self.git.run(&clone, ["fetch", "--quiet", "origin"])?;
        Ok(clone)
    }

    /// The clone's local branches.
    fn local_heads(&self, clone: &Path) -> Result<Vec<String>> {
        let out = self.git.run(
            clone,
            ["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
        )?;
        Ok(out.lines().map(str::to_string).collect())
    }

    /// The worktree of the clone that has `branch` checked out, if any.
    fn checked_out_at(&self, clone: &Path, branch: &str) -> Result<Option<PathBuf>> {
        let out = self.git.run(clone, ["worktree", "list", "--porcelain"])?;
        let want = format!("branch refs/heads/{branch}");
        let mut at = None;
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                at = Some(PathBuf::from(p));
            } else if line == want {
                return Ok(at.filter(|p| p.exists()));
            }
        }
        Ok(None)
    }

    fn remote_heads(&self, clone: &Path) -> Result<Vec<String>> {
        let out = self.git.run(clone, ["ls-remote", "--heads", "origin"])?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .filter_map(|(_, r)| r.strip_prefix("refs/heads/"))
            .map(str::to_string)
            .collect())
    }

    /// `origin/<base>` when the remote has that branch, else `base` as a commit; default `origin/main`.
    fn resolve_base(&self, clone: &Path, base: Option<&str>) -> Result<String> {
        let base = base.unwrap_or("main");
        let remote = format!("origin/{base}");
        if self.git.check(
            clone,
            [
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/remotes/{remote}"),
            ],
        )? {
            return Ok(remote);
        }
        if self.has_commit(clone, base)? {
            return Ok(base.to_string());
        }
        Err(TreeError::Invalid(format!(
            "base {base:?} is neither a branch of origin nor a commit"
        )))
    }

    fn has_commit(&self, cwd: &Path, rev: &str) -> Result<bool> {
        self.git
            .check(cwd, ["cat-file", "-e", &format!("{rev}^{{commit}}")])
    }

    fn rev(&self, cwd: &Path, rev: &str) -> Result<String> {
        Ok(self
            .git
            .run(cwd, ["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?
            .trim()
            .to_string())
    }

    fn remove_dir(&self, dir: &Path) -> Result<()> {
        let dir = self.layout.confine(dir)?;
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_symlink() => return Err(TreeError::Escape(dir)),
            Ok(_) => {}
            Err(_) => return Ok(()),
        }
        std::fs::remove_dir_all(&dir).map_err(|e| TreeError::io(&dir, e))
    }
}

/// The git directory of a worktree checkout: what its `.git` file names.
fn git_dir(src: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(src.join(".git")).ok()?;
    let p = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    Some(if p.is_absolute() { p } else { src.join(p) })
}

/// Remove the worktree's `index.lock` when no process has it open: a git killed with its
/// turn left it, and every later git command in the tree would fail on it.
pub fn clear_stale_index_lock(src: &Path) -> bool {
    let Some(lock) = git_dir(src).map(|g| g.join("index.lock")) else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(&lock) else {
        return false;
    };
    if held_open(&lock, &meta) {
        tracing::info!(lock = %lock.display(), "index.lock is held by a running process; left");
        return false;
    }
    match std::fs::remove_file(&lock) {
        Ok(()) => {
            tracing::warn!(lock = %lock.display(), "removed a stale index.lock");
            true
        }
        Err(e) => {
            tracing::warn!(lock = %lock.display(), "removing a stale index.lock: {e}");
            false
        }
    }
}

/// Whether some process has `path` open: `/proc` on Linux, `lsof` elsewhere; unknown
/// counts as held unless the file is older than an hour.
fn held_open(path: &Path, meta: &std::fs::Metadata) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let id = (meta.dev(), meta.ino());
        let Ok(procs) = std::fs::read_dir("/proc") else {
            return true;
        };
        for p in procs.flatten() {
            let Ok(fds) = std::fs::read_dir(p.path().join("fd")) else {
                continue;
            };
            if fds
                .flatten()
                .any(|fd| std::fs::metadata(fd.path()).is_ok_and(|m| (m.dev(), m.ino()) == id))
            {
                return true;
            }
        }
        let _ = path;
        false
    }
    #[cfg(not(target_os = "linux"))]
    {
        match std::process::Command::new("lsof")
            .arg("-t")
            .arg("--")
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        {
            Ok(o) => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
            Err(_) => meta
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_none_or(|age| age < std::time::Duration::from_secs(3600)),
        }
    }
}

pub fn read_task(dir: &Path) -> Result<TaskFile> {
    let p = dir.join(TASK_FILE);
    let text = std::fs::read_to_string(&p).map_err(|e| TreeError::io(&p, e))?;
    serde_json::from_str(&text).map_err(|source| TreeError::Json { path: p, source })
}

/// Write `task.json` atomically.
pub fn write_task(dir: &Path, f: &TaskFile) -> Result<()> {
    let p = dir.join(TASK_FILE);
    let tmp = dir.join(format!("{TASK_FILE}.tmp"));
    let text = serde_json::to_string_pretty(f).map_err(|source| TreeError::Json {
        path: p.clone(),
        source,
    })?;
    std::fs::write(&tmp, text + "\n").map_err(|e| TreeError::io(&tmp, e))?;
    std::fs::rename(&tmp, &p).map_err(|e| TreeError::io(&p, e))
}

/// Bytes under `p`, not following symlinks; unreadable entries count as 0.
pub fn dir_size(p: &Path) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    if !m.is_dir() {
        return m.len();
    }
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += m.len();
            }
        }
    }
    total
}

fn mkdir(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p).map_err(|e| TreeError::io(p, e))
}

fn read_dir(p: &Path) -> Result<Vec<std::fs::DirEntry>> {
    match std::fs::read_dir(p) {
        Ok(rd) => Ok(rd.flatten().collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(TreeError::io(p, e)),
    }
}

fn mtime_stamp(p: &Path) -> String {
    let t = std::fs::metadata(p)
        .and_then(|m| m.modified())
        .map(chrono::DateTime::<chrono::Utc>::from);
    t.map(hgl_ai_proto::utc_stamp)
        .unwrap_or_else(|_| hgl_ai_proto::utc_now_stamp())
}

/// Helpers for tests here and in the bridge: a local "remote" with a `main` branch.
#[doc(hidden)]
pub mod testing {
    use std::path::{Path, PathBuf};

    use crate::Git;

    pub const IDENTITY: [&str; 6] = [
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.invalid",
        "-c",
        "commit.gpgsign=false",
    ];

    /// A git runner isolated from the user's configuration, with its empty config in `dir`.
    pub fn git(dir: &Path) -> Git {
        let cfg = dir.join("gitconfig");
        std::fs::write(&cfg, "").unwrap();
        Git::isolated(&cfg)
    }

    /// `<dir>/<repo>.git` (bare, branch `main` with one commit) and its seed checkout `<dir>/<repo>-seed`.
    pub fn remote(git: &Git, dir: &Path, repo: &str) -> (PathBuf, PathBuf) {
        let bare = dir.join(format!("{repo}.git"));
        let seed = dir.join(format!("{repo}-seed"));
        std::fs::create_dir_all(&bare).unwrap();
        git.run(&bare, ["init", "--quiet", "--bare", "-b", "main"])
            .unwrap();
        git.run(
            dir,
            [
                "clone".as_ref(),
                "--quiet".as_ref(),
                bare.as_os_str(),
                seed.as_os_str(),
            ],
        )
        .unwrap();
        git.run(&seed, ["checkout", "--quiet", "-b", "main"])
            .unwrap();
        commit(git, &seed, "README", "hello\n", "initial");
        git.run(&seed, ["push", "--quiet", "origin", "main"])
            .unwrap();
        (bare, seed)
    }

    /// Write `file` and commit it in `repo`; returns the new HEAD.
    pub fn commit(git: &Git, repo: &Path, file: &str, text: &str, msg: &str) -> String {
        std::fs::write(repo.join(file), text).unwrap();
        git.run(repo, ["add", file]).unwrap();
        let mut args = IDENTITY.to_vec();
        args.extend(["commit", "--quiet", "-m", msg]);
        git.run(repo, args).unwrap();
        git.run(repo, ["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string()
    }
}

#[cfg(test)]
mod tests;
