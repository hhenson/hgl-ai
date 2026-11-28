//! Git lifecycle against a local bare "remote" (no network).

use super::testing::{commit, git, remote};
use super::*;

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    git: Git,
    seed: PathBuf,
    trees: Trees,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let git = git(&root);
    let (_bare, seed) = remote(&git, &root, "demo");
    let github = Github {
        clone_url: format!("{}/{{repo}}.git", root.display()),
        ..Github::default()
    };
    let trees = Trees::with(
        Layout::new(root.join("work")).unwrap(),
        "build-1",
        &github,
        git.clone(),
    );
    Env {
        _dir: dir,
        root,
        git,
        seed,
        trees,
    }
}

fn seat(s: &str) -> Seat {
    Seat::new(s).unwrap()
}

fn task(n: u64) -> TaskKey {
    TaskKey {
        repo: "demo".into(),
        issue: n,
    }
}

fn implement(title: &str) -> Create {
    Create::Implement {
        title: title.into(),
        base: None,
    }
}

#[test]
fn implementor_tree_on_a_new_branch() {
    let e = env();
    let c = e
        .trees
        .create_for(
            &seat("w1"),
            &task(980),
            &implement("[claude] Assign: demo#980 Fix the thing"),
            42,
            &|| None,
        )
        .unwrap();
    assert!(!c.reused);
    assert_eq!(c.file.seq, Some(42), "the note that made the tree");
    let work = e.root.join("work");
    assert_eq!(c.dir, work.join("w1/demo-980"));
    for sub in ["src/README", "tmp", "build", "task.json"] {
        assert!(c.dir.join(sub).exists(), "{sub}");
    }
    assert_eq!(c.file.branch.as_deref(), Some("codex/q980-fix-the-thing"));
    assert_eq!(c.file.base, "origin/main");
    assert_eq!(c.file.start, c.file.main.clone().unwrap());
    let src = c.dir.join("src");
    assert_eq!(
        e.git
            .run(&src, ["branch", "--show-current"])
            .unwrap()
            .trim(),
        "codex/q980-fix-the-thing"
    );
    let upstream = e
        .git
        .output(&src, ["rev-parse", "--abbrev-ref", "@{u}"])
        .unwrap();
    assert!(
        !upstream.ok,
        "a new branch tracks nothing, so `git push` cannot hit main"
    );
    assert_eq!(c.record.key(), "build-1.w1.demo-980");
    assert!(c.record.size_bytes > 0);
    assert!(work.join("clones/demo/HEAD").is_file());

    let again = e
        .trees
        .create_for(
            &seat("w1"),
            &task(980),
            &implement("other title"),
            50,
            &|| panic!("a kept tree asks for no title"),
        )
        .unwrap();
    assert!(again.reused);
    assert_eq!(again.file.seq, Some(42), "a kept tree keeps its own");
    assert_eq!(again.file, c.file);
    assert_eq!(read_task(&c.dir).unwrap(), c.file);
}

#[test]
fn a_pushed_branch_is_resumed() {
    let e = env();
    e.git
        .run(
            &e.seed,
            ["checkout", "--quiet", "-b", "codex/7-first-title"],
        )
        .unwrap();
    let pushed = commit(&e.git, &e.seed, "work.txt", "w\n", "work");
    e.git
        .run(
            &e.seed,
            ["push", "--quiet", "origin", "codex/7-first-title"],
        )
        .unwrap();
    let c = e
        .trees
        .create(&seat("w2"), &task(7), &implement("A different title"))
        .unwrap();
    assert_eq!(c.file.branch.as_deref(), Some("codex/7-first-title"));
    let src = c.dir.join("src");
    assert_eq!(
        e.git.run(&src, ["rev-parse", "HEAD"]).unwrap().trim(),
        pushed
    );
    assert_eq!(
        e.git
            .run(&src, ["rev-parse", "--abbrev-ref", "@{u}"])
            .unwrap()
            .trim(),
        "origin/codex/7-first-title"
    );
}

