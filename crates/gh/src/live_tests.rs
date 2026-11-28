//! Live tests against a real `nats-server -js` on a free loopback port with a temporary
//! store; GitHub is scripted. Skipped, with the reason printed, when `nats-server` is not on
//! `PATH`.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use hgl_ai_bus::{Bus, ProvisionOptions};
use hgl_ai_proto::{Kind, Note, Seat};

use crate::backlog::Backlog;
use crate::coord::{self, CoordOpts};
use crate::exec::fake::FakeGh;
use crate::lock::LandLock;
use crate::table::Table;
use crate::testutil::{empty_intake, fixture, intake_node, settings, without_receipts};
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
        .on("query Intake", &empty_intake(3).to_string())
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
        .on("query Intake", &empty_intake(3).to_string())
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
        .on("query Intake", &empty_intake(3).to_string())
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
            .on("query Intake", &empty_intake(3).to_string())
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
        .on("query Intake", &empty_intake(3).to_string())
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

/// `merge-async`'s answer for a merge done at once.
const MERGED_ASYNC: &str = "{\"status\":\"merged\",\"details\":{\"message\":\"merged\"}}";

/// A scripted GitHub under which the gate merges `alpha#7` at `HEAD`.
fn mergeable() -> FakeGh {
    FakeGh::default()
        .on("pulls/7/merge-async", MERGED_ASYNC)
        .on("pulls/7/files", &fixture("pr-files.json"))
        .on(
            "Accept: application/vnd.github.v3.diff",
            &fixture("pr.diff"),
        )
        .on("pulls/7", &fixture("pr-open.json"))
        .on("issues/7/comments", &fixture("comments-clean.json"))
        .on("compare/", "{\"ahead_by\":0,\"files\":[]}")
        .on("check-runs", "{\"check_runs\":[]}")
        .on("/status?per_page", "{\"statuses\":[]}")
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
        cause: None,
    };
    let mut p = crate::progress::Progress::new(HEAD, None);
    let mut lines = Vec::new();
    let r = land::after_merge(
        &gh, &s, &a, "alpha", 7, &merged, &o, &held, &mut p, &mut lines,
    )
    .await;
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
fn js_api(url: &str, subject: &str, payload: &str) -> String {
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
        "CONNECT {{\"verbose\":false,\"pedantic\":false}}\r\nSUB _INBOX.t 1\r\nPUB {subject} _INBOX.t {}\r\n{payload}\r\nPING\r\n",
        payload.len()
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

#[tokio::test]
async fn intake_suppresses_handled_items_and_only_announces_a_later_reopen() {
    use chrono::{TimeDelta, Utc};
    use hgl_ai_proto::{TaskKey, TreeRecord};
    use serde_json::json;

    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let mut s = settings();
    s.repos = ["alpha", "beta", "gamma"].map(String::from).to_vec();
    b.tree_put(&TreeRecord {
        host: "host".into(),
        seat: Seat::new("w1").unwrap(),
        task: TaskKey {
            repo: "beta".into(),
            issue: 30,
        },
        review: false,
        path: "work".into(),
        branch: Some("codex/q30-fleet".into()),
        created_at: hgl_ai_proto::utc_now_stamp(),
        size_bytes: 0,
        families: Vec::new(),
        class: None,
        class_seq: None,
        seq: None,
        handoff_seq: None,
    })
    .await
    .unwrap();
    let recent = Utc::now() - TimeDelta::minutes(1);
    let old = recent - TimeDelta::days(3);
    let mut handoff = hgl_ai_proto::Note::new(
        s.controller.clone(),
        Seat::new("w2").unwrap(),
        Kind::Assign,
        "Assigned before the bridge registered its tree",
    );
    handoff.repo = Some("alpha".into());
    handoff.issue = 26;
    b.send(&handoff).await.unwrap();
    let mut labelled = intake_node(22, recent, recent);
    labelled["labels"]["nodes"] = json!([{ "name": "codex-priority" }]);
    let mut assigned = intake_node(23, recent, recent);
    assigned["assignees"]["totalCount"] = json!(1);
    let mut external = intake_node(24, old, recent);
    external["timelineItems"]["nodes"] = json!([{ "id": "reopen-24", "createdAt": recent }]);
    external["labels"]["nodes"] = json!([{ "name": "spec-refine" }]);
    let mut routed = intake_node(25, recent, recent);
    routed["reviewRequests"] = json!({ "totalCount": 1 });
    let mut fleet = intake_node(31, recent, recent);
    fleet["headRefName"] = json!("codex/q30-fleet");
    let mut fork = intake_node(32, recent, recent);
    fork["headRefName"] = json!("codex/q30-fleet");
    fork["isCrossRepository"] = json!(true);

    let mut response = empty_intake(3);
    response["data"]["r0"]["items"]["nodes"] = json!([
        intake_node(21, recent, recent),
        labelled,
        assigned,
        intake_node(26, recent, recent)
    ]);
    response["data"]["r1"]["items"]["nodes"] = json!([external, routed]);
    response["data"]["r2"]["items"]["nodes"] = json!([intake_node(30, recent, recent)]);
    response["data"]["r3"]["items"]["nodes"] = json!([fleet, fork]);
    let fake = |response: &serde_json::Value| {
        FakeGh::default()
            .on("auth status", "")
            .on("query Backlog", &fixture("backlog-batch.json"))
            .on("query Intake", &response.to_string())
            .on("graphql", &fixture("coord-batch.json"))
    };
    let opts = CoordOpts {
        once: true,
        dry_run: false,
        interval: coord::MIN_INTERVAL,
    };
    let triage = Seat::new("triage").unwrap();
    let notes = || b.read_notes(std::slice::from_ref(&triage), 1, 20, Duration::ZERO);

    let state = || async {
        let entry = b.state_get(crate::intake::KEY).await.unwrap().unwrap();
        serde_json::from_str::<serde_json::Value>(&entry.value).unwrap()
    };
    coord::run(&fake(&response), &s, Some(&b), &opts, &mut Vec::new())
        .await
        .unwrap();
    let actual = notes().await.unwrap();
    assert_eq!(actual.len(), 1);
    let note = &actual[0].note;
    let body = note.body.as_str();
    assert_eq!(note.kind, Kind::Note);
    assert_eq!(note.sender, s.coord_seat);
    assert_eq!(body.lines().count(), 5, "{body}");
    assert!(body.contains("alpha#21 [new issue]"), "{body}");
    assert!(body.contains("alpha#24 [reopened PR] by contributor [spec-refine] [external, controller routes review to another model family]"), "{body}");
    assert!(body.contains("beta#31 [new PR] by contributor —"), "{body}");
    assert!(
        body.contains("beta#32 [new PR] by contributor [external"),
        "{body}"
    );
    assert!(state().await["pending"].is_null());

    // A restart with the same events is silent. A later reopen of an already
    // announced item gets its own notification, even within the scan overlap.
    coord::run(&fake(&response), &s, Some(&b), &opts, &mut Vec::new())
        .await
        .unwrap();
    assert_eq!(notes().await.unwrap().len(), 1);
    response["data"]["r0"]["items"]["nodes"][0]["timelineItems"]["nodes"] =
        json!([{ "id": "reopen-21", "createdAt": recent + TimeDelta::seconds(1) }]);
    coord::run(&fake(&response), &s, Some(&b), &opts, &mut Vec::new())
        .await
        .unwrap();
    let actual = notes().await.unwrap();
    assert_eq!(actual.len(), 2);
    assert_eq!(actual[1].note.body.lines().count(), 2);
    assert!(actual[1].note.body.contains("alpha#21 [reopened issue]"));
}

#[tokio::test]
async fn intake_retries_pending_delivery_and_checks_history_after_the_duplicate_window() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let triage = Seat::new("triage").unwrap();
    let mut note = hgl_ai_proto::Note::new(
        Seat::new("coord").unwrap(),
        triage.clone(),
        Kind::Note,
        "[coord] Intake (ROLE-3)\n- alpha#21",
    );
    let cursor = |note: &hgl_ai_proto::Note| serde_json::json!({ "through": null, "seen": {}, "pending": note });
    b.state_create(crate::intake::KEY, &cursor(&note).to_string())
        .await
        .unwrap();
    let reply = js_api(&h.url, "$JS.API.STREAM.DELETE.HGL_NOTES", "");
    assert!(reply.contains("\"success\":true"), "{reply}");
    let round = crate::intake::Round::load(Some(&b), chrono::Utc::now())
        .await
        .unwrap();
    assert!(round.from().is_none(), "retry before starting a new scan");
    assert!(round.retry(&b).await.is_err(), "the stream was deleted");
    b.provision().await.unwrap();
    let round = crate::intake::Round::load(Some(&b), chrono::Utc::now())
        .await
        .unwrap();
    round.retry(&b).await.unwrap();

    let entry = b.state_get(crate::intake::KEY).await.unwrap().unwrap();
    assert!(serde_json::from_str::<serde_json::Value>(&entry.value).unwrap()["pending"].is_null());
    note.created_at = "20260101T000000Z".into();
    b.state_update(
        crate::intake::KEY,
        &cursor(&note).to_string(),
        entry.revision,
    )
    .await
    .unwrap();
    // Publish with another ID so JetStream's current duplicate cache cannot
    // mask a duplicate send from recovery.
    b.send_with_id(&note, "published-before-restart")
        .await
        .unwrap();
    let round = crate::intake::Round::load(Some(&b), chrono::Utc::now())
        .await
        .unwrap();
    round.retry(&b).await.unwrap();
    let notes = b
        .read_notes(&[triage], 1, 20, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(notes.len(), 2);
    let entry = b.state_get(crate::intake::KEY).await.unwrap().unwrap();
    assert!(serde_json::from_str::<serde_json::Value>(&entry.value).unwrap()["pending"].is_null());
}

/// Alerts from consecutive failed rounds survive a restart, even after the table advances.
#[tokio::test]
async fn alerts_from_consecutive_failed_rounds_survive_a_restart() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let mut s = settings();
    s.repos = ["alpha", "beta", "gamma"].map(String::from).to_vec();
    store::put_latest(&b, state::KEY_PRS, &fixture("coord-prev.prs"))
        .await
        .unwrap();
    let prs = fixture("coord-batch.json");
    let gh = |prs: &str| {
        FakeGh::default()
            .on("auth status", "")
            .on("query Backlog", &fixture("backlog-batch.json"))
            .on("query Intake", &empty_intake(3).to_string())
            .on("graphql", prs)
    };
    let o = CoordOpts {
        once: true,
        dry_run: false,
        interval: coord::MIN_INTERVAL,
    };
    let reply = js_api(&h.url, "$JS.API.STREAM.DELETE.HGL_NOTES", "");
    assert!(reply.contains("\"success\":true"), "{reply}");
    let first = prs.replace("\"state\": \"FAILURE\"", "\"state\": \"SUCCESS\"");
    let mut ids = Vec::new();
    for (snapshot, expected) in [(&first, vec![11]), (&prs, vec![11, 12])] {
        let r = coord::run(&gh(snapshot), &s, Some(&b), &o, &mut Vec::new()).await;
        assert!(r.is_err(), "the sends failed");
        let t = Table::parse(&b.state_get(state::KEY_PRS).await.unwrap().unwrap().value);
        assert_eq!(t.rows.len(), 6);
        let kept = b.state_get(coord::KEY_UNSENT).await.unwrap().unwrap().value;
        let kept: Vec<hgl_ai_proto::Note> = serde_json::from_str(&kept).unwrap();
        assert_eq!(kept.iter().map(|n| n.issue).collect::<Vec<_>>(), expected);
        let stored_ids: Vec<_> = kept.iter().map(hgl_ai_proto::Note::msg_id).collect();
        assert!(stored_ids.starts_with(&ids), "the retry keeps the same ID");
        ids = stored_ids;
    }

    // The stream is back; a new run (a restarted service) sends them.
    b.provision().await.unwrap();
    let controller = Seat::new("controller").unwrap();
    let mut inbox = b.inbox(&controller).await.unwrap();
    let offline = FakeGh::default()
        .on("auth status", "")
        .fail("graphql", "gh: Server Error (HTTP 502)");
    assert!(
        coord::run(&offline, &s, Some(&b), &o, &mut Vec::new())
            .await
            .is_err()
    );
    let got = inbox.fetch(10, Duration::from_secs(5)).await.unwrap();
    let firsts: Vec<String> = got
        .iter()
        .map(|d| d.note.body.lines().next().unwrap_or("").to_string())
        .collect();
    assert_eq!(got.len(), 2, "{firsts:?}");
    assert_eq!(got.iter().map(|d| d.note.msg_id()).collect::<Vec<_>>(), ids);
    assert!(
        firsts[0].starts_with("[coord] Alert: alpha#11"),
        "{firsts:?}"
    );
    assert!(b.state_get(coord::KEY_UNSENT).await.unwrap().is_none());
    for d in &got {
        d.ack().await.unwrap();
    }
    // After recovery, an unchanged round sends nothing.
    coord::run(&gh(&prs), &s, Some(&b), &o, &mut Vec::new())
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

#[tokio::test]
async fn the_table_waits_for_a_durable_alert_queue() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let mut s = settings();
    s.repos = ["alpha", "beta", "gamma"].map(String::from).to_vec();
    let previous = fixture("coord-prev.prs");
    store::put_latest(&b, state::KEY_PRS, &previous)
        .await
        .unwrap();
    let gh = FakeGh::default()
        .on("auth status", "")
        .on("query Backlog", &fixture("backlog-batch.json"))
        .on("query Intake", &empty_intake(3).to_string())
        .on("graphql", &fixture("coord-batch.json"));
    let o = CoordOpts {
        once: true,
        dry_run: false,
        interval: coord::MIN_INTERVAL,
    };
    let controller = Seat::new("controller").unwrap();
    let mut inbox = b.inbox(&controller).await.unwrap();

    // Keep the table writable while the stream refuses the alert queue subject.
    let info = js_api(&h.url, "$JS.API.STREAM.INFO.KV_HGL_STATE", "");
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    let mut config = info["config"].clone();
    config["subjects"] = serde_json::json!(["$KV.HGL_STATE.prs", "$KV.HGL_STATE.hold"]);
    let reply = js_api(
        &h.url,
        "$JS.API.STREAM.UPDATE.KV_HGL_STATE",
        &config.to_string(),
    );
    assert!(!reply.contains("\"error\""), "{reply}");

    let r = coord::run(&gh, &s, Some(&b), &o, &mut Vec::new()).await;
    assert!(r.is_err(), "writing the alert queue should fail");
    assert_eq!(
        b.state_get(state::KEY_PRS).await.unwrap().unwrap().value,
        previous
    );
    assert!(
        inbox
            .fetch(10, Duration::from_millis(300))
            .await
            .unwrap()
            .is_empty()
    );

    let reply = js_api(
        &h.url,
        "$JS.API.STREAM.UPDATE.KV_HGL_STATE",
        &info["config"].to_string(),
    );
    assert!(!reply.contains("\"error\""), "{reply}");
    coord::run(&gh, &s, Some(&b), &o, &mut Vec::new())
        .await
        .unwrap();
    assert_eq!(
        inbox.fetch(10, Duration::from_secs(2)).await.unwrap().len(),
        2
    );
}

