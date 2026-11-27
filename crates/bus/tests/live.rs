//! Live tests against a real `nats-server -js` started per test on a free
//! loopback port with a token and a temporary store. Skipped, with the reason
//! printed, when `nats-server` is not on `PATH`.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use async_nats::jetstream;
use hgl_ai_bus::{Bus, BusError, InboxOptions};
use hgl_ai_proto::{
    Family, HostRecord, Kind, Note, Role, Runtime, Seat, SeatRecord, TaskKey, TreeRecord,
};

struct Hub {
    child: Child,
    _dir: tempfile::TempDir,
    url: String,
    token: String,
}

impl Hub {
    /// Start a hub on a port it picks itself, or print why not and return `None`.
    fn start() -> Option<Hub> {
        let Some(bin) = find_on_path("nats-server") else {
            eprintln!("skipped: nats-server is not on PATH");
            return None;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let token = format!(
            "t{}x{}",
            std::process::id(),
            dir.path().display().to_string().len()
        );
        let child = Command::new(bin)
            .args(["-js", "-a", "127.0.0.1", "-p", "-1", "--auth", &token])
            .arg("-sd")
            .arg(dir.path().join("store"))
            .arg("-l")
            .arg(dir.path().join("nats.log"))
            .arg("--ports_file_dir")
            .arg(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start nats-server");
        let ports = dir.path().join(format!("nats-server_{}.ports", child.id()));
        let mut hub = Hub {
            child,
            _dir: dir,
            url: String::new(),
            token,
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        hub.url = loop {
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
        Some(hub)
    }

    async fn bus(&self) -> Bus {
        Bus::connect_url(&self.url, Some(self.token.clone()))
            .await
            .expect("connect")
    }

    async fn ready(&self) -> Bus {
        let bus = self.bus().await;
        bus.provision().await.expect("provision");
        bus
    }

    async fn raw(&self) -> jetstream::Context {
        let c = async_nats::ConnectOptions::with_token(self.token.clone())
            .connect(&self.url)
            .await
            .expect("raw connect");
        jetstream::new(c)
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn seat(s: &str) -> Seat {
    Seat::new(s).unwrap()
}

fn note(from: &str, to: &str, body: &str) -> Note {
    Note::new(seat(from), seat(to), Kind::Note, body)
}

const SHORT: Duration = Duration::from_millis(300);
const LONG: Duration = Duration::from_secs(5);

#[tokio::test]
async fn provision_is_idempotent_and_corrects_settings() {
    let Some(hub) = Hub::start() else { return };
    let js = hub.raw().await;
    // A stream that exists with the wrong retention is brought to §3.
    js.create_stream(jetstream::stream::Config {
        name: "HGL_NOTES".into(),
        subjects: vec!["hgl.notes.>".into()],
        max_age: Duration::from_secs(3600),
        ..Default::default()
    })
    .await
    .unwrap();
    let bus = hub.bus().await;
    bus.provision().await.unwrap();
    bus.provision().await.unwrap();

    let info = js
        .get_stream("HGL_NOTES")
        .await
        .unwrap()
        .cached_info()
        .clone();
    assert_eq!(info.config.max_age, Duration::from_secs(14 * 86_400));
    assert_eq!(info.config.duplicate_window, Duration::from_secs(86_400));
    assert_eq!(info.config.storage, jetstream::stream::StorageType::File);
    assert_eq!(info.config.subjects, vec!["hgl.notes.>".to_string()]);

    let kv = |b: &'static str| {
        let js = js.clone();
        async move {
            js.get_stream(format!("KV_{b}"))
                .await
                .unwrap()
                .cached_info()
                .config
                .clone()
        }
    };
    let state = kv("HGL_STATE").await;
    assert_eq!(
        (state.max_messages_per_subject, state.storage),
        (5, jetstream::stream::StorageType::File)
    );
    let hosts = kv("HGL_HOSTS").await;
    assert_eq!(hosts.storage, jetstream::stream::StorageType::Memory);
    assert_eq!(hosts.max_age, Duration::from_secs(120));
    let trees = kv("HGL_TREES").await;
    assert_eq!(
        (trees.max_messages_per_subject, trees.storage),
        (1, jetstream::stream::StorageType::File)
    );
}

#[tokio::test]
async fn send_fetch_ack() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    let mut inbox = bus.inbox(&seat("claude")).await.unwrap();
    let mut n = note("w1", "claude", "[codex] Ready: hgl-compiler#980");
    n.kind = Kind::Ready;
    n.repo = Some("hgl-compiler".into());
    n.issue = 980;
    n.head = Some("8a5c3d264c2edbbdaa61530d810bff276c3eebba".into());
    let r = bus.send(&n).await.unwrap();
    assert!(!r.duplicate);
    assert!(r.seq > 0);

    let got = inbox.fetch(10, LONG).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].seq, got[0].delivered), (r.seq, 1));
    assert_eq!(got[0].note, n);
    got[0].ack().await.unwrap();
    assert!(inbox.fetch(10, SHORT).await.unwrap().is_empty());

    // Another seat's note does not reach this inbox.
    bus.send(&note("w1", "w2", "not for claude")).await.unwrap();
    assert!(inbox.fetch(10, SHORT).await.unwrap().is_empty());
}

#[tokio::test]
async fn connect_with_config_reads_the_token_file() {
    let Some(hub) = Hub::start() else { return };
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("hub.token");
    std::fs::write(&token_file, format!("{}\n", hub.token)).unwrap();
    let mut cfg = hgl_ai_config::Config::new("test-host");
    cfg.hub.url = hub.url.clone();
    cfg.hub.token_file = token_file.display().to_string();
    let bus = Bus::connect(&cfg).await.unwrap();
    bus.provision().await.unwrap();
    assert_eq!(bus.url(), hub.url);

    cfg.hub.token_file = dir.path().join("missing").display().to_string();
    assert!(matches!(
        Bus::connect(&cfg).await,
        Err(BusError::Token { .. })
    ));
}

#[tokio::test]
async fn fetch_returns_at_the_first_note_and_respects_max() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    let mut inbox = bus.inbox(&seat("w3")).await.unwrap();
    let sender = hub.bus().await;
    let t = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        sender.send(&note("claude", "w3", "late")).await.unwrap();
    });
    let start = Instant::now();
    let got = inbox.fetch(10, Duration::from_secs(20)).await.unwrap();
    assert_eq!(got.len(), 1);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "waited {:?}",
        start.elapsed()
    );
    got[0].ack().await.unwrap();
    t.await.unwrap();

    for i in 0..3 {
        bus.send(&note("claude", "w3", &format!("n{i}")))
            .await
            .unwrap();
    }
    let got = inbox.fetch(2, LONG).await.unwrap();
    assert_eq!(
        got.iter().map(|d| d.note.body.as_str()).collect::<Vec<_>>(),
        ["n0\n", "n1\n"]
    );
    for d in &got {
        d.ack().await.unwrap();
    }
    let rest = inbox.fetch(10, Duration::ZERO).await.unwrap();
    assert_eq!(rest.len(), 1, "a zero wait still returns what is pending");
    rest[0].ack().await.unwrap();
}