#[test]
fn review_tree_merges_head_onto_main() {
    let e = env();
    e.git
        .run(&e.seed, ["checkout", "--quiet", "-b", "feature"])
        .unwrap();
    let head = commit(&e.git, &e.seed, "feature.txt", "f\n", "feature");
    e.git
        .run(&e.seed, ["push", "--quiet", "origin", "feature"])
        .unwrap();
    e.git.run(&e.seed, ["checkout", "--quiet", "main"]).unwrap();
    let main = commit(&e.git, &e.seed, "main.txt", "m\n", "main moves");
    e.git
        .run(&e.seed, ["push", "--quiet", "origin", "main"])
        .unwrap();

    let c = e
        .trees
        .create(
            &seat("r1"),
            &task(919),
            &Create::Review {
                head: head.clone(),
                base: None,
            },
        )
        .unwrap();
    assert_eq!(c.dir, e.root.join("work/r1/demo-919-review"));
    assert!(c.file.conflicts.is_empty());
    assert_eq!(c.file.main.as_deref(), Some(main.as_str()));
    assert_eq!(c.file.branch, None);
    assert_eq!(c.record.key(), "build-1.r1.demo-919-review");
    let src = c.dir.join("src");
    assert!(src.join("feature.txt").exists() && src.join("main.txt").exists());
    assert!(
        e.git
            .check(&src, ["merge-base", "--is-ancestor", &head, "HEAD"])
            .unwrap()
    );
    assert!(
        e.git
            .check(&src, ["merge-base", "--is-ancestor", &main, "HEAD"])
            .unwrap()
    );
    assert!(
        e.git
            .run(&src, ["branch", "--show-current"])
            .unwrap()
            .trim()
            .is_empty(),
        "detached"
    );

    assert!(matches!(
        e.trees.create(
            &seat("r1"),
            &task(919),
            &Create::Review { head, base: None }
        ),
        Err(TreeError::Exists(_))
    ));
    // Untouched, the bridge's own merge commit is not salvaged.
    let rm = e
        .trees
        .remove(&seat("r1"), &task(919), true)
        .unwrap()
        .unwrap();
    assert_eq!(rm.salvage, None);
    assert!(!rm.dir.exists());
}

#[test]
fn review_conflict_is_a_result_not_an_error() {
    let e = env();
    e.git
        .run(&e.seed, ["checkout", "--quiet", "-b", "clash"])
        .unwrap();
    let head = commit(&e.git, &e.seed, "README", "theirs\n", "clash");
    e.git
        .run(&e.seed, ["push", "--quiet", "origin", "clash"])
        .unwrap();
    e.git.run(&e.seed, ["checkout", "--quiet", "main"]).unwrap();
    commit(&e.git, &e.seed, "README", "ours\n", "main edit");
    e.git
        .run(&e.seed, ["push", "--quiet", "origin", "main"])
        .unwrap();

    let c = e
        .trees
        .create(
            &seat("r1"),
            &task(5),
            &Create::Review {
                head: head.clone(),
                base: None,
            },
        )
        .unwrap();
    assert_eq!(c.file.conflicts, vec!["README".to_string()]);
    let src = c.dir.join("src");
    assert_eq!(e.git.run(&src, ["rev-parse", "HEAD"]).unwrap().trim(), head);
    assert!(
        e.git
            .run(&src, ["status", "--porcelain"])
            .unwrap()
            .trim()
            .is_empty()
    );
}

#[test]
fn missing_head_fails_and_leaves_nothing() {
    let e = env();
    let head = "0123456789abcdef0123456789abcdef01234567".to_string();
    let r = e
        .trees
        .create(&seat("r1"), &task(6), &Create::Review { head, base: None });
    assert!(matches!(r, Err(TreeError::MissingCommit(_))), "{r:?}");
    assert!(!e.root.join("work/r1/demo-6-review").exists());
}

