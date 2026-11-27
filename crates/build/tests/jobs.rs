//! What a builder does with a `build` job it must not run as asked, against a live
//! `nats-server -js` (skipped without one) and a local lane: a malformed id, a reply or
//! notify address that may not get receipts, and an id already used for another request.
//! Each is terminated before it touches a path or the hub, and the lane goes on.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hgl_ai_build::service::{self, Settings};
use hgl_ai_build::store;
use hgl_ai_bus::Bus;
use hgl_ai_proto::build::{BUILD_VERSION, BuildJob, BuildStatus, reply_seat};
use hgl_ai_proto::{Kind, Note, Seat};

struct Nats(Child, #[allow(dead_code)] tempfile::TempDir, String, String);

impl Drop for Nats {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn nats() -> Option<Nats> {
    let bin = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("nats-server"))
        .find(|p| p.is_file());
    let Some(bin) = bin else {
        eprintln!("skipped: nats-server is not on PATH");
        return None;
    };
    let dir = tempfile::Builder::new().prefix("hbj").tempdir().unwrap();
    let token = format!("jobs{}", std::process::id());
    let child = Command::new(bin)
        .args(["-js", "-a", "127.0.0.1", "-p", "-1", "--auth", &token])
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
    let deadline = Instant::now() + Duration::from_secs(15);
    let url = loop {
        if let Some(u) = std::fs::read_to_string(&ports)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["nats"][0].as_str().map(String::from))
        {
            break u;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    };
    Some(Nats(child, dir, url, token))
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
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// A hub, an origin with two commits, and a running local builder `lab1`.
struct Rig {
    _nats: Nats,
    _tmp: tempfile::TempDir,
    root: PathBuf,
    bus: Arc<Bus>,
    heads: [String; 2],
    stop: tokio::sync::watch::Sender<bool>,
    svc: Option<tokio::task::JoinHandle<Result<(), service::ServiceError>>>,
    cfg: hgl_ai_config::Config,
    settings: Settings,
    /// Cuts the service's connection to the hub (a partition).
    cut: Option<Arc<std::sync::atomic::AtomicBool>>,
}

type Before =
    Box<dyn Fn(&Arc<Bus>) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>;

#[derive(Default)]
struct Opts {
    locks_max_age: Option<Duration>,
    proxy: bool,
    remote: bool,
    /// Runs on the hub before the service starts.
    before: Option<Before>,
}

fn fake_ssh() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake-ssh")
}

/// A TCP proxy to the hub at `url`; setting the flag drops everything both ways while the
/// connections stay open, as a partition does.
async fn tcp_proxy(url: &str) -> (String, Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let target = url.trim_start_matches("nats://").to_string();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let cut = Arc::new(AtomicBool::new(false));
    let c = cut.clone();
    tokio::spawn(async move {
        while let Ok((inbound, _)) = l.accept().await {
            let Ok(outbound) = tokio::net::TcpStream::connect(&target).await else {
                continue;
            };
            let (ri, wi) = inbound.into_split();
            let (ro, wo) = outbound.into_split();
            for (mut r, mut w) in [
                (
                    Box::new(ri) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
                    Box::new(wo) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
                ),
                (
                    Box::new(ro) as Box<dyn tokio::io::AsyncRead + Unpin + Send>,
                    Box::new(wi) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
                ),
            ] {
                let c = c.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 64 << 10];
                    loop {
                        let n = match r.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        if !c.load(Ordering::SeqCst) && w.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        }
    });
    (format!("nats://127.0.0.1:{port}"), cut)
}

impl Rig {
    async fn start() -> Option<Rig> {
        Rig::start_with(Opts::default()).await
    }

    async fn start_with(o: Opts) -> Option<Rig> {
        let n = nats()?;
        let bus = Arc::new(Bus::connect_url(&n.2, Some(n.3.clone())).await.unwrap());
        bus.provision_with(hgl_ai_bus::ProvisionOptions {
            locks_max_age: o.locks_max_age.unwrap_or(hgl_ai_bus::LOCKS_MAX_AGE),
        })
        .await
        .unwrap();
        // The service's own connection, through a proxy the test can cut.
        let (svc_bus, cut) = match o.proxy {
            false => (bus.clone(), None),
            true => {
                let (url, cut) = tcp_proxy(&n.2).await;
                (
                    Arc::new(Bus::connect_url(&url, Some(n.3.clone())).await.unwrap()),
                    Some(cut),
                )
            }
        };
        let tmp = tempfile::Builder::new().prefix("hbr").tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let bare = root.join("origin.git");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--quiet", "--bare", "-b", "main"]);
        git(&root, &["clone", "--quiet", bare.to_str().unwrap(), "seed"]);
        let seed = root.join("seed");
        git(&seed, &["checkout", "--quiet", "-b", "main"]);
        let mut heads = Vec::new();
        for (i, text) in ["one\n", "two\n"].iter().enumerate() {
            std::fs::write(seed.join("f.txt"), text).unwrap();
            git(&seed, &["add", "f.txt"]);
            git(&seed, &["commit", "--quiet", "-m", &format!("c{i}")]);
            heads.push(git(&seed, &["rev-parse", "HEAD"]));
        }
        git(&seed, &["push", "--quiet", "origin", "main"]);
        let recipe = "repo = \"demo\"\n[[step]]\nname = \"check\"\nrun = \"test -f f.txt\"\nmemcap_gb = 1\ntime = \"2m\"\n";
        store::recipe_put(&bus, recipe, None).await.unwrap();
        let mut cfg = hgl_ai_config::Config::new("lab1");
        cfg.paths.work_root = root.join("work").display().to_string();
        cfg.paths.data_dir = root.join("data").display().to_string();
        cfg.builder.toolchains = vec!["git".into()];
        if o.remote {
            let state = root.join("winbox");
            std::fs::create_dir_all(&state).unwrap();
            cfg.builder.remote.push(
                toml::from_str(&format!(
                    "platform = \"windows\"\nssh = \"{}\"\nwork_dir = \"{}\"\nshell = \"sh\"\nlabel = \"win1\"\n",
                    state.display(),
                    root.join("remote").display()
                ))
                .unwrap(),
            );
        }
        if let Some(f) = &o.before {
            f(&bus).await;
        }
        let lanes = service::lanes(&cfg, fake_ssh()).unwrap();
        let origin = bare.display().to_string();
        let settings = Settings {
            clone_url: Arc::new(move |_| origin.clone()),
            toolchains: vec!["git".into()],
            heartbeat: Duration::from_millis(300),
        };
        let (stop, rx) = tokio::sync::watch::channel(false);
        let svc = tokio::spawn(service::serve(svc_bus, lanes, settings.clone(), rx));
        Some(Rig {
            cut,
            cfg,
            settings,
            _nats: n,
            _tmp: tmp,
            root,
            bus,
            heads: [heads[0].clone(), heads[1].clone()],
            stop,
            svc: Some(svc),
        })
    }

    fn job(&self, id: &str, head: &str) -> BuildJob {
        BuildJob {
            v: BUILD_VERSION,
            id: id.into(),
            repo: "demo".into(),
            head: head.into(),
            base: None,
            issue: 0,
            steps: None,
            recipe_rev: None,
            keep_going: false,
            reply_to: reply_seat(id).unwrap_or_else(|_| Seat::new("r-x").unwrap()),
            notify: None,
            requester: "w1".into(),
        }
    }

    async fn send(&self, j: &BuildJob) {
        let mut n = Note::new(
            Seat::new("w1").unwrap(),
            Seat::new("builder-lab1").unwrap(),
            Kind::Build,
            serde_json::to_string(j).unwrap(),
        );
        n.repo = Some(j.repo.clone());
        n.head = Some(j.head.clone());
        self.bus.send(&n).await.unwrap();
    }

    /// The receipt of a valid job, proving the lane still takes jobs.
    async fn receipt(&self, id: &str) -> hgl_ai_proto::build::Summary {
        let n = self
            .bus
            .reply_wait(&reply_seat(id).unwrap(), Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("no receipt for {id}: the lane is stuck"));
        store::parse_receipt(&n).unwrap()
    }

    /// Stream sequence of the last note to `seat`, if any.
    async fn last_seq(&self, seat: &str) -> Option<u64> {
        let client = async_nats::ConnectOptions::with_token(self._nats.3.clone())
            .connect(self._nats.2.as_str())
            .await
            .unwrap();
        let js = async_nats::jetstream::new(client);
        let stream = js.get_stream("HGL_NOTES").await.unwrap();
        stream
            .get_last_raw_message_by_subject(&format!("hgl.notes.{seat}"))
            .await
            .ok()
            .map(|m| m.sequence)
    }

    /// Another builder service with this one's configuration (a second `builder run`).
    fn second(
        &self,
    ) -> (
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<Result<(), service::ServiceError>>,
    ) {
        let lanes = service::lanes(&self.cfg, PathBuf::from("ssh")).unwrap();
        let (tx, rx) = tokio::sync::watch::channel(false);
        let h = tokio::spawn(service::serve(
            self.bus.clone(),
            lanes,
            self.settings.clone(),
            rx,
        ));
        (tx, h)
    }

    /// Stop the builder service; the hub stays.
    async fn halt(&mut self) {
        let _ = self.stop.send(true);
        if let Some(svc) = self.svc.take() {
            tokio::time::timeout(Duration::from_secs(60), svc)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }

    async fn stop(mut self) {
        self.halt().await;
    }
}

/// Finding 1: an id that is not a build id (here a path out of the log directory).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_id_is_refused_and_writes_nothing() {
    let Some(r) = Rig::start().await else { return };
    let bad = r.job("../../../../escaped", &r.heads[0]);
    r.send(&bad).await;
    let ok = r.job("20261127-110000-aaaaaa", &r.heads[0]);
    r.send(&ok).await;
    assert_eq!(r.receipt(&ok.id).await.status, BuildStatus::Pass);
    assert!(
        !r.root.join("escaped").exists(),
        "nothing written outside the log directory"
    );
    r.stop().await;
}

/// Finding 2: receipts go only to the build's own address and to a seat that may get them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reply_and_notify_must_be_receipt_seats() {
    let Some(r) = Rig::start().await else { return };
    let mut owner = r.job("20261127-110000-bbbbbb", &r.heads[0]);
    owner.reply_to = Seat::owner();
    r.send(&owner).await;
    let mut all = r.job("20261127-110000-cccccc", &r.heads[0]);
    all.reply_to = Seat::broadcast();
    r.send(&all).await;
    let mut to_builder = r.job("20261127-110000-dddddd", &r.heads[0]);
    to_builder.notify = Some(Seat::new("builder-lab2").unwrap());
    r.send(&to_builder).await;
    let ok = r.job("20261127-110000-eeeeee", &r.heads[0]);
    r.send(&ok).await;
    assert_eq!(r.receipt(&ok.id).await.status, BuildStatus::Pass);
    assert_eq!(r.last_seq("all").await, None, "no receipt broadcast");
    assert_eq!(r.last_seq("builder-lab2").await, None);
    for id in [&owner.id, &all.id, &to_builder.id] {
        assert!(
            store::run_get(&r.bus, id).await.unwrap().is_none(),
            "{id} was not run"
        );
    }
    r.stop().await;
}

/// Finding 8: a finished id answers only the request it was built for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_id_is_not_replayed_for_another_request() {
    let Some(r) = Rig::start().await else { return };
    let first = r.job("20261127-110000-ffffff", &r.heads[0]);
    r.send(&first).await;
    let s = r.receipt(&first.id).await;
    assert_eq!(s.head.as_deref(), Some(r.heads[0].as_str()));
    let seq = r.last_seq(&format!("r-{}", first.id)).await;
    // The same id for another head, a second later (a receipt then is a new note), with
    // a seat to notify: a replayed result would reach it.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let mut other = r.job(&first.id, &r.heads[1]);
    other.notify = Some(Seat::new("w5").unwrap());
    r.send(&other).await;
    let next = r.job("20261127-110000-abcdef", &r.heads[1]);
    r.send(&next).await;
    assert_eq!(r.receipt(&next.id).await.status, BuildStatus::Pass);
    assert_eq!(
        r.last_seq(&format!("r-{}", first.id)).await,
        seq,
        "no second receipt under the reused id"
    );
    assert_eq!(
        r.last_seq("w5").await,
        None,
        "no result for a request not built"
    );
    let kept = store::run_get(&r.bus, &first.id).await.unwrap().unwrap();
    assert_eq!(kept.head.as_deref(), Some(r.heads[0].as_str()));
    r.stop().await;
}

