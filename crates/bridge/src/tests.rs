//! The seat loop against the fake bus, a local bare "remote", and fake agent CLIs
//! (shell scripts that print fixture output).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hgl_ai_config::{Binding, Config, SeatConfig};
use hgl_ai_proto::{Family, Kind, Note, Role, Runtime, Seat, Tier};
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
        },
        SeatConfig {
            name: seat("r1"),
            role: Role::Reviewer,
            binding: b(Runtime::Claude, None, Tier::Frontier, Some("xhigh")),
            fallback: vec![],
            memcap_gb: None,
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
            "#!/bin/sh\nC='{c}'\nn=$(cat \"$C/n\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"$C/n\"\nd=\"$C/$n\"; mkdir -p \"$d\"\nprintf '%s\\n' \"$@\" > \"$d/args\"\npwd > \"$d/cwd\"\nenv > \"$d/env\"\ncat > \"$d/stdin\"\n{body}\n",
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

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 checks existence only; a zombie does not count.
    let exists = unsafe { libc::kill(pid, 0) == 0 };
    exists
        && std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "))
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
    assert_eq!(&c.args[..4], ["exec", "--json", "-m", "gpt-6.2-astra"]);
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
    assert_eq!(tf.branch.as_deref(), Some("codex/980-fix-the-thing"));
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
    let (tx, j) = h.start(&["w1"]);
    let a = h.bus.push(
        "w1",
        note(Kind::Assign, "w1", 3, "[claude] Assign: demo#3 Three"),
    );
    eventually("nak then ack", || {
        h.bus.events(a) == [Ev::Nak(Duration::from_millis(50)), Ev::Ack]
    })
    .await;
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
        h.bus.sent_containing("taking `assign` notes again").len() == 1
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
    eventually("naked while the hub cannot say", || {
        h.bus.events(a).first() == Some(&Ev::Nak(Duration::from_millis(50)))
    })
    .await;
    assert!(h.calls("codex").is_empty());
    h.bus.ended_down(false, false);
    eventually("run once the hub answers", || {
        h.bus.events(a).last() == Some(&Ev::Ack)
    })
    .await;
    assert_eq!(h.calls("codex").len(), 1);

    // The end cannot be recorded: the tree goes at once, the note is naked and retried.
    h.bus.ended_down(false, true);
    let l = h.bus.push(
        "w1",
        note(Kind::Landed, "w1", 22, "[claude] Landed: demo#22"),
    );
    eventually("landed naked", || {
        h.bus.events(l).first() == Some(&Ev::Nak(Duration::from_millis(50)))
    })
    .await;
    assert!(!h.work().join("w1/demo-22").exists());
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
    let ours = format!("build-1 pid {} (bridge)", std::process::id());
    assert_eq!(h.bus.lease_holder("seat.w1"), Some(ours));
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
    let ours = format!("build-1 pid {} (bridge)", std::process::id());
    assert_eq!(h.bus.lease_holder("seat.w1"), Some(ours.clone()));
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
    assert_eq!(h.bus.lease_holder("seat.w1"), Some(ours));
    stop(tx, j).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_seat_lease_stops_the_seat() {
    let h = harness();
    let pid = h.calls.join("pid");
    h.script("codex", &format!("echo $$ > '{}'; sleep 60", pid.display()));
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
    let held = FileLock::turn(&h.work().join("w1/demo-7/turn.lock"))
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
            "case \"$(pwd)\" in *demo-11*) while [ ! -e '{g}' ]; do sleep 0.05; done;; esac\ncat '{f}'",
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
    h.script("codex", &format!("echo $$ > '{}'; sleep 60", pid.display()));
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
    h.script("codex", &format!("echo $$ > '{}'; sleep 60", pid.display()));
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
    assert_eq!(host.memcap_gb, 64);
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
    });
    let trees = Trees::with(
        Layout::new(h.work()).unwrap(),
        "build-1",
        &cfg.github,
        h.git.clone(),
    );
    let deps = Deps {
        closed: Arc::new(|_: &str, _: u64, _: bool| None),
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
            "if [ \"$n\" -ge 2 ]; then echo $$ > '{p}'; while [ ! -e '{g}' ]; do sleep 0.05; done; fi\ncat '{f}'",
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

/// A `landed` whose end could not be recorded (naked) is delivered again after a newer
/// `assign` restarted the task: the restart is left alone, and the end is then recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_end_redelivered_before_it_was_recorded_leaves_a_restart_alone() {
    let mut h = harness();
    // Room for the restart to start before `max_deliver` runs out.
    h.opts.nak_delay = Duration::from_secs(1);
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
    eventually("landed naked", || {
        h.bus.events(l).first() == Some(&Ev::Nak(Duration::from_secs(1)))
    })
    .await;
    let dir = h.work().join("w1/demo-41");
    assert!(!dir.exists());
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
            "if [ \"$n\" -eq 2 ]; then echo $$ > '{p}'; sleep 60; fi\ncat '{f}'",
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
