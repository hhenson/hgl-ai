//! Hub users against a real `nats-server` from `PATH` (skipped without one), started with
//! the rendered `nats.conf`: each role's permissions hold, and no seat can make a note or
//! thread event that a seat's inbox or the console accepts as the owner's (OWN-5).

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures::StreamExt;
use hgl_ai_bus::{Bus, BusError, Credential};
use hgl_ai_install::nats::{Auth, Websocket, render_conf_ws};
use hgl_ai_install::users::{Registry, Role};
use hgl_ai_proto::{Kind, MetricsSample, Note, Seat};

struct Server(Child, #[allow(dead_code)] tempfile::TempDir);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Hub {
    _server: Server,
    url: String,
    creds: std::collections::BTreeMap<&'static str, Credential>,
    reg: std::sync::Mutex<Registry>,
    conf: std::path::PathBuf,
    store: std::path::PathBuf,
    port: u16,
    /// The WebSocket listener the console's page reads through.
    ws: Websocket,
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Hub {
    fn start() -> Option<Hub> {
        Hub::start_with(|_, conf| conf)
    }

    /// The hub with `edit` applied to its rendered `nats.conf`.
    fn start_with(edit: impl Fn(&Registry, String) -> String) -> Option<Hub> {
        let bin = std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join("nats-server"))
            .find(|p| p.is_file());
        let Some(bin) = bin else {
            eprintln!("skipped: nats-server is not on PATH");
            return None;
        };
        let d = tempfile::tempdir().unwrap();
        let mut reg = Registry::default();
        let mut creds = std::collections::BTreeMap::new();
        for (name, role) in [
            ("build-1", Role::Seat),
            ("hub-coord", Role::Coord),
            ("lab1-builder", Role::Builder),
            ("console", Role::Console),
            ("viewer", Role::Viewer),
            ("hub-admin", Role::Admin),
            ("lab2", Role::Seat),
            ("hub-slack", Role::Slack),
        ] {
            let text = reg.issue(name, role, false).unwrap();
            creds.insert(name, hgl_ai_bus::parse_credential(&text).unwrap().unwrap());
        }
        let port = free_port();
        let ws = Websocket {
            port: free_port(),
            origins: vec!["http://localhost".into()],
        };
        let conf = d.path().join("nats.conf");
        std::fs::create_dir_all(d.path().join("store")).unwrap();
        std::fs::write(
            &conf,
            edit(
                &reg,
                render_conf_ws(
                    "127.0.0.1",
                    port,
                    &d.path().join("store"),
                    &Auth::Users(&reg),
                    Some(&ws),
                ),
            ),
        )
        .unwrap();
        let child = Command::new(bin)
            .arg("-c")
            .arg(&conf)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let store = d.path().join("store");
        let conf_path = conf.clone();
        let server = Server(child, d);
        let up = (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
        });
        assert!(up, "nats-server did not start");
        Some(Hub {
            _server: server,
            url: format!("nats://127.0.0.1:{port}"),
            creds,
            reg: std::sync::Mutex::new(reg),
            conf: conf_path,
            store,
            port,
            ws,
        })
    }

    /// Rewrite `nats.conf` from the registry and reload the hub as `server user add`
    /// does: SIGHUP through the pid file.
    fn reload(&self) {
        let reg = self.reg.lock().unwrap();
        std::fs::write(
            &self.conf,
            render_conf_ws(
                "127.0.0.1",
                self.port,
                &self.store,
                &Auth::Users(&reg),
                Some(&self.ws),
            ),
        )
        .unwrap();
        let env =
            hgl_ai_install::Env::from_process(self.conf.with_file_name("config.toml")).unwrap();
        let sys = hgl_ai_install::RealSystem::new(env);
        assert!(hgl_ai_install::service::reload_hub(&sys, &self.store).unwrap());
        std::thread::sleep(Duration::from_millis(500));
    }

    async fn bus(&self, who: &str) -> Bus {
        Bus::connect_with(&self.url, Some(self.creds[who].clone()))
            .await
            .unwrap()
    }

    fn seed(&self, who: &str) -> String {
        let Credential::Nkey { seed, .. } = self.creds[who].clone() else {
            unreachable!()
        };
        seed
    }

    /// A plain client of `who`, with its own inbox prefix as the bus sets it.
    async fn raw(&self, who: &str) -> async_nats::Client {
        self.raw_with(who, async_nats::ConnectOptions::new()).await
    }

    /// `who`, connected with `opts` (an event callback) and its own inbox prefix.
    async fn raw_with(&self, who: &str, opts: async_nats::ConnectOptions) -> async_nats::Client {
        opts.custom_inbox_prefix(self.inbox(who))
            .nkey(self.seed(who))
            .connect(&self.url)
            .await
            .unwrap()
    }

    /// The inbox prefix of `who`'s replies.
    fn inbox(&self, who: &str) -> String {
        self.creds[who].inbox_prefix().unwrap()
    }

    /// What `doctor` asks the hub (`hub inboxes`): may `who`'s credential, as a host's
    /// `hub.token`, subscribe to `_INBOX.>`?
    async fn shared_inbox(&self, who: &str, bare: bool) -> Option<bool> {
        use hgl_ai_install::Hub as _;
        let Credential::Nkey { seed, user, role } = self.creds[who].clone() else {
            unreachable!()
        };
        let text = if bare {
            seed
        } else {
            Credential::render_nkey(&user.unwrap(), &role.unwrap(), &seed)
        };
        let dir = self.conf.with_file_name(format!("probe-{who}-{bare}"));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("hub.token");
        std::fs::write(&f, text).unwrap();
        let mut cfg = hgl_ai_config::Config::new("t");
        cfg.hub.url = self.url.clone();
        cfg.hub.token_file = f.display().to_string();
        hgl_ai_install::BusHub.shared_inbox(&cfg).await
    }

    /// The viewer as the page connects: its own inbox prefix.
    async fn viewer(&self) -> async_nats::Client {
        self.raw_with(
            "viewer",
            async_nats::ConnectOptions::new().custom_inbox_prefix(hgl_ai_proto::VIEWER_INBOX),
        )
        .await
    }
}

