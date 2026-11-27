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
        .create(
            &seat("w1"),
            &task(980),
            &implement("[claude] Assign: demo#980 Fix the thing"),
        )
        .unwrap();
    assert!(!c.reused);
    let work = e.root.join("work");
    assert_eq!(c.dir, work.join("w1/demo-980"));
    for sub in ["src/README", "tmp", "build", "task.json"] {
        assert!(c.dir.join(sub).exists(), "{sub}");
    }
    assert_eq!(c.file.branch.as_deref(), Some("codex/980-fix-the-thing"));
    assert_eq!(c.file.base, "origin/main");
    assert_eq!(c.file.start, c.file.main.clone().unwrap());
    let src = c.dir.join("src");
    assert_eq!(
        e.git
            .run(&src, ["branch", "--show-current"])
            .unwrap()
            .trim(),
        "codex/980-fix-the-thing"
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
        .create(&seat("w1"), &task(980), &implement("other title"))
        .unwrap();
    assert!(again.reused);
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
    assert_eq!(rm.branch.as_deref(), Some("codex/1-one"));
    assert!(rm.freed_bytes > 0);
    assert!(!c.dir.exists());
    let clone = e.root.join("work/clones/demo");
    assert!(
        !e.git
            .check(
                &clone,
                ["rev-parse", "--verify", "--quiet", "refs/heads/codex/1-one"]
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