#[tokio::test]
async fn an_overflowing_alert_queue_drops_the_oldest_and_drains_during_hold() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let mut s = settings();
    s.repos = ["alpha", "beta", "gamma"].map(String::from).to_vec();
    let previous = fixture("coord-prev.prs");
    b.state_create(state::KEY_PRS, &previous).await.unwrap();
    let large = "\"é".repeat(130_000);
    let legacy: Vec<_> = (1..=2)
        .map(|issue| {
            let mut note = Note::new(
                s.coord_seat.clone(),
                s.controller.clone(),
                Kind::Note,
                &large,
            );
            note.issue = issue;
            note
        })
        .collect();
    let saved = serde_json::to_string(&legacy).unwrap();
    b.state_create(coord::KEY_UNSENT, &saved).await.unwrap();
    let url = "https://github.com/o/alpha/pull/12";
    let prs = fixture("coord-batch.json").replace(
        &format!("\"{url}\""),
        &format!("\"{url}/{}\"", "x".repeat(12_000)),
    );
    let gh = FakeGh::default().on("auth status", "").on("graphql", &prs);
    let opts = CoordOpts {
        once: true,
        dry_run: false,
        interval: coord::MIN_INTERVAL,
    };
    let reply = js_api(&h.url, "$JS.API.STREAM.DELETE.HGL_NOTES", "");
    assert!(reply.contains("\"success\":true"), "{reply}");

    assert!(
        coord::run(&gh, &s, Some(&b), &opts, &mut Vec::new())
            .await
            .is_err()
    );
    let current = b.state_get(state::KEY_PRS).await.unwrap().unwrap().value;
    assert_ne!(
        previous, current,
        "the bounded queue lets the table advance"
    );
    let entry = b.state_get(coord::KEY_UNSENT).await.unwrap().unwrap();
    assert!(entry.value.len() <= hgl_ai_bus::MAX_VALUE_BYTES);
    let kept: Vec<Note> = serde_json::from_str(&entry.value).unwrap();
    assert_eq!(
        kept.iter().map(|n| n.issue).collect::<Vec<_>>(),
        [2, 11, 12, 0]
    );
    assert!(kept[3].body.contains("dropped 1 oldest"));

    b.provision().await.unwrap();
    let mut inbox = b.inbox(&s.controller).await.unwrap();
    b.state_create(state::KEY_HOLD, "2100-01-01T00:00:00Z")
        .await
        .unwrap();
    let held = FakeGh::default().on("auth status", "");
    coord::run(&held, &s, Some(&b), &opts, &mut Vec::new())
        .await
        .unwrap();
    assert_eq!(held.calls().len(), 1, "hold suppressed all GitHub reads");
    let got = inbox.fetch(10, Duration::from_secs(2)).await.unwrap();
    assert_eq!(got.into_iter().map(|d| d.note).collect::<Vec<_>>(), kept);
    assert!(b.state_get(coord::KEY_UNSENT).await.unwrap().is_none());
}

#[tokio::test]
async fn an_invalid_alert_becomes_a_durable_dead_letter_after_five_failures() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let s = settings();
    let mut invalid = Note::new(
        s.coord_seat.clone(),
        s.controller.clone(),
        Kind::Note,
        "bad alert",
    );
    invalid.v = 0;
    invalid.repo = Some("alpha".to_string());
    invalid.issue = 11;
    let good = Note::new(
        s.coord_seat.clone(),
        s.controller.clone(),
        Kind::Note,
        "good alert",
    );
    b.state_create(
        coord::KEY_UNSENT,
        &serde_json::to_string(&[invalid, good.clone()]).unwrap(),
    )
    .await
    .unwrap();
    b.state_create(state::KEY_HOLD, "2100-01-01T00:00:00Z")
        .await
        .unwrap();
    let gh = FakeGh::default().on("auth status", "");
    let opts = CoordOpts {
        once: true,
        dry_run: false,
        interval: coord::MIN_INTERVAL,
    };
    let mut inbox = b.inbox(&s.controller).await.unwrap();
    for _ in 0..5 {
        let error = coord::run(&gh, &s, Some(&b), &opts, &mut Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Bus(hgl_ai_bus::BusError::Proto(_))));
    }
    let entry = b.state_get(coord::KEY_UNSENT).await.unwrap().unwrap();
    let queued: Vec<Note> = serde_json::from_str(&entry.value).unwrap();
    let [report] = queued.as_slice() else {
        panic!("expected just the dead letter")
    };
    assert_eq!(report.issue, 11);
    assert!(report.body.starts_with("[coord] Dead letter:"));
    assert!(report.body.contains("unsupported version 0"));
    assert!(report.body.contains("bad alert"));

    let signed_out = FakeGh::default().fail("auth status", "bad credentials");
    let error = coord::run(&signed_out, &s, Some(&b), &opts, &mut Vec::new())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Auth(_)));
    let got = inbox.fetch(10, Duration::from_secs(2)).await.unwrap();
    assert_eq!(
        got.into_iter().map(|d| d.note).collect::<Vec<_>>(),
        [good, report.clone()]
    );
    assert!(b.state_get(coord::KEY_UNSENT).await.unwrap().is_none());
}

#[tokio::test]
async fn alert_report_failures_drop_invalid_reports_and_retry_transient_failures() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let s = settings();
    let invalid = Note::new(
        s.coord_seat.clone(),
        Seat::owner(),
        Kind::Note,
        "[coord] Invalid report recipient",
    );
    let valid = Note::new(
        s.coord_seat.clone(),
        s.controller.clone(),
        Kind::Note,
        "[coord] Dead letter: pending report",
    );
    let report = |note: &Note| {
        let mut value = serde_json::to_value(note).unwrap();
        value["report"] = true.into();
        value
    };
    let saved = serde_json::json!([report(&invalid), report(&valid)]);
    b.state_create(coord::KEY_UNSENT, &saved.to_string())
        .await
        .unwrap();
    b.state_create(state::KEY_HOLD, "2100-01-01T00:00:00Z")
        .await
        .unwrap();
    let reply = js_api(&h.url, "$JS.API.STREAM.DELETE.HGL_NOTES", "");
    assert!(reply.contains("\"success\":true"), "{reply}");
    let gh = FakeGh::default().on("auth status", "");
    let opts = CoordOpts {
        once: true,
        dry_run: false,
        interval: coord::MIN_INTERVAL,
    };

    let result = coord::run(&gh, &s, Some(&b), &opts, &mut Vec::new()).await;
    assert!(matches!(
        result,
        Err(Error::Bus(hgl_ai_bus::BusError::Proto(_)))
    ));
    let kept = b.state_get(coord::KEY_UNSENT).await.unwrap().unwrap();
    let queued: Vec<Note> = serde_json::from_str(&kept.value).unwrap();
    assert_eq!(queued.as_slice(), std::slice::from_ref(&valid));

    let result = coord::run(&gh, &s, Some(&b), &opts, &mut Vec::new()).await;
    assert!(matches!(
        result,
        Err(Error::Bus(hgl_ai_bus::BusError::Nats(_)))
    ));
    let retried = b.state_get(coord::KEY_UNSENT).await.unwrap().unwrap();
    assert_eq!(retried.revision, kept.revision);

    b.provision().await.unwrap();
    let mut inbox = b.inbox(&s.controller).await.unwrap();
    coord::run(&gh, &s, Some(&b), &opts, &mut Vec::new())
        .await
        .unwrap();
    let got = inbox.fetch(10, Duration::from_secs(2)).await.unwrap();
    assert_eq!(got.into_iter().map(|d| d.note).collect::<Vec<_>>(), [valid]);
    assert!(b.state_get(coord::KEY_UNSENT).await.unwrap().is_none());
}

#[tokio::test]
async fn malformed_alert_state_is_replaced_before_advancing_the_table() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    b.state_create(coord::KEY_UNSENT, "{").await.unwrap();
    b.state_create(state::KEY_PRS, &fixture("coord-prev.prs"))
        .await
        .unwrap();
    let mut s = settings();
    s.repos = ["alpha", "beta", "gamma"].map(String::from).to_vec();
    let gh = FakeGh::default()
        .on("auth status", "")
        .on("query Backlog", &fixture("backlog-batch.json"))
        .on("query Intake", &empty_intake(3).to_string())
        .on("graphql", &fixture("coord-batch.json"));
    let opts = CoordOpts {
        once: true,
        dry_run: false,
        interval: coord::MIN_INTERVAL,
    };
    let mut inbox = b.inbox(&s.controller).await.unwrap();
    coord::run(&gh, &s, Some(&b), &opts, &mut Vec::new())
        .await
        .unwrap();
    assert_eq!(
        inbox.fetch(10, Duration::from_secs(2)).await.unwrap().len(),
        2
    );
    assert!(b.state_get(coord::KEY_UNSENT).await.unwrap().is_none());
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
        handoff_seq: None,
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
        .on("pulls/7/merge-async", MERGED_ASYNC)
        .on("pulls/7/files", &fixture("pr-files.json"))
        .on(
            "Accept: application/vnd.github.v3.diff",
            &fixture("pr.diff"),
        )
        .on("issues/7/comments", &fixture("comments-clean.json"))
        .on("compare/", "{\"ahead_by\":0,\"files\":[]}")
        .on("check-runs", "{\"check_runs\":[]}")
        .on("/status?per_page", "{\"statuses\":[]}")
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

fn reads(gh: &FakeGh, what: &str) -> usize {
    gh.calls()
        .iter()
        .filter(|c| c.contains(what) && !c.contains("-X POST") && !c.contains("-X PUT"))
        .count()
}

fn posts(gh: &FakeGh, what: &str) -> usize {
    gh.calls()
        .iter()
        .filter(|c| c.contains(what) && (c.contains("-X POST") || c.contains("-X PUT")))
        .count()
}

/// An accepted `merge-async` still pending at the deadline (#21): not refused, the landing
/// record is kept and the lock released; a later run that finds the pull request merged
/// finishes the landing.
#[tokio::test]
async fn a_merge_still_pending_keeps_its_record() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    authored_and_reviewed(&b).await;
    let mut s = settings();
    s.repos = vec!["alpha".into()];
    s.pins = vec![];
    let o = land::LandOpts {
        timeout: Duration::from_millis(50),
        ..land::LandOpts::default()
    };
    let pending =
        "{\"status\":\"pending\",\"details\":{\"uuid\":\"u-1\",\"merge_method\":\"merge\"}}";
    let gh = mergeable()
        .once("pulls/7/merge-async --input", pending)
        .on("merge-async/u-1", pending);
    // The rules above go first: `mergeable` answers the merge as done.
    gh.rules.lock().unwrap().rotate_right(2);
    let l = land::merge_once(&gh, &s, &b, "alpha", 7, HEAD, &o)
        .await
        .unwrap();
    assert_eq!(
        l.outcome.kind,
        crate::gate::Kind::Wait,
        "{}",
        l.outcome.line
    );
    assert!(
        l.outcome.line.contains("merge accepted, not done yet"),
        "{}",
        l.outcome.line
    );
    assert!(!l.outcome.line.contains("refused"), "{}", l.outcome.line);
    let rec = crate::progress::read(&b, "alpha", 7)
        .await
        .unwrap()
        .expect("the record is kept");
    assert_eq!((rec.head.as_str(), rec.merge.as_deref()), (HEAD, None));
    // The lock is free again.
    LandLock::acquire(&b, "alpha", "other", Duration::ZERO)
        .await
        .unwrap()
        .release()
        .await;
    // GitHub merged it meanwhile: the next run finishes the landing without merging again.
    let gh = landing_gh(|g| g);
    gh.rules
        .lock()
        .unwrap()
        .retain(|r| r.0 != "pulls/7" || !r.2);
    let l = land::merge_once(&gh, &s, &b, "alpha", 7, HEAD, &o)
        .await
        .unwrap();
    assert_eq!(
        l.outcome.kind,
        crate::gate::Kind::Resumed,
        "{}",
        l.outcome.line
    );
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 0);
    let log = b.state_get(state::KEY_LANDED).await.unwrap().unwrap().value;
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none()
    );
}

/// Carbon's check 4 (hgl-ai#7): the read right after the merge fails (one HTTP 502):
/// `PARTIAL`, naming every step after the merge as not done, not a bare error. The retry
/// finds the pull request merged and finishes its landing (`landed.log`, `main`, the notes,
/// the comment) instead of reporting it merged with nothing recorded; a third run finds it
/// landed and does nothing.
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
    let l = land::land(&gh, &s, &b, "alpha", 7, HEAD, &o).await.unwrap();
    assert_eq!(l.outcome.kind, crate::gate::Kind::Partial);
    assert!(!l.outcome.kind.satisfied(), "exit 3");
    assert!(
        l.outcome.line.starts_with(
            "alpha #7 is merged, but landed.log, main, the landed notes and the comment are not done ("
        ) && l.outcome.line.contains("HTTP 502")
            && l.outcome
                .line
                .ends_with("; run the same command again to finish PARTIAL"),
        "{}",
        l.outcome.line
    );
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 1, "merged");
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
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 1, "not merged again");
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

    // Asked with an older head (the gate updated the branch before the merge): the merged
    // head is reported, and the line ends in its status word (r6 on #133).
    let older = "e".repeat(40);
    let again = land::land(&gh, &s, &b, "alpha", 7, &older, &o)
        .await
        .unwrap();
    assert_eq!(again.outcome.kind, crate::gate::Kind::AlreadyMerged);
    assert_eq!(again.outcome.head, HEAD);
    assert_eq!(again.outcome.merge_commit.as_deref(), Some(MERGE));
    assert!(
        again.outcome.line.ends_with(" MERGED")
            && again
                .outcome
                .line
                .contains(&format!("at {}", crate::short(HEAD))),
        "{}",
        again.outcome.line
    );
    let again = land::merge_once(&gh, &s, &b, "alpha", 7, &older, &o)
        .await
        .unwrap();
    assert_eq!(again.outcome.kind, crate::gate::Kind::AlreadyMerged);
    assert_eq!(again.outcome.head, HEAD);
    assert!(
        again.after[0].starts_with("landed earlier"),
        "{:?}",
        again.after
    );
    assert_eq!(posts(&gh, "issues/7/comments"), 1, "no second comment");
    assert_eq!(landed_notes(&b).await.len(), 3);
}

