//! hgl-ai#55: a task moved back into its kept tree.

use super::*;

/// A sendback moves the task to w2 at a higher class; w2 pushes; a sendback without a
/// class moves it back into w1's kept tree. w1's tree takes w2's class, at its sequence,
/// and w2's commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_moved_back_takes_the_newest_class_and_the_pushed_commits() {
    let mut h = harness();
    let mut w2 = h.cfg.seats[0].clone();
    w2.name = seat("w2");
    h.cfg.seats.push(w2);
    h.cfg.validate().unwrap();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let push = |src: &std::path::Path, branch: &str| {
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
    };

    let (tx, j) = h.start(&["w1"]);
    let mut n = note(Kind::Assign, "w1", 55, "[claude] Assign: demo#55 Keep it");
    n.class = Some(TaskClass::Simple);
    let a = h.bus.push("w1", n);
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    let dir1 = h.work().join("w1/demo-55");
    let branch = hgl_ai_tree::read_task(&dir1).unwrap().branch.unwrap();
    commit(&h.git, &dir1.join("src"), "w1.txt", "w1\n", "w1's work");
    push(&dir1.join("src"), &branch);
    stop(tx, j).await;

    let (tx, j) = h.start(&["w2"]);
    let mut sb = note(Kind::Sendback, "w2", 55, "[claude] Sent back: demo#55");
    sb.class = Some(TaskClass::Complex);
    let s = h.bus.push("w2", sb);
    eventually("sendback acked", || h.bus.events(s) == [Ev::Ack]).await;
    let src2 = h.work().join("w2/demo-55/src");
    let theirs = commit(&h.git, &src2, "w2.txt", "w2\n", "w2's work");
    push(&src2, &branch);
    stop(tx, j).await;

    let reports = h.bus.sent_containing("work_root uses").len();
    let calls = h.calls("codex").len();
    let (tx, j) = h.start(&["w1"]);
    eventually("startup report", || {
        h.bus.sent_containing("work_root uses").len() > reports
    })
    .await;
    let back = h.bus.push(
        "w1",
        note(Kind::Sendback, "w1", 55, "[claude] Sent back: demo#55"),
    );
    eventually("sendback to w1 acked", || h.bus.events(back) == [Ev::Ack]).await;
    let f = hgl_ai_tree::read_task(&dir1).unwrap();
    assert_eq!(
        (f.class, f.class_seq, f.handoff_seq),
        (Some(TaskClass::Complex), Some(s), Some(back)),
        "w2's class, at its sequence"
    );
    let w1 = &h.bus.trees()["build-1.w1.demo-55"];
    assert_eq!(
        (w1.class, w1.class_seq),
        (Some(TaskClass::Complex), Some(s))
    );
    assert_eq!(
        h.git
            .run(&dir1.join("src"), ["rev-parse", "HEAD"])
            .unwrap()
            .trim(),
        theirs,
        "w2's pushed commits"
    );
    assert!(
        h.calls("codex")[calls..]
            .iter()
            .any(|c| c.stdin.contains("\nTask class: complex.\n"))
    );
    stop(tx, j).await;

    // A later sendback below that class leaves it, and its sequence, where it is.
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
