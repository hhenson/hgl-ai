//! Live tests against a real `nats-server -js` on a free loopback port with a temporary
//! store; GitHub is scripted. Skipped, with the reason printed, when `nats-server` is not on
//! `PATH`.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use hgl_ai_bus::{Bus, ProvisionOptions};
use hgl_ai_proto::{Kind, Seat};

use crate::backlog::Backlog;
use crate::coord::{self, CoordOpts};
use crate::exec::fake::FakeGh;
use crate::lock::LandLock;
use crate::table::Table;
use crate::testutil::{fixture, settings, without_receipts};
use crate::{Error, land, state, store};

struct Hub {
    child: Child,
    _dir: tempfile::TempDir,
    url: String,
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn on_path(name: &str) -> Option<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn hub() -> Option<Hub> {
    let Some(bin) = on_path("nats-server") else {
        eprintln!("skipped: nats-server is not on PATH");
        return None;
    };
    let dir = tempfile::tempdir().unwrap();
    let child = Command::new(bin)
        .args(["-js", "-a", "127.0.0.1", "-p", "-1"])
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
    let mut h = Hub {
        child,
        _dir: dir,
        url: String::new(),
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    h.url = loop {
        let found = std::fs::read_to_string(&ports)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["nats"][0].as_str().map(String::from));
        if let Some(u) = found {
            break u;
        }
        assert!(
            Instant::now() < deadline,
            "nats-server did not report its port"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    Some(h)
}

async fn bus(h: &Hub) -> Bus {
    let b = Bus::connect_url(&h.url, None).await.unwrap();
    b.provision().await.unwrap();
    b
}

#[tokio::test]
async fn compare_and_set_writers() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let r1 = store::put_latest(&b, "hold", "2100-01-01T00:00:00Z")
        .await
        .unwrap();
    let r2 = store::put_latest(&b, "hold", "2100-01-01T01:00:00Z")
        .await
        .unwrap();
    assert!(r2 > r1);
    assert!(
        !store::clear_if(&b, "hold", "2100-01-01T00:00:00Z")
            .await
            .unwrap(),
        "renewed by another writer"
    );
    assert!(
        store::clear_if(&b, "hold", "2100-01-01T01:00:00Z")
            .await
            .unwrap()
    );
    assert!(b.state_get("hold").await.unwrap().is_none());
    for i in 0..5 {
        store::append_bounded(&b, "landed.log", &format!("line {i}"), 3)
            .await
            .unwrap();
    }
    assert_eq!(
        b.state_get("landed.log").await.unwrap().unwrap().value,
        "line 2\nline 3\nline 4\n"
    );
}

#[tokio::test]
async fn a_round_writes_the_table_alerts_and_hold() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let mut s = settings();
    s.repos = ["alpha", "beta", "gamma"].map(String::from).to_vec();
    store::put_latest(&b, state::KEY_PRS, &fixture("coord-prev.prs"))
        .await
        .unwrap();
    let controller = Seat::new("controller").unwrap();
    let mut inbox = b.inbox(&controller).await.unwrap();

    let gh = FakeGh::default()
        .on("auth status", "")
        .on("query Backlog", &fixture("backlog-batch.json"))
        .on("graphql", &fixture("coord-batch.json"));
    let o = CoordOpts {
        once: true,
        dry_run: false,
        interval: coord::MIN_INTERVAL,
    };
    coord::run(&gh, &s, Some(&b), &o, &mut Vec::new())
        .await
        .unwrap();
    let t = Table::parse(&b.state_get(state::KEY_PRS).await.unwrap().unwrap().value);
    assert_eq!(t.rows.len(), 6);
    assert_eq!(t.rate.as_ref().map(|r| r.remaining), Some(4945));
    assert!(b.state_get(state::KEY_HOLD).await.unwrap().is_none());
    let got = inbox.fetch(10, Duration::from_secs(5)).await.unwrap();
    let firsts: Vec<String> = got
        .iter()
        .map(|d| d.note.body.lines().next().unwrap_or("").to_string())
        .collect();
    assert_eq!(got.len(), 2, "{firsts:?}");
    assert!(
        got.iter()
            .all(|d| d.note.kind == Kind::Note && d.note.sender.as_str() == "coord")
    );
    assert!(
        firsts[0].starts_with("[coord] Alert: alpha#11"),
        "{firsts:?}"
    );
    for d in &got {
        d.ack().await.unwrap();
    }

    // The allowance runs low: the round writes `hold`; the backlog round and the next round
    // do not run.
    let low = fixture("coord-batch.json")
        .replace("\"remaining\": 4945", "\"remaining\": 40")
        .replace("2026-01-05T12:55:49Z", "2100-01-01T00:00:00Z");
    let gh = FakeGh::default()
        .on("auth status", "")
        .on("query Backlog", &fixture("backlog-batch.json"))
        .on("graphql", &low);
    coord::run(&gh, &s, Some(&b), &o, &mut Vec::new())
        .await
        .unwrap();
    assert_eq!(
        b.state_get(state::KEY_HOLD).await.unwrap().unwrap().value,
        "2100-01-01T00:00:00Z"
    );
    assert!(!gh.calls().iter().any(|c| c.contains("query Backlog")));
    assert!(
        b.state_get(state::KEY_BACKLOG).await.unwrap().is_some(),
        "from the first run"
    );
    let gh = FakeGh::default()
        .on("auth status", "")
        .on("query Backlog", &fixture("backlog-batch.json"))
        .on("graphql", &low);
    coord::run(&gh, &s, Some(&b), &o, &mut Vec::new())
        .await
        .unwrap();
    assert!(
        !gh.calls().iter().any(|c| c.contains("graphql")),
        "no round under a hold"
    );
}

/// A backlog round writes `backlog` and one `HGL_METRICS` sample per repository for the day;
/// the next round rewrites the same keys.
#[tokio::test]
async fn a_backlog_round_writes_the_queue_and_the_day() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let mut s = settings();
    s.repos = ["alpha", "beta", "gamma"].map(String::from).to_vec();
    let o = CoordOpts {
        once: true,
        dry_run: false,
        interval: coord::MIN_INTERVAL,
    };
    let both = || {
        FakeGh::default()
            .on("auth status", "")
            .on("query Backlog", &fixture("backlog-batch.json"))
            .on("graphql", &fixture("coord-batch.json"))
    };
    coord::run(&both(), &s, Some(&b), &o, &mut Vec::new())
        .await
        .unwrap();
    let text = b
        .state_get(state::KEY_BACKLOG)
        .await
        .unwrap()
        .unwrap()
        .value;
    let got = Backlog::parse(&text);
    let want = Backlog::parse(&fixture("backlog-batch.backlog"));
    assert_eq!(got.queue, want.queue);
    assert_eq!((&got.open, &got.labels), (&want.open, &want.labels));
    let day = chrono::Utc::now().format("%Y%m%d").to_string();
    let m = b.metrics().await.unwrap();
    let keys: Vec<String> = m.iter().map(|m| m.key()).collect();
    assert_eq!(
        keys,
        ["alpha", "beta", "gamma"].map(|r| format!("{r}.{day}"))
    );
    assert_eq!((m[0].open, m[0].opened, m[0].closed), (9, 2, 1));
    assert_eq!(m[0].labels.get("good first issue"), Some(&2));
    assert_eq!(m[0].taken_at, got.read);

