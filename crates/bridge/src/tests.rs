//! The seat loop against the fake bus, a local bare "remote", and fake agent CLIs
//! (shell scripts that print fixture output).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hgl_ai_config::{Binding, Config, SeatConfig};
use hgl_ai_proto::{Family, Kind, Note, Role, Runtime, Seat, TaskClass, TaskKey, Tier};
use hgl_ai_tree::testing::{commit, git, remote};
use hgl_ai_tree::{Git, Layout, Trees};
use tokio::sync::watch;

use crate::fake::{Ev, FakeBus, FixedDisk};
use crate::lock::FileLock;
use crate::{Bridge, CapMethod, Deps, Options};

const THREAD: &str = "00000000-0000-4000-8000-000000000002";

fn fixture(name: &str) -> String {
    format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn seat(s: &str) -> Seat {
    Seat::new(s).unwrap()
}

struct H {
    _dir: tempfile::TempDir,
    root: PathBuf,
    git: Git,
    seed: PathBuf,
    bus: FakeBus,
    bin: PathBuf,
    calls: PathBuf,
    cfg: Config,
    opts: Options,
    disk: Arc<FixedDisk>,
    /// Issue titles by number, as GitHub would answer; an issue not here is unknown.
    titles: Arc<std::sync::Mutex<HashMap<u64, Result<String, String>>>>,
    /// The issues whose title was asked for, in order.
    asked: Arc<std::sync::Mutex<Vec<u64>>>,
    title: Arc<dyn crate::IssueTitle + Send + Sync>,
}

struct Call {
    args: Vec<String>,
    cwd: String,
    env: HashMap<String, String>,
    stdin: String,
}

fn harness() -> H {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let git = git(&root);
    let (_bare, seed) = remote(&git, &root, "demo");
    let mut cfg = Config::new("build-1");
    cfg.github.controller = "claude".into();
    cfg.paths.work_root = root.join("work").display().to_string();
    cfg.paths.data_dir = root.join("data").display().to_string();
    cfg.github.clone_url = format!("{}/{{repo}}.git", root.display());
    let b = |runtime, family, tier, effort: Option<&str>| Binding {
        runtime,
        family,
        tier: Some(tier),
        model: None,
        effort: effort.map(String::from),
    };
    cfg.seats = vec![
        SeatConfig {
            name: seat("w1"),
            role: Role::Implementor,
            binding: b(Runtime::Codex, None, Tier::Frontier, Some("high")),
            fallback: vec![b(Runtime::Agy, Some(Family::Claude), Tier::Frontier, None)],
            memcap_gb: None,
            headless: false,
        },
        SeatConfig {
            name: seat("r1"),
            role: Role::Reviewer,
            binding: b(Runtime::Claude, None, Tier::Frontier, Some("xhigh")),
            fallback: vec![],
            memcap_gb: None,
            headless: false,
        },
    ];
    cfg.validate().unwrap();
    let bin = root.join("bin");
    let calls = root.join("calls");
    std::fs::create_dir_all(&bin).unwrap();
    let mut opts = Options::from_config(&cfg);
    opts.fetch_wait = Duration::from_millis(100);
    opts.nak_delay = Duration::from_millis(50);
    opts.busy_retry = Duration::from_millis(200);
    opts.in_progress_every = Duration::from_millis(100);
    opts.heartbeat_every = Duration::from_millis(100);
    opts.report_every = Duration::from_secs(3600);
    opts.seat_retry = Duration::from_millis(100);
    opts.turn_timeout = Duration::from_secs(20);
    opts.shutdown_grace = Duration::from_millis(300);
    opts.kill_grace = Duration::from_millis(300);
    opts.max_line = 1 << 20;
    opts.cap_method = Some(CapMethod::Rlimit);
    for rt in Runtime::ALL {
        opts.bins.insert(*rt, bin.join(rt.as_str()));
    }
    let disk = Arc::new(FixedDisk(AtomicU64::new(1000 * crate::disk::GB)));
    let titles: Arc<std::sync::Mutex<HashMap<u64, Result<String, String>>>> = Arc::default();
    let asked: Arc<std::sync::Mutex<Vec<u64>>> = Arc::default();
    let (t, a) = (titles.clone(), asked.clone());
    let title = Arc::new(move |repo: &str, issue: u64| {
        if repo != "demo" {
            return Err(format!("gh: no repository {repo}"));
        }
        a.lock().unwrap().push(issue);
        t.lock()
            .unwrap()
            .get(&issue)
            .cloned()
            .unwrap_or_else(|| Err("gh: Could not resolve to an issue".into()))
    });
    let h = H {
        _dir: dir,
        root,
        git,
        seed,
        bus: FakeBus::default(),
        bin,
        calls,
        cfg,
        opts,
        disk,
        titles,
        asked,
        title,
    };
    for rt in ["claude", "codex", "agy"] {
        h.script(rt, "echo no fake set >&2; exit 9");
    }
    h
}

impl H {
    /// A fake CLI: records its args, cwd, environment and stdin, then runs `body` (with
    /// `$n` its call number).
    fn script(&self, rt: &str, body: &str) {
        let c = self.calls.join(rt);
        std::fs::create_dir_all(&c).unwrap();
        let text = format!(
            "#!/bin/sh\nC='{c}'\nn=$(cat \"$C/n\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"$C/n.tmp\"; mv \"$C/n.tmp\" \"$C/n\"\nd=\"$C/$n\"; mkdir -p \"$d\"\nprintf '%s\\n' \"$@\" > \"$d/args\"\npwd > \"$d/cwd\"\nenv > \"$d/env\"\ncat > \"$d/stdin\"\ntouch \"$d/ready\"\n{body}\n",
            c = c.display()
        );
        let p = self.bin.join(rt);
        std::fs::write(&p, text).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn calls(&self, rt: &str) -> Vec<Call> {
        let c = self.calls.join(rt);
        let n: usize = std::fs::read_to_string(c.join("n"))
            .map(|s| s.trim().parse().unwrap())
            .unwrap_or(0);
        (1..=n)
            .filter_map(|i| {
                let d = c.join(i.to_string());
                if !d.join("ready").exists() {
                    return None;
                }
                let args = std::fs::read_to_string(d.join("args"))
                    .ok()?
                    .lines()
                    .map(String::from)
                    .collect();
                let cwd = std::fs::read_to_string(d.join("cwd"))
                    .ok()?
                    .trim()
                    .to_string();
                let env = std::fs::read_to_string(d.join("env"))
                    .ok()?
                    .lines()
                    .filter_map(|l| {
                        l.split_once('=')
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                    })
                    .collect();
                let stdin = std::fs::read_to_string(d.join("stdin")).ok()?;
                Some(Call {
                    args,
                    cwd,
                    env,
                    stdin,
                })
            })
            .collect()
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn start(
        &self,
        only: &[&str],
    ) -> (
        watch::Sender<bool>,
        tokio::task::JoinHandle<crate::Result<()>>,
    ) {
        self.start_with(&self.cfg, only)
    }

    /// The configuration of another host, `host`, on the same hub: its own work root and
    /// data directory.
    fn other_host(&self, host: &str) -> Config {
        let mut cfg = self.cfg.clone();
        cfg.host = host.into();
        cfg.paths.work_root = self.root.join(host).join("work").display().to_string();
        cfg.paths.data_dir = self.root.join(host).join("data").display().to_string();
        cfg
    }

    fn start_with(
        &self,
        cfg: &Config,
        only: &[&str],
    ) -> (
        watch::Sender<bool>,
        tokio::task::JoinHandle<crate::Result<()>>,
    ) {
        let trees = Trees::with(
            Layout::new(cfg.paths.work_root.clone()).unwrap(),
            &cfg.host,
            &cfg.github,
            self.git.clone(),
        );
        let deps = Deps {
            closed: Arc::new(|_: &str, _: u64, _: bool| None),
            title: self.title.clone(),
            disk: self.disk.clone(),
        };
        let bridge = Bridge::new(
            cfg.clone(),
            self.bus.clone(),
            trees,
            deps,
            self.opts.clone(),
        );
        let (tx, rx) = watch::channel(false);
        let only: Vec<Seat> = only.iter().map(|s| seat(s)).collect();
        let h = tokio::spawn(async move { bridge.run(&only, rx).await });
        (tx, h)
    }
}

async fn stop(tx: watch::Sender<bool>, h: tokio::task::JoinHandle<crate::Result<()>>) {
    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(15), h)
        .await
        .expect("bridge stops")
        .unwrap()
        .unwrap();
}

async fn eventually(what: &str, f: impl Fn() -> bool) {
    let end = std::time::Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(
            std::time::Instant::now() < end,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn note(kind: Kind, to: &str, issue: u64, body: &str) -> Note {
    let mut n = Note::new(seat("claude"), seat(to), kind, body);
    n.repo = Some("demo".into());
    n.issue = issue;
    if kind.needs_head() {
        n.head = Some("a".repeat(40));
    }
    n
}

use crate::proc::alive;

/// r1's minor 5 and r3's finding 3 on hgl-ai PR 25: a tree made anew for a task begun
/// elsewhere (another host, another seat) takes the class of the record its notes set last,
/// by stream sequence, not by tree creation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recreated_tree_keeps_the_class_from_its_record() {
    use hgl_ai_tree::Registry as _;
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let task = TaskKey {
        repo: "demo".into(),
        issue: 11,
    };
    let rec =
        |host: &str, who: &str, review, created: &str, class, class_seq| hgl_ai_proto::TreeRecord {
            host: host.into(),
            seat: seat(who),
            task: task.clone(),
            review,
            path: "elsewhere".into(),
            branch: None,
            created_at: created.into(),
            size_bytes: 0,
            families: vec![],
            class,
            class_seq,
            seq: None,
            handoff_seq: None,
        };
    // The task was moved from w2, where a sendback made it complex (seq 50), to w1 on
    // another host, where a later-made tree still says simple from its assign (seq 10).
    let older = "20261127T100000Z";
    let newer = "20261127T110000Z";
    for r in [
        rec(
            "build-0",
            "w2",
            false,
            older,
            Some(TaskClass::Complex),
            Some(50),
        ),
        rec(
            "build-2",
            "w1",
            false,
            newer,
            Some(TaskClass::Simple),
            Some(10),
        ),
        rec(
            "build-3",
            "r1",
            true,
            newer,
            Some(TaskClass::Simple),
            Some(60),
        ),
    ] {
        h.bus.tree_put(&r).await.unwrap();
    }
    let (tx, j) = h.start(&["w1"]);
    let s = h.bus.push(
        "w1",
        note(Kind::Sendback, "w1", 11, "[claude] Sendback: demo#11"),
    );
    eventually("sendback acked", || h.bus.events(s) == [Ev::Ack]).await;
    let dir = h.work().join("w1/demo-11");
    assert_eq!(
        hgl_ai_tree::read_task(&dir).unwrap().class,
        Some(TaskClass::Complex)
    );
    assert!(h.calls("codex")[0].stdin.contains("Task class: complex"));
    stop(tx, j).await;
}

/// r3's blocker on hgl-ai PR 25 (ROLE-15 in one step): `gh sendback --class C --to w2`
/// moves the task to w2. Its bridge makes w2's tree on the PR branch at class C, from the
/// sendback alone, while w1 is offline; w1 gets no note. The new tree's record is the newer
/// one, so verdicts without `--to` reach only w2 (PROTO-21).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sendback_to_another_seat_moves_the_task_there_at_its_class() {
    let mut h = harness();
    let mut w2 = h.cfg.seats[0].clone();
    w2.name = seat("w2");
    h.cfg.seats.push(w2);
    h.cfg.validate().unwrap();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let mut n = note(Kind::Assign, "w1", 41, "[claude] Assign: demo#41 Rename it");
    n.class = Some(TaskClass::Simple);
    let a = h.bus.push("w1", n);
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    // w1 pushes its branch (the pull request's), then goes offline.
    let src1 = h.work().join("w1/demo-41/src");
    let branch = hgl_ai_tree::read_task(&h.work().join("w1/demo-41"))
        .unwrap()
        .branch
        .unwrap();
    let head = commit(&h.git, &src1, "w.txt", "work\n", "w1's work");
    h.git
        .run(
            &src1,
            [
                "push",
                "--quiet",
                "origin",
                &format!("HEAD:refs/heads/{branch}"),
            ],
        )
        .unwrap();
    stop(tx, j).await;
    let sent_before = h.bus.sent().len();

    let (tx, j) = h.start(&["w2"]);
    let mut sb = note(
        Kind::Sendback,
        "w2",
        41,
        "[claude] Sent back: demo#41 at 1f461fa7",
    );
    sb.class = Some(TaskClass::Standard);
    let s = h.bus.push("w2", sb);
    eventually("sendback acked", || h.bus.events(s) == [Ev::Ack]).await;
    let dir2 = h.work().join("w2/demo-41");
    let f = hgl_ai_tree::read_task(&dir2).unwrap();
    assert_eq!((f.class, f.class_seq), (Some(TaskClass::Standard), Some(s)));
    assert_eq!(f.branch.as_deref(), Some(branch.as_str()), "the PR branch");
    assert_eq!(
        h.git
            .run(&dir2.join("src"), ["rev-parse", "HEAD"])
            .unwrap()
            .trim(),
        head
    );
    assert!(
        h.calls("codex")[1]
            .stdin
            .contains("\nTask class: standard.\n")
    );
    assert!(
        h.bus.sent()[sent_before..]
            .iter()
            .all(|n| n.recipient.as_str() != "w1"),
        "w1 gets no note"
    );
    let trees = h.bus.trees();
    assert_eq!(trees["build-1.w1.demo-41"].seq, Some(a));
    assert_eq!(
        trees["build-1.w2.demo-41"].seq,
        Some(s),
        "w2's tree is the newer"
    );
    assert_eq!(trees["build-1.w2.demo-41"].class, Some(TaskClass::Standard));
    assert_eq!(trees["build-1.w2.demo-41"].handoff_seq, Some(s));
    stop(tx, j).await;

    // hgl-ai#44: a sendback moves the task back to w1, into its older tree, kept: that
    // tree now holds the newest handoff, so verdicts without `--to` reach w1 again.
    let (tx, j) = h.start(&["w1"]);
    let back = h.bus.push(
        "w1",
        note(
            Kind::Sendback,
            "w1",
            41,
            "[claude] Sent back: demo#41 at 1f461fa7",
        ),
    );
    eventually("sendback to w1 acked", || h.bus.events(back) == [Ev::Ack]).await;
    let trees = h.bus.trees();
    let w1 = &trees["build-1.w1.demo-41"];
    assert_eq!(
        (w1.seq, w1.handoff_seq),
        (Some(a), Some(back)),
        "the kept tree keeps its seq and takes the newer handoff"
    );
    assert!(w1.handoff_seq > trees["build-1.w2.demo-41"].handoff_seq);
    stop(tx, j).await;
}

/// hgl-ai#51: the report lists the trees, then a handoff turn registers a fresher record
/// before the report writes its listing. The report keeps the turn's `handoff_seq`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_report_listed_before_a_handoff_turn_keeps_the_turns_record() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 41, "[claude] Assign: demo#41 Rename it"),
    );
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    stop(tx, j).await;
    let key = "build-1.w1.demo-41";
    assert_eq!(h.bus.trees()[key].handoff_seq, Some(a));

    // The startup report has listed the tree (handoff `a`) and waits to write it.
    h.bus.hold_tree_merges();
    let reports = h.bus.sent_containing("work_root uses").len();
    let (tx, j) = h.start(&["w1"]);
    eventually("the report holds its listing", || {
        h.bus.tree_merges_held() == 1
    })
    .await;
    let back = h.bus.push(
        "w1",
        note(
            Kind::Sendback,
            "w1",
            41,
            "[claude] Sent back: demo#41 at 1f461fa7",
        ),
    );
    eventually("sendback acked", || h.bus.events(back) == [Ev::Ack]).await;
    assert_eq!(
        h.bus.trees()[key].handoff_seq,
        Some(back),
        "the turn's record"
    );

    h.bus.release_tree_merges();
    eventually("the report", || {
        h.bus.sent_containing("work_root uses").len() > reports
    })
    .await;
    let rec = &h.bus.trees()[key];
    assert_eq!(
        (rec.seq, rec.handoff_seq),
        (Some(a), Some(back)),
        "the report did not take the handoff back"
    );
    stop(tx, j).await;
}

