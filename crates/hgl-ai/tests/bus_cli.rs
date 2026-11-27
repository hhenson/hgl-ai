//! The `note`, `state`, `init`, `hosts` and `cap` commands, run as the binary
//! against a live `nats-server -js` (skipped, with the reason printed, when
//! `nats-server` is not on `PATH`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use hgl_ai_proto::{Family, HostRecord, Role, Runtime, Seat, SeatRecord};

/// A scratch directory under cargo's target tmp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("bus-cli-{tag}-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Hub {
    child: Child,
    url: String,
    dir: Scratch,
    config: PathBuf,
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
        let token = format!("cli{}", std::process::id());
        let child = Command::new(bin)
            .args(["-js", "-a", "127.0.0.1", "-p", "-1", "--auth", &token])
            .arg("-sd")
            .arg(dir.0.join("store"))
            .arg("-l")
            .arg(dir.0.join("nats.log"))
            .arg("--ports_file_dir")
            .arg(&dir.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start nats-server");
        let ports = dir.0.join(format!("nats-server_{}.ports", child.id()));
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
        let mut cfg = hgl_ai_config::Config::new("test-host");
        cfg.hub.url = url.clone();
        cfg.hub.token_file = token_file.display().to_string();
        cfg.save(&config).unwrap();
        Some(Hub {
            child,
            url,
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
            .env_remove("HGL_AI_CONFIG");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).stdin(Stdio::null()).output().unwrap()
    }

    fn run_stdin(&self, args: &[&str], input: &str) -> Output {
        let mut c = self
            .cmd(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        c.wait_with_output().unwrap()
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.dir.0.join("hub.token"))
            .unwrap()
            .trim()
            .to_string()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[track_caller]
fn ok(o: &Output) -> String {
    assert_eq!(
        o.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        out(o),
        err(o)
    );
    out(o)
}

#[test]
fn notes_send_wait_peek() {
    let Some(hub) = Hub::start("notes") else {
        return;
    };
    assert_eq!(
        ok(&hub.run(&["init"])),
        "provisioned HGL_NOTES HGL_STATE HGL_HOSTS HGL_TREES HGL_LOCKS\n"
    );
    ok(&hub.run(&["init"]));

    let sent = ok(&hub.run(&[
        "note", "send", "--from", "w1", "--to", "claude", "--kind", "note", "--body", "first",
    ]));
    assert_eq!(sent, "seq 1\n");
    let mut env_from = hub.cmd(&[
        "note",
        "send",
        "--to",
        "claude",
        "--kind",
        "question",
        "--body-file",
        "-",
    ]);
    env_from
        .env("HGL_AI_SEAT", "w2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = env_from.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"from stdin\n")
        .unwrap();
    assert_eq!(ok(&child.wait_with_output().unwrap()), "seq 2\n");

    let peeked = ok(&hub.run(&["note", "peek", "claude"]));
    let lines: Vec<&str> = peeked.lines().collect();
    assert_eq!(lines.len(), 4, "{peeked}");
    assert!(
        lines[0].starts_with("--- seq=1 from=w1 kind=note at "),
        "{peeked}"
    );
    assert_eq!(lines[1], "first");
    assert!(
        lines[2].starts_with("--- seq=2 from=w2 kind=question at "),
        "{peeked}"
    );
    assert_eq!(lines[3], "from stdin");

    let waited = ok(&hub.run(&["--json", "note", "wait", "claude", "--timeout", "5s"]));
    let notes: Vec<serde_json::Value> = waited
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(notes.len(), 2);
    assert_eq!(
        (notes[0]["seq"].as_u64(), notes[0]["delivered"].as_u64()),
        (Some(1), Some(2)),
        "peek counted"
    );
    assert_eq!(notes[1]["body"], "from stdin\n");

    let empty = hub.run(&["note", "wait", "claude", "--timeout", "1s"]);
    assert_eq!(empty.status.code(), Some(3), "{}", err(&empty));
    assert_eq!(out(&empty), "");
    assert!(
        err(&empty).contains("no note for claude within 1s"),
        "{}",
        err(&empty)
    );

    let bad = hub.run(&[
        "note", "send", "--from", "w1", "--to", "claude", "--kind", "ready", "--body", "x",
    ]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(err(&bad).contains("needs a full head"), "{}", err(&bad));
    let usage = hub.run(&[
        "note", "send", "--from", "w1", "--to", "claude", "--kind", "note",
    ]);
    assert_eq!(usage.status.code(), Some(2), "a body is required");
}

#[test]
fn bridged_seats_refuse_interactive_waits() {
    let Some(hub) = Hub::start("bridged") else {
        return;
    };
    ok(&hub.run(&["init"]));
    assert_eq!(ok(&hub.run(&["hosts"])), "");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let bus = hgl_ai_bus::Bus::connect_url(&hub.url, Some(hub.token()))
            .await
            .unwrap();
        bus.host_put(&HostRecord {
            host: "build-1".into(),
            seats: vec![SeatRecord {
                seat: Seat::new("w3").unwrap(),
                role: Role::Implementor,
                runtime: Runtime::Codex,
                family: Family::Gpt,
                model: "gpt-6.1-sol".into(),
            }],
            memcap_gb: 64,
            work_root_free_gb: 812,
            updated_at: "20261127T083000Z".into(),
        })
        .await
        .unwrap();
    });
    assert_eq!(
        ok(&hub.run(&["hosts"])),
        "build-1 at 20261127T083000Z memcap 64G free 812G: w3 implementor/codex/gpt-6.1-sol\n"
    );
    let rec: serde_json::Value =
        serde_json::from_str(ok(&hub.run(&["--json", "hosts"])).trim()).unwrap();
    assert_eq!(rec["seats"][0]["seat"], "w3");

    for sub in ["wait", "peek"] {
        let refused = hub.run(&["note", sub, "w3"]);
        assert_eq!(refused.status.code(), Some(1), "{sub}");
        assert!(
            err(&refused).contains("served by the bridge on host build-1"),
            "{}",
            err(&refused)
        );
    }
    let forced = hub.run(&["note", "wait", "w3", "--timeout", "1s", "--force"]);
    assert_eq!(forced.status.code(), Some(3), "{}", err(&forced));

    // A held seat lease refuses too, with no host record (a bridge between the two).
    let lease = rt.block_on(async {
        let bus = hgl_ai_bus::Bus::connect_url(&hub.url, Some(hub.token()))
            .await
            .unwrap();
        bus.lock_acquire("seat.w4", "build-2 pid 7 (bridge)")
            .await
            .unwrap()
    });
    for sub in ["wait", "peek"] {
        let refused = hub.run(&["note", sub, "w4"]);
        assert_eq!(refused.status.code(), Some(1), "{sub}");
        assert!(
            err(&refused)
                .contains("seat w4 is held by build-2 pid 7 (bridge) (lease seat.w4 since"),
            "{}",
            err(&refused)
        );
    }
    let forced = hub.run(&["note", "wait", "w4", "--timeout", "1s", "--force"]);
    assert_eq!(forced.status.code(), Some(3), "{}", err(&forced));
    rt.block_on(lease.release()).unwrap();
    let free = hub.run(&["note", "wait", "w4", "--timeout", "1s"]);
    assert_eq!(free.status.code(), Some(3), "{}", err(&free));
}

#[test]
fn state_commands() {
    let Some(hub) = Hub::start("state") else {
        return;
    };
    ok(&hub.run(&["init"]));
    let missing = hub.run(&["state", "get", "prs"]);
    assert_eq!(
        (missing.status.code(), out(&missing).as_str()),
        (Some(1), "")
    );

    let rev = ok(&hub.run_stdin(&["state", "put", "prs", "-"], "# read 20261127T041100Z\n"));
    let r1: u64 = rev
        .trim()
        .strip_prefix("revision ")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        ok(&hub.run(&["state", "get", "prs"])),
        "# read 20261127T041100Z\n"
    );
    let j: serde_json::Value =
        serde_json::from_str(&ok(&hub.run(&["--json", "state", "get", "prs"]))).unwrap();
    assert_eq!(j["revision"].as_u64(), Some(r1));

    let r1s = r1.to_string();
    ok(&hub.run(&["state", "put", "prs", "two", "--if-revision", &r1s]));
    let lost = hub.run(&["state", "put", "prs", "three", "--if-revision", &r1s]);
    assert_eq!(lost.status.code(), Some(3), "{}", err(&lost));
    assert!(
        err(&lost).contains("no longer at revision"),
        "{}",
        err(&lost)
    );
    let exists = hub.run(&["state", "put", "prs", "x", "--if-revision", "0"]);
    assert_eq!(exists.status.code(), Some(3));
    assert_eq!(ok(&hub.run(&["state", "get", "prs"])), "two\n");

    ok(&hub.run(&[
        "state",
        "append",
        "landed.log",
        "20261127T080000Z hgl-compiler #1 aaa",
    ]));
    ok(&hub.run_stdin(
        &["state", "append", "landed.log", "-"],
        "20261127T080100Z hgl-compiler #2 bbb\n",
    ));
    assert_eq!(
        ok(&hub.run(&["state", "get", "landed.log"])),
        "20261127T080000Z hgl-compiler #1 aaa\n20261127T080100Z hgl-compiler #2 bbb\n"
    );

    assert_eq!(ok(&hub.run(&["state", "put", "hold", ""])), "deleted\n");
    ok(&hub.run(&["state", "del", "prs"]));
    assert_eq!(hub.run(&["state", "get", "prs"]).status.code(), Some(1));
    let bad = hub.run(&["state", "get", "a b"]);
    assert_eq!(bad.status.code(), Some(1));
}

#[test]
fn unreachable_hub_is_status_10() {
    let Some(hub) = Hub::start("unreachable") else {
        return;
    };
    std::fs::write(hub.dir.0.join("hub.token"), "wrong\n").unwrap();
    let o = hub.run(&["state", "get", "prs"]);
    assert_eq!(o.status.code(), Some(10), "{}", err(&o));
    assert!(err(&o).contains("hub unreachable"), "{}", err(&o));
}

#[test]
fn cap_passes_status_and_reports_limits() {
    let cap = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_hgl-ai"))
            .arg("cap")
            .args(args)
            .output()
            .unwrap()
    };
    assert_eq!(
        cap(&["64M", "--", "sh", "-c", "exit 7"]).status.code(),
        Some(7)
    );
    assert_eq!(
        cap(&["64M", "sh", "-c", "echo $0 -x", "arg0"])
            .status
            .code(),
        Some(0),
        "no -- needed"
    );
    let echoed = cap(&[
        "64M",
        "--",
        "sh",
        "-c",
        "echo \"$$\" >/dev/null; printf '%s' \"$1\"",
        "sh",
        "a$b",
    ]);
    assert_eq!(
        String::from_utf8_lossy(&echoed.stdout),
        "a$b",
        "arguments reach the command unchanged"
    );
    let start = Instant::now();
    let timed = cap(&[
        "512M",
        "--time",
        "1",
        "--kill-after",
        "1",
        "--",
        "sleep",
        "30",
    ]);
    assert_eq!(timed.status.code(), Some(143), "{}", err(&timed));
    assert!(
        err(&timed).contains("the 1s time limit was reached"),
        "{}",
        err(&timed)
    );
    assert!(start.elapsed() < Duration::from_secs(10));
    let usage = cap(&["lots", "--", "true"]);
    assert_eq!(usage.status.code(), Some(2));
    assert!(err(&usage).contains("invalid size"), "{}", err(&usage));
    let missing = cap(&["64M", "--", "hgl-ai-no-such-command"]);
    assert_eq!(missing.status.code(), Some(127));
}