/// `main` cannot be read after the merge (HTTP 502, three times): `PARTIAL` rather than a
/// `main` without the repository, naming the steps left; the retry finishes from where it
/// stopped, without a second `landed.log` line.
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
    let l = land::land(&gh, &s, &b, "alpha", 7, HEAD, &o).await.unwrap();
    assert_eq!(l.outcome.kind, crate::gate::Kind::Partial);
    assert_eq!(l.outcome.merge_commit.as_deref(), Some(MERGE));
    assert!(
        l.outcome.line.starts_with(&format!(
            "alpha #7 is merged at {}, but main, the landed notes and the comment are not done (",
            &MERGE[..8]
        )) && l.outcome.line.contains("HTTP 502"),
        "{}",
        l.outcome.line
    );
    assert!(l.after.is_empty(), "{:?}", l.after);
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
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 1);
}

/// hgl-ai#7: the comment cannot be posted after the merge (one HTTP 502) under
/// `merge-if-clean`: `PARTIAL`, naming only the comment, with the lines of the steps done;
/// the record keeps them done. The same command posts the comment, and sends no note again.
#[tokio::test]
async fn a_landing_whose_comment_failed_is_partial_and_finished_by_its_retry() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    authored_and_reviewed(&b).await;
    let mut s = settings();
    s.repos = vec!["alpha".into()];
    s.pins = vec![];
    let o = land::LandOpts::default();
    let gh = landing_gh(|g| {
        g.push_fail_once("issues/7/comments -f body", "gh: Server Error (HTTP 502)")
    });
    // Ahead of the rule that answers every comments call.
    {
        let mut rules = gh.rules.lock().unwrap();
        let i = rules
            .iter()
            .position(|r| r.0 == "issues/7/comments -f body")
            .unwrap();
        let r = rules.remove(i);
        rules.insert(0, r);
    }
    let l = land::merge_once(&gh, &s, &b, "alpha", 7, HEAD, &o)
        .await
        .unwrap();
    assert_eq!(l.outcome.kind, crate::gate::Kind::Partial);
    assert!(!l.outcome.kind.satisfied(), "exit 3");
    assert_eq!(l.outcome.merge_commit.as_deref(), Some(MERGE));
    assert!(
        l.outcome.line.starts_with(&format!(
            "alpha #7 is merged at {}, but the comment is not done (",
            &MERGE[..8]
        )) && l.outcome.line.contains("HTTP 502"),
        "{}",
        l.outcome.line
    );
    assert!(
        l.after
            .iter()
            .any(|a| a.starts_with("sent landed for alpha-980 to w1")),
        "{:?}",
        l.after
    );
    let rec = crate::progress::read(&b, "alpha", 7)
        .await
        .unwrap()
        .expect("the record is kept");
    assert_eq!(
        rec.done,
        [
            crate::progress::LANDED_LOG,
            crate::progress::MAIN,
            crate::progress::NOTES
        ]
    );
    assert_eq!(landed_notes(&b).await.len(), 3);

    let l = land::merge_once(&gh, &s, &b, "alpha", 7, HEAD, &o)
        .await
        .unwrap();
    assert_eq!(
        l.outcome.kind,
        crate::gate::Kind::Resumed,
        "{}",
        l.outcome.line
    );
    assert_eq!(l.after, ["commented on alpha#7"]);
    assert_eq!(
        posts(&gh, "issues/7/comments"),
        2,
        "the failed post and one more"
    );
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 1, "not merged again");
    assert_eq!(landed_notes(&b).await.len(), 3, "no note sent again");
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none()
    );
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

/// A hub, an origin with `main` at m1 and a pull request head off it, the recipe, and a
/// local builder `lab1` (unless `builder` is false).
struct GateRig {
    h: Hub,
    b: Bus,
    _tmp: tempfile::TempDir,
    seed: std::path::PathBuf,
    m1: String,
    head: String,
    name: &'static str,
    stop: Option<tokio::sync::watch::Sender<bool>>,
    svc: Option<
        tokio::task::JoinHandle<std::result::Result<(), hgl_ai_build::service::ServiceError>>,
    >,
}

const GATE_RECIPE: &str = "repo = \"demo\"\n[[step]]\nname = \"check\"\nrun = \"test -f g.txt -o -f bad.txt && test ! -f bad.txt\"\nmemcap_gb = 1\ntime = \"2m\"\n";

impl GateRig {
    async fn start(builder: bool) -> Option<GateRig> {
        use hgl_ai_build::service::{self, Settings};
        use std::sync::Arc;
        let h = hub()?;
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
        hgl_ai_build::store::recipe_put(&b, GATE_RECIPE, None)
            .await
            .unwrap();
        let name = hgl_ai_proto::build::Platform::current().unwrap().as_str();
        let (mut stop, mut svc) = (None, None);
        if builder {
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
                pr_ended: None,
            };
            let svc_bus = Arc::new(Bus::connect_url(&h.url, None).await.unwrap());
            let (tx, rx) = tokio::sync::watch::channel(false);
            svc = Some(tokio::spawn(service::serve(svc_bus, lanes, settings, rx)));
            stop = Some(tx);
            let deadline = Instant::now() + Duration::from_secs(30);
            while b.builders().await.unwrap().is_empty() {
                assert!(Instant::now() < deadline, "the builder did not register");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Some(GateRig {
            h,
            b,
            _tmp: tmp,
            seed,
            m1,
            head,
            name,
            stop,
            svc,
        })
    }

    async fn rev(&self) -> u64 {
        hgl_ai_build::store::recipe_get(&self.b, "demo", None)
            .await
            .unwrap()
            .unwrap()
            .revision
    }

    async fn key(&self, head: &str, main: &str) -> String {
        hgl_ai_proto::build::verdict_key("demo", 7, self.name, head, main, self.rev().await)
    }

    async fn state(
        &self,
        builds: Option<&crate::builds::GateBuilds>,
        head: &str,
        main: &str,
    ) -> crate::builds::VerdictState {
        crate::builds::verdict_state(Some(&self.b), builds, "demo", 7, self.name, head, main)
            .await
            .unwrap()
    }

    async fn stop(mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(true);
        }
        if let Some(svc) = self.svc.take() {
            tokio::time::timeout(Duration::from_secs(60), svc)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        drop(self.h);
    }
}

fn controller_builds() -> crate::builds::GateBuilds {
    crate::builds::GateBuilds::new(Seat::new("controller").unwrap(), "controller")
}

/// hgl-ai#6: the gate asks a builder for the local build verdict of the head merged onto
/// the `main` it saw, and reads the key of exactly that pair; a `main` that has moved is
/// another key, asked again; a build that fails or does not merge says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate_builds_write_verdicts_and_a_moved_main_is_built_again() {
    use crate::builds::VerdictState;
    let Some(r) = GateRig::start(true).await else {
        return;
    };
    let (b, head, m1, name) = (&r.b, r.head.clone(), r.m1.clone(), r.name);
    // Nothing there and nothing asked (a dry run): missing.
    assert_eq!(r.state(None, &head, &m1).await, VerdictState::Missing(None));
    let builds = controller_builds();
    let first = judged(b, &builds, name, &head, &m1).await;
    let VerdictState::Pass(v) = &first else {
        panic!("{first:?}")
    };
    assert!(v.starts_with(&format!("{head} {m1} pass ")), "{v}");
    assert!(v.ends_with(&format!(" lab1 r{}", r.rev().await)), "{v}");
    let k1 = r.key(&head, &m1).await;
    assert_eq!(&b.state_get(&k1).await.unwrap().unwrap().value, v);
    let id = hgl_ai_proto::build::VerdictValue::parse(v)
        .unwrap()
        .id
        .unwrap();
    let run = hgl_ai_build::store::run_get(b, &id).await.unwrap().unwrap();
    assert_eq!(
        run.base_sha.as_deref(),
        Some(m1.as_str()),
        "it built onto m1"
    );
    // Judged: not asked again.
    assert_eq!(r.state(Some(&builds), &head, &m1).await, first);
    // `build gate` by hand: the pull request's head on GitHub's `main`.
    let mut st = crate::testutil::settings();
    st.gates.entry("demo".into()).or_default().verdicts = vec![name.to_string()];
    let pr = fixture("pr-open.json").replace("1f461fa77bba922fe81383df4c475feb58b7c233", &head);
    let fake = FakeGh::default().on("pulls/7", &pr).on(
        "git/ref/heads/main",
        &format!("{{\"object\":{{\"sha\":\"{m1}\"}}}}"),
    );
    let (h0, m0, by_hand) = crate::builds::gate(&fake, &st, b, &builds, "demo", 7)
        .await
        .unwrap();
    assert_eq!((h0.as_str(), m0.as_str()), (head.as_str(), m1.as_str()));
    assert_eq!(by_hand, vec![(name.to_string(), first.clone())]);
    st.gates.get_mut("demo").unwrap().verdicts.clear();
    assert!(
        crate::builds::gate(&fake, &st, b, &builds, "demo", 7)
            .await
            .is_err()
    );
    // `main` moves: m2's key is empty, so the gate waits and asks; m1's pass stays its own.
    git(&r.seed, &["checkout", "--quiet", "main"]);
    std::fs::write(r.seed.join("h.txt"), "h\n").unwrap();
    git(&r.seed, &["add", "h.txt"]);
    git(&r.seed, &["commit", "--quiet", "-m", "c1"]);
    git(&r.seed, &["push", "--quiet", "origin", "main"]);
    let m2 = git(&r.seed, &["rev-parse", "HEAD"]);
    assert_eq!(r.state(None, &head, &m2).await, VerdictState::Missing(None));
    match r.state(Some(&builds), &head, &m2).await {
        VerdictState::Building(w) => assert!(w.starts_with("asked "), "{w}"),
        other => panic!("not asked again: {other:?}"),
    }
    let second = judged(b, &builds, name, &head, &m2).await;
    let VerdictState::Pass(v2) = &second else {
        panic!("{second:?}")
    };
    assert!(v2.starts_with(&format!("{head} {m2} pass ")), "{v2}");
    assert_eq!(&b.state_get(&k1).await.unwrap().unwrap().value, v);
    // A head whose build fails: fail, naming the step.
    let bad = commit_on(&r.seed, "feature", &m1, "bad.txt", "x\n");
    match judged(b, &builds, name, &bad, &m2).await {
        VerdictState::Fail(v) => {
            assert!(v.starts_with(&format!("{bad} {m2} fail ")), "{v}");
            assert!(v.ends_with("step check failed"), "{v}");
        }
        other => panic!("{other:?}"),
    }
    // A head that does not merge onto main: not run, and the gate is told why.
    let clash = commit_on(&r.seed, "feature", &m1, "h.txt", "clash\n");
    match judged(b, &builds, name, &clash, &m2).await {
        VerdictState::NotRun(why) => assert!(why.contains("does not merge"), "{why}"),
        other => panic!("{other:?}"),
    }
    r.stop().await;
}

/// The controller on #56: the gate needs the verdict of the current recipe. An update is
/// another key, asked again; the old revision's pass no longer counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recipe_update_asks_again() {
    use crate::builds::VerdictState;
    let Some(r) = GateRig::start(true).await else {
        return;
    };
    let builds = controller_builds();
    let VerdictState::Pass(old) = judged(&r.b, &builds, r.name, &r.head, &r.m1).await else {
        panic!("no first pass")
    };
    let r1 = r.rev().await;
    hgl_ai_build::store::recipe_put(&r.b, &format!("{GATE_RECIPE}# stricter\n"), None)
        .await
        .unwrap();
    let r2 = r.rev().await;
    assert!(r2 > r1);
    assert_eq!(
        r.state(None, &r.head, &r.m1).await,
        VerdictState::Missing(None)
    );
    let VerdictState::Pass(new) = judged(&r.b, &builds, r.name, &r.head, &r.m1).await else {
        panic!("no pass under the new recipe")
    };
    assert!(old.ends_with(&format!(" r{r1}")) && new.ends_with(&format!(" r{r2}")));
    assert_ne!(old, new);
    r.stop().await;
}

use hgl_ai_proto::build::BuildJob;