/// r3 on #57: the report lists a tree, then `closed` removes it and an `assign` makes the
/// task's tree anew at that key. The report keeps the newer tree's record. (Its write of
/// demo#40, listed first, is held: a held write of demo#41 holds that key's removal, #58.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_report_listed_before_a_tree_was_made_anew_keeps_the_new_record() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    for (i, t) in [(40, "Move it"), (41, "Rename it")] {
        let a = h.bus.push(
            "w1",
            note(
                Kind::Assign,
                "w1",
                i,
                &format!("[claude] Assign: demo#{i} {t}"),
            ),
        );
        eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    }
    stop(tx, j).await;
    let key = "build-1.w1.demo-41";

    h.bus.hold_tree_merges();
    let reports = h.bus.sent_containing("work_root uses").len();
    let (tx, j) = h.start(&["w1"]);
    eventually("the report holds its write of demo#40", || {
        h.bus.tree_merges_held() == 1
    })
    .await;
    let c = h.bus.push(
        "w1",
        note(Kind::Closed, "w1", 41, "[claude] Closed: demo#41"),
    );
    eventually("closed acked", || h.bus.events(c) == [Ev::Ack]).await;
    let again = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 41, "[claude] Assign: demo#41 Again"),
    );
    eventually("assign acked", || h.bus.events(again) == [Ev::Ack]).await;
    assert_eq!(h.bus.trees()[key].seq, Some(again), "the new tree's record");

    h.bus.release_tree_merges();
    eventually("the report", || {
        h.bus.sent_containing("work_root uses").len() > reports
    })
    .await;
    let rec = &h.bus.trees()[key];
    assert_eq!(
        (rec.seq, rec.handoff_seq),
        (Some(again), Some(again)),
        "the report did not bring the removed tree back"
    );
    stop(tx, j).await;
}

/// hgl-ai#58: the report has listed the tree and is writing it when `closed` removes the
/// tree. The removal's deregistering waits for that write, so no record is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_report_writing_a_tree_as_cleanup_removes_it_leaves_no_record() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 41, "[claude] Assign: demo#41 Rename it"),
    );
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    stop(tx, j).await;
    let key = "build-1.w1.demo-41";
    let dir = h.work().join("w1/demo-41");
    assert!(h.bus.trees().contains_key(key) && dir.exists());

    // The report is past its check that the tree is there, inside the write.
    h.bus.hold_tree_merges();
    let reports = h.bus.sent_containing("work_root uses").len();
    let (tx, j) = h.start(&["w1"]);
    eventually("the report holds its write", || {
        h.bus.tree_merges_held() == 1
    })
    .await;
    let c = h.bus.push(
        "w1",
        note(Kind::Closed, "w1", 41, "[claude] Closed: demo#41"),
    );
    eventually("the tree removed", || !dir.exists()).await;

    h.bus.release_tree_merges();
    eventually("closed acked", || h.bus.events(c) == [Ev::Ack]).await;
    eventually("the report", || {
        h.bus.sent_containing("work_root uses").len() > reports
    })
    .await;
    assert!(
        !h.bus.trees().contains_key(key),
        "the report did not register the removed tree"
    );
    stop(tx, j).await;
}

/// hgl-ai#58: the report lists two trees; while it writes the first, `closed` removes the
/// second. The report, checking before each write, does not register the second again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_report_does_not_register_a_tree_removed_after_its_listing() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    for (i, t) in [(41, "Rename it"), (42, "Move it")] {
        let a = h.bus.push(
            "w1",
            note(
                Kind::Assign,
                "w1",
                i,
                &format!("[claude] Assign: demo#{i} {t}"),
            ),
        );
        eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    }
    stop(tx, j).await;
    let (first, second) = ("build-1.w1.demo-41", "build-1.w1.demo-42");
    let dir = h.work().join("w1/demo-42");
    assert!(h.bus.trees().contains_key(second) && dir.exists());

    // The report holds its write of the first tree (they are listed in key order).
    h.bus.hold_tree_merges();
    let reports = h.bus.sent_containing("work_root uses").len();
    let (tx, j) = h.start(&["w1"]);
    eventually("the report holds its first write", || {
        h.bus.tree_merges_held() == 1
    })
    .await;
    let c = h.bus.push(
        "w1",
        note(Kind::Closed, "w1", 42, "[claude] Closed: demo#42"),
    );
    eventually("the second tree removed", || !dir.exists()).await;

    h.bus.release_tree_merges();
    eventually("closed acked", || h.bus.events(c) == [Ev::Ack]).await;
    eventually("the report", || {
        h.bus.sent_containing("work_root uses").len() > reports
    })
    .await;
    let trees = h.bus.trees();
    assert!(
        trees.contains_key(first),
        "the first tree is still registered"
    );
    assert!(
        !trees.contains_key(second),
        "the report did not register the removed tree"
    );
    stop(tx, j).await;
}

/// r1's minor 3 on hgl-ai PR 25: a dead letter quotes the note's class, so a resend keeps it.
#[test]
fn a_dead_letter_quotes_the_class() {
    let mut n = note(Kind::Assign, "w1", 3, "[claude] Assign: demo#3");
    n.class = Some(TaskClass::Standard);
    let d = crate::describe_note(9, &n);
    assert!(
        d.lines().next().unwrap().contains(" class=standard "),
        "{d}"
    );
}

/// r3's finding 2 and the review bot's thread on hgl-ai PR 25: an older `assign` delivered
/// again after a newer `sendback` moved the class up does not move it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_older_class_note_delivered_again_changes_nothing() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let dir = h.work().join("w1/demo-23");
    let mut n = note(Kind::Assign, "w1", 23, "[claude] Assign: demo#23");
    n.class = Some(TaskClass::Simple);
    let a = h.bus.push("w1", n.clone());
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    let mut up = note(Kind::Sendback, "w1", 23, "[claude] Sendback: demo#23");
    up.class = Some(TaskClass::Complex);
    let s = h.bus.push("w1", up);
    eventually("sendback acked", || h.bus.events(s) == [Ev::Ack]).await;
    assert_eq!(
        hgl_ai_tree::read_task(&dir).unwrap().class,
        Some(TaskClass::Complex)
    );
    // The assign again, at its own (older) stream sequence.
    h.bus.redeliver("w1", a, n);
    eventually("redelivery acked", || h.bus.events(a) == [Ev::Ack, Ev::Ack]).await;
    assert_eq!(
        hgl_ai_tree::read_task(&dir).unwrap().class,
        Some(TaskClass::Complex)
    );
    assert_eq!(
        h.bus.trees()["build-1.w1.demo-23"].class,
        Some(TaskClass::Complex)
    );
    stop(tx, j).await;
}

/// PROTO-21 (the controller's decision on hgl-ai PR 25): the class picks the seat, not the
/// model. The seat's own binding runs a task whatever its class; the tree records the
/// class, the prompt names it, and a `sendback` only moves it up (ROLE-15).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_seats_binding_runs_every_class_and_the_tree_records_it() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let dir = h.work().join("w1/demo-980");
    let recorded = || {
        (
            hgl_ai_tree::read_task(&dir).unwrap().class,
            h.bus.trees()["build-1.w1.demo-980"].class,
        )
    };
    let model = |i: usize| {
        let c = &h.calls("codex")[i];
        let at = c.args.iter().position(|a| a == "-m").unwrap();
        c.args[at + 1].clone()
    };

    let mut n = note(
        Kind::Assign,
        "w1",
        980,
        "[claude] Assign: demo#980 Rename it",
    );
    n.class = Some(TaskClass::Simple);
    let a = h.bus.push("w1", n);
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    assert_eq!(
        model(0),
        "gpt-6.2-astra",
        "the seat's binding, not a lighter one"
    );
    assert!(
        h.calls("codex")[0]
            .stdin
            .contains("\nTask class: simple.\n")
    );
    let simple = Some(TaskClass::Simple);
    assert_eq!(recorded(), (simple, simple));

    // A sendback without a class keeps it.
    let s = h.bus.push(
        "w1",
        note(Kind::Sendback, "w1", 980, "[claude] Sendback: once"),
    );
    eventually("first sendback acked", || h.bus.events(s) == [Ev::Ack]).await;
    assert_eq!(recorded(), (simple, simple));

    // The second for the same finding moves it up; the binding stays the seat's.
    let mut n = note(Kind::Sendback, "w1", 980, "[claude] Sendback: again");
    n.class = Some(TaskClass::Complex);
    let s = h.bus.push("w1", n);
    eventually("second sendback acked", || h.bus.events(s) == [Ev::Ack]).await;
    assert_eq!(model(2), "gpt-6.2-astra");
    assert!(
        h.calls("codex")[2]
            .stdin
            .contains("\nTask class: complex.\n")
    );
    let complex = Some(TaskClass::Complex);
    assert_eq!(recorded(), (complex, complex));

    // A sendback never moves a class down.
    let mut n = note(Kind::Sendback, "w1", 980, "[claude] Sendback: lower");
    n.class = Some(TaskClass::Simple);
    let s = h.bus.push("w1", n);
    eventually("third sendback acked", || h.bus.events(s) == [Ev::Ack]).await;
    assert_eq!(recorded(), (complex, complex));
    stop(tx, j).await;
}