#[test]
fn clean_removal_frees_everything() {
    let e = env();
    let c = e
        .trees
        .create(&seat("w1"), &task(1), &implement("one"))
        .unwrap();
    let src = c.dir.join("src");
    commit(&e.git, &src, "pushed.txt", "p\n", "pushed");
    e.git
        .run(&src, ["push", "--quiet", "-u", "origin", "HEAD"])
        .unwrap();
    let rm = e
        .trees
        .remove(&seat("w1"), &task(1), false)
        .unwrap()
        .unwrap();
    assert_eq!(rm.salvage, None);
    assert_eq!(rm.key, "build-1.w1.demo-1");
    assert_eq!(rm.branch.as_deref(), Some("codex/q1-one"));
    assert!(rm.freed_bytes > 0);
    assert!(!c.dir.exists());
    let clone = e.root.join("work/clones/demo");
    assert!(
        !e.git
            .check(
                &clone,
                [
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/heads/codex/q1-one"
                ]
            )
            .unwrap()
    );
    assert_eq!(
        e.git
            .run(&clone, ["worktree", "list"])
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(
        e.trees
            .remove(&seat("w1"), &task(1), false)
            .unwrap()
            .is_none()
    );
}

#[test]
fn dirty_removal_salvages_a_patch() {
    let e = env();
    let c = e
        .trees
        .create(&seat("w1"), &task(2), &implement("two"))
        .unwrap();
    let src = c.dir.join("src");
    commit(&e.git, &src, "local.txt", "unpushed work\n", "unpushed");
    std::fs::write(src.join("README"), "edited\n").unwrap();
    std::fs::write(src.join("new.txt"), "untracked\n").unwrap();
    let rm = e
        .trees
        .remove(&seat("w1"), &task(2), false)
        .unwrap()
        .unwrap();
    let patch = rm.salvage.expect("salvaged");
    assert!(patch.starts_with(e.root.join("work/salvage")));
    let name = patch.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("w1-demo-2-") && name.ends_with("Z.patch"),
        "{name}"
    );
    let text = std::fs::read_to_string(&patch).unwrap();
    for needle in [
        "unpushed commits: 1",
        "Subject: [PATCH] unpushed",
        "+unpushed work",
        "+edited",
        "+untracked",
    ] {
        assert!(text.contains(needle), "{needle} in\n{text}");
    }
    assert!(!c.dir.exists());
}

#[test]
fn listing_and_staleness() {
    let e = env();
    e.trees
        .create(&seat("w1"), &task(3), &implement("three"))
        .unwrap();
    std::fs::create_dir_all(e.root.join("work/w1/demo-4/src")).unwrap();
    e.trees.ensure_inbox(&seat("w1")).unwrap();
    let l = e.trees.list().unwrap();
    let keys: Vec<String> = l.iter().map(|t| t.record.key()).collect();
    assert_eq!(keys, vec!["build-1.w1.demo-3", "build-1.w1.demo-4"]);
    assert!(
        l[0].file.is_some() && l[1].file.is_none(),
        "a partial tree is listed too"
    );
    let recs: Vec<TreeRecord> = l.into_iter().map(|t| t.record).collect();
    let now = chrono::Utc::now();
    let closed = |_: &str, n: u64, _: bool| Some(n == 4);
    let s = stale::stale(&recs, now, std::time::Duration::from_secs(3600), &closed);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].record.task.issue, 4);
    let later = now + chrono::Duration::hours(2);
    assert_eq!(
        stale::stale(
            &recs,
            later,
            std::time::Duration::from_secs(3600),
            &stale::Unknown
        )
        .len(),
        2
    );
}

#[test]
fn removal_refuses_a_link_out_of_work_root() {
    let e = env();
    let outside = e.root.join("outside");
    std::fs::create_dir_all(outside.join("src")).unwrap();
    std::fs::create_dir_all(e.root.join("work/w1")).unwrap();
    std::os::unix::fs::symlink(&outside, e.root.join("work/w1/demo-9")).unwrap();
    assert!(matches!(
        e.trees.remove(&seat("w1"), &task(9), false),
        Err(TreeError::Escape(_))
    ));
    assert!(outside.join("src").exists());
}

