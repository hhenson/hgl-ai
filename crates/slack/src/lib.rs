//! The Slack adapter (docs/owner.md). The hub keeps the event log,
//! delivery state, and thread mappings for questions and conversations.

mod digest;
mod interaction;
mod retry;
mod web;

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use futures::{SinkExt as _, StreamExt as _};
use hgl_ai_bus::{Bus, BusError, Credential, StreamMsg};
use hgl_ai_config::slack::{APP_TOKEN_FILE, BOT_TOKEN_FILE, Slack};
use hgl_ai_owner::{Body, Event, Origin, Thread, ThreadState, event, fold};
use hgl_ai_proto::{
    CONSOLE_PREFIX, OWNER, OWNER_PREFIX, SLACK_FEED, SLACK_OWNER_FILTER, Seat, utc_stamp,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::{Instant, sleep, timeout};
use tokio_websockets::Message;

use retry::{Backoff, RetryAfter};
use web::{Bot, Marker, Web};

const FEED_POLL: Duration = Duration::from_secs(5);
const HEALTHY: Duration = Duration::from_secs(60);
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(45);
const RETRY_AFTER: i64 = 30;
const CLOCK_SKEW: i64 = 5 * 60;
const SLACK_TEXT_LIMIT: usize = 40_000;

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    envelope_id: Option<String>,
    #[serde(default)]
    payload: Value,
    #[serde(default)]
    accepts_response_payload: bool,
}

#[derive(Deserialize)]
struct Callback {
    #[serde(rename = "type")]
    kind: String,
    team_id: Option<String>,
    event: Option<Incoming>,
}

#[derive(Deserialize)]
struct Incoming {
    #[serde(rename = "type")]
    kind: String,
    team: Option<String>,
    user_team: Option<String>,
    source_team: Option<String>,
    channel: Option<String>,
    channel_type: Option<String>,
    user: Option<String>,
    bot_id: Option<String>,
    bot_profile: Option<serde_json::Value>,
    app_id: Option<String>,
    subtype: Option<String>,
    #[serde(default)]
    hidden: bool,
    ts: Option<String>,
    thread_ts: Option<String>,
    text: Option<String>,
}

struct Reply {
    parent: String,
    ts: String,
    at: DateTime<Utc>,
    text: String,
}

fn owner_reply(env: &Envelope, cfg: &Slack, bot: &Bot) -> Option<Reply> {
    let msg = owner_message(env, cfg, bot)?;
    if msg.kind != "message" || msg.channel_type.as_deref() != Some("group") {
        return None;
    }
    let parent = msg.thread_ts?;
    let root = slack_timestamp(&parent)?;
    let ts = msg.ts?;
    let at = slack_timestamp(&ts)?;
    let text = msg.text?;
    (at > root && valid_text(&text)).then_some(Reply {
        parent,
        ts,
        at,
        text,
    })
}

fn valid_text(text: &str) -> bool {
    !text.trim().is_empty() && text.chars().count() <= SLACK_TEXT_LIMIT
}

fn owner_message(env: &Envelope, cfg: &Slack, bot: &Bot) -> Option<Incoming> {
    let payload = Callback::deserialize(&env.payload).ok()?;
    let msg = payload.event?;
    if env.kind != "events_api"
        || payload.kind != "event_callback"
        || payload.team_id.as_deref() != Some(cfg.team.as_str())
        || msg.channel.as_deref() != Some(cfg.channel.as_str())
        || msg.user.as_deref() != Some(cfg.user.as_str())
        || cfg.user == bot.user_id
        || [&msg.team, &msg.user_team, &msg.source_team]
            .into_iter()
            .flatten()
            .any(|team| team != &cfg.team)
        || msg.bot_id.is_some()
        || msg.bot_profile.is_some()
        || msg.app_id.is_some()
        || msg.subtype.is_some()
        || msg.hidden
    {
        return None;
    }
    Some(msg)
}