    // The next round rewrites the day's samples.
    let more = fixture("backlog-batch.json").replace("\"totalCount\": 9", "\"totalCount\": 10");
    let gh = FakeGh::default()
        .on("auth status", "")
        .on("query Backlog", &more)
        .on("graphql", &fixture("coord-batch.json"));
    coord::run(&gh, &s, Some(&b), &o, &mut Vec::new())
        .await
        .unwrap();
    let m = b.metrics().await.unwrap();
    assert_eq!(m.len(), 3);
    assert_eq!(m[0].open, 10);
    let text = b
        .state_get(state::KEY_BACKLOG)
        .await
        .unwrap()
        .unwrap()
        .value;
    assert!(text.contains("# open alpha 10\n"), "{text}");

    // Under a hold neither round runs.
    store::put_latest(&b, state::KEY_HOLD, "2100-01-01T00:00:00Z")
        .await
        .unwrap();
    let gh = both();
    coord::run(&gh, &s, Some(&b), &o, &mut Vec::new())
        .await
        .unwrap();
    assert!(!gh.calls().iter().any(|c| c.contains("graphql")));
}

#[tokio::test]
async fn a_landing_is_recorded() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let mut s = settings();
    s.repos = vec!["alpha".into()];
    let sub = "{\"type\":\"submodule\",\"sha\":\"5555555555555555555555555555555555555555\"}";
    let gh = FakeGh::default()
        .on(
            "git/ref/heads/main",
            "{\"object\":{\"sha\":\"cccccccccccccccccccccccccccccccccccccccc\"}}",
        )
        .on("contents/", sub);
    let merge = "01e99ea5e46457bac035ca8a2699b6b6575b32ed";
    land::record(&gh, &s, &b, "alpha", 7, merge, None)
        .await
        .unwrap();
    land::record(&gh, &s, &b, "alpha", 8, merge, None)
        .await
        .unwrap();
    let log = b.state_get(state::KEY_LANDED).await.unwrap().unwrap().value;
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[1].ends_with(&format!(" alpha #8 {merge}")), "{log}");
    let main = b.state_get(state::KEY_MAIN).await.unwrap().unwrap().value;
    assert!(main.starts_with("updated "), "{main}");
    assert!(main.contains("alpha main cccccccccccccccccccccccccccccccccccccccc\n"));
    assert!(main.contains("hgl-project pins hgl_spec 5555555555555555555555555555555555555555\n"));
}

