//! `hgl-ai build` and `hgl-ai builder`, run as the binary against a live
//! `nats-server -js` (skipped, with the reason printed, when `nats-server` is not on
//! `PATH`): a builder service and `build run --platform P` through the hub, P this host's
//! own platform, on a fixture repository whose recipe has a passing and a failing step.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use hgl_ai_proto::build::{BuilderRecord, Platform};

/// This host's platform: the one its builder advertises and builds.
fn platform() -> &'static str {
    Platform::current()
        .expect("a linux, macos or windows host")
        .as_str()
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("build-cli-{tag}-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Kills its process on drop.
struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Hub {
    _nats: Proc,
    dir: Scratch,
    config: PathBuf,
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
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

impl Hub {
    fn start(tag: &str) -> Option<Hub> {
        let Some(bin) = std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join("nats-server"))
            .find(|p| p.is_file())
        else {
            eprintln!("skipped: nats-server is not on PATH");
            return None;
        };
        let dir = Scratch::new(tag);
        let token = format!("build{}", std::process::id());
        let child = Command::new(bin)
            .args(["-js", "-a", "127.0.0.1", "-p", "-1", "--auth", &token])
            .arg("-sd")
            .arg(dir.0.join("store"))
            .arg("--ports_file_dir")
            .arg(&dir.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start nats-server");
        let ports = dir.0.join(format!("nats-server_{}.ports", child.id()));
        let nats = Proc(child);
        let deadline = Instant::now() + Duration::from_secs(15);
        let url = loop {
            let found = std::fs::read_to_string(&ports)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| v["nats"][0].as_str().map(String::from));
            if let Some(url) = found {
                break url;
            }
            assert!(
                Instant::now() < deadline,
                "nats-server did not report its port"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        let token_file = dir.0.join("hub.token");
        std::fs::write(&token_file, format!("{token}\n")).unwrap();
        let config = dir.0.join("config.toml");
        let mut cfg = hgl_ai_config::Config::new("lab1");
        cfg.hub.url = url;
        cfg.hub.token_file = token_file.display().to_string();
        cfg.paths.work_root = dir.0.join("work").display().to_string();
        cfg.paths.data_dir = dir.0.join("data").display().to_string();
        cfg.github.clone_url = format!("{}/remotes/{{repo}}.git", dir.0.display());
        cfg.builder.toolchains = vec!["git".into()];
        cfg.save(&config).unwrap();
        Some(Hub {
            _nats: nats,
            dir,
            config,
        })
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_hgl-ai"));
        c.arg("--config")
            .arg(&self.config)
            .args(args)
            .env_remove("HGL_AI_SEAT")
            .env_remove("HGL_AI_TASK_DIR")
            .env_remove("HGL_AI_TURN")
            .env_remove("HGL_AI_CONFIG")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    /// JetStream API requests the hub has answered so far.
    fn api_requests(&self) -> u64 {
        let cfg = hgl_ai_config::Config::load(&self.config).unwrap();
        let token = std::fs::read_to_string(cfg.hub.token_file).unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let c = async_nats::ConnectOptions::with_token(token.trim().to_string())
                .connect(cfg.hub.url.as_str())
                .await
                .unwrap();
            async_nats::jetstream::new(c)
                .query_account()
                .await
                .unwrap()
                .requests
                .total
        })
    }
}

fn text(o: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

const RECIPE: &str = r#"
repo = "demo"

[[step]]
name = "prepare"
run = "test -f main.c && echo prepared"
memcap_gb = 1
time = "2m"

[[step]]
name = "compile"
run = "echo 'main.c:3:5: error: unknown type name ‘widget’' >&2; echo 'main.c:7:9: warning: unused variable ‘n’ [-Wunused-variable]'; exit 1"
parser = "gcc"
memcap_gb = 1
time = "2m"

[[step]]
name = "test"
run = "echo never"
"#;

#[test]
fn builder_runs_a_remote_build_through_the_hub() {
    let Some(hub) = Hub::start("e2e") else { return };
    let o = hub.run(&["init"]);
    assert!(o.status.success(), "{}", text(&o));

    // The fixture repository: a bare remote and a checkout with one commit.
    let remotes = hub.dir.0.join("remotes");
    std::fs::create_dir_all(&remotes).unwrap();
    git(
        &remotes,
        &["init", "--quiet", "--bare", "-b", "main", "demo.git"],
    );
    let seed = hub.dir.0.join("seed");
    git(
        &hub.dir.0,
        &[
            "clone",
            "--quiet",
            &remotes.join("demo.git").display().to_string(),
            "seed",
        ],
    );
    std::fs::write(seed.join("main.c"), "int main(void) { return 0; }\n").unwrap();
    git(&seed, &["checkout", "--quiet", "-b", "main"]);
    git(&seed, &["add", "main.c"]);
    git(&seed, &["commit", "--quiet", "-m", "initial"]);
    git(&seed, &["push", "--quiet", "origin", "main"]);
    let head = git(&seed, &["rev-parse", "HEAD"]);

    let recipe = hub.dir.0.join("demo.toml");
    std::fs::write(&recipe, RECIPE).unwrap();
    let o = hub.run(&["build", "recipe", "import", &recipe.display().to_string()]);
    assert_eq!(
        String::from_utf8_lossy(&o.stdout),
        "recipe.demo revision 1\n",
        "{}",
        text(&o)
    );

    // No builder yet: not run (3).
    let o = hub
        .cmd(&["build", "run", "demo", "--platform", platform()])
        .current_dir(&seed)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(3), "{}", text(&o));

    let builder = Proc(
        hub.cmd(&["builder", "run"])
            .env("HGL_AI_LOG", "info")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(hub.dir.0.join("builder.log")).unwrap())
            .spawn()
            .unwrap(),
    );
    // The record as written, field by field, whatever the platform and the line format.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let o = hub.run(&["--json", "builder", "list"]);
        let l = String::from_utf8_lossy(&o.stdout).into_owned();
        let found = l
            .lines()
            .filter_map(|r| serde_json::from_str::<BuilderRecord>(r).ok())
            .find(|b| b.label == "lab1");
        if let Some(b) = found.filter(|b| b.reachable && b.state == "active") {
            assert_eq!(b.platform.as_str(), platform(), "{b:?}");
            assert_eq!(
                (b.executor.as_str(), b.slots, b.busy),
                ("local", 1, 0),
                "{b:?}"
            );
            assert_eq!(b.inbox.as_str(), "builder-lab1");
            assert!(b.toolchains["git"].starts_with("git version"), "{b:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no builder record: {l}\n{}",
            std::fs::read_to_string(hub.dir.0.join("builder.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let o = hub.run(&["hosts"]);
    assert_eq!(
        String::from_utf8_lossy(&o.stdout),
        "",
        "builder records are not hosts\n{}",
        text(&o)
    );

    let o = hub
        .cmd(&[
            "--json",
            "build",
            "run",
            "--platform",
            platform(),
            "--timeout",
            "2m",
        ])
        .current_dir(&seed)
        .output()
        .unwrap();
    assert_eq!(
        o.status.code(),
        Some(1),
        "a failing step fails the build\n{}",
        text(&o)
    );
    let s: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(s["status"], "fail");
    assert_eq!(s["repo"], "demo");
    assert_eq!(s["head"], head);
    assert_eq!(s["host"], "lab1");
    assert_eq!(s["executor"], "local");
    let steps: Vec<(&str, &str)> = s["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (x["name"].as_str().unwrap(), x["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        steps,
        [
            ("prepare", "pass"),
            ("compile", "fail"),
            ("test", "skipped")
        ]
    );
    assert_eq!(s["errors_total"], 1);
    let e = &s["errors"][0];
    assert_eq!(e["file"], "main.c");
    assert_eq!(e["line"], 3);
    assert_eq!(e["col"], 5);
    assert_eq!(e["step"], "compile");
    assert_eq!(e["message"], "unknown type name ‘widget’");
    assert_eq!(s["warnings"]["-Wunused-variable"], 1);
    let id = s["id"].as_str().unwrap().to_string();
    let log_dir = PathBuf::from(s["log_dir"].as_str().unwrap());
    let log = std::fs::read(log_dir.join("compile.log")).unwrap();
    let (a, b) = (
        e["start"].as_u64().unwrap() as usize,
        e["end"].as_u64().unwrap() as usize,
    );
    assert!(String::from_utf8_lossy(&log[a..b]).starts_with("main.c:3:5: error:"));
    // The builder's host reads the range as the summary says (Carbon's second review of
    // #12, finding 4), and with --log-dir naming the run's own directory.
    let range = format!("{a}-{b}");
    let o = hub.run(&["build", "log", &id, "compile", "--bytes", &range]);
    assert!(
        String::from_utf8_lossy(&o.stdout).starts_with("main.c:3:5: error:"),
        "{}",
        text(&o)
    );
    let o = hub.run(&[
        "build",
        "log",
        &id,
        "compile",
        "--bytes",
        &range,
        "--log-dir",
        log_dir.to_str().unwrap(),
    ]);
    assert!(
        String::from_utf8_lossy(&o.stdout).starts_with("main.c:3:5: error:"),
        "{}",
        text(&o)
    );

    // Cleanup is part of done: no checkout left, the clone kept.
    let work = hub.dir.0.join("work/builder");
    assert!(!work.join("jobs").join(&id).exists());
    assert!(work.join("clones/demo/HEAD").is_file());

    // The run is on the hub; the text summary is bounded.
    let o = hub.run(&["build", "status", &id]);
    let t = String::from_utf8_lossy(&o.stdout).into_owned();
    println!("--- sample summary ---\n{t}---");
    assert!(
        t.starts_with(&format!(
            "build {id} demo at {} on lab1 ({}): FAIL",
            &head[..12],
            platform()
        )),
        "{t}"
    );
    assert!(
        t.contains("errors: 1 (first 1)\n  main.c:3:5 unknown type name ‘widget’  [compile.log "),
        "{t}"
    );
    assert!(t.len() <= 4096);
    let o = hub.run(&["build", "list", "--repo", "demo", "--failed"]);
    assert!(
        String::from_utf8_lossy(&o.stdout).starts_with(&id),
        "{}",
        text(&o)
    );
    let o = hub.run(&["build", "wait", &id, "--timeout", "10s"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    // A note on the reply address that is not the builder's receipt (here one claiming a
    // pass) changes nothing, and no seat may send as a builder (Carbon's second review
    // of #12, finding 10).
    let mut forged: serde_json::Value = s.clone();
    forged["status"] = "pass".into();
    let body = hub.dir.0.join("forged.json");
    std::fs::write(&body, forged.to_string()).unwrap();
    let reply = format!("r-{id}");
    let b = body.display().to_string();
    let o = hub.run(&[
        "note",
        "send",
        "--from",
        "w2",
        "--to",
        &reply,
        "--kind",
        "note",
        "--body-file",
        &b,
    ]);
    assert!(o.status.success(), "{}", text(&o));
    // From a host without the builder's logs: the reply address and the hub.
    let logs_here = hub.dir.0.join("work/builder/logs");
    let away = hub.dir.0.join("logs-away");
    std::fs::rename(&logs_here, &away).unwrap();
    let o = hub.run(&["build", "wait", &id, "--timeout", "10s"]);
    std::fs::rename(&away, &logs_here).unwrap();
    assert_eq!(
        o.status.code(),
        Some(1),
        "still the builder's result\n{}",
        text(&o)
    );
    let o = hub.run(&[
        "note",
        "send",
        "--from",
        "builder-lab1",
        "--to",
        &reply,
        "--kind",
        "note",
        "--body-file",
        &b,
    ]);
    assert!(
        !o.status.success(),
        "a seat sent as a builder\n{}",
        text(&o)
    );

    // A passing selection, detached.
    let o = hub
        .cmd(&[
            "build",
            "run",
            "--platform",
            platform(),
            "--steps",
            "prepare",
            "--detach",
        ])
        .current_dir(&seed)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o));
    let id2 = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let o = hub.run(&["build", "wait", &id2, "--timeout", "60s"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(
        String::from_utf8_lossy(&o.stdout).contains(": PASS in "),
        "{}",
        text(&o)
    );

    // Waiting on the builder's own host for a build still running (Carbon's check of
    // the round-two fixes, finding 1): the builder's run has no pid file, which is no
    // reason to call it stopped.
    let slow = hub.dir.0.join("slow.toml");
    std::fs::write(
        &slow,
        RECIPE.replace(
            "test -f main.c && echo prepared",
            "sleep 2 && test -f main.c && echo prepared",
        ),
    )
    .unwrap();
    let o = hub.run(&["build", "recipe", "import", &slow.display().to_string()]);
    assert!(o.status.success(), "{}", text(&o));
    let o = hub
        .cmd(&[
            "build",
            "run",
            "--platform",
            platform(),
            "--steps",
            "prepare",
            "--detach",
        ])
        .current_dir(&seed)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o));
    let id4 = String::from_utf8_lossy(&o.stdout).trim().to_string();
    // Once the builder has taken it (its run directory exists).
    let status = hub
        .dir
        .0
        .join("work/builder/logs/lab1")
        .join(&id4)
        .join("status.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !status.is_file() {
        assert!(Instant::now() < deadline, "the builder did not take it");
        std::thread::sleep(Duration::from_millis(20));
    }
    let t0 = Instant::now();
    let o = hub.run(&["build", "wait", &id4, "--timeout", "60s"]);
    assert_eq!(o.status.code(), Some(0), "waited for the end\n{}", text(&o));
    assert!(
        t0.elapsed() >= Duration::from_millis(1500),
        "{:?}",
        t0.elapsed()
    );
    let o = hub.run(&["build", "recipe", "import", &recipe.display().to_string()]);
    assert!(o.status.success(), "{}", text(&o));

    // A stray note on a build's reply address is read once, and the wait keeps its pace
    // (finding 2): a few hub requests a second, not thousands.
    let stray = "20261127-230000-abcdef";
    let o = hub.run(&[
        "note",
        "send",
        "--from",
        "w2",
        "--to",
        &format!("r-{stray}"),
        "--kind",
        "note",
        "--body",
        "x",
    ]);
    assert!(o.status.success(), "{}", text(&o));
    let before = hub.api_requests();
    let t0 = Instant::now();
    let o = hub.run(&["build", "wait", stray, "--timeout", "3s"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(t0.elapsed() >= Duration::from_secs(3), "{:?}", t0.elapsed());
    let calls = hub.api_requests() - before;
    assert!(calls < 100, "{calls} hub requests in a 3s wait");

    // A local run with the hub's recipe.
    let logs = hub.dir.0.join("logs");
    let o = hub
        .cmd(&[
            "build",
            "run",
            "--steps",
            "prepare",
            "--log-dir",
            &logs.display().to_string(),
        ])
        .current_dir(&seed)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));

    // Detached here: wait, then read the log by byte range.
    let l = logs.display().to_string();
    let o = hub
        .cmd(&[
            "build",
            "run",
            "--steps",
            "prepare",
            "--log-dir",
            &l,
            "--detach",
        ])
        .current_dir(&seed)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o));
    let id3 = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let o = hub.run(&["build", "wait", &id3, "--log-dir", &l, "--timeout", "60s"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let o = hub.run(&[
        "build",
        "log",
        &id3,
        "prepare",
        "--log-dir",
        &l,
        "--bytes",
        "0-8",
    ]);
    assert_eq!(
        String::from_utf8_lossy(&o.stdout),
        "prepared\n",
        "{}",
        text(&o)
    );

    // Shutdown removes the record.
    // SAFETY: plain kill(2) of our child.
    unsafe { libc::kill(builder.0.id() as i32, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut builder = builder;
    loop {
        if let Some(st) = builder.0.try_wait().unwrap() {
            assert!(
                st.success(),
                "{}",
                std::fs::read_to_string(hub.dir.0.join("builder.log")).unwrap()
            );
            break;
        }
        assert!(Instant::now() < deadline, "the builder did not stop");
        std::thread::sleep(Duration::from_millis(100));
    }
    let o = hub.run(&["builder", "list"]);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "", "{}", text(&o));
}

/// Carbon's finding 7 on #12: a detached build inside an agent turn would end with the
/// turn (its slice and marker are the turn's), so it is refused with the way that
/// outlives the turn.
#[test]
fn detach_inside_a_turn_is_refused() {
    let dir = Scratch::new("turn");
    let src = dir.0.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let recipe = dir.0.join("r.toml");
    std::fs::write(
        &recipe,
        "repo = \"demo\"\n[[step]]\nname = \"a\"\nrun = \"true\"\n",
    )
    .unwrap();
    let logs = dir.0.join("logs");
    let o = Command::new(env!("CARGO_BIN_EXE_hgl-ai"))
        .args(["build", "run", "demo", "--recipe"])
        .arg(&recipe)
        .arg("--dir")
        .arg(&src)
        .arg("--log-dir")
        .arg(&logs)
        .arg("--detach")
        .env("HGL_AI_CONFIG", dir.0.join("none.toml"))
        .env("HGL_AI_TURN", "t-1")
        .env_remove("HGL_AI_TASK_DIR")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(3), "{}", text(&o));
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("--platform"),
        "{}",
        text(&o)
    );
    assert!(!logs.exists(), "nothing started");
}

#[test]
fn turn_term_stops_a_local_build_without_a_failure_verdict() {
    turn_term_build(false, false);
}

#[test]
fn turn_term_preserves_an_earlier_failure() {
    turn_term_build(true, false);
}

#[test]
fn turn_sigint_then_term_stops_a_local_build() {
    turn_term_build(false, true);
}

fn turn_term_build(earlier_failure: bool, interrupt_first: bool) {
    use std::os::unix::process::CommandExt;

    let dir = Scratch::new("turn-term");
    let src = dir.0.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let recipe = dir.0.join("recipe.toml");
    let first = if earlier_failure {
        "[[step]]\nname = \"fail\"\nrun = \"exit 1\"\n"
    } else {
        ""
    };
    std::fs::write(
        &recipe,
        format!(
            "repo = \"demo\"\n{first}[[step]]\nname = \"test\"\nrun = \"echo started; sleep 60\"\n"
        ),
    )
    .unwrap();
    let logs = dir.0.join("logs");
    let mut command = Command::new(env!("CARGO_BIN_EXE_hgl-ai"));
    command
        .args(["--json", "build", "run", "demo", "--keep-going", "--recipe"])
        .arg(&recipe)
        .arg("--dir")
        .arg(&src)
        .arg("--log-dir")
        .arg(&logs)
        .arg("--build-dir")
        .arg(dir.0.join("build"))
        .env("HGL_AI_CONFIG", dir.0.join("none.toml"))
        .env("HGL_AI_TURN", "test-turn")
        .env_remove("HGL_AI_TASK_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().unwrap();
    let pid = child.id() as i32;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            panic!("build exited before starting: {}", text(&output));
        }
        let started = std::fs::read_dir(&logs)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| {
                std::fs::read_to_string(e.path().join("test.log"))
                    .is_ok_and(|s| s.contains("started"))
            });
        if started {
            break;
        }
        assert!(Instant::now() < deadline, "build did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Signal the whole group, as turn cleanup does.
    if interrupt_first {
        assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(unsafe { libc::kill(-pid, libc::SIGTERM) }, 0);
    let output = child.wait_with_output().unwrap();
    let summary: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("{}", text(&output)));
    if earlier_failure {
        assert_eq!(output.status.code(), Some(1), "{}", text(&output));
        assert_eq!(summary["status"], "fail");
        assert_eq!(summary["steps"][0]["status"], "fail");
        assert_eq!(summary["steps"][1]["status"], "stopped");
    } else {
        assert_eq!(output.status.code(), Some(1), "{}", text(&output));
        assert_eq!(summary["status"], "stopped", "{}", text(&output));
        assert_eq!(summary["reason"], "turn ended");
        assert_eq!(summary["steps"][0]["status"], "stopped");
    }
}

/// Carbon's finding 10 on #12: a range in an XML report is read from that report, which
/// the summary names.
#[test]
fn a_report_range_reads_the_report() {
    let dir = Scratch::new("xml");
    let src = dir.0.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let recipe = dir.0.join("r.toml");
    std::fs::write(
        &recipe,
        r#"repo = "demo"
[[step]]
name = "test"
run = "printf 'ran\n'; printf '<testsuite><testcase name=\"t1\"><failure message=\"boom\"/></testcase></testsuite>\n' > $HGL_AI_BUILD_DIR/report.xml"
parser = "junit"
artifacts = ["$HGL_AI_BUILD_DIR/report.xml"]
"#,
    )
    .unwrap();
    let logs = dir.0.join("logs");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_hgl-ai"))
            .args(args)
            .arg("--log-dir")
            .arg(&logs)
            .env("HGL_AI_CONFIG", dir.0.join("none.toml"))
            .env_remove("HGL_AI_TASK_DIR")
            .env_remove("HGL_AI_TURN")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let o = Command::new(env!("CARGO_BIN_EXE_hgl-ai"))
        .args(["--json", "build", "run", "demo", "--recipe"])
        .arg(&recipe)
        .arg("--dir")
        .arg(&src)
        .arg("--build-dir")
        .arg(dir.0.join("build"))
        .arg("--log-dir")
        .arg(&logs)
        .env("HGL_AI_CONFIG", dir.0.join("none.toml"))
        .env_remove("HGL_AI_TASK_DIR")
        .env_remove("HGL_AI_TURN")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let s: serde_json::Value =
        serde_json::from_slice(&o.stdout).unwrap_or_else(|_| panic!("{}", text(&o)));
    let f = &s["tests"]["failures"][0];
    let source = f["source"].as_str().unwrap().to_string();
    assert_eq!(source, "artifacts/test/report.xml");
    let id = s["id"].as_str().unwrap().to_string();
    let range = format!("{}-{}", f["start"], f["end"]);
    let o = run(&["build", "log", &id, &source, "--bytes", &range]);
    let got = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(got.starts_with("<testcase name=\"t1\">"), "{}", text(&o));
    let o = run(&["build", "status", &id]);
    let t = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(t.contains(&format!("[{source} {range}]")), "{t}");
    // Nothing outside the run's directory.
    let o = run(&["build", "log", &id, "../../r.toml"]);
    assert_ne!(o.status.code(), Some(0), "{}", text(&o));
}

/// Carbon's second review of #12, finding 7: a `build shell` checkout is not a job's,
/// so a builder starting meanwhile (its cleanup of leftover jobs) leaves it alone.
#[test]
fn a_shell_checkout_survives_a_builder_starting() {
    let dir = Scratch::new("shell");
    let remotes = dir.0.join("remotes");
    std::fs::create_dir_all(&remotes).unwrap();
    git(
        &remotes,
        &["init", "--quiet", "--bare", "-b", "main", "demo.git"],
    );
    let seed = dir.0.join("seed");
    git(
        &dir.0,
        &[
            "clone",
            "--quiet",
            &remotes.join("demo.git").display().to_string(),
            "seed",
        ],
    );
    std::fs::write(seed.join("f.txt"), "x\n").unwrap();
    git(&seed, &["checkout", "--quiet", "-b", "main"]);
    git(&seed, &["add", "f.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "c"]);
    git(&seed, &["push", "--quiet", "origin", "main"]);
    let mut cfg = hgl_ai_config::Config::new("lab1");
    cfg.paths.work_root = dir.0.join("work").display().to_string();
    cfg.paths.data_dir = dir.0.join("data").display().to_string();
    cfg.github.clone_url = format!("{}/{{repo}}.git", remotes.display());
    let config = dir.0.join("config.toml");
    cfg.save(&config).unwrap();
    let (ready, go, result) = (dir.0.join("ready"), dir.0.join("go"), dir.0.join("result"));
    let sh = dir.0.join("shell.sh");
    std::fs::write(
        &sh,
        format!(
            "#!/bin/sh\n: > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\nif [ -f f.txt ]; then echo kept; else echo gone; fi > '{}'\n",
            ready.display(),
            go.display(),
            result.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let child = Proc(
        Command::new(env!("CARGO_BIN_EXE_hgl-ai"))
            .arg("--config")
            .arg(&config)
            .args(["build", "shell", "--platform", platform(), "demo", "--dir"])
            .arg(&seed)
            .env("SHELL", &sh)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("HGL_AI_TASK_DIR")
            .env_remove("HGL_AI_TURN")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "the shell did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A builder starts on this host: it removes leftover job directories.
    use hgl_ai_build::exec::Executor;
    hgl_ai_build::exec::local::Local::new(
        dir.0.join("work/builder"),
        Default::default(),
        hgl_ai_tree::Git::default(),
    )
    .clean_all()
    .unwrap();
    std::fs::write(&go, "").unwrap();
    let mut child = child;
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the shell did not end");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(std::fs::read_to_string(&result).unwrap(), "kept\n");
}
