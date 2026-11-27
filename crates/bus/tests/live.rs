//! Live tests against a real `nats-server -js` started per test on a free
//! loopback port with a token and a temporary store. Skipped, with the reason
//! printed, when `nats-server` is not on `PATH`.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use async_nats::jetstream;
use hgl_ai_bus::{Bus, BusError, InboxOptions, ProvisionOptions};
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
    let locks = kv("HGL_LOCKS").await;
    assert_eq!(
        (locks.max_messages_per_subject, locks.storage, locks.max_age),
        (
            1,
            jetstream::stream::StorageType::File,
            Duration::from_secs(30 * 60)
        )
    );
    let ended = kv("HGL_ENDED").await;
    assert_eq!(
        (ended.max_messages_per_subject, ended.storage, ended.max_age),
        (
            1,
            jetstream::stream::StorageType::File,
            Duration::from_secs(15 * 86_400)
        )
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

/// PROTO-9: one body from one sender in the same second, to two recipients and for two
/// tasks (what `gh land` sends), is four notes, not one and three duplicates.
#[tokio::test]
async fn same_body_to_other_recipients_or_tasks_is_not_a_duplicate() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    let mut w1 = bus.inbox(&seat("w1")).await.unwrap();
    let mut w2 = bus.inbox(&seat("w2")).await.unwrap();
    let landed = |to: &str, issue: u64| {
        let mut n = Note::new(
            seat("claude"),
            Seat::new(to).unwrap(),
            Kind::Landed,
            "[claude] Landed: hgl-compiler#981",
        );
        n.created_at = "20261127T041100Z".into();
        n.repo = Some("hgl-compiler".into());
        n.issue = issue;
        n.base = Some("01e99ea5e46457bac035ca8a2699b6b6575b32ed".into());
        n
    };
    let sends = [
        landed("w1", 980),
        landed("all", 980),
        landed("all", 981),
        landed("w2", 981),
    ];
    let mut seqs = Vec::new();
    for n in &sends {
        let r = bus.send(n).await.unwrap();
        assert!(!r.duplicate, "{} {}", n.recipient, n.issue);
        seqs.push(r.seq);
    }
    // A retry of the same note is still dropped.
    let again = bus.send(&sends[0]).await.unwrap();
    assert!(again.duplicate);
    assert_eq!(again.seq, seqs[0]);

    for (inbox, want) in [
        (&mut w1, vec![("w1", 980), ("all", 980), ("all", 981)]),
        (&mut w2, vec![("all", 980), ("all", 981), ("w2", 981)]),
    ] {
        let got = inbox.fetch(10, LONG).await.unwrap();
        let seen: Vec<(&str, u64)> = got
            .iter()
            .map(|d| (d.note.recipient.as_str(), d.note.issue))
            .collect();
        assert_eq!(seen, want);
        for d in &got {
            d.ack().await.unwrap();
        }
    }
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
        families: Vec::new(),
        class: None,
        class_seq: None,
        seq: None,
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

const LAND: &str = "lock.land.hgl-compiler";

#[tokio::test]
async fn one_of_two_concurrent_acquirers_holds_the_lease() {
    let Some(hub) = Hub::start() else { return };
    hub.ready().await;
    let (a, b) = (hub.bus().await, hub.bus().await);
    let (ra, rb) = tokio::join!(
        a.lock_acquire(LAND, "build-1 pid 1 (land #1)"),
        b.lock_acquire(LAND, "build-2 pid 2 (land #2)")
    );
    let (won, lost, loser) = match (ra, rb) {
        (Ok(l), Err(e)) => (l, e, "build-2"),
        (Err(e), Ok(l)) => (l, e, "build-1"),
        (Ok(_), Ok(_)) => panic!("both acquired {LAND}"),
        (Err(x), Err(y)) => panic!("neither acquired {LAND}: {x}; {y}"),
    };
    match &lost {
        BusError::Held {
            name,
            holder,
            since,
        } => {
            assert_eq!(name, LAND);
            assert_eq!(holder, &won.record().holder);
            assert!(!holder.starts_with(loser), "{holder}");
            assert_eq!(since, &won.record().acquired_at);
        }
        other => panic!("not Held: {other}"),
    }
    assert!(lost.to_string().contains("is held by"), "{lost}");
    // Other names are independent.
    a.lock_acquire("lock.land.hgl_spec", "x")
        .await
        .unwrap()
        .release()
        .await
        .unwrap();
    assert!(won.release().await.unwrap());
}

#[tokio::test]
async fn a_released_lease_can_be_taken_again() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    assert_eq!(bus.lock_get(LAND).await.unwrap(), None);
    let first = bus.lock_acquire(LAND, "first").await.unwrap();
    assert_eq!(
        bus.lock_get(LAND).await.unwrap().as_ref(),
        Some(first.record())
    );
    assert!(matches!(
        bus.lock_acquire(LAND, "second").await,
        Err(BusError::Held { .. })
    ));
    assert!(first.release().await.unwrap());
    assert_eq!(bus.lock_get(LAND).await.unwrap(), None);
    let second = bus.lock_acquire(LAND, "second").await.unwrap();
    assert_eq!(second.record().holder, "second");
    assert!(matches!(
        bus.lock_acquire(LAND, "third").await,
        Err(BusError::Held { holder, .. }) if holder == "second"
    ));
    // Taken over only from the holder named as dead, and only once.
    assert!(matches!(
        bus.lock_take_over(LAND, "third", "first").await,
        Err(BusError::Held { holder, .. }) if holder == "second"
    ));
    let (a, b) = tokio::join!(
        bus.lock_take_over(LAND, "third", "second"),
        bus.lock_take_over(LAND, "fourth", "second")
    );
    assert!(a.is_ok() != b.is_ok(), "one takes it over");
    let third = a.or(b).unwrap();
    assert!(
        !second.release().await.unwrap(),
        "the dead holder's lease is gone"
    );
    assert_eq!(
        bus.lock_get(LAND).await.unwrap().map(|r| r.holder),
        Some(third.record().holder.clone())
    );
    assert!(third.release().await.unwrap());
    let fresh = bus.lock_take_over(LAND, "fifth", "gone").await.unwrap();
    assert_eq!(
        fresh.record().holder,
        "fifth",
        "absent: taken as by lock_acquire"
    );
    assert!(fresh.release().await.unwrap());
    assert!(matches!(
        bus.lock_acquire("bad..name", "x").await,
        Err(BusError::Nats(_))
    ));
}

