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
            ("console", Role::Console),
            ("hub-admin", Role::Admin),
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
            render_conf(
                "127.0.0.1",
                port,
                &d.path().join("store"),
                &Auth::Users(&reg),
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

    async fn raw(&self, who: &str) -> async_nats::Client {
        let Credential::Nkey { seed, .. } = self.creds[who].clone() else {
            unreachable!()
        };
        async_nats::ConnectOptions::with_nkey(seed)
            .connect(&self.url)
            .await
            .unwrap()
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