fn slack_timestamp(s: &str) -> Option<DateTime<Utc>> {
    let (seconds, micros) = s.split_once('.')?;
    if seconds.len() != 10
        || micros.len() != 6
        || !seconds
            .bytes()
            .chain(micros.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    DateTime::from_timestamp(seconds.parse().ok()?, micros.parse::<u32>().ok()? * 1000)
}

fn timestamp_key(ts: &str) -> String {
    ts.replace('.', "_")
}

fn secret(dir: &Path, name: &str, prefix: &str) -> Result<String> {
    let path = dir.join(name);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("opening {name}"))?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid reads the calling process's effective user ID.
    let uid = unsafe { libc::geteuid() };
    ensure!(
        metadata.is_file() && metadata.mode() & 0o7777 == 0o600 && metadata.uid() == uid,
        "{name} must be a regular mode-0600 file owned by the server user"
    );
    let mut token = String::new();
    file.take(4097)
        .read_to_string(&mut token)
        .with_context(|| format!("reading {name}"))?;
    let value = token.trim();
    ensure!(
        token.len() <= 4096
            && value.starts_with(prefix)
            && value.len() > prefix.len()
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "{name} does not contain a valid Slack token"
    );
    Ok(value.to_owned())
}

/// Run the Slack service. Tokens are read once and a service restart reloads them.
pub async fn run(
    hub_url: &str,
    credential: Credential,
    cfg: Slack,
    controller: Seat,
    console_origin: &str,
    secret_dir: &Path,
) -> Result<()> {
    cfg.validate().map_err(anyhow::Error::msg)?;
    let app = secret(secret_dir, APP_TOKEN_FILE, "xapp-")?;
    let bot = secret(secret_dir, BOT_TOKEN_FILE, "xoxb-")?;
    let web = Web::new("https://slack.com/api".into(), app, bot)?;
    let mut hub_retry = Backoff::default();
    let bus = loop {
        match Bus::connect_with(hub_url, Some(credential.clone())).await {
            Ok(bus) => break bus,
            Err(e) => {
                pause(
                    &mut hub_retry,
                    Some(e.into()),
                    "connecting the Slack adapter to the hub",
                )
                .await
            }
        }
    };
    let mut slack_retry = Backoff::default();
    let adapter = loop {
        match Adapter::new(
            &bus,
            cfg.clone(),
            controller.clone(),
            console_origin.into(),
            web.clone(),
        )
        .await
        {
            Ok(adapter) => break adapter,
            Err(e) => pause(&mut slack_retry, Some(e), "Slack adapter startup").await,
        }
    };
    tokio::join!(
        adapter.outbound_loop(None),
        adapter.socket_loop(),
        adapter.status_loop()
    );
    Ok(())
}

async fn pause(retries: &mut Backoff, error: Option<anyhow::Error>, operation: &str) {
    let delay = retries.delay(error.as_ref());
    if let Some(error) = error {
        tracing::warn!(?delay, "{operation}: {error}");
    }
    sleep(delay).await;
}

struct Adapter<'a> {
    bus: &'a Bus,
    cfg: Slack,
    controller: Seat,
    console_origin: String,
    bot: Bot,
    web: Web,
}

struct Post<'a> {
    channel: &'a str,
    parent: Option<&'a str>,
    text: &'a str,
    blocks: &'a [Value],
}

#[derive(Serialize, Deserialize)]
struct Delivery {
    event: String,
    started: i64,
    attempted: i64,
    ts: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    opening_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
}

impl Delivery {
    fn pending(event: &str, opening_seq: Option<u64>) -> Self {
        let now = Utc::now().timestamp();
        Self {
            event: event.into(),
            started: now,
            attempted: now,
            ts: None,
            opening_seq,
            parent: None,
        }
    }

    fn thread_ts(&self) -> Option<&str> {
        self.parent.as_deref().or(self.ts.as_deref())
    }

    fn wait_for_recovery(&self) -> Result<()> {
        let seconds = (self.attempted + RETRY_AFTER - Utc::now().timestamp()).clamp(0, RETRY_AFTER);
        if seconds > 0 {
            return Err(RetryAfter(Duration::from_secs(seconds as u64)).into());
        }
        Ok(())
    }
}