const HEAD: &str = "1f461fa77bba922fe81383df4c475feb58b7c233";

fn locked(r: Result<LandLock, Error>) -> String {
    match r {
        Err(Error::Locked(m)) => m,
        Err(e) => panic!("not Locked: {e}"),
        Ok(l) => panic!("acquired {}", l.name()),
    }
}

/// The landing lock is the hub's, so two connections (two hosts) exclude each other; it is
/// released after a landing however the landing ends.
#[tokio::test]
async fn the_landing_lock_excludes_across_connections() {
    let Some(h) = hub() else { return };
    let (a, b) = (bus(&h).await, bus(&h).await);
    let one = LandLock::acquire(&a, "alpha", "build-1 pid 1 (land alpha#7)", Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(one.name(), "lock.land.alpha");
    let m = locked(LandLock::acquire(&b, "alpha", "build-2", Duration::ZERO).await);
    assert!(
        m.contains("lock.land.alpha is held by build-1 pid 1 (land alpha#7)"),
        "{m}"
    );
    // Another repository's lock is free.
    LandLock::acquire(&b, "beta", "build-2", Duration::ZERO)
        .await
        .unwrap()
        .release()
        .await;
    // A waiter gets it once the holder releases.
    let waiter = tokio::spawn(async move {
        let l = LandLock::acquire(&b, "alpha", "build-2", Duration::from_secs(20)).await;
        (l.map(|l| l.name().to_string()), b)
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    one.release().await;
    let (got, b) = waiter.await.unwrap();
    assert_eq!(got.unwrap(), "lock.land.alpha");

    // The waiter's guard was dropped without `release`: the lease still goes.
    let end = Instant::now() + Duration::from_secs(10);
    let again = loop {
        match LandLock::acquire(&a, "alpha", "build-1", Duration::ZERO).await {
            Ok(l) => break l,
            Err(Error::Locked(_)) if Instant::now() < end => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(e) => panic!("{e}"),
        }
    };

    // `merge-if-clean` is refused before any GitHub call while another landing holds it.
    let s = settings();
    let gh = FakeGh::default();
    let o = land::LandOpts::default();
    let r = land::merge_once(
        &gh,
        &s,
        &b,
        "alpha",
        7,
        HEAD,
        &land::LandOpts {
            lock_wait: Duration::ZERO,
            ..o.clone()
        },
    )
    .await;
    assert!(matches!(r, Err(Error::Locked(_))), "{:?}", r.err());
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
    again.release().await;

    // A landing that fails part way releases the lock before it returns.
    let r = land::merge_once(&gh, &s, &b, "alpha", 7, HEAD, &o).await;
    assert!(matches!(r, Err(Error::Command { .. })), "{:?}", r.err());
    assert!(!gh.calls().is_empty());
    LandLock::acquire(&a, "alpha", "build-1", Duration::ZERO)
        .await
        .expect("released after the failed landing")
        .release()
        .await;
}

/// A held lock is refreshed in the background, so it outlives the bucket's `max_age`.
#[tokio::test]
async fn a_held_landing_lock_is_refreshed() {
    let Some(h) = hub() else { return };
    let a = Bus::connect_url(&h.url, None).await.unwrap();
    a.provision_with(ProvisionOptions {
        locks_max_age: Duration::from_secs(1),
    })
    .await
    .unwrap();
    let b = Bus::connect_url(&h.url, None).await.unwrap();
    let held = LandLock::acquire_every(
        &a,
        "lock.land.alpha",
        "build-1",
        Duration::ZERO,
        Duration::from_millis(200),
    )
    .await
    .unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        locked(LandLock::acquire(&b, "alpha", "build-2", Duration::ZERO).await);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    held.release().await;
    LandLock::acquire(&b, "alpha", "build-2", Duration::ZERO)
        .await
        .unwrap()
        .release()
        .await;
}

/// A scripted GitHub under which the gate merges `alpha#7` at `HEAD`.
fn mergeable() -> FakeGh {
    FakeGh::default()
        .on("pulls/7/merge", "{\"merged\":true}")
        .on("pulls/7/files", &fixture("pr-files.json"))
        .on(
            "Accept: application/vnd.github.v3.diff",
            &fixture("pr.diff"),
        )
        .on("pulls/7", &fixture("pr-open.json"))
        .on("issues/7/comments", &fixture("comments-clean.json"))
        .on("compare/", "{\"ahead_by\":0,\"files\":[]}")
        .on(
            "git/ref/heads/main",
            "{\"object\":{\"sha\":\"0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a\"}}",
        )
        .on(
            "graphql",
            "{\"data\":{\"repository\":{\"pullRequest\":{\"reviewThreads\":{\"nodes\":[]}}}}}",
        )
}

/// A landing lock that expired and was taken by another holder stops the landing before
/// the merge, and after a merge before `landed.log` and `main` (exit 3 at the command line).
#[tokio::test]
async fn a_lost_landing_lock_stops_the_landing() {
    let Some(h) = hub() else { return };
    let a = Bus::connect_url(&h.url, None).await.unwrap();
    a.provision_with(ProvisionOptions {
        locks_max_age: Duration::from_secs(1),
    })
    .await
    .unwrap();
    let b = Bus::connect_url(&h.url, None).await.unwrap();
    // Never refreshed in the background: it expires after the bucket's max_age.
    let held = LandLock::acquire_every(
        &a,
        "lock.land.alpha",
        "build-1",
        Duration::ZERO,
        Duration::from_secs(3600),
    )
    .await
    .unwrap();
    held.confirm("the test").await.expect("still ours");
    let end = Instant::now() + Duration::from_secs(20);
    let other = loop {
        match LandLock::acquire(&b, "alpha", "build-2", Duration::ZERO).await {
            Ok(l) => break l,
            Err(Error::Locked(_)) if Instant::now() < end => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(e) => panic!("{e}"),
        }
    };
    let lost = |r: Result<(), Error>| match r {
        Err(Error::LeaseLost(m)) => m,
        Err(e) => panic!("not LeaseLost: {e}"),
        Ok(()) => panic!("confirmed a lost lease"),
    };
    let m = lost(held.confirm("merging alpha #7").await);
    assert_eq!(
        m,
        "the landing lock lock.land.alpha is no longer ours (it expired or another holder took it); stopped before merging alpha #7"
    );

    // The gate would merge; the lost lock stops it before the merge call.
    let s = without_receipts(settings(), &["alpha"]);
    let gh = mergeable();
    let o = land::LandOpts::default();
    let r = land::merge_locked(&gh, &s, &a, "alpha", 7, HEAD, &o, &held).await;
    assert!(
        matches!(&r, Err(Error::LeaseLost(m)) if m.ends_with("stopped before merging alpha #7")),
        "{:?}",
        r.err()
    );
    assert!(gh.calls().iter().any(|c| c.contains("pulls/7")));
    assert!(
        !gh.calls().iter().any(|c| c.contains("/merge")),
        "{:?}",
        gh.calls()
    );

    // Merged already: nothing is recorded under a lost lock.
    let gh = mergeable().on("contents/", "{}");
    let merged = crate::gate::Outcome {
        kind: crate::gate::Kind::Merged,
        line: "merged".into(),
        retry: false,
        merge_commit: Some("01e99ea5e46457bac035ca8a2699b6b6575b32ed".into()),
        head: HEAD.into(),
    };
    let r = land::after_merge(&gh, &s, &a, "alpha", 7, &merged, &o, &held, None).await;
    match r {
        Err(Error::LeaseLost(m)) => assert!(
            m.ends_with(
                "stopped before writing landed.log: alpha #7 is merged at 01e99ea5; landed.log, main and the landed notes are not written"
            ),
            "{m}"
        ),
        other => panic!("{:?}", other.err()),
    }
    assert!(a.state_get(state::KEY_LANDED).await.unwrap().is_none());
    assert!(a.state_get(state::KEY_MAIN).await.unwrap().is_none());
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
    held.release().await;
    // The release of a lost lock leaves the new holder's lease alone.
    locked(LandLock::acquire(&a, "alpha", "build-1", Duration::ZERO).await);
    other.release().await;
}

/// A hold that outlasts the landing's time stops it without running the gate (exit 3).
#[tokio::test]
async fn a_hold_at_the_deadline_stops_the_landing() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    store::put_latest(&b, state::KEY_HOLD, "2100-01-01T00:00:00Z")
        .await
        .unwrap();
    let s = settings();
    let gh = FakeGh::default();
    let o = land::LandOpts {
        timeout: Duration::from_millis(300),
        interval: Duration::from_millis(100),
        ..land::LandOpts::default()
    };
    let l = land::land(&gh, &s, &b, "alpha", 7, HEAD, &o).await.unwrap();
    assert_eq!(l.outcome.kind, crate::gate::Kind::Hold);
    assert!(!l.outcome.kind.satisfied());
    assert_eq!(
        l.outcome.line,
        "alpha #7: hold still active at the deadline (hold until 2100-01-01T00:00:00Z) HOLD"
    );
    assert!(l.after.is_empty());
    assert!(gh.calls().is_empty(), "no gate call: {:?}", gh.calls());
    // The lock was released.
    LandLock::acquire(&b, "alpha", "build-1", Duration::ZERO)
        .await
        .unwrap()
        .release()
        .await;
}

/// A JetStream API request over the plain NATS protocol (the hub here has no auth), for a
/// test to change the hub under the code it tests. Returns the reply's payload.
fn js_api(url: &str, subject: &str) -> String {
    use std::io::{BufRead, BufReader, Write};
    let addr = url.trim_start_matches("nats://");
    let stream = std::net::TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    assert!(line.starts_with("INFO"), "{line}");
    write!(
        w,
        "CONNECT {{\"verbose\":false,\"pedantic\":false}}\r\nSUB _INBOX.t 1\r\nPUB {subject} _INBOX.t 0\r\n\r\nPING\r\n"
    )
    .unwrap();
    loop {
        line.clear();
        r.read_line(&mut line).unwrap();
        if line.starts_with("MSG _INBOX.t") {
            line.clear();
            r.read_line(&mut line).unwrap();
            return line.trim().to_string();
        }
        assert!(!line.starts_with("-ERR"), "{line}");
    }
}

/// Carbon's check 8b (hgl-ai#8): a round whose alerts cannot be sent (the notes stream is
/// gone) keeps them on the hub, and the next run sends them, though the table it compares
/// with no longer shows the change.
#[tokio::test]
async fn an_alert_whose_send_failed_is_sent_by_the_next_round() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let mut s = settings();
    s.repos = ["alpha", "beta", "gamma"].map(String::from).to_vec();
    store::put_latest(&b, state::KEY_PRS, &fixture("coord-prev.prs"))
        .await
        .unwrap();
    // The run's backlog round too (merged with the owner console's coordinator).
    let gh = || {
        FakeGh::default()
            .on("auth status", "")
            .on("query Backlog", &fixture("backlog-batch.json"))
            .on("graphql", &fixture("coord-batch.json"))
    };
    let o = CoordOpts {
        once: true,
        dry_run: false,
        interval: coord::MIN_INTERVAL,
    };
    let reply = js_api(&h.url, "$JS.API.STREAM.DELETE.HGL_NOTES");
    assert!(reply.contains("\"success\":true"), "{reply}");
    let r = coord::run(&gh(), &s, Some(&b), &o, &mut Vec::new()).await;
    assert!(r.is_err(), "the sends failed");
    // The table moved on; the alerts wait on the hub.
    let t = Table::parse(&b.state_get(state::KEY_PRS).await.unwrap().unwrap().value);
    assert_eq!(t.rows.len(), 6);
    let kept = b.state_get(coord::KEY_UNSENT).await.unwrap().unwrap().value;
    let kept: Vec<hgl_ai_proto::Note> = serde_json::from_str(&kept).unwrap();
    assert_eq!(kept.len(), 2);

    // The stream is back; a new run (a restarted service) sends them.
    b.provision().await.unwrap();
    let controller = Seat::new("controller").unwrap();
    let mut inbox = b.inbox(&controller).await.unwrap();
    coord::run(&gh(), &s, Some(&b), &o, &mut Vec::new())
        .await
        .unwrap();
    let got = inbox.fetch(10, Duration::from_secs(5)).await.unwrap();
    let firsts: Vec<String> = got
        .iter()
        .map(|d| d.note.body.lines().next().unwrap_or("").to_string())
        .collect();
    assert_eq!(got.len(), 2, "{firsts:?}");
    assert!(
        firsts[0].starts_with("[coord] Alert: alpha#11"),
        "{firsts:?}"
    );
    assert!(b.state_get(coord::KEY_UNSENT).await.unwrap().is_none());
    for d in &got {
        d.ack().await.unwrap();
    }
    // Sent once: a third round sends nothing.
    coord::run(&gh(), &s, Some(&b), &o, &mut Vec::new())
        .await
        .unwrap();
    assert!(
        inbox
            .fetch(10, Duration::from_millis(300))
            .await
            .unwrap()
            .is_empty()
    );
}

const MERGE: &str = "01e99ea5e46457bac035ca8a2699b6b6575b32ed";

/// The hub as a landing of `alpha#7` (issue 980) finds it: the author's tree with its
/// family, and a clean review receipt from another family among the controller's notes
/// (with notes that are not one for it around it).
async fn authored_and_reviewed(b: &Bus) {
    let tree = hgl_ai_proto::TreeRecord {
        host: "build-1".into(),
        seat: Seat::new("w1").unwrap(),
        task: hgl_ai_proto::TaskKey {
            repo: "alpha".into(),
            issue: 980,
        },
        review: false,
        path: "/w/w1/alpha-980".into(),
        branch: Some("codex/980-format-patterns".into()),
        created_at: "20260101T000000Z".into(),
        size_bytes: 0,
        families: vec![hgl_ai_proto::Family::Gpt],
        class: None,
        class_seq: None,
        seq: None,
    };
    b.tree_put(&tree).await.unwrap();
    let controller = Seat::new("controller").unwrap();
    let note = |kind: Kind, from: &str, pr: u64, body: &str| {
        let mut n =
            hgl_ai_proto::Note::new(Seat::new(from).unwrap(), controller.clone(), kind, body);
        n.repo = Some("alpha".into());
        n.issue = pr;
        if kind.needs_head() {
            n.head = Some(HEAD.into());
        }
        n
    };
    let r = fixture("receipt.json");
    for n in [
        note(
            Kind::Receipt,
            "desk",
            7,
            "[desk] Sets: alpha#7 at 1f461fa7 on main 0a0a0a0a, exit 0: x",
        ),
        note(Kind::Receipt, "r1", 8, &r.replace("\"pr\": 7", "\"pr\": 8")),
        note(Kind::Ready, "w1", 7, "[codex] Ready: alpha#7"),
        note(
            Kind::Receipt,
            "r1",
            7,
            &format!("[claude] Receipt:\n```json\n{r}```\n"),
        ),
    ] {
        b.send(&n).await.unwrap();
    }
    let seen = crate::gate::receipts(b, &settings(), "alpha", 7)
        .await
        .unwrap();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].sender, "r1");
}

