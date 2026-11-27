//! The owner channel end to end against a real `nats-server` with hub users (skipped
//! without one): an agent asks, the console folds and notifies, the signed-in owner
//! answers in the console, and the asker's inbox gets the answer from `owner`. A seat's
//! forged answer is refused.

mod common;

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::routing::post;
use common::*;
use hgl_ai_bus::{Bus, Credential};
use hgl_ai_config::Notify;
use hgl_ai_console::App;
use hgl_ai_install::nats::{Auth, render_conf};
use hgl_ai_install::users::{Registry, Role};
use hgl_ai_owner::{self as owner, Ask, Body, Event, Priority, QOption, ThreadState};
use hgl_ai_proto::{Kind, Seat};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Hub {
    _server: Server,
    _dir: tempfile::TempDir,
    url: String,
    creds: std::collections::BTreeMap<&'static str, Credential>,
}

fn start_hub() -> Option<Hub> {
    let bin = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("nats-server"))
        .find(|p| p.is_file());
    let Some(bin) = bin else {
        eprintln!("skipped: nats-server is not on PATH");
        return None;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut reg = Registry::default();
    let mut creds = std::collections::BTreeMap::new();
    for (n, r) in [
        ("hub", Role::Seat),
        ("console", Role::Console),
        ("hub-admin", Role::Admin),
    ] {
        let t = reg.issue(n, r, false).unwrap();
        creds.insert(n, hgl_ai_bus::parse_credential(&t).unwrap().unwrap());
    }
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let conf = dir.path().join("nats.conf");
    std::fs::create_dir_all(dir.path().join("store")).unwrap();
    std::fs::write(
        &conf,
        render_conf(
            "127.0.0.1",
            port,
            &dir.path().join("store"),
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
    let server = Server(child);
    let up = (0..200).any(|_| {
        std::thread::sleep(Duration::from_millis(50));
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    assert!(up, "nats-server did not start");
    Some(Hub {
        _server: server,
        _dir: dir,
        url: format!("nats://127.0.0.1:{port}"),
        creds,
    })
}

async fn until<T, F: std::future::Future<Output = Option<T>>>(
    what: &str,
    mut f: impl FnMut() -> F,
) -> T {
    for _ in 0..100 {
        if let Some(v) = f().await {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

/// A fake ntfy topic that records each push.
async fn ntfy() -> (String, Arc<Mutex<Vec<(String, String, String)>>>) {
    let seen: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
    let s = seen.clone();
    let app = axum::Router::new().route(
        "/hgl",
        post(move |h: axum::http::HeaderMap, body: String| {
            let s = s.clone();
            async move {
                let g = |k: &str| {
                    h.get(k)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string()
                };
                s.lock().unwrap().push((g("title"), g("click"), body));
                "ok"
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await });
    (format!("http://{addr}/hgl"), seen)
}

#[tokio::test]
async fn ask_answer_converse_and_refuse_forgery() {
    let Some(hub) = start_hub() else { return };
    let admin = Bus::connect_with(&hub.url, Some(hub.creds["hub-admin"].clone()))
        .await
        .unwrap();
    admin.provision().await.unwrap();
    let seat_bus = Bus::connect_with(&hub.url, Some(hub.creds["hub"].clone()))
        .await
        .unwrap();
    let console_bus = Bus::connect_with(&hub.url, Some(hub.creds["console"].clone()))
        .await
        .unwrap();

    let (pem, public) = apple_key();
    let providers = Providers::start(public).await;
    let mut h = Harness::new(&providers, &pem);
    let (topic, pushes) = ntfy().await;
    h.cfg.console.as_mut().unwrap().notify = vec![Notify::Ntfy {
        url: topic,
        token_file: None,
    }];
    h.cfg.hub.url = hub.url.clone();
    let app = Arc::new(App::new(h.cfg.clone()).unwrap());
    app.set_hub(Arc::new(console_bus));
    tokio::spawn(hgl_ai_console::service::start_background(app.clone()));
    let base = serve(app.clone()).await;
    let (mut b, _key) = signed_in(&base).await;
    let claude = Seat::new("claude").unwrap();
    let mut inbox = admin.inbox(&claude).await.unwrap();

    // The controller asks; the console folds it and pushes a p1 notification.
    let id = owner::ask(
        &seat_bus,
        &claude,
        Ask {
            title: "Pick the lowering rule".into(),
            priority: Priority::P1,
            issue: Some("hgl-compiler#980".into()),
            context: "Secret context that never leaves the console.".into(),
            question: "Which rule lowers first?".into(),
            options: vec![
                QOption::parse("a:The first").unwrap(),
                QOption::parse("b:The second").unwrap(),
            ],
            suggested: Some("a".into()),
        },
    )
    .await
    .unwrap();
    until("the fold", || async {
        owner::show(&seat_bus, &id).await.unwrap()
    })
    .await;
    let push = until("the push", || async {
        pushes.lock().unwrap().first().cloned()
    })
    .await;
    assert_eq!(push.0, "p1 question: Pick the lowering rule");
    assert_eq!(push.1, format!("{ORIGIN}/t/{id}"));
    assert!(
        !push.2.contains("Secret context"),
        "a notification never carries the context"
    );

    let page = b.get("/questions").await.text().await.unwrap();
    assert!(page.contains("Pick the lowering rule") && page.contains("Questions (1 open)"));
    let token = b.csrf(&format!("/t/{id}")).await;
    let r = b
        .post_form(
            &format!("/t/{id}/answer"),
            &[
                ("csrf", &token),
                ("choice", "accept"),
                ("option", ""),
                ("text", ""),
                ("note", "keep it simple"),
            ],
            Some(ORIGIN),
        )
        .await;
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let t = owner::show(&seat_bus, &id).await.unwrap().unwrap();
    assert_eq!(t.state, ThreadState::Answered);
    assert_eq!(t.answer.as_ref().unwrap().user, "owner@example.com");

    let got = inbox.fetch(10, Duration::from_secs(5)).await.unwrap();
    let ans: Vec<_> = got.iter().filter(|d| d.note.kind == Kind::Answer).collect();
    assert_eq!(ans.len(), 1);
    let n = &ans[0].note;
    assert!(n.sender.is_owner());
    assert_eq!((n.repo.as_deref(), n.issue), (Some("hgl-compiler"), 980));
    assert_eq!(n.by.as_deref(), Some("This Dude"));
    assert!(
        n.body.starts_with("[owner] Answer from This Dude:")
            && n.body
                .contains("accept a: The first (note: keep it simple) [This Dude, ")
            && n.body.contains("`This Dude, 20"),
        "{}",
        n.body
    );
    assert_eq!(
        t.answer.as_ref().unwrap().handle.as_deref(),
        Some("This Dude")
    );
    for d in &got {
        d.ack().await.unwrap();
    }
    let log = owner::decisions_tail(&seat_bus, 5).await.unwrap();
    assert!(
        log.iter()
            .any(|l| l.contains(&id) && l.contains("This Dude (owner@example.com): accept a")),
        "{log:?}"
    );

    // A seat forges the owner's answer to a second question on the subject it may use.
    let id2 = owner::ask(
        &seat_bus,
        &claude,
        Ask {
            title: "Second".into(),
            priority: Priority::P3,
            issue: None,
            context: String::new(),
            question: "Q?".into(),
            options: vec![],
            suggested: None,
        },
    )
    .await
    .unwrap();
    until("the second fold", || async {
        owner::show(&seat_bus, &id2).await.unwrap()
    })
    .await;
    let forged = Event {
        v: 1,
        id: id2.clone(),
        sender: "owner".into(),
        user: Some("owner@example.com".into()),
        handle: Some("This Dude".into()),
        at: hgl_ai_proto::utc_now_stamp(),
        body: Body::Answer {
            choice: hgl_ai_owner::Choice::Reject,
            option: None,
            text: String::new(),
            note: String::new(),
        },
    };
    seat_bus
        .publish_event(
            &format!("hgl.console.thread.{id2}"),
            &serde_json::to_vec(&forged).unwrap(),
            "forged-1",
        )
        .await
        .unwrap();
    // A reply after it, so we know the forgery was folded (or refused) first.
    let seq = owner::reply(&seat_bus, &claude, &id2, "after the forgery")
        .await
        .unwrap();
    let t2 = until("the reply", || async {
        owner::show(&seat_bus, &id2)
            .await
            .unwrap()
            .filter(|t| t.seq >= seq)
    })
    .await;
    assert_eq!(t2.state, ThreadState::Open, "the forged answer was refused");
    assert!(t2.answer.is_none());
    assert_eq!(
        pushes.lock().unwrap().len(),
        1,
        "p3 shows only in the console"
    );

    // A conversation from the owner reaches the controller; its reply shows in the thread.
    let token = b.csrf("/conversations").await;
    let r = b
        .post_form(
            "/conversations",
            &[
                ("csrf", &token),
                ("title", "Release timing"),
                ("issue", ""),
                ("body", "Can we tag this week?"),
            ],
            Some(ORIGIN),
        )
        .await;
    assert_eq!(r.status(), 303);
    let loc = r.headers()["location"].to_str().unwrap().to_string();
    let cid = loc.trim_start_matches("/t/").to_string();
    let got = inbox.fetch(10, Duration::from_secs(5)).await.unwrap();
    let conv = got
        .iter()
        .find(|d| d.note.body.contains("Can we tag this week?"))
        .expect("the controller's note");
    assert!(conv.note.sender.is_owner() && conv.note.kind == Kind::Note);
    for d in &got {
        d.ack().await.unwrap();
    }
    let seq = owner::reply(&seat_bus, &claude, &cid, "Thursday, after #980 lands.")
        .await
        .unwrap();
    until("the controller's reply", || async {
        owner::show(&seat_bus, &cid)
            .await
            .unwrap()
            .filter(|t| t.seq >= seq)
    })
    .await;
    let page = b.get(&loc).await.text().await.unwrap();
    assert!(
        page.contains("Thursday, after #980 lands.")
            && page.contains("This Dude, ")
            && page.contains(" (owner)")
    );
    let page = b
        .get("/conversations?q=release&state=open")
        .await
        .text()
        .await
        .unwrap();
    assert!(page.contains("1 found") && page.contains("Release timing"));

    // A status report shows on the front page.
    owner::status_post(&seat_bus, &claude, "**Landed** hgl-compiler#980.")
        .await
        .unwrap();
    let mut page = String::new();
    for _ in 0..100 {
        page = b.get("/").await.text().await.unwrap();
        if page.contains("**Landed** hgl-compiler#980.") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(page.contains("**Landed** hgl-compiler#980."), "{page}");
    assert!(page.contains("from claude"));

    // Telemetry reads the hub as the console's user.
    // The first reading may predate the questions; the next comes within 10 s.
    let mut snap = serde_json::Value::Null;
    for _ in 0..150 {
        snap = b.get("/telemetry.json").await.json().await.unwrap();
        if snap["open_questions"]["p3"] == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(snap["open_questions"]["p3"], 1, "{snap}");
    assert_eq!(snap["mode"], "normal");
    assert!(snap["error"].is_null(), "{}", snap["error"]);
}

/// A console restart keeps who is signed in and the pending p2 batch (both on the hub,
/// in `HGL_CONSOLE`); sign-out still ends a session. Seats cannot read it.
#[tokio::test]
async fn sessions_and_the_p2_batch_survive_a_restart() {
    let Some(hub) = start_hub() else { return };
    let admin = Bus::connect_with(&hub.url, Some(hub.creds["hub-admin"].clone()))
        .await
        .unwrap();
    admin.provision().await.unwrap();
    let (pem, public) = apple_key();
    let providers = Providers::start(public).await;
    let mut h = Harness::new(&providers, &pem);
    h.cfg.hub.url = hub.url.clone();
    let start = || async {
        let app = Arc::new(App::new(h.cfg.clone()).unwrap());
        let bus = Arc::new(
            Bus::connect_with(&hub.url, Some(hub.creds["console"].clone()))
                .await
                .unwrap(),
        );
        app.set_hub(bus.clone());
        hgl_ai_console::service::restore(&app, &bus).await;
        let base = serve(app.clone()).await;
        (app, base)
    };

    let (first, base) = start().await;
    let (mut b, _key) = signed_in(&base).await;
    assert_eq!(b.get("/telemetry").await.status(), 200);
    let t = owner::Thread {
        id: "q-0000000a".into(),
        kind: owner::ThreadKind::Question,
        title: "Later".into(),
        issue: None,
        priority: Some(Priority::P2),
        state: ThreadState::Open,
        seat: "claude".into(),
        created_at: "20261127T000000Z".into(),
        updated_at: "20261127T000000Z".into(),
        context: String::new(),
        question: "q".into(),
        options: vec![],
        suggested: None,
        answer: None,
        messages: vec![],
        seq: 1,
    };
    assert!(hgl_ai_console::service::on_new_question(&first, &t).is_none());
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The console restarts: the same cookie is still signed in, the batch still waits.
    let (second, base2) = start().await;
    b.base = base2;
    assert_eq!(b.get("/telemetry").await.status(), 200, "still signed in");
    let batch = hgl_ai_console::service::p2_batch(&second).unwrap();
    assert_eq!(batch.title, "p2 question: Later");
    hgl_ai_console::service::p2_sent(&second, &["q-0000000a".into()]);

    // Seats cannot read the console's state.
    let seat_bus = Bus::connect_with(&hub.url, Some(hub.creds["hub"].clone()))
        .await
        .unwrap();
    let read = tokio::time::timeout(
        Duration::from_secs(8),
        seat_bus.console_list::<serde_json::Value>("session."),
    )
    .await;
    assert!(
        !matches!(read, Ok(Ok(ref v)) if !v.is_empty()),
        "a seat read the sessions"
    );

    // Sign-out reaches the hub: a third start does not bring the session back, and
    // the sent batch stays sent.
    let token = b.csrf("/telemetry").await;
    let before = b.jar.clone();
    let r = b
        .post_form("/auth/signout", &[("csrf", &token)], Some(ORIGIN))
        .await;
    assert_eq!(r.status(), 303);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (third, base3) = start().await;
    // Replay the cookie from before the sign-out.
    let mut old = Browser::new(&base3);
    old.jar = before;
    assert_eq!(
        old.get("/telemetry").await.status(),
        303,
        "signed out stays signed out"
    );
    assert!(
        hgl_ai_console::service::p2_batch(&third).is_none(),
        "the batch was sent"
    );
    // A question no longer open (here: not in HGL_THREADS at all) leaves the batch
    // before it is sent.
    let mut gone = t.clone();
    gone.id = "q-0000000b".into();
    hgl_ai_console::service::on_new_question(&third, &gone);
    assert!(!hgl_ai_console::service::p2_round(&third, third.hub().map(|b| &**b)).await);
    assert!(
        hgl_ai_console::service::p2_batch(&third).is_none(),
        "pruned"
    );
}