#[tokio::test]
async fn duplicate_send_is_reported() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    let n = note("w1", "claude", "same body");
    let a = bus.send(&n).await.unwrap();
    let b = bus.send(&n).await.unwrap();
    assert!(!a.duplicate);
    assert!(b.duplicate);
    assert_eq!(a.seq, b.seq);
    let mut inbox = bus.inbox(&seat("claude")).await.unwrap();
    let got = inbox.fetch(10, LONG).await.unwrap();
    assert_eq!(got.len(), 1, "the duplicate was not stored");
}

#[tokio::test]
async fn broadcast_reaches_every_inbox() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    let mut w1 = bus.inbox(&seat("w1")).await.unwrap();
    let r = bus
        .send(&note("claude", "all", "[claude] Landed"))
        .await
        .unwrap();
    // An inbox created after the broadcast still gets it.
    let mut w2 = bus.inbox(&seat("w2")).await.unwrap();
    for inbox in [&mut w1, &mut w2] {
        let got = inbox.fetch(10, LONG).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].seq, r.seq);
        assert!(got[0].note.recipient.is_broadcast());
        got[0].ack().await.unwrap();
    }
    assert!(
        bus.inbox(&Seat::broadcast()).await.is_err(),
        "`all` has no inbox"
    );
}

#[tokio::test]
async fn nak_redelivers_with_the_count_incremented() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    let mut inbox = bus.inbox(&seat("w4")).await.unwrap();
    bus.send(&note("claude", "w4", "retry me")).await.unwrap();

    let first = inbox.fetch(10, LONG).await.unwrap();
    assert_eq!(first[0].delivered, 1);
    first[0].nak(Duration::ZERO).await.unwrap();
    let second = inbox.fetch(10, LONG).await.unwrap();
    assert_eq!((second[0].seq, second[0].delivered), (first[0].seq, 2));

    second[0].nak(Duration::from_secs(2)).await.unwrap();
    assert!(
        inbox.fetch(10, SHORT).await.unwrap().is_empty(),
        "held back for the delay"
    );
    let third = inbox.fetch(10, LONG).await.unwrap();
    assert_eq!(third[0].delivered, 3);
    third[0].ack().await.unwrap();
}