fn seat(s: &str) -> Seat {
    Seat::new(s).unwrap()
}

/// Refused: the API's permission error, or, where the server drops the lookup itself
/// (a stream the user may not even look up), no answer.
fn refused(r: Result<impl std::fmt::Debug, BusError>) {
    match r {
        Err(BusError::Denied { .. }) => {}
        Err(e) if e.to_string().contains("timed out") => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn denied(r: Result<impl std::fmt::Debug, BusError>) -> String {
    match r {
        Err(e @ BusError::Denied { .. }) => e.to_string(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn roles_hold_and_seats_cannot_speak_as_the_owner() {
    let Some(hub) = Hub::start() else { return };
    let admin = hub.bus("hub-admin").await;
    admin.provision().await.unwrap();
    let seat_bus = hub.bus("build-1").await;
    assert_eq!(seat_bus.who(), "hub user build-1 (seat)");

    // What a seat does every day works.
    seat_bus
        .send(&Note::new(seat("w1"), seat("claude"), Kind::Note, "hello"))
        .await
        .unwrap();
    seat_bus.state_put("mode", "normal").await.unwrap();
    seat_bus.append_line("landed.log", "x").await.unwrap();
    assert_eq!(
        seat_bus.state_get("mode").await.unwrap().unwrap().value,
        "normal"
    );
    seat_bus.hosts().await.unwrap();
    seat_bus.trees().await.unwrap();
    // Usage records, their history and labels (PROTO-23).
    let usage = hgl_ai_proto::UsageRecord {
        runtime: hgl_ai_proto::Runtime::Codex,
        account: hgl_ai_proto::account_fingerprint("acct"),
        label: None,
        host: "build-1".into(),
        plan: None,
        observed_at: "20261127T000000Z".into(),
        collected_at: "20261127T000000Z".into(),
        source_age_s: 0,
        buckets: vec![],
        limit_reached: None,
        unparsed: vec![],
    };
    assert!(matches!(
        seat_bus.usage_put(&usage).await.unwrap(),
        hgl_ai_bus::UsagePut::Written { .. }
    ));
    seat_bus
        .usage_label(&usage.account, Some("team"))
        .await
        .unwrap();
    assert_eq!(
        seat_bus.usage().await.unwrap()[0].label.as_deref(),
        Some("team")
    );
    // Every role reads the usage history (PROTO-24).
    for who in ["build-1", "hub-coord", "console"] {
        let got = hub
            .bus(who)
            .await
            .history(
                hgl_ai_proto::STREAM_USAGE,
                "hgl.usage.>",
                chrono::DateTime::UNIX_EPOCH,
                10,
            )
            .await;
        assert_eq!(got.map(|h| h.len()).ok(), Some(1), "{who}");
    }
    let task = hgl_ai_proto::TaskKey {
        repo: "alpha".into(),
        issue: 7,
    };
    seat_bus.ended_put(&seat("w1"), &task, 42).await.unwrap();
    assert!(
        seat_bus
            .ended_get(&seat("w1"), &task)
            .await
            .unwrap()
            .is_some()
    );
    let mut lease = seat_bus.lock_acquire("seat.w1", "build-1").await.unwrap();
    lease.refresh().await.unwrap();
    assert!(lease.release().await.unwrap());
    // A seat's bridge opens and works its inbox (consumer, pull, progress, ack, nak).
    let mut inbox = seat_bus.inbox(&seat("w9")).await.unwrap();
    seat_bus
        .send(&Note::new(seat("w1"), seat("w9"), Kind::Note, "to w9"))
        .await
        .unwrap();
    let got = inbox.fetch(5, Duration::from_secs(5)).await.unwrap();
    assert_eq!(got.len(), 1);
    got[0].in_progress().await.unwrap();
    got[0].nak(Duration::ZERO).await.unwrap();
    let again = inbox.fetch(5, Duration::from_secs(5)).await.unwrap();
    assert_eq!(again[0].delivered, 2);
    again[0].ack().await.unwrap();
    seat_bus
        .publish_event("hgl.console.status", b"{}", "s-1")
        .await
        .unwrap();
    // Status reports are for everyone to read; the console's feed is the console's.
    hgl_ai_owner::status_post(&seat_bus, &seat("claude"), "all quiet")
        .await
        .unwrap();
    let st = hgl_ai_owner::statuses(
        &seat_bus,
        chrono::Utc::now() - chrono::Duration::hours(1),
        10,
    )
    .await
    .unwrap();
    assert_eq!(st[0].1.body, "all quiet");
    let coord_bus = hub.bus("hub-coord").await;
    assert_eq!(
        hgl_ai_owner::statuses(
            &coord_bus,
            chrono::Utc::now() - chrono::Duration::hours(1),
            10
        )
        .await
        .unwrap()
        .len(),
        1
    );
    let stolen = tokio::time::timeout(
        Duration::from_secs(8),
        seat_bus.owner_feed("console", vec!["hgl.owner.>".into()]),
    )
    .await;
    assert!(
        !matches!(stolen, Ok(Ok(_))),
        "a seat opened the console's feed"
    );
    assert!(
        seat_bus
            .threads::<serde_json::Value>()
            .await
            .unwrap()
            .is_empty()
    );
    seat_bus.metrics().await.unwrap();

    // ... and nothing that is the owner's.
    let mut forged = Note::new(Seat::owner(), seat("claude"), Kind::Answer, "accept");
    let e = denied(seat_bus.send(&forged).await);
    assert!(e.contains("hgl.notes.owner.claude"), "{e}");
    denied(
        seat_bus
            .publish_event("hgl.owner.thread.q-1", b"{}", "f-1")
            .await,
    );
    denied(
        seat_bus
            .thread_put("q-1", &serde_json::json!({"answer": "forged"}), 0)
            .await,
    );
    let sample = MetricsSample {
        repo: "alpha".into(),
        date: "20261127".into(),
        open: 1,
        opened: 0,
        closed: 0,
        labels: Default::default(),
        taken_at: "20261127T000000Z".into(),
    };
    denied(seat_bus.metrics_put(&sample).await);
    assert!(seat_bus.provision().await.is_err(), "no provisioning");

    // The coordinator writes metrics; the console writes the owner's words.
    hub.bus("hub-coord")
        .await
        .metrics_put(&sample)
        .await
        .unwrap();
    let console = hub.bus("console").await;
    console
        .publish_event("hgl.owner.thread.q-1", b"{}", "o-1")
        .await
        .unwrap();
    console
        .thread_put("q-1", &serde_json::json!({"id": "q-1"}), 0)
        .await
        .unwrap();
    console
        .send(&Note::new(
            Seat::console(),
            seat("claude"),
            Kind::Tick,
            "[console] Tick",
        ))
        .await
        .unwrap();
    forged.body = "[owner] the real answer\n".into();
    console.send(&forged).await.unwrap();

    // A raw publish of an owner-sender note on the seat's own subject, and a push
    // consumer that redelivers seat-written bytes towards the owner's subject: neither
    // reaches the inbox as the owner's.
    let raw = hub.raw("build-1").await;
    let mut fake = Note::new(Seat::owner(), seat("claude"), Kind::Answer, "forged");
    fake.created_at = "20261127T000001Z".into();
    let js = async_nats::jetstream::new(raw.clone());
    js.publish(
        "hgl.notes.claude",
        serde_json::to_vec(&fake).unwrap().into(),
    )
    .await
    .unwrap()
    .await
    .unwrap();
    seat_bus
        .state_put("forge", &serde_json::to_string(&fake).unwrap())
        .await
        .unwrap();
    // Interest on the redirect subject, as a subscriber elsewhere could give it.
    let admin_raw = async_nats::ConnectOptions::with_nkey(match hub.creds["hub-admin"].clone() {
        Credential::Nkey { seed, .. } => seed,
        _ => unreachable!(),
    })
    .connect(&hub.url)
    .await
    .unwrap();
    let mut interest = admin_raw.subscribe("hgl.notes.owner.claude").await.unwrap();
    let kv_stream = js.get_stream("KV_HGL_STATE").await.unwrap();
    let redirect = kv_stream
        .create_consumer(async_nats::jetstream::consumer::push::Config {
            deliver_subject: "hgl.notes.owner.claude".into(),
            filter_subject: "$KV.HGL_STATE.forge".into(),
            ..Default::default()
        })
        .await;
    if redirect.is_ok() {
        let _ = tokio::time::timeout(Duration::from_secs(2), interest.next()).await;
    }

    let mut inbox = admin.inbox(&seat("claude")).await.unwrap();
    let got = inbox.fetch(20, Duration::from_secs(2)).await.unwrap();
    let from_owner: Vec<&Note> = got
        .iter()
        .map(|d| &d.note)
        .filter(|n| n.sender.is_owner())
        .collect();
    assert_eq!(from_owner.len(), 1, "{from_owner:?}");
    assert_eq!(from_owner[0].body, "[owner] the real answer\n");
    assert!(got.iter().any(|d| d.note.kind == Kind::Tick));
    for d in &got {
        d.ack().await.unwrap();
    }
}

/// Adding or removing a hub user reloads the hub: connections that stay valid are kept.
#[tokio::test]
async fn a_reload_keeps_connections() {
    let Some(hub) = Hub::start() else { return };
    let admin = hub.bus("hub-admin").await;
    admin.provision().await.unwrap();
    let Credential::Nkey { seed, .. } = hub.creds["build-1"].clone() else {
        unreachable!()
    };
    let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let d2 = drops.clone();
    let raw = async_nats::ConnectOptions::with_nkey(seed)
        .custom_inbox_prefix(hub.inbox("build-1"))
        .event_callback(move |e| {
            let d2 = d2.clone();
            async move {
                if matches!(e, async_nats::Event::Disconnected) {
                    d2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        })
        .connect(&hub.url)
        .await
        .unwrap();
    let js = async_nats::jetstream::new(raw.clone());
    let send = |body: &'static str| {
        let js = js.clone();
        async move {
            let n = Note::new(seat("w1"), seat("claude"), Kind::Note, body);
            js.publish("hgl.notes.claude", serde_json::to_vec(&n).unwrap().into())
                .await
                .unwrap()
                .await
                .unwrap();
        }
    };
    send("before").await;

    // A new user appears without a restart.
    let text = hub
        .reg
        .lock()
        .unwrap()
        .issue("build-2", Role::Seat, false)
        .unwrap();
    let new = hgl_ai_bus::parse_credential(&text).unwrap();
    assert!(
        Bus::connect_with(&hub.url, new.clone()).await.is_err(),
        "not yet"
    );
    hub.reload();
    Bus::connect_with(&hub.url, new).await.unwrap();
    send("after").await;
    assert_eq!(
        drops.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the existing connection was kept"
    );

    // A removed user is refused from then on.
    hub.reg.lock().unwrap().remove("build-2");
    hub.reload();
    let text_err = Bus::connect_with(&hub.url, hgl_ai_bus::parse_credential(&text).unwrap()).await;
    assert!(text_err.is_err(), "build-2 was removed");
    send("still").await;
}

/// Carbon's finding 4 on #12: a seat keeps build runs but may not write recipes, which
/// every builder runs.
#[tokio::test]
async fn seats_keep_build_runs_but_not_recipes() {
    let Some(hub) = Hub::start() else { return };
    let admin = hub.bus("hub-admin").await;
    admin.provision().await.unwrap();
    let recipe = b"repo = \"demo\"\n[[step]]\nname = \"t\"\nrun = \"true\"\n";
    admin.build_put("recipe.demo", recipe).await.unwrap();
    let seat_bus = hub.bus("build-1").await;
    seat_bus
        .build_put("run.20261127-120000-aaaaaa", b"{}")
        .await
        .unwrap();
    seat_bus
        .build_del("run.20261127-120000-aaaaaa")
        .await
        .unwrap();
    assert!(
        seat_bus.build_get("recipe.demo").await.unwrap().is_some(),
        "seats read recipes"
    );
    denied(seat_bus.build_put("recipe.demo", b"repo = \"evil\"").await);
    denied(seat_bus.build_del("recipe.demo").await);
    assert_eq!(
        admin.build_get("recipe.demo").await.unwrap().unwrap().value,
        recipe
    );
    // No hub role but the admin writes recipes: seats share one host credential, so a
    // recipe-writing seat role would hand every seat of its host the fleet's builders
    // (Carbon's second review of #12, finding 1). Recipes change by pull request and the
    // admin imports them.
    assert!("build".parse::<Role>().is_err(), "no `build` hub role");
    admin.build_del("recipe.demo").await.unwrap();
}

/// hgl-ai#6: only a builder's hub user writes the local build verdicts the merge gate
/// reads; a seat's and the coordinator's are refused, and a builder keeps a seat's limits
/// otherwise.
#[tokio::test]
async fn only_builders_write_verdicts() {
    let Some(hub) = Hub::start() else { return };
    let admin = hub.bus("hub-admin").await;
    admin.provision().await.unwrap();
    let h = "a".repeat(40);
    let key = "verdict.hgl-compiler.7.linux";
    let builder = hub.bus("lab1-builder").await;
    builder
        .state_put(key, &format!("{h} {h} pass 20261127-120000-aaaaaa lab1"))
        .await
        .unwrap();
    for who in ["build-1", "hub-coord", "console"] {
        let b = hub.bus(who).await;
        denied(b.state_put(key, &format!("{h} {h} pass forged")).await);
        denied(
            b.state_update(key, &format!("{h} {h} pass forged"), 1)
                .await,
        );
        denied(b.state_create("verdict.hgl-compiler.9.linux", "x").await);
        denied(b.state_del(key).await);
        denied(b.state_put("verdict.hgl-compiler.8.mac", "x").await);
        // The rest of `HGL_STATE` stays a seat's (the console writes only its log).
        if who != "console" {
            b.state_put("mode", "normal").await.unwrap();
        }
        assert!(
            b.state_get(key).await.unwrap().is_some(),
            "{who} reads verdicts"
        );
    }
    assert!(
        admin
            .state_get(key)
            .await
            .unwrap()
            .unwrap()
            .value
            .ends_with("lab1")
    );
    // `builder run` asks the hub, not the file: a bare NKey seed of a builder user may
    // write verdicts; a seat's or the coordinator's may not (r3 on #56, finding 4).
    use hgl_ai_build::store::may_write_verdicts;
    assert!(may_write_verdicts(&builder, "lab1").await.unwrap());
    let Credential::Nkey { seed, .. } = hub.creds["lab1-builder"].clone() else {
        unreachable!()
    };
    let bare = Credential::Nkey {
        seed,
        user: None,
        role: None,
    };
    let bare = Bus::connect_with(&hub.url, Some(bare)).await.unwrap();
    assert!(may_write_verdicts(&bare, "lab1").await.unwrap());
    for who in ["build-1", "hub-coord"] {
        assert!(
            !may_write_verdicts(&hub.bus(who).await, "lab1")
                .await
                .unwrap()
        );
    }
    assert!(
        admin
            .state_get("verdict._probe.0.lab1")
            .await
            .unwrap()
            .is_none()
    );
    // A builder sends notes (its receipts) but writes no recipe.
    let n = Note::new(seat("builder-lab1"), seat("controller"), Kind::Note, "x");
    builder.send(&n).await.unwrap();
    denied(builder.build_put("recipe.demo", b"repo = \"evil\"").await);
}

/// `request` as `c`, with a second to answer: `None` when nothing answers (the server
/// drops a request the user may not publish), else the reply's payload.
async fn ask(c: &async_nats::Client, subject: &str, body: serde_json::Value) -> Option<String> {
    match tokio::time::timeout(
        Duration::from_secs(1),
        c.request(subject.to_string(), body.to_string().into()),
    )
    .await
    {
        Ok(Ok(m)) => Some(String::from_utf8_lossy(&m.payload).into_owned()),
        _ => None,
    }
}

/// Each request gets no answer (asked all at once).
async fn unanswered(c: &async_nats::Client, asks: Vec<(String, serde_json::Value)>) {
    let answers = futures::future::join_all(
        asks.into_iter()
            .map(|(subject, body)| async move { (ask(c, &subject, body).await, subject) }),
    )
    .await;
    for (answer, subject) in answers {
        assert_eq!(answer, None, "{subject}");
    }
}

/// A consumer create's body: an ordered push consumer, as a watch makes, delivering to
/// `deliver` (`filter`: one subject, `filters`: several, neither: the whole stream).
fn consumer_body(
    stream: &str,
    name: Option<&str>,
    durable: Option<&str>,
    deliver: &str,
    filter: Option<&str>,
    filters: &[&str],
) -> serde_json::Value {
    let mut config = serde_json::json!({
        "deliver_subject": deliver,
        "deliver_policy": "last_per_subject",
        "ack_policy": "none",
        "flow_control": true,
        "idle_heartbeat": 5_000_000_000u64,
    });
    if let Some(n) = name {
        config["name"] = n.into();
    }
    if let Some(d) = durable {
        config["durable_name"] = d.into();
    }
    if let Some(f) = filter {
        config["filter_subject"] = f.into();
    }
    if !filters.is_empty() {
        config["filter_subjects"] = filters.into();
    }
    serde_json::json!({"stream_name": stream, "config": config})
}

/// Subscribe to `deliver`, ask for a consumer delivering there, and read for a second:
/// the API's answer, if any, and the values delivered.
async fn consume(
    c: &async_nats::Client,
    subject: &str,
    body: serde_json::Value,
) -> (Option<String>, Vec<String>) {
    let deliver = body["config"]["deliver_subject"]
        .as_str()
        .unwrap()
        .to_string();
    let mut sub = c.subscribe(deliver).await.unwrap();
    c.flush().await.unwrap();
    let answer = ask(c, subject, body).await;
    let mut got = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(m) = sub.next().await {
            got.push(String::from_utf8_lossy(&m.payload).into_owned());
        }
    })
    .await;
    (answer, got)
}

/// What the hub holds that the viewer may not read: values of `HGL_STATE` outside the
/// page's keys.
const SECRET: &str = "not-for-the-viewer";

/// The console's page reads and watches the telemetry buckets as `viewer`, and can do
/// nothing else by subject: no write, no notes, no threads, no console state, no other
/// stream, and of `HGL_STATE` only the page's keys. (Writes through replies are not
/// enforced until hgl-ai#141.)
#[tokio::test]
async fn the_viewer_reads_the_telemetry_and_nothing_else() {
    let Some(hub) = Hub::start() else { return };
    let admin = hub.bus("hub-admin").await;
    admin.provision().await.unwrap();
    let seat_bus = hub.bus("build-1").await;
    seat_bus.state_put("mode", "normal").await.unwrap();
    seat_bus
        .state_put("decisions.log", &format!("{SECRET} decision\n"))
        .await
        .unwrap();
    seat_bus
        .state_put("coord.alerts", &format!("{SECRET} alert\n"))
        .await
        .unwrap();
    let h = "a".repeat(40);
    hub.bus("lab1-builder")
        .await
        .state_put(
            "verdict.alpha.7.linux",
            &format!("{h} {h} pass {SECRET} lab1"),
        )
        .await
        .unwrap();
    seat_bus
        .send(&Note::new(seat("w1"), seat("w2"), Kind::Note, "hello"))
        .await
        .unwrap();
    hub.bus("console")
        .await
        .console_put("session.x", &serde_json::json!({"secret": 1}))
        .await
        .unwrap();
    let v = hub.bus("viewer").await;
    assert_eq!(v.who(), "hub user viewer (viewer)");

    // A key the page reads, by its subject (a direct get for the last value).
    assert_eq!(v.state_get("mode").await.unwrap().unwrap().value, "normal");
    // No other key: not by its subject, nor named in a request's body.
    let raw = hub.viewer().await;
    let secret = "$KV.HGL_STATE.decisions.log";
    let mut asks: Vec<(String, serde_json::Value)> =
        ["decisions.log", "coord.alerts", "verdict.alpha.7.linux"]
            .iter()
            .map(|k| {
                (
                    format!("$JS.API.DIRECT.GET.KV_HGL_STATE.$KV.HGL_STATE.{k}"),
                    serde_json::json!({}),
                )
            })
            .collect();
    asks.extend(
        [
            (
                "$JS.API.DIRECT.GET.KV_HGL_STATE",
                serde_json::json!({"last_by_subj": secret}),
            ),
            (
                "$JS.API.STREAM.MSG.GET.KV_HGL_STATE",
                serde_json::json!({"last_by_subj": secret}),
            ),
            (
                "$JS.API.DIRECT.GET.KV_HGL_STATE",
                serde_json::json!({"seq": 2}),
            ),
        ]
        .map(|(subject, body)| (subject.to_string(), body)),
    );
    unanswered(&raw, asks).await;
    // The subject form names its key: a sequence in the body reads no other.
    for seq in 1..=6 {
        let r = ask(
            &raw,
            "$JS.API.DIRECT.GET.KV_HGL_STATE.$KV.HGL_STATE.mode",
            serde_json::json!({"seq": seq}),
        )
        .await;
        assert!(!r.unwrap_or_default().contains(SECRET), "seq {seq}");
    }

    // Consumers: only the filtered create form, on a key the page reads.
    let state = "KV_HGL_STATE";
    let d = |n: &str| format!("{}.probe.{n}", hgl_ai_proto::VIEWER_INBOX);
    let refused_creates = [
        // The bare, named and durable forms, which carry the filter in the body only.
        (
            format!("$JS.API.CONSUMER.CREATE.{state}"),
            consumer_body(state, None, None, &d("a"), Some(secret), &[]),
        ),
        (
            format!("$JS.API.CONSUMER.CREATE.{state}.b"),
            consumer_body(state, Some("b"), None, &d("b"), Some(secret), &[]),
        ),
        (
            format!("$JS.API.CONSUMER.DURABLE.CREATE.{state}.c"),
            consumer_body(state, None, Some("c"), &d("c"), Some(secret), &[]),
        ),
        (
            "$JS.API.CONSUMER.CREATE.KV_HGL_HOSTS".to_string(),
            consumer_body("KV_HGL_HOSTS", None, None, &d("h"), None, &[]),
        ),
        (
            "$JS.API.CONSUMER.DURABLE.CREATE.KV_HGL_HOSTS.h2".to_string(),
            consumer_body("KV_HGL_HOSTS", None, Some("h2"), &d("h2"), None, &[]),
        ),
        // The filtered form on a key the page does not read.
        (
            format!("$JS.API.CONSUMER.CREATE.{state}.e.{secret}"),
            consumer_body(state, Some("e"), None, &d("e"), Some(secret), &[]),
        ),
    ]
    .map(|(subject, body)| {
        let raw = &raw;
        async move { (consume(raw, &subject, body).await, subject) }
    });
    for ((answer, got), subject) in futures::future::join_all(refused_creates).await {
        assert_eq!((answer, got), (None, vec![]), "{subject}");
    }
    // The filtered form on a page key, with another filter in the body (or none): the
    // server holds the body to the subject's filter.
    let mismatched = [
        (Some(secret), vec![]),
        (None, vec![secret]),
        (None, vec!["$KV.HGL_STATE.prs", secret]),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, (filter, filters))| {
        let name = format!("f{i}");
        let subject = format!("$JS.API.CONSUMER.CREATE.{state}.{name}.$KV.HGL_STATE.prs");
        let body = consumer_body(state, Some(&name), None, &d(&name), filter, &filters);
        let raw = &raw;
        async move { consume(raw, &subject, body).await }
    });
    for (answer, got) in futures::future::join_all(mismatched).await {
        let answer = answer.expect("the API answers");
        assert!(answer.contains("\"error\""), "{answer}");
        assert!(got.iter().all(|g| !g.contains(SECRET)), "{got:?}");
    }
    // A filter-less body takes the subject's filter, or is refused: no other key comes.
    let (_, got) = consume(
        &raw,
        &format!("$JS.API.CONSUMER.CREATE.{state}.f9.$KV.HGL_STATE.prs"),
        consumer_body(state, Some("f9"), None, &d("f9"), None, &[]),
    )
    .await;
    assert!(got.iter().all(|g| !g.contains(SECRET)), "{got:?}");
    // And a filtered create on a page key works: the value comes.
    let (answer, got) = consume(
        &raw,
        &format!("$JS.API.CONSUMER.CREATE.{state}.g.$KV.HGL_STATE.mode"),
        consumer_body(
            state,
            Some("g"),
            None,
            &d("g"),
            Some("$KV.HGL_STATE.mode"),
            &[],
        ),
    )
    .await;
    let answer = answer.expect("the API answers");
    assert!(!answer.contains("\"error\""), "{answer}");
    assert_eq!(got, vec!["normal".to_string()]);

    // No write by subject (through replies: hgl-ai#141).
    denied(v.state_put("mode", "drain").await);
    denied(
        v.send(&Note::new(seat("w1"), seat("w2"), Kind::Note, "forged"))
            .await,
    );
    denied(
        v.metrics_put(&MetricsSample {
            repo: "alpha".into(),
            date: "20261128".into(),
            open: 1,
            opened: 0,
            closed: 0,
            labels: Default::default(),
            taken_at: "20261128T000000Z".into(),
        })
        .await,
    );
    denied(v.build_put("run.x", b"{}").await);
    // Nothing outside the telemetry: notes, threads, the console's state.
    let (history, threads, sessions) = tokio::join!(
        v.history(
            hgl_ai_proto::STREAM_NOTES,
            "hgl.notes.>",
            chrono::DateTime::UNIX_EPOCH,
            10,
        ),
        v.threads::<serde_json::Value>(),
        v.console_list::<serde_json::Value>("session."),
    );
    refused(history.map(|h| h.len()));
    refused(threads);
    refused(sessions);
    // Not even a direct get on the console's bucket, or a consumer delete.
    let others = [
        "$JS.API.DIRECT.GET.KV_HGL_CONSOLE.$KV.HGL_CONSOLE.session.x",
        "$JS.API.STREAM.MSG.GET.KV_HGL_THREADS",
        "$JS.API.CONSUMER.DELETE.KV_HGL_STATE.any",
        "$JS.API.CONSUMER.LIST.KV_HGL_STATE",
        "$JS.API.STREAM.PURGE.KV_HGL_STATE",
        "$KV.HGL_STATE.mode",
        "hgl.notes.w1",
        "hgl.owner.x",
    ]
    .map(|subject| (subject.to_string(), serde_json::json!({})));
    unanswered(&raw, others.to_vec()).await;
    // A consumer aimed at a notes subject instead of the viewer's inbox does not make the
    // server write a note for it (a push consumer delivers only to subscribers, and the
    // viewer may subscribe to its inbox only).
    let notes = || async {
        seat_bus
            .history(
                hgl_ai_proto::STREAM_NOTES,
                "hgl.notes.>",
                chrono::DateTime::UNIX_EPOCH,
                100,
            )
            .await
            .unwrap()
            .len()
    };
    let before = notes().await;
    let r = ask(
        &raw,
        "$JS.API.CONSUMER.CREATE.KV_HGL_STATE.n.$KV.HGL_STATE.mode",
        consumer_body(
            state,
            Some("n"),
            None,
            "hgl.notes.w9",
            Some("$KV.HGL_STATE.mode"),
            &[],
        ),
    )
    .await;
    assert!(r.is_some(), "the filtered form is allowed");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(notes().await, before, "no note from the viewer's consumer");
    assert_eq!(
        seat_bus.state_get("mode").await.unwrap().unwrap().value,
        "normal",
        "nothing the viewer did changed a value"
    );
}

/// The viewer subscribes to its own inbox prefix only: not to `_INBOX.>`, where every
/// other client's replies go (the console's reads of its sessions, threads, notes).
#[tokio::test]
async fn the_viewer_sees_no_other_clients_replies() {
    let Some(hub) = Hub::start() else { return };
    let admin = hub.bus("hub-admin").await;
    admin.provision().await.unwrap();
    let console = hub.bus("console").await;
    console
        .console_put("session.x", &serde_json::json!({"csrf": SECRET}))
        .await
        .unwrap();
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel::<String>();
    let v = hub
        .raw_with(
            "viewer",
            async_nats::ConnectOptions::new()
                .custom_inbox_prefix(hgl_ai_proto::VIEWER_INBOX)
                .event_callback(move |e| {
                    let tx = tx.clone();
                    async move {
                        let _ = tx.send(e.to_string());
                    }
                }),
        )
        .await;
    let mut spy = v.subscribe("_INBOX.>").await.unwrap();
    let mut all = v.subscribe(">").await.unwrap();
    v.flush().await.unwrap();
    let mut refusals = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(e) = events.recv().await {
            if e.to_ascii_lowercase().contains("permissions violation") {
                refusals.push(e);
                if refusals.len() == 2 {
                    break;
                }
            }
        }
    })
    .await;
    assert!(
        refusals.iter().any(|e| e.contains("\"_INBOX.>\"")),
        "{refusals:?}"
    );
    // The console reads its state; the replies go to its `_INBOX.…`, not to the viewer.
    for _ in 0..3 {
        let s: Option<serde_json::Value> = console.console_get("session.x").await.unwrap();
        assert!(s.is_some());
        console
            .console_list::<serde_json::Value>("session.")
            .await
            .unwrap();
    }
    for sub in [&mut spy, &mut all] {
        let r = tokio::time::timeout(Duration::from_secs(1), sub.next()).await;
        assert!(
            !matches!(r, Ok(Some(_))),
            "the viewer saw a message: {:?}",
            r.ok().flatten().map(|m| m.subject)
        );
    }
    // Its own prefix works.
    let r = v
        .request("$JS.API.STREAM.INFO.KV_HGL_HOSTS", "".into())
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&r.payload).contains("KV_HGL_HOSTS"));
}