/// Carbon's check 7a (hgl-ai PR 1): a git killed in the tree left `index.lock`; the tree is
/// still salvaged and removed, and the lock no longer stops git in it.
#[test]
fn a_stale_index_lock_does_not_stop_the_removal() {
    let e = env();
    let c = e
        .trees
        .create(&seat("w1"), &task(1), &implement("One"))
        .unwrap();
    let src = c.dir.join("src");
    std::fs::write(src.join("dirty.txt"), "wip\n").unwrap();
    let gitdir = git_dir(&src).unwrap();
    std::fs::write(gitdir.join("index.lock"), "").unwrap();
    let rm = e
        .trees
        .remove(&seat("w1"), &task(1), false)
        .unwrap()
        .unwrap();
    let text = std::fs::read_to_string(rm.salvage.expect("salvaged")).unwrap();
    assert!(text.contains("+wip"), "{text}");
    assert!(!c.dir.exists());
    assert!(!gitdir.exists(), "the worktree's metadata went too");

    // A kept tree with a stale lock: cleared when the next turn's tree is prepared.
    let c = e
        .trees
        .create(&seat("w1"), &task(2), &implement("Two"))
        .unwrap();
    let lock = git_dir(&c.dir.join("src")).unwrap().join("index.lock");
    std::fs::write(&lock, "").unwrap();
    let again = e
        .trees
        .create(&seat("w1"), &task(2), &implement("Two"))
        .unwrap();
    assert!(again.reused);
    assert!(!lock.exists());
    assert!(again.record.size_bytes > 0, "a kept tree is measured too");
    // One a live process holds is left alone.
    std::fs::write(&lock, "").unwrap();
    let _held = std::fs::File::open(&lock).unwrap();
    assert!(!clear_stale_index_lock(&c.dir.join("src")));
    assert!(lock.exists());
}

/// Carbon's check 7b: the task moves to another seat of the same host (ROLE-15) while the
/// first still has its branch checked out: the new seat's tree shares the branch.
#[test]
fn a_task_moved_to_another_seat_of_the_host_keeps_its_branch() {
    let e = env();
    let w2 = e
        .trees
        .create(&seat("w2"), &task(3), &implement("Three"))
        .unwrap();
    commit(
        &e.git,
        &w2.dir.join("src"),
        "w2.txt",
        "w2 work\n",
        "w2 work",
    );
    let w1 = e
        .trees
        .create(&seat("w1"), &task(3), &implement("A sendback title"))
        .unwrap();
    assert_eq!(w1.file.branch, w2.file.branch);
    assert_eq!(w1.file.branch.as_deref(), Some("codex/q3-three"));
    assert!(
        w1.dir.join("src/w2.txt").exists(),
        "the unpushed work follows"
    );
    // The first seat's removal leaves the branch the second still uses.
    e.trees.remove(&seat("w2"), &task(3), false).unwrap();
    assert_eq!(
        e.git
            .run(&w1.dir.join("src"), ["branch", "--show-current"])
            .unwrap()
            .trim(),
        "codex/q3-three"
    );
}

/// Carbon's check 7c: branches named as docs/work.md says (`codex/q<issue>-<short-name>`)
/// are made and resumed, and so are older `codex/<issue>-<slug>` ones.
#[test]
fn q_branches_and_older_ones_are_resumed() {
    let e = env();
    for (issue, branch) in [(980, "codex/q980-fix"), (981, "codex/981-older")] {
        e.git
            .run(
                &e.seed,
                ["checkout", "--quiet", "-B", branch, "origin/main"],
            )
            .unwrap();
        let pushed = commit(&e.git, &e.seed, "q.txt", &format!("{branch}\n"), "q work");
        e.git
            .run(&e.seed, ["push", "--quiet", "origin", branch])
            .unwrap();
        let c = e
            .trees
            .create(&seat("w1"), &task(issue), &implement("Sendback title"))
            .unwrap();
        assert_eq!(c.file.branch.as_deref(), Some(branch));
        assert_eq!(c.file.start, pushed);
    }
}