impl<'a> Adapter<'a> {
    async fn new(
        bus: &'a Bus,
        cfg: Slack,
        controller: Seat,
        console_origin: String,
        web: Web,
    ) -> Result<Self> {
        cfg.validate().map_err(anyhow::Error::msg)?;
        let bot = web.identify().await?;
        ensure!(
            bot.team_id == cfg.team,
            "the Slack bot token belongs to a different workspace"
        );
        ensure!(
            bot.user_id != cfg.user,
            "the Slack owner must be a human account"
        );
        web.verify_channel(&cfg.channel).await?;
        Ok(Self {
            bus,
            cfg,
            controller,
            console_origin,
            bot,
            web,
        })
    }

    fn key(&self, kind: &str, id: &str) -> String {
        format!("{}.{}.{kind}.{id}", self.cfg.team, self.cfg.channel)
    }

    async fn thread_for(&self, parent: &str) -> Result<Option<String>> {
        let key = self.key("thread", &timestamp_key(parent));
        if let Some((id, _)) = self.bus.slack_get::<String>(&key).await? {
            return Ok(Some(id));
        }
        // A reply can arrive after Slack accepted a post but before we saved its ts.
        let Some(post) = self.web.message_at(&self.cfg.channel, None, parent).await? else {
            return Ok(None);
        };
        let Some(marker) = post.marker(&self.bot) else {
            return Ok(None);
        };
        if hgl_ai_owner::check_id(&marker.question).is_err() {
            return Ok(None);
        }
        let delivery = self
            .bus
            .slack_get::<Delivery>(&self.root_key(&marker.question))
            .await?;
        Ok(delivery
            .filter(|(record, _)| {
                record.event == marker.event && record.ts.as_deref().is_none_or(|ts| ts == parent)
            })
            .map(|_| marker.question))
    }

    async fn ingest_event(&self, envelope: &Envelope) -> Result<()> {
        let Some(reply) = owner_reply(envelope, &self.cfg, &self.bot) else {
            return self.ingest_start(envelope).await;
        };
        let Some(id) = self.thread_for(&reply.parent).await? else {
            return Ok(());
        };
        self.publish_action(
            &id,
            &timestamp_key(&reply.ts),
            reply.at,
            Body::OwnerMessage { body: reply.text },
        )
        .await?;
        Ok(())
    }

    async fn ingest_start(&self, envelope: &Envelope) -> Result<()> {
        let Some(msg) = owner_message(envelope, &self.cfg, &self.bot).filter(|msg| {
            msg.kind == "app_mention"
                && msg.thread_ts.is_none()
                && msg
                    .channel_type
                    .as_deref()
                    .is_none_or(|kind| kind == "group")
        }) else {
            return Ok(());
        };
        let Some(ts) = msg.ts else {
            return Ok(());
        };
        let Some(at) = slack_timestamp(&ts) else {
            return Ok(());
        };
        let mention = format!("<@{}>", self.bot.user_id);
        let Some(text) = msg
            .text
            .as_deref()
            .and_then(|text| text.trim().strip_prefix(&mention))
            .map(str::trim)
            .filter(|text| valid_text(text))
        else {
            return Ok(());
        };
        let (title, body) = text.split_once('\n').unwrap_or((text, text));
        if title.trim().is_empty() || title.trim().len() > 200 || body.trim().is_empty() {
            return Ok(());
        }
        use sha2::{Digest as _, Sha256};
        let hash = Sha256::digest(format!("{}:{}:{ts}", self.cfg.team, self.cfg.channel));
        let id = format!("c-{}", hex::encode(&hash[..4]));
        let key = format!("start_{}", timestamp_key(&ts));
        let body = Body::Start {
            title: title.trim().into(),
            issue: None,
            body: body.trim().into(),
        };
        if self.publish_action(&id, &key, at, body).await? {
            self.map_thread(&id, &ts).await?;
        }
        Ok(())
    }

