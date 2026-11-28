//! The ssh executor and the proxy builder against a fake `ssh` (`tests/support/fake-ssh`)
//! that runs each remote command on this host, standing in for the remote: checkout,
//! steps, streaming, the time limit's kill, cleanup, a dropped connection and its
//! reconnect, an unreachable remote holding its jobs, and shutdown. The remote shell here
//! is POSIX `sh`; that PowerShell runs the Windows scripts as intended is checked only
//! against a real Windows host. The service tests need `nats-server` on `PATH`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hgl_ai_build::driver::{self, Meta, Options};
use hgl_ai_build::exec::ssh::{Conn, Dialect, Ssh};
use hgl_ai_build::exec::{Cancel, Checkout, Executor, Prep, StepSpec};
use hgl_ai_build::recipe::Recipe;
use hgl_ai_build::service::{self, Settings};
use hgl_ai_build::store;
use hgl_ai_bus::Bus;
use hgl_ai_proto::build::{BUILD_VERSION, BuildJob, BuildStatus, Platform, reply_seat};
use hgl_ai_proto::{Kind, Note, Seat};

fn fake_ssh() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-ssh")
}

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
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// A scratch tree: the origin repository, the remote's state and work directory, and the
/// proxy's directories. Short paths, for the control socket.
struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    origin: String,
    head: String,
    state: PathBuf,
    remote: PathBuf,
}

impl World {
    fn new() -> World {
        let tmp = tempfile::Builder::new().prefix("hb").tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let bare = root.join("origin.git");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--quiet", "--bare", "-b", "main"]);
        let seed = root.join("seed");
        git(&root, &["clone", "--quiet", bare.to_str().unwrap(), "seed"]);
        git(&seed, &["checkout", "--quiet", "-b", "main"]);
        std::fs::write(seed.join("main.c"), "int main(void) { return 0; }\n").unwrap();
        git(&seed, &["add", "main.c"]);
        git(&seed, &["commit", "--quiet", "-m", "initial"]);
        git(&seed, &["push", "--quiet", "origin", "main"]);
        let head = git(&seed, &["rev-parse", "HEAD"]);
        let state = root.join("winbox");
        let remote = root.join("remote");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        World {
            origin: bare.display().to_string(),
            _tmp: tmp,
            root,
            head,
            state,
            remote,
        }
    }

    fn conn(&self) -> Arc<Conn> {
        Conn::new(
            "win1",
            self.state.to_str().unwrap(),
            fake_ssh(),
            &self.root.join("ssh"),
            Dialect::Posix,
        )
        .unwrap()
    }

    fn checkout(&self, id: &str) -> Checkout {
        Checkout {
            id: id.into(),
            repo: "demo".into(),
            url: self.origin.clone(),
            head: self.head.clone(),
            base: None,
            issue: 0,
            shell: false,
        }
    }

    fn down(&self, down: bool) {
        let p = self.state.join("down");
        if down {
            std::fs::write(p, "").unwrap();
        } else {
            let _ = std::fs::remove_file(p);
        }
    }