/// r3 and r1 on #56: a `running` value whose builder is gone, or that ran past its bound,
/// never holds the gate: it is asked again of a live builder, once. While that replacement
/// is queued the old value is not judged again and nothing more is asked; a live one's
/// `running` is waited for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_running_with_a_queued_replacement_is_asked_once() {
    use crate::builds::VerdictState;
    // No builder serves: the replacement stays queued with `parked`, a record only.
    let Some(r) = GateRig::start(false).await else {
        return;
    };
    let platform = hgl_ai_proto::build::Platform::current().unwrap();
    let parked = hgl_ai_proto::build::BuilderRecord {
        label: "parked".into(),
        inbox: hgl_ai_proto::build::builder_inbox("parked").unwrap(),
        platform,
        arch: String::new(),
        executor: "local".into(),
        proxy: None,
        reachable: true,
        reason: None,
        state: "active".into(),
        slots: 1,
        busy: 1,
        free_gb: None,
        toolchains: Default::default(),
        updated_at: hgl_ai_proto::utc_now_stamp(),
    };
    r.b.builder_put(&parked).await.unwrap();
    let key = r.key(&r.head, &r.m1).await;
    for (label, until, why) in [
        (
            "ghost",
            "29991231T000000Z",
            "builder ghost of 20261127-120000-eeeeee is gone",
        ),
        (
            "parked",
            "20000101T000000Z",
            "20261127-120000-eeeeee on parked ran past its bound",
        ),
    ] {
        r.b.state_put(
            &key,
            &format!(
                "{} {} running 20261127-120000-eeeeee {label} r1 until={until}",
                r.head, r.m1
            ),
        )
        .await
        .unwrap();
        let builds = controller_builds();
        let VerdictState::Building(w) = r.state(Some(&builds), &r.head, &r.m1).await else {
            panic!("a lost build held the gate")
        };
        assert!(w.starts_with(why) && w.contains("; asked "), "{w}");
        let id = w
            .rsplit("; asked ")
            .next()
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string();
        // Rounds later the old value is still there and the replacement still queued:
        // waited for, never asked again.
        for _ in 0..4 {
            assert_eq!(
                r.state(Some(&builds), &r.head, &r.m1).await,
                VerdictState::Building(format!("asked {id} of parked"))
            );
        }
        let queued = hgl_ai_build::store::run_get(&r.b, &id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(queued.status, hgl_ai_proto::build::BuildStatus::Queued);
    }
    // A live builder's bounded `running`: waited for, nothing asked.
    r.b.state_put(
        &key,
        &format!(
            "{} {} running 20261127-120000-ffffff parked r1 until=29991231T000000Z",
            r.head, r.m1
        ),
    )
    .await
    .unwrap();
    let builds = controller_builds();
    assert_eq!(
        r.state(Some(&builds), &r.head, &r.m1).await,
        VerdictState::Building("building 20261127-120000-ffffff on parked".into())
    );
    // r3's minor: that build, stopped and queued again, with `parked` now draining: lost,
    // and asked of another builder (here none is live).
    let job = BuildJob {
        v: hgl_ai_proto::build::BUILD_VERSION,
        id: "20261127-120000-ffffff".into(),
        repo: "demo".into(),
        head: r.head.clone(),
        base: Some(r.m1.clone()),
        issue: 7,
        steps: None,
        recipe_rev: None,
        keep_going: false,
        verdict: Some(r.name.into()),
        reply_to: hgl_ai_proto::build::reply_seat("20261127-120000-ffffff").unwrap(),
        notify: None,
        requester: "controller".into(),
    };
    let mut draining = parked.clone();
    draining.state = "draining".into();
    r.b.builder_put(&draining).await.unwrap();
    hgl_ai_build::store::run_put(&r.b, &hgl_ai_build::request::queued(&job, &draining))
        .await
        .unwrap();
    match r.state(Some(&controller_builds()), &r.head, &r.m1).await {
        VerdictState::Missing(Some(w)) => assert!(
            w.starts_with(
                "20261127-120000-ffffff is queued again with builder parked, which takes no jobs; no live"
            ),
            "{w}"
        ),
        other => panic!("a build queued with a draining builder held the gate: {other:?}"),
    }
    r.stop().await;
}

/// Ready order (WORK-6) from the hub: the latest `ready` note to the controller per
/// repository and head, other kinds and other seats' notes left out.
#[tokio::test]
async fn ready_notes_are_read_from_the_controllers_notes() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let note = |kind: Kind, to: &str, head: &str, at: &str| {
        let mut n = hgl_ai_proto::Note::new(
            Seat::new("w1").unwrap(),
            Seat::new(to).unwrap(),
            kind,
            format!("[codex] {kind}: alpha#7 at {head} ({at})"),
        );
        n.repo = Some("alpha".into());
        n.issue = 7;
        n.head = Some(head.into());
        n.created_at = at.into();
        n
    };
    let other = "2222222222222222222222222222222222222222";
    for n in [
        note(Kind::Ready, "controller", HEAD, "20261128T090000Z"),
        note(Kind::Ready, "controller", HEAD, "20261128T100000Z"),
        note(Kind::Ready, "controller", other, "20261128T080000Z"),
        note(Kind::Blocked, "controller", HEAD, "20261128T110000Z"),
        note(Kind::Ready, "r1", HEAD, "20261128T120000Z"),
    ] {
        b.send(&n).await.unwrap();
    }
    let got = crate::candidates::read_ready_notes(&b, &settings())
        .await
        .unwrap();
    let at = |s| crate::table::parse_time(s).unwrap();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(
        got[&("alpha".to_string(), HEAD.to_string())],
        at("20261128T100000Z")
    );
    assert_eq!(
        got[&("alpha".to_string(), other.to_string())],
        at("20261128T080000Z")
    );
}

/// `alpha#7` at `HEAD` as one coordinator round reads it: Ready (its `Ready` comment), the
/// row clean; and `alpha` opted in to coordinator landing.
fn ready_round() -> (crate::Settings, Vec<crate::table::PrInfo>) {
    let mut s = crate::testutil::with_coordinator_lands(settings(), &["alpha"], true);
    s.repos = vec!["alpha".into()];
    s.pins = vec![];
    s.gates.get_mut("alpha").unwrap().verdicts = vec![platform_name().into()];
    let row = crate::table::Row {
        repo: "alpha".into(),
        number: 7,
        head: HEAD.into(),
        draft: false,
        bot: crate::bot::BotState::Clean,
        unresolved: 0,
        checks: crate::table::Checks::Pass,
        updated: hgl_ai_proto::utc_now_stamp(),
    };
    let info = crate::table::PrInfo {
        row,
        note: Some(crate::bot::NoteState::Ready),
        ready_at: Some("2026-11-28T10:00:00Z".into()),
        ready_cleared: None,
        url: "https://github.com/o/alpha/pull/7".into(),
        review_url: None,
    };
    (s, vec![info])
}

/// This host's platform: the local build verdict `alpha` names in [`ready_round`].
fn platform_name() -> &'static str {
    hgl_ai_proto::build::Platform::current().unwrap().as_str()
}

const ALPHA_MAIN: &str = "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a";

/// The hub as a coordinator landing of `alpha#7` finds it: [`authored_and_reviewed`], a
/// recipe for `alpha`, and (with `pass`) the builder's passed verdict for `HEAD` on `main`.
async fn coord_hub(b: &Bus, pass: bool) {
    authored_and_reviewed(b).await;
    hgl_ai_build::store::recipe_put(b, &GATE_RECIPE.replace("demo", "alpha"), None)
        .await
        .unwrap();
    if pass {
        let rev = hgl_ai_build::store::recipe_get(b, "alpha", None)
            .await
            .unwrap()
            .unwrap()
            .revision;
        let key =
            hgl_ai_proto::build::verdict_key("alpha", 7, platform_name(), HEAD, ALPHA_MAIN, rev);
        b.state_put(
            &key,
            &format!("{HEAD} {ALPHA_MAIN} pass 20261128-120000-aaaaaa lab1 r{rev}"),
        )
        .await
        .unwrap();
    }
}

/// A landing record of the coordinator's.
fn coord_record(merge: Option<&str>) -> crate::progress::Progress {
    crate::progress::Progress::new(HEAD, merge.map(String::from)).by(Some(crate::autoland::BY))
}

async fn put_record(b: &Bus, p: &crate::progress::Progress) {
    store::put_latest(
        b,
        &crate::progress::key("alpha", 7),
        &serde_json::to_string(p).unwrap(),
    )
    .await
    .unwrap();
}

/// The pull request's comments with `extra` note comments added: the clean fixture has the
/// bot's clean review and `Approved at 1f461fa7`, no `Ready`.
fn comments_with(extra: &[(&str, &str)]) -> String {
    let mut v: Vec<serde_json::Value> =
        serde_json::from_str(&fixture("comments-clean.json")).unwrap();
    for (body, at) in extra {
        v.push(serde_json::json!({
            "user": {"login": "octo"}, "body": body, "created_at": at, "updated_at": at
        }));
    }
    serde_json::to_string(&v).unwrap()
}

/// [`landing_gh`] with the comments the gate reads holding a `Ready` at `HEAD` before the
/// approval, and `extra` after it.
fn coord_gh(extra: &[(&str, &str)], after: impl FnOnce(FakeGh) -> FakeGh) -> FakeGh {
    let mut notes = vec![(
        "[claude] Ready: alpha#7 at 1f461fa7",
        "2026-01-01T05:50:00Z",
    )];
    notes.extend_from_slice(extra);
    let gh = landing_gh(after).on("issues/7/comments", &comments_with(&notes));
    // First: ahead of the clean fixture's rule.
    gh.rules.lock().unwrap().rotate_right(1);
    open_twice(gh)
}

/// The pull request reads `open` once more before it reads `merged`: the gate reads it,
/// then reads it again right before the merge call.
fn open_twice(gh: FakeGh) -> FakeGh {
    {
        let mut rules = gh.rules.lock().unwrap();
        let i = rules
            .iter()
            .position(|r| r.0 == "pulls/7" && r.2)
            .expect("the pull request reads open once");
        let again = rules[i].clone();
        rules.insert(i, again);
    }
    gh
}

/// The coordinator's reports: its notes to the controller and `decisions.log`.
async fn coord_reports(b: &Bus) -> (Vec<String>, Vec<String>) {
    let notes = b
        .read_notes(&[Seat::new("controller").unwrap()], 1, 100, Duration::ZERO)
        .await
        .unwrap()
        .into_iter()
        .filter(|n| n.note.kind == Kind::Note && n.note.sender.as_str() == "coord")
        .map(|n| n.note.body)
        .collect();
    let log = hgl_ai_owner::decisions_tail(b, 10).await.unwrap();
    (notes, log)
}

/// hgl-ai#123: a round lands the Ready candidate of an opted-in repository through the
/// gate and reports it once, to the controller and `decisions.log`, before the record goes;
/// the next round, the pull request gone from the open ones, does nothing more.
#[tokio::test]
async fn the_coordinator_lands_a_ready_pull_request() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, infos) = ready_round();
    let gh = coord_gh(&[], |g| g);
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &infos).await.unwrap();
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 1, "merged");
    let log = b.state_get(state::KEY_LANDED).await.unwrap().unwrap().value;
    assert!(
        log.trim_end().ends_with(&format!("alpha #7 {MERGE}")),
        "{log}"
    );
    assert_eq!(
        landed_notes(&b).await,
        [
            ("w1".to_string(), 980),
            ("all".into(), 980),
            ("all".into(), 7)
        ]
    );
    let (sent, decisions) = coord_reports(&b).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        sent[0].starts_with(&format!(
            "[coord] Landed: alpha#7 at 1f461fa7 as {}\n",
            &MERGE[..8]
        )),
        "{}",
        sent[0]
    );
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert!(
        decisions[0].ends_with(&format!(
            "coord: landed alpha#7 at 1f461fa7 as {}",
            &MERGE[..8]
        )),
        "{decisions:?}"
    );
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none()
    );

    let calls = gh.calls().len();
    a.round(&gh, &s, &b, &[]).await.unwrap();
    assert_eq!(gh.calls().len(), calls, "no GitHub call");
    assert_eq!(coord_reports(&b).await.0.len(), 1);
    assert_eq!(hgl_ai_owner::decisions_tail(&b, 10).await.unwrap().len(), 1);
}

/// Review (r6): Ready is confirmed at the moment of the attempt. Sent back after the round
/// read it Ready: the gate refuses, nothing is merged and nothing is reported.
#[tokio::test]
async fn a_candidate_sent_back_since_the_read_is_not_merged() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    authored_and_reviewed(&b).await;
    let (s, infos) = ready_round();
    let gh = coord_gh(
        &[(
            "[codex] (controller) Sent back at 1f461fa7",
            "2026-01-01T06:10:00Z",
        )],
        |g| g,
    );
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &infos).await.unwrap();
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 0, "not merged");
    assert!(b.state_get(state::KEY_LANDED).await.unwrap().is_none());
    assert_eq!(coord_reports(&b).await, (vec![], vec![]));
}

/// hgl-ai#123 with hgl-ai#7: the coordinator is cut off right after the merge (the read
/// back fails), the record holding only the intent. A restarted coordinator, the pull
/// request gone from the open ones, asks GitHub, finds it merged, and finishes it once.
#[tokio::test]
async fn a_coordinator_landing_cut_off_after_the_merge_resumes_next_round() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, infos) = ready_round();
    let gh = coord_gh(&[], |g| {
        g.push_fail_once("pulls/7", "gh: Server Error (HTTP 502)")
    });
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &infos).await.unwrap();
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 1, "merged");
    assert!(b.state_get(state::KEY_LANDED).await.unwrap().is_none());
    let rec = crate::progress::read(&b, "alpha", 7)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rec.merge, None, "the intent only");
    assert!(
        coord_reports(&b)
            .await
            .0
            .iter()
            .all(|n| !n.starts_with("[coord] Landed"))
    );

    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &[]).await.unwrap();
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 1, "not merged again");
    let (sent, decisions) = coord_reports(&b).await;
    let landed: Vec<&String> = sent
        .iter()
        .filter(|n| n.starts_with("[coord] Landed"))
        .collect();
    assert_eq!(landed.len(), 1, "{sent:?}");
    assert!(
        landed[0].starts_with(&format!(
            "[coord] Landed: alpha#7 at 1f461fa7 as {}, finishing an earlier attempt",
            &MERGE[..8]
        )),
        "{}",
        landed[0]
    );
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    let log = b.state_get(state::KEY_LANDED).await.unwrap().unwrap().value;
    assert_eq!(log.lines().count(), 1, "{log}");
    assert_eq!(landed_notes(&b).await.len(), 3);
    assert_eq!(posts(&gh, "issues/7/comments"), 1);
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none()
    );

    a.round(&gh, &s, &b, &[]).await.unwrap();
    assert_eq!(coord_reports(&b).await.1.len(), 1);
}