    async fn publish_action(
        &self,
        id: &str,
        key: &str,
        at: DateTime<Utc>,
        body: Body,
    ) -> Result<bool> {
        let subject = event::slack_subject(id, &self.cfg.team, &self.cfg.channel, key);
        let event = Event {
            v: event::EVENT_VERSION,
            id: id.into(),
            sender: OWNER.into(),
            user: Some(format!("slack:{}:{}", self.cfg.team, self.cfg.user)),
            handle: Some(self.cfg.handle.clone()),
            at: utc_stamp(at),
            body,
        };
        if matches!(event.body, Body::Start { .. }) {
            if !fold(None, &subject, 1, &event, &self.controller)
                .is_ok_and(|(_, effects)| effects.oversize.is_none())
            {
                return Ok(false);
            }
        } else if !matches!(event.body, Body::OwnerMessage { .. }) {
            let Some((root, _)) = self.bus.slack_get::<Delivery>(&self.root_key(id)).await? else {
                return Ok(false);
            };
            let opening = self.opening(&root).await?;
            let Ok((_, effects)) = fold(
                Some(&opening),
                &subject,
                opening.seq + 1,
                &event,
                &self.controller,
            ) else {
                return Ok(false);
            };
            if effects.oversize.is_some() {
                return Ok(false);
            }
            if self
                .bus
                .slack_get::<Delivery>(&self.key("state", id))
                .await?
                .is_some_and(|(edit, _)| edit.event != delivery_id(&event, &subject))
            {
                return Ok(false);
            }
        }
        self.bus
            .publish_slack_event(&subject, &serde_json::to_vec(&event)?)
            .await?;
        Ok(true)
    }

    async fn socket_loop(&self) {
        let mut retries = Backoff::default();
        loop {
            let started = Instant::now();
            let result = self.socket_session().await;
            if started.elapsed() >= HEALTHY {
                retries = Backoff::default();
            }
            pause(&mut retries, result.err(), "Slack Socket Mode").await;
        }
    }

    async fn socket_session(&self) -> Result<()> {
        let mut socket = self.web.socket().await?;
        while let Some(frame) = timeout(Duration::from_secs(120), socket.next())
            .await
            .context("Slack Socket Mode went quiet")?
        {
            let frame = frame.map_err(|_| anyhow!("Slack Socket Mode receive failed"))?;
            if frame.is_close() {
                break;
            }
            let Some(text) = frame.as_text() else {
                continue;
            };
            let envelope: Envelope = match serde_json::from_str(text) {
                Ok(e) => e,
                Err(_) => {
                    tracing::warn!("unreadable Slack envelope");
                    continue;
                }
            };
            if envelope.kind == "disconnect" {
                break;
            }
            let Some(id) = &envelope.envelope_id else {
                continue;
            };
            let handled = match envelope.kind.as_str() {
                "events_api" => self.ingest_event(&envelope).await.map(|()| None),
                "interactive" => self.ingest_interaction(&envelope.payload).await,
                _ => Ok(None),
            };
            let payload = match handled {
                Ok(payload) => payload,
                Err(e) => {
                    // Slack retries an unacknowledged envelope.
                    tracing::warn!("saving Slack owner action: {e}");
                    continue;
                }
            };
            let mut ack = serde_json::json!({ "envelope_id": id });
            if envelope.accepts_response_payload
                && let Some(payload) = payload
            {
                ack["payload"] = payload;
            }
            socket
                .send(Message::text(ack.to_string()))
                .await
                .map_err(|_| anyhow!("Slack Socket Mode acknowledgement failed"))?;
        }
        Ok(())
    }

    async fn outbound_loop(&self, status_channel: Option<&str>) {
        let mut retries = Backoff::default();
        loop {
            let started = Instant::now();
            let result = self.drain_feed(status_channel).await;
            if started.elapsed() >= HEALTHY {
                retries = Backoff::default();
            }
            pause(&mut retries, result.err(), "Slack owner feed").await;
        }
    }

