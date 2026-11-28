//! Hub users against a real `nats-server` from `PATH` (skipped without one), started with
//! the rendered `nats.conf`: each role's permissions hold, and no seat can make a note or
//! thread event that a seat's inbox or the console accepts as the owner's (OWN-5).

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures::StreamExt;
use hgl_ai_bus::{Bus, BusError, Credential};
use hgl_ai_install::nats::{Auth, render_conf};
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
            ("hub-admin", Role::Admin),
            ("lab2", Role::Seat),
            ("hub-slack", Role::Slack),
        ] {
            let text = reg.issue(name, role, false).unwrap();
            creds.insert(name, hgl_ai_bus::parse_credential(&text).unwrap().unwrap());
        }
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let conf = d.path().join("nats.conf");
        std::fs::create_dir_all(d.path().join("store")).unwrap();
        std::fs::write(
            &conf,
            edit(
                &reg,
                render_conf(
                    "127.0.0.1",
                    port,
                    &d.path().join("store"),
                    &Auth::Users(&reg),
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
        })
    }

    /// Rewrite `nats.conf` from the registry and reload the hub as `server user add`
    /// does: SIGHUP through the pid file.
    fn reload(&self) {
        let reg = self.reg.lock().unwrap();
        std::fs::write(
            &self.conf,
            render_conf("127.0.0.1", self.port, &self.store, &Auth::Users(&reg)),
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

    /// A plain client of `who`, with its own inbox prefix as the bus sets it.
    async fn raw(&self, who: &str) -> async_nats::Client {
        self.raw_with(who, async_nats::ConnectOptions::new()).await
    }

    async fn raw_with(&self, who: &str, opts: async_nats::ConnectOptions) -> async_nats::Client {
        let c = &self.creds[who];
        let Credential::Nkey { seed, .. } = c.clone() else {
            unreachable!()
        };
        opts.custom_inbox_prefix(c.inbox_prefix().unwrap())
            .nkey(seed)
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
}

fn seat(s: &str) -> Seat {
    Seat::new(s).unwrap()
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

const SECRET: &str = "csrf-0123456789";

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