/// Review (codex P1, r6): a record with no merge whose pull request is not among the open
/// ones read is not taken as closed. Still open on GitHub: left to the candidates, nothing
/// merged. Closed without a merge: retired, and not asked about again.
#[tokio::test]
async fn an_unmerged_record_is_finished_only_when_github_shows_it_merged() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    authored_and_reviewed(&b).await;
    let (s, _) = ready_round();
    put_record(&b, &coord_record(None)).await;
    let gh = FakeGh::default().on("pulls/7", &fixture("pr-open.json"));
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &[]).await.unwrap();
    a.round(&gh, &s, &b, &[]).await.unwrap();
    assert_eq!(gh.calls().len(), 2, "asked each round: {:?}", gh.calls());
    assert!(
        gh.calls().iter().all(|c| !c.contains("merge")),
        "{:?}",
        gh.calls()
    );
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_some()
    );

    let closed = fixture("pr-open.json").replace("\"state\": \"open\"", "\"state\": \"closed\"");
    assert!(
        closed.contains("\"state\": \"closed\""),
        "the fixture's form"
    );
    let gh = FakeGh::default().on("pulls/7", &closed);
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &[]).await.unwrap();
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none(),
        "retired"
    );
    // Retired: a restarted coordinator does not ask again.
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &[]).await.unwrap();
    // Read, then read again under the lock before it is retired; then never again.
    assert_eq!(gh.calls().len(), 2, "{:?}", gh.calls());
    assert_eq!(coord_reports(&b).await, (vec![], vec![]));
}

/// Review (r6): both reports are steps of the landing's record. A record whose landing
/// steps and controller note are done, its `decisions.log` line not: the next round writes
/// only the line, then deletes the record.
#[tokio::test]
async fn a_report_left_undone_is_finished_from_the_record() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    authored_and_reviewed(&b).await;
    let (s, _) = ready_round();
    let mut p = coord_record(Some(MERGE));
    for step in [
        crate::progress::LANDED_LOG,
        crate::progress::MAIN,
        crate::progress::NOTES,
        crate::progress::COMMENT,
        crate::autoland::STEP_NOTE,
    ] {
        p.mark(step);
    }
    p.notes_at = Some("20261128T120000Z".into());
    put_record(&b, &p).await;
    let gh = coord_gh(&[], |g| g);
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &[]).await.unwrap();
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 0);
    assert_eq!(posts(&gh, "issues/7/comments"), 0);
    let (sent, decisions) = coord_reports(&b).await;
    assert!(sent.is_empty(), "{sent:?}");
    assert_eq!(
        decisions,
        [format!(
            "20261128T120000Z coord: landed alpha#7 at 1f461fa7 as {}",
            &MERGE[..8]
        )]
    );
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none()
    );
}

/// No repository opts in (the default): Ready pull requests and landing records are left
/// to the controller, with no GitHub call.
#[tokio::test]
async fn without_opt_in_the_coordinator_lands_nothing() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    authored_and_reviewed(&b).await;
    let (s, infos) = ready_round();
    let s = crate::testutil::with_coordinator_lands(s, &["alpha"], false);
    store::put_latest(
        &b,
        &crate::progress::key("alpha", 7),
        &serde_json::to_string(&crate::progress::Progress::new(HEAD, Some(MERGE.into()))).unwrap(),
    )
    .await
    .unwrap();
    let gh = coord_gh(&[], |g| g);
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &infos).await.unwrap();
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_some()
    );
}

/// The row says a check failed: no gate call this round.
#[tokio::test]
async fn a_candidate_the_row_holds_back_costs_no_github_call() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let (s, mut infos) = ready_round();
    infos[0].row.checks = crate::table::Checks::Fail;
    let gh = coord_gh(&[], |g| g);
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &infos).await.unwrap();
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
}

/// Review (codex P2): a candidate lands, then the next in Ready order hits GitHub's rate
/// limit. The round fails; the landing before it is reported and logged all the same.
#[tokio::test]
async fn a_landing_before_a_rate_limit_is_still_reported() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, mut infos) = ready_round();
    let mut eight = infos[0].clone();
    eight.row.number = 8;
    eight.ready_at = Some("2026-11-28T11:00:00Z".into());
    infos.push(eight);
    let gh = coord_gh(&[], |g| {
        g.fail("/8", "gh: API rate limit exceeded (HTTP 403)")
    });
    let mut a = crate::autoland::AutoLand::new(&s);
    let e = a.round(&gh, &s, &b, &infos).await.unwrap_err();
    assert!(matches!(e, Error::RateLimited(_)), "{e}");
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 1, "#7 merged");
    let (sent, decisions) = coord_reports(&b).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        sent[0].starts_with("[coord] Landed: alpha#7 at 1f461fa7"),
        "{}",
        sent[0]
    );
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none()
    );
}

/// Review (r6, P1 hold): a `hold` stops the coordinator's landings. One already there:
/// no GitHub call. One written after the attempt began: read again right before the merge
/// call, and nothing is merged.
#[tokio::test]
async fn a_hold_stops_the_coordinators_landings() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, infos) = ready_round();
    b.state_put(state::KEY_HOLD, "2999-01-01T00:00:00Z owner freeze")
        .await
        .unwrap();
    let gh = coord_gh(&[], |g| g);
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &infos).await.unwrap();
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());

    // At the merge: the same attempt, the hold there when it gets to the merge call.
    let l = land::merge_once(&gh, &s, &b, "alpha", 7, HEAD, &a.opts())
        .await
        .unwrap();
    assert_eq!(
        l.outcome.kind,
        crate::gate::Kind::Hold,
        "{}",
        l.outcome.line
    );
    assert!(
        l.outcome
            .line
            .contains("at the merge: hold until 2999-01-01T00:00:00Z"),
        "{}",
        l.outcome.line
    );
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 0, "not merged");
    assert_eq!(reads(&gh, "issues/7/comments"), 2, "got to the merge call");
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none(),
        "no intent written"
    );
    // The coordinator's own hold never shortens it.
    assert_eq!(
        store::raise_hold(&b, "2100-01-01T00:00:00Z").await.unwrap(),
        "2999-01-01T00:00:00Z owner freeze"
    );
}

/// Review (r6, P1 Ready): Ready is read again with the pull request right before the
/// merge call. Sent back between the gate's read and the merge: nothing merged.
#[tokio::test]
async fn a_sendback_just_before_the_merge_stops_it() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, infos) = ready_round();
    let ready = comments_with(&[(
        "[claude] Ready: alpha#7 at 1f461fa7",
        "2026-01-01T05:50:00Z",
    )]);
    let sent_back = comments_with(&[
        (
            "[claude] Ready: alpha#7 at 1f461fa7",
            "2026-01-01T05:50:00Z",
        ),
        (
            "[codex] (controller) Sent back at 1f461fa7",
            "2026-01-01T06:10:00Z",
        ),
    ]);
    let gh = open_twice(
        landing_gh(|g| g)
            .once("issues/7/comments", &ready)
            .on("issues/7/comments", &sent_back),
    );
    gh.rules.lock().unwrap().rotate_right(2);
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &infos).await.unwrap();
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 0, "not merged");
    assert_eq!(
        reads(&gh, "issues/7/comments"),
        2,
        "stopped at the merge call"
    );
    assert!(b.state_get(state::KEY_LANDED).await.unwrap().is_none());
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(coord_reports(&b).await, (vec![], vec![]));
}

/// Review (r6, P1 verdict): CI checks alone never land. A repository that names no local
/// build verdict is refused; one whose verdict for the head on `main` is not in is not
/// merged either.
#[tokio::test]
async fn without_a_build_verdict_the_coordinator_does_not_land() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, false).await;
    let (mut s, infos) = ready_round();
    let gh = coord_gh(&[], |g| g);
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &infos).await.unwrap();
    assert_eq!(
        posts(&gh, "pulls/7/merge-async"),
        0,
        "no verdict: not merged"
    );

    s.gates.get_mut("alpha").unwrap().verdicts.clear();
    let l = land::merge_once(&gh, &s, &b, "alpha", 7, HEAD, &a.opts())
        .await
        .unwrap();
    assert_eq!(
        l.outcome.kind,
        crate::gate::Kind::Blocked,
        "{}",
        l.outcome.line
    );
    assert!(
        l.outcome.line.contains("names no local build verdict"),
        "{}",
        l.outcome.line
    );
    assert_eq!(
        posts(&gh, "pulls/7/merge-async"),
        0,
        "checks only: not merged"
    );
    assert_eq!(coord_reports(&b).await, (vec![], vec![]));
}

/// Review (r6, P2 pins): the controller's landing record whose only step left is the
/// wrapper's pins is the controller's: neither finished nor deleted, as a record or as a
/// candidate's.
#[tokio::test]
async fn the_controllers_record_with_the_pins_left_is_left_to_it() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, infos) = ready_round();
    let mut p = crate::progress::Progress::new(HEAD, Some(MERGE.into()));
    for step in [
        crate::progress::LANDED_LOG,
        crate::progress::MAIN,
        crate::progress::NOTES,
        crate::progress::COMMENT,
    ] {
        p.mark(step);
    }
    put_record(&b, &p).await;
    let gh = coord_gh(&[], |g| g);
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &[]).await.unwrap();
    a.round(&gh, &s, &b, &infos).await.unwrap();
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
    assert_eq!(
        crate::progress::read(&b, "alpha", 7).await.unwrap(),
        Some(p),
        "untouched"
    );
    assert_eq!(coord_reports(&b).await, (vec![], vec![]));
}

/// Review (r6, P1 reports): a `PARTIAL` goes to the controller once, also across a
/// restart: a step of the landing's record.
#[tokio::test]
async fn a_partial_is_sent_once_across_a_restart() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let (s, _) = ready_round();
    put_record(&b, &coord_record(Some(MERGE))).await;
    let mut outcome = crate::gate::Outcome::new(
        crate::gate::Kind::Partial,
        "alpha #7 is merged, but the landed notes are not sent PARTIAL".into(),
        false,
        HEAD,
    );
    outcome.merge_commit = Some(MERGE.into());
    let l = land::Landing {
        outcome,
        after: vec![],
    };
    let t = crate::autoland::Try {
        repo: "alpha".into(),
        number: 7,
        head: HEAD.into(),
    };
    for _ in 0..2 {
        let lock = crate::lock::LandLock::acquire(&b, "alpha", "test", Duration::ZERO)
            .await
            .unwrap();
        let mut a = crate::autoland::AutoLand::new(&s);
        a.partial(&s, &b, &t, &l, &lock).await.unwrap();
        lock.release().await;
    }
    let (sent, _) = coord_reports(&b).await;
    assert_eq!(
        sent,
        ["[coord] Partial: alpha #7 is merged, but the landed notes are not sent PARTIAL\n"]
    );
    let p = crate::progress::read(&b, "alpha", 7)
        .await
        .unwrap()
        .unwrap();
    assert!(p.has(crate::autoland::STEP_PARTIAL));
}

/// Review (r6, P2 builder): the gate build the coordinator asked for is kept on the hub.
/// A restarted coordinator finds it still queued and waits for it: no second job.
#[tokio::test]
async fn a_queued_gate_build_is_not_asked_again_after_a_restart() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, false).await;
    // No builder serves: the job stays queued with `parked`, a record only.
    let parked = hgl_ai_proto::build::BuilderRecord {
        label: "parked".into(),
        inbox: hgl_ai_proto::build::builder_inbox("parked").unwrap(),
        platform: hgl_ai_proto::build::Platform::current().unwrap(),
        arch: String::new(),
        executor: "local".into(),
        proxy: None,
        reachable: true,
        reason: None,
        state: "active".into(),
        slots: 1,
        busy: 0,
        free_gb: None,
        toolchains: Default::default(),
        updated_at: hgl_ai_proto::utc_now_stamp(),
    };
    b.builder_put(&parked).await.unwrap();
    let (s, infos) = ready_round();
    let jobs = || async {
        hgl_ai_build::store::runs(&b, Some("alpha"), false, 100)
            .await
            .unwrap()
    };
    for _ in 0..3 {
        // A new coordinator each round: its memory is gone.
        let gh = coord_gh(&[], |g| g.on("pulls/7", &fixture("pr-open.json")));
        let mut a = crate::autoland::AutoLand::new(&s);
        a.round(&gh, &s, &b, &infos).await.unwrap();
        assert_eq!(posts(&gh, "pulls/7/merge-async"), 0);
        let runs = jobs().await;
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].status, hgl_ai_proto::build::BuildStatus::Queued);
    }
    let keys = b.state_keys(crate::autoland::ASK_PREFIX).await.unwrap();
    assert_eq!(
        keys,
        [format!(
            "{}alpha.7.{}",
            crate::autoland::ASK_PREFIX,
            platform_name()
        )]
    );
}

/// One rule of `src` (its first), put first in `gh`'s rules: it answers before the rest.
fn first_rule(gh: &FakeGh, src: FakeGh) {
    let r = src.rules.lock().unwrap().remove(0);
    gh.rules.lock().unwrap().insert(0, r);
}

/// The land steps of a merged landing done, its `landed` notes stamped: what is left is
/// the coordinator's reports.
fn landed_record() -> crate::progress::Progress {
    let mut p = coord_record(Some(MERGE));
    for step in [
        crate::progress::LANDED_LOG,
        crate::progress::MAIN,
        crate::progress::NOTES,
        crate::progress::COMMENT,
    ] {
        p.mark(step);
    }
    p.notes_at = Some("20261128T120000Z".into());
    p
}

/// Review (r6, P1): a coordinator's record is the coordinator's to finish. The
/// controller's `merge-if-clean` and `land` refuse it, the record untouched, and the
/// coordinator's next round sends both reports.
#[tokio::test]
async fn the_controller_leaves_a_coordinator_record_to_it() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, _) = ready_round();
    let p = landed_record();
    put_record(&b, &p).await;
    let gh = coord_gh(&[], |g| g);
    let o = land::LandOpts::default();
    let l = land::merge_once(&gh, &s, &b, "alpha", 7, HEAD, &o)
        .await
        .unwrap();
    assert_eq!(
        l.outcome.kind,
        crate::gate::Kind::Blocked,
        "{}",
        l.outcome.line
    );
    assert!(
        l.outcome.line.contains("is the coordinator's"),
        "{}",
        l.outcome.line
    );
    let l = land::land(&gh, &s, &b, "alpha", 7, HEAD, &o).await.unwrap();
    assert_eq!(
        l.outcome.kind,
        crate::gate::Kind::Blocked,
        "{}",
        l.outcome.line
    );
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
    assert_eq!(
        crate::progress::read(&b, "alpha", 7).await.unwrap(),
        Some(p),
        "untouched"
    );

    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &[]).await.unwrap();
    let (sent, decisions) = coord_reports(&b).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(decisions.len(), 1, "{decisions:?}");
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none()
    );
}

