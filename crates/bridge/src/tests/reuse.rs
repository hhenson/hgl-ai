//! hgl-ai#55: a task moved back into its kept tree. Two trees of one host never share a
//! local branch (the controller's decision on PR 75): each moves only by its own actions.

use super::*;

/// What a move to w2 and back leaves: the harness, the task's branch, w1's tree, w1's
/// pushed commit, w2's pushed commit, and the sequence of the sendback that moved it.
struct Moved {
    h: H,
    branch: String,
    dir1: PathBuf,
    src2: PathBuf,
    theirs: String,
    s: u64,
}

fn push(h: &H, src: &std::path::Path, branch: &str) {
    h.git
        .run(
            src,
            [
                "push",
                "--quiet",
                "origin",
                &format!("HEAD:refs/heads/{branch}"),
            ],
        )
        .unwrap();
}

fn head(h: &H, src: &std::path::Path) -> String {
    h.git
        .run(src, ["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string()
}

fn status(h: &H, src: &std::path::Path) -> String {
    h.git.run(src, ["status", "--porcelain"]).unwrap()
}

/// w1 takes demo#55 at simple, commits and pushes; a sendback moves it to w2, on this host,
/// at complex; w2 commits on a local branch of its own and pushes. w1's tree is untouched.
async fn moved_to_w2() -> Moved {
    let mut h = harness();
    h.opts.prep_retry = Duration::from_millis(100);
    let mut w2 = h.cfg.seats[0].clone();
    w2.name = seat("w2");
    h.cfg.seats.push(w2);
    h.cfg.validate().unwrap();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));

    let (tx, j) = h.start(&["w1"]);
    let mut n = note(Kind::Assign, "w1", 55, "[claude] Assign: demo#55 Keep it");
    n.class = Some(TaskClass::Simple);
    let a = h.bus.push("w1", n);
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    let dir1 = h.work().join("w1/demo-55");
    let src1 = dir1.join("src");
    let branch = hgl_ai_tree::read_task(&dir1).unwrap().branch.unwrap();
    commit(&h.git, &src1, "w1.txt", "w1\n", "w1's work");
    push(&h, &src1, &branch);
    let mine = head(&h, &src1);
    stop(tx, j).await;

    let (tx, j) = h.start(&["w2"]);
    let mut sb = note(Kind::Sendback, "w2", 55, "[claude] Sent back: demo#55");
    sb.class = Some(TaskClass::Complex);
    let s = h.bus.push("w2", sb);
    eventually("sendback acked", || h.bus.events(s) == [Ev::Ack]).await;
    let dir2 = h.work().join("w2/demo-55");
    let f2 = hgl_ai_tree::read_task(&dir2).unwrap();
    let own = format!("{branch}@w2");
    assert_eq!(
        (f2.branch.as_deref(), f2.local()),
        (Some(branch.as_str()), Some(own.as_str())),
        "a local branch of its own, pushing to the task's"
    );
    assert!(
        h.calls("codex")
            .last()
            .unwrap()
            .stdin
            .contains(&format!("push with `git push origin HEAD:{branch}`")),
        "the prompt names the push"
    );
    let src2 = dir2.join("src");
    let theirs = commit(&h.git, &src2, "w2.txt", "w2\n", "w2's work");
    push(&h, &src2, &branch);
    stop(tx, j).await;
    assert_eq!(head(&h, &src1), mine, "w2's commit did not move w1");
    assert_eq!(status(&h, &src1), "");
    Moved {
        h,
        branch,
        dir1,
        src2,
        theirs,
        s,
    }
}

/// Start w1 after its startup report, whose listing would otherwise race the turn's record.
async fn start_w1(
    h: &H,
) -> (
    watch::Sender<bool>,
    tokio::task::JoinHandle<crate::Result<()>>,
) {
    let reports = h.bus.sent_containing("work_root uses").len();
    let (tx, j) = h.start(&["w1"]);
    eventually("startup report", || {
        h.bus.sent_containing("work_root uses").len() > reports
    })
    .await;
    (tx, j)
}

fn back(h: &H) -> u64 {
    h.bus.push(
        "w1",
        note(Kind::Sendback, "w1", 55, "[claude] Sent back: demo#55"),
    )
}