/// Finding 5: a second builder for the same label (a second `builder run`) refuses to
/// start, and its startup cleanup never touches the running build's checkout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_builder_per_label() {
    let Some(r) = Rig::start().await else { return };
    let slow = "repo = \"demo\"\n[[step]]\nname = \"check\"\nrun = \"sleep 3 && test -f f.txt\"\nmemcap_gb = 1\ntime = \"2m\"\n";
    store::recipe_put(&r.bus, slow, None).await.unwrap();
    let job = r.job("20261127-130000-aaaaaa", &r.heads[0]);
    r.send(&job).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let running = store::run_get(&r.bus, &job.id)
            .await
            .unwrap()
            .is_some_and(|s| s.steps.iter().any(|x| x.status.as_str() == "running"));
        if running {
            break;
        }
        assert!(Instant::now() < deadline, "the step did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (tx, second) = r.second();
    let ended = tokio::time::timeout(Duration::from_secs(15), second).await;
    let _ = tx.send(true);
    let err = match ended {
        Ok(Ok(Err(e))) => e.to_string(),
        other => panic!("the second builder did not refuse: {other:?}"),
    };
    assert!(err.contains("lab1"), "{err}");
    assert_eq!(r.receipt(&job.id).await.status, BuildStatus::Pass);
    r.stop().await;
}