/// hgl-ai#27: a new branch is named from the issue's title, every word kept; the title is
/// asked for only when no `q<issue>-*` branch exists, on the remote or on this host.
#[test]
fn only_a_new_branch_asks_for_the_issue_title() {
    let e = env();
    let asked = std::cell::Cell::new(0);
    let title = |t: &'static str| {
        let asked = &asked;
        move || {
            asked.set(asked.get() + 1);
            Some(t.to_string())
        }
    };
    let note = implement("Class: standard (docs/roles.md §3)");
    let c = e
        .trees
        .create_for(
            &seat("w1"),
            &task(7),
            &note,
            1,
            &title("[T; N] array literals"),
        )
        .unwrap();
    assert_eq!(
        c.file.branch.as_deref(),
        Some("codex/q7-t-n-array-literals")
    );
    assert_eq!(asked.get(), 1);

    // Unknown: the note's line names it.
    let c = e
        .trees
        .create_for(&seat("w1"), &task(8), &note, 2, &|| None)
        .unwrap();
    assert_eq!(
        c.file.branch.as_deref(),
        Some("codex/q8-class-standard-docs-roles-md-3")
    );

    // Pushed under another name: resumed, nothing asked.
    e.git
        .run(
            &e.seed,
            [
                "checkout",
                "--quiet",
                "-B",
                "codex/q9-old-title",
                "origin/main",
            ],
        )
        .unwrap();
    commit(&e.git, &e.seed, "q9.txt", "q9\n", "q9 work");
    e.git
        .run(&e.seed, ["push", "--quiet", "origin", "codex/q9-old-title"])
        .unwrap();
    let c = e
        .trees
        .create_for(&seat("w1"), &task(9), &note, 3, &title("New title"))
        .unwrap();
    assert_eq!(c.file.branch.as_deref(), Some("codex/q9-old-title"));
    assert_eq!(asked.get(), 1, "a pushed branch asks for no title");

    // Made on this host and not pushed (the task moved to another seat): kept, nothing asked.
    let c = e
        .trees
        .create_for(&seat("w2"), &task(7), &note, 4, &title("Renamed"))
        .unwrap();
    assert_eq!(
        c.file.branch.as_deref(),
        Some("codex/q7-t-n-array-literals")
    );
    assert_eq!(asked.get(), 1, "a local branch asks for no title");
}

/// Carbon's check 7d: a repository whose name cannot be a bucket key part (`.github`) is
/// refused before any tree is made.
#[test]
fn a_leading_dot_repository_is_refused_before_a_tree() {
    let e = env();
    remote(&e.git, &e.root, ".github");
    let t = TaskKey {
        repo: ".github".into(),
        issue: 5,
    };
    assert!(matches!(
        e.trees.create(&seat("w1"), &t, &implement("CI")),
        Err(TreeError::Invalid(_))
    ));
    assert!(!e.root.join("work/w1").join(".github-5").exists());
}

/// `git status --porcelain` of a tree's checkout.
fn status(e: &Env, src: &Path) -> String {
    e.git.run(src, ["status", "--porcelain"]).unwrap()
}

