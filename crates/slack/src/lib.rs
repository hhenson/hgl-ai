//! Stage 1 of the Slack adapter (docs/owner.md). The hub keeps the event log,
//! delivery state, and thread mappings for questions and conversations.

mod interaction;
mod web;

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use chrono::{DateTime, Utc};
use futures::{SinkExt as _, StreamExt as _};
use hgl_ai_bus::{Bus, BusError, StreamMsg};
use hgl_ai_config::slack::{APP_TOKEN_FILE, BOT_TOKEN_FILE, Slack};
use hgl_ai_owner::{Body, Event, Origin, Thread, ThreadState, event, fold};
use hgl_ai_proto::{
    CONSOLE_PREFIX, OWNER, OWNER_PREFIX, SLACK_FEED, SLACK_OWNER_FILTER, Seat, utc_stamp,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::{sleep, timeout};
use tokio_websockets::Message;

use web::{Bot, Marker, Web};

const RECONNECT: Duration = Duration::from_secs(5);
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(45);
const RETRY_AFTER: i64 = 30;
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
    let payload = Callback::deserialize(&env.payload).ok()?;
    let msg = payload.event?;
    if env.kind != "events_api"
        || payload.kind != "event_callback"
        || payload.team_id.as_deref() != Some(cfg.team.as_str())
        || msg.kind != "message"
        || msg.channel_type.as_deref() != Some("group")
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
    let parent = msg.thread_ts?;
    let root = slack_timestamp(&parent)?;
    let ts = msg.ts?;
    let at = slack_timestamp(&ts)?;
    let text = msg.text?;
    (at > root && !text.trim().is_empty() && text.chars().count() <= SLACK_TEXT_LIMIT).then_some(
        Reply {
            parent,
            ts,
            at,
            text,
        },
    )
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
pub async fn run(bus: &Bus, cfg: Slack, controller: Seat, secret_dir: &Path) -> Result<()> {
    let app = secret(secret_dir, APP_TOKEN_FILE, "xapp-")?;
    let bot = secret(secret_dir, BOT_TOKEN_FILE, "xoxb-")?;
    let adapter = Adapter::new(
        bus,
        cfg,
        controller,
        Web::new("https://slack.com/api".into(), app, bot)?,
    )
    .await?;
    tokio::join!(adapter.outbound_loop(), adapter.socket_loop());
    Ok(())
}

struct Adapter<'a> {
    bus: &'a Bus,
    cfg: Slack,
    controller: Seat,
    bot: Bot,
    web: Web,
}

#[derive(Serialize, Deserialize)]
struct Delivery {
    event: String,
    started: i64,
    attempted: i64,
    ts: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    opening_seq: Option<u64>,
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
        }
    }
}