    async fn drain_feed(&self, status_channel: Option<&str>) -> Result<()> {
        let mut feed = match status_channel {
            Some(channel) => self.open_status_feed(channel).await?,
            None => self.open_feed().await?,
        };
        let mut retries = Backoff::default();
        loop {
            let Some(message) = feed.next(FEED_POLL).await? else {
                continue;
            };
            let deliver = async {
                match status_channel {
                    Some(channel) => self.deliver_digest(channel, &message).await,
                    None => self.deliver(&message).await,
                }
            };
            let delivery = timeout(DELIVERY_TIMEOUT, deliver)
                .await
                .context("Slack delivery timed out")
                .and_then(|result| result);
            if let Err(error) = delivery {
                let delay = retries.delay(Some(&error));
                tracing::warn!(seq = message.seq, ?delay, "Slack delivery pending: {error}");
                message.nak(delay).await?;
                sleep(delay).await;
            } else {
                message.ack().await?;
                retries = Backoff::default();
            }
        }
    }

    async fn open_feed(&self) -> Result<hgl_ai_bus::Feed> {
        Ok(self
            .bus
            .owner_feed(
                SLACK_FEED,
                vec![
                    format!("{CONSOLE_PREFIX}.thread.*"),
                    format!("{OWNER_PREFIX}.thread.*"),
                    SLACK_OWNER_FILTER.into(),
                ],
            )
            .await?)
    }

    async fn deliver(&self, message: &StreamMsg) -> Result<()> {
        let Ok(ev) = serde_json::from_slice::<Event>(&message.payload) else {
            return Ok(());
        };
        if event::validate_event(&message.subject, &ev).is_err() {
            return Ok(());
        }
        let root_key = self.root_key(&ev.id);
        let event_id = delivery_id(&ev, &message.subject);
        let from_slack = ev.subject() != message.subject;
        let source = match (from_slack, ev.body.origin()) {
            (true, _) => "Slack",
            (false, Origin::Owner) => "Console",
            (false, Origin::Agent) => "hgl-ai",
        };
        let link = format!("{}/t/{}", self.console_origin, ev.id);
        let marker = Marker {
            event: event_id.clone(),
            question: ev.id.clone(),
            state_event: None,
        };
        match &ev.body {
            Body::Ask(_) | Body::Start { .. } => {
                let Ok((thread, _)) =
                    fold(None, &message.subject, message.seq, &ev, &self.controller)
                else {
                    return Ok(());
                };
                let parent = event::slack_source(&message.subject)
                    .filter(|(team, channel, _)| {
                        *team == self.cfg.team && *channel == self.cfg.channel
                    })
                    .and_then(|(_, _, key)| key.strip_prefix("start_"))
                    .map(|ts| ts.replace('_', "."))
                    .filter(|ts| slack_timestamp(ts).is_some());
                let text = root_text(&thread, parent.is_none());
                let mut blocks = interaction::root_blocks(&thread, &text);
                blocks.push(interaction::console_link(&link));
                let ts = self
                    .post_once(
                        &root_key,
                        marker,
                        Post {
                            channel: &self.cfg.channel,
                            parent: parent.as_deref(),
                            text: &text,
                            blocks: &blocks,
                        },
                        Some(message.seq),
                    )
                    .await?;
                self.map_thread(&ev.id, &ts).await?;
                let parent = parent.as_deref().unwrap_or(&ts);
                self.map_thread(&ev.id, parent).await?;
                self.store_link(&ev.id, parent).await?;
            }
            Body::Message { body } | Body::OwnerMessage { body }
                if !from_slack && !body.trim().is_empty() =>
            {
                let Some((root, _)) = self.bus.slack_get::<Delivery>(&root_key).await? else {
                    return Ok(());
                };
                let parent = root
                    .thread_ts()
                    .context("the Slack thread is still pending")?;
                let who = ev.handle.as_deref().unwrap_or(&ev.sender);
                let text = format!("{who} · {source}\n{body}");
                let mut blocks = interaction::text_blocks(&text);
                blocks.push(interaction::console_link(&link));
                let key = self.key("out", &event_id);
                self.post_once(
                    &key,
                    marker,
                    Post {
                        channel: &self.cfg.channel,
                        parent: Some(parent),
                        text: &text,
                        blocks: &blocks,
                    },
                    None,
                )
                .await?;
            }
            Body::Answer { .. } | Body::Close | Body::Withdraw { .. } => {
                self.edit_state(message, &ev, &event_id, &link, source)
                    .await?;
            }
            _ => {}
        }
        Ok(())
    }

