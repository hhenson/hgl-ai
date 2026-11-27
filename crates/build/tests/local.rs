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
                        shell: false,
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

/// Carbon's second review of #12, finding 8: a step sees every SDK the builder has, not
/// only the ones it needs (an optional prefix, as `recipes/hgraph.toml` reads).
#[test]
fn every_configured_sdk_reaches_a_step() {
    use hgl_ai_build::driver::{self, Meta, Options};
    use hgl_ai_build::exec::{Cancel, Workspace};
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let ex = Local::new(
        tmp.path().join("builder"),
        BTreeMap::from([("hgraph-deps".to_string(), "/opt/hgraph-deps".to_string())]),
        Git::default(),
    );
    let recipe = hgl_ai_build::Recipe::parse(
        "repo = \"demo\"\n[[step]]\nname = \"configure\"\nrun = 'echo \"deps=${HGL_SDK_HGRAPH_DEPS:-none}\"'\nmemcap_gb = 1\n",
    )
    .unwrap();
    let ws = Workspace {
        src: src.display().to_string(),
        build: tmp.path().join("build").display().to_string(),
        tmp: tmp.path().display().to_string(),
        job: None,
        clone: None,
    };
    let logs = tmp.path().join("logs");
    let opts = Options {
        log_dir: logs.clone(),
        ..Options::default()
    };
    let d = driver::drive(
        &ex,
        &ws,
        &recipe,
        &Meta::default(),
        &opts,
        &Cancel::default(),
        &mut |_| {},
    );
    assert_eq!(d.summary.status.as_str(), "pass");
    assert_eq!(
        std::fs::read_to_string(logs.join("configure.log")).unwrap(),
        "deps=/opt/hgraph-deps\n"
    );
}

/// Finding 9: pruning old logs keeps a build that has not ended.
#[test]
fn pruning_keeps_unfinished_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let logs = tmp.path();
    for (id, done) in [
        ("20261127-100000-000000", false),
        ("20261127-110000-000000", true),
        ("20261127-120000-000000", true),
    ] {
        let d = logs.join(id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("request.json"), "{}").unwrap();
        if done {
            std::fs::write(d.join("summary.json"), "{}").unwrap();
        }
    }
    hgl_ai_build::service::prune_logs(logs, 1);
    assert!(
        logs.join("20261127-100000-000000").exists(),
        "still running"
    );
    assert!(!logs.join("20261127-110000-000000").exists());
    assert!(logs.join("20261127-120000-000000").exists());
}