/// hgl-ai#55: a kept tree takes the commits the seat the task moved to pushed in between,
/// from another clone; a branch never pushed, or one that has diverged, is left as it is.
#[test]
fn a_kept_tree_fast_forwards_to_its_pushed_branch() {
    let e = env();
    let c = e
        .trees
        .create(&seat("w1"), &task(55), &implement("Kept"))
        .unwrap();
    let mut f = c.file.clone();
    assert_eq!(
        f.checkout.as_deref(),
        Some(f.start.as_str()),
        "made at start"
    );
    let src = c.dir.join("src");
    let head = |p: &Path| {
        e.git
            .run(p, ["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string()
    };
    assert_eq!(
        e.trees.refresh_branch(&c.dir, &mut f).unwrap(),
        None,
        "not pushed"
    );
    let branch = f.branch.clone().unwrap();
    commit(&e.git, &src, "w1.txt", "w1\n", "w1's work");
    e.git
        .run(
            &src,
            ["push", "--quiet", "origin", &format!("HEAD:{branch}")],
        )
        .unwrap();
    // The turn's end records where it left the files.
    write_task(&c.dir, &f).unwrap();
    e.trees.note_checkout(&c.dir).unwrap();
    let mut f = read_task(&c.dir).unwrap();
    assert_eq!(f.checkout, Some(head(&src)));
    // The interim seat, in another clone, pushes on the same branch.
    e.git.run(&e.seed, ["fetch", "--quiet", "origin"]).unwrap();
    e.git
        .run(
            &e.seed,
            [
                "checkout",
                "--quiet",
                "-b",
                &branch,
                &format!("origin/{branch}"),
            ],
        )
        .unwrap();
    let theirs = commit(&e.git, &e.seed, "w2.txt", "w2\n", "w2's work");
    e.git
        .run(&e.seed, ["push", "--quiet", "origin", &branch])
        .unwrap();
    // An uncommitted change of w1's own is kept.
    std::fs::write(src.join("mine.txt"), "mine\n").unwrap();
    assert_eq!(
        e.trees.refresh_branch(&c.dir, &mut f).unwrap(),
        Some(theirs.clone())
    );
    assert_eq!(
        (head(&src), f.checkout.clone()),
        (theirs.clone(), Some(theirs.clone()))
    );
    assert_eq!(std::fs::read_to_string(src.join("w2.txt")).unwrap(), "w2\n");
    assert_eq!(status(&e, &src), "?? mine.txt\n");
    std::fs::remove_file(src.join("mine.txt")).unwrap();
    assert_eq!(
        e.trees.refresh_branch(&c.dir, &mut f).unwrap(),
        None,
        "up to date"
    );
    // Diverged: both sides commit; the tree keeps its own.
    let ours = commit(&e.git, &src, "w1b.txt", "w1b\n", "w1 again");
    commit(&e.git, &e.seed, "w2b.txt", "w2b\n", "w2 again");
    e.git
        .run(&e.seed, ["push", "--quiet", "origin", &branch])
        .unwrap();
    assert_eq!(
        e.trees.refresh_branch(&c.dir, &mut f).unwrap(),
        None,
        "diverged"
    );
    assert_eq!(head(&src), ours);
    assert_eq!(status(&e, &src), "");
}

/// r1's major 1 on PR 75: two seats of one host share the branch's ref, so the other
/// seat's commit moves it under the kept tree. Its index and files catch up, with nothing
/// staged that would revert that commit, and its own uncommitted changes are kept; a change
/// the catch-up would overwrite stops it, and the tree is left as it was.
#[test]
fn a_kept_tree_catches_up_with_a_ref_another_tree_of_its_host_moved() {
    let e = env();
    let w1 = e
        .trees
        .create(&seat("w1"), &task(56), &implement("Shared"))
        .unwrap();
    let src1 = w1.dir.join("src");
    commit(&e.git, &src1, "a.txt", "w1\n", "w1's work");
    e.trees.note_checkout(&w1.dir).unwrap();
    let w2 = e
        .trees
        .create(&seat("w2"), &task(56), &implement("Shared"))
        .unwrap();
    let src2 = w2.dir.join("src");
    std::fs::write(src1.join("mine.txt"), "mine\n").unwrap();
    std::fs::write(src1.join("a.txt"), "w1, uncommitted\n").unwrap();
    let theirs = commit(&e.git, &src2, "b.txt", "w2\n", "w2's work");
    assert_eq!(
        e.git.run(&src1, ["rev-parse", "HEAD"]).unwrap().trim(),
        theirs,
        "the ref moved under w1"
    );
    assert!(
        status(&e, &src1).contains("D  b.txt"),
        "a staged revert, before"
    );
    let mut f = read_task(&w1.dir).unwrap();
    assert_eq!(
        e.trees.refresh_branch(&w1.dir, &mut f).unwrap(),
        Some(theirs.clone())
    );
    assert_eq!(f.checkout.as_deref(), Some(theirs.as_str()));
    assert_eq!(std::fs::read_to_string(src1.join("b.txt")).unwrap(), "w2\n");
    assert_eq!(
        status(&e, &src1),
        " M a.txt\n?? mine.txt\n",
        "only w1's own changes"
    );
    e.git.run(&src1, ["checkout", "--", "a.txt"]).unwrap();
    std::fs::remove_file(src1.join("mine.txt")).unwrap();
    assert_eq!(status(&e, &src1), "");
    write_task(&w1.dir, &f).unwrap();

    // w2 changes a file w1 has an uncommitted change in: no catch-up, nothing lost.
    std::fs::write(src1.join("b.txt"), "w1's own\n").unwrap();
    commit(&e.git, &src2, "b.txt", "w2 again\n", "w2 again");
    let mut f = read_task(&w1.dir).unwrap();
    assert!(e.trees.refresh_branch(&w1.dir, &mut f).is_err());
    assert_eq!(
        f.checkout.as_deref(),
        Some(theirs.as_str()),
        "left as it was"
    );
    assert_eq!(
        std::fs::read_to_string(src1.join("b.txt")).unwrap(),
        "w1's own\n"
    );
}