/// Finding 5 across hosts: a lease another host holds keeps this one out; one a gone
/// builder of this host left is taken over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hub_lease_names_the_builder() {
    let Some(mut r) = Rig::start().await else {
        return;
    };
    r.halt().await;
    assert!(
        r.bus.lock_get("builder.lab1").await.unwrap().is_none(),
        "released at the end"
    );
    let other = r
        .bus
        .lock_acquire(
            "builder.lab1",
            "lab9 pid 4242 (builder) id 0011223344556677",
        )
        .await
        .unwrap();
    let (tx, svc) = r.second();
    let err = match tokio::time::timeout(Duration::from_secs(15), svc).await {
        Ok(Ok(Err(e))) => e.to_string(),
        other => panic!("served beside another host's builder: {other:?}"),
    };
    let _ = tx.send(true);
    assert!(err.contains("served by lab9 pid 4242 (builder)"), "{err}");
    assert!(other.release().await.unwrap());
    // A builder of this host that died left its lease: taken over.
    let id = hgl_ai_bridge::holder::host_id(&r.root.join("data"));
    let _gone = r
        .bus
        .lock_acquire(
            "builder.lab1",
            &format!("lab1 pid 999999 (builder) id {id}"),
        )
        .await
        .unwrap();
    let (tx, svc) = r.second();
    let ok = r.job("20261127-130000-bbbbbb", &r.heads[0]);
    r.send(&ok).await;
    assert_eq!(r.receipt(&ok.id).await.status, BuildStatus::Pass);
    let held = r.bus.lock_get("builder.lab1").await.unwrap().unwrap();
    assert!(
        held.holder.starts_with(&format!(
            "lab1 pid {} (builder) id {id}",
            std::process::id()
        )),
        "{held:?}"
    );
    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(60), svc)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    r.stop().await;
}