/// Everything the gate and the steps after the merge read, for `alpha#7` at `HEAD`; the
/// pull request reads `open` once (the gate), then `merged`. `after` goes first, so it can
/// fail a call once.
fn landing_gh(after: impl FnOnce(FakeGh) -> FakeGh) -> FakeGh {
    let main = "{\"object\":{\"sha\":\"0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a\"}}";
    let gh = FakeGh::default()
        .on("pulls/7/merge", "{\"merged\":true}")
        .on("pulls/7/files", &fixture("pr-files.json"))
        .on(
            "Accept: application/vnd.github.v3.diff",
            &fixture("pr.diff"),
        )
        .on("issues/7/comments", &fixture("comments-clean.json"))
        .on("compare/", "{\"ahead_by\":0,\"files\":[]}")
        .on(
            "graphql",
            "{\"data\":{\"repository\":{\"pullRequest\":{\"reviewThreads\":{\"nodes\":[]}}}}}",
        )
        .once("pulls/7", &fixture("pr-open.json"));
    after(gh)
        .once("git/ref/heads/main", main)
        .on("pulls/7", &fixture("pr-merged.json"))
        .on("git/ref/heads/main", main)
}

async fn landed_notes(b: &Bus) -> Vec<(String, u64)> {
    let all = b
        .read_notes(
            &[Seat::new("w1").unwrap(), Seat::broadcast()],
            1,
            100,
            Duration::ZERO,
        )
        .await
        .unwrap();
    all.into_iter()
        .filter(|n| n.note.kind == Kind::Landed)
        .map(|n| (n.note.recipient.to_string(), n.note.issue))
        .collect()
}