/// The class leaves quota failover as on main (PROTO-17): a classed task refused on the
/// primary moves the seat to its fallback, and its class goes with the task.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_classed_task_fails_over_as_any_other() {
    let h = harness();
    h.script(
        "codex",
        &format!("cat '{}'; exit 1", fixture("codex-quota.synthetic.jsonl")),
    );
    h.script("agy", &format!("cat '{}'", fixture("agy-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let mut n = note(Kind::Assign, "w1", 31, "[claude] Assign: demo#31 Rename");
    n.class = Some(TaskClass::Simple);
    let a = h.bus.push("w1", n);
    eventually("acked once moved", || h.bus.events(a) == [Ev::Ack]).await;
    assert_eq!(
        h.bus
            .sent_containing("moved from codex gpt/gpt-6.2-astra to agy claude/claude-opus-5-5")
            .len(),
        1
    );
    let agy = h.calls("agy");
    let i = agy[0].args.iter().position(|a| a == "--model").unwrap();
    assert_eq!(agy[0].args[i + 1], "claude-opus-5-5");
    let f = hgl_ai_tree::read_task(&h.work().join("w1/demo-31")).unwrap();
    assert_eq!(f.class, Some(TaskClass::Simple));
    stop(tx, j).await;
}

/// Every log line of the test binary, for tests that check what the bridge logs; set once.
fn logs() -> &'static Arc<std::sync::Mutex<Vec<u8>>> {
    static LOGS: std::sync::OnceLock<Arc<std::sync::Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        #[derive(Clone)]
        struct W(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for W {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Arc::<std::sync::Mutex<Vec<u8>>>::default();
        let w = W(buf.clone());
        let _ = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || w.clone())
            .try_init();
        buf
    })
}

fn logged(text: &str) -> Vec<String> {
    String::from_utf8_lossy(&logs().lock().unwrap())
        .lines()
        .filter(|l| l.contains(text))
        .map(String::from)
        .collect()
}

/// hgl-ai#27: a new branch is named from the issue's GitHub title (trimmed, every word
/// kept), not the note's first line; a pushed `codex/q<issue>-*` branch is resumed with no
/// title asked for; with the title unknown (the reason logged), empty, or during a hold
/// (nothing asked), the note's first line names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_branch_is_named_from_the_issue_title_and_a_pushed_one_is_kept() {
    logs();
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    {
        let mut t = h.titles.lock().unwrap();
        t.insert(40, Ok("  [P-F1] Parser: reject a trailing comma \n".into()));
        t.insert(41, Ok("A renamed issue".into()));
        t.insert(42, Err("HTTP 502: Bad Gateway (gh issue view 42)".into()));
        t.insert(43, Ok(" \t ".into()));
        t.insert(44, Ok("Not asked during a hold".into()));
    }
    // Issue 41's branch was pushed earlier, under the issue's old title.
    h.git
        .run(
            &h.seed,
            [
                "checkout",
                "--quiet",
                "-B",
                "codex/q41-old-title",
                "origin/main",
            ],
        )
        .unwrap();
    let pushed = commit(&h.git, &h.seed, "q41.txt", "earlier\n", "q41 work");
    h.git
        .run(
            &h.seed,
            ["push", "--quiet", "origin", "codex/q41-old-title"],
        )
        .unwrap();
    let (tx, j) = h.start(&["w1"]);
    let branch = |issue: u64| {
        hgl_ai_tree::read_task(&h.work().join(format!("w1/demo-{issue}")))
            .unwrap()
            .branch
            .unwrap()
    };
    let from_note = "class-standard-docs-roles-md-3";
    for (issue, want) in [
        (
            40,
            "codex/q40-p-f1-parser-reject-a-trailing-comma".to_string(),
        ),
        (41, "codex/q41-old-title".into()),
        (42, format!("codex/q42-{from_note}")),
        (43, format!("codex/q43-{from_note}")),
        (44, format!("codex/q44-{from_note}")),
    ] {
        if issue == 44 {
            h.bus.set_state("hold", "rate limit");
        }
        let body = format!("Class: standard (docs/roles.md §3)\n\nFollow demo#{issue}.");
        let a = h.bus.push("w1", note(Kind::Assign, "w1", issue, &body));
        eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
        assert_eq!(branch(issue), want, "issue {issue}");
    }
    let f = hgl_ai_tree::read_task(&h.work().join("w1/demo-41")).unwrap();
    assert_eq!(f.start, pushed, "the pushed branch is resumed");
    assert_eq!(
        *h.asked.lock().unwrap(),
        [40, 42, 43],
        "asked only for new branches, never during a hold"
    );
    let why = logged("task=demo-42");
    assert!(
        why.iter()
            .any(|l| l.contains("issue title unknown") && l.contains("HTTP 502: Bad Gateway")),
        "the reason is logged: {why:?}"
    );
    assert!(
        logged("task=demo-43")
            .iter()
            .any(|l| l.contains("the issue title is empty"))
    );
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn assign_then_sendback_resumes_then_landed_cleans_up() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);

    let a = h.bus.push(
        "w1",
        note(
            Kind::Assign,
            "w1",
            980,
            "[claude] Assign: demo#980 Fix the thing\n\nDetails.",
        ),
    );
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    let dir = h.work().join("w1/demo-980");
    let calls = h.calls("codex");
    assert_eq!(calls.len(), 1);
    let c = &calls[0];
    assert_eq!(c.cwd, dir.join("src").display().to_string());
    for (k, v) in [
        ("TMPDIR", dir.join("tmp").display().to_string()),
        ("CARGO_TARGET_DIR", dir.join("build").display().to_string()),
        ("HGL_AI_TASK_DIR", dir.display().to_string()),
        ("HGL_AI_SEAT", "w1".into()),
        ("HGL_AI_ROLE", "implementor".into()),
    ] {
        assert_eq!(c.env.get(k), Some(&v), "{k}");
    }
    assert_eq!(
        &c.args[..5],
        ["exec", "--json", "--strict-config", "-m", "gpt-6.2-astra"]
    );
    assert!(!c.args.contains(&"resume".to_string()));
    assert!(
        c.stdin
            .contains("--- Note 1 of 1: assign from claude to w1, demo#980"),
        "the prompt on stdin"
    );
    let prompts: Vec<_> = std::fs::read_dir(dir.join("prompts")).unwrap().collect();
    assert_eq!(prompts.len(), 1, "the turn's prompt is recorded");
    assert!(h.bus.trees().contains_key("build-1.w1.demo-980"));
    let tf = hgl_ai_tree::read_task(&dir).unwrap();
    assert_eq!(tf.branch.as_deref(), Some("codex/q980-fix-the-thing"));
    assert_eq!(tf.sessions.get("codex").map(String::as_str), Some(THREAD));
    assert!(
        std::fs::read_dir(dir.join("log")).unwrap().count() >= 2,
        "turn logs"
    );

    let s = h.bus.push(
        "w1",
        note(Kind::Sendback, "w1", 980, "[claude] Sendback: fix the test"),
    );
    eventually("sendback acked", || h.bus.events(s) == [Ev::Ack]).await;
    let calls = h.calls("codex");
    assert_eq!(calls.len(), 2);
    assert_eq!(&calls[1].args[..2], ["exec", "resume"]);
    assert!(
        calls[1].args.contains(&THREAD.to_string()),
        "resumed by the saved session id"
    );

    std::fs::write(dir.join("src/wip.txt"), "unsaved work\n").unwrap();
    let l = h.bus.push(
        "w1",
        note(Kind::Landed, "w1", 980, "[claude] Landed: demo#980"),
    );
    eventually("landed acked", || h.bus.events(l) == [Ev::Ack]).await;
    assert!(!dir.exists());
    assert!(h.bus.trees().is_empty());
    let done: Vec<Note> = h
        .bus
        .sent()
        .into_iter()
        .filter(|n| n.kind == Kind::Done)
        .collect();
    assert_eq!(done.len(), 1);
    assert_eq!(
        (
            done[0].sender.as_str(),
            done[0].recipient.as_str(),
            done[0].issue
        ),
        ("w1", "claude", 980)
    );
    assert!(
        done[0].body.contains("removed demo-980"),
        "{}",
        done[0].body
    );
    assert!(
        done[0].body.contains("salvage: salvage/w1-demo-980-"),
        "{}",
        done[0].body
    );
    let salvaged: Vec<_> = std::fs::read_dir(h.work().join("salvage"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(salvaged.len(), 1);
    assert!(
        std::fs::read_to_string(salvaged[0].path())
            .unwrap()
            .contains("+unsaved work")
    );
    assert_eq!(h.calls("codex").len(), 2, "no turn for `landed`");
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_refusal_naks_fails_over_and_tells_the_controller() {
    let h = harness();
    h.script(
        "codex",
        &format!("cat '{}'; exit 1", fixture("codex-quota.synthetic.jsonl")),
    );
    h.script("agy", &format!("cat '{}'", fixture("agy-ok.jsonl")));
    // The refused family remains eligible to review the fallback's work.
    h.bus.put_host(hgl_ai_proto::HostRecord {
        host: "build-2".into(),
        seats: vec![hgl_ai_proto::SeatRecord {
            seat: seat("r9"),
            role: Role::Reviewer,
            runtime: Runtime::Codex,
            family: Family::Gpt,
            model: "gpt-6.2-astra".into(),
        }],
        memcap_gb: 64,
        work_root_free_gb: 500,
        updated_at: hgl_ai_proto::utc_now_stamp(),
    });
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 3, "[claude] Assign: demo#3 Three"),
    );
    // Held, not naked: the refusal spends no delivery (PROTO-8).
    eventually("acked once moved", || h.bus.events(a) == [Ev::Ack]).await;
    let tf = hgl_ai_tree::read_task(&h.work().join("w1/demo-3")).unwrap();
    assert_eq!(tf.families, [Family::Claude]);
    assert_eq!(
        h.bus.trees()["build-1.w1.demo-3"].families,
        [Family::Claude]
    );
    let moved = h
        .bus
        .sent_containing("moved from codex gpt/gpt-6.2-astra to agy claude/claude-opus-5-5");
    assert_eq!(moved.len(), 1);
    assert_eq!(
        (moved[0].kind, moved[0].recipient.as_str()),
        (Kind::Note, "claude")
    );
    let agy = h.calls("agy");
    assert_eq!(agy.len(), 1);
    let i = agy[0].args.iter().position(|a| a == "--model").unwrap();
    assert_eq!(agy[0].args[i + 1], "claude-opus-5-5");
    assert!(
        !agy[0].args.contains(&"--conversation".to_string()),
        "no agy session yet"
    );
    eventually("host record shows the new binding", || {
        h.bus.last_host().is_some_and(|r| {
            r.seats.iter().any(|s| {
                s.seat.as_str() == "w1" && s.runtime == Runtime::Agy && s.family == Family::Claude
            })
        })
    })
    .await;
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_naks_without_failover() {
    let h = harness();
    h.script("codex", "echo boom >&2; exit 2");
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 4, "[claude] Assign: demo#4 Four"),
    );
    eventually("nak", || {
        h.bus.events(a).first() == Some(&Ev::Nak(Duration::from_millis(50)))
    })
    .await;
    assert!(h.bus.sent_containing("Failover").is_empty());
    assert!(h.calls("agy").is_empty());
    let tf = hgl_ai_tree::read_task(&h.work().join("w1/demo-4")).unwrap();
    assert_eq!(tf.families, [Family::Gpt]);
    assert_eq!(h.bus.trees()["build-1.w1.demo-4"].families, [Family::Gpt]);
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_holds_assign_until_the_gate_opens_and_tells_once_per_change() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    h.bus.set_state("mode", "drain");
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 1, "[claude] Assign: demo#1 One"),
    );
    let b = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 2, "[claude] Assign: demo#2 Two"),
    );
    eventually("both kept alive", || {
        h.bus.progressed(a) && h.bus.progressed(b)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        h.bus.events(a).is_empty() && h.bus.events(b).is_empty(),
        "held, neither naked (max_deliver) nor acked: {:?} {:?}",
        h.bus.events(a),
        h.bus.events(b)
    );
    assert_eq!(h.bus.sent_containing("mode is drain").len(), 1);
    assert!(h.calls("codex").is_empty());
    assert!(!h.work().join("w1/demo-1").exists());
    h.bus.set_state("mode", "normal");
    eventually("both run once the gate opens", || {
        h.bus.events(a) == [Ev::Ack] && h.bus.events(b) == [Ev::Ack]
    })
    .await;
    assert_eq!(h.calls("codex").len(), 2);
    eventually("open again", || {
        h.bus.sent_containing("taking new work again").len() == 1
    })
    .await;
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn low_disk_holds_assign_and_shutdown_hands_it_back() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    h.disk.0.store(crate::disk::GB, Ordering::SeqCst);
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 1, "[claude] Assign: demo#1 One"),
    );
    eventually("kept alive", || h.bus.progressed(a)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(h.bus.events(a).is_empty(), "{:?}", h.bus.events(a));
    let told = h.bus.sent_containing("below min_free_gb 200");
    assert_eq!(told.len(), 1);
    assert_eq!(told[0].sender.as_str(), "build-1");
    assert!(h.calls("codex").is_empty());
    // A stopping bridge hands the held note back for redelivery.
    stop(tx, j).await;
    assert_eq!(h.bus.events(a), [Ev::Nak(Duration::from_millis(50))]);
    // Space again: the next bridge runs it.
    h.disk.0.store(1000 * crate::disk::GB, Ordering::SeqCst);
    let (tx, j) = h.start(&["w1"]);
    eventually("run after the redelivery", || {
        h.bus.events(a) == [Ev::Nak(Duration::from_millis(50)), Ev::Ack]
    })
    .await;
    assert_eq!(h.calls("codex").len(), 1);
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_sent_before_its_task_ended_is_acked_without_a_turn() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let assign = note(Kind::Assign, "w1", 20, "[claude] Assign: demo#20 Twenty");
    let a = h.bus.push("w1", assign.clone());
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    let l = h.bus.push(
        "w1",
        note(Kind::Landed, "w1", 20, "[claude] Landed: demo#20"),
    );
    eventually("landed acked", || h.bus.events(l) == [Ev::Ack]).await;
    let dir = h.work().join("w1/demo-20");
    assert!(!dir.exists());
    // JetStream hands the assign out again after the end (a redelivery, a moved seat).
    h.bus.redeliver("w1", a, assign.clone());
    eventually("acked again", || h.bus.events(a) == [Ev::Ack, Ev::Ack]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.calls("codex").len(), 1, "no turn for the ended task");
    assert!(!dir.exists(), "no tree for the ended task");
    stop(tx, j).await;

    // Remembered across a restart (under data_dir).
    let (tx, j) = h.start(&["w1"]);
    h.bus.redeliver("w1", a, assign.clone());
    eventually("acked after the restart", || h.bus.events(a).len() == 3).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.calls("codex").len(), 1);
    assert!(!dir.exists());
    // An `assign` sent after the end starts the task again.
    let again = h.bus.push("w1", assign);
    eventually("the new assign runs", || h.bus.events(again) == [Ev::Ack]).await;
    assert_eq!(h.calls("codex").len(), 2);
    assert!(dir.exists());
    stop(tx, j).await;
}

