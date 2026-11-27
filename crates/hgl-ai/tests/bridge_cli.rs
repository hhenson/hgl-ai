//! The `seat`, `tree` and `bridge` commands, run as the binary. The bridge test runs
//! against a live `nats-server -js` (skipped, with the reason printed, when it is not
//! on `PATH`) with a fake `codex` first on `PATH` and a local bare "remote".

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use hgl_ai_config::{Binding, Config, SeatConfig};
use hgl_ai_proto::{Role, Runtime, Seat, TaskKey, Tier};
use hgl_ai_tree::testing::{git, remote};
use hgl_ai_tree::{Create, Trees};

/// A scratch directory under cargo's target tmp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("bridge-cli-{tag}-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p.canonicalize().unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn hgl(config: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_hgl-ai"));
    c.arg("--config")
        .arg(config)
        .args(args)
        .env_remove("HGL_AI_SEAT")
        .env_remove("HGL_AI_CONFIG")
        .stdin(Stdio::null());
    c
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A configuration in `dir`: work root, data dir, and clone URLs pointing into it.
fn config(dir: &Path, hub_url: &str, token_file: &Path) -> (Config, PathBuf) {
    let mut cfg = Config::new("test-host");
    cfg.hub.url = hub_url.into();
    cfg.hub.token_file = token_file.display().to_string();
    cfg.paths.work_root = dir.join("work").display().to_string();
    cfg.paths.data_dir = dir.join("data").display().to_string();
    cfg.github.clone_url = format!("{}/{{repo}}.git", dir.display());
    let p = dir.join("config.toml");
    cfg.save(&p).unwrap();
    (cfg, p)
}