#[tokio::test]
async fn in_progress_holds_a_note_past_ack_wait() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    let opts = InboxOptions {
        ack_wait: Duration::from_secs(1),
        ..InboxOptions::default()
    };
    let mut a = bus.inbox_with(&seat("w5"), opts).await.unwrap();
    let mut b = bus.inbox_with(&seat("w5"), opts).await.unwrap();

    bus.send(&note("claude", "w5", "long turn")).await.unwrap();
    let held = a.fetch(1, LONG).await.unwrap();
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        held[0].in_progress().await.unwrap();
    }
    assert!(
        b.fetch(1, SHORT).await.unwrap().is_empty(),
        "kept alive past ack_wait"
    );
    held[0].ack().await.unwrap();

    // Without in_progress, the same wait lets it go to another fetch.
    bus.send(&note("claude", "w5", "abandoned turn"))
        .await
        .unwrap();
    let dropped = a.fetch(1, LONG).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let again = b.fetch(1, LONG).await.unwrap();
    assert_eq!((again[0].seq, again[0].delivered), (dropped[0].seq, 2));
    again[0].ack().await.unwrap();
}

#[tokio::test]
async fn malformed_messages_are_terminated() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    let opts = InboxOptions {
        ack_wait: Duration::from_secs(1),
        ..InboxOptions::default()
    };
    let mut inbox = bus.inbox_with(&seat("claude"), opts).await.unwrap();
    let js = hub.raw().await;
    js.publish("hgl.notes.claude", "not json".into())
        .await
        .unwrap()
        .await
        .unwrap();
    // A well-formed note on the wrong subject is rejected too.
    let stray = serde_json::to_vec(&note("w1", "w2", "stray")).unwrap();
    js.publish("hgl.notes.claude", stray.into())
        .await
        .unwrap()
        .await
        .unwrap();
    let good = bus.send(&note("w1", "claude", "good")).await.unwrap();

    let got = inbox.fetch(10, LONG).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].seq, good.seq);
    got[0].ack().await.unwrap();
    // Past ack_wait nothing comes back: the bad ones were terminated, not left pending.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(inbox.fetch(10, SHORT).await.unwrap().is_empty());
    let info = js
        .get_stream("HGL_NOTES")
        .await
        .unwrap()
        .consumer_info("inbox-claude")
        .await
        .unwrap();
    assert_eq!((info.num_pending, info.num_ack_pending), (0, 0));
}