fn posts(gh: &FakeGh, what: &str) -> usize {
    gh.calls()
        .iter()
        .filter(|c| c.contains(what) && (c.contains("-X POST") || c.contains("-X PUT")))
        .count()
}

/// Carbon's check 4 (hgl-ai#7): the read right after the merge fails (one HTTP 502). The
/// retry finds the pull request merged and finishes its landing (`landed.log`, `main`, the
/// notes, the comment) instead of reporting it merged with nothing recorded; a third run
/// finds it landed and does nothing.
#[tokio::test]
async fn a_landing_cut_off_after_the_merge_is_finished_by_its_retry() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    authored_and_reviewed(&b).await;
    let mut s = settings();
    s.repos = vec!["alpha".into()];
    s.pins = vec![];
    let o = land::LandOpts::default();
    let gh = landing_gh(|g| g.push_fail_once("pulls/7", "gh: Server Error (HTTP 502)"));
    let e = land::land(&gh, &s, &b, "alpha", 7, HEAD, &o)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("HTTP 502"), "{e}");
    assert_eq!(posts(&gh, "pulls/7/merge"), 1, "merged");
    let rec = crate::progress::read(&b, "alpha", 7)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (rec.head.as_str(), rec.merge.as_deref()),
        (HEAD, None),
        "the intent"
    );
    assert!(b.state_get(state::KEY_MAIN).await.unwrap().is_none());

    let l = land::land(&gh, &s, &b, "alpha", 7, HEAD, &o).await.unwrap();
    assert_eq!(
        l.outcome.kind,
        crate::gate::Kind::Resumed,
        "{}",
        l.outcome.line
    );
    assert!(l.outcome.kind.satisfied());
    assert_eq!(posts(&gh, "pulls/7/merge"), 1, "not merged again");
    let log = b.state_get(state::KEY_LANDED).await.unwrap().unwrap().value;
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(
        log.trim_end().ends_with(&format!("alpha #7 {MERGE}")),
        "{log}"
    );
    let main = b.state_get(state::KEY_MAIN).await.unwrap().unwrap().value;
    assert!(main.contains("alpha main 0a0a0a0a"), "{main}");
    assert_eq!(
        landed_notes(&b).await,
        [
            ("w1".to_string(), 980),
            ("all".into(), 980),
            ("all".into(), 7)
        ]
    );
    assert_eq!(posts(&gh, "issues/7/comments"), 1, "{:?}", l.after);
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none(),
        "done"
    );

    let again = land::land(&gh, &s, &b, "alpha", 7, HEAD, &o).await.unwrap();
    assert_eq!(again.outcome.kind, crate::gate::Kind::AlreadyMerged);
    assert!(
        again.after[0].starts_with("landed earlier"),
        "{:?}",
        again.after
    );
    assert_eq!(posts(&gh, "issues/7/comments"), 1, "no second comment");
    assert_eq!(landed_notes(&b).await.len(), 3);
}