    fn jobs(&self) -> Vec<String> {
        std::fs::read_dir(self.remote.join("jobs"))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn step(name: &str, script: &str, time: Duration) -> StepSpec {
    StepSpec {
        name: name.into(),
        script: script.into(),
        cwd: ".".into(),
        env: vec![("GREETING".into(), "it's here".into())],
        mem_bytes: 1 << 30,
        time,
        address_limit: true,
    }
}

#[test]
fn ssh_executor_checks_out_runs_streams_kills_and_cleans_up() {
    let w = World::new();
    let conn = w.conn();
    conn.connect().unwrap();
    assert!(
        w.state.join("master").is_file(),
        "the proxy owns a master connection"
    );
    let ex = Ssh::new(
        Platform::Windows,
        Dialect::Posix,
        w.remote.to_str().unwrap(),
        BTreeMap::new(),
        conn.clone(),
    );

    let ws = ex.prepare(&w.checkout("20261127-083000-aaaaaa")).unwrap();
    assert_eq!(
        std::fs::read_to_string(format!("{}/main.c", ws.src)).unwrap(),
        "int main(void) { return 0; }\n"
    );
    assert!(
        std::fs::read_to_string(w.state.join("log"))
            .unwrap()
            .contains("git -C"),
        "git ran on the remote"
    );

    let logs = w.root.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    // Streaming: stdout and stderr together, the pid line kept out, the environment set.
    let log = logs.join("out.log");
    let r = ex.run_step(
        &ws,
        &step(
            "out",
            "echo \"$GREETING\"; echo oops >&2; test -n \"$TEMP\"; exit 3",
            Duration::from_secs(30),
        ),
        &log,
        &Cancel::default(),
    );
    assert_eq!(
        (r.exit, r.limit, r.unreachable),
        (Some(3), None, false),
        "{r:?}"
    );
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("it's here\n") && text.contains("oops\n"),
        "{text}"
    );
    assert!(!text.contains("hgl-ai-pid"), "{text}");
    assert_eq!(r.cap, None, "no memory cap on a remote");

    // The time limit kills the remote tree.
    let pidfile = w.root.join("sleep.pid");
    let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
    let t0 = Instant::now();
    let r = ex.run_step(
        &ws,
        &step("slow", &script, Duration::from_secs(1)),
        &logs.join("slow.log"),
        &Cancel::default(),
    );
    assert_eq!(r.limit, Some("time"), "{r:?}");
    assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    assert!(!alive(&pid), "the remote child is gone");

    // A stop before the remote's pid has arrived still kills the remote tree.
    let (shell_pid, sleep_pid) = (
        w.root.join("early-shell.pid"),
        w.root.join("early-sleep.pid"),
    );
    let script = format!(
        "echo $$ > {}; sleep 30 & echo $! > {}; wait",
        shell_pid.display(),
        sleep_pid.display()
    );
    let t0 = Instant::now();
    let r = ex.run_step(
        &ws,
        &step("early", &script, Duration::ZERO),
        &logs.join("early.log"),
        &Cancel::default(),
    );
    assert_eq!(r.limit, Some("time"), "{r:?}");
    assert!(
        t0.elapsed() < Duration::from_secs(8),
        "killed, not left to the grace: {:?}",
        t0.elapsed()
    );
    std::thread::sleep(Duration::from_millis(500));
    for f in [&shell_pid, &sleep_pid] {
        if let Ok(pid) = std::fs::read_to_string(f) {
            assert!(!alive(&pid), "{} is still running", f.display());
        }
    }

    // Cancel does the same.
    let cancel = Cancel::default();
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        c2.cancel();
    });
    let r = ex.run_step(
        &ws,
        &step("stop", "sleep 30", Duration::from_secs(60)),
        &logs.join("stop.log"),
        &cancel,
    );
    assert!(r.stopped, "{r:?}");

    // Artifacts come back to the proxy.
    let env = vec![("HGL_AI_BUILD_DIR".to_string(), ws.build.clone())];
    let mut s = step(
        "report",
        &format!(
            "echo '<testsuite><testcase name=\"a\"/></testsuite>' > {}/r.xml",
            ws.build
        ),
        Duration::from_secs(30),
    );
    s.env = env.clone();
    assert_eq!(
        ex.run_step(&ws, &s, &logs.join("report.log"), &Cancel::default())
            .exit,
        Some(0)
    );
    let got = ex.collect(
        &ws,
        ".",
        &["$HGL_AI_BUILD_DIR/*.xml".to_string()],
        &env,
        &logs.join("artifacts"),
        &Cancel::default(),
    );
    assert_eq!(got.len(), 1);
    assert!(
        std::fs::read_to_string(&got[0])
            .unwrap()
            .contains("<testcase")
    );
    // An artifact over the budget is skipped, never read whole.
    std::fs::write(format!("{}/big.bin", ws.build), vec![b'x'; 5000]).unwrap();
    std::fs::write(format!("{}/small.bin", ws.build), vec![b'y'; 100]).unwrap();
    let small = Ssh::new(
        Platform::Windows,
        Dialect::Posix,
        w.remote.to_str().unwrap(),
        BTreeMap::new(),
        conn.clone(),
    )
    .with_artifact_limit(1000);
    let got = small.collect(
        &ws,
        ".",
        &["$HGL_AI_BUILD_DIR/*.bin".to_string()],
        &env,
        &logs.join("bins"),
        &Cancel::default(),
    );
    assert_eq!(got, [logs.join("bins/small.bin")]);
    assert!(!logs.join("bins/big.bin").exists());

    // Probes and needs.
    let p = ex.probe(&["git".into(), "no-such-tool".into()]).unwrap();
    assert!(p.toolchains["git"].starts_with("git version"), "{p:?}");
    assert!(!p.toolchains.contains_key("no-such-tool"));
    assert!(p.free_gb.is_some());
    assert!(ex.has_tool("git").unwrap());

    // Cleanup removes the job directory and the worktree.
    ex.cleanup(&ws).unwrap();
    assert!(w.jobs().is_empty(), "{:?}", w.jobs());
    let wt = git(&w.remote.join("clones/demo"), &["worktree", "list"]);
    assert_eq!(wt.lines().count(), 1, "{wt}");

    // A head the origin lacks is not run.
    let mut c = w.checkout("20261127-083000-bbbbbb");
    c.head = "0123456789abcdef0123456789abcdef01234567".into();
    assert!(matches!(ex.prepare(&c), Err(Prep::NotRun(r)) if r.contains("not on origin")));

    // The whole driver, parsing on the proxy's side.
    let ws = ex.prepare(&w.checkout("20261127-083000-cccccc")).unwrap();
    let recipe = Recipe::parse(
        "repo = \"demo\"\n[[step]]\nname = \"ok\"\nrun = \"test -f main.c\"\n[[step]]\nname = \"cc\"\nrun = \"echo 'main.c:1:5: error: expected ; before }' >&2; exit 1\"\nparser = \"gcc\"\n",
    )
    .unwrap();
    let meta = Meta {
        id: "20261127-083000-cccccc".into(),
        repo: "demo".into(),
        head: Some(w.head.clone()),
        host: "win1".into(),
        proxy: Some("lab1".into()),
        ..Meta::default()
    };
    let opts = Options {
        env: driver::build_env(&meta.id, &ws),
        log_dir: logs.join(&meta.id),
        ..Options::default()
    };
    let d = driver::drive(
        &ex,
        &ws,
        &recipe,
        &meta,
        &opts,
        &Cancel::default(),
        &mut |_| {},
    );
    ex.cleanup(&ws).unwrap();
    let s = d.summary;
    assert_eq!(s.status, BuildStatus::Fail);
    assert_eq!(
        (s.executor.as_str(), s.platform),
        ("ssh", Platform::Windows)
    );
    assert_eq!(s.errors[0].message, "expected ; before }");
    assert_eq!(s.errors[0].line, Some(1));

    conn.close();
    assert!(!w.state.join("master").exists(), "the master is closed");
}

