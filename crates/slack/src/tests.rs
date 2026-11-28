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
use hgl_ai_owner::{Ask, Choice, Priority, QOption, ThreadState};
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
    updates: usize,
    views: Vec<Value>,
    lose_response: bool,
    rate_limit_open: Option<u64>,
    socket_opens: usize,
    history_calls: usize,
    history_error: Option<String>,
    next_post_skew: i64,
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
                        let mut posts = state.posts.lock().unwrap();
                        posts.socket_opens += 1;
                        if let Some(after) = posts.rate_limit_open.take() {
                            return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", after.to_string())]).into_response();
                        }
                        Json(json!({ "ok": true, "url": state.socket_url })).into_response()
                    },
                ),
            )
            .route("/chat.getPermalink", get(|headers: HeaderMap, Query(query): Query<BTreeMap<String, String>>| async move {
                assert_eq!(headers["authorization"], "Bearer xoxb-test");
                assert_eq!(query["channel"], "CQA");
                let ts = query["message_ts"].replace('.', "");
                Json(json!({"ok":true, "permalink": format!("https://qa.slack.com/archives/CQA/p{ts}") }))
            }))
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
                        let timestamp = Utc::now().timestamp() + std::mem::take(&mut posts.next_post_skew);
                        let ts = format!("{timestamp}.{:06}", posts.items.len() + 1);
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
            .route(
                "/chat.update",
                post(
                    |State(state): State<SlackState>, headers: HeaderMap, Json(body): Json<Value>| async move {
                        assert_eq!(headers["authorization"], "Bearer xoxb-test");
                        assert_eq!(body["channel"], "CQA");
                        assert_eq!(body["mrkdwn"], false);
                        let mut posts = state.posts.lock().unwrap();
                        let item = posts.items.iter_mut().find(|item| item["ts"] == body["ts"]).unwrap();
                        for name in ["text", "blocks", "metadata"] {
                            item[name] = body[name].clone();
                        }
                        posts.updates += 1;
                        if std::mem::take(&mut posts.lose_response) {
                            return StatusCode::BAD_GATEWAY.into_response();
                        }
                        Json(json!({ "ok":true })).into_response()
                    },
                ),
            )
            .route(
                "/views.open",
                post(
                    |State(state): State<SlackState>, headers: HeaderMap, Json(body): Json<Value>| async move {
                        assert_eq!(headers["authorization"], "Bearer xoxb-test");
                        assert_eq!(body["trigger_id"], "trigger");
                        state.posts.lock().unwrap().views.push(body["view"].clone());
                        Json(json!({ "ok":true }))
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
            Seat::new("seat").unwrap(),
            "https://console.example.com".into(),
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
    let mut posts = state.posts.lock().unwrap();
    posts.history_calls += 1;
    if let Some(code) = &posts.history_error {
        return Json(json!({"ok": false, "error": code}));
    }
    let oldest = query["oldest"]
        .split('.')
        .next()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let items: Vec<_> = posts
        .items
        .iter()
        .filter(|item| {
            item["thread_ts"].as_str() == query.get("ts").map(String::as_str)
                && item["ts"]
                    .as_str()
                    .and_then(slack_timestamp)
                    .is_some_and(|at| at.timestamp() >= oldest)
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

async fn elapse_recovery(adapter: &Adapter<'_>, key: &str) {
    let (mut attempt, rev) = adapter
        .bus
        .slack_get::<Delivery>(key)
        .await
        .unwrap()
        .unwrap();
    attempt.attempted -= RETRY_AFTER + 1;
    adapter.bus.slack_put(key, &attempt, rev).await.unwrap();
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
        fake.state.posts.lock().unwrap().next_post_skew = -120;
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
    elapse_recovery(&adapter, &adapter.root_key(&question)).await;
    adapter.deliver(&ask).await.unwrap();
    ask.ack().await.unwrap();
    assert_eq!(
        fake.items().len(),
        1,
        "recover the posted root instead of posting on restart"
    );
    assert_eq!(
        adapter.thread_for(&root).await.unwrap().as_deref(),
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
    let forged = Event {
        v: event::EVENT_VERSION,
        id: question.clone(),
        sender: OWNER.into(),
        user: None,
        handle: None,
        at: hgl_ai_proto::utc_now_stamp(),
        body: Body::Start {
            title: "Forged".into(),
            issue: None,
            body: "Not allowed".into(),
        },
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
    for _ in 0..2 {
        let native_reply = next(&mut feed).await;
        adapter.deliver(&native_reply).await.unwrap();
        native_reply.ack().await.unwrap();
    }
    let from_console = next(&mut feed).await;
    fake.lose_next_response();
    assert!(adapter.deliver(&from_console).await.is_err());
    let history_calls = fake.state.posts.lock().unwrap().history_calls;
    assert!(
        adapter
            .deliver(&from_console)
            .await
            .unwrap_err()
            .downcast_ref::<RetryAfter>()
            .is_some()
    );
    assert_eq!(
        fake.state.posts.lock().unwrap().history_calls,
        history_calls
    );
    let event: Event = serde_json::from_slice(&from_console.payload).unwrap();
    elapse_recovery(&adapter, &adapter.key("out", &event.msg_id())).await;
    adapter.deliver(&from_console).await.unwrap();
    from_console.ack().await.unwrap();
    let from_seat = next(&mut feed).await;
    adapter.deliver(&from_seat).await.unwrap();
    adapter.deliver(&from_seat).await.unwrap();
    from_seat.ack().await.unwrap();
    let items = fake.items();
    assert_eq!(items.len(), 3);
    assert_eq!(items[1]["thread_ts"], root);
    assert_eq!(
        items[1]["text"],
        "The Owner · Console\nFollow-up from the console"
    );
    assert_eq!(items[2]["thread_ts"], root);
    assert_eq!(items[2]["text"], "seat · hgl-ai\nFollow-up from the asker");

    let forged = Event {
        v: event::EVENT_VERSION,
        id: question,
        sender: asker.to_string(),
        at: hgl_ai_proto::utc_now_stamp(),
        user: None,
        handle: Some("The Owner".into()),
        body: Body::Message {
            body: "Forged author".into(),
        },
    };
    let message = StreamMsg::detached(
        10_000,
        &forged.subject(),
        serde_json::to_vec(&forged).unwrap(),
    );
    adapter.deliver(&message).await.unwrap();
    assert_eq!(
        fake.items(),
        items,
        "Slack refuses an agent event with an owner handle"
    );
}

fn click(envelope: &str, root: &Value, action: &str, ts: &str) -> Value {
    let block = root["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| {
            block["elements"]
                .as_array()
                .is_some_and(|elements| elements.iter().any(|button| button["action_id"] == action))
        })
        .expect("action button on the Slack root");
    let mut button = block["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|button| button["action_id"] == action)
        .unwrap()
        .clone();
    button["action_ts"] = json!(ts);
    button["block_id"] = block["block_id"].clone();
    json!({
        "type": "interactive", "envelope_id": envelope,
        "payload": {
            "type": "block_actions", "team": {"id": "TQA"},
            "user": {"id": "UOWNER", "team_id": "TQA"},
            "channel": {"id": "CQA"}, "trigger_id": "trigger",
            "container": {"type": "message", "channel_id": "CQA", "message_ts": root["ts"]},
            "message": {"ts": root["ts"], "thread_ts": root["thread_ts"], "user": "UQABOT"},
            "actions": [button]
        }
    })
}

fn action_ts(root: &Value, micros: u32) -> String {
    let (seconds, _) = root["ts"].as_str().unwrap().split_once('.').unwrap();
    format!("{seconds}.{micros:06}")
}

fn submission(envelope: &str, form: &Value, text: &str, note: &str) -> Value {
    json!({
        "type": "interactive", "envelope_id": envelope, "accepts_response_payload": true,
        "payload": {
            "type": "view_submission", "team": {"id": "TQA"}, "user": {"id": "UOWNER"},
            "view": {
                "callback_id": form["callback_id"], "private_metadata": form["private_metadata"],
                "state": {"values": {"text": {"text": {"value": text}}, "note": {"note": {"value": note}}}}
            }
        }
    })
}

async fn fold_next(console: &Bus, feed: &mut Feed, controller: &Seat) -> Thread {
    let msg = next(feed).await;
    let hgl_ai_owner::Applied::Folded { thread, .. } =
        hgl_ai_owner::apply(console, controller, &msg)
            .await
            .unwrap()
    else {
        panic!("refused event at {}", msg.seq);
    };
    msg.ack().await.unwrap();
    *thread
}

async fn deliver_next(adapter: &Adapter<'_>, feed: &mut Feed) -> u64 {
    let msg = next(feed).await;
    adapter.deliver(&msg).await.unwrap();
    msg.ack().await.unwrap();
    msg.seq
}

fn assert_terminal(root: &Value, state: &str) {
    assert!(root["text"].as_str().unwrap().starts_with(state), "{root}");
    assert!(
        root["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["type"] != "actions")
    );
    assert!(root["metadata"]["event_payload"]["state_event"].is_string());
}

#[tokio::test]
async fn slack_and_console_share_owner_actions_once() {
    if Command::new("nats-server")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: nats-server is unavailable");
        return;
    }
    let hub = Hub::new();
    let _server = hub.start().await;
    let admin = hub.bus("admin").await;
    admin.provision().await.unwrap();
    let slack = hub.bus("slack").await;
    let console = hub.bus("console").await;
    let seat = hub.bus("seat").await;
    let controller = Seat::new("seat").unwrap();
    let fake = FakeSlack::start().await;
    let adapter = fake.adapter(&slack).await;
    let mut outbound = adapter.open_feed().await.unwrap();
    let mut inbound = console
        .owner_feed(
            hgl_ai_owner::FEED,
            vec![format!("{CONSOLE_PREFIX}.>"), format!("{OWNER_PREFIX}.>")],
        )
        .await
        .unwrap();

    for from_slack in [true, false] {
        let (id, _) = hgl_ai_owner::start_conversation(
            &console,
            &controller,
            "console-owner",
            Some("The Owner"),
            "Project",
            None,
            "Hello",
        )
        .await
        .unwrap();
        assert_eq!(
            fold_next(&console, &mut inbound, &controller).await.state,
            ThreadState::Open
        );
        deliver_next(&adapter, &mut outbound).await;
        let root = fake.items().last().unwrap().clone();
        let ts = action_ts(&root, 100_000);
        let close = click("close", &root, "hgl_close", &ts);
        if from_slack {
            let invalid = vec![
                replace(close.clone(), "/payload/user/id", json!("UOTHER")),
                replace(close.clone(), "/payload/team/id", json!("TOTHER")),
                replace(close.clone(), "/payload/channel/id", json!("COTHER")),
                replace(
                    close.clone(),
                    "/payload/container/channel_id",
                    json!("COTHER"),
                ),
            ];
            fake.send(&adapter, invalid).await;
            assert!(
                inbound
                    .next(Duration::from_millis(100))
                    .await
                    .unwrap()
                    .is_none()
            );
            fake.send(
                &adapter,
                vec![
                    close.clone(),
                    replace(close.clone(), "/envelope_id", json!("retry")),
                ],
            )
            .await;
        } else {
            hgl_ai_owner::owner_event(
                &console,
                &controller,
                &id,
                "console-owner",
                Some("The Owner"),
                Body::Close,
            )
            .await
            .unwrap();
        }
        let thread = fold_next(&console, &mut inbound, &controller).await;
        assert_eq!(thread.id, id);
        assert_eq!(thread.state, ThreadState::Closed);
        assert_eq!(
            thread
                .messages
                .iter()
                .filter(|m| m.body == "closed")
                .count(),
            1
        );
        assert_eq!(
            hgl_ai_owner::show_full(&console, &id)
                .await
                .unwrap()
                .unwrap()
                .state,
            ThreadState::Closed
        );

        // Slack applied the edit but the receipt was lost. A new adapter reconciles
        // the same root by its terminal event marker before acknowledging the feed.
        let updates = fake.state.posts.lock().unwrap().updates;
        fake.lose_next_response();
        let msg = next(&mut outbound).await;
        assert!(adapter.deliver(&msg).await.is_err());
        let restarted = fake.adapter(&slack).await;
        elapse_recovery(&restarted, &restarted.key("state", &id)).await;
        restarted.deliver(&msg).await.unwrap();
        restarted.deliver(&msg).await.unwrap();
        msg.ack().await.unwrap();
        let edited = fake.items().last().unwrap().clone();
        assert_eq!(edited["ts"], root["ts"]);
        assert_terminal(&edited, "Closed");
        assert_eq!(fake.state.posts.lock().unwrap().updates, updates + 1);

        // A retry or stale click after the edit leaves the same console and Slack state.
        fake.send(&restarted, vec![close]).await;
        assert!(
            inbound
                .next(Duration::from_millis(100))
                .await
                .unwrap()
                .is_none()
        );
    }

    for (choice, action) in [
        (Choice::Option, "hgl_option_a"),
        (Choice::Other, "hgl_answer"),
        (Choice::Accept, "hgl_accept"),
        (Choice::Reject, "hgl_reject"),
    ] {
        let id = hgl_ai_owner::ask(
            &seat,
            &controller,
            Ask {
                title: format!("Pick a colour ({choice})"),
                priority: Priority::P3,
                issue: None,
                context: String::new(),
                question: "Which colour?".into(),
                options: vec![QOption {
                    label: "a".into(),
                    line: "Blue".into(),
                }],
                suggested: Some("a".into()),
            },
        )
        .await
        .unwrap();
        fold_next(&console, &mut inbound, &controller).await;
        deliver_next(&adapter, &mut outbound).await;
        let root = fake.items().last().unwrap().clone();
        if choice == Choice::Reject {
            // The console's formal answer also edits the Slack root.
            hgl_ai_owner::owner_event(
                &console,
                &controller,
                &id,
                "console-owner",
                Some("The Owner"),
                Body::Answer {
                    choice,
                    option: None,
                    text: String::new(),
                    note: "Thanks".into(),
                },
            )
            .await
            .unwrap();
        } else {
            let ts = action_ts(&root, 200_000);
            fake.send(&adapter, vec![click("answer", &root, action, &ts)])
                .await;
            let form = fake
                .state
                .posts
                .lock()
                .unwrap()
                .views
                .last()
                .unwrap()
                .clone();
            let answer = submission("submit", &form, "Green", "Thanks");
            fake.send(
                &adapter,
                vec![
                    answer.clone(),
                    replace(answer, "/envelope_id", json!("retry-submit")),
                ],
            )
            .await;
        }
        let answer = fold_next(&console, &mut inbound, &controller)
            .await
            .answer
            .unwrap();
        assert_eq!(answer.choice, choice);
        assert_eq!(answer.note, "Thanks");
        match choice {
            Choice::Accept | Choice::Option => assert_eq!(answer.option.as_deref(), Some("a")),
            Choice::Other => assert_eq!(answer.text, "Green"),
            Choice::Reject => {}
        }
        deliver_next(&adapter, &mut outbound).await;
        let edited = fake.items().last().unwrap().clone();
        assert_terminal(&edited, "Answered");
        assert!(edited["text"].as_str().unwrap().contains("Thanks"));
        assert!(
            inbound
                .next(Duration::from_millis(100))
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn startup_error_chain_keeps_only_safe_slack_error_codes() {
    let fake = FakeSlack::start().await;
    let web = Web::new(fake.base.clone(), "xapp-test".into(), "xoxb-test".into()).unwrap();
    for (response, expected) in [
        ("missing_scope", "missing_scope"),
        ("xoxb-secret-token", "unknown_error"),
        ("", "unknown_error"),
    ] {
        fake.state.posts.lock().unwrap().history_error = Some(response.into());
        let error = web.verify_channel("CQA").await.unwrap_err();
        let logged = format!("{error:#}");
        assert!(logged.contains("checking the private Slack Q&A channel"));
        assert!(logged.contains(&format!("conversations.history: {expected}")));
        assert!(!logged.contains("xoxb-"));
        assert!(!logged.contains("xapp-"));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn startup_retry_logs_and_persists_error_then_clears_it() {
    if Command::new("nats-server")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: nats-server is unavailable");
        return;
    }
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let hub = Hub::new();
    let _server = hub.start().await;
    hub.bus("admin").await.provision().await.unwrap();
    let slack = hub.bus("slack").await;
    let seat = hub.bus("seat").await;
    let fake = FakeSlack::start().await;
    let web = Web::new(fake.base.clone(), "xapp-test".into(), "xoxb-test".into()).unwrap();
    fake.state.posts.lock().unwrap().history_error = Some("missing_scope".into());
    let error = web.verify_channel("CQA").await.unwrap_err();
    let output = Capture(Arc::default());
    let writer = output.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    startup_failure(&slack, &mut Backoff::default(), &error).await;
    let logged = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
    assert!(logged.contains("Slack adapter startup: checking the private Slack Q&A channel: conversations.history: missing_scope"), "{logged}");
    assert!(!logged.contains("xoxb-") && !logged.contains("xapp-"));
    let key = hgl_ai_proto::SLACK_STARTUP_KEY;
    let status = seat.state_get(key).await.unwrap().unwrap().value;
    assert!(status.contains("missing_scope"));
    slack
        .slack_put("TQA.CQA.thread.123", &"private", 0)
        .await
        .unwrap();
    assert!(
        seat.slack_get::<String>("TQA.CQA.thread.123")
            .await
            .is_err()
    );
    assert!(record_startup(&slack, None).await);
    assert_eq!(seat.state_get(key).await.unwrap().unwrap().value, "ok");
}

#[tokio::test]
async fn rate_limit_cools_down_a_method_for_every_caller() {
    let fake = FakeSlack::start().await;
    fake.state.posts.lock().unwrap().rate_limit_open = Some(90);
    let web = Web::new(fake.base.clone(), "xapp-test".into(), "xoxb-test".into()).unwrap();
    let Err(first) = web.socket().await else {
        panic!("Slack should have rejected the request");
    };
    assert_eq!(
        first.downcast_ref::<RetryAfter>().unwrap().0,
        Duration::from_secs(90)
    );
    let Err(second) = web.clone().socket().await else {
        panic!("method should still be limited");
    };
    let after = second.downcast_ref::<RetryAfter>().unwrap().0;
    assert!((Duration::from_secs(80)..=Duration::from_secs(90)).contains(&after));
    assert_eq!(fake.state.posts.lock().unwrap().socket_opens, 1);
    // Slack limits by method, so a throttled socket open does not block auth.test.
    web.identify().await.unwrap();
}

#[tokio::test]
async fn switch_between_slack_and_console_on_one_conversation() {
    if Command::new("nats-server")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: nats-server is unavailable");
        return;
    }
    use hgl_ai_config::{Config, Console, Person, User};
    use hgl_ai_console::{
        App,
        auth::{Session, session_key},
    };
    use reqwest::header::{CONTENT_TYPE, COOKIE, LOCATION, ORIGIN};

    const CONSOLE: &str = "https://console.example.com";
    let hub = Hub::new();
    let _server = hub.start().await;
    hub.bus("admin").await.provision().await.unwrap();
    let slack = hub.bus("slack").await;
    let console = Arc::new(hub.bus("console").await);
    let fake = FakeSlack::start().await;
    let adapter = fake.adapter(&slack).await;
    let mut outbound = adapter.open_feed().await.unwrap();

    let mut cfg = Config::new("hub");
    cfg.github.controller = "seat".into();
    cfg.paths.data_dir = hub.dir.path().join("console-data").display().to_string();
    cfg.slack = Some(settings());
    cfg.console = Some(Console {
        url: CONSOLE.into(),
        users: vec![User::Person(Person {
            email: "owner@example.com".into(),
            handle: Some("The Owner".into()),
            github: None,
            apple: None,
            google: None,
        })],
        ..Console::default()
    });
    let app = Arc::new(App::new(cfg).unwrap());
    app.set_hub(console.clone());
    hgl_ai_console::service::start_background(app.clone()).await;
    let session = Session {
        id: "test-session".into(),
        user: "owner@example.com".into(),
        via: "passkey".into(),
        csrf: "csrf-token".into(),
        expires_at: Utc::now().timestamp() + 60,
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.auth
        .attach(vec![(session_key(&session.id), session.clone())], tx);
    let cookie = app
        .auth
        .session_cookie(&session)
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, hgl_ai_console::router(app))
            .await
            .unwrap()
    });
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = client
        .post(format!("{base}/conversations"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, CONSOLE)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body("csrf=csrf-token&title=Release+planning&body=Kick+off+in+the+console")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let path = response.headers()[LOCATION].to_str().unwrap();
    let id = path.strip_prefix("/t/").unwrap();
    deliver_next(&adapter, &mut outbound).await;
    let root = fake.items()[0].clone();
    let root_ts = root["ts"].as_str().unwrap();
    let ts = action_ts(&root, 100_000);
    let mut slack_reply = reply("reply", root_ts, &ts);
    slack_reply["payload"]["event"]["text"] = json!("Follow up from Slack");
    fake.state
        .posts
        .lock()
        .unwrap()
        .items
        .push(slack_reply["payload"]["event"].clone());
    fake.send(
        &adapter,
        vec![
            slack_reply.clone(),
            replace(slack_reply, "/envelope_id", json!("retry")),
        ],
    )
    .await;
    let reply_seq = deliver_next(&adapter, &mut outbound).await;
    assert!(
        hgl_ai_owner::wait_folded(&console, id, reply_seq, Duration::from_secs(3))
            .await
            .unwrap()
            .is_some_and(|t| t.seq >= reply_seq)
    );
    let response = client
        .post(format!("{base}{path}/close"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, CONSOLE)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body("csrf=csrf-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    deliver_next(&adapter, &mut outbound).await;

    let page = client
        .get(format!("{base}{path}"))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let permalink = format!(
        "https://qa.slack.com/archives/CQA/p{}",
        root_ts.replace('.', "")
    );
    assert!(page.contains(&permalink) && page.contains("Open in Slack"));
    assert!(page.contains("· Slack") && page.contains("· Console"));
    let messages = page
        .split("<ol class=\"messages\">")
        .nth(1)
        .unwrap()
        .split("</ol>")
        .next()
        .unwrap();
    let expected = [
        "Kick off in the console",
        "Follow up from Slack",
        ">closed<",
    ];
    for text in expected {
        assert_eq!(messages.matches(text).count(), 1, "{messages}");
    }
    assert!(messages.find(expected[0]) < messages.find(expected[1]));
    assert!(messages.find(expected[1]) < messages.find(expected[2]));
    let items = fake.items();
    assert_eq!(items.len(), 2, "one root and the native Slack reply");
    assert_eq!(items[0]["ts"], root_ts);
    assert_eq!(items[1]["thread_ts"], root_ts);
    assert_eq!(items[1]["text"], "Follow up from Slack");
    assert_eq!(
        items[0]["text"]
            .as_str()
            .unwrap()
            .matches("Kick off in the console")
            .count(),
        1
    );
    assert_terminal(&items[0], "Closed · Console");
    assert!(
        items[0]["blocks"]
            .to_string()
            .contains(&format!("{CONSOLE}{path}"))
    );

    // The other direction keeps a native Slack root, with controls in a bot reply.
    let native_ts = format!("{}.900000", Utc::now().timestamp());
    let native = json!({
        "type": "app_mention", "ts": native_ts, "channel": "CQA", "user": "UOWNER",
        "text": "<@UQABOT> Discuss the next release\nStart in Slack"
    });
    let start = json!({
        "type": "events_api", "envelope_id": "start",
        "payload": { "type": "event_callback", "team_id": "TQA", "event": native }
    });
    fake.state.posts.lock().unwrap().items.push(native);
    fake.send(
        &adapter,
        vec![
            start.clone(),
            replace(start, "/envelope_id", json!("start-retry")),
        ],
    )
    .await;
    let start_seq = deliver_next(&adapter, &mut outbound).await;
    let id = adapter.thread_for(&native_ts).await.unwrap().unwrap();
    let thread = hgl_ai_owner::wait_folded(&console, &id, start_seq, Duration::from_secs(3))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(thread.messages.len(), 1);
    assert_eq!(thread.messages[0].source(), "Slack");
    let control = fake.items().last().unwrap().clone();
    assert_eq!(control["thread_ts"], native_ts);
    assert!(!control["text"].as_str().unwrap().contains("Start in Slack"));
    let answer = client
        .post(format!("{base}/t/{id}/message"))
        .header(COOKIE, &cookie)
        .header(ORIGIN, CONSOLE)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body("csrf=csrf-token&body=Continue+in+the+console")
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::SEE_OTHER);
    deliver_next(&adapter, &mut outbound).await;
    let continued = fake.items().last().unwrap().clone();
    assert_eq!(continued["thread_ts"], native_ts);
    assert!(
        continued["blocks"]
            .to_string()
            .contains(&format!("{CONSOLE}/t/{id}"))
    );
    let action = action_ts(&control, 950_000);
    fake.send(
        &adapter,
        vec![click("close-native", &control, "hgl_close", &action)],
    )
    .await;
    let close_seq = deliver_next(&adapter, &mut outbound).await;
    let thread = hgl_ai_owner::wait_folded(&console, &id, close_seq, Duration::from_secs(3))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(thread.state, ThreadState::Closed);
    assert_eq!(thread.messages.last().unwrap().source(), "Slack");
    assert_terminal(&fake.items()[3], "Closed · Slack");
}

#[tokio::test]
async fn socket_mode_retries_quick_disconnects_with_growing_delays() {
    if Command::new("nats-server")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: nats-server is unavailable");
        return;
    }
    let hub = Hub::new();
    let _server = hub.start().await;
    let slack = hub.bus("slack").await;
    let fake = FakeSlack::start().await;
    let adapter = fake.adapter(&slack).await;
    let (delays_tx, mut delays_rx) = tokio::sync::mpsc::unbounded_channel();
    let disconnects = async {
        for min_ms in [500, 1000, 2000, 4000] {
            let (stream, _) = fake.listener.accept().await.unwrap();
            let mut ws = ServerBuilder::new().accept(stream).await.unwrap();
            ws.send(Message::text(json!({"type": "disconnect"}).to_string()))
                .await
                .unwrap();
            let delay = delays_rx.recv().await.unwrap();
            assert!(
                delay >= Duration::from_millis(min_ms),
                "{delay:?} < {min_ms}ms"
            );
        }
    };
    tokio::select! {
        result = timeout(Duration::from_secs(12), disconnects) => result.unwrap(),
        () = adapter.socket_loop_with_sleep(|delay| {
            delays_tx.send(delay).unwrap();
            std::future::ready(())
        }) => panic!("Slack socket loop exited"),
    }
}