/// The page's own code (`web/src/hub.ts`) watches every source as the viewer over the
/// hub's WebSocket listener, and is refused nothing: `web/src/hub.live.test.ts`, run here
/// with the hub's address (skipped without `npm ci` in `crates/console/web`).
#[tokio::test]
async fn the_page_watches_as_the_viewer() {
    let web = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../console/web");
    let vitest = web.join("node_modules/.bin/vitest");
    if !vitest.is_file() {
        eprintln!("skipped: no {} (`npm ci` there)", vitest.display());
        return;
    }
    let Some(hub) = Hub::start() else { return };
    let admin = hub.bus("hub-admin").await;
    admin.provision().await.unwrap();
    let seat_bus = hub.bus("build-1").await;
    seat_bus.state_put("mode", "drain").await.unwrap();
    seat_bus
        .host_put(&hgl_ai_proto::HostRecord {
            host: "lab1".into(),
            seats: Vec::new(),
            memcap_gb: 64,
            work_root_free_gb: 100,
            updated_at: "20261128T000000Z".into(),
        })
        .await
        .unwrap();
    // `hold` written again and again, so writes land while the page watches.
    let writer = {
        let b = hub.bus("build-1").await;
        tokio::spawn(async move {
            loop {
                let _ = b.state_put("hold", "landing").await;
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
    };
    let ws = format!("ws://127.0.0.1:{}", hub.ws.port);
    let seed = hub.seed("viewer");
    let out = tokio::task::spawn_blocking(move || {
        Command::new(vitest)
            .args(["run", "src/hub.live.test.ts"])
            .current_dir(&web)
            .env("HGL_AI_LIVE_WS", ws)
            .env("HGL_AI_LIVE_SEED", seed)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    writer.abort();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed"), "the live test ran: {text}");
    // The page made no durable consumer: the doctor's check finds none.
    let js = async_nats::jetstream::new(hub.raw("build-1").await);
    assert_eq!(
        hgl_ai_install::hub::lasting_consumers(&js).await.unwrap(),
        Vec::<String>::new()
    );
}

/// What subjects cannot stop: a `durable_name` or a long `inactive_threshold` in a filtered
/// create's body. `doctor`'s listing, as a seat's hub user, finds both (docs/design.md §9).
#[tokio::test]
async fn a_lasting_consumer_the_viewer_makes_is_found() {
    let Some(hub) = Hub::start() else { return };
    let admin = hub.bus("hub-admin").await;
    admin.provision().await.unwrap();
    let js = async_nats::jetstream::new(hub.raw("build-1").await);
    assert_eq!(
        hgl_ai_install::hub::lasting_consumers(&js).await.unwrap(),
        Vec::<String>::new()
    );
    let raw = hub.viewer().await;
    let deliver = format!("{}.d", hgl_ai_proto::VIEWER_INBOX);
    let r = ask(
        &raw,
        "$JS.API.CONSUMER.CREATE.KV_HGL_HOSTS.kept.$KV.HGL_HOSTS.>",
        consumer_body(
            "KV_HGL_HOSTS",
            Some("kept"),
            Some("kept"),
            &deliver,
            Some("$KV.HGL_HOSTS.>"),
            &[],
        ),
    )
    .await
    .expect("the API answers");
    assert!(!r.contains("\"error\""), "{r}");
    // Nor an inactive threshold: one of a day outlives the page as a durable would.
    let mut body = consumer_body(
        "KV_HGL_TREES",
        Some("idle"),
        None,
        &format!("{deliver}.i"),
        Some("$KV.HGL_TREES.>"),
        &[],
    );
    body["config"]["inactive_threshold"] = 86_400_000_000_000u64.into();
    let r = ask(
        &raw,
        "$JS.API.CONSUMER.CREATE.KV_HGL_TREES.idle.$KV.HGL_TREES.>",
        body,
    )
    .await
    .expect("the API answers");
    assert!(!r.contains("\"error\""), "{r}");
    assert_eq!(
        hgl_ai_install::hub::lasting_consumers(&js).await.unwrap(),
        vec![
            "KV_HGL_HOSTS/kept".to_string(),
            "KV_HGL_TREES/idle".to_string()
        ]
    );
}

/// A client of `who` that reports its refusals.
async fn spy(
    hub: &Hub,
    who: &str,
) -> (
    async_nats::Client,
    tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let c = hub
        .raw_with(
            who,
            async_nats::ConnectOptions::new().event_callback(move |e| {
                let tx = tx.clone();
                async move {
                    let s = e.to_string();
                    if s.to_ascii_lowercase().contains("permissions violation") {
                        let _ = tx.send(s);
                    }
                }
            }),
        )
        .await;
    (c, rx)
}

/// Up to `n` refusals, within a few seconds.
async fn refusals(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>, n: usize) -> Vec<String> {
    let mut got = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while got.len() < n {
            match rx.recv().await {
                Some(e) => got.push(e),
                None => break,
            }
        }
    })
    .await;
    got
}

/// Every hub user subscribes to its own inbox prefix only (hgl-ai#111): no seat, builder,
/// coordinator, Slack adapter or console sees another user's JetStream and KV replies,
/// such as the console's reads of its sessions, or a seat's of the notes.
#[tokio::test]
async fn no_user_sees_another_users_replies() {
    let Some(hub) = Hub::start() else { return };
    let admin = hub.bus("hub-admin").await;
    admin.provision().await.unwrap();
    let console = hub.bus("console").await;
    console
        .console_put("session.x", &serde_json::json!({"csrf": SECRET}))
        .await
        .unwrap();
    let seat_bus = hub.bus("build-1").await;
    seat_bus.state_put("mode", SECRET).await.unwrap();

    let mut spies = Vec::new();
    for who in [
        "build-1",
        "lab2",
        "hub-coord",
        "lab1-builder",
        "hub-slack",
        "console",
    ] {
        let (c, mut rx) = spy(&hub, who).await;
        let mut wild = vec!["_INBOX.>".to_string(), ">".into(), "_INBOX.*.>".into()];
        for victim in ["console", "build-1"] {
            if victim != who {
                wild.push(format!("{}.>", hub.inbox(victim)));
            }
        }
        let mut subs = Vec::new();
        for w in &wild {
            subs.push(c.subscribe(w.clone()).await.unwrap());
        }
        c.flush().await.unwrap();
        let got = refusals(&mut rx, wild.len()).await;
        for w in &wild {
            assert!(
                got.iter().any(|e| e.contains(&format!("\"{w}\""))),
                "{who} may subscribe to {w}: {got:?}"
            );
        }
        // Its own prefix works.
        let probe = if who == "hub-slack" {
            "$JS.API.STREAM.INFO.KV_HGL_SLACK"
        } else {
            "$JS.API.INFO"
        };
        let r = c.request(probe, "".into()).await.unwrap();
        assert!(
            !String::from_utf8_lossy(&r.payload).contains("\"error\""),
            "{who}: {:?}",
            r.payload
        );
        spies.push((who, c, subs));
    }

    // The console reads its sessions, a seat its state; nobody else sees the replies.
    for _ in 0..3 {
        let s: Option<serde_json::Value> = console.console_get("session.x").await.unwrap();
        assert!(s.unwrap().to_string().contains(SECRET));
        console
            .console_list::<serde_json::Value>("session.")
            .await
            .unwrap();
        let m = seat_bus.state_get("mode").await.unwrap().unwrap();
        assert_eq!(m.value, SECRET);
    }
    for (who, _c, subs) in &mut spies {
        for sub in subs {
            let r = tokio::time::timeout(Duration::from_millis(300), sub.next()).await;
            assert!(
                !matches!(r, Ok(Some(_))),
                "{who} saw a message: {:?}",
                r.ok().flatten().map(|m| m.subject)
            );
        }
    }

    // A seat's direct publish to the console's inbox is refused. JetStream replies and
    // consumer deliver subjects are not checked until hgl-ai#141.
    let c = hub.raw("console").await;
    let into = format!("{}.forged", hub.inbox("console"));
    let mut own = c.subscribe(into.clone()).await.unwrap();
    c.flush().await.unwrap();
    let (s, mut rx) = spy(&hub, "build-1").await;
    s.publish(into.clone(), "forged".into()).await.unwrap();
    s.flush().await.unwrap();
    let got = refusals(&mut rx, 1).await;
    assert!(got.iter().any(|e| e.contains(&into)), "{got:?}");
    let r = tokio::time::timeout(Duration::from_millis(500), own.next()).await;
    assert!(!matches!(r, Ok(Some(_))), "the forged reply arrived");

    // A client on the shared `_INBOX` (one that predates this) gets no replies.
    let Credential::Nkey { seed, .. } = hub.creds["build-1"].clone() else {
        unreachable!()
    };
    let old = async_nats::ConnectOptions::with_nkey(seed)
        .connect(&hub.url)
        .await
        .unwrap();
    let r = tokio::time::timeout(
        Duration::from_secs(3),
        old.request("$JS.API.INFO", "".into()),
    )
    .await;
    assert!(!matches!(r, Ok(Ok(_))), "{r:?}");

    // `doctor`'s probe: an upgraded hub refuses `_INBOX.>`; the admin and a bare seed
    // cannot tell.
    for who in ["build-1", "console", "hub-coord"] {
        assert_eq!(hub.shared_inbox(who, false).await, Some(false), "{who}");
    }
    assert_eq!(hub.shared_inbox("hub-admin", false).await, None);
    assert_eq!(hub.shared_inbox("build-1", true).await, None);
}

/// A client with its own inbox still works on a hub whose `nats.conf` predates them and
/// allows every user `_INBOX.>`: upgrade clients first, then the hub (hgl-ai#111).
#[tokio::test]
async fn own_inboxes_work_on_an_older_hub() {
    let Some(hub) = Hub::start_with(|reg, mut conf| {
        for u in &reg.users {
            conf = conf.replace(&format!("\"_INBOX.{}.>\"", u.nkey), "\"_INBOX.>\"");
        }
        assert!(!conf.contains("\"_INBOX.U"), "{conf}");
        conf
    }) else {
        return;
    };
    hub.bus("hub-admin").await.provision().await.unwrap();
    let seat_bus = hub.bus("build-1").await;
    seat_bus.state_put("mode", "normal").await.unwrap();
    assert_eq!(
        seat_bus.state_get("mode").await.unwrap().unwrap().value,
        "normal"
    );
    seat_bus.hosts().await.unwrap();
    let console = hub.bus("console").await;
    console
        .console_put("session.y", &serde_json::json!({"csrf": "y"}))
        .await
        .unwrap();
    assert!(
        console
            .console_get::<serde_json::Value>("session.y")
            .await
            .unwrap()
            .is_some()
    );
    // `doctor`'s probe sees the open `_INBOX.>`.
    for who in ["build-1", "console"] {
        assert_eq!(hub.shared_inbox(who, false).await, Some(true), "{who}");
    }
}