#[test]
fn a_drop_during_a_step_is_not_a_build_failure() {
    let w = World::new();
    let conn = w.conn();
    conn.connect().unwrap();
    let ex = Ssh::new(
        Platform::Windows,
        Dialect::Posix,
        w.remote.to_str().unwrap(),
        BTreeMap::new(),
        conn.clone(),
    );
    let ws = ex.prepare(&w.checkout("20261127-083000-dddddd")).unwrap();
    let pidfile = w.root.join("orphan.pid");
    let state = w.state.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        std::fs::write(state.join("down"), "").unwrap();
    });
    let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
    let r = ex.run_step(
        &ws,
        &step("long", &script, Duration::from_secs(60)),
        &w.root.join("long.log"),
        &Cancel::default(),
    );
    assert!(r.unreachable, "{r:?}");
    assert!(!conn.is_reachable());
    assert!(ex.cleanup(&ws).is_err(), "cleanup waits for the remote");
    let orphan = std::fs::read_to_string(&pidfile).unwrap();
    assert!(alive(&orphan), "the remote went on without us");
    w.down(false);
    conn.connect().unwrap();
    ex.cleanup(&ws).unwrap();
    assert!(!alive(&orphan), "the orphaned tree is killed on reconnect");
    assert!(w.jobs().is_empty());
    conn.close();
}