/// Carbon's second review, finding 2: a lane that loses its lease takes the process down
/// with an error, so the service manager restarts it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_lease_ends_the_service_with_an_error() {
    let Some(mut r) = Rig::start().await else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let held = loop {
        if let Some(h) = r.bus.lock_get("builder.lab1").await.unwrap() {
            break h.holder;
        }
        assert!(Instant::now() < deadline, "no lease");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let _thief = r
        .bus
        .lock_take_over("builder.lab1", "lab9 pid 1 (builder) id 00000000", &held)
        .await
        .unwrap();
    let svc = r.svc.take().unwrap();
    match tokio::time::timeout(Duration::from_secs(15), svc).await {
        Ok(Ok(Err(e))) => assert!(e.to_string().contains("lease"), "{e}"),
        other => panic!("the service went on, or ended clean: {other:?}"),
    }
}

/// Finding 2: one lane that cannot serve (its lease held elsewhere) ends the whole
/// service, the other lane with it, instead of leaving it dead beside a running one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lane_that_cannot_serve_ends_the_service() {
    let before: Before = Box::new(|bus: &Arc<Bus>| {
        let bus = bus.clone();
        Box::pin(async move {
            let l = bus
                .lock_acquire("builder.win1", "lab9 pid 1 (builder) id 00000000")
                .await
                .unwrap();
            std::mem::forget(l);
        })
    });
    let Some(mut r) = Rig::start_with(Opts {
        remote: true,
        before: Some(before),
        ..Opts::default()
    })
    .await
    else {
        return;
    };
    let svc = r.svc.take().unwrap();
    match tokio::time::timeout(Duration::from_secs(15), svc).await {
        Ok(Ok(Err(e))) => assert!(e.to_string().contains("win1"), "{e}"),
        other => panic!("the service went on with a dead lane: {other:?}"),
    }
}

