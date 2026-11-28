use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse as _;
use axum::{
    Json, Router,
    routing::{get, post},
};
use hgl_ai_bus::{Credential, Feed};
use hgl_ai_install::nats::{Auth, render_conf};
use hgl_ai_install::users::{Registry, Role};
use hgl_ai_owner::{Ask, Priority, QOption, ThreadState};
use hgl_ai_proto::{Kind, Note, STREAM_OWNER};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_websockets::ServerBuilder;

use super::*;

fn settings() -> Slack {
    Slack {
        team: "TQA".into(),
        channel: "CQA".into(),
        user: "UOWNER".into(),
        handle: "The Owner".into(),
    }
}

fn reply(id: &str, parent: &str, ts: &str) -> Value {
    json!({
        "type":"events_api", "envelope_id":id,
        "payload":{"type":"event_callback", "team_id":"TQA",
            "event":{"type":"message", "channel_type":"group", "channel":"CQA",
                     "user":"UOWNER", "ts":ts, "thread_ts":parent, "text":"Close the question"}}
    })
}

fn refused() -> [(&'static str, Value); 13] {
    [
        ("/payload/team_id", json!("TOTHER")),
        ("/payload/event/channel", json!("COTHER")),
        ("/payload/event/channel_type", json!("channel")),
        ("/payload/event/user", json!("UOTHER")),
        ("/payload/event/user", json!("UQABOT")),
        ("/payload/event/user_team", json!("TOTHER")),
        ("/payload/event/source_team", json!("TOTHER")),
        ("/payload/event/bot_id", json!("BBOT")),
        ("/payload/event/app_id", json!("AOTHER")),
        ("/payload/event/subtype", json!("message_changed")),
        ("/payload/event/hidden", json!(true)),
        ("/payload/event/thread_ts", Value::Null),
        ("/payload/event/ts", json!("1720000000.000000")),
    ]
}

fn replace(mut envelope: Value, path: &str, value: Value) -> Value {
    let (parent, key) = path.rsplit_once('/').unwrap();
    envelope.pointer_mut(parent).unwrap()[key] = value;
    envelope
}

#[test]
fn only_the_allowlisted_human_private_thread_reply_is_accepted() {
    let cfg = settings();
    let bot = Bot {
        team_id: cfg.team.clone(),
        user_id: "UQABOT".into(),
        bot_id: "BQABOT".into(),
    };
    let good = reply("a", "1720000000.000000", "1720000001.000001");
    assert!(owner_reply(&serde_json::from_value(good.clone()).unwrap(), &cfg, &bot).is_some());
    for (path, value) in refused() {
        let bad = replace(good.clone(), path, value);
        assert!(
            owner_reply(&serde_json::from_value(bad).unwrap(), &cfg, &bot).is_none(),
            "{path}"
        );
    }
}

#[test]
fn token_file_must_be_private_regular_and_owned_by_the_reader() {
    let dir = tempfile::tempdir().unwrap();
    let bot = dir.path().join(BOT_TOKEN_FILE);
    std::fs::write(&bot, "xoxb-test-only\n").unwrap();
    std::fs::set_permissions(&bot, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(secret(dir.path(), BOT_TOKEN_FILE, "xoxb-").is_err());
    std::fs::set_permissions(&bot, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        secret(dir.path(), BOT_TOKEN_FILE, "xoxb-").unwrap(),
        "xoxb-test-only"
    );
    std::os::unix::fs::symlink(&bot, dir.path().join(APP_TOKEN_FILE)).unwrap();
    assert!(secret(dir.path(), APP_TOKEN_FILE, "xoxb-").is_err());
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        // SAFETY: this signals the child this fixture started.
        unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        let _ = self.0.wait();
    }
}

struct Hub {
    dir: tempfile::TempDir,
    port: u16,
    creds: BTreeMap<&'static str, Credential>,
}

impl Hub {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut registry = Registry::default();
        let creds = [
            ("seat", Role::Seat),
            ("console", Role::Console),
            ("slack", Role::Slack),
            ("admin", Role::Admin),
        ]
        .into_iter()
        .map(|(id, role)| {
            let text = registry.issue(id, role, false).unwrap();
            (id, hgl_ai_bus::parse_credential(&text).unwrap().unwrap())
        })
        .collect();
        let store = dir.path().join("store");
        std::fs::create_dir(&store).unwrap();
        std::fs::write(
            dir.path().join("nats.conf"),
            render_conf("127.0.0.1", port, &store, &Auth::Users(&registry)),
        )
        .unwrap();
        Self { dir, port, creds }
    }

    async fn start(&self) -> Server {
        let server = Server(
            Command::new("nats-server")
                .arg("-c")
                .arg(self.dir.path().join("nats.conf"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        timeout(Duration::from_secs(5), async {
            while tokio::net::TcpStream::connect(("127.0.0.1", self.port))
                .await
                .is_err()
            {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("nats-server did not start");
        server
    }

    async fn bus(&self, id: &str) -> Bus {
        Bus::connect_with(
            &format!("nats://127.0.0.1:{}", self.port),
            Some(self.creds[id].clone()),
        )
        .await
        .unwrap()
    }
}

#[derive(Default)]
struct Posts {
    items: Vec<Value>,
    lose_response: bool,
}

#[derive(Clone)]
struct SlackState {
    socket_url: String,
    posts: Arc<Mutex<Posts>>,
}

struct FakeSlack {
    state: SlackState,
    base: String,
    listener: TcpListener,
    http: JoinHandle<()>,
}

impl FakeSlack {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = SlackState {
            socket_url: format!("ws://{}", listener.local_addr().unwrap()),
            posts: Default::default(),
        };
        let routes = Router::new()
            .route(
                "/apps.connections.open",
                post(
                    |State(state): State<SlackState>, headers: HeaderMap| async move {
                        assert_eq!(headers["authorization"], "Bearer xapp-test");
                        Json(json!({ "ok": true, "url": state.socket_url }))
                    },
                ),
            )
            .route(
                "/auth.test",
                post(|headers: HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer xoxb-test");
                    (
                        [("x-oauth-scopes", "chat:write,groups:history,incoming-webhook,app_mentions:read")],
                        Json(json!({"ok":true, "team_id":"TQA", "user_id":"UQABOT", "bot_id":"BQABOT"})),
                    )
                }),
            )
            .route(
                "/chat.postMessage",
                post(
                    |State(state): State<SlackState>,
                     headers: HeaderMap,
                     Json(mut body): Json<Value>| async move {
                        assert_eq!(headers["authorization"], "Bearer xoxb-test");
                        assert_eq!(body["channel"], "CQA");
                        assert_eq!(body["mrkdwn"], false);
                        let mut posts = state.posts.lock().unwrap();
                        let ts = format!("{}.{:06}", Utc::now().timestamp(), posts.items.len() + 1);
                        body["ts"] = json!(ts);
                        body["user"] = json!("UQABOT");
                        posts.items.push(body);
                        if std::mem::take(&mut posts.lose_response) {
                            return StatusCode::BAD_GATEWAY.into_response();
                        }
                        Json(json!({ "ok":true, "ts":ts, "channel":"CQA" })).into_response()
                    },
                ),
            )
            .route("/conversations.history", get(history))
            .route("/conversations.replies", get(history))
            .with_state(state.clone());
        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", http_listener.local_addr().unwrap());
        let http = tokio::spawn(async move { axum::serve(http_listener, routes).await.unwrap() });
        Self {
            state,
            base,
            listener,
            http,
        }
    }

    async fn adapter<'a>(&self, bus: &'a Bus) -> Adapter<'a> {
        Adapter::new(
            bus,
            settings(),
            Web::new(self.base.clone(), "xapp-test".into(), "xoxb-test".into()).unwrap(),
        )
        .await
        .unwrap()
    }

    fn lose_next_response(&self) {
        self.state.posts.lock().unwrap().lose_response = true;
    }

    fn items(&self) -> Vec<Value> {
        self.state.posts.lock().unwrap().items.clone()
    }

    async fn send(&self, adapter: &Adapter<'_>, envelopes: Vec<Value>) {
        let server = async {
            let (stream, _) = self.listener.accept().await.unwrap();
            let mut ws = ServerBuilder::new().accept(stream).await.unwrap();
            ws.send(Message::text(json!({"type":"hello"}).to_string()))
                .await
                .unwrap();
            for envelope in envelopes {
                ws.send(Message::text(envelope.to_string())).await.unwrap();
                let ack = ws.next().await.unwrap().unwrap();
                let ack: Value = serde_json::from_str(ack.as_text().unwrap()).unwrap();
                assert_eq!(ack, json!({"envelope_id": envelope["envelope_id"]}));
            }
            ws.send(Message::close(None, "")).await.unwrap();
        };
        let ((), result) = timeout(Duration::from_secs(5), async {
            tokio::join!(server, adapter.socket_session())
        })
        .await
        .expect("adapter did not acknowledge a Slack envelope");
        result.unwrap();
    }
}

impl Drop for FakeSlack {
    fn drop(&mut self) {
        self.http.abort();
    }
}

async fn history(
    State(state): State<SlackState>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Json<Value> {
    assert_eq!(headers["authorization"], "Bearer xoxb-test");
    assert_eq!(query["channel"], "CQA");
    assert_eq!(query["include_all_metadata"], "true");
    let posts = state.posts.lock().unwrap();
    let items: Vec<_> = posts
        .items
        .iter()
        .filter(|item| {
            item["thread_ts"].as_str() == query.get("ts").map(String::as_str)
                && query
                    .get("latest")
                    .is_none_or(|ts| item["ts"] == ts.as_str())
        })
        .collect();
    Json(json!({ "ok":true, "messages":items, "has_more":false }))
}

async fn next(feed: &mut Feed) -> StreamMsg {
    feed.next(Duration::from_secs(3))
        .await
        .unwrap()
        .expect("event in durable feed")
}

#[tokio::test]
async fn question_and_replies_survive_slack_and_jetstream_restart() {
    if Command::new("nats-server")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: nats-server is unavailable");
        return;
    }
    let hub = Hub::new();
    let fake = FakeSlack::start().await;
    let asker = Seat::new("seat").unwrap();
    let question;
    let root;
    let first;
    {
        let _server = hub.start().await;
        hub.bus("admin").await.provision().await.unwrap();
        let slack = hub.bus("slack").await;
        let adapter = fake.adapter(&slack).await;
        let mut feed = adapter.open_feed().await.unwrap();
        question = hgl_ai_owner::ask(
            &hub.bus("seat").await,
            &asker,
            Ask {
                title: "Pick a colour".into(),
                priority: Priority::P2,
                issue: Some("repo#1".into()),
                context: "The console has the record.".into(),
                question: "Blue or green?".into(),
                options: vec![QOption {
                    label: "a".into(),
                    line: "Blue".into(),
                }],
                suggested: Some("a".into()),
            },
        )
        .await
        .unwrap();
        fake.lose_next_response();
        let ask = next(&mut feed).await;
        assert!(adapter.deliver(&ask).await.is_err());
        ask.nak(Duration::from_millis(10)).await.unwrap();
        let items = fake.items();
        assert_eq!(items.len(), 1);
        assert!(
            items[0]["text"]
                .as_str()
                .unwrap()
                .contains("Blue or green?")
        );
        root = items[0]["ts"].as_str().unwrap().to_owned();
        assert!(
            slack
                .slack_get::<String>(&adapter.key("thread", &timestamp_key(&root)))
                .await
                .unwrap()
                .is_none()
        );
        first = format!("{}.100000", root.split_once('.').unwrap().0);
        // The Web API receipt was lost. The owner's immediate reply still maps to the
        // durable question attempt, and refusals leave no owner event.
        let mut invalid = Vec::new();
        for (path, value) in refused() {
            invalid.push(replace(reply("reject", &root, &first), path, value));
        }
        invalid.push(reply("unmapped", "1720000000.000001", &first));
        fake.send(&adapter, invalid).await;
        let admin = hub.bus("admin").await;
        assert!(
            admin
                .history(
                    STREAM_OWNER,
                    hgl_ai_proto::SLACK_OWNER_FILTER,
                    DateTime::UNIX_EPOCH,
                    10
                )
                .await
                .unwrap()
                .is_empty()
        );
        fake.send(&adapter, vec![reply("first", &root, &first)])
            .await;
    }

    let _server = hub.start().await;
    let slack = hub.bus("slack").await;
    let adapter = fake.adapter(&slack).await;
    let mut feed = adapter.open_feed().await.unwrap();
    let ask = next(&mut feed).await;
    adapter.deliver(&ask).await.unwrap();
    ask.ack().await.unwrap();
    assert_eq!(
        fake.items().len(),
        1,
        "recover the posted root instead of posting on restart"
    );
    assert_eq!(
        adapter.question_for(&root).await.unwrap().as_deref(),
        Some(question.as_str())
    );

    let second = format!("{}.100001", root.split_once('.').unwrap().0);
    fake.send(
        &adapter,
        vec![
            reply("retried", &root, &first),
            reply("second", &root, &second),
        ],
    )
    .await;
    let console = hub.bus("console").await;
    let mut console_feed = console
        .owner_feed(
            "console",
            vec![format!("{CONSOLE_PREFIX}.>"), format!("{OWNER_PREFIX}.>")],
        )
        .await
        .unwrap();
    for _ in 0..3 {
        let event = next(&mut console_feed).await;
        assert!(matches!(
            hgl_ai_owner::apply(&console, &asker, &event).await.unwrap(),
            hgl_ai_owner::Applied::Folded { .. }
        ));
        event.ack().await.unwrap();
    }
    assert!(
        console_feed
            .next(Duration::from_millis(200))
            .await
            .unwrap()
            .is_none()
    );
    let thread = hgl_ai_owner::show(&console, &question)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(thread.state, ThreadState::Open);
    assert_eq!(thread.messages.len(), 2);
    assert!(
        thread
            .messages
            .iter()
            .all(|m| m.handle.as_deref() == Some("The Owner") && m.body == "Close the question")
    );
    let seat_bus = hub.bus("seat").await;
    let (mut partial, revision) = console
        .thread_get::<hgl_ai_owner::Thread>(&question)
        .await
        .unwrap()
        .unwrap();
    partial.older = partial.messages.len() as u64;
    partial.messages.clear();
    console
        .thread_put(&question, &partial, revision)
        .await
        .unwrap();
    let rebuilt = hgl_ai_owner::show_full(&seat_bus, &question)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rebuilt.messages, thread.messages,
        "Slack events survive a full thread replay"
    );
    let mut inbox = seat_bus.inbox(&asker).await.unwrap();
    assert_eq!(
        inbox.fetch(2, Duration::from_secs(1)).await.unwrap().len(),
        2,
        "identical Slack replies within a second deliver distinct owner notes"
    );
    let slack_subject = event::slack_subject(&question, "TQA", "CQA", &timestamp_key(&first));
    let forged = Body::Answer {
        choice: Choice::Accept,
        option: None,
        text: String::new(),
        note: String::new(),
    };
    let forged = Event {
        v: event::EVENT_VERSION,
        id: question.clone(),
        sender: OWNER.into(),
        user: None,
        handle: None,
        at: hgl_ai_proto::utc_now_stamp(),
        body: forged,
    };
    let encoded = serde_json::to_vec(&forged).unwrap();
    assert!(matches!(
        slack
            .publish_event(&forged.subject(), &encoded, &forged.msg_id())
            .await,
        Err(BusError::Denied { .. })
    ));
    assert!(matches!(
        seat_bus.publish_slack_event(&slack_subject, &encoded).await,
        Err(BusError::Denied { .. })
    ));
    assert!(matches!(
        slack
            .send(&Note::new(
                Seat::owner(),
                asker.clone(),
                Kind::Note,
                "forged"
            ))
            .await,
        Err(BusError::Denied { .. })
    ));
    assert!(slack.thread_put(&question, &thread, 0).await.is_err());
    assert!(
        hgl_ai_owner::fold(
            Some(&thread),
            &slack_subject,
            thread.seq + 1,
            &forged,
            &asker
        )
        .is_err()
    );

    hgl_ai_owner::owner_event(
        &console,
        &asker,
        &question,
        "console-owner",
        Some("The Owner"),
        Body::OwnerMessage {
            body: "Follow-up from the console".into(),
        },
    )
    .await
    .unwrap();
    hgl_ai_owner::reply(&seat_bus, &asker, &question, "Follow-up from the asker")
        .await
        .unwrap();
    let from_console = next(&mut feed).await;
    fake.lose_next_response();
    assert!(adapter.deliver(&from_console).await.is_err());
    adapter.deliver(&from_console).await.unwrap();
    from_console.ack().await.unwrap();
    let from_seat = next(&mut feed).await;
    adapter.deliver(&from_seat).await.unwrap();
    adapter.deliver(&from_seat).await.unwrap();
    from_seat.ack().await.unwrap();
    let items = fake.items();
    assert_eq!(items.len(), 3);
    assert_eq!(items[1]["thread_ts"], root);
    assert_eq!(items[1]["text"], "The Owner\nFollow-up from the console");
    assert_eq!(items[2]["thread_ts"], root);
    assert_eq!(items[2]["text"], "seat\nFollow-up from the asker");
}