fn isolate(c: &mut Command, dir: &Path) {
    c.env("GIT_CONFIG_GLOBAL", dir.join("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1");
}

#[test]
fn seats_are_added_listed_and_removed() {
    let d = Scratch::new("seat");
    let (_, p) = config(&d.0, "nats://127.0.0.1:1", &d.0.join("none.token"));
    let add = |args: &[&str]| {
        hgl(&p, &[&["seat", "add"], args].concat())
            .output()
            .unwrap()
    };
    let o = add(&[
        "w1",
        "--role",
        "implementor",
        "--runtime",
        "codex",
        "--tier",
        "frontier",
        "--effort",
        "high",
        "--fallback",
        "agy:claude:frontier:high",
    ]);
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(
        out(&o).trim(),
        "added w1 implementor codex gpt/gpt-6.1-sol effort high; fallback agy claude/claude-opus-5-5 effort high; restart the bridge service to serve it"
    );
    let o = add(&[
        "w1",
        "--role",
        "implementor",
        "--runtime",
        "codex",
        "--tier",
        "fast",
    ]);
    assert_eq!(o.status.code(), Some(1));
    assert!(err(&o).contains("exists"));
    let o = add(&[
        "r1",
        "--role",
        "reviewer",
        "--runtime",
        "codex",
        "--family",
        "claude",
        "--tier",
        "fast",
    ]);
    assert_eq!(o.status.code(), Some(1), "codex cannot run a claude model");
    let o = add(&[
        "r1",
        "--role",
        "reviewer",
        "--runtime",
        "claude",
        "--tier",
        "fast",
        "--model",
        "x",
    ]);
    assert_eq!(o.status.code(), Some(2), "tier and model are exclusive");
    let o = add(&[
        "r1",
        "--role",
        "reviewer",
        "--runtime",
        "claude",
        "--model",
        "claude-opus-5-5",
        "--effort",
        "xhigh",
    ]);
    assert!(o.status.success(), "{}", err(&o));

    let o = hgl(&p, &["--json", "seat", "list"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v[0]["seat"], "w1");
    assert_eq!(v[0]["model"], "gpt-6.1-sol");
    assert_eq!(v[0]["fallback"][0]["model"], "claude-opus-5-5");
    assert_eq!(v[1]["family"], "claude");
    assert_eq!(Config::load(&p).unwrap().seats.len(), 2);

    let o = hgl(&p, &["seat", "remove", "w1"]).output().unwrap();
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(
        hgl(&p, &["seat", "remove", "w1"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(1)
    );
    let o = hgl(&p, &["seat", "list"]).output().unwrap();
    assert!(
        out(&o).starts_with("r1 reviewer claude claude/claude-opus-5-5 effort xhigh"),
        "{}",
        out(&o)
    );
}

#[test]
fn trees_are_listed_and_removed_on_this_host() {
    let d = Scratch::new("tree");
    let g = git(&d.0);
    remote(&g, &d.0, "demo");
    let (cfg, p) = config(&d.0, "nats://127.0.0.1:1", &d.0.join("none.token"));
    let trees = Trees::new(&cfg, g.clone()).unwrap();
    let w1 = Seat::new("w1").unwrap();
    let t3 = TaskKey {
        repo: "demo".into(),
        issue: 3,
    };
    let t4 = TaskKey {
        repo: "demo".into(),
        issue: 4,
    };
    let c3 = trees
        .create(
            &w1,
            &t3,
            &Create::Implement {
                title: "Three".into(),
                base: None,
            },
        )
        .unwrap();
    let c4 = trees
        .create(
            &w1,
            &t4,
            &Create::Implement {
                title: "Four".into(),
                base: None,
            },
        )
        .unwrap();
    std::fs::write(c3.dir.join("src/wip.txt"), "wip\n").unwrap();

    let o = hgl(&p, &["--json", "tree", "list", "--local"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);
    assert_eq!(v[0]["branch"], "codex/3-three");
    assert!(v[0]["size_bytes"].as_u64().unwrap() > 0);

    // Not stale: young, and closed-ness unknown without `gh`.
    let o = hgl(&p, &["tree", "list", "--local", "--stale"])
        .env("PATH", d.0.join("nobin"))
        .output()
        .unwrap();
    assert!(o.status.success() && out(&o).is_empty(), "{}", err(&o));

    let mut rm = hgl(&p, &["tree", "remove", "w1", "demo", "3"]);
    isolate(&mut rm, &d.0);
    let o = rm.output().unwrap();
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        out(&o).contains("salvage: ") && !out(&o).contains("salvage: none"),
        "{}",
        out(&o)
    );
    assert!(
        err(&o).contains("still registered"),
        "hub unreachable: {}",
        err(&o)
    );
    assert!(!c3.dir.exists());
    assert_eq!(
        std::fs::read_dir(d.0.join("work/salvage")).unwrap().count(),
        1
    );
    let mut again = hgl(&p, &["tree", "remove", "w1", "demo", "3"]);
    isolate(&mut again, &d.0);
    assert_eq!(again.output().unwrap().status.code(), Some(1));

    // A tree whose turn lock is held (a running turn) is not removed.
    let lock = hgl_ai_bridge::lock::FileLock::turn(&c4.dir.join("turn.lock"))
        .unwrap()
        .unwrap();
    let mut busy = hgl(&p, &["tree", "remove", "w1", "demo", "4"]);
    isolate(&mut busy, &d.0);
    let o = busy.output().unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(err(&o).contains("a turn is running"), "{}", err(&o));
    assert!(c4.dir.exists());
    drop(lock);
}

struct Hub {
    child: Child,
    url: String,
    token: String,
}

impl Hub {
    fn start(dir: &Path) -> Option<Hub> {
        let Some(bin) = std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join("nats-server"))
            .find(|p| p.is_file())
        else {
            eprintln!("skipped: nats-server is not on PATH");
            return None;
        };
        let token = format!("br{}", std::process::id());
        let child = Command::new(bin)
            .args(["-js", "-a", "127.0.0.1", "-p", "-1", "--auth", &token])
            .arg("-sd")
            .arg(dir.join("store"))
            .arg("--ports_file_dir")
            .arg(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start nats-server");
        let ports = dir.join(format!("nats-server_{}.ports", child.id()));
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
        Some(Hub { child, url, token })
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn until(what: &str, secs: u64, log: &dyn Fn() -> String, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(
            Instant::now() < end,
            "timed out waiting for: {what}\nbridge log:\n{}",
            log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn the_bridge_serves_a_seat_against_a_live_hub() {
    let d = Scratch::new("live");
    let Some(hub) = Hub::start(&d.0) else { return };
    let g = git(&d.0);
    remote(&g, &d.0, "demo");
    let token = d.0.join("hub.token");
    std::fs::write(&token, format!("{}\n", hub.token)).unwrap();
    let (mut cfg, p) = config(&d.0, &hub.url, &token);
    cfg.seats.push(SeatConfig {
        name: Seat::new("w1").unwrap(),
        role: Role::Implementor,
        binding: Binding {
            runtime: Runtime::Codex,
            family: None,
            tier: Some(Tier::Fast),
            model: None,
            effort: None,
        },
        fallback: vec![],
        memcap_gb: Some(4),
    });
    cfg.save(&p).unwrap();
    assert!(hgl(&p, &["init"]).output().unwrap().status.success());

    // A fake `codex`: records its working directory, prints a real capture.
    let bin = d.0.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../bridge/fixtures/codex-ok.jsonl");
    let calls = d.0.join("calls");
    let script = format!(
        "#!/bin/sh\npwd >> '{}'\ncat '{}'\n",
        calls.display(),
        fixture.display()
    );
    std::fs::write(bin.join("codex"), script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(bin.join("codex"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut bridge = hgl(&p, &["bridge", "run", "--seat", "w1"]);
    isolate(&mut bridge, &d.0);
    let log = std::fs::File::create(d.0.join("bridge.log")).unwrap();
    let daemon = Daemon(
        bridge
            .env("PATH", &path)
            .env("HGL_AI_LOG", "info")
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let bridge_log = || std::fs::read_to_string(d.0.join("bridge.log")).unwrap_or_default();

    until("the host record lists w1", 20, &bridge_log, || {
        out(&hgl(&p, &["hosts"]).output().unwrap()).contains("w1 implementor/codex")
    });
    let send = |kind: &str, body: &str| {
        let o = hgl(
            &p,
            &[
                "note", "send", "--from", "claude", "--to", "w1", "--kind", kind, "--repo", "demo",
                "--issue", "7", "--body", body,
            ],
        )
        .output()
        .unwrap();
        assert!(o.status.success(), "{}", err(&o));
    };
    send("assign", "[claude] Assign: demo#7 Seven");
    let src = d.0.join("work/w1/demo-7/src");
    until("the turn ran in the tree", 30, &bridge_log, || {
        std::fs::read_to_string(&calls).is_ok_and(|c| c.trim() == src.display().to_string())
    });
    until(
        "the tree is registered in HGL_TREES",
        20,
        &bridge_log,
        || {
            let o = hgl(&p, &["--json", "tree", "list"]).output().unwrap();
            let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap_or_default();
            v.as_array().is_some_and(|a| {
                a.iter().any(|r| {
                    r["seat"] == "w1" && r["task"]["issue"] == 7 && r["host"] == "test-host"
                })
            })
        },
    );
    let o = hgl(&p, &["note", "wait", "w1", "--timeout", "1s"])
        .output()
        .unwrap();
    assert!(
        err(&o).contains("served by the bridge on host test-host"),
        "PROTO-6: {}",
        err(&o)
    );

    send("landed", "[claude] Landed: demo#7");
    let mut seen = String::new();
    until("a cleanup receipt reaches claude", 30, &bridge_log, || {
        let o = hgl(&p, &["note", "wait", "claude", "--timeout", "2s"])
            .output()
            .unwrap();
        seen.push_str(&out(&o));
        seen.contains("Cleanup: demo#7 on test-host/w1")
    });
    assert!(
        seen.contains("work_root uses"),
        "the start-up report: {seen}"
    );
    assert!(!d.0.join("work/w1/demo-7").exists());
    assert_eq!(
        std::fs::read_to_string(&calls).unwrap().lines().count(),
        1,
        "no turn for `landed`\n{}",
        bridge_log()
    );

    let mut daemon = daemon;
    // SIGTERM: a graceful stop removes the host record.
    let pid = daemon.0.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let end = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = daemon.0.try_wait().unwrap() {
            break s;
        }
        assert!(
            Instant::now() < end,
            "the bridge did not stop\n{}",
            bridge_log()
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "{}", bridge_log());
    let o = hgl(&p, &["hosts"]).output().unwrap();
    assert!(!out(&o).contains("test-host"), "{}", out(&o));
    drop(hub);
}