/// Carbon's second review, finding 3: two hosts that share a label by mistake have
/// different host ids; neither takes the other's lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_label_on_another_host_is_not_taken_over() {
    let Some(r) = Rig::start().await else { return };
    let deadline = Instant::now() + Duration::from_secs(10);
    while r.bus.lock_get("builder.lab1").await.unwrap().is_none() {
        assert!(Instant::now() < deadline, "no lease");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Another machine: its own work root and data directory (so its own host id).
    let mut cfg = r.cfg.clone();
    cfg.paths.work_root = r.root.join("other-work").display().to_string();
    cfg.paths.data_dir = r.root.join("other-data").display().to_string();
    let lanes = service::lanes(&cfg, fake_ssh()).unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let svc = tokio::spawn(service::serve(r.bus.clone(), lanes, r.settings.clone(), rx));
    let ended = tokio::time::timeout(Duration::from_secs(15), svc).await;
    let _ = tx.send(true);
    match ended {
        Ok(Ok(Err(e))) => assert!(e.to_string().contains("served by"), "{e}"),
        other => panic!("took the other host's lease: {other:?}"),
    }
    let ok = r.job("20261127-140000-aaaaaa", &r.heads[0]);
    r.send(&ok).await;
    assert_eq!(
        r.receipt(&ok.id).await.status,
        BuildStatus::Pass,
        "the first serves on"
    );
    r.stop().await;
}

/// Finding 3: cut off from the hub, a builder stops before its lease can expire and
/// another take it: no refresh confirmed for half the bucket's `max_age` ends it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_partitioned_builder_stops_before_its_lease_expires() {
    let Some(mut r) = Rig::start_with(Opts {
        locks_max_age: Some(Duration::from_secs(4)),
        proxy: true,
        ..Opts::default()
    })
    .await
    else {
        return;
    };
    let slow = "repo = \"demo\"\n[[step]]\nname = \"check\"\nrun = \"sleep 30\"\nmemcap_gb = 1\ntime = \"2m\"\n";
    store::recipe_put(&r.bus, slow, None).await.unwrap();
    let job = r.job("20261127-140000-bbbbbb", &r.heads[0]);
    r.send(&job).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let running = store::run_get(&r.bus, &job.id)
            .await
            .unwrap()
            .is_some_and(|s| s.steps.iter().any(|x| x.status.as_str() == "running"));
        if running {
            break;
        }
        assert!(Instant::now() < deadline, "the step did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    r.cut
        .as_ref()
        .unwrap()
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let cut_at = Instant::now();
    let svc = r.svc.take().unwrap();
    match tokio::time::timeout(Duration::from_secs(25), svc).await {
        Ok(Ok(Err(e))) => assert!(e.to_string().contains("not refreshed"), "{e}"),
        other => panic!("a partitioned builder went on: {other:?}"),
    }
    assert!(
        cut_at.elapsed() < Duration::from_secs(25),
        "{:?}",
        cut_at.elapsed()
    );
}