#[tokio::test]
async fn ended_tasks_are_shared_and_a_later_end_stands() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    let other = hub.bus().await;
    let t = TaskKey {
        repo: "hgl-compiler".into(),
        issue: 992,
    };
    assert_eq!(bus.ended_get(&seat("w1"), &t).await.unwrap(), None);
    assert_eq!(bus.ended_put(&seat("w1"), &t, 10).await.unwrap(), 10);
    // Another connection (another host) reads it.
    let r = other.ended_get(&seat("w1"), &t).await.unwrap().unwrap();
    assert_eq!(r.seq, 10);
    assert_eq!(r.ended_at.len(), "20261127T041100Z".len());
    assert_eq!(other.ended_put(&seat("w1"), &t, 8).await.unwrap(), 10);
    assert_eq!(
        bus.ended_get(&seat("w1"), &t).await.unwrap().unwrap().seq,
        10
    );
    assert_eq!(other.ended_put(&seat("w1"), &t, 12).await.unwrap(), 12);
    assert_eq!(
        bus.ended_get(&seat("w1"), &t).await.unwrap().unwrap().seq,
        12
    );
    // Per seat.
    assert_eq!(bus.ended_get(&seat("w2"), &t).await.unwrap(), None);
    // Concurrent ends: the later one stands.
    let u = TaskKey {
        repo: "hgl_spec".into(),
        issue: 3,
    };
    let w1 = seat("w1");
    let (a, b) = tokio::join!(bus.ended_put(&w1, &u, 20), other.ended_put(&w1, &u, 21));
    assert!(a.unwrap() >= 20 && b.unwrap() == 21);
    assert_eq!(
        bus.ended_get(&seat("w1"), &u).await.unwrap().unwrap().seq,
        21
    );
}