/// `main` cannot be read after the merge (HTTP 502, three times): the landing fails rather
/// than write a `main` without the repository; the retry finishes from where it stopped,
/// without a second `landed.log` line.
#[tokio::test]
async fn a_landing_that_could_not_read_main_resumes_there() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    authored_and_reviewed(&b).await;
    let mut s = settings();
    s.repos = vec!["alpha".into()];
    s.pins = vec![];
    let o = land::LandOpts::default();
    let main = "{\"object\":{\"sha\":\"0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a\"}}";
    let gh = landing_gh(|g| {
        // The gate's read of main answers; the three of the steps after the merge do not.
        g.once("git/ref/heads/main", main)
            .push_fail_once("git/ref/heads/main", "gh: Server Error (HTTP 502)")
            .push_fail_once("git/ref/heads/main", "gh: Server Error (HTTP 502)")
            .push_fail_once("git/ref/heads/main", "gh: Server Error (HTTP 502)")
    });
    let e = land::land(&gh, &s, &b, "alpha", 7, HEAD, &o)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("HTTP 502"), "{e}");
    assert!(
        b.state_get(state::KEY_MAIN).await.unwrap().is_none(),
        "no partial main"
    );
    let rec = crate::progress::read(&b, "alpha", 7)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rec.merge.as_deref(), Some(MERGE));
    assert_eq!(rec.done, [crate::progress::LANDED_LOG]);
    assert!(landed_notes(&b).await.is_empty());

    let l = land::land(&gh, &s, &b, "alpha", 7, HEAD, &o).await.unwrap();
    assert_eq!(
        l.outcome.kind,
        crate::gate::Kind::Resumed,
        "{}",
        l.outcome.line
    );
    let log = b.state_get(state::KEY_LANDED).await.unwrap().unwrap().value;
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(b.state_get(state::KEY_MAIN).await.unwrap().is_some());
    assert_eq!(landed_notes(&b).await.len(), 3);
    assert_eq!(posts(&gh, "issues/7/comments"), 1);
    assert_eq!(posts(&gh, "pulls/7/merge"), 1);
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
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
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// A commit on branch `branch` of the seed from `from`, writing `file`, pushed.
fn commit_on(seed: &std::path::Path, branch: &str, from: &str, file: &str, text: &str) -> String {
    git(seed, &["checkout", "--quiet", "-B", branch, from]);
    std::fs::write(seed.join(file), text).unwrap();
    git(seed, &["add", file]);
    git(
        seed,
        &["commit", "--quiet", "-m", &format!("{branch} {file}")],
    );
    git(seed, &["push", "--quiet", "--force", "origin", branch]);
    git(seed, &["rev-parse", "HEAD"])
}

