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
    /// One account per role (hgl-ai#141), not one account with permissions.
    accounts: bool,
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
        Hub::start_layout(false, &[], edit)
    }

    /// The hub with one account per role (hgl-ai#141).
    fn accounts() -> Option<Hub> {
        Hub::start_layout(true, &[], |_, conf| conf)
    }

    /// [`Hub::accounts`] with the users `omit` not in the registry yet (their credentials
    /// are issued): roles that have no user.
    fn accounts_without(omit: &[&str]) -> Option<Hub> {
        Hub::start_layout(true, omit, |_, conf| conf)
    }

    fn auth(accounts: bool, reg: &Registry) -> Auth<'_> {
        if accounts {
            Auth::Accounts(reg)
        } else {
            Auth::Users(reg)
        }
    }

    fn start_layout(
        accounts: bool,
        omit: &[&str],
        edit: impl Fn(&Registry, String) -> String,
    ) -> Option<Hub> {
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
        for name in omit {
            assert!(reg.remove(name), "{name}");
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
                    &Hub::auth(accounts, &reg),
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
            accounts,
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
                &Hub::auth(self.accounts, &reg),
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
        "KV_HGL_METRICS",
        Some("idle"),
        None,
        &format!("{deliver}.i"),
        Some("$KV.HGL_METRICS.>"),
        &[],
    );
    body["config"]["inactive_threshold"] = 86_400_000_000_000u64.into();
    let r = ask(
        &raw,
        "$JS.API.CONSUMER.CREATE.KV_HGL_METRICS.idle.$KV.HGL_METRICS.>",
        body,
    )
    .await
    .expect("the API answers");
    assert!(!r.contains("\"error\""), "{r}");
    assert_eq!(
        hgl_ai_install::hub::lasting_consumers(&js).await.unwrap(),
        vec![
            "KV_HGL_HOSTS/kept".to_string(),
            "KV_HGL_METRICS/idle".to_string()
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
    for who in ["build-1", "console", "hub-coord", "hub-slack", "viewer"] {
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
    for who in ["build-1", "console", "hub-slack"] {
        assert_eq!(hub.shared_inbox(who, false).await, Some(true), "{who}");
    }
}

// ---- One account per role (hgl-ai#141) ----------------------------------------------
//
// Raw JetStream requests under the hub's API prefix: `bus` uses it from hgl-ai#144.

use async_nats::jetstream::{self, consumer, kv};
use hgl_ai_install::users::{Target, access_table, local_subject};

/// The user of each role with an account.
const ROLE_USERS: [(Role, &str); 5] = [
    (Role::Seat, "build-1"),
    (Role::Coord, "hub-coord"),
    (Role::Builder, "lab1-builder"),
    (Role::Console, "console"),
    (Role::Slack, "hub-slack"),
];

/// A NATS subject pattern matches `subject`.
fn matches(pattern: &str, subject: &str) -> bool {
    let (p, s): (Vec<&str>, Vec<&str>) =
        (pattern.split('.').collect(), subject.split('.').collect());
    for (i, t) in p.iter().enumerate() {
        match *t {
            ">" => return s.len() > i,
            "*" if i < s.len() => {}
            t if i < s.len() && s[i] == t => {}
            _ => return false,
        }
    }
    p.len() == s.len()
}

/// `pattern` with each wildcard token `token`.
fn sample(pattern: &str, token: &str) -> String {
    pattern
        .split('.')
        .map(|t| if t == "*" || t == ">" { token } else { t })
        .collect::<Vec<_>>()
        .join(".")
}

/// The subject a write row publishes on in `HUB`.
fn write_pattern(t: &Target) -> Option<String> {
    match t {
        Target::Keys { bucket, keys } => Some(format!("$KV.{bucket}.{keys}")),
        Target::Subjects(s) => Some(s.clone()),
        _ => None,
    }
}

/// What `role` may write, as `HUB` subject patterns.
fn writes_of(role: Role) -> Vec<String> {
    access_table()
        .iter()
        .filter(|r| r.of(role).writes())
        .filter_map(|r| write_pattern(&r.target))
        .collect()
}

impl Hub {
    /// `who`'s JetStream context in its own account, through the hub's API prefix.
    async fn js_of(&self, who: &str) -> (async_nats::Client, jetstream::Context) {
        let c = self.raw(who).await;
        let js = jetstream::with_prefix(c.clone(), hgl_ai_proto::HUB_API_PREFIX);
        (c, js)
    }

    /// The admin's own JetStream, in `HUB`.
    async fn admin_js(&self) -> jetstream::Context {
        let Credential::Nkey { seed, .. } = self.creds["hub-admin"].clone() else {
            unreachable!()
        };
        let c = async_nats::ConnectOptions::with_nkey(seed)
            .custom_inbox_prefix(self.inbox("hub-admin"))
            .connect(&self.url)
            .await
            .unwrap();
        jetstream::new(c)
    }
}

/// The hub provisioned, with the verdict bucket (hgl-ai#145), two keys in every bucket,
/// two messages in every stream, and the Slack adapter's link and delivery keys.
async fn seeded(hub: &Hub) -> jetstream::Context {
    hub.bus("hub-admin").await.provision().await.unwrap();
    let js = hub.admin_js().await;
    js.create_key_value(kv::Config {
        bucket: hgl_ai_install::users::KV_VERDICTS.into(),
        history: 5,
        ..Default::default()
    })
    .await
    .unwrap();
    let buckets: std::collections::BTreeSet<String> = access_table()
        .into_iter()
        .filter_map(|r| match r.target {
            Target::Keys { bucket, .. } => Some(bucket),
            _ => None,
        })
        .collect();
    for bucket in buckets {
        {
            let store = js.get_key_value(&bucket).await.unwrap();
            for k in ["seed.a", "seed.b", "links.a.b", "d.a.b.c"] {
                store.put(k, "seed".into()).await.unwrap();
            }
        }
    }
    for s in ["hgl.notes.seed", "hgl.owner.thread.seed", "hgl.usage.seed"] {
        for _ in 0..2 {
            js.publish(s, "seed".into()).await.unwrap().await.unwrap();
        }
    }
    js
}

/// Whether anything is stored on `subject` in `HUB`; every subject a role could write is
/// in some stream. Absent only when the stream answers "no message found": any other
/// error fails the test, so a denial proof cannot pass on a failed read.
async fn stored(admin: &jetstream::Context, subject: &str) -> bool {
    use async_nats::jetstream::stream::LastRawMessageErrorKind;
    let name = admin
        .stream_by_subject(subject)
        .await
        .unwrap_or_else(|e| panic!("no stream holds {subject}: {e}"));
    let stream = admin.get_stream(&name).await.unwrap();
    match stream.get_last_raw_message_by_subject(subject).await {
        Ok(_) => true,
        Err(e) if e.kind() == LastRawMessageErrorKind::NoMessageFound => false,
        Err(e) => panic!("reading {subject} back from {name}: {e}"),
    }
}

/// Up to `n` messages of one pull, within two seconds.
async fn pull(c: &consumer::Consumer<consumer::pull::Config>, n: usize) -> Vec<jetstream::Message> {
    let mut b = c
        .batch()
        .max_messages(n)
        .expires(Duration::from_secs(2))
        .messages()
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(m) = b.next().await {
        out.push(m.unwrap());
    }
    out
}

/// Every role reads and writes all its row gives it, across the account boundary: the
/// hub's account info, buckets by key, by sequence and through a pull consumer, streams
/// through a durable with acknowledgement, its feed, `HGL_OWNER` by sequence, and every
/// write by publish and by bucket put.
#[tokio::test]
async fn accounts_every_role_reads_and_writes_its_rows() {
    let Some(hub) = Hub::accounts() else { return };
    let admin = seeded(&hub).await;
    for (role, who) in ROLE_USERS {
        let (conn, js) = hub.js_of(who).await;
        for row in access_table() {
            let a = row.of(role);
            let what = format!("{who} ({role}) {:?} {a:?}", row.target);
            if a.reads() {
                match &row.target {
                    Target::AccountInfo => {
                        assert!(js.query_account().await.unwrap().streams > 0, "{what}");
                    }
                    Target::Keys { bucket, keys } if keys != ">" => {
                        // Some keys: a direct get by subject, and no more: no other key,
                        // no stream state (counts, key names), no consumer.
                        let p = hgl_ai_proto::HUB_API_PREFIX;
                        let s = format!("KV_{bucket}");
                        let get = |key: &str| {
                            let subject = format!("{p}.DIRECT.GET.{s}.$KV.{bucket}.{key}");
                            let c = conn.clone();
                            async move {
                                tokio::time::timeout(
                                    Duration::from_secs(1),
                                    c.request(subject, "".into()),
                                )
                                .await
                                .ok()
                                .and_then(|r| r.ok())
                                .map(|m| String::from_utf8_lossy(&m.payload).into_owned())
                            }
                        };
                        assert_eq!(get("links.a.b").await.as_deref(), Some("seed"), "{what}");
                        assert_eq!(get("d.a.b.c").await, None, "{what}");
                        unanswered(
                            &conn,
                            vec![
                                (format!("{p}.STREAM.INFO.{s}"), serde_json::json!({})),
                                (
                                    format!("{p}.STREAM.INFO.{s}"),
                                    serde_json::json!({"subjects_filter": ">"}),
                                ),
                                (format!("{p}.DIRECT.GET.{s}"), serde_json::json!({"seq": 1})),
                                (
                                    format!("{p}.STREAM.MSG.GET.{s}"),
                                    serde_json::json!({"seq": 1}),
                                ),
                                (
                                    format!("{p}.CONSUMER.CREATE.{s}"),
                                    serde_json::json!({"stream_name": s, "config": {}}),
                                ),
                            ],
                        )
                        .await;
                    }
                    Target::Keys { bucket, .. } => {
                        let store = js.get_key_value(bucket).await.expect(&what);
                        assert_eq!(
                            store.get("seed.a").await.expect(&what).unwrap(),
                            "seed",
                            "{what}"
                        );
                        {
                            let s = js.get_stream(format!("KV_{bucket}")).await.unwrap();
                            let first = s.cached_info().state.first_sequence;
                            s.direct_get(first).await.expect(&what);
                            s.get_raw_message(first).await.expect(&what);
                            let c = s
                                .create_consumer(consumer::pull::Config {
                                    ack_policy: consumer::AckPolicy::None,
                                    filter_subject: format!("$KV.{bucket}.seed.>"),
                                    ..Default::default()
                                })
                                .await
                                .expect(&what);
                            assert_eq!(pull(&c, 2).await.len(), 2, "a batch: {what}");
                        }
                    }
                    Target::Stream(name)
                    | Target::BySequence(name)
                    | Target::Feed { stream: name, .. } => {
                        let s = js.get_stream(name).await.expect(&what);
                        if !matches!(row.target, Target::Feed { .. }) {
                            s.get_raw_message(1).await.expect(&what);
                        }
                        let durable = match &row.target {
                            Target::Feed { consumer, .. } => Some(consumer.clone()),
                            Target::Stream(_) => Some(format!("probe-{role}")),
                            _ => None,
                        };
                        let Some(durable) = durable else { continue };
                        let c = s
                            .create_consumer(consumer::pull::Config {
                                durable_name: Some(durable.clone()),
                                ack_policy: consumer::AckPolicy::Explicit,
                                ..Default::default()
                            })
                            .await
                            .expect(&what);
                        let got = pull(&c, 2).await;
                        assert_eq!(got.len(), 2, "a batch: {what}");
                        for m in &got {
                            m.double_ack().await.expect(&what);
                        }
                        let info = admin
                            .get_stream(name)
                            .await
                            .unwrap()
                            .consumer_info(&durable)
                            .await
                            .unwrap();
                        assert_eq!(info.num_ack_pending, 0, "{what}");
                        assert_eq!(
                            info.ack_floor.stream_sequence,
                            got.last().unwrap().info().unwrap().stream_sequence,
                            "{what}"
                        );
                    }
                    Target::Subjects(_) => unreachable!(),
                }
            }
            if a.writes() {
                let subject = sample(&write_pattern(&row.target).unwrap(), &format!("ok-{role}"));
                match &row.target {
                    Target::Keys { bucket, .. } => {
                        let key = subject.strip_prefix(&format!("$KV.{bucket}.")).unwrap();
                        js.get_key_value(bucket)
                            .await
                            .expect(&what)
                            .put(key, "ok".into())
                            .await
                            .expect(&what);
                    }
                    _ => {
                        js.publish(subject.clone(), "ok".into())
                            .await
                            .expect(&what)
                            .await
                            .expect(&what);
                    }
                }
                assert!(stored(&admin, &subject).await, "{what}");
            }
        }
        // `doctor`'s `telemetry consumers` check lists the telemetry streams' consumers
        // (hgl-ai#105); Slack reads no telemetry.
        if role != Role::Slack {
            hgl_ai_install::hub::lasting_consumers(&js)
                .await
                .unwrap_or_else(|e| panic!("{who} ({role}): {e}"));
        }
    }
}

/// hgl-ai#141: no role writes outside its rows, neither by publishing nor by having
/// JetStream reply onto the subject (a request's reply, a pull's, a publish
/// acknowledgement's), nor through a push consumer; nor does any role reach the admin's
/// stream operations.
#[tokio::test]
async fn accounts_refuse_every_write_outside_a_row() {
    let Some(hub) = Hub::accounts() else { return };
    let admin = seeded(&hub).await;
    let patterns: Vec<String> = access_table()
        .iter()
        .filter_map(|r| write_pattern(&r.target))
        .collect();
    for (role, who) in ROLE_USERS {
        let (c, mut rx) = spy(&hub, who).await;
        let js = jetstream::with_prefix(c.clone(), hgl_ai_proto::HUB_API_PREFIX);
        let mine = writes_of(role);
        let token = format!("probe-{role}");
        let denied: Vec<String> = patterns
            .iter()
            .map(|p| sample(p, &token))
            .filter(|s| !mine.iter().any(|m| matches(m, s)))
            .collect();
        assert!(!denied.is_empty(), "{who}");

        // Directly, under the prefix and by the hub's own name.
        let mut tried = std::collections::BTreeSet::new();
        for s in &denied {
            for subject in [local_subject(s), s.clone()] {
                if tried.insert(subject.clone()) {
                    c.publish(subject, "forged".into()).await.unwrap();
                }
            }
        }
        c.flush().await.unwrap();
        let got = refusals(&mut rx, tried.len()).await;
        for subject in &tried {
            assert!(
                got.iter().any(|e| e.contains(&format!("\"{subject}\""))),
                "{who} published {subject}: {got:?}"
            );
        }

        // By the reply route: requests the role may make, replying onto each subject.
        let mut requests = vec![(
            format!("{}.INFO", hgl_ai_proto::HUB_API_PREFIX),
            String::new(),
        )];
        if let Some(w) = mine.first() {
            requests.push((local_subject(&sample(w, &format!("ok-{role}"))), "x".into()));
        }
        let pullable = access_table().into_iter().find_map(|r| match r.target {
            Target::Stream(s) if r.of(role).reads() => Some((s, format!("probe-{role}"))),
            Target::Feed { stream, consumer } if r.of(role).reads() => Some((stream, consumer)),
            _ => None,
        });
        let (stream, durable) = pullable.unwrap();
        js.get_stream(&stream)
            .await
            .unwrap()
            .create_consumer(consumer::pull::Config {
                durable_name: Some(durable.clone()),
                ack_policy: consumer::AckPolicy::Explicit,
                ..Default::default()
            })
            .await
            .unwrap();
        requests.push((
            format!(
                "{}.CONSUMER.MSG.NEXT.{stream}.{durable}",
                hgl_ai_proto::HUB_API_PREFIX
            ),
            r#"{"batch":5,"expires":500000000}"#.into(),
        ));
        for (req, body) in &requests {
            for s in &denied {
                for reply in [s.clone(), local_subject(s)] {
                    c.publish_with_reply(req.clone(), reply, body.clone().into())
                        .await
                        .unwrap();
                }
            }
        }
        // A push consumer delivering a stored value onto each subject. A consumer's
        // create request does not say push or pull in its subject, so the hub creates it;
        // it delivers in `HUB`, where nothing but the admin's own inbox has interest.
        let kv_stream = access_table().into_iter().find_map(|r| match r.target {
            Target::Keys { bucket, keys } if keys == ">" && r.of(role).reads() => Some(bucket),
            _ => None,
        });
        if let Some(bucket) = kv_stream {
            let s = js.get_stream(format!("KV_{bucket}")).await.unwrap();
            // (Onto its own subjects the hub refuses it: "forms a cycle".)
            let own = format!("$KV.{bucket}.");
            for d in denied.iter().filter(|d| !d.starts_with(&own)) {
                let r = s
                    .create_consumer(consumer::push::Config {
                        deliver_subject: d.clone(),
                        filter_subject: format!("$KV.{bucket}.seed.a"),
                        ..Default::default()
                    })
                    .await;
                assert!(r.is_ok(), "{who}: {d}: {:?}", r.err());
            }
        }
        c.flush().await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        for s in &denied {
            assert!(!stored(&admin, s).await, "{who} ({role}) wrote {s}");
        }

        // The admin's operations.
        for op in [
            "STREAM.PURGE",
            "STREAM.DELETE",
            "STREAM.UPDATE",
            "STREAM.MSG.DELETE",
        ] {
            let local = format!("{}.{op}.HGL_NOTES", hgl_ai_proto::HUB_API_PREFIX);
            c.publish(local.clone(), "{\"seq\":1}".into())
                .await
                .unwrap();
            c.flush().await.unwrap();
            assert!(!refusals(&mut rx, 1).await.is_empty(), "{who}: {op}");
        }
        assert!(js.delete_stream("HGL_NOTES").await.is_err(), "{who}");
        assert!(
            js.create_stream(jetstream::stream::Config {
                name: "EVIL".into(),
                subjects: vec!["evil.>".into()],
                ..Default::default()
            })
            .await
            .is_err(),
            "{who}"
        );
    }
    let notes = admin.get_stream("HGL_NOTES").await.unwrap();
    assert!(notes.get_raw_message(1).await.is_ok(), "nothing purged");
    match admin.get_stream("EVIL").await {
        Err(e) => assert!(
            matches!(
                e.kind(),
                jetstream::context::GetStreamErrorKind::JetStream(j)
                    if j.error_code() == jetstream::ErrorCode::STREAM_NOT_FOUND
            ),
            "reading EVIL back: {e}"
        ),
        Ok(_) => panic!("a role created EVIL"),
    }
}

/// The parts #141 left unproven: acknowledgement (ack, progress, nak, redelivery), a
/// batch pull, and leases (`HGL_LOCKS` create and update) across accounts; and no role
/// account has JetStream of its own.
#[tokio::test]
async fn accounts_ack_batch_and_leases_work_across_accounts() {
    let Some(hub) = Hub::accounts() else { return };
    let admin = seeded(&hub).await;
    let (_c, js) = hub.js_of("build-1").await;
    for i in 0..4 {
        js.publish(format!("hgl.notes.w{i}"), "n".into())
            .await
            .unwrap()
            .await
            .unwrap();
    }
    let c = js
        .get_stream("HGL_NOTES")
        .await
        .unwrap()
        .create_consumer(consumer::pull::Config {
            durable_name: Some("inbox-w9".into()),
            filter_subjects: vec![
                "hgl.notes.w0".into(),
                "hgl.notes.w1".into(),
                "hgl.notes.w2".into(),
                "hgl.notes.w3".into(),
            ],
            ack_policy: consumer::AckPolicy::Explicit,
            ..Default::default()
        })
        .await
        .unwrap();
    let got = pull(&c, 10).await;
    assert_eq!(got.len(), 4, "one batch");
    got[0].ack_with(jetstream::AckKind::Progress).await.unwrap();
    got[0]
        .ack_with(jetstream::AckKind::Nak(None))
        .await
        .unwrap();
    for m in &got[1..] {
        m.double_ack().await.unwrap();
    }
    let again = pull(&c, 10).await;
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].info().unwrap().delivered, 2);
    again[0].double_ack().await.unwrap();
    let info = admin
        .get_stream("HGL_NOTES")
        .await
        .unwrap()
        .consumer_info("inbox-w9")
        .await
        .unwrap();
    assert_eq!((info.num_ack_pending, info.num_redelivered), (0, 0));

    // Leases: create once, update by revision, a stale update refused.
    for (role, who) in ROLE_USERS {
        let (_c, js) = hub.js_of(who).await;
        let key = format!("seat.{role}");
        if !role_writes_locks(role) {
            // The console reads the bucket; Slack does not see it.
            if let Ok(locks) = js.get_key_value("HGL_LOCKS").await {
                assert_eq!(role, Role::Console);
                assert!(locks.create(&key, "x".into()).await.is_err(), "{who}");
            }
            continue;
        }
        let locks = js.get_key_value("HGL_LOCKS").await.unwrap();
        let rev = locks.create(&key, who.into()).await.unwrap();
        assert!(
            locks.create(&key, "other".into()).await.is_err(),
            "{who}: held"
        );
        let rev2 = locks.update(&key, "refreshed".into(), rev).await.unwrap();
        assert!(
            locks.update(&key, "stale".into(), rev).await.is_err(),
            "{who}: stale"
        );
        assert_eq!(locks.get(&key).await.unwrap().unwrap(), "refreshed");
        locks
            .delete_expect_revision(&key, Some(rev2))
            .await
            .unwrap();
        assert!(locks.get(&key).await.unwrap().is_none());
        // And a bucket put and get.
        let state = js.get_key_value("HGL_STATE").await.unwrap();
        state.put(format!("k.{role}"), "v".into()).await.unwrap();
        assert_eq!(state.get(format!("k.{role}")).await.unwrap().unwrap(), "v");
    }

    // No role has JetStream of its own, and none reaches the hub's under `$JS.API`.
    for (_, who) in ROLE_USERS {
        let (c, mut rx) = spy(&hub, who).await;
        let own = jetstream::new(c.clone());
        let r = tokio::time::timeout(Duration::from_secs(3), own.query_account()).await;
        assert!(!matches!(r, Ok(Ok(_))), "{who}: {r:?}");
        assert!(!refusals(&mut rx, 1).await.is_empty(), "{who}");
    }
}

/// The first user of a role that had none needs only the reload `server user add` does
/// (r1 on #152): the role's account, its imports and `HUB`'s exports to it were rendered
/// before it had a user.
#[tokio::test]
async fn accounts_the_first_user_of_a_role_needs_one_reload() {
    let new = [("lab1-builder", Role::Builder), ("hub-slack", Role::Slack)];
    let Some(hub) = Hub::accounts_without(&new.map(|n| n.0)) else {
        return;
    };
    let admin = seeded(&hub).await;
    {
        let mut reg = hub.reg.lock().unwrap();
        for (name, role) in new {
            let nkey = nkeys::KeyPair::from_seed(&hub.seed(name))
                .unwrap()
                .public_key();
            reg.register(name, role, &nkey, false).unwrap();
        }
    }
    hub.reload();
    let (_c, builder) = hub.js_of("lab1-builder").await;
    assert!(builder.query_account().await.unwrap().streams > 0);
    let verdicts = builder
        .get_key_value(hgl_ai_install::users::KV_VERDICTS)
        .await
        .unwrap();
    verdicts.put("first", "ok".into()).await.unwrap();
    assert_eq!(verdicts.get("first").await.unwrap().unwrap(), "ok");
    let (_c, slack) = hub.js_of("hub-slack").await;
    let state = slack.get_key_value(hgl_ai_proto::KV_SLACK).await.unwrap();
    state.put("d.a.b.c", "first".into()).await.unwrap();
    assert_eq!(state.get("d.a.b.c").await.unwrap().unwrap(), "first");
    let subject = sample(hgl_ai_proto::SLACK_OWNER_FILTER, "first");
    slack
        .publish(subject.clone(), "ok".into())
        .await
        .unwrap()
        .await
        .unwrap();
    assert!(stored(&admin, &subject).await);
}

fn role_writes_locks(role: Role) -> bool {
    writes_of(role).iter().any(|w| w == "$KV.HGL_LOCKS.>")
}