/// Wait until `f` holds, or fail after `secs`.
async fn within(secs: u64, what: &str, mut f: impl AsyncFnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !f().await {
        assert!(Instant::now() < end, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn a_refreshed_lease_outlives_max_age_and_an_abandoned_one_expires() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.bus().await;
    let max_age = Duration::from_secs(1);
    bus.provision_with(ProvisionOptions {
        locks_max_age: max_age,
    })
    .await
    .unwrap();
    let other = hub.bus().await;

    // Refreshed well within max_age, it is kept for several times max_age.
    let mut held = bus.lock_acquire(LAND, "build-1 pid 1").await.unwrap();
    let start = Instant::now();
    while start.elapsed() < max_age * 3 {
        tokio::time::sleep(max_age / 4).await;
        let before = held.revision();
        held.refresh().await.unwrap();
        assert!(held.revision() > before);
        assert!(
            matches!(
                other.lock_acquire(LAND, "build-2 pid 2").await,
                Err(BusError::Held { .. })
            ),
            "taken at {:?} despite refreshes",
            start.elapsed()
        );
    }

    // Abandoned (a dead holder), it expires and another host takes it.
    let mut taken = None;
    within(15, "the abandoned lease expires", async || {
        match other.lock_acquire(LAND, "build-2 pid 2").await {
            Ok(l) => {
                taken = Some(l);
                true
            }
            Err(BusError::Held { .. }) => false,
            Err(e) => panic!("{e}"),
        }
    })
    .await;
    let taken = taken.unwrap();
    assert_eq!(taken.record().holder, "build-2 pid 2");
    // The old holder can neither keep it nor delete the new holder's lease.
    assert!(matches!(
        held.refresh().await,
        Err(BusError::LeaseLost { .. })
    ));
    assert!(!held.release().await.unwrap());
    assert!(matches!(
        bus.lock_acquire(LAND, "build-1 pid 1").await,
        Err(BusError::Held { holder, .. }) if holder == "build-2 pid 2"
    ));
    assert!(taken.release().await.unwrap());
}

/// `peek` reads what the inbox has not acknowledged without a delivery, so any number of
/// peeks leave the note to the inbox (hgl-ai#5); naks never drop a note (max_deliver is
/// unlimited; the bridge dead-letters, PROTO-8).
#[tokio::test]
async fn peeks_spend_no_delivery_and_naks_never_drop() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    let claude = seat("claude");
    let first = bus.send(&note("w1", "claude", "one")).await.unwrap();
    bus.send(&note("w1", "w2", "not for claude")).await.unwrap();
    let all = bus
        .send(&Note::new(seat("w1"), Seat::broadcast(), Kind::Note, "all"))
        .await
        .unwrap();
    // No inbox yet: everything for the seat.
    for _ in 0..8 {
        let p = bus.peek(&claude, 10, SHORT).await.unwrap();
        let seqs: Vec<u64> = p.iter().map(|n| n.seq).collect();
        assert_eq!(seqs, [first.seq, all.seq]);
        assert_eq!(p[0].note.body.trim(), "one");
    }
    let mut inbox = bus.inbox(&claude).await.unwrap();
    let got = inbox.fetch(10, LONG).await.unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].delivered, 1, "peeks spent no delivery");
    got[0].ack().await.unwrap();
    // After the floor: only the unacknowledged one; `max` and an empty wait.
    let p = bus.peek(&claude, 10, SHORT).await.unwrap();
    assert_eq!(p.iter().map(|n| n.seq).collect::<Vec<_>>(), [all.seq]);
    assert_eq!(bus.peek(&claude, 0, SHORT).await.unwrap(), []);
    got[1].ack().await.unwrap();
    let t = Instant::now();
    assert!(bus.peek(&claude, 10, SHORT).await.unwrap().is_empty());
    assert!(
        t.elapsed() >= Duration::from_millis(250),
        "waited for a note"
    );
    // A whole subject's history from a sequence, with a cap.
    let r = bus
        .read_notes(&[seat("w2")], 1, 10, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].note.body.trim(), "not for claude");

    // Twenty naks, and the note is still there. An inbox made with the old limit of 5 is
    // brought to unlimited when it is opened.
    let n = bus.send(&note("claude", "w4", "retry me")).await.unwrap();
    let old = InboxOptions {
        max_deliver: 5,
        ..InboxOptions::default()
    };
    drop(bus.inbox_with(&seat("w4"), old).await.unwrap());
    let mut w4 = bus.inbox(&seat("w4")).await.unwrap();
    let info = hub
        .raw()
        .await
        .get_stream("HGL_NOTES")
        .await
        .unwrap()
        .consumer_info("inbox-w4")
        .await
        .unwrap();
    assert_eq!(info.config.max_deliver, -1);
    let b = w4.fetch(1, LONG).await.unwrap();
    assert_eq!(b[0].seq, all.seq, "the broadcast first");
    b[0].ack().await.unwrap();
    for i in 1..=20u64 {
        let d = w4.fetch(1, LONG).await.unwrap();
        assert_eq!((d[0].seq, d[0].delivered), (n.seq, i));
        d[0].nak(Duration::ZERO).await.unwrap();
    }
    let d = w4.fetch(1, LONG).await.unwrap();
    assert_eq!(d[0].delivered, 21);
    d[0].ack().await.unwrap();
}

/// A whole-history read goes on past a message it cannot read: each page names the last
/// sequence read, a skipped one included.
#[tokio::test]
async fn pages_go_on_past_a_malformed_message() {
    let Some(hub) = Hub::start() else { return };
    let bus = hub.ready().await;
    let js = hub.raw().await;
    let bad = js
        .publish("hgl.notes.claude", "not json".into())
        .await
        .unwrap()
        .await
        .unwrap();
    let good = bus.send(&note("w1", "claude", "good")).await.unwrap();
    let c = [seat("claude")];
    let (notes, last) = bus.read_page(&c, 1, 1).await.unwrap();
    assert!(notes.is_empty());
    assert_eq!(last, Some(bad.sequence));
    let (notes, last) = bus.read_page(&c, bad.sequence + 1, 1).await.unwrap();
    assert_eq!((notes.len(), last), (1, Some(good.seq)));
    let (notes, last) = bus.read_page(&c, good.seq + 1, 10).await.unwrap();
    assert_eq!((notes.len(), last), (0, None));
}
