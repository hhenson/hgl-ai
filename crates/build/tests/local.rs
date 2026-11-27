//! The local executor's checkouts: several at once on one shared clone (a builder with
//! `concurrency > 1`, or one beside `build shell`).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use hgl_ai_build::exec::local::Local;
use hgl_ai_build::exec::{Checkout, Executor};
use hgl_ai_tree::Git;

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// Carbon's finding 6 on #12: concurrent checkouts and removals on one bare clone.
#[test]
fn concurrent_checkouts_share_a_clone() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let bare = root.join("origin.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "--quiet", "--bare", "-b", "main"]);
    git(root, &["clone", "--quiet", bare.to_str().unwrap(), "seed"]);
    let seed = root.join("seed");
    git(&seed, &["checkout", "--quiet", "-b", "main"]);
    std::fs::write(seed.join("f.txt"), "x\n").unwrap();
    git(&seed, &["add", "f.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "c"]);
    git(&seed, &["push", "--quiet", "origin", "main"]);
    let head = git(&seed, &["rev-parse", "HEAD"]);
    let ex = Arc::new(Local::new(
        root.join("builder"),
        BTreeMap::new(),
        Git::default(),
    ));
    for round in 0..3 {
        let handles: Vec<_> = (0..6)
            .map(|i| {
                let (ex, head, url) = (ex.clone(), head.clone(), bare.display().to_string());
                std::thread::spawn(move || {
                    let c = Checkout {
                        id: format!("20261127-12000{round}-00000{i}"),
                        repo: "demo".into(),
                        url,
                        head,
                        base: Some("main".into()),
                        issue: 0,
                    };
                    let ws = ex.prepare(&c).map_err(|e| format!("{e:?}"))?;
                    assert!(Path::new(&ws.src).join("f.txt").is_file());
                    ex.cleanup(&ws).map_err(|e| format!("{e:?}"))
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().unwrap();
        }
    }
    let wt = git(&root.join("builder/clones/demo"), &["worktree", "list"]);
    assert_eq!(wt.lines().count(), 1, "{wt}");
}