/// Review (r6, P1): an intent for `HEAD` whose pull request GitHub shows merged at another
/// head is not the coordinator's merge: retired, nothing finished or announced.
#[tokio::test]
async fn an_intent_whose_head_did_not_merge_is_retired_unannounced() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, _) = ready_round();
    put_record(&b, &coord_record(None)).await;
    let other = "b".repeat(40);
    let merged_other = fixture("pr-merged.json").replace(HEAD, &other);
    let gh = FakeGh::default().on("pulls/7", &merged_other);
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &[]).await.unwrap();
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none(),
        "retired"
    );
    assert!(b.state_get(state::KEY_LANDED).await.unwrap().is_none());
    assert_eq!(coord_reports(&b).await, (vec![], vec![]));
    assert!(landed_notes(&b).await.is_empty());
    assert_eq!(posts(&gh, "issues/7/comments"), 0);
}

/// Review (r6, P1): right before the merge call the pull request is read again with
/// `main`. `main` moved since the gate read it (the verdict is for the old one): no merge,
/// tried again later. The pull request retargeted off `main`: no merge.
#[tokio::test]
async fn main_or_the_base_moving_during_the_gate_stops_the_merge() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, infos) = ready_round();
    let gh = coord_gh(&[], |g| g);
    {
        // The second read of main (right before the merge) sees it moved.
        let mut rules = gh.rules.lock().unwrap();
        let i = rules
            .iter()
            .position(|r| r.0 == "git/ref/heads/main")
            .unwrap();
        let mut moved = rules[i].clone();
        moved.1.stdout = moved.1.stdout.replace(ALPHA_MAIN, &"d".repeat(40));
        rules.insert(i + 1, moved);
    }
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &infos).await.unwrap();
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 0, "not merged");
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none()
    );

    let gh = coord_gh(&[], |g| g);
    {
        // The second read of the pull request (right before the merge) has another base.
        let mut rules = gh.rules.lock().unwrap();
        let i = rules.iter().position(|r| r.0 == "pulls/7" && r.2).unwrap();
        let body = &rules[i + 1].1.stdout;
        assert!(body.contains("\"ref\": \"main\""), "the fixture's form");
        rules[i + 1].1.stdout = body.replace("\"ref\": \"main\"", "\"ref\": \"release\"");
    }
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &infos).await.unwrap();
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 0, "not merged");
    assert_eq!(coord_reports(&b).await, (vec![], vec![]));
}

/// Review (r6, P2): refused credentials on the diff read are not this pull request's
/// block: the round stops with the error.
#[tokio::test]
async fn a_401_on_the_diff_stops_the_round() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, infos) = ready_round();
    let gh = coord_gh(&[], |g| g);
    first_rule(
        &gh,
        FakeGh::default().fail(
            "Accept: application/vnd.github.v3.diff",
            "gh: Bad credentials (HTTP 401)",
        ),
    );
    let mut a = crate::autoland::AutoLand::new(&s);
    let e = a.round(&gh, &s, &b, &infos).await.unwrap_err();
    assert!(matches!(e, Error::Auth(_)), "{e}");
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 0);
}

/// The `build` notes to the parked builder.
async fn build_notes(b: &Bus) -> Vec<String> {
    let inbox = hgl_ai_proto::build::builder_inbox("parked").unwrap();
    b.read_notes(&[inbox], 1, 100, Duration::ZERO)
        .await
        .unwrap()
        .into_iter()
        .filter(|n| n.note.kind == Kind::Build)
        .map(|n| n.note.body)
        .collect()
}

async fn park_a_builder(b: &Bus) {
    let parked = hgl_ai_proto::build::BuilderRecord {
        label: "parked".into(),
        inbox: hgl_ai_proto::build::builder_inbox("parked").unwrap(),
        platform: hgl_ai_proto::build::Platform::current().unwrap(),
        arch: String::new(),
        executor: "local".into(),
        proxy: None,
        reachable: true,
        reason: None,
        state: "active".into(),
        slots: 1,
        busy: 0,
        free_gb: None,
        toolchains: Default::default(),
        updated_at: hgl_ai_proto::utc_now_stamp(),
    };
    b.builder_put(&parked).await.unwrap();
}

/// Review (r6 P2, bot P2): a coordinator stopped between the queued summary and the job's
/// note leaves an ask kept unsent. The next one sends the same job again (its id, its
/// note's `created_at`), once; an ask whose job did go out is sent again as the same
/// message, which the hub drops.
#[tokio::test]
async fn a_gate_build_cut_off_before_its_job_went_out_is_sent_again() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, false).await;
    park_a_builder(&b).await;
    let (s, infos) = ready_round();
    let round = || async {
        let gh = coord_gh(&[], |g| g.on("pulls/7", &fixture("pr-open.json")));
        let mut a = crate::autoland::AutoLand::new(&s);
        a.round(&gh, &s, &b, &infos).await.unwrap();
        assert_eq!(posts(&gh, "pulls/7/merge-async"), 0);
    };
    round().await;
    assert_eq!(build_notes(&b).await.len(), 1);
    let key = format!("{}alpha.7.{}", crate::autoland::ASK_PREFIX, platform_name());
    let kept = b.state_get(&key).await.unwrap().unwrap().value;
    let mut v: serde_json::Value = serde_json::from_str(&kept).unwrap();
    assert_eq!(v["sent"], serde_json::json!(true), "{kept}");
    let id = v["id"].as_str().unwrap().to_string();

    // Its send not known done (the flag lost): the same message again, dropped by the hub.
    v["sent"] = serde_json::json!(false);
    store::put_latest(&b, &key, &v.to_string()).await.unwrap();
    round().await;
    assert_eq!(build_notes(&b).await.len(), 1, "the same message");
    let v: serde_json::Value =
        serde_json::from_str(&b.state_get(&key).await.unwrap().unwrap().value).unwrap();
    assert_eq!(v["sent"], serde_json::json!(true));

    // Cut off after the queued summary, before the note: a new job kept unsent.
    let fresh = hgl_ai_proto::build::new_build_id();
    let text = v.to_string().replace(&id, &fresh);
    let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v["sent"] = serde_json::json!(false);
    v["at"] = serde_json::json!(hgl_ai_proto::utc_now_stamp());
    store::put_latest(&b, &key, &v.to_string()).await.unwrap();
    let job: hgl_ai_proto::build::BuildJob = serde_json::from_value(v["job"].clone()).unwrap();
    let parked = b.builders().await.unwrap().remove(0);
    hgl_ai_build::store::run_put(&b, &hgl_ai_build::request::queued(&job, &parked))
        .await
        .unwrap();
    round().await;
    let notes = build_notes(&b).await;
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(notes[1].contains(&fresh), "{}", notes[1]);
    round().await;
    assert_eq!(build_notes(&b).await.len(), 2, "sent once");
}

/// The rule answering `pattern` (its first, `once` or not), changed by `f`.
fn change_rule(gh: &FakeGh, pattern: &str, once: bool, f: impl FnOnce(&mut String)) {
    let mut rules = gh.rules.lock().unwrap();
    let r = rules
        .iter_mut()
        .find(|r| r.0 == pattern && r.2 == once)
        .expect("the rule");
    f(&mut r.1.stdout);
}

/// Review (r6 at b0cca78e, P1): a refused merge request read back merged by someone else,
/// at another head or into another base, is not this landing's merge: not journaled, not
/// announced, its intent gone.
#[tokio::test]
async fn a_refused_merge_read_back_merged_otherwise_is_not_journaled() {
    for (what, from, to) in [
        ("another head", HEAD.to_string(), "b".repeat(40)),
        (
            "another base",
            "\"ref\": \"main\"".to_string(),
            "\"ref\": \"release\"".to_string(),
        ),
    ] {
        let Some(h) = hub() else { return };
        let b = bus(&h).await;
        coord_hub(&b, true).await;
        let (s, infos) = ready_round();
        let gh = coord_gh(&[], |g| g);
        first_rule(
            &gh,
            FakeGh::default().fail(
                "pulls/7/merge-async",
                "gh: Pull Request is not mergeable (HTTP 405)",
            ),
        );
        change_rule(&gh, "pulls/7", false, |body| {
            assert!(body.contains(&from), "{what}: the fixture's form");
            *body = body.replace(&from, &to);
        });
        let mut a = crate::autoland::AutoLand::new(&s);
        a.round(&gh, &s, &b, &infos).await.unwrap();
        assert_eq!(
            posts(&gh, "pulls/7/merge-async"),
            1,
            "{what}: asked, refused"
        );
        assert!(
            crate::progress::read(&b, "alpha", 7)
                .await
                .unwrap()
                .is_none(),
            "{what}: no record"
        );
        assert!(
            b.state_get(state::KEY_LANDED).await.unwrap().is_none(),
            "{what}"
        );
        assert_eq!(coord_reports(&b).await, (vec![], vec![]), "{what}");
        assert!(landed_notes(&b).await.is_empty(), "{what}");
    }
}

/// Review (r6 at b0cca78e, P1): an intent at `HEAD` whose pull request GitHub shows merged
/// at that head but into another base is not recovered: the coordinator retires it,
/// unannounced, and the controller's `land` finishes nothing for it.
#[tokio::test]
async fn an_intent_merged_into_another_base_is_not_recovered() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, _) = ready_round();
    put_record(&b, &coord_record(None)).await;
    let elsewhere = fixture("pr-merged.json").replace("\"ref\": \"main\"", "\"ref\": \"release\"");
    assert!(elsewhere.contains("release"), "the fixture's form");
    let gh = FakeGh::default().on("pulls/7", &elsewhere);
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &[]).await.unwrap();
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none(),
        "retired"
    );
    assert_eq!(coord_reports(&b).await, (vec![], vec![]));

    let gh = coord_gh(&[], |g| g);
    for r in gh
        .rules
        .lock()
        .unwrap()
        .iter_mut()
        .filter(|r| r.0 == "pulls/7")
    {
        r.1.stdout = elsewhere.clone();
    }
    // r6 at f75a442f, P1: unsatisfied, never AlreadyMerged (the pins took that for landed),
    // and the controller's record of it is kept.
    put_record(&b, &crate::progress::Progress::new(HEAD, None)).await;
    for l in [
        land::merge_once(&gh, &s, &b, "alpha", 7, HEAD, &land::LandOpts::default())
            .await
            .unwrap(),
        land::land(&gh, &s, &b, "alpha", 7, HEAD, &land::LandOpts::default())
            .await
            .unwrap(),
    ] {
        assert_eq!(
            l.outcome.kind,
            crate::gate::Kind::Blocked,
            "{}",
            l.outcome.line
        );
        assert!(!l.outcome.kind.satisfied() && !l.outcome.kind.landed());
        assert!(l.outcome.line.contains("not main"), "{}", l.outcome.line);
    }
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_some(),
        "kept"
    );
    assert!(b.state_get(state::KEY_LANDED).await.unwrap().is_none());
    assert!(landed_notes(&b).await.is_empty());
}

/// Review (codex P1, r6 at b0cca78e): only the fleet's comments are notes. An outsider's
/// `Ready` after the fleet's sendback does not make the pull request Ready; an outsider's
/// `Blocked` does not stop a Ready one.
#[tokio::test]
async fn only_the_fleets_comments_decide_ready() {
    let comments = |extra: &[(&str, &str, &str)]| {
        let mut v: Vec<serde_json::Value> =
            serde_json::from_str(&fixture("comments-clean.json")).unwrap();
        for (who, body, at) in [(
            "octo",
            "[claude] Ready: alpha#7 at 1f461fa7",
            "2026-01-01T05:50:00Z",
        )]
        .iter()
        .chain(extra)
        {
            v.push(serde_json::json!({
                "user": {"login": who}, "body": body, "created_at": at, "updated_at": at
            }));
        }
        serde_json::to_string(&v).unwrap()
    };
    let sendback = (
        "octo",
        "[codex] (controller) Sent back at 1f461fa7",
        "2026-01-01T06:10:00Z",
    );
    for (what, extra, merges) in [
        (
            "a forged Ready after a sendback",
            vec![
                sendback,
                (
                    "stranger",
                    "[claude] Ready: alpha#7 at 1f461fa7",
                    "2026-01-01T06:20:00Z",
                ),
            ],
            false,
        ),
        (
            "a stranger's Blocked",
            vec![(
                "stranger",
                "[codex] Blocked: alpha#7",
                "2026-01-01T06:20:00Z",
            )],
            true,
        ),
    ] {
        let Some(h) = hub() else { return };
        let b = bus(&h).await;
        coord_hub(&b, true).await;
        let (s, infos) = ready_round();
        let gh = coord_gh(&[], |g| g);
        let text = comments(&extra);
        change_rule(&gh, "issues/7/comments", false, |body| *body = text.clone());
        let mut a = crate::autoland::AutoLand::new(&s);
        a.round(&gh, &s, &b, &infos).await.unwrap();
        assert_eq!(
            posts(&gh, "pulls/7/merge-async"),
            usize::from(merges),
            "{what}"
        );
    }
}

/// Review (r6 at b0cca78e, P2): the coordinator's intent at `HEAD`, the pull request now
/// open at another head. The controller's `merge-if-clean` is not blocked by it: under
/// the lock it is retired as obsolete. The coordinator's round retires such an intent too.
#[tokio::test]
async fn a_stale_intent_is_retired_under_the_lock() {
    let other = "e".repeat(40);
    let moved = fixture("pr-open.json").replace(HEAD, &other);
    {
        let Some(h) = hub() else { return };
        let b = bus(&h).await;
        coord_hub(&b, true).await;
        let (s, _) = ready_round();
        put_record(&b, &coord_record(None)).await;
        let gh = FakeGh::default()
            .on("pulls/7", &moved)
            .on(
                "git/ref/heads/main",
                &format!("{{\"object\":{{\"sha\":\"{ALPHA_MAIN}\"}}}}"),
            )
            .on("git/commits/", "{\"parents\":[]}");
        let l = land::merge_once(&gh, &s, &b, "alpha", 7, HEAD, &land::LandOpts::default())
            .await
            .unwrap();
        assert!(
            !l.outcome.line.contains("is the coordinator's"),
            "{}",
            l.outcome.line
        );
        assert!(
            crate::progress::read(&b, "alpha", 7)
                .await
                .unwrap()
                .is_none(),
            "retired"
        );
    }
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, mut infos) = ready_round();
    put_record(&b, &coord_record(None)).await;
    infos[0].row.head = other.clone();
    infos[0].ready_at = None;
    let gh = FakeGh::default().on("pulls/7", &moved);
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &infos).await.unwrap();
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none(),
        "retired"
    );
    assert_eq!(coord_reports(&b).await, (vec![], vec![]));
}