/// The move back: w1's kept tree takes w2's class, at its sequence, and w2's commits, in
/// its index and files; a later sendback below that class leaves it where it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_moved_back_takes_the_newest_class_and_the_pushed_commits() {
    let Moved {
        h, dir1, theirs, s, ..
    } = moved_to_w2().await;
    let calls = h.calls("codex").len();
    let (tx, j) = start_w1(&h).await;
    let b = back(&h);
    eventually("sendback to w1 acked", || h.bus.events(b) == [Ev::Ack]).await;
    let f = hgl_ai_tree::read_task(&dir1).unwrap();
    assert_eq!(
        (f.class, f.class_seq, f.handoff_seq),
        (Some(TaskClass::Complex), Some(s), Some(b)),
        "w2's class, at its sequence"
    );
    let w1 = &h.bus.trees()["build-1.w1.demo-55"];
    assert_eq!(
        (w1.class, w1.class_seq),
        (Some(TaskClass::Complex), Some(s))
    );
    let src1 = dir1.join("src");
    assert_eq!(head(&h, &src1), theirs, "w2's pushed commits");
    assert_eq!(
        std::fs::read_to_string(src1.join("w2.txt")).unwrap(),
        "w2\n"
    );
    assert_eq!(status(&h, &src1), "");
    assert!(
        h.calls("codex")[calls..]
            .iter()
            .any(|c| c.stdin.contains("\nTask class: complex.\n"))
    );
    stop(tx, j).await;

    let (tx, j) = h.start(&["w1"]);
    let mut low = note(Kind::Sendback, "w1", 55, "[claude] Sent back: demo#55");
    low.class = Some(TaskClass::Standard);
    let l = h.bus.push("w1", low);
    eventually("lower sendback acked", || h.bus.events(l) == [Ev::Ack]).await;
    let f = hgl_ai_tree::read_task(&dir1).unwrap();
    assert_eq!(
        (f.class, f.class_seq, f.handoff_seq),
        (Some(TaskClass::Complex), Some(s), Some(l))
    );
    stop(tx, j).await;
}

/// w2's push lands while w1's turn runs: w1's tree does not move under it, and the next
/// handoff brings it to that push.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_during_the_turn_waits_for_the_next_handoff() {
    let Moved {
        h,
        branch,
        dir1,
        src2,
        theirs,
        ..
    } = moved_to_w2().await;
    let later = commit(&h.git, &src2, "w2b.txt", "w2b\n", "w2 later");
    h.script(
        "codex",
        &format!(
            "if [ \"$HGL_AI_SEAT\" = w1 ]; then GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null git -C '{}' push --quiet origin HEAD:refs/heads/{branch}; fi\ncat '{}'",
            src2.display(),
            fixture("codex-ok.jsonl")
        ),
    );
    let src1 = dir1.join("src");
    let (tx, j) = start_w1(&h).await;
    let b = back(&h);
    eventually("sendback to w1 acked", || h.bus.events(b) == [Ev::Ack]).await;
    let pushed = h
        .git
        .run(
            &src2,
            ["ls-remote", "origin", &format!("refs/heads/{branch}")],
        )
        .unwrap();
    assert!(pushed.starts_with(&later), "pushed mid-turn: {pushed}");
    assert_eq!(head(&h, &src1), theirs, "not moved under the turn");
    assert!(!src1.join("w2b.txt").exists());
    assert_eq!(status(&h, &src1), "");
    let b2 = back(&h);
    eventually("next sendback acked", || h.bus.events(b2) == [Ev::Ack]).await;
    assert_eq!(head(&h, &src1), later);
    assert_eq!(
        std::fs::read_to_string(src1.join("w2b.txt")).unwrap(),
        "w2b\n"
    );
    assert_eq!(status(&h, &src1), "");
    stop(tx, j).await;
}

/// The bot's P1 and r1's major 1 on PR 75: w1 has an unpushed commit to a file w2's push
/// also changed. The handoff starts no turn on the stale tree: it is held, not acked, and
/// the controller is told once with a `blocked` note naming the tree and the file. Once w1's
/// tree is put right, the held note runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conflicting_unpushed_change_blocks_the_handoff() {
    let Moved { h, dir1, .. } = moved_to_w2().await;
    let src1 = dir1.join("src");
    let ours = commit(&h.git, &src1, "w2.txt", "w1's own\n", "w1 unpushed");
    let calls = h.calls("codex").len();
    let (tx, j) = start_w1(&h).await;
    let b = back(&h);
    let blocked = || {
        h.bus
            .sent()
            .into_iter()
            .filter(|n| n.kind == Kind::Blocked)
            .collect::<Vec<_>>()
    };
    eventually("blocked", || !blocked().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(h.bus.events(b).is_empty(), "held: {:?}", h.bus.events(b));
    let told = blocked();
    assert_eq!(told.len(), 1, "told once");
    assert_eq!(told[0].recipient.as_str(), "claude", "the controller");
    let body = &told[0].body;
    assert!(body.contains(&dir1.display().to_string()), "{body}");
    assert!(
        body.contains("diverged") && body.contains("(w2.txt)"),
        "{body}"
    );
    assert_eq!(h.calls("codex").len(), calls, "no turn on the stale tree");
    assert_eq!(head(&h, &src1), ours);
    assert_eq!(
        std::fs::read_to_string(src1.join("w2.txt")).unwrap(),
        "w1's own\n"
    );
    assert_eq!(status(&h, &src1), "");
    // Put right by hand: the held note runs.
    h.git
        .run(&src1, ["reset", "--quiet", "--hard", "HEAD~"])
        .unwrap();
    eventually("acked once it heals", || h.bus.events(b) == [Ev::Ack]).await;
    assert_eq!(
        std::fs::read_to_string(src1.join("w2.txt")).unwrap(),
        "w2\n"
    );
    stop(tx, j).await;
}