impl<'a> Adapter<'a> {
    async fn new(bus: &'a Bus, cfg: Slack, controller: Seat, web: Web) -> Result<Self> {
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
        let Some(post) = self.web.root_at(&self.cfg.channel, parent).await? else {
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

    async fn ingest_reply(&self, envelope: &Envelope) -> Result<()> {
        let Some(reply) = owner_reply(envelope, &self.cfg, &self.bot) else {
            return Ok(());
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
        if !matches!(event.body, Body::OwnerMessage { .. }) {
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
        loop {
            if let Err(e) = self.socket_session().await {
                tracing::warn!("Slack Socket Mode: {e}");
            }
            sleep(RECONNECT).await;
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
                "events_api" => self.ingest_reply(&envelope).await.map(|()| None),
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

    async fn outbound_loop(&self) {
        loop {
            if let Err(e) = self.drain_feed().await {
                tracing::warn!("Slack owner feed: {e}");
            }
            sleep(RECONNECT).await;
        }
    }

    async fn drain_feed(&self) -> Result<()> {
        let mut feed = self.open_feed().await?;
        loop {
            let Some(message) = feed.next(RECONNECT).await? else {
                continue;
            };
            let delivery = timeout(DELIVERY_TIMEOUT, self.deliver(&message))
                .await
                .context("Slack delivery timed out")
                .and_then(|result| result);
            if let Err(error) = delivery {
                tracing::warn!(seq = message.seq, "Slack delivery pending: {error}");
                message.nak(RECONNECT).await?;
            } else {
                message.ack().await?;
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
        if hgl_ai_owner::check_id(&ev.id).is_err()
            || event::parse_subject(&message.subject) != Some((ev.body.origin(), &ev.id))
            || ev.v != event::EVENT_VERSION
            || hgl_ai_proto::parse_stamp(&ev.at).is_none()
        {
            return Ok(());
        }
        let origin_ok = match ev.body.origin() {
            Origin::Owner => ev.sender == OWNER,
            Origin::Agent => Seat::new(ev.sender.clone()).is_ok_and(|seat| !seat.is_reserved()),
        };
        if !origin_ok {
            return Ok(());
        }
        let root_key = self.root_key(&ev.id);
        let event_id = delivery_id(&ev, &message.subject);
        let from_slack = ev.subject() != message.subject;
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
                let text = root_text(&thread);
                let blocks = interaction::root_blocks(&thread, &text);
                let ts = self
                    .post_once(&root_key, None, marker, &text, &blocks, Some(message.seq))
                    .await?;
                self.map_thread(&ev.id, &ts).await?;
            }
            Body::Message { body } | Body::OwnerMessage { body }
                if !from_slack && !body.trim().is_empty() =>
            {
                let Some((root, _)) = self.bus.slack_get::<Delivery>(&root_key).await? else {
                    return Ok(());
                };
                let parent = root.ts.context("the Slack thread is still pending")?;
                let who = ev.handle.as_deref().unwrap_or(&ev.sender);
                let text = format!("{who}\n{body}");
                let key = self.key("out", &event_id);
                self.post_once(&key, Some(&parent), marker, &text, &[], None)
                    .await?;
            }
            Body::Answer { .. } | Body::Close | Body::Withdraw { .. } => {
                self.edit_state(message, &ev, &event_id).await?;
            }
            _ => {}
        }
        Ok(())
    }

    fn root_key(&self, id: &str) -> String {
        self.key("question", id)
    }

    async fn opening(&self, root: &Delivery) -> Result<Thread> {
        let seq = root
            .opening_seq
            .context("the Slack thread has no opening event")?;
        let message = self.bus.owner_event_at(seq).await?;
        let ev: Event = serde_json::from_slice(&message.payload)?;
        ensure!(
            ev.msg_id() == root.event,
            "the Slack thread points to the wrong opening event"
        );
        Ok(fold(None, &message.subject, seq, &ev, &self.controller)?.0)
    }

    async fn edit_state(&self, message: &StreamMsg, ev: &Event, event_id: &str) -> Result<()> {
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
        let parent = root
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
            let posted = self.web.root_at(&self.cfg.channel, parent).await?;
            if posted
                .and_then(|post| post.marker(&self.bot))
                .is_some_and(|marker| {
                    marker.event == root.event && marker.state_event.as_deref() == Some(event_id)
                })
            {
                self.record_post(&key, revision, edit, parent.into())
                    .await?;
                return Ok(());
            }
            ensure!(
                Utc::now().timestamp() >= edit.attempted + RETRY_AFTER,
                "waiting to reconcile a Slack edit"
            );
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
        let text = terminal_text(&opening, &thread);
        self.web
            .update(
                &self.cfg.channel,
                parent,
                marker,
                &escaped_text(&text, 4_000),
                &interaction::text_blocks(&text),
            )
            .await?;
        self.record_post(&key, claim, edit, parent.into()).await?;
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
        parent: Option<&str>,
        marker: Marker,
        text: &str,
        blocks: &[Value],
        opening_seq: Option<u64>,
    ) -> Result<String> {
        let now = Utc::now().timestamp();
        let (mut delivery, revision) = match self.bus.slack_get::<Delivery>(key).await? {
            Some((delivery, revision)) => {
                if let Some(ts) = delivery.ts {
                    return Ok(ts);
                }
                ensure!(
                    delivery.event == marker.event,
                    "the original Slack post is still pending"
                );
                let oldest = (delivery.started - 5).to_string();
                if let Some(ts) = self
                    .web
                    .find(&self.cfg.channel, parent, &oldest, &marker.event, &self.bot)
                    .await?
                {
                    return self.record_post(key, revision, delivery, ts).await;
                }
                if now < delivery.attempted + RETRY_AFTER {
                    bail!("waiting to reconcile an earlier Slack post");
                }
                (delivery, revision)
            }
            None => (Delivery::pending(&marker.event, opening_seq), 0),
        };
        delivery.attempted = Utc::now().timestamp();
        let claim = self.bus.slack_put(key, &delivery, revision).await?;
        let ts = self
            .web
            .post(
                &self.cfg.channel,
                parent,
                marker,
                &escaped_text(text, SLACK_TEXT_LIMIT),
                blocks,
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

fn root_text(thread: &Thread) -> String {
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
        text.push_str(&format!("\n\n{who}\n{}", message.body));
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

fn terminal_text(opening: &Thread, thread: &Thread) -> String {
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
    let mut text = root_text(opening);
    if let Some(detail) = detail.filter(|detail| !detail.eq_ignore_ascii_case(state)) {
        text = format!("{detail}\n\n{text}");
    }
    format!("{state}\n\n{text}")
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