#[tokio::test]
async fn state_put_get_delete_with_revisions() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    assert_eq!(bus.state_get("prs").await.unwrap(), None);
    let r1 = bus
        .state_put("prs", "# read 20261127T041100Z\n")
        .await
        .unwrap();
    let e = bus.state_get("prs").await.unwrap().unwrap();
    assert_eq!(
        (e.value.as_str(), e.revision),
        ("# read 20261127T041100Z\n", r1)
    );
    let r2 = bus.state_put("prs", "second").await.unwrap();
    assert!(r2 > r1);
    assert_eq!(bus.state_get("prs").await.unwrap().unwrap().revision, r2);

    assert_eq!(
        bus.state_put("prs", "").await.unwrap(),
        0,
        "an empty value deletes"
    );
    assert_eq!(bus.state_get("prs").await.unwrap(), None);

    bus.state_put("hold", "20261127T060000Z").await.unwrap();
    bus.state_del("hold").await.unwrap();
    assert_eq!(bus.state_get("hold").await.unwrap(), None);
    bus.state_del("hold").await.unwrap();

    bus.state_put("landed.log", "x").await.unwrap();
    assert!(bus.state_get("landed.log").await.unwrap().is_some());
    assert!(matches!(
        bus.state_put("a b", "x").await,
        Err(BusError::Proto(_))
    ));
}

#[tokio::test]
async fn hosts_and_trees_round_trip() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    assert!(bus.hosts().await.unwrap().is_empty());
    let host = HostRecord {
        host: "build-1".into(),
        seats: vec![SeatRecord {
            seat: seat("w3"),
            role: Role::Implementor,
            runtime: Runtime::Codex,
            family: Family::Gpt,
            model: "gpt-6.1-sol".into(),
        }],
        memcap_gb: 64,
        work_root_free_gb: 812,
        updated_at: "20261127T083000Z".into(),
    };
    let other = HostRecord {
        host: "build-0".into(),
        seats: vec![],
        ..host.clone()
    };
    bus.host_put(&host).await.unwrap();
    bus.host_put(&other).await.unwrap();
    assert_eq!(
        bus.hosts().await.unwrap(),
        vec![other.clone(), host.clone()]
    );
    bus.host_del("build-0").await.unwrap();
    assert_eq!(bus.hosts().await.unwrap(), vec![host.clone()]);

    let tree = |s: &str, issue: u64, review: bool| TreeRecord {
        host: "build-1".into(),
        seat: seat(s),
        task: TaskKey {
            repo: "hgl-compiler".into(),
            issue,
        },
        review,
        path: format!("work/{s}/hgl-compiler-{issue}"),
        branch: (!review).then(|| format!("codex/{issue}-x")),
        created_at: "20261127T083000Z".into(),
        size_bytes: 42,
    };
    let a = tree("w1", 992, false);
    let b = tree("r1", 992, true);
    bus.tree_put(&a).await.unwrap();
    bus.tree_put(&b).await.unwrap();
    assert_eq!(
        bus.trees().await.unwrap(),
        vec![b.clone(), a.clone()],
        "ordered by key"
    );
    bus.tree_del(&b.key()).await.unwrap();
    assert_eq!(bus.trees().await.unwrap(), vec![a.clone()]);
    bus.tree_del(&b.key()).await.unwrap();
}

#[tokio::test]
async fn refused_or_absent_hub_is_unreachable() {
    let Some(hub) = Hub::start() else { return };
    let wrong = Bus::connect_url(&hub.url, Some("wrong".into())).await;
    assert!(
        matches!(wrong, Err(BusError::Unreachable { .. })),
        "wrong token"
    );
    let none = Bus::connect_url(&hub.url, None).await;
    assert!(
        matches!(none, Err(BusError::Unreachable { .. })),
        "no token"
    );
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .unwrap()
        .port();
    let closed = Bus::connect_url(&format!("nats://127.0.0.1:{port}"), None).await;
    assert!(
        matches!(closed, Err(BusError::Unreachable { .. })),
        "nothing listening"
    );
}

#[tokio::test]
async fn unprovisioned_hub_says_to_run_init() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.bus().await;
    let hint = |r: Result<(), BusError>| {
        let e = r.unwrap_err().to_string();
        assert!(e.contains("hgl-ai init"), "{e}");
    };
    hint(bus.send(&note("w1", "claude", "x")).await.map(|_| ()));
    hint(bus.inbox(&seat("claude")).await.map(|_| ()));
    hint(bus.state_get("prs").await.map(|_| ()));
    hint(bus.hosts().await.map(|_| ()));
}