/// Review (r6 at b0cca78e, P2): a job sent again never sets back its summary: one the
/// builder has moved on (here final) stays.
#[tokio::test]
async fn sending_a_job_again_keeps_a_later_summary() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    park_a_builder(&b).await;
    let parked = b.builders().await.unwrap().remove(0);
    let id = hgl_ai_proto::build::new_build_id();
    let job: hgl_ai_proto::build::BuildJob = serde_json::from_value(serde_json::json!({
        "v": hgl_ai_proto::build::BUILD_VERSION, "id": id, "repo": "alpha", "head": HEAD,
        "issue": 7, "reply_to": format!("r-{id}"), "requester": "coordinator",
    }))
    .unwrap();
    let mut done = hgl_ai_build::request::queued(&job, &parked);
    done.status = hgl_ai_proto::build::BuildStatus::Pass;
    hgl_ai_build::store::run_put(&b, &done).await.unwrap();
    let at = hgl_ai_proto::utc_now_stamp();
    hgl_ai_build::request::send_at(&b, Seat::new("coord").unwrap(), &parked, &job, Some(&at))
        .await
        .unwrap();
    let kept = hgl_ai_build::store::run_get(&b, &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.status, hgl_ai_proto::build::BuildStatus::Pass);
    // Absent, it is created queued.
    let fresh = hgl_ai_proto::build::new_build_id();
    let job2 = serde_json::from_str::<hgl_ai_proto::build::BuildJob>(
        &serde_json::to_string(&job).unwrap().replace(&id, &fresh),
    )
    .unwrap();
    hgl_ai_build::request::send(&b, Seat::new("coord").unwrap(), &parked, &job2)
        .await
        .unwrap();
    let new = hgl_ai_build::store::run_get(&b, &fresh)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new.status, hgl_ai_proto::build::BuildStatus::Queued);
}

/// Review (r6 at b0cca78e, P2): refused credentials while polling an accepted merge reach
/// the round, the intent kept for a later attempt.
#[tokio::test]
async fn a_401_while_polling_the_merge_reaches_the_round() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, infos) = ready_round();
    let pending =
        "{\"status\":\"pending\",\"details\":{\"uuid\":\"u-1\",\"merge_method\":\"merge\"}}";
    let gh = coord_gh(&[], |g| g);
    first_rule(
        &gh,
        FakeGh::default().fail("merge-async/u-1", "gh: Bad credentials (HTTP 401)"),
    );
    first_rule(
        &gh,
        FakeGh::default().on("pulls/7/merge-async --input", pending),
    );
    let mut a = crate::autoland::AutoLand::new(&s);
    let e = a.round(&gh, &s, &b, &infos).await.unwrap_err();
    assert!(matches!(e, Error::Auth(_)), "{e}");
    let rec = crate::progress::read(&b, "alpha", 7)
        .await
        .unwrap()
        .expect("the intent");
    assert_eq!((rec.head.as_str(), rec.merge.as_deref()), (HEAD, None));
}

/// Audit (r6 at b0cca78e): refused credentials on the gate's branch update reach the
/// caller, rather than read as this pull request's block. Only a landing without a local
/// build verdict updates a branch (one with a verdict merges onto the `main` it was built
/// on), so it is the controller's `merge-if-clean` here.
#[tokio::test]
async fn a_401_on_the_branch_update_reaches_the_caller() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    authored_and_reviewed(&b).await;
    let (mut s, _) = ready_round();
    s.gates.entry("alpha".into()).or_default().verdicts.clear();
    let gh = coord_gh(&[], |g| g);
    first_rule(
        &gh,
        FakeGh::default().on(
            "compare/",
            "{\"ahead_by\":1,\"files\":[{\"filename\":\"src/both.rs\"}]}",
        ),
    );
    first_rule(
        &gh,
        FakeGh::default().fail("update-branch", "gh: Bad credentials (HTTP 401)"),
    );
    let e = land::merge_once(&gh, &s, &b, "alpha", 7, HEAD, &land::LandOpts::default())
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Auth(_)), "{e}");
    assert!(
        gh.calls().iter().any(|c| c.contains("update-branch")),
        "{:?}",
        gh.calls()
    );
}

/// What GitHub shows for the wrapper's pins current at `cccccccc` (the compiler's `main`)
/// and pin pull request `n` merged at `merge`.
fn pins_current_gh(n: u64, merge: &str) -> FakeGh {
    let sha = |c: char| c.to_string().repeat(40);
    let sub = |c: char| format!("{{\"type\":\"submodule\",\"sha\":\"{}\"}}", sha(c));
    let pr = fixture("pr-merged.json")
        .replace("\"number\": 7", &format!("\"number\": {n}"))
        .replace(MERGE, merge)
        .replace(HEAD, &sha('d'))
        .replace("codex/980-format-patterns", "codex/pins-compiler-cccccccc");
    FakeGh::default()
        .on(&format!("hgl-project/pulls/{n}"), &pr)
        .on("hgl-compiler/pulls/7", &fixture("pr-merged.json"))
        .on(
            "git/ref/heads/main",
            &format!("{{\"object\":{{\"sha\":\"{}\"}}}}", sha('c')),
        )
        .on("hgl-compiler/contents/spec/hgl_spec", &sub('5'))
        .on("hgl-compiler/contents/spec/hgl_core", &sub('6'))
        .on("hgl-project/contents/hgl-compiler", &sub('c'))
        .on("hgl-project/contents/hgl_spec", &sub('5'))
        .on("hgl-project/contents/hgl_core", &sub('6'))
}

/// hgl-ai#133: a compiler landing raised the pins, and the pin pull request's landing
/// stopped after its merge (its record holds the merge, nothing done); the compiler's
/// landing is `PARTIAL`. Its retry finds the pins current, and finishes the pin pull
/// request's landing (`landed.log`, `main`, the record) before its own, rather than delete
/// its own record and leave the pin landing's behind.
#[tokio::test]
async fn a_pin_landing_cut_off_after_its_merge_is_finished_by_the_retry() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let mut s = settings();
    s.repos = vec!["hgl-compiler".into()];
    let pin_merge = "02e99ea5e46457bac035ca8a2699b6b6575b32ed";
    let pin_head = "d".repeat(40);
    // The compiler's landing: every step under the lock done, the pins left.
    let mut src = crate::progress::Progress::new(HEAD, Some(MERGE.into()));
    for step in [
        crate::progress::LANDED_LOG,
        crate::progress::MAIN,
        crate::progress::NOTES,
        crate::progress::COMMENT,
    ] {
        src.mark(step);
    }
    crate::progress::write(&b, "hgl-compiler", 7, &src)
        .await
        .unwrap();
    // The pin pull request's: merged, its `landed.log` write failed.
    let pin = crate::progress::Progress::new(&pin_head, Some(pin_merge.into()));
    crate::progress::write(&b, "hgl-project", 50, &pin)
        .await
        .unwrap();

    let gh = pins_current_gh(50, pin_merge);
    let o = land::LandOpts::default();
    let l = land::land(&gh, &s, &b, "hgl-compiler", 7, HEAD, &o)
        .await
        .unwrap();
    assert_eq!(
        l.outcome.kind,
        crate::gate::Kind::Resumed,
        "{}",
        l.outcome.line
    );
    assert!(l.outcome.kind.satisfied(), "exit 0");
    assert!(
        l.after
            .iter()
            .any(|a| a.starts_with("hgl-project #50 was merged at 02e99ea5")),
        "{:?}",
        l.after
    );
    let log = b.state_get(state::KEY_LANDED).await.unwrap().unwrap().value;
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(
        log.trim_end()
            .ends_with(&format!("hgl-project #50 {pin_merge}")),
        "{log}"
    );
    let main = b.state_get(state::KEY_MAIN).await.unwrap().unwrap().value;
    assert!(
        main.contains(&format!("hgl-project pins hgl-compiler {}", "c".repeat(40))),
        "{main}"
    );
    for (repo, n) in [("hgl-compiler", 7), ("hgl-project", 50)] {
        assert!(
            crate::progress::read(&b, repo, n).await.unwrap().is_none(),
            "{repo} #{n} finished"
        );
    }
    assert_eq!(posts(&gh, "merge-async"), 0, "nothing merged");

    // Only the intent was recorded (the merge commit's write failed), and GitHub shows it
    // merged: `gh pins sync` finishes it too.
    let pin = crate::progress::Progress::new(&pin_head, None);
    crate::progress::write(&b, "hgl-project", 51, &pin)
        .await
        .unwrap();
    // An intent whose pull request is not merged is the gate's, not resumed here.
    crate::progress::write(&b, "hgl-project", 52, &pin)
        .await
        .unwrap();
    let open = fixture("pr-open.json")
        .replace("\"number\": 7", "\"number\": 52")
        .replace("codex/980-format-patterns", "codex/pins-compiler-cccccccc");
    // A wrapper pull request that is not a pin one, merged, its notes and comment left:
    // its own command finishes it, with them; the sync does not.
    let mut other = crate::progress::Progress::new(HEAD, Some(MERGE.into()));
    other.mark(crate::progress::LANDED_LOG);
    other.mark(crate::progress::MAIN);
    crate::progress::write(&b, "hgl-project", 53, &other)
        .await
        .unwrap();
    let merged = fixture("pr-merged.json").replace("\"number\": 7", "\"number\": 53");
    let gh = FakeGh::default()
        .on("hgl-project/pulls/52", &open)
        .on("hgl-project/pulls/53", &merged)
        .on("auth status", "");
    let gh = {
        let more = pins_current_gh(51, pin_merge);
        let r = std::mem::take(&mut *more.rules.lock().unwrap());
        gh.rules.lock().unwrap().extend(r);
        gh
    };
    let r = crate::pins::sync(&gh, &s, Some(&b), &crate::pins::PinOpts::default(), false)
        .await
        .unwrap();
    assert!(
        matches!(&r, crate::pins::Synced::Landed(o) if o.line.starts_with("hgl-project #51 is merged at 02e99ea5")),
        "{r:?}"
    );
    let log = b.state_get(state::KEY_LANDED).await.unwrap().unwrap().value;
    assert!(
        log.trim_end()
            .ends_with(&format!("hgl-project #51 {pin_merge}")),
        "{log}"
    );
    assert!(
        crate::progress::read(&b, "hgl-project", 51)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        crate::progress::read(&b, "hgl-project", 52)
            .await
            .unwrap()
            .is_some(),
        "not merged: left"
    );
    assert_eq!(
        crate::progress::read(&b, "hgl-project", 53)
            .await
            .unwrap()
            .map(|p| p.done),
        Some(vec![
            crate::progress::LANDED_LOG.to_string(),
            crate::progress::MAIN.to_string()
        ]),
        "not a pin pull request: left"
    );
    assert!(!log.contains("hgl-project #53"), "{log}");
    assert_eq!(posts(&gh, "merge-async"), 0);
    // Current, with nothing left to finish.
    for n in [52, 53] {
        crate::progress::delete(&b, "hgl-project", n).await.unwrap();
    }
    let r = crate::pins::sync(&gh, &s, Some(&b), &crate::pins::PinOpts::default(), false)
        .await
        .unwrap();
    assert!(
        matches!(&r, crate::pins::Synced::Current(l) if l == "pins current at cccccccc"),
        "{r:?}"
    );
}

/// Audit (r6 at b0cca78e, with #133): a rate limit on a step after the merge is a
/// `PARTIAL`, reported once; it still reaches the round, which stops (the hold follows).
#[tokio::test]
async fn a_rate_limit_after_the_merge_is_reported_and_stops_the_round() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, infos) = ready_round();
    let gh = coord_gh(&[], |g| g);
    first_rule(
        &gh,
        FakeGh::default().fail(
            "issues/7/comments -f body",
            "gh: API rate limit exceeded (HTTP 403)",
        ),
    );
    let mut a = crate::autoland::AutoLand::new(&s);
    let e = a.round(&gh, &s, &b, &infos).await.unwrap_err();
    assert!(matches!(e, Error::RateLimited(_)), "{e}");
    assert_eq!(posts(&gh, "pulls/7/merge-async"), 1, "merged");
    let (sent, _) = coord_reports(&b).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].starts_with("[coord] Partial:"), "{}", sent[0]);
    let rec = crate::progress::read(&b, "alpha", 7)
        .await
        .unwrap()
        .unwrap();
    assert!(!rec.has(crate::progress::COMMENT), "{rec:?}");
}

/// r6 at f75a442f, P1 (probe hgl-project #60): a pin pull request merged into another
/// base is not a landed pin. The sync does not take it for updated, and its landing
/// record is kept rather than deleted.
#[tokio::test]
async fn a_pin_pull_request_merged_into_another_base_is_not_landed() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let mut s = settings();
    s.repos = vec!["hgl-compiler".into()];
    let pin_merge = "02e99ea5e46457bac035ca8a2699b6b6575b32ed";
    let head = "d".repeat(40);
    let stale = format!("{{\"type\":\"submodule\",\"sha\":\"{}\"}}", "b".repeat(40));
    let open = format!("[{{\"number\":60,\"head\":{{\"sha\":\"{head}\"}}}}]");
    let gh = FakeGh::default()
        .on("pulls?state=open", &open)
        .on("hgl-project/contents/hgl-compiler?ref=main", &stale);
    let more = pins_current_gh(60, pin_merge);
    let r = std::mem::take(&mut *more.rules.lock().unwrap());
    gh.rules.lock().unwrap().extend(r);
    for r in gh
        .rules
        .lock()
        .unwrap()
        .iter_mut()
        .filter(|r| r.0 == "hgl-project/pulls/60")
    {
        assert!(
            r.1.stdout.contains("\"ref\": \"main\""),
            "the fixture's form"
        );
        r.1.stdout =
            r.1.stdout
                .replace("\"ref\": \"main\"", "\"ref\": \"release\"");
    }
    let intent = crate::progress::Progress::new(&head, None);
    store::put_latest(
        &b,
        &crate::progress::key("hgl-project", 60),
        &serde_json::to_string(&intent).unwrap(),
    )
    .await
    .unwrap();
    let r = crate::pins::sync(&gh, &s, Some(&b), &crate::pins::PinOpts::default(), false)
        .await
        .unwrap();
    assert!(!r.ok(), "{r:?}");
    assert!(
        matches!(&r, crate::pins::Synced::NotLanded(o) if o.line.contains("not main")),
        "{r:?}"
    );
    assert!(
        crate::progress::read(&b, "hgl-project", 60)
            .await
            .unwrap()
            .is_some(),
        "the record is kept"
    );
    assert!(b.state_get(state::KEY_LANDED).await.unwrap().is_none());
}

