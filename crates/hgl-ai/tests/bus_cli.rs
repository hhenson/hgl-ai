//! The `note`, `state`, `init`, `hosts` and `cap` commands, run as the binary
//! against a live `nats-server -js` (skipped, with the reason printed, when
//! `nats-server` is not on `PATH`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use hgl_ai_bus::{Bus, Credential, parse_credential};
use hgl_ai_install::{
    nats::{Auth, render_conf},
    users::{Registry, Role as HubRole},
};
use hgl_ai_proto::{
    Family, HostRecord, KEY_DECISIONS, KV_SCREEN, KV_STATE, Role, Runtime, Seat, SeatRecord,
    screen::{ScreenAction, ScreenLog, ScreenMode, ScreenRule},
};

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
        let token = format!("cli{}", std::process::id());
        Self::launch(tag, &token, |cmd, dir| {
            cmd.args(["-js", "-a", "127.0.0.1", "-p", "-1", "--auth", &token])
                .arg("-sd")
                .arg(dir.join("store"));
        })
    }

    fn launch(
        tag: &str,
        credential: &str,
        configure: impl FnOnce(&mut Command, &Path),
    ) -> Option<Hub> {
        let Some(bin) = std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join("nats-server"))
            .find(|p| p.is_file())
        else {
            eprintln!("skipped: nats-server is not on PATH");
            return None;
        };
        let dir = Scratch::new(tag);
        let mut cmd = Command::new(bin);
        configure(&mut cmd, &dir.0);
        let log = dir.0.join("nats.log");
        let stderr = dir.0.join("nats.stderr");
        let mut child = cmd
            .arg("-l")
            .arg(&log)
            .arg("--ports_file_dir")
            .arg(&dir.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr).unwrap())
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
            if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "nats-server did not report its port\n{}\n{}",
                    std::fs::read_to_string(&stderr).unwrap_or_default(),
                    std::fs::read_to_string(&log).unwrap_or_default(),
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let token_file = dir.0.join("hub.token");
        std::fs::write(&token_file, credential).unwrap();
        let config = dir.0.join("config.toml");
        let mut cfg = hgl_ai_config::Config::new("test-host");
        cfg.hub.url = url.clone();
        cfg.hub.token_file = token_file.display().to_string();
        // Seat locks and the host id stay in the scratch directory.
        cfg.paths.data_dir = dir.0.join("data").display().to_string();
        cfg.paths.work_root = dir.0.join("work").display().to_string();
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
            .env_remove("HGL_AI_CONFIG")
            .env_remove("HGL_AI_HUB_CREDENTIAL");
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
        "provisioned HGL_NOTES HGL_OWNER HGL_USAGE_LOG HGL_STATE HGL_HOSTS HGL_TREES HGL_LOCKS HGL_ENDED HGL_THREADS HGL_METRICS HGL_CONSOLE HGL_SLACK HGL_SCREEN HGL_USAGE HGL_BUILD\n"
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

    // Peeks spend no delivery, however many (hgl-ai#5, Carbon's check 3).
    for _ in 0..7 {
        ok(&hub.run(&["note", "peek", "claude"]));
    }
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
        (Some(1), Some(1)),
        "the peeks spent no delivery"
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

    let screen = ok(&hub.run(&[
        "note", "send", "--from", "screen", "--to", "claude", "--kind", "answer", "--body", "seen",
    ]));
    assert_eq!(screen, "seq 3\n");
    let peeked = ok(&hub.run(&["note", "peek", "claude"]));
    assert!(
        peeked.contains("--- seq=3 from=screen kind=answer at "),
        "{peeked}"
    );
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
        "build-1 at 20261127T083000Z memcap 64G free 812 GB: w3 implementor/codex/gpt-6.1-sol\n"
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
    assert!(
        err(&forced)
            .contains("warning: --force: receiving for seat w3 without its lock and lease seat.w3"),
        "{}",
        err(&forced)
    );

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