/// The seat moves to another host after its task ended: the hub's `HGL_ENDED` record, not
/// the first host's cache, tells the new host the redelivered assignment is moot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_moved_to_another_host_keeps_its_ended_tasks() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let assign = note(
        Kind::Assign,
        "w1",
        21,
        "[claude] Assign: demo#21 Twenty-one",
    );
    let a = h.bus.push("w1", assign.clone());
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    let l = h.bus.push(
        "w1",
        note(Kind::Landed, "w1", 21, "[claude] Landed: demo#21"),
    );
    eventually("landed acked", || h.bus.events(l) == [Ev::Ack]).await;
    assert_eq!(
        h.bus.ended("w1", "demo", 21),
        Some(l),
        "recorded on the hub"
    );
    stop(tx, j).await;

    let b = h.other_host("build-2");
    let (tx, j) = h.start_with(&b, &["w1"]);
    h.bus.redeliver("w1", a, assign.clone());
    eventually("acked on the new host", || {
        h.bus.events(a) == [Ev::Ack, Ev::Ack]
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.calls("codex").len(), 1, "no turn for the ended task");
    let dir_b = h.root.join("build-2/work/w1/demo-21");
    assert!(!dir_b.exists(), "no tree for the ended task");
    assert!(
        !h.root.join("build-2/data/bridge/ended/w1.json").exists(),
        "the new host's cache had no record of it"
    );
    // An `assign` sent after the end starts the task again, on the new host.
    let again = h.bus.push("w1", assign);
    eventually("the new assign runs", || h.bus.events(again) == [Ev::Ack]).await;
    assert_eq!(h.calls("codex").len(), 2);
    assert!(dir_b.exists());
    stop(tx, j).await;
}

/// Without the hub's `HGL_ENDED`, no turn starts for a task and no end is acked unrecorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ended_tasks_wait_for_the_hub() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    h.bus.ended_down(true, false);
    let a = h.bus.push(
        "w1",
        note(
            Kind::Assign,
            "w1",
            22,
            "[claude] Assign: demo#22 Twenty-two",
        ),
    );
    // Held, kept alive, no delivery spent (PROTO-8).
    eventually("held while the hub cannot say", || h.bus.progressed(a)).await;
    assert!(h.bus.events(a).is_empty(), "{:?}", h.bus.events(a));
    assert!(h.calls("codex").is_empty());
    h.bus.ended_down(false, false);
    eventually("run once the hub answers", || {
        h.bus.events(a).last() == Some(&Ev::Ack)
    })
    .await;
    assert_eq!(h.calls("codex").len(), 1);

    // The end cannot be recorded: the tree goes at once, the note is held and done again.
    h.bus.ended_down(false, true);
    let l = h.bus.push(
        "w1",
        note(Kind::Landed, "w1", 22, "[claude] Landed: demo#22"),
    );
    eventually("landed held", || {
        h.bus.progressed(l) && !h.work().join("w1/demo-22").exists()
    })
    .await;
    assert!(h.bus.events(l).is_empty(), "{:?}", h.bus.events(l));
    h.bus.ended_down(false, false);
    eventually("landed acked once recorded", || {
        h.bus.events(l).last() == Some(&Ev::Ack)
    })
    .await;
    assert_eq!(h.bus.ended("w1", "demo", 22), Some(l));
    assert_eq!(
        h.bus.sent_containing("Cleanup: demo#22").len(),
        1,
        "one cleanup receipt"
    );
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_whose_lease_another_bridge_holds_is_not_served() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    h.bus.steal_lease("seat.w1", "build-2 pid 9 (bridge)");
    let (tx, j) = h.start(&["w1"]);
    let n = h.bus.push(
        "w1",
        Note::new(seat("claude"), seat("w1"), Kind::Note, "hello"),
    );
    eventually("a heartbeat", || h.bus.last_host().is_some()).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(h.bus.events(n).is_empty(), "build-2 holds w1");
    assert!(h.calls("codex").is_empty());
    assert!(h.bus.last_host().unwrap().seats.is_empty());
    assert_eq!(
        h.bus.lease_holder("seat.w1").as_deref(),
        Some("build-2 pid 9 (bridge)")
    );
    // A live process of this host named as holder (pid 1 always exists): not taken over.
    h.bus.steal_lease("seat.w1", "build-1 pid 1 (bridge)");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(h.bus.events(n).is_empty());
    // The other bridge lets go: this one takes the seat.
    h.bus.drop_lease("seat.w1");
    eventually("served", || h.bus.events(n) == [Ev::Ack]).await;
    let ours = format!("build-1 pid {} (bridge) id ", std::process::id());
    assert!(h.bus.lease_holder("seat.w1").unwrap().starts_with(&ours));
    stop(tx, j).await;
    assert_eq!(h.bus.lease_holder("seat.w1"), None, "released at shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lease_of_a_crashed_bridge_of_this_host_is_taken_over() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    // Above the largest Linux pid: no such process.
    h.bus
        .steal_lease("seat.w1", "build-1 pid 999999999 (bridge)");
    let (tx, j) = h.start(&["w1"]);
    let n = h.bus.push(
        "w1",
        Note::new(seat("claude"), seat("w1"), Kind::Note, "hello"),
    );
    eventually("served at once", || h.bus.events(n) == [Ev::Ack]).await;
    let ours = h.bus.lease_holder("seat.w1").unwrap();
    assert!(ours.starts_with(&format!("build-1 pid {} (bridge) id ", std::process::id())));
    stop(tx, j).await;

    // Likewise the lease of a `note wait` of this host that is gone.
    h.bus
        .steal_lease("seat.w1", "build-1 pid 999999999 (note wait)");
    let (tx, j) = h.start(&["w1"]);
    let n = h.bus.push(
        "w1",
        Note::new(seat("claude"), seat("w1"), Kind::Note, "again"),
    );
    eventually("served at once", || h.bus.events(n) == [Ev::Ack]).await;
    assert_eq!(h.bus.lease_holder("seat.w1"), Some(ours.clone()));
    stop(tx, j).await;

    // A dead holder of another machine that shares the label (another host id): not ours.
    let id = ours.rsplit(' ').next().unwrap();
    let other = format!("build-1 pid 999999999 (bridge) id {}", "f".repeat(id.len()));
    h.bus.steal_lease("seat.w1", &other);
    let (tx, j) = h.start(&["w1"]);
    let n = h.bus.push(
        "w1",
        Note::new(seat("claude"), seat("w1"), Kind::Note, "third"),
    );
    eventually("a heartbeat", || h.bus.last_host().is_some()).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        h.bus.events(n).is_empty(),
        "not taken from the other machine"
    );
    assert_eq!(h.bus.lease_holder("seat.w1"), Some(other));
    h.bus.drop_lease("seat.w1");
    eventually("served once it lets go", || h.bus.events(n) == [Ev::Ack]).await;
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_seat_lease_stops_the_seat() {
    let h = harness();
    let pid = h.calls.join("pid");
    h.script(
        "codex",
        &format!(
            "echo $$ > '{p}.new' && mv '{p}.new' '{p}'; sleep 60",
            p = pid.display()
        ),
    );
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 30, "[claude] Assign: demo#30 Thirty"),
    );
    eventually("turn running", || pid.exists()).await;
    eventually("advertised", || {
        h.bus.last_host().is_some_and(|r| r.seats.len() == 1)
    })
    .await;
    h.bus.steal_lease("seat.w1", "build-2 pid 9 (bridge)");
    eventually("the turn's note handed back", || {
        h.bus.events(a) == [Ev::Nak(Duration::from_millis(50))]
    })
    .await;
    let p: i32 = std::fs::read_to_string(&pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    eventually("the turn stopped", || !alive(p)).await;
    eventually("the controller told", || {
        !h.bus
            .sent_containing("the seat lease seat.w1 is no longer ours")
            .is_empty()
    })
    .await;
    let told = h
        .bus
        .sent_containing("the seat lease seat.w1 is no longer ours");
    assert_eq!(told.len(), 1);
    assert_eq!(told[0].recipient.as_str(), "claude");
    assert!(
        told[0].body.contains("stopped its running turn"),
        "{}",
        told[0].body
    );
    eventually("no longer advertised", || {
        h.bus.last_host().is_some_and(|r| r.seats.is_empty())
    })
    .await;
    // The redelivered note waits for the seat's new holder.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(h.calls("codex").len(), 1);
    assert_eq!(h.bus.events(a), [Ev::Nak(Duration::from_millis(50))]);
    stop(tx, j).await;
    assert_eq!(
        h.bus.lease_holder("seat.w1").as_deref(),
        Some("build-2 pid 9 (bridge)"),
        "the new holder's lease is left alone"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversized_line_then_a_hang_is_killed_and_turns_never_overlap() {
    let mut h = harness();
    // Long enough for each attempt to reach its helpers on a loaded runner.
    h.opts.turn_timeout = Duration::from_secs(2);
    let c = h.calls.display().to_string();
    h.script(
        "codex",
        &format!(
            "for p in $(cat '{c}/pids' 2>/dev/null); do kill -0 $p 2>/dev/null && echo $p >> '{c}/overlap'; done\n\
             echo $$ >> '{c}/pids'\n\
             head -c 3000000 /dev/zero | tr '\\0' x; echo\n\
             cat '{f}'\n\
             sleep 60 & echo $! >> '{c}/pids'\n\
             sleep 60",
            f = fixture("codex-ok.jsonl")
        ),
    );
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 6, "[claude] Assign: demo#6 Six"),
    );
    eventually("two attempts", || {
        h.calls("codex").len() >= 2 && h.bus.events(a).len() >= 2
    })
    .await;
    stop(tx, j).await;
    assert!(
        h.bus.events(a).iter().all(|e| matches!(e, Ev::Nak(_))),
        "{:?}",
        h.bus.events(a)
    );
    assert!(
        !h.calls.join("overlap").exists(),
        "a turn started while an earlier one lived"
    );
    let pids: Vec<i32> = std::fs::read_to_string(h.calls.join("pids"))
        .unwrap()
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();
    assert!(pids.len() >= 4);
    for p in pids {
        assert!(!alive(p), "{p} outlived its turn");
    }
    let tf = hgl_ai_tree::read_task(&h.work().join("w1/demo-6")).unwrap();
    assert_eq!(
        tf.sessions.get("codex").map(String::as_str),
        Some(THREAD),
        "the line after the long one was parsed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_turn_lock_defers_the_turn() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 7, "[claude] Assign: demo#7 Seven"),
    );
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    let held = FileLock::turn_patiently(&h.work().join("w1/demo-7/turn.lock"))
        .await
        .unwrap()
        .expect("free after the turn");
    let q = h.bus.push(
        "w1",
        note(Kind::Question, "w1", 7, "[claude] Question: why?"),
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        h.calls("codex").len(),
        1,
        "no second turn while the lock is held"
    );
    assert!(
        h.bus.events(q).is_empty(),
        "the note waits; it is neither acked nor naked"
    );
    assert!(h.bus.progressed(q), "and kept alive");
    assert_eq!(
        h.bus
            .sent_containing("still holds the turn lock of demo#7")
            .len(),
        1
    );
    drop(held);
    eventually("question acked", || h.bus.events(q) == [Ev::Ack]).await;
    assert_eq!(h.calls("codex").len(), 2);
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_held_elsewhere_is_not_served() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let held = FileLock::try_exclusive(&h.cfg.seat_lock_path(&seat("w1")).unwrap())
        .unwrap()
        .unwrap();
    let (tx, j) = h.start(&["w1"]);
    let n = h.bus.push(
        "w1",
        Note::new(seat("claude"), seat("w1"), Kind::Note, "hello"),
    );
    eventually("a heartbeat", || h.bus.last_host().is_some()).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(h.bus.events(n).is_empty());
    assert!(h.calls("codex").is_empty());
    assert!(
        h.bus.last_host().unwrap().seats.is_empty(),
        "an unserved seat is not advertised"
    );
    drop(held);
    eventually("served", || h.bus.events(n) == [Ev::Ack]).await;
    let c = &h.calls("codex")[0];
    assert_eq!(c.cwd, h.work().join("w1/_inbox").display().to_string());
    eventually("advertised", || {
        h.bus.last_host().is_some_and(|r| r.seats.len() == 1)
    })
    .await;
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn review_runs_in_a_detached_merge_and_landed_removes_it() {
    let h = harness();
    h.git
        .run(&h.seed, ["checkout", "--quiet", "-b", "feature"])
        .unwrap();
    let head = commit(&h.git, &h.seed, "f.txt", "f\n", "feature");
    h.git
        .run(&h.seed, ["push", "--quiet", "origin", "feature"])
        .unwrap();
    h.script("claude", &format!("cat '{}'", fixture("claude-ok.jsonl")));
    let (tx, j) = h.start(&["r1"]);
    let mut r = note(Kind::Review, "r1", 5, "[claude] Review: demo#5");
    r.head = Some(head.clone());
    let s = h.bus.push("r1", r);
    eventually("review acked", || h.bus.events(s) == [Ev::Ack]).await;
    let dir = h.work().join("r1/demo-5-review");
    let c = &h.calls("claude")[0];
    assert_eq!(c.cwd, dir.join("src").display().to_string());
    assert_eq!(c.args[0], "-p");
    assert!(
        c.stdin.contains(&format!("review of demo#5 at {head}")),
        "the prompt names the reviewed head"
    );
    for pair in [
        ["--model", "claude-opus-5-5"],
        ["--effort", "xhigh"],
        ["--permission-mode", "bypassPermissions"],
    ] {
        let i = c.args.iter().position(|a| a == pair[0]).unwrap();
        assert_eq!(c.args[i + 1], pair[1]);
    }
    assert!(!c.args.contains(&"--resume".to_string()));
    assert!(h.bus.trees().contains_key("build-1.r1.demo-5-review"));

    let l = h
        .bus
        .push("r1", note(Kind::Landed, "r1", 5, "[claude] Landed: demo#5"));
    eventually("landed acked", || h.bus.events(l) == [Ev::Ack]).await;
    assert!(!dir.exists());
    let done = h.bus.sent_containing("Cleanup: demo#5 on build-1/r1");
    assert_eq!(done.len(), 1);
    assert!(done[0].body.contains("salvage: none"), "{}", done[0].body);
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn landed_drops_the_queued_notes_of_its_task() {
    let h = harness();
    let gate = h.calls.join("go");
    h.script(
        "codex",
        &format!(
            "case \"$(pwd)\" in *demo-11*) i=0; while [ ! -e '{g}' ]; do i=$((i+1)); if [ \"$i\" -ge 400 ]; then echo 'timed out waiting for gate' >&2; exit 1; fi; sleep 0.05; done;; esac\ncat '{f}'",
            g = gate.display(),
            f = fixture("codex-ok.jsonl")
        ),
    );
    let (tx, j) = h.start(&["w1"]);
    let b = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 11, "[claude] Assign: demo#11 Busy"),
    );
    eventually("the turn of demo#11 runs", || h.calls("codex").len() == 1).await;
    // Queued behind the running turn, then ended before it could start.
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 12, "[claude] Assign: demo#12 Moot"),
    );
    let s = h.bus.push(
        "w1",
        note(Kind::Sendback, "w1", 12, "[claude] Sendback: demo#12"),
    );
    let l = h.bus.push(
        "w1",
        note(Kind::Landed, "w1", 12, "[claude] Landed: demo#12"),
    );
    eventually("landed acked", || h.bus.events(l) == [Ev::Ack]).await;
    assert_eq!(h.bus.events(a), [Ev::Ack], "the queued assign is acked");
    assert_eq!(h.bus.events(s), [Ev::Ack], "and the queued sendback");
    assert!(h.bus.events(b).is_empty(), "demo#11 still runs");

    std::fs::write(&gate, "").unwrap();
    eventually("demo#11 acked", || h.bus.events(b) == [Ev::Ack]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!h.work().join("w1/demo-12").exists(), "no tree for demo#12");
    assert!(!h.bus.trees().contains_key("build-1.w1.demo-12"));
    assert_eq!(h.calls("codex").len(), 1, "no turn for demo#12");
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn landed_without_a_tree_is_acked_silently() {
    let h = harness();
    let (tx, j) = h.start(&["w1"]);
    let l = h.bus.push(
        "w1",
        note(Kind::Landed, "w1", 99, "[claude] Landed: demo#99"),
    );
    eventually("acked", || h.bus.events(l) == [Ev::Ack]).await;
    assert!(h.bus.sent().iter().all(|n| n.kind != Kind::Done));
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_stops_a_running_turn_after_the_grace_and_naks() {
    let h = harness();
    let pid = h.calls.join("pid");
    h.script(
        "codex",
        &format!(
            "echo $$ > '{p}.new' && mv '{p}.new' '{p}'; sleep 60",
            p = pid.display()
        ),
    );
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 8, "[claude] Assign: demo#8 Eight"),
    );
    eventually("turn running", || pid.exists()).await;
    let t = std::time::Instant::now();
    stop(tx, j).await;
    assert!(t.elapsed() < Duration::from_secs(5));
    assert_eq!(h.bus.events(a), [Ev::Nak(Duration::from_millis(50))]);
    let p: i32 = std::fs::read_to_string(&pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!alive(p));
}