#[tokio::test]
async fn compare_and_set() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    let r1 = bus.state_create("main", "updated x\n").await.unwrap();
    assert!(matches!(
        bus.state_create("main", "again").await,
        Err(BusError::Conflict { expected: 0, .. })
    ));
    let r2 = bus.state_update("main", "updated y\n", r1).await.unwrap();
    assert!(r2 > r1);
    let stale = bus.state_update("main", "lost", r1).await;
    assert!(
        matches!(stale, Err(BusError::Conflict { expected, .. }) if expected == r1),
        "{stale:?}"
    );
    assert_eq!(
        bus.state_get("main").await.unwrap().unwrap().value,
        "updated y\n"
    );
    // A blind put in between makes the next update conflict.
    bus.state_put("main", "blind").await.unwrap();
    assert!(matches!(
        bus.state_update("main", "late", r2).await,
        Err(BusError::Conflict { .. })
    ));
    // Delete under a condition; then the key can be created again.
    let cur = bus.state_get("main").await.unwrap().unwrap().revision;
    assert!(matches!(
        bus.state_update("main", "", r1).await,
        Err(BusError::Conflict { .. })
    ));
    assert_eq!(bus.state_update("main", "", cur).await.unwrap(), 0);
    assert_eq!(bus.state_get("main").await.unwrap(), None);
    let r = bus.state_update("main", "fresh", 0).await.unwrap();
    assert_eq!(bus.state_get("main").await.unwrap().unwrap().revision, r);
}

#[tokio::test]
async fn concurrent_appends_keep_every_line() {
    let Some(hub) = Hub::start() else { return };
    hub.ready().await;
    let mut buses = Vec::new();
    for _ in 0..4 {
        buses.push(std::sync::Arc::new(hub.bus().await));
    }
    let mut tasks = Vec::new();
    for i in 0..20 {
        let bus = buses[i % buses.len()].clone();
        tasks.push(tokio::spawn(async move {
            bus.append_line(
                "landed.log",
                &format!("20261127T0800{i:02}Z hgl-compiler #{i} abc\n"),
            )
            .await
            .unwrap()
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let value = buses[0]
        .state_get("landed.log")
        .await
        .unwrap()
        .unwrap()
        .value;
    let lines: Vec<&str> = value.lines().collect();
    assert_eq!(lines.len(), 20, "{value}");
    for i in 0..20 {
        let want = format!("20261127T0800{i:02}Z hgl-compiler #{i} abc");
        assert_eq!(lines.iter().filter(|l| **l == want).count(), 1, "{want}");
    }
    assert!(value.ends_with('\n'));
}

#[tokio::test]
async fn oversized_notes_are_refused_and_terminated() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    let big = note("w1", "claude", &"x".repeat(hgl_ai_bus::MAX_NOTE_BYTES));
    let err = bus.send(&big).await.unwrap_err().to_string();
    assert!(err.contains("limit"), "{err}");
    assert!(
        bus.state_put("prs", &"x".repeat(hgl_ai_bus::MAX_VALUE_BYTES + 1))
            .await
            .is_err()
    );

    let info = hub
        .raw()
        .await
        .get_stream("HGL_NOTES")
        .await
        .unwrap()
        .cached_info()
        .config
        .clone();
    assert!(
        info.max_message_size > hgl_ai_bus::MAX_NOTE_BYTES as i32,
        "the hub bounds messages too"
    );
}

#[tokio::test]
async fn bridge_host_names_the_serving_host() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    assert_eq!(bus.bridge_host(&seat("w3")).await.unwrap(), None);
    let host = HostRecord {
        host: "build-1".into(),
        seats: vec![SeatRecord {
            seat: seat("w3"),
            role: Role::Implementor,
            runtime: Runtime::Codex,
            family: Family::Gpt,
            model: "gpt-6.1-sol".into(),
        }],
        memcap_gb: 64,
        work_root_free_gb: 812,
        updated_at: "20261127T083000Z".into(),
    };
    bus.host_put(&host).await.unwrap();
    assert_eq!(
        bus.bridge_host(&seat("w3")).await.unwrap().as_deref(),
        Some("build-1")
    );
    assert_eq!(bus.bridge_host(&seat("claude")).await.unwrap(), None);
}
