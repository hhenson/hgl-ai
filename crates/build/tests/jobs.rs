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

/// Runs on the hub (its bus, URL and token) before the service starts.
type Before = Box<
    dyn Fn(
        &Arc<Bus>,
        &str,
        &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
>;

#[derive(Default)]
struct Opts {
    locks_max_age: Option<Duration>,
    proxy: bool,
    remote: bool,
    /// Runs on the hub before the service starts.
    before: Option<Before>,
    /// The service's hub user may not write verdicts (a seat's).
    no_verdicts: bool,
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
            f(&bus, &n.2, &n.3).await;
        }
        let lanes = service::lanes(&cfg, fake_ssh()).unwrap();
        let origin = bare.display().to_string();
        let settings = Settings {
            clone_url: Arc::new(move |_| origin.clone()),
            toolchains: vec!["git".into()],
            heartbeat: Duration::from_millis(300),
            verdicts: !o.no_verdicts,
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
            verdict: None,
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
        store::parse_receipt(&n, id).unwrap()
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

/// A gate build of `head` merged onto `main` for pull request 7.
fn gate_job(r: &Rig, id: &str, head: &str, name: &str) -> BuildJob {
    let mut j = r.job(id, head);
    j.base = Some(r.heads[0].clone());
    j.issue = 7;
    j.verdict = Some(name.into());
    j
}

async fn send_pr(r: &Rig, j: &BuildJob) {
    let mut n = Note::new(
        Seat::new("w1").unwrap(),
        Seat::new("builder-lab1").unwrap(),
        Kind::Build,
        serde_json::to_string(j).unwrap(),
    );
    n.repo = Some(j.repo.clone());
    n.head = Some(j.head.clone());
    n.base = j.base.clone();
    n.issue = j.issue;
    r.bus.send(&n).await.unwrap();
}

/// hgl-ai#6: a builder writes only its own platform's verdict, binding the main it merged
/// onto; a verdict for another platform is refused before anything runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gate_build_writes_its_own_platforms_verdict() {
    let Some(r) = Rig::start().await else { return };
    let mine = hgl_ai_proto::build::Platform::current().unwrap().as_str();
    let other = if mine == "windows" {
        "linux"
    } else {
        "windows"
    };
    let foreign = gate_job(&r, "20261127-120000-aaaaaa", &r.heads[1], other);
    send_pr(&r, &foreign).await;
    let ok = gate_job(&r, "20261127-120000-bbbbbb", &r.heads[1], mine);
    send_pr(&r, &ok).await;
    let s = r.receipt(&ok.id).await;
    assert_eq!(s.status, BuildStatus::Pass);
    assert_eq!(s.base_sha.as_deref(), Some(r.heads[0].as_str()));
    let v = r
        .bus
        .state_get(&format!("verdict.demo.7.{mine}"))
        .await
        .unwrap()
        .unwrap()
        .value;
    assert_eq!(
        v,
        format!(
            "{} {} pass {} lab1 r{}",
            r.heads[1],
            r.heads[0],
            ok.id,
            s.recipe_rev.unwrap()
        )
    );
    assert!(
        r.bus
            .state_get(&format!("verdict.demo.7.{other}"))
            .await
            .unwrap()
            .is_none(),
        "no verdict for another platform"
    );
    assert!(store::run_get(&r.bus, &foreign.id).await.unwrap().is_none());
    r.stop().await;
}

/// hgl-ai#6 (r3 on #56, finding 2): a gate build asked earlier never replaces the verdict
/// of one asked later, whichever ends last; `running` is bounded; an id stamped in the
/// future is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_earlier_gate_build_never_replaces_a_later_ones_verdict() {
    let Some(r) = Rig::start().await else { return };
    let mine = hgl_ai_proto::build::Platform::current().unwrap().as_str();
    let key = format!("verdict.demo.7.{mine}");
    // The later request (a newer main) runs first.
    let mut later = gate_job(&r, "20261127-130000-bbbbbb", &r.heads[1], mine);
    later.base = Some(r.heads[1].clone());
    send_pr(&r, &later).await;
    assert_eq!(r.receipt(&later.id).await.status, BuildStatus::Pass);
    let held = r.bus.state_get(&key).await.unwrap().unwrap().value;
    assert!(held.contains(&later.id), "{held}");
    // The earlier request (main before it moved) ends after it: refused.
    let earlier = gate_job(&r, "20261127-120000-aaaaaa", &r.heads[1], mine);
    send_pr(&r, &earlier).await;
    assert_eq!(r.receipt(&earlier.id).await.status, BuildStatus::Pass);
    assert_eq!(
        r.bus.state_get(&key).await.unwrap().unwrap().value,
        held,
        "the later build's verdict stays"
    );
    // A slow gate build shows `running`, bounded by its steps' limits and a margin.
    let slow = "repo = \"demo\"\n[[step]]\nname = \"check\"\nrun = \"sleep 3 && test -f f.txt\"\nmemcap_gb = 1\ntime = \"2m\"\n";
    store::recipe_put(&r.bus, slow, None).await.unwrap();
    let now = gate_job(&r, &hgl_ai_proto::build::new_build_id(), &r.heads[1], mine);
    send_pr(&r, &now).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    let running = loop {
        let v = r.bus.state_get(&key).await.unwrap().unwrap().value;
        let p = hgl_ai_proto::build::VerdictValue::parse(&v).unwrap();
        if p.word == hgl_ai_proto::build::VerdictWord::Running {
            break p;
        }
        assert!(Instant::now() < deadline, "no running verdict: {v}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(running.id.as_deref(), Some(now.id.as_str()));
    let rev = store::recipe_get(&r.bus, "demo", None)
        .await
        .unwrap()
        .unwrap()
        .revision;
    assert_eq!(running.recipe, Some(rev), "it records the recipe it runs");
    let until = running.until.clone().unwrap();
    let in_16m = hgl_ai_proto::utc_stamp(chrono::Utc::now() + chrono::Duration::minutes(16));
    let in_18m = hgl_ai_proto::utc_stamp(chrono::Utc::now() + chrono::Duration::minutes(18));
    assert!(
        until.as_str() > in_16m.as_str() && until.as_str() <= in_18m.as_str(),
        "{until}"
    );
    assert_eq!(r.receipt(&now.id).await.status, BuildStatus::Pass);
    // An id stamped an hour ahead would hold the key against every real build.
    let ahead = (chrono::Utc::now() + chrono::Duration::hours(1)).format("%Y%m%d-%H%M%S-cccccc");
    let future = gate_job(&r, &ahead.to_string(), &r.heads[1], mine);
    send_pr(&r, &future).await;
    let ok = r.job("20261127-140000-dddddd", &r.heads[0]);
    r.send(&ok).await;
    assert_eq!(r.receipt(&ok.id).await.status, BuildStatus::Pass);
    assert!(store::run_get(&r.bus, &future.id).await.unwrap().is_none());
    assert!(
        !r.bus
            .state_get(&key)
            .await
            .unwrap()
            .unwrap()
            .value
            .contains("cccccc")
    );
    r.stop().await;
}

/// hgl-ai#6: a builder whose hub user is a seat's does not run gate builds: they end not
/// run, saying so, and no verdict passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_builder_without_a_builder_user_runs_no_gate_build() {
    let Some(r) = Rig::start_with(Opts {
        no_verdicts: true,
        ..Opts::default()
    })
    .await
    else {
        return;
    };
    let mine = hgl_ai_proto::build::Platform::current().unwrap().as_str();
    let j = gate_job(&r, "20261127-120000-cccccc", &r.heads[1], mine);
    send_pr(&r, &j).await;
    let s = r.receipt(&j.id).await;
    assert_eq!(s.status, BuildStatus::NotRun);
    assert!(s.reason.unwrap().contains("builder hub user"));
    assert!(s.steps.is_empty(), "nothing built");
    // It does not try to write a verdict its hub user may not write.
    assert!(
        r.bus
            .state_get(&format!("verdict.demo.7.{mine}"))
            .await
            .unwrap()
            .is_none()
    );
    r.stop().await;
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
    // The builder record is the new holder's to keep: the lane that lost the lease does
    // not delete it on its way out (Carbon's check of #12's round-two fixes, note 3).
    assert!(
        r.bus
            .builders()
            .await
            .unwrap()
            .iter()
            .any(|b| b.label == "lab1"),
        "the record was deleted by a lane no longer holding the lease"
    );
}

/// Note 3: a lane that fails to open its inbox gives its lease and tasks back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lane_that_cannot_open_its_inbox_leaves_nothing_behind() {
    let before: Before = Box::new(|_: &Arc<Bus>, url: &str, token: &str| {
        let (url, token) = (url.to_string(), token.to_string());
        Box::pin(async move {
            // No notes stream: the inbox cannot be opened.
            let c = async_nats::ConnectOptions::with_token(token)
                .connect(url.as_str())
                .await
                .unwrap();
            async_nats::jetstream::new(c)
                .delete_stream(hgl_ai_proto::STREAM_NOTES)
                .await
                .unwrap();
        })
    });
    let Some(mut r) = Rig::start_with(Opts {
        before: Some(before),
        ..Opts::default()
    })
    .await
    else {
        return;
    };
    let svc = r.svc.take().unwrap();
    match tokio::time::timeout(Duration::from_secs(15), svc).await {
        Ok(Ok(Err(_))) => {}
        other => panic!("expected the lane to fail: {other:?}"),
    }
    // Two refresh periods later: no lease kept alive, no record rewritten.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(
        r.bus.lock_get("builder.lab1").await.unwrap().is_none(),
        "the lease was kept"
    );
    r.bus.builder_del("lab1").await.unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(
        r.bus.builders().await.unwrap().is_empty(),
        "a heartbeat still writes the record"
    );
}

/// Finding 2: one lane that cannot serve (its lease held elsewhere) ends the whole
/// service, the other lane with it, instead of leaving it dead beside a running one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lane_that_cannot_serve_ends_the_service() {
    let before: Before = Box::new(|bus: &Arc<Bus>, _: &str, _: &str| {
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