/// The inbox fetcher ending under a running turn (the race behind a shutdown that left the
/// turn running): the turn is stopped and its note naked, as at shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_inbox_fetcher_stops_the_running_turn() {
    let h = harness();
    let pid = h.calls.join("pid");
    h.script(
        "codex",
        &format!(
            "echo $$ > '{p}.new' && mv '{p}.new' '{p}'; sleep 60",
            p = pid.display()
        ),
    );
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 9, "[claude] Assign: demo#9 Nine"),
    );
    eventually("turn running", || pid.exists()).await;
    h.bus.break_inbox();
    eventually("the turn's note naked", || {
        h.bus.events(a) == [Ev::Nak(Duration::from_millis(50))]
    })
    .await;
    let p: i32 = std::fs::read_to_string(&pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    eventually("the turn stopped", || !alive(p)).await;
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_report_and_heartbeat() {
    let h = harness();
    std::fs::create_dir_all(h.work().join("w1/demo-12/src")).unwrap();
    let (tx, j) = h.start(&[]);
    eventually("report", || {
        !h.bus.sent_containing("work_root uses").is_empty()
    })
    .await;
    let r = &h.bus.sent_containing("work_root uses")[0];
    assert_eq!(
        (r.sender.as_str(), r.recipient.as_str(), r.kind),
        ("build-1", "claude", Kind::Note)
    );
    assert!(r.body.contains("1 trees"), "{}", r.body);
    eventually("both seats advertised", || {
        h.bus.last_host().is_some_and(|r| r.seats.len() == 2)
    })
    .await;
    let host = h.bus.last_host().unwrap();
    assert_eq!(host.host, "build-1");
    assert_eq!(host.memcap_gb, 96, "the default turn cap");
    assert_eq!(host.work_root_free_gb, 1000);
    assert_eq!(host.seats[0].seat.as_str(), "r1");
    assert_eq!(
        (host.seats[1].seat.as_str(), host.seats[1].family),
        ("w1", Family::Gpt)
    );
    stop(tx, j).await;
}

#[test]
fn controllers_are_not_served_unless_named() {
    let h = harness();
    let mut cfg = h.cfg.clone();
    cfg.seats.push(SeatConfig {
        name: seat("claude"),
        role: Role::Controller,
        binding: Binding {
            runtime: Runtime::Claude,
            family: None,
            tier: Some(Tier::Frontier),
            model: None,
            effort: None,
        },
        fallback: vec![],
        memcap_gb: None,
        headless: false,
    });
    let trees = Trees::with(
        Layout::new(h.work()).unwrap(),
        "build-1",
        &cfg.github,
        h.git.clone(),
    );
    let deps = Deps {
        closed: Arc::new(|_: &str, _: u64, _: bool| None),
        title: h.title.clone(),
        disk: h.disk.clone(),
    };
    let b = Bridge::new(cfg, h.bus.clone(), trees, deps, h.opts.clone());
    let names = |v: Vec<SeatConfig>| {
        v.into_iter()
            .map(|s| s.name.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(b.select(&[]).unwrap()), ["w1", "r1"]);
    assert_eq!(names(b.select(&[seat("claude")]).unwrap()), ["claude"]);
    assert!(b.select(&[seat("nope")]).is_err());

    // A headless controller is served without being named (OWN-7).
    let mut cfg = h.cfg.clone();
    let mut ctl = b.cfg.seat("claude").unwrap().clone();
    ctl.headless = true;
    cfg.seats.push(ctl);
    let trees = Trees::with(
        Layout::new(h.work()).unwrap(),
        "build-1",
        &cfg.github,
        h.git.clone(),
    );
    let deps = Deps {
        closed: Arc::new(|_: &str, _: u64, _: bool| None),
        title: h.title.clone(),
        disk: h.disk.clone(),
    };
    let b = Bridge::new(cfg, h.bus.clone(), trees, deps, h.opts.clone());
    assert_eq!(names(b.select(&[]).unwrap()), ["w1", "r1", "claude"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_another_hosts_bridge_serves_is_not_served() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    h.bus.put_host(hgl_ai_proto::HostRecord {
        host: "build-2".into(),
        seats: vec![hgl_ai_proto::SeatRecord {
            seat: seat("w1"),
            role: Role::Implementor,
            runtime: Runtime::Codex,
            family: Family::Gpt,
            model: "gpt-6.1-sol".into(),
        }],
        memcap_gb: 64,
        work_root_free_gb: 500,
        updated_at: hgl_ai_proto::utc_now_stamp(),
    });
    let (tx, j) = h.start(&["w1"]);
    let n = h.bus.push(
        "w1",
        Note::new(seat("claude"), seat("w1"), Kind::Note, "hello"),
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(h.bus.events(n).is_empty(), "build-2 serves w1");
    assert!(h.calls("codex").is_empty());
    stop(tx, j).await;
    assert_eq!(
        h.bus.deleted_hosts(),
        ["build-1"],
        "the host record goes at shutdown"
    );
}

/// A question to `to` whose encoded note is one byte under the bus's limit, its body full
/// of what JSON escapes.
fn near_limit(to: &str) -> Note {
    let mk = |b: String| Note::new(seat("claude"), seat(to), Kind::Question, b);
    let size = |n: &Note| serde_json::to_vec(n).unwrap().len();
    let limit = hgl_ai_bus::MAX_NOTE_BYTES - 1;
    let line = "a line with \"quotes\", a \\ backslash, a\ttab, é and 日本\n";
    let per = serde_json::to_string(line).unwrap().len() - 2;
    let mut body = line.repeat((limit - size(&mk(String::new()))) / per - 1);
    // Padded before the last newline, which `Note::new` would otherwise add.
    let pad = limit - size(&mk(body.clone()));
    body.pop();
    body.push_str(&"x".repeat(pad));
    body.push('\n');
    let n = mk(body);
    assert_eq!(size(&n), limit);
    n
}

/// A note at the bus's size limit reaches each CLI whole, on stdin: no argument carries it
/// (Linux caps one at 128 KiB), and the turn's prompt is kept in its directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_at_the_bus_limit_reaches_each_cli_whole_on_stdin() {
    let h = harness();
    let mut cfg = h.cfg.clone();
    cfg.seats.push(SeatConfig {
        name: seat("g1"),
        role: Role::Implementor,
        binding: Binding {
            runtime: Runtime::Agy,
            family: Some(Family::Gemini),
            tier: Some(Tier::Frontier),
            model: None,
            effort: None,
        },
        fallback: vec![],
        memcap_gb: None,
        headless: false,
    });
    cfg.validate().unwrap();
    for rt in ["claude", "codex", "agy"] {
        h.script(rt, &format!("cat '{}'", fixture(&format!("{rt}-ok.jsonl"))));
    }
    let (tx, j) = h.start_with(&cfg, &["w1", "r1", "g1"]);
    for (s, rt) in [("w1", "codex"), ("r1", "claude"), ("g1", "agy")] {
        let n = near_limit(s);
        let whole = format!("---\n{}\n", n.body.trim_end());
        let q = h.bus.push(s, n);
        eventually("acked", || h.bus.events(q) == [Ev::Ack]).await;
        let calls = h.calls(rt);
        assert_eq!(calls.len(), 1, "{rt}");
        let c = &calls[0];
        assert!(
            c.args.iter().all(|a| a.len() < 4096),
            "{rt}: no argument carries the prompt"
        );
        let prompt = if rt == "agy" {
            assert_eq!(c.stdin.lines().count(), 1, "one stream-json line");
            let v: serde_json::Value = serde_json::from_str(&c.stdin).unwrap();
            assert_eq!(v["event"], "user");
            v["message"]["content"].as_str().unwrap().to_string()
        } else {
            c.stdin.clone()
        };
        assert!(prompt.contains(&whole), "{rt}: the note arrives whole");
        assert!(!prompt.contains("truncated by the bridge"), "{rt}");
        let dir = h.work().join(s).join("_inbox/prompts");
        let recorded: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(recorded.len(), 1, "{rt}");
        assert!(
            std::fs::read_to_string(recorded[0].path()).unwrap() == prompt,
            "{rt}: the prompt recorded is the one sent"
        );
    }
    stop(tx, j).await;
}

/// A CLI that is not installed: the note is dropped at once, not naked until `max_deliver`
/// drops it, and the controller hears which.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_cli_drops_the_note_once_and_tells_the_controller() {
    let mut h = harness();
    h.opts
        .bins
        .insert(Runtime::Codex, h.bin.join("missing-codex"));
    let (tx, j) = h.start(&["w1"]);
    let n = h.bus.push(
        "w1",
        Note::new(seat("claude"), seat("w1"), Kind::Note, "hello"),
    );
    eventually("terminated", || h.bus.events(n) == [Ev::Term]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.bus.events(n), [Ev::Term], "not retried");
    let told = h
        .bus
        .sent_containing("cannot start a turn in the inbox directory");
    assert_eq!(told.len(), 1);
    assert_eq!(told[0].recipient.as_str(), "claude");
    for part in [
        "missing-codex: no such executable".to_string(),
        format!("seq {n} note from claude"),
    ] {
        assert!(told[0].body.contains(&part), "{part}: {}", told[0].body);
    }
    stop(tx, j).await;
}

/// A fake `codex` whose turns after the first write their pid to `pid` and wait for `go`.
fn held_after_the_first(h: &H) -> (PathBuf, PathBuf) {
    let (pid, gate) = (h.calls.join("pid"), h.calls.join("go"));
    h.script(
        "codex",
        &format!(
            "if [ \"$n\" -ge 2 ]; then echo $$ > '{p}.new' && mv '{p}.new' '{p}'; while [ ! -e '{g}' ]; do sleep 0.05; done; fi\ncat '{f}'",
            p = pid.display(),
            g = gate.display(),
            f = fixture("codex-ok.jsonl")
        ),
    );
    (pid, gate)
}

fn pid_in(p: &std::path::Path) -> i32 {
    std::fs::read_to_string(p).unwrap().trim().parse().unwrap()
}

/// A `landed` delivered again after its end was recorded (its ack was lost), once a newer
/// `assign` restarted the task: acked, and the restart's turn and tree are left alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redelivered_end_leaves_a_restarted_task_running() {
    let h = harness();
    let (pid, gate) = held_after_the_first(&h);
    let (tx, j) = h.start(&["w1"]);
    let assign = note(Kind::Assign, "w1", 40, "[claude] Assign: demo#40 Forty");
    let a = h.bus.push("w1", assign.clone());
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    let landed = note(Kind::Landed, "w1", 40, "[claude] Landed: demo#40");
    let l = h.bus.push("w1", landed.clone());
    eventually("landed acked", || h.bus.events(l) == [Ev::Ack]).await;
    let dir = h.work().join("w1/demo-40");
    assert!(!dir.exists());
    let again = h.bus.push("w1", assign);
    eventually("the restart runs", || pid.exists()).await;
    assert_eq!(hgl_ai_tree::read_task(&dir).unwrap().seq, Some(again));

    h.bus.redeliver("w1", l, landed);
    eventually("acked again", || h.bus.events(l) == [Ev::Ack, Ev::Ack]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(alive(pid_in(&pid)), "the restart's turn still runs");
    assert!(dir.join("src").exists(), "and its tree stays");
    assert!(h.bus.events(again).is_empty());
    assert_eq!(h.bus.sent_containing("Cleanup: demo#40").len(), 1);
    std::fs::write(&gate, "").unwrap();
    eventually("the restart acked", || h.bus.events(again) == [Ev::Ack]).await;
    assert!(dir.exists());
    stop(tx, j).await;
}

/// A `landed` whose end could not be recorded (held) is done again after a newer `assign`
/// restarted the task: the restart is left alone, and the end is then recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_end_redelivered_before_it_was_recorded_leaves_a_restart_alone() {
    let mut h = harness();
    // Room for the restart to start while the end waits for the hub.
    h.opts.hub_retry = Duration::from_millis(300);
    let (pid, gate) = held_after_the_first(&h);
    let (tx, j) = h.start(&["w1"]);
    let assign = note(Kind::Assign, "w1", 41, "[claude] Assign: demo#41 Forty-one");
    let a = h.bus.push("w1", assign.clone());
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    h.bus.ended_down(false, true);
    let l = h.bus.push(
        "w1",
        note(Kind::Landed, "w1", 41, "[claude] Landed: demo#41"),
    );
    let dir = h.work().join("w1/demo-41");
    eventually("landed held, its tree removed", || !dir.exists()).await;
    assert!(h.bus.events(l).is_empty(), "{:?}", h.bus.events(l));
    let again = h.bus.push("w1", assign);
    eventually("the restart runs", || pid.exists()).await;
    h.bus.ended_down(false, false);
    eventually("the end recorded", || {
        h.bus.events(l).last() == Some(&Ev::Ack)
    })
    .await;
    assert_eq!(h.bus.ended("w1", "demo", 41), Some(l));
    assert!(alive(pid_in(&pid)), "the restart's turn still runs");
    assert!(dir.join("src").exists(), "and its tree stays");
    assert!(h.bus.events(again).is_empty());
    assert_eq!(h.bus.sent_containing("Cleanup: demo#41").len(), 1);
    std::fs::write(&gate, "").unwrap();
    eventually("the restart acked", || h.bus.events(again) == [Ev::Ack]).await;
    stop(tx, j).await;
}

/// An end delivered late, while a newer note of the task runs in the tree made before the
/// end: the tree goes, and the newer note is handed back and runs again, not dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_end_hands_back_the_newer_notes_of_the_turn_it_stops() {
    let h = harness();
    let pid = h.calls.join("pid");
    h.script(
        "codex",
        &format!(
            "if [ \"$n\" -eq 2 ]; then echo $$ > '{p}.new' && mv '{p}.new' '{p}'; sleep 60; fi\ncat '{f}'",
            p = pid.display(),
            f = fixture("codex-ok.jsonl")
        ),
    );
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 50, "[claude] Assign: demo#50 Fifty"),
    );
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    // Sent next, delivered after the question below.
    let landed = note(Kind::Landed, "w1", 50, "[claude] Landed: demo#50");
    let l = h.bus.push("nobody", landed.clone());
    let q = h.bus.push(
        "w1",
        note(Kind::Question, "w1", 50, "[claude] Question: still there?"),
    );
    eventually("the question's turn runs", || pid.exists()).await;
    h.bus.redeliver("w1", l, landed);
    eventually("landed acked", || h.bus.events(l) == [Ev::Ack]).await;
    assert!(!h.work().join("w1/demo-50").exists());
    eventually("the question handed back, then run again", || {
        h.bus.events(q) == [Ev::Nak(Duration::from_millis(50)), Ev::Ack]
    })
    .await;
    assert!(!alive(pid_in(&pid)));
    let calls = h.calls("codex");
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[2].cwd,
        h.work().join("w1/_inbox").display().to_string()
    );
    stop(tx, j).await;
}