struct Nats(Child, #[allow(dead_code)] tempfile::TempDir, String, String);

impl Drop for Nats {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn nats() -> Option<Nats> {
    let bin = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("nats-server"))
        .find(|p| p.is_file());
    let Some(bin) = bin else {
        eprintln!("skipped: nats-server is not on PATH");
        return None;
    };
    let dir = tempfile::Builder::new().prefix("hbn").tempdir().unwrap();
    let token = format!("proxy{}", std::process::id());
    let child = Command::new(bin)
        .args(["-js", "-a", "127.0.0.1", "-p", "-1", "--auth", &token])
        .arg("-sd")
        .arg(dir.path().join("store"))
        .arg("--ports_file_dir")
        .arg(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let ports = dir.path().join(format!("nats-server_{}.ports", child.id()));
    let deadline = Instant::now() + Duration::from_secs(15);
    let url = loop {
        if let Some(u) = std::fs::read_to_string(&ports)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["nats"][0].as_str().map(String::from))
        {
            break u;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    };
    Some(Nats(child, dir, url, token))
}

async fn until<F: std::future::Future<Output = bool>>(
    what: &str,
    secs: u64,
    mut f: impl FnMut() -> F,
) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !f().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn job(id: &str, head: &str) -> (Note, Seat) {
    let reply = reply_seat(id).unwrap();
    let j = BuildJob {
        v: BUILD_VERSION,
        id: id.into(),
        repo: "demo".into(),
        head: head.into(),
        base: None,
        issue: 0,
        steps: None,
        recipe_rev: None,
        keep_going: false,
        verdict: None,
        reply_to: reply.clone(),
        notify: None,
        requester: "w1".into(),
    };
    let mut n = Note::new(
        Seat::new("w1").unwrap(),
        Seat::new("builder-win1").unwrap(),
        Kind::Build,
        serde_json::to_string(&j).unwrap(),
    );
    n.repo = Some("demo".into());
    n.head = Some(head.into());
    (n, reply)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxy_builder_holds_jobs_while_unreachable_retries_a_drop_and_cleans_up_at_shutdown() {
    let Some(n) = nats() else { return };
    let bus = Arc::new(Bus::connect_url(&n.2, Some(n.3.clone())).await.unwrap());
    bus.provision().await.unwrap();
    let w = World::new();
    let recipe = r#"
repo = "demo"
[[step]]
name = "build"
run = 'test -f main.c && mkdir -p "$CARGO_BUILD_BUILD_DIR" && echo warm > "$CARGO_BUILD_BUILD_DIR/marker"'
time = "2m"
"#;
    store::recipe_put(&bus, recipe, None).await.unwrap();

    let mut cfg = hgl_ai_config::Config::new("lab1");
    cfg.paths.work_root = w.root.join("work").display().to_string();
    cfg.paths.data_dir = w.root.join("data").display().to_string();
    cfg.builder.local = false;
    cfg.builder.remote.push(
        toml::from_str(&format!(
            "platform = \"windows\"\nssh = \"{}\"\nwork_dir = \"{}\"\nshell = \"sh\"\nlabel = \"win1\"\n",
            w.state.display(),
            w.remote.display()
        ))
        .unwrap(),
    );
    cfg.validate().unwrap();
    let lanes = service::lanes(&cfg, fake_ssh()).unwrap();
    let origin = w.origin.clone();
    let settings = Settings {
        clone_url: Arc::new(move |_| origin.clone()),
        toolchains: vec!["git".into()],
        heartbeat: Duration::from_millis(300),
        verdicts: true,
        pr_ended: None,
    };
    // A leftover job directory that cannot be removed yet: no job may start before the
    // startup cleanup is done.
    let jobs = w.remote.join("jobs");
    std::fs::create_dir_all(jobs.join("20261126-000000-000000/src")).unwrap();
    let cache = w.remote.join("cargo-cache/windows");
    let orphan = cache.join("cache-old-label.windows-20261126-000000-aaaaaa");
    std::fs::create_dir_all(&orphan).unwrap();
    let mode = |path: &Path, m: u32| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(m)).unwrap();
    };
    mode(&jobs, 0o500);
    mode(&cache, 0o500);
    // Unreachable from the start.
    w.down(true);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let svc = tokio::spawn(service::serve(bus.clone(), lanes, settings, stop_rx));
    until("an unreachable record", 15, || {
        let bus = bus.clone();
        async move {
            bus.builders().await.unwrap().iter().any(|b| {
                b.label == "win1"
                    && !b.reachable
                    && b.proxy.as_deref() == Some("lab1")
                    && b.platform == Platform::Windows
            })
        }
    })
    .await;

    // A job waits rather than fails.
    let (note, reply) = job("20261127-090000-aaaaaa", &w.head);
    bus.send(&note).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        store::run_get(&bus, "20261127-090000-aaaaaa")
            .await
            .unwrap()
            .is_none(),
        "not taken while unreachable"
    );
    assert!(
        bus.reply_wait(&reply, Duration::ZERO)
            .await
            .unwrap()
            .is_none()
    );

    // Back up: the proxy reconnects, but the cleanup fails, so the job still waits.
    w.down(false);
    until("a reachable record", 30, || {
        let bus = bus.clone();
        async move {
            bus.builders()
                .await
                .unwrap()
                .iter()
                .any(|b| b.label == "win1" && b.reachable)
        }
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        store::run_get(&bus, "20261127-090000-aaaaaa")
            .await
            .unwrap()
            .is_none(),
        "not taken before the startup cleanup"
    );
    // Checkout cleanup can succeed while the old cache's removal still fails.
    mode(&jobs, 0o755);
    until("checkout cleanup", 15, || async { !jobs.exists() }).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(orphan.exists());
    assert!(
        store::run_get(&bus, "20261127-090000-aaaaaa")
            .await
            .unwrap()
            .is_none(),
        "not taken before cache reconciliation"
    );
    // A later heartbeat retries cache cleanup before admitting the job.
    mode(&cache, 0o755);
    let r = bus
        .reply_wait(&reply, Duration::from_secs(60))
        .await
        .unwrap()
        .expect("a receipt");
    assert!(
        !jobs.join("20261126-000000-000000").exists(),
        "the leftover is gone"
    );
    let s = store::parse_receipt(&r, "20261127-090000-aaaaaa").unwrap();
    assert_eq!(s.status, BuildStatus::Pass, "{s:?}");
    assert_eq!(
        (s.host.as_str(), s.proxy.as_deref(), s.executor.as_str()),
        ("win1", Some("lab1"), "ssh")
    );
    assert_eq!(r.kind, Kind::Receipt);
    until("a reachable record with toolchains", 15, || {
        let bus = bus.clone();
        async move {
            bus.builders()
                .await
                .unwrap()
                .iter()
                .any(|b| b.label == "win1" && b.reachable && b.toolchains.contains_key("git"))
        }
    })
    .await;
    assert!(
        w.jobs().is_empty(),
        "the remote job directory is gone: {:?}",
        w.jobs()
    );
    assert!(!orphan.exists(), "the previous proxy's cache is gone");
    let slot = std::fs::read_dir(&cache).unwrap().next().unwrap().unwrap();
    assert_eq!(
        std::fs::read_to_string(slot.path().join("marker")).unwrap(),
        "warm\n"
    );

    // A drop during the step: retried once on reconnect, not a failure.
    let hold = r#"
repo = "demo"
[[step]]
name = "build"
run = '''
test -f main.c || exit 1
if [ -n "${CARGO_BUILD_BUILD_DIR:-}" ]; then
    test -f "$CARGO_BUILD_BUILD_DIR/marker" || exit 1
fi
sleep 3
'''
time = "2m"
"#;
    store::recipe_put(&bus, hold, None).await.unwrap();
    let (note, reply) = job("20261127-090000-bbbbbb", &w.head);
    bus.send(&note).await.unwrap();
    until("the step to start", 30, || {
        let bus = bus.clone();
        async move {
            store::run_get(&bus, "20261127-090000-bbbbbb")
                .await
                .unwrap()
                .is_some_and(|s| s.steps.iter().any(|x| x.status.as_str() == "running"))
        }
    })
    .await;
    w.down(true);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    w.down(false);
    let r = bus
        .reply_wait(&reply, Duration::from_secs(90))
        .await
        .unwrap();
    if r.is_none() {
        let log = std::fs::read_to_string(w.state.join("log")).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(15).collect();
        panic!(
            "no receipt; run {:?}\nremote log tail: {tail:#?}\nbuilders {:?}",
            store::run_get(&bus, "20261127-090000-bbbbbb").await,
            bus.builders().await
        );
    }
    let r = r.unwrap();
    let s = store::parse_receipt(&r, "20261127-090000-bbbbbb").unwrap();
    assert_eq!(
        s.status,
        BuildStatus::Pass,
        "retried after the reconnect: {s:?}"
    );
    assert!(w.jobs().is_empty(), "{:?}", w.jobs());
    assert_eq!(std::fs::read_dir(cache).unwrap().count(), 0);
    // The retry checked out in a directory of its own, which no stale cleanup of the
    // first attempt can remove (Carbon's second review of #12, finding 5).
    let log = std::fs::read_to_string(w.state.join("log")).unwrap();
    assert!(
        log.contains("jobs/20261127-090000-bbbbbb-r1/src"),
        "the retry reused the first attempt's directory"
    );

    // A redelivered job whose result was saved before the crash: published, not rebuilt.
    let saved = w.root.join("work/builder/logs/win1/20261127-090000-dddddd");
    std::fs::create_dir_all(&saved).unwrap();
    let mut done = s.clone();
    done.id = "20261127-090000-dddddd".into();
    done.reason = Some("saved before the crash".into());
    std::fs::write(
        saved.join("summary.json"),
        serde_json::to_vec(&done).unwrap(),
    )
    .unwrap();
    let before = std::fs::read_to_string(w.state.join("log"))
        .unwrap()
        .lines()
        .count();
    let (note, reply) = job("20261127-090000-dddddd", &w.head);
    // The same request was taken before: its log directory holds it.
    std::fs::write(saved.join("request.json"), note.body.trim()).unwrap();
    bus.send(&note).await.unwrap();
    let r = bus
        .reply_wait(&reply, Duration::from_secs(30))
        .await
        .unwrap()
        .expect("a receipt");
    let got = store::parse_receipt(&r, "20261127-090000-dddddd").unwrap();
    assert_eq!(got.reason.as_deref(), Some("saved before the crash"));
    let log = std::fs::read_to_string(w.state.join("log")).unwrap();
    assert!(
        !log.lines().skip(before).any(|l| l.contains("worktree")),
        "no checkout for a saved result"
    );
    assert_eq!(
        store::run_get(&bus, "20261127-090000-dddddd")
            .await
            .unwrap()
            .unwrap()
            .reason,
        done.reason
    );

    // Drain: the record says so.
    bus.state_put("builder.win1", "drain").await.unwrap();
    until("a draining record", 10, || {
        let bus = bus.clone();
        async move {
            bus.builders()
                .await
                .unwrap()
                .iter()
                .any(|b| b.label == "win1" && b.state == "draining")
        }
    })
    .await;
    bus.state_del("builder.win1").await.unwrap();

    // Shutdown during a build: the remote tree is killed, the job directory removed, the
    // master closed, the record gone, and the job back in the inbox.
    // With --keep-going and a failed step before it, so the aggregate is `fail`.
    let long = "repo = \"demo\"\n[[step]]\nname = \"lint\"\nrun = \"false\"\n[[step]]\nname = \"build\"\nrun = \"sleep 60 & echo $! > ../../../long.pid; wait\"\ntime = \"5m\"\n";
    store::recipe_put(&bus, long, None).await.unwrap();
    let (mut note, reply) = job("20261127-090000-cccccc", &w.head);
    let mut j: BuildJob = serde_json::from_str(note.body.trim()).unwrap();
    j.keep_going = true;
    note.body = serde_json::to_string(&j).unwrap() + "\n";
    bus.send(&note).await.unwrap();
    let pidfile = w.remote.join("long.pid");
    until("the long step", 30, || {
        let p = pidfile.clone();
        async move { p.is_file() }
    })
    .await;
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    assert!(alive(&pid));
    stop_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(60), svc)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!alive(&pid), "the remote tree is killed");
    assert!(w.jobs().is_empty(), "{:?}", w.jobs());
    assert!(!w.state.join("master").exists(), "the master is closed");
    assert!(
        bus.builders().await.unwrap().is_empty(),
        "the record is removed"
    );
    assert!(
        bus.reply_wait(&reply, Duration::ZERO)
            .await
            .unwrap()
            .is_none(),
        "no receipt for a stopped job"
    );
    let s = store::run_get(&bus, "20261127-090000-cccccc")
        .await
        .unwrap()
        .unwrap();
    assert!(
        s.reason
            .as_deref()
            .is_some_and(|r| r.contains("queued again")),
        "requeued although a step before failed: {s:?}"
    );
    assert_eq!(s.status, BuildStatus::Queued, "not the partial aggregate");
}