/// `gh` that, at the first call matching `trigger`, publishes a new revision of `alpha`'s
/// recipe (another step), as an owner may while a gate runs.
struct RecipeRace {
    gh: FakeGh,
    bus: Bus,
    trigger: &'static str,
    raced: std::sync::atomic::AtomicBool,
}

impl crate::exec::Gh for RecipeRace {
    async fn exec(
        &self,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> crate::Result<crate::exec::Output> {
        if args.join(" ").contains(self.trigger)
            && !self.raced.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let more = format!(
                "{}[[step]]\nname = \"more\"\nrun = \"true\"\nmemcap_gb = 1\ntime = \"1m\"\n",
                GATE_RECIPE.replace("demo", "alpha")
            );
            hgl_ai_build::store::recipe_put(&self.bus, &more, None)
                .await
                .unwrap();
        }
        self.gh.exec(args, stdin).await
    }
}

/// r6 at f75a442f, P1 (BUILD-15): a recipe published while the gate reads the checks binds
/// new verdicts. The verdict judged is for the old revision: no merge, tried again.
#[tokio::test]
async fn a_recipe_published_during_the_gate_stops_the_merge() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, infos) = ready_round();
    let gh = RecipeRace {
        gh: coord_gh(&[], |g| g),
        bus: bus(&h).await,
        trigger: "check-runs",
        raced: Default::default(),
    };
    let mut a = crate::autoland::AutoLand::new(&s);
    a.round(&gh, &s, &b, &infos).await.unwrap();
    assert!(
        gh.raced.load(std::sync::atomic::Ordering::SeqCst),
        "the race ran"
    );
    assert_eq!(posts(&gh.gh, "pulls/7/merge-async"), 0, "not merged");
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(coord_reports(&b).await, (vec![], vec![]));
}

/// r6 at f75a442f, P2: the merge request refused outright (HTTP 401): nothing can merge
/// from it, so its intent goes, and a controller with working credentials is not stopped
/// by it.
#[tokio::test]
async fn a_merge_request_refused_outright_leaves_no_intent() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, infos) = ready_round();
    let gh = coord_gh(&[], |g| g);
    first_rule(
        &gh,
        FakeGh::default().fail(
            "pulls/7/merge-async --input",
            "gh: Bad credentials (HTTP 401)",
        ),
    );
    let mut a = crate::autoland::AutoLand::new(&s);
    let e = a.round(&gh, &s, &b, &infos).await.unwrap_err();
    assert!(matches!(e, Error::Auth(_)), "{e}");
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none(),
        "no intent"
    );
    let gh = coord_gh(&[], |g| g);
    let l = land::merge_once(&gh, &s, &b, "alpha", 7, HEAD, &land::LandOpts::default())
        .await
        .unwrap();
    assert_eq!(
        l.outcome.kind,
        crate::gate::Kind::Merged,
        "{}",
        l.outcome.line
    );
}

/// r6 at f75a442f, P2: with `coordinator_lands` turned off, the coordinator still finishes
/// a confirmed landing of its own (its reports, the record), and retires an intent whose
/// pull request is still open there: no new merge.
#[tokio::test]
async fn opting_out_leaves_no_landing_without_an_owner() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, infos) = ready_round();
    let off = crate::testutil::with_coordinator_lands(s, &["alpha"], false);
    put_record(&b, &landed_record()).await;
    let gh = coord_gh(&[], |g| g);
    let mut a = crate::autoland::AutoLand::new(&off);
    a.round(&gh, &off, &b, &[]).await.unwrap();
    let (sent, decisions) = coord_reports(&b).await;
    assert_eq!(
        (sent.len(), decisions.len()),
        (1, 1),
        "{sent:?} {decisions:?}"
    );
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none()
    );

    put_record(&b, &coord_record(None)).await;
    let gh = FakeGh::default().on("pulls/7", &fixture("pr-open.json"));
    let mut a = crate::autoland::AutoLand::new(&off);
    a.round(&gh, &off, &b, &infos).await.unwrap();
    assert_eq!(posts(&gh, "merge-async"), 0, "no new merge");
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_none(),
        "retired"
    );
}

/// r6 at f75a442f, P2: while `hold` holds, `gh land` and `gh merge-if-clean` neither read
/// GitHub about another's intent nor retire it.
#[tokio::test]
async fn a_hold_defers_reading_another_intent() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    coord_hub(&b, true).await;
    let (s, _) = ready_round();
    put_record(&b, &coord_record(None)).await;
    store::put_latest(&b, state::KEY_HOLD, "2100-01-01T00:00:00Z")
        .await
        .unwrap();
    let moved = fixture("pr-open.json").replace(HEAD, &"e".repeat(40));
    let gh = FakeGh::default().on("pulls/7", &moved);
    let l = land::merge_once(&gh, &s, &b, "alpha", 7, HEAD, &land::LandOpts::default())
        .await
        .unwrap();
    assert_eq!(
        l.outcome.kind,
        crate::gate::Kind::Hold,
        "{}",
        l.outcome.line
    );
    let o = land::LandOpts {
        timeout: Duration::from_millis(50),
        ..land::LandOpts::default()
    };
    let l = land::land(&gh, &s, &b, "alpha", 7, HEAD, &o).await.unwrap();
    assert_eq!(
        l.outcome.kind,
        crate::gate::Kind::Hold,
        "{}",
        l.outcome.line
    );
    assert!(gh.calls().is_empty(), "{:?}", gh.calls());
    assert!(
        crate::progress::read(&b, "alpha", 7)
            .await
            .unwrap()
            .is_some(),
        "kept"
    );
}

/// `tasks` with no task is never written empty (an empty create fails every round), and
/// a text left from a task that is gone is cleared.
#[tokio::test]
async fn tasks_without_a_task_write_nothing() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let mut idx = crate::tasks::NotesIndex::default();
    let mut written = None;
    for _ in 0..2 {
        crate::tasks::keep(&b, &mut idx, &mut written)
            .await
            .unwrap();
    }
    assert!(b.state_get(state::KEY_TASKS).await.unwrap().is_none());

    let mut n = hgl_ai_proto::Note::new(
        Seat::new("controller").unwrap(),
        Seat::new("w1").unwrap(),
        Kind::Assign,
        "x",
    );
    n.repo = Some("alpha".into());
    n.issue = 7;
    b.send(&n).await.unwrap();
    let tree: hgl_ai_proto::TreeRecord = serde_json::from_value(serde_json::json!({
        "host": "lab1", "seat": "w1", "task": {"repo": "alpha", "issue": 7},
        "path": "/w", "created_at": "20261128T000000Z",
    }))
    .unwrap();
    b.tree_put(&tree).await.unwrap();
    crate::tasks::keep(&b, &mut idx, &mut written)
        .await
        .unwrap();
    let held = b.state_get(state::KEY_TASKS).await.unwrap().unwrap().value;
    assert!(
        held.starts_with("alpha\t7\tassign\tcontroller\tw1\t"),
        "{held}"
    );

    b.tree_del(&tree.key()).await.unwrap();
    crate::tasks::keep(&b, &mut idx, &mut written)
        .await
        .unwrap();
    assert!(b.state_get(state::KEY_TASKS).await.unwrap().is_none());
    // A new run (nothing written yet) finds nothing to clear.
    let mut written = None;
    crate::tasks::keep(&b, &mut idx, &mut written)
        .await
        .unwrap();
    assert!(b.state_get(state::KEY_TASKS).await.unwrap().is_none());
}

/// `gh` for [`a_pin_landing_finished_by_another_sync_is_done`]: at the first call matching
/// `trigger`, another sync finishes the landing of the pin pull request `n` (its
/// `landed.log` line, its record gone).
struct Racing {
    gh: FakeGh,
    bus: Bus,
    merge: String,
    trigger: &'static str,
    n: u64,
    raced: std::sync::atomic::AtomicBool,
}

impl Racing {
    fn new(gh: FakeGh, bus: Bus, merge: &str, trigger: &'static str, n: u64) -> Racing {
        Racing {
            gh,
            bus,
            merge: merge.into(),
            trigger,
            n,
            raced: Default::default(),
        }
    }
}

/// A fresh `prs` table whose row for the wrapper's pull request `n` says the review bot is
/// at its usage limit: the gate's table check would stop a landing there (BOTLIMIT).
async fn bot_limit_row(b: &Bus, n: u64) {
    let text = format!(
        "# read {}\nhgl-project\t{n}\t{}\topen\tlimit\t0\tpass\t{}\n",
        hgl_ai_proto::utc_now_stamp(),
        "d".repeat(40),
        hgl_ai_proto::utc_now_stamp()
    );
    store::put_latest(b, state::KEY_PRS, &text).await.unwrap();
}

impl crate::exec::Gh for Racing {
    async fn exec(
        &self,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> crate::Result<crate::exec::Output> {
        if args.join(" ").contains(self.trigger)
            && !self.raced.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let line = format!("20261128T000000Z hgl-project #{} {}", self.n, self.merge);
            store::append_bounded(&self.bus, state::KEY_LANDED, &line, 100)
                .await
                .unwrap();
            crate::progress::delete(&self.bus, "hgl-project", self.n)
                .await
                .unwrap();
        }
        self.gh.exec(args, stdin).await
    }
}

/// hgl-ai#133: two syncs pick the same unfinished pin landing, and the other finishes it
/// first. This one finds it landed: done, not `NotLanded`, and the plan is still read. It
/// finds that before the gate, whose table says BOTLIMIT for the pull request (r6).
/// A dry run reads the hub too: it names an unfinished pin landing, rather than say the
/// pins are current.
#[tokio::test]
async fn a_pin_landing_finished_by_another_sync_is_done() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let mut s = settings();
    s.repos = vec!["hgl-compiler".into()];
    let pin_merge = "02e99ea5e46457bac035ca8a2699b6b6575b32ed";
    let pin = crate::progress::Progress::new(&"d".repeat(40), Some(pin_merge.into()));
    crate::progress::write(&b, "hgl-project", 50, &pin)
        .await
        .unwrap();

    let dry = crate::exec::ReadOnly(pins_current_gh(50, pin_merge));
    let r = crate::pins::sync(&dry, &s, Some(&b), &crate::pins::PinOpts::default(), true)
        .await
        .unwrap();
    assert!(
        matches!(&r, crate::pins::Synced::Would(l) if l == "would finish the landing of the pin pull request hgl-project#50"),
        "{r:?}"
    );
    assert!(
        crate::progress::read(&b, "hgl-project", 50)
            .await
            .unwrap()
            .is_some()
    );

    bot_limit_row(&b, 50).await;
    let gh = Racing::new(
        pins_current_gh(50, pin_merge),
        bus(&h).await,
        pin_merge,
        "hgl-project/pulls/50",
        50,
    );
    let r = crate::pins::sync(&gh, &s, Some(&b), &crate::pins::PinOpts::default(), false)
        .await
        .unwrap();
    assert!(r.ok(), "{r:?}");
    assert!(
        matches!(&r, crate::pins::Synced::Current(l) if l == "pins current at cccccccc"),
        "{r:?}"
    );
    let log = b.state_get(state::KEY_LANDED).await.unwrap().unwrap().value;
    assert_eq!(log.lines().count(), 1, "landed once: {log}");
    assert!(
        b.state_get(state::KEY_MAIN).await.unwrap().is_none(),
        "not finished again"
    );
}

/// r6 on #133: a sync finds an open pin pull request, and another sync lands it while this
/// one waits for the lock. This one finds it landed in `landed.log` before the gate (whose
/// table says BOTLIMIT): done, not `NotLanded` with "already MERGED".
#[tokio::test]
async fn an_open_pin_pull_request_landed_by_another_sync_is_done() {
    let Some(h) = hub() else { return };
    let b = bus(&h).await;
    let mut s = settings();
    s.repos = vec!["hgl-compiler".into()];
    let pin_merge = "02e99ea5e46457bac035ca8a2699b6b6575b32ed";
    let head = "d".repeat(40);
    bot_limit_row(&b, 60).await;
    let stale = format!("{{\"type\":\"submodule\",\"sha\":\"{}\"}}", "b".repeat(40));
    let open = format!("[{{\"number\":60,\"head\":{{\"sha\":\"{head}\"}}}}]");
    // First match wins: the wrapper's main holds an old compiler pin; the open pull
    // request's head holds the wanted ones.
    let gh = FakeGh::default()
        .on("pulls?state=open", &open)
        .on("hgl-project/contents/hgl-compiler?ref=main", &stale);
    let more = pins_current_gh(60, pin_merge);
    let r = std::mem::take(&mut *more.rules.lock().unwrap());
    gh.rules.lock().unwrap().extend(r);
    let gh = Racing::new(gh, bus(&h).await, pin_merge, "pulls?state=open", 60);
    let r = crate::pins::sync(&gh, &s, Some(&b), &crate::pins::PinOpts::default(), false)
        .await
        .unwrap();
    assert!(r.ok(), "{r:?}");
    assert!(
        matches!(&r, crate::pins::Synced::Landed(o) if o.kind == crate::gate::Kind::AlreadyMerged),
        "{r:?}"
    );
    assert_eq!(posts(&gh.gh, "merge-async"), 0, "not merged again");
    let log = b.state_get(state::KEY_LANDED).await.unwrap().unwrap().value;
    assert_eq!(log.lines().count(), 1, "landed once: {log}");
}