/// The bridge's notes (cleanup receipts, gate changes, reports) go to the configured
/// controller seat, `[github] controller`, and each prompt names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_bridges_notes_go_to_the_configured_controller() {
    let mut h = harness();
    h.cfg.github.controller = "boss".into();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    eventually("the startup report", || {
        !h.bus.sent_containing("work_root uses").is_empty()
    })
    .await;
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 60, "[claude] Assign: demo#60 Sixty"),
    );
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    assert!(
        h.calls("codex")[0]
            .stdin
            .contains("The controller is seat `boss`.")
    );
    let l = h.bus.push(
        "w1",
        note(Kind::Landed, "w1", 60, "[claude] Landed: demo#60"),
    );
    eventually("landed acked", || h.bus.events(l) == [Ev::Ack]).await;
    h.bus.set_state("mode", "drain");
    h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 61, "[claude] Assign: demo#61 Held"),
    );
    eventually("the gate change", || {
        !h.bus.sent_containing("mode is drain").is_empty()
    })
    .await;
    assert_eq!(h.bus.sent_containing("Cleanup: demo#60").len(), 1);
    let sent = h.bus.sent();
    assert!(sent.len() >= 3);
    for n in &sent {
        assert_eq!(n.recipient.as_str(), "boss", "{}", n.body);
    }
    stop(tx, j).await;
}

/// A note whose turns fail for real (a crash) is naked until the fifth failure, then
/// dead-lettered: terminated and sent, with the reason and the note, to the controller. The
/// count is kept across bridges of the host (PROTO-8).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_that_fails_five_times_is_dead_lettered() {
    let h = harness();
    h.script("codex", "echo boom >&2; exit 2");
    let (tx, j) = h.start(&["w1"]);
    let body = "[claude] Assign: demo#5 Five\n\nThe details that must not be lost.";
    let a = h.bus.push("w1", note(Kind::Assign, "w1", 5, body));
    let nak = Ev::Nak(Duration::from_millis(50));
    eventually("two real failures", || h.bus.events(a).len() == 2).await;
    stop(tx, j).await;
    // A restarted bridge goes on counting.
    let (tx, j) = h.start(&["w1"]);
    eventually("terminated at the fifth", || {
        h.bus.events(a).last() == Some(&Ev::Term)
    })
    .await;
    let events = h.bus.events(a);
    assert_eq!(
        events.iter().filter(|e| **e == nak).count(),
        4,
        "{events:?}"
    );
    assert_eq!(h.calls("codex").len(), 5);
    // The note is terminated first, then the dead letter is sent: wait for it.
    eventually("the dead letter", || {
        !h.bus.sent_containing("[bridge] Dead letter:").is_empty()
    })
    .await;
    let dead = h.bus.sent_containing("[bridge] Dead letter:");
    assert_eq!(dead.len(), 1);
    let d = &dead[0];
    assert_eq!((d.kind, d.recipient.as_str()), (Kind::Note, "claude"));
    assert_eq!((d.repo.as_deref(), d.issue), (Some("demo"), 5));
    assert!(d.body.contains("failed 5 times"), "{}", d.body);
    assert!(d.body.contains("exit 2"), "{}", d.body);
    assert!(
        d.body
            .contains(&format!("--- seq={a} from=claude to=w1 kind=assign demo#5")),
        "{}",
        d.body
    );
    assert!(d.body.contains("The details that must not be lost."));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        h.calls("codex").len(),
        5,
        "not retried after the dead letter"
    );
    stop(tx, j).await;
}

