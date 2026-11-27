//! Live tests against a real `nats-server -js` on a free loopback port with a temporary
//! store; GitHub is scripted. Skipped, with the reason printed, when `nats-server` is not on
//! `PATH`.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use hgl_ai_bus::{Bus, ProvisionOptions};
use hgl_ai_proto::{Kind, Seat};

use crate::coord::{self, CoordOpts};
use crate::exec::fake::FakeGh;
use crate::lock::LandLock;
use crate::table::Table;
use crate::testutil::{fixture, settings};
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
    let controller = Seat::new("claude").unwrap();
    let mut inbox = b.inbox(&controller).await.unwrap();

    let gh = FakeGh::default()
        .on("auth status", "")
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

    // The allowance runs low: the round writes `hold`, the next round does not run.
    let low = fixture("coord-batch.json").replace("\"remaining\": 4945", "\"remaining\": 40");
    let gh = FakeGh::default().on("auth status", "").on("graphql", &low);
    coord::run(&gh, &s, Some(&b), &o, &mut Vec::new())
        .await
        .unwrap();
    assert_eq!(
        b.state_get(state::KEY_HOLD).await.unwrap().unwrap().value,
        "2026-01-05T12:55:49Z"
    );
    store::put_latest(&b, state::KEY_HOLD, "2100-01-01T00:00:00Z")
        .await
        .unwrap();
    let gh = FakeGh::default().on("auth status", "").on("graphql", &low);
    coord::run(&gh, &s, Some(&b), &o, &mut Vec::new())
        .await
        .unwrap();
    assert!(
        !gh.calls().iter().any(|c| c.contains("graphql")),
        "no round under a hold"
    );
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
    let s = settings();
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
    let r = land::after_merge(&gh, &s, &a, "alpha", 7, &merged, &o, &held).await;
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
