//! Live tests against a real `nats-server -js` on a free loopback port with a temporary
//! store; GitHub is scripted. Skipped, with the reason printed, when `nats-server` is not on
//! `PATH`.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use hgl_ai_bus::Bus;
use hgl_ai_proto::{Kind, Seat};

use crate::coord::{self, CoordOpts};
use crate::exec::fake::FakeGh;
use crate::table::Table;
use crate::testutil::{fixture, settings};
use crate::{land, state, store};

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
    land::record(&gh, &s, &b, "alpha", 7, merge).await.unwrap();
    land::record(&gh, &s, &b, "alpha", 8, merge).await.unwrap();
    let log = b.state_get(state::KEY_LANDED).await.unwrap().unwrap().value;
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[1].ends_with(&format!(" alpha #8 {merge}")), "{log}");
    let main = b.state_get(state::KEY_MAIN).await.unwrap().unwrap().value;
    assert!(main.starts_with("updated "), "{main}");
    assert!(main.contains("alpha main cccccccccccccccccccccccccccccccccccccccc\n"));
    assert!(main.contains("hgl-project pins hgl_spec 5555555555555555555555555555555555555555\n"));
}