/// Carbon's check 3 (hgl-ai PR 1): a seat refused for quota with no binding left keeps its
/// notes held, alive and undelivered, and tries again; nothing is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quota_refusal_with_no_fallback_holds_the_notes() {
    let mut h = harness();
    h.opts.quota_retry = Duration::from_millis(400);
    let flag = h.calls.join("ok");
    h.script(
        "claude",
        &format!(
            "if [ -e '{ok}' ]; then cat '{good}'; else cat '{quota}'; exit 1; fi",
            ok = flag.display(),
            good = fixture("claude-ok.jsonl"),
            quota = fixture("claude-quota.synthetic.jsonl")
        ),
    );
    let (tx, j) = h.start(&["r1"]);
    let mut rv = note(Kind::Review, "r1", 6, "[claude] Review: demo#6");
    rv.head = Some(
        h.git
            .run(&h.seed, ["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string(),
    );
    let r = h.bus.push("r1", rv);
    eventually("refused twice", || h.calls("claude").len() >= 2).await;
    assert!(h.bus.events(r).is_empty(), "{:?}", h.bus.events(r));
    assert!(h.bus.progressed(r), "kept alive");
    let told = h.bus.sent_containing("has no fallback left");
    assert_eq!(told.len(), 1, "told once");
    std::fs::write(&flag, "").unwrap();
    eventually("acked once allowed", || h.bus.events(r) == [Ev::Ack]).await;
    stop(tx, j).await;
}

/// After a failover the seat returns to its primary binding once the cool-down is over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_over_seat_returns_to_its_primary_after_the_cool_down() {
    let mut h = harness();
    h.opts.failover_cooldown = Duration::from_millis(300);
    let flag = h.calls.join("ok");
    h.script(
        "codex",
        &format!(
            "if [ -e '{ok}' ]; then cat '{good}'; else cat '{quota}'; exit 1; fi",
            ok = flag.display(),
            good = fixture("codex-ok.jsonl"),
            quota = fixture("codex-quota.synthetic.jsonl")
        ),
    );
    h.script("agy", &format!("cat '{}'", fixture("agy-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 7, "[claude] Assign: demo#7 Seven"),
    );
    eventually("run on the fallback", || h.bus.events(a) == [Ev::Ack]).await;
    assert_eq!(h.calls("agy").len(), 1);
    std::fs::write(&flag, "").unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let q = h.bus.push(
        "w1",
        note(Kind::Question, "w1", 7, "[claude] Question: done?"),
    );
    eventually("run on the primary again", || h.bus.events(q) == [Ev::Ack]).await;
    assert_eq!(h.calls("agy").len(), 1);
    assert_eq!(h.calls("codex").len(), 2);
    assert_eq!(
        h.bus
            .sent_containing("to its primary codex gpt/gpt-6.2-astra after")
            .len(),
        1
    );
    // Both families worked on the task: its authors (ROLE-11).
    let tf = hgl_ai_tree::read_task(&h.work().join("w1/demo-7")).unwrap();
    assert_eq!(tf.families, [Family::Claude, Family::Gpt]);
    let rec = h.bus.trees()["build-1.w1.demo-7"].clone();
    assert_eq!(rec.families, [Family::Claude, Family::Gpt]);
    stop(tx, j).await;
}

/// A failover that would leave the task no reviewer of another family (ROLE-11) is not
/// taken: the seat holds its notes instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failover_that_breaks_role_9_is_not_taken() {
    let mut h = harness();
    h.opts.quota_retry = Duration::from_secs(3600);
    h.script(
        "codex",
        &format!("cat '{}'; exit 1", fixture("codex-quota.synthetic.jsonl")),
    );
    h.script("agy", &format!("cat '{}'", fixture("agy-ok.jsonl")));
    // The fleet's only reviewer is of the fallback's family, claude.
    h.bus.put_host(hgl_ai_proto::HostRecord {
        host: "build-2".into(),
        seats: vec![hgl_ai_proto::SeatRecord {
            seat: seat("r9"),
            role: Role::Reviewer,
            runtime: Runtime::Claude,
            family: Family::Claude,
            model: "claude-opus-5-5".into(),
        }],
        memcap_gb: 64,
        work_root_free_gb: 500,
        updated_at: hgl_ai_proto::utc_now_stamp(),
    });
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 8, "[claude] Assign: demo#8 Eight"),
    );
    eventually("held", || {
        !h.bus.sent_containing("has no fallback left").is_empty()
    })
    .await;
    let told = &h.bus.sent_containing("has no fallback left")[0];
    assert!(
        told.body
            .contains("every live reviewer family (claude) would then be among the task's authors"),
        "{}",
        told.body
    );
    assert!(h.calls("agy").is_empty());
    assert!(h.bus.events(a).is_empty());
    stop(tx, j).await;
}

/// A note of a task queued behind its `assign` joins the assign's turn in the new tree
/// rather than running in the inbox; under low disk, a review (a new tree) waits too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn follow_ups_join_their_tasks_tree_and_low_disk_holds_every_new_tree() {
    let h = harness();
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    h.script("claude", &format!("cat '{}'", fixture("claude-ok.jsonl")));
    h.bus.set_state("mode", "drain");
    let (tx, j) = h.start(&["w1", "r1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 9, "[claude] Assign: demo#9 Nine"),
    );
    let q = h.bus.push(
        "w1",
        note(
            Kind::Answer,
            "w1",
            9,
            "[claude] Answer: use the second form",
        ),
    );
    eventually("both held", || h.bus.progressed(a) && h.bus.progressed(q)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        h.calls("codex").is_empty(),
        "the answer waits with its assign"
    );
    h.bus.set_state("mode", "normal");
    eventually("one turn for both", || {
        h.bus.events(a) == [Ev::Ack] && h.bus.events(q) == [Ev::Ack]
    })
    .await;
    let calls = h.calls("codex");
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].cwd,
        h.work().join("w1/demo-9/src").display().to_string()
    );
    assert!(
        calls[0].stdin.contains("Note 2 of 2: answer"),
        "{}",
        calls[0].stdin
    );

    h.disk.0.store(crate::disk::GB, Ordering::SeqCst);
    let head = h
        .git
        .run(&h.seed, ["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let mut rv = note(Kind::Review, "r1", 10, "[claude] Review: demo#10");
    rv.head = Some(head);
    let r = h.bus.push("r1", rv);
    eventually("the review held", || h.bus.progressed(r)).await;
    assert!(h.calls("claude").is_empty());
    assert!(!h.work().join("r1/demo-10-review").exists());
    h.disk.0.store(1000 * crate::disk::GB, Ordering::SeqCst);
    eventually("reviewed once there is room", || {
        h.bus.events(r) == [Ev::Ack]
    })
    .await;
    stop(tx, j).await;
}

/// A tree that cannot be prepared (the clone cannot be fetched) holds its notes, alive and
/// undelivered, tells the controller once, and is tried again until it works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tree_that_cannot_be_prepared_yet_is_tried_again() {
    let mut h = harness();
    h.opts.prep_retry = Duration::from_millis(100);
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let mut n = note(Kind::Assign, "w1", 11, "[claude] Assign: later#11 Eleven");
    n.repo = Some("later".into());
    let a = h.bus.push("w1", n);
    eventually("held after failed preparations", || {
        h.bus.progressed(a)
            && !h
                .bus
                .sent_containing("could not prepare later#11")
                .is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(h.bus.events(a).is_empty(), "{:?}", h.bus.events(a));
    assert_eq!(
        h.bus.sent_containing("could not prepare").len(),
        1,
        "told once"
    );
    // The repository appears: the next try makes the tree and runs.
    remote(&h.git, &h.root, "later");
    eventually("run once it heals", || h.bus.events(a) == [Ev::Ack]).await;
    stop(tx, j).await;
}

/// hgl-ai#15: a `build` note at an agent seat starts no turn; it is terminated and its
/// sender told.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_note_at_an_agent_seat_is_refused() {
    let h = harness();
    h.script("codex", "echo should-not-run");
    let (tx, j) = h.start(&["w1"]);
    let mut n = Note::new(seat("w2"), seat("w1"), Kind::Build, "{\"v\":1}");
    n.repo = Some("demo".into());
    n.head = Some("a".repeat(40));
    let b = h.bus.push("w1", n);
    eventually("terminated", || h.bus.events(b) == [Ev::Term]).await;
    eventually("the sender told", || {
        !h.bus.sent_containing("[bridge] Not a builder").is_empty()
    })
    .await;
    let told = &h.bus.sent_containing("[bridge] Not a builder")[0];
    assert_eq!((told.recipient.as_str(), told.kind), ("w2", Kind::Note));
    assert!(
        told.body.contains("hgl-ai build run --platform"),
        "{}",
        told.body
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.calls("codex").is_empty(), "no turn");
    stop(tx, j).await;
}

/// The bot's P1 (hgl-ai PR 1): the hub stops answering (a partition), so no refresh says
/// the lease is lost while it runs out. The seat stops, its turn and notes with it, once the
/// lease has gone unrefreshed for half the bucket's `max_age`: before it can expire and
/// another host take it. Until the hub answers again the seat cannot be taken again; then
/// the bridge serves it again.
///
/// hgl-ai#41: the bounds are measured from the last refresh the hub confirmed (the fake
/// records it; no refresh succeeds once `refresh_down` returns), not from when the test cut
/// the hub, and the seat's note waits for its notice, which the bridge sends after handing
/// the notes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unrefreshed_lease_stops_the_seat_before_it_can_expire() {
    let h = harness();
    let max_age = Duration::from_secs(3);
    h.bus.set_lease_max_age(max_age);
    let pid = h.calls.join("pid");
    // Publish the pid only after it is complete: redirection creates an empty file first.
    h.script(
        "codex",
        &format!(
            "if [ \"$n\" -eq 1 ]; then echo $$ > '{p}.new' && mv '{p}.new' '{p}'; sleep 60; fi\ncat '{f}'",
            p = pid.display(),
            f = fixture("codex-ok.jsonl")
        ),
    );
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 60, "[claude] Assign: demo#60 Sixty"),
    );
    eventually("the turn runs", || pid.exists()).await;
    let agent = pid_in(&pid);
    h.bus.refresh_down(true);
    let confirmed = h
        .bus
        .lease_confirmed_at("seat.w1")
        .expect("the seat lease held");
    eventually("the turn stopped", || !alive(agent)).await;
    let took = confirmed.elapsed();
    assert!(
        took < max_age,
        "stopped {took:?} after the last refresh, when the lease could have expired"
    );
    assert!(
        took >= max_age / 2,
        "not before the deadline: {took:?} after the last refresh"
    );
    eventually("its note handed back", || {
        h.bus.events(a).first() == Some(&Ev::Nak(Duration::from_millis(50)))
    })
    .await;
    eventually("the seat told", || {
        !h.bus.sent_containing("was not refreshed for").is_empty()
    })
    .await;
    // While the hub does not answer the seat cannot be taken again: nothing more runs.
    eventually("a take refused", || h.bus.lease_takes_refused() > 0).await;
    assert_eq!(h.calls("codex").len(), 1, "one turn while the hub is down");
    h.bus.refresh_down(false);
    eventually("served again, the note run", || {
        h.bus.events(a).last() == Some(&Ev::Ack)
    })
    .await;
    assert_eq!(h.calls("codex").len(), 2);
    assert_eq!(
        h.bus.sent_containing("was not refreshed for").len(),
        1,
        "told once"
    );
    stop(tx, j).await;
}