    fn root_key(&self, id: &str) -> String {
        self.key("question", id)
    }

    async fn store_link(&self, id: &str, ts: &str) -> Result<()> {
        let key = self.cfg.link_key(id);
        if self.bus.slack_get::<String>(&key).await?.is_none() {
            let link = self.web.permalink(&self.cfg.channel, ts).await?;
            match self.bus.slack_put(&key, &link, 0).await {
                Ok(_) | Err(BusError::Conflict { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    async fn opening(&self, root: &Delivery) -> Result<Thread> {
        let seq = root
            .opening_seq
            .context("the Slack thread has no opening event")?;
        let message = self.bus.owner_event_at(seq).await?;
        let ev: Event = serde_json::from_slice(&message.payload)?;
        ensure!(
            delivery_id(&ev, &message.subject) == root.event,
            "the Slack thread points to the wrong opening event"
        );
        Ok(fold(None, &message.subject, seq, &ev, &self.controller)?.0)
    }

    async fn edit_state(
        &self,
        message: &StreamMsg,
        ev: &Event,
        event_id: &str,
        link: &str,
        source: &str,
    ) -> Result<()> {
        let key = self.key("state", &ev.id);
        let prior = self.bus.slack_get::<Delivery>(&key).await?;
        if prior
            .as_ref()
            .is_some_and(|(edit, _)| edit.ts.is_some() || edit.event != event_id)
        {
            return Ok(());
        }
        let Some((root, _)) = self
            .bus
            .slack_get::<Delivery>(&self.root_key(&ev.id))
            .await?
        else {
            return Ok(());
        };
        let ts = root
            .ts
            .as_deref()
            .context("the Slack thread is still pending")?;
        let opening = self.opening(&root).await?;
        let Ok((thread, _)) = fold(
            Some(&opening),
            &message.subject,
            message.seq,
            ev,
            &self.controller,
        ) else {
            return Ok(());
        };
        let (mut edit, revision) = if let Some((edit, revision)) = prior {
            edit.wait_for_recovery()?;
            let posted = self
                .web
                .message_at(&self.cfg.channel, root.parent.as_deref(), ts)
                .await?;
            if posted
                .and_then(|post| post.marker(&self.bot))
                .is_some_and(|marker| {
                    marker.event == root.event && marker.state_event.as_deref() == Some(event_id)
                })
            {
                self.record_post(&key, revision, edit, ts.into()).await?;
                return Ok(());
            }
            (edit, revision)
        } else {
            (Delivery::pending(event_id, None), 0)
        };
        edit.attempted = Utc::now().timestamp();
        let claim = self.bus.slack_put(&key, &edit, revision).await?;
        let marker = Marker {
            event: root.event,
            question: ev.id.clone(),
            state_event: Some(event_id.into()),
        };
        let text = terminal_text(&opening, &thread, root.parent.is_none(), source);
        let mut blocks = interaction::text_blocks(&text);
        blocks.push(interaction::console_link(link));
        self.web
            .update(
                &self.cfg.channel,
                ts,
                marker,
                &escaped_text(&text, 4_000),
                &blocks,
            )
            .await?;
        self.record_post(&key, claim, edit, ts.into()).await?;
        Ok(())
    }

    async fn map_thread(&self, id: &str, ts: &str) -> Result<()> {
        let key = self.key("thread", &timestamp_key(ts));
        match self.bus.slack_put(&key, &id, 0).await {
            Ok(_) => Ok(()),
            Err(BusError::Conflict { .. }) => {
                let route = self.bus.slack_get::<String>(&key).await?;
                ensure!(
                    route.is_some_and(|(stored, _)| stored == id),
                    "conflicting Slack thread mapping"
                );
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn post_once(
        &self,
        key: &str,
        marker: Marker,
        post: Post<'_>,
        opening_seq: Option<u64>,
    ) -> Result<String> {
        let (mut delivery, revision) = match self.bus.slack_get::<Delivery>(key).await? {
            Some((delivery, revision)) => {
                if let Some(ts) = delivery.ts {
                    return Ok(ts);
                }
                ensure!(
                    delivery.event == marker.event,
                    "the original Slack post is still pending"
                );
                delivery.wait_for_recovery()?;
                let oldest = (delivery.started - CLOCK_SKEW).to_string();
                if let Some(ts) = self
                    .web
                    .find(post.channel, post.parent, &oldest, &marker.event, &self.bot)
                    .await?
                {
                    return self.record_post(key, revision, delivery, ts).await;
                }
                (delivery, revision)
            }
            None => (Delivery::pending(&marker.event, opening_seq), 0),
        };
        delivery.attempted = Utc::now().timestamp();
        delivery.parent = post.parent.map(str::to_string);
        let claim = self.bus.slack_put(key, &delivery, revision).await?;
        let ts = self
            .web
            .post(
                post.channel,
                post.parent,
                marker,
                &escaped_text(post.text, SLACK_TEXT_LIMIT),
                post.blocks,
            )
            .await?;
        self.record_post(key, claim, delivery, ts).await
    }

    async fn record_post(
        &self,
        key: &str,
        revision: u64,
        mut delivery: Delivery,
        ts: String,
    ) -> Result<String> {
        delivery.ts = Some(ts.clone());
        self.bus.slack_put(key, &delivery, revision).await?;
        Ok(ts)
    }
}

fn root_text(thread: &Thread, include_opening_message: bool) -> String {
    let mut text = thread.id.clone();
    if let Some(priority) = thread.priority {
        text.push_str(&format!(" · {priority}"));
    }
    text.push_str(&format!(" · {}", thread.title));
    if let Some(issue) = &thread.issue {
        text.push_str(&format!(" ({issue})"));
    }
    if let Some(message) = thread.messages.first() {
        let who = message
            .handle
            .as_deref()
            .or(message.user.as_deref())
            .unwrap_or(&message.sender);
        text.push_str(&format!("\n\n{who} · {}", message.source()));
        if include_opening_message {
            text.push_str(&format!("\n{}", message.body));
        }
    } else {
        text.push_str(&format!("\n\n{}", thread.question));
    }
    for option in &thread.options {
        let suggested = if thread.suggested.as_deref() == Some(&option.label) {
            " (suggested)"
        } else {
            ""
        };
        text.push_str(&format!("\n{}{}  {}", option.label, suggested, option.line));
    }
    if !thread.context.trim().is_empty() {
        text.push_str(&format!("\n\n{}", thread.context));
    }
    text
}

fn terminal_text(
    opening: &Thread,
    thread: &Thread,
    include_opening_message: bool,
    source: &str,
) -> String {
    let state = match thread.state {
        ThreadState::Answered => "Answered",
        ThreadState::Closed => "Closed",
        ThreadState::Withdrawn => "Withdrawn",
        ThreadState::Open => "Open",
    };
    let detail = thread
        .answer
        .as_ref()
        .map(|answer| thread.answer_text(answer));
    let detail = detail
        .as_deref()
        .or_else(|| thread.messages.last().map(|m| m.body.as_str()));
    let mut text = root_text(opening, include_opening_message);
    if let Some(detail) = detail.filter(|detail| !detail.eq_ignore_ascii_case(state)) {
        text = format!("{detail}\n\n{text}");
    }
    format!("{state} · {source}\n\n{text}")
}

fn delivery_id(ev: &Event, subject: &str) -> String {
    let id = ev.msg_id();
    if ev.subject() == subject {
        id
    } else {
        let source = subject.rsplit('.').next().unwrap_or_default();
        format!("{id}-{source}")
    }
}

fn escaped_text(text: &str, limit: usize) -> String {
    const CLIPPED: &str = "\n… (full text in hgl-ai)";
    let mut escaped = String::new();
    for ch in text.chars() {
        let extra = match ch {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            _ => "",
        };
        if escaped.len() + ch.len_utf8().max(extra.len()) > limit - CLIPPED.len() {
            escaped.push_str(CLIPPED);
            break;
        }
        if extra.is_empty() {
            escaped.push(ch);
        } else {
            escaped.push_str(extra);
        }
    }
    escaped
}

#[cfg(test)]
mod tests;