/// `note wait` holds the seat lease while it waits, and gives it up when it returns, a
/// note or a signal ending it (PROTO-6).
#[test]
fn an_interactive_wait_holds_the_seat_lease() {
    let Some(hub) = Hub::start("held") else {
        return;
    };
    ok(&hub.run(&["init"]));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bus = rt.block_on(async {
        hgl_ai_bus::Bus::connect_url(&hub.url, Some(hub.token()))
            .await
            .unwrap()
    });
    let holder = || {
        rt.block_on(bus.lock_get("seat.w5"))
            .unwrap()
            .map(|r| r.holder)
    };
    let spawn_wait = || {
        hub.cmd(&["note", "wait", "w5", "--timeout", "60s"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let held_by = |c: &Child| {
        let want = format!("test-host pid {} (note wait) id ", c.id());
        let end = Instant::now() + Duration::from_secs(20);
        while !holder().is_some_and(|h| h.starts_with(&want)) {
            assert!(
                Instant::now() < end,
                "the wait took no lease: {:?}",
                holder()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        holder().unwrap()
    };

    let waiting = spawn_wait();
    held_by(&waiting);
    // The seat's lock on this host refuses a second receive here (PROTO-6).
    let refused = hub.run(&["note", "peek", "w5"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        err(&refused).contains(&format!(
            "seat w5 is served on this host by pid {}",
            waiting.id()
        )),
        "{}",
        err(&refused)
    );
    ok(&hub.run(&[
        "note", "send", "--from", "claude", "--to", "w5", "--kind", "note", "--body", "go",
    ]));
    let done = waiting.wait_with_output().unwrap();
    assert!(ok(&done).ends_with("go\n"), "{}", out(&done));
    assert_eq!(holder(), None, "released when the wait returns");

    // Interrupted, it gives the lease up too.
    let waiting = spawn_wait();
    held_by(&waiting);
    let pid = waiting.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-INT", &pid])
            .status()
            .unwrap()
            .success()
    );
    let done = waiting.wait_with_output().unwrap();
    assert_eq!(done.status.code(), Some(130), "{}", err(&done));
    assert_eq!(holder(), None, "released on SIGINT");

    // Killed with SIGKILL it releases nothing; the next wait of this host takes the lease
    // over at once instead of waiting for it to expire (Carbon's check 6).
    let mut waiting = spawn_wait();
    let stale = held_by(&waiting);
    waiting.kill().unwrap();
    waiting.wait().unwrap();
    assert_eq!(holder().as_deref(), Some(stale.as_str()));
    let again = hub.run(&["note", "wait", "w5", "--timeout", "1s"]);
    assert_eq!(again.status.code(), Some(3), "{}", err(&again));
    assert!(
        err(&again).contains(&format!("taking over the seat lease seat.w5 from {stale}")),
        "{}",
        err(&again)
    );
    assert_eq!(holder(), None, "released by the wait that took it over");
    // A holder of another machine that shares the label is not taken over.
    let other = format!("test-host pid 999999999 (note wait) id {}", "f".repeat(16));
    let lease = rt.block_on(bus.lock_acquire("seat.w5", &other)).unwrap();
    let refused = hub.run(&["note", "wait", "w5", "--timeout", "1s"]);
    assert_eq!(refused.status.code(), Some(1), "{}", err(&refused));
    assert!(err(&refused).contains(&format!("seat w5 is held by {other}")));
    rt.block_on(lease.release()).unwrap();
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

/// `usage collect`, `show` and `label` against a live hub, the sources being the captured
/// fixtures behind stand-in CLIs (PROTO-23, PROTO-24).
#[test]
fn usage_collect_show_label() {
    use std::os::unix::fs::PermissionsExt as _;
    let Some(hub) = Hub::start("usage") else {
        return;
    };
    ok(&hub.run(&["init"]));
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("../usage/fixtures");
    let root = hub.dir.0.join("sources");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let cli = |name: &str, auth: &str, usage: &str| {
        let p = bin.join(name);
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\ncase \"$1 $2\" in\n  'auth status') cat '{}' ;;\n  '-p /usage') cat '{}' ;;\n  *) exit 2 ;;\nesac\n",
                fx.join(auth).display(),
                fx.join(usage).display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    };
    let claude = cli("claude", "claude-auth.json", "claude-usage.txt");
    let agy = cli("agy", "claude-auth.json", "agy-usage.txt");
    let codex = root.join("codex");
    std::fs::create_dir_all(codex.join("sessions/2026/11/27")).unwrap();
    std::fs::copy(
        fx.join("codex-auth.synthetic.json"),
        codex.join("auth.json"),
    )
    .unwrap();
    std::fs::copy(
        fx.join("codex-rollout.jsonl"),
        codex.join("sessions/2026/11/27/rollout-2026-11-27T21-00-34-a.jsonl"),
    )
    .unwrap();
    let agy_home = root.join("agy");
    std::fs::create_dir_all(agy_home.join("log")).unwrap();
    std::fs::copy(fx.join("agy-cli.log"), agy_home.join("log/cli-1.log")).unwrap();
    let run = |args: &[&str]| {
        hub.cmd(args)
            .env("HGL_AI_USAGE_CLAUDE", &claude)
            .env("HGL_AI_USAGE_AGY", &agy)
            .env("CODEX_HOME", &codex)
            .env("HGL_AI_USAGE_AGY_HOME", &agy_home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };

    assert_eq!(
        ok(&hub.run(&["usage", "show"])),
        "no usage records; run `hgl-ai usage collect`\n"
    );
    let dry = ok(&run(&[
        "usage",
        "collect",
        "--dry-run",
        "--runtime",
        "codex",
    ]));
    assert_eq!(dry.lines().count(), 1, "{dry}");
    assert!(dry.contains("\"runtime\":\"codex\""), "{dry}");
    assert_eq!(
        ok(&hub.run(&["usage", "show"])),
        "no usage records; run `hgl-ai usage collect`\n",
        "a dry run publishes nothing"
    );

    let first = ok(&run(&["usage", "collect"]));
    let lines: Vec<&str> = first.lines().collect();
    assert_eq!(lines.len(), 3, "{first}");
    assert!(
        lines[0].starts_with("claude.") && lines[0].ends_with(": 2 buckets, written"),
        "{first}"
    );
    assert!(lines[1].ends_with(": 1 bucket, written"), "{first}");
    assert!(lines[2].ends_with(": 4 buckets, written"), "{first}");
    // The Codex observation is the rollout's own: the same again is stale.
    let again = ok(&run(&["usage", "--json", "collect", "--runtime", "codex"]));
    let v: serde_json::Value = serde_json::from_str(again.trim()).unwrap();
    assert_eq!(v["outcome"], "stale", "{again}");
    assert_eq!(v["host"], "test-host");

    let table = ok(&hub.run(&["usage", "show"]));
    let rows: Vec<&str> = table.lines().collect();
    assert!(rows[0].starts_with("RUNTIME  ACCOUNT"), "{table}");
    assert_eq!(rows.len(), 1 + 2 + 1 + 4, "{table}");
    assert!(
        rows.iter()
            .any(|r| r.starts_with("codex") && r.contains("promax") && r.contains(" 7% ")),
        "{table}"
    );
    for secret in ["someone@example.com", "acct-fixture", "FIXTURE-NOT-A-TOKEN"] {
        assert!(!table.contains(secret), "{secret}");
    }

    let codex_key = lines[1].split(':').next().unwrap();
    let account = codex_key.strip_prefix("codex.").unwrap();
    assert_eq!(
        ok(&hub.run(&["usage", "label", account, "team-gpt"])),
        format!("{account} labelled team-gpt\n")
    );
    let json = ok(&hub.run(&["--json", "usage", "show"]));
    let codex_rec: serde_json::Value = json
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["runtime"] == "codex")
        .unwrap();
    assert_eq!(codex_rec["label"], "team-gpt");
    assert!(codex_rec["buckets"][0]["hours_to_reset"].is_number());
    assert!(!json.contains("someone@example.com"));
    ok(&hub.run(&["usage", "label", account, ""]));
    assert!(!ok(&hub.run(&["usage", "show"])).contains("team-gpt"));
    let bad = hub.run(&["usage", "label", "nothex", "x"]);
    assert_eq!(bad.status.code(), Some(1), "{}", err(&bad));
}

#[tokio::test]
async fn screen_commands_audit_and_read_on_both_hub_layouts() {
    for accounts in [false, true] {
        let mut registry = Registry::default();
        let admin = registry.issue("test-admin", HubRole::Admin, false).unwrap();
        let rules = registry
            .issue("test-rules", HubRole::ScreenRules, false)
            .unwrap();
        let screen = registry.issue("screen", HubRole::Screen, false).unwrap();
        let seat = registry.issue("test-seat", HubRole::Seat, false).unwrap();
        let auth = if accounts {
            Auth::Accounts(&registry)
        } else {
            Auth::Users(&registry)
        };
        let Some(hub) = Hub::launch("screen", &rules, |cmd, dir| {
            let conf = dir.join("nats.conf");
            let store = dir.join("store");
            std::fs::create_dir_all(&store).unwrap();
            let text = render_conf("127.0.0.1", 0, &store, &auth);
            std::fs::write(&conf, text.replace("port: 0\n", "port: -1\n")).unwrap();
            cmd.arg("-c").arg(conf);
        }) else {
            return;
        };
        let admin_cred = hub.dir.0.join("admin.cred");
        std::fs::write(&admin_cred, &admin).unwrap();
        ok(&hub
            .cmd(&["init"])
            .env("HGL_AI_HUB_CREDENTIAL", &admin_cred)
            .output()
            .unwrap());

        // A seat name and missing role label cannot override the broker's grants.
        for (name, credential) in [("seat", &seat), ("screen", &screen)] {
            let Some(Credential::Nkey { seed, .. }) = parse_credential(credential).unwrap() else {
                panic!("expected an NKey");
            };
            let bare = hub.dir.0.join(format!("{name}.cred"));
            std::fs::write(&bare, seed).unwrap();
            let refused = hub
                .cmd(&["screen", "rule", "list"])
                .env("HGL_AI_HUB_CREDENTIAL", &bare)
                .env("HGL_AI_SEAT", "controller")
                .output()
                .unwrap();
            assert_eq!(refused.status.code(), Some(1));
            assert!(
                err(&refused).contains("may not publish"),
                "{}",
                err(&refused)
            );
        }

        let admin_client = nats_for(&hub, &admin).await;
        let state = admin_client.get_key_value(KV_STATE).await.unwrap();
        let stored_rules = admin_client.get_key_value(KV_SCREEN).await.unwrap();
        let screen = Bus::connect_with(&hub.url, parse_credential(&screen).unwrap())
            .await
            .unwrap();

        assert_eq!(
            ok(&hub.run(&["--json", "screen", "rule", "list"])).trim(),
            "[]"
        );
        let rule = r#"{
            "id":"progress", "kinds":["note"], "roles":["implementor"],
            "when":"The note reports routine progress.", "action":"close",
            "cites":"example#185", "expires":"29990101T000000Z"
        }"#;
        let act = rule.replace(r#""expires""#, r#""mode":"act", "expires""#);
        let refused = hub.run_stdin(&["screen", "rule", "add", "-"], &act);
        assert_eq!(refused.status.code(), Some(1));
        assert!(err(&refused).contains("shadow"));
        assert!(screen.screen_rules().await.unwrap().is_empty());

        // Make the second write fail without affecting the rule stream. The retry must
        // finish the missing decision and preserve a single record of each change.
        state.put(KEY_DECISIONS, vec![0xff].into()).await.unwrap();
        let failed = hub.run_stdin(&["screen", "rule", "add", "-"], rule);
        assert_eq!(failed.status.code(), Some(1));
        assert!(
            err(&failed).contains("retry this command"),
            "{}",
            err(&failed)
        );
        let pending = screen.screen_rule_get("progress").await.unwrap().unwrap();
        assert!(pending.decision_at.is_some());

        let existing = "20260101T000000Z controller: Routine progress can close.";
        state
            .put(KEY_DECISIONS, format!("{existing}\n").into())
            .await
            .unwrap();
        let add = ["--json", "screen", "rule", "add", "-"];
        let added: ScreenRule = serde_json::from_str(&ok(&hub.run_stdin(&add, rule))).unwrap();
        assert_eq!(added.mode, ScreenMode::Shadow);
        assert_eq!(
            ok(&hub.run_stdin(&add, rule)),
            serde_json::to_string(&added).unwrap() + "\n"
        );
        let other = rule
            .replace("progress", "other")
            .replace("example#185", existing);
        let rule_file = hub.dir.0.join("other.json");
        std::fs::write(&rule_file, other).unwrap();
        ok(&hub.run(&["screen", "rule", "add", rule_file.to_str().unwrap()]));
        let shown = hub.run(&["--json", "screen", "rule", "show", "progress"]);
        assert_eq!(
            serde_json::from_str::<ScreenRule>(&ok(&shown)).unwrap(),
            added
        );

        let retire = ["--json", "screen", "rule", "retire", "progress"];
        let retired: ScreenRule = serde_json::from_str(&ok(&hub.run(&retire))).unwrap();
        assert!(retired.retired_at.is_some());
        assert_eq!(
            ok(&hub.run(&retire)),
            serde_json::to_string(&retired).unwrap() + "\n"
        );
        let listed = ok(&hub.run(&["--json", "screen", "rule", "list"]));
        let listed: Vec<ScreenRule> = serde_json::from_str(&listed).unwrap();
        assert_eq!(
            listed.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["other", "progress"]
        );
        assert_eq!(listed[1], retired);
        assert_eq!(screen.screen_rules().await.unwrap(), listed);
        let stream = stored_rules.stream;
        let old = stream.get_raw_message(pending.revision).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<ScreenRule>(&old.payload).unwrap(),
            added
        );
        let current = stream
            .get_last_raw_message_by_subject("$KV.HGL_SCREEN.rule.progress")
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<ScreenRule>(&current.payload).unwrap(),
            retired
        );

        let lines = ok(&hub.run(&["decision", "tail"]));
        let lines: Vec<_> = lines.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], existing);
        assert!(lines[1].contains("screen rule add progress"));
        assert!(lines[2].contains("screen rule add other"));
        assert!(lines[3].contains("screen rule retire progress"));

        for seq in [23, 2, 11, 23] {
            screen
                .screen_log_put(&ScreenLog {
                    seq,
                    rule: Some("progress".into()),
                    action: ScreenAction::Close,
                    reply: String::new(),
                    mode: ScreenMode::Shadow,
                    model: "fixture-model".into(),
                })
                .await
                .unwrap();
        }
        let log = ok(&hub.run(&["--json", "screen", "log", "--since", "2"]));
        let log: Vec<ScreenLog> = serde_json::from_str(&log).unwrap();
        assert_eq!(
            log.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
            [11, 23]
        );
        assert_eq!(
            ok(&hub.run(&["--json", "screen", "log", "--since", "23"])).trim(),
            "[]"
        );
        assert!(screen.screen_log(0).await.is_err());
    }
}

async fn nats_for(hub: &Hub, credential: &str) -> async_nats::jetstream::Context {
    let cred = parse_credential(credential).unwrap().unwrap();
    let inbox = cred.inbox_prefix().unwrap();
    let Credential::Nkey { seed, .. } = cred else {
        panic!("expected an admin key");
    };
    async_nats::jetstream::new(
        async_nats::ConnectOptions::new()
            .nkey(seed)
            .custom_inbox_prefix(inbox)
            .connect(&hub.url)
            .await
            .unwrap(),
    )
}