/// hgl-ai#65: the hub stalls silently, so every refresh hangs until it times out (`every`).
/// The refreshes run at once, so with any number of seats each stops before its lease can
/// expire. Measure each seat from its last confirmed write to its actual stop, rather than
/// from the stall to when the test happens to observe all the notices.
async fn hanging_refreshes_stop_every_seat_before_its_lease_can_expire(n: usize) {
    let mut h = harness();
    let max_age = Duration::from_secs(3);
    let every = max_age / 6;
    h.bus.set_lease_max_age(max_age);
    h.opts.heartbeat_every = every;
    let names: Vec<String> = (1..=n).map(|i| format!("w{i}")).collect();
    let w = h.cfg.seats[0].clone();
    h.cfg.seats = names
        .iter()
        .map(|name| {
            let mut s = w.clone();
            s.name = seat(name);
            s
        })
        .collect();
    h.cfg.validate().unwrap();
    let only: Vec<&str> = names.iter().map(String::as_str).collect();
    let (tx, j) = h.start(&only);
    eventually("every seat served", || {
        h.bus.last_host().is_some_and(|r| r.seats.len() == n)
            && names
                .iter()
                .all(|s| h.bus.lease_holder(&format!("seat.{s}")).is_some())
    })
    .await;
    let confirmed = h.bus.refresh_hangs(true);
    eventually("every seat stopped", || {
        let told = h.bus.sent_containing("was not refreshed for");
        names
            .iter()
            .all(|s| told.iter().any(|t| t.sender.as_str() == s))
    })
    .await;
    let stops = h.bus.lease_stops();
    for name in &names {
        let last = confirmed[&format!("seat.{name}")];
        let (start, end) = stops[&seat(name)];
        let start = start.duration_since(last);
        let end = end.expect("seat stopped").duration_since(last);
        assert!(
            start >= max_age / 2,
            "{name}: stopped too early after {start:?}"
        );
        assert!(
            end < max_age,
            "{name}: stopped after {end:?}, when its lease could have expired"
        );
    }
    h.bus.refresh_hangs(false);
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hanging_refresh_stops_one_seat_before_its_lease_can_expire() {
    hanging_refreshes_stop_every_seat_before_its_lease_can_expire(1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hanging_refreshes_stop_three_seats_before_their_leases_can_expire() {
    hanging_refreshes_stop_every_seat_before_its_lease_can_expire(3).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hanging_refreshes_stop_twenty_seats_before_their_leases_can_expire() {
    hanging_refreshes_stop_every_seat_before_its_lease_can_expire(20).await;
}

/// The bot's P2: a removal on `landed` that fails (here the salvage patch cannot be written)
/// is not forgotten once the end is recorded: it is tried again until it works, and
/// reported then.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_removal_is_tried_again_until_it_works() {
    let mut h = harness();
    h.opts.cleanup_retry = Duration::from_millis(200);
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 61, "[claude] Assign: demo#61 Sixty-one"),
    );
    eventually("assign acked", || h.bus.events(a) == [Ev::Ack]).await;
    let dir = h.work().join("w1/demo-61");
    std::fs::write(dir.join("src/wip.txt"), "unsaved\n").unwrap();
    // No salvage directory can be made: the patch cannot be written, so nothing is removed.
    let salvage = h.work().join("salvage");
    std::fs::write(&salvage, "not a directory").unwrap();
    let l = h.bus.push(
        "w1",
        note(Kind::Landed, "w1", 61, "[claude] Landed: demo#61"),
    );
    eventually("landed acked", || h.bus.events(l) == [Ev::Ack]).await;
    assert_eq!(h.bus.ended("w1", "demo", 61), Some(l), "the end stands");
    let told = h.bus.sent_containing("Cleanup: demo#61");
    assert_eq!(told.len(), 1);
    assert!(told[0].body.contains("tried again"), "{}", told[0].body);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(dir.exists(), "still owed");
    assert!(h.bus.trees().contains_key("build-1.w1.demo-61"));
    // It can be written again: the next try removes the tree, salvages the work, says so.
    std::fs::remove_file(&salvage).unwrap();
    eventually("removed on a later try", || !dir.exists()).await;
    eventually("reported", || {
        !h.bus.sent_containing("removed demo-61 at try").is_empty()
    })
    .await;
    assert!(!h.bus.trees().contains_key("build-1.w1.demo-61"));
    let patches: Vec<_> = std::fs::read_dir(&salvage).unwrap().flatten().collect();
    assert_eq!(patches.len(), 1);
    stop(tx, j).await;
    let owed = h.root.join("data/bridge/cleanup/w1.json");
    assert_eq!(std::fs::read_to_string(owed).unwrap().trim(), "[]");
}

/// The bot's P2: under `drain` and low disk at once, low disk rules: a review, which drain
/// lets through, makes no tree on the full disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn low_disk_holds_new_trees_under_drain_too() {
    let h = harness();
    h.script("claude", &format!("cat '{}'", fixture("claude-ok.jsonl")));
    h.bus.set_state("mode", "drain");
    h.disk.0.store(crate::disk::GB, Ordering::SeqCst);
    let (tx, j) = h.start(&["r1"]);
    let mut rv = note(Kind::Review, "r1", 62, "[claude] Review: demo#62");
    rv.head = Some(
        h.git
            .run(&h.seed, ["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string(),
    );
    let r = h.bus.push("r1", rv);
    eventually("the review held", || h.bus.progressed(r)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(h.calls("claude").is_empty());
    assert!(!h.work().join("r1/demo-62-review").exists());
    assert_eq!(h.bus.sent_containing("below min_free_gb").len(), 1);
    assert!(h.bus.sent_containing("mode is drain").is_empty());
    // Room again, still draining: the review is in-flight work and runs.
    h.disk.0.store(1000 * crate::disk::GB, Ordering::SeqCst);
    eventually("reviewed under drain", || h.bus.events(r) == [Ev::Ack]).await;
    stop(tx, j).await;
}

mod reuse;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn author_registration_failure_blocks_and_retries_before_starting() {
    let mut h = harness();
    h.opts.prep_retry = Duration::from_millis(100);
    h.script("codex", &format!("cat '{}'", fixture("codex-ok.jsonl")));
    h.bus.set_tree_put_down(true);
    let (tx, j) = h.start(&["w1"]);
    let seq = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 981, "[claude] Assign: demo#981"),
    );
    eventually("registration failure", || {
        !h.bus
            .sent_containing("could not prepare demo#981")
            .is_empty()
    })
    .await;
    assert!(h.calls("codex").is_empty());
    assert!(h.bus.events(seq).is_empty());
    h.bus.set_tree_put_down(false);
    eventually("turn after registration", || h.bus.events(seq) == [Ev::Ack]).await;
    assert_eq!(h.calls("codex").len(), 1);
    assert!(
        h.bus
            .trees()
            .values()
            .any(|r| r.task.issue == 981 && r.families.contains(&Family::Gpt))
    );
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacement_review_waits_for_disk_before_removing_old_tree() {
    let h = harness();
    h.script("claude", &format!("cat '{}'", fixture("claude-ok.jsonl")));
    let (tx, j) = h.start(&["r1"]);
    let head = h
        .git
        .run(&h.seed, ["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let mut review = note(Kind::Review, "r1", 982, "[claude] Review: demo#982");
    review.head = Some(head);
    let first = h.bus.push("r1", review.clone());
    eventually("first review", || h.bus.events(first) == [Ev::Ack]).await;
    let marker = h.work().join("r1/demo-982-review/src/kept-marker");
    std::fs::write(&marker, "keep").unwrap();
    h.disk.0.store(crate::disk::GB, Ordering::SeqCst);
    let second = h.bus.push("r1", review);
    eventually("replacement held", || h.bus.progressed(second)).await;
    assert!(marker.exists());
    assert_eq!(h.calls("claude").len(), 1);
    h.disk.0.store(1000 * crate::disk::GB, Ordering::SeqCst);
    eventually("replacement runs", || h.bus.events(second) == [Ev::Ack]).await;
    assert_eq!(h.calls("claude").len(), 2);
    assert!(!marker.exists());
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refusal_after_tool_activity_keeps_the_author() {
    let h = harness();
    h.script("codex", &format!("echo '{{\"type\":\"item.started\",\"item\":{{\"type\":\"command_execution\"}}}}'; cat '{}'; exit 1", fixture("codex-quota.synthetic.jsonl")));
    h.script("agy", &format!("cat '{}'", fixture("agy-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let seq = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 982, "[claude] Assign: demo#982"),
    );
    eventually("fallback completed", || h.bus.events(seq) == [Ev::Ack]).await;
    let f = hgl_ai_tree::read_task(&h.work().join("w1/demo-982")).unwrap();
    assert!(f.families.contains(&Family::Gpt));
    assert!(f.families.contains(&Family::Claude));
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollback_publication_failure_holds_notes_and_author() {
    let mut h = harness();
    h.opts.prep_retry = Duration::from_secs(60);
    let flag = h.root.join("release");
    h.script(
        "codex",
        &format!(
            "while [ ! -e '{}' ]; do sleep 0.05; done; cat '{}'; exit 1",
            flag.display(),
            fixture("codex-quota.synthetic.jsonl")
        ),
    );
    let (tx, j) = h.start(&["w1"]);
    let seq = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 983, "[claude] Assign: demo#983"),
    );
    eventually("started", || !h.calls("codex").is_empty()).await;
    h.bus.set_tree_put_down(true);
    std::fs::write(flag, "").unwrap();
    eventually("rollback failed", || {
        !h.bus
            .sent_containing("registering the family rollback")
            .is_empty()
    })
    .await;
    assert!(h.bus.events(seq).is_empty());
    assert!(h.calls("agy").is_empty());
    assert!(
        hgl_ai_tree::read_task(&h.work().join("w1/demo-983"))
            .unwrap()
            .families
            .contains(&Family::Gpt)
    );
    assert!(
        h.bus.trees()["build-1.w1.demo-983"]
            .families
            .contains(&Family::Gpt)
    );
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_family_fallback_after_refusal_still_checks_reviewers() {
    let mut h = harness();
    h.opts.quota_retry = Duration::from_secs(3600);
    h.cfg.seats[0].fallback[0].family = Some(Family::Gpt);
    h.script(
        "codex",
        &format!("cat '{}'; exit 1", fixture("codex-quota.synthetic.jsonl")),
    );
    h.bus.put_host(hgl_ai_proto::HostRecord {
        host: "build-2".into(),
        seats: vec![hgl_ai_proto::SeatRecord {
            seat: seat("r9"),
            role: Role::Reviewer,
            runtime: Runtime::Codex,
            family: Family::Gpt,
            model: "test".into(),
        }],
        memcap_gb: 64,
        work_root_free_gb: 500,
        updated_at: hgl_ai_proto::utc_now_stamp(),
    });
    let (tx, j) = h.start(&["w1"]);
    let seq = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 984, "[claude] Assign: demo#984"),
    );
    eventually("fallback blocked", || {
        !h.bus.sent_containing("has no fallback left").is_empty()
    })
    .await;
    assert!(h.bus.events(seq).is_empty());
    assert!(
        hgl_ai_tree::read_task(&h.work().join("w1/demo-984"))
            .unwrap()
            .families
            .is_empty()
    );
    assert!(h.bus.trees()["build-1.w1.demo-984"].families.is_empty());
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recorded_authors_are_checked_for_failover() {
    let mut h = harness();
    h.opts.quota_retry = Duration::from_secs(3600);
    h.script(
        "codex",
        &format!(
            "if [ \"$n\" = 1 ]; then cat '{}'; else cat '{}'; exit 1; fi",
            fixture("codex-ok.jsonl"),
            fixture("codex-quota.synthetic.jsonl")
        ),
    );
    h.bus.put_host(hgl_ai_proto::HostRecord {
        host: "build-2".into(),
        seats: vec![
            hgl_ai_proto::SeatRecord {
                seat: seat("r8"),
                role: Role::Reviewer,
                runtime: Runtime::Codex,
                family: Family::Gpt,
                model: "test".into(),
            },
            hgl_ai_proto::SeatRecord {
                seat: seat("r9"),
                role: Role::Reviewer,
                runtime: Runtime::Claude,
                family: Family::Claude,
                model: "test".into(),
            },
        ],
        memcap_gb: 64,
        work_root_free_gb: 500,
        updated_at: hgl_ai_proto::utc_now_stamp(),
    });
    let (tx, j) = h.start(&["w1"]);
    let seq = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 985, "[claude] Assign: demo#985"),
    );
    eventually("first turn", || h.bus.events(seq) == [Ev::Ack]).await;
    let next = h.bus.push(
        "w1",
        note(Kind::Question, "w1", 985, "[claude] Question: continue"),
    );
    eventually("fallback blocked", || {
        !h.bus.sent_containing("has no fallback left").is_empty()
    })
    .await;
    assert!(h.bus.events(next).is_empty());
    assert!(h.calls("agy").is_empty());
    assert_eq!(
        hgl_ai_tree::read_task(&h.work().join("w1/demo-985"))
            .unwrap()
            .families,
        [Family::Gpt]
    );
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refusal_after_checkout_change_keeps_the_author() {
    let h = harness();
    h.script(
        "codex",
        &format!(
            "echo changed > work.txt; cat '{}'; exit 1",
            fixture("codex-quota.synthetic.jsonl")
        ),
    );
    h.script("agy", &format!("cat '{}'", fixture("agy-ok.jsonl")));
    let (tx, j) = h.start(&["w1"]);
    let seq = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 986, "[claude] Assign: demo#986"),
    );
    eventually("fallback completed", || h.bus.events(seq) == [Ev::Ack]).await;
    let f = hgl_ai_tree::read_task(&h.work().join("w1/demo-986")).unwrap();
    assert!(f.families.contains(&Family::Gpt));
    stop(tx, j).await;
}