/// Ask until the verdict is judged (pass, fail) or the gate build did not run.
async fn judged(
    b: &Bus,
    builds: &crate::builds::GateBuilds,
    name: &str,
    head: &str,
    main: &str,
) -> crate::builds::VerdictState {
    use crate::builds::{VerdictState, verdict_state};
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let st = verdict_state(Some(b), Some(builds), "demo", 7, name, head, main)
            .await
            .unwrap();
        match st {
            VerdictState::Building(_) => {}
            VerdictState::Missing(ref why) => panic!("nothing asked: {why:?}"),
            other => return other,
        }
        assert!(Instant::now() < deadline, "no verdict for {name}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// hgl-ai#6: the gate asks a builder for the local build verdict of the head merged onto
/// the `main` it saw; the builder writes `<head> <main> pass|fail ...`; a verdict for a
/// `main` that has moved does not pass the gate and is asked again; a build that fails or
/// does not merge says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate_builds_write_verdicts_and_a_moved_main_is_built_again() {
    use crate::builds::{GateBuilds, VerdictState, verdict_state};
    use hgl_ai_build::service::{self, Settings};
    use std::sync::Arc;
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let bare = root.join("origin.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "--quiet", "--bare", "-b", "main"]);
    git(&root, &["clone", "--quiet", bare.to_str().unwrap(), "seed"]);
    let seed = root.join("seed");
    git(&seed, &["checkout", "--quiet", "-b", "main"]);
    std::fs::write(seed.join("f.txt"), "one\n").unwrap();
    git(&seed, &["add", "f.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "c0"]);
    git(&seed, &["push", "--quiet", "origin", "main"]);
    let m1 = git(&seed, &["rev-parse", "HEAD"]);
    let head = commit_on(&seed, "feature", &m1, "g.txt", "g\n");
    let recipe = "repo = \"demo\"\n[[step]]\nname = \"check\"\nrun = \"test -f g.txt -o -f bad.txt && test ! -f bad.txt\"\nmemcap_gb = 1\ntime = \"2m\"\n";
    hgl_ai_build::store::recipe_put(&b, recipe, None)
        .await
        .unwrap();
    // A local builder of this host's platform.
    let platform = hgl_ai_proto::build::Platform::current().unwrap();
    let name = platform.as_str();
    let mut cfg = hgl_ai_config::Config::new("lab1");
    cfg.paths.work_root = root.join("work").display().to_string();
    cfg.paths.data_dir = root.join("data").display().to_string();
    let lanes = service::lanes(&cfg, std::path::PathBuf::from("ssh")).unwrap();
    let origin = bare.display().to_string();
    let settings = Settings {
        clone_url: Arc::new(move |_| origin.clone()),
        toolchains: vec!["git".into()],
        heartbeat: Duration::from_millis(300),
        verdicts: true,
    };
    let svc_bus = Arc::new(Bus::connect_url(&h.url, None).await.unwrap());
    let (stop, rx) = tokio::sync::watch::channel(false);
    let svc = tokio::spawn(service::serve(svc_bus, lanes, settings, rx));
    let deadline = Instant::now() + Duration::from_secs(30);
    while b.builders().await.unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the builder did not register");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let key = state::verdict_key("demo", 7, name);
    // Nothing there and nothing asked (a dry run): missing.
    assert_eq!(
        verdict_state(Some(&b), None, "demo", 7, name, &head, &m1)
            .await
            .unwrap(),
        VerdictState::Missing(None)
    );
    let builds = GateBuilds::new(Seat::new("controller").unwrap(), "controller");
    let first = judged(&b, &builds, name, &head, &m1).await;
    let VerdictState::Pass(v) = &first else {
        panic!("{first:?}")
    };
    assert!(v.starts_with(&format!("{head} {m1} pass ")), "{v}");
    assert!(v.ends_with(" lab1"), "{v}");
    let value = b.state_get(&key).await.unwrap().unwrap().value;
    let id = hgl_ai_proto::build::VerdictValue::parse(&value)
        .unwrap()
        .id
        .unwrap();
    let run = hgl_ai_build::store::run_get(&b, &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        run.base_sha.as_deref(),
        Some(m1.as_str()),
        "it built onto m1"
    );
    // Asked once: judged now, it is not asked again.
    assert_eq!(
        verdict_state(Some(&b), Some(&builds), "demo", 7, name, &head, &m1)
            .await
            .unwrap(),
        first
    );
    // `main` moves: the verdict for m1 no longer passes, and is asked again for m2.
    git(&seed, &["checkout", "--quiet", "main"]);
    std::fs::write(seed.join("h.txt"), "h\n").unwrap();
    git(&seed, &["add", "h.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "c1"]);
    git(&seed, &["push", "--quiet", "origin", "main"]);
    let m2 = git(&seed, &["rev-parse", "HEAD"]);
    assert_eq!(state::verdict_for(Some(&value), &head, &m2), None);
    match verdict_state(Some(&b), None, "demo", 7, name, &head, &m2)
        .await
        .unwrap()
    {
        VerdictState::Missing(Some(w)) => assert!(w.contains(&m1[..8]), "{w}"),
        other => panic!("a stale verdict counted: {other:?}"),
    }
    match verdict_state(Some(&b), Some(&builds), "demo", 7, name, &head, &m2)
        .await
        .unwrap()
    {
        VerdictState::Building(w) => assert!(w.starts_with("asked "), "{w}"),
        other => panic!("not asked again: {other:?}"),
    }
    let second = judged(&b, &builds, name, &head, &m2).await;
    let VerdictState::Pass(v2) = &second else {
        panic!("{second:?}")
    };
    assert!(v2.starts_with(&format!("{head} {m2} pass ")), "{v2}");
    let now = b.state_get(&key).await.unwrap().unwrap().value;
    assert_eq!(state::verdict_for(Some(&now), &head, &m1), None);
    assert_eq!(
        state::verdict_for(Some(&now), &head, &m2).map(|v| v.0),
        Some(true)
    );
    // A head whose build fails: fail, naming the step.
    let bad = commit_on(&seed, "feature", &m1, "bad.txt", "x\n");
    match judged(&b, &builds, name, &bad, &m2).await {
        VerdictState::Fail(v) => {
            assert!(v.starts_with(&format!("{bad} {m2} fail ")), "{v}");
            assert!(v.ends_with("step check failed"), "{v}");
        }
        other => panic!("{other:?}"),
    }
    // A head that does not merge onto main: not run, and the gate is told why.
    let clash = commit_on(&seed, "feature", &m1, "h.txt", "clash\n");
    match judged(&b, &builds, name, &clash, &m2).await {
        VerdictState::NotRun(why) => assert!(why.contains("does not merge"), "{why}"),
        other => panic!("{other:?}"),
    }
    let _ = stop.send(true);
    tokio::time::timeout(Duration::from_secs(60), svc)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