/// Carbon's second review of #12, finding 6: a session that dies with its master is a
/// drop even when the master is back before the proxy looks (here a supervisor
/// reconnects while the step's session ends).
#[test]
fn a_session_lost_with_its_master_is_a_drop_after_a_quick_reconnect() {
    let w = World::new();
    let conn = w.conn();
    conn.connect().unwrap();
    let ex = Ssh::new(
        Platform::Windows,
        Dialect::Posix,
        w.remote.to_str().unwrap(),
        BTreeMap::new(),
        conn.clone(),
    );
    let ws = ex.prepare(&w.checkout("20261127-150000-aaaaaa")).unwrap();
    let (state, c2) = (w.state.clone(), conn.clone());
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        let pid = std::fs::read_to_string(state.join("master")).unwrap();
        Command::new("kill").arg(pid.trim()).status().unwrap();
        while state.join("master").exists() {
            std::thread::sleep(Duration::from_millis(5));
        }
        // A new master before the step's session notices.
        let deadline = Instant::now() + Duration::from_secs(10);
        while c2.connect().is_err() {
            assert!(Instant::now() < deadline);
        }
        let now = std::fs::read_to_string(state.join("master")).unwrap();
        assert_ne!(now.trim(), pid.trim(), "a new master");
    });
    let r = ex.run_step(
        &ws,
        &step("long", "sleep 5", Duration::from_secs(60)),
        &w.root.join("long.log"),
        &Cancel::default(),
    );
    t.join().unwrap();
    assert!(r.unreachable, "a drop, not a failure: {r:?}");
    ex.cleanup(&ws).unwrap();
    conn.close();
}

/// Carbon's second review of #12, finding 7: two proxies' processes (a builder and a
/// `build shell`) on one remote clone take turns, not only threads of one.
#[test]
fn remote_checkouts_take_turns_across_processes() {
    let w = World::new();
    let conn = w.conn();
    conn.connect().unwrap();
    // Two executors with no state in common but the ssh directory, as two processes are.
    let a = Arc::new(Ssh::new(
        Platform::Windows,
        Dialect::Posix,
        w.remote.to_str().unwrap(),
        BTreeMap::new(),
        conn.clone(),
    ));
    let b = Arc::new(Ssh::new(
        Platform::Windows,
        Dialect::Posix,
        w.remote.to_str().unwrap(),
        BTreeMap::new(),
        conn.clone(),
    ));
    for round in 0..2 {
        let handles: Vec<_> = (0..6)
            .map(|i| {
                let ex = if i % 2 == 0 { a.clone() } else { b.clone() };
                let mut c = w.checkout(&format!("20261127-16000{round}-00000{i}"));
                c.base = Some("main".into());
                std::thread::spawn(move || {
                    let ws = ex.prepare(&c).map_err(|e| format!("{e:?}"))?;
                    ex.cleanup(&ws).map_err(|e| format!("{e:?}"))
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().unwrap();
        }
    }
    conn.close();
}
