//! Stage 1 of the Slack adapter (docs/owner.md). The hub keeps the event log,
//! delivery state, and thread mappings. Slack is a Q&A surface.

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
use hgl_ai_owner::{Body, Choice, Event, Origin, event};
use hgl_ai_proto::{CONSOLE_PREFIX, OWNER, OWNER_PREFIX, SLACK_FEED, Seat, utc_stamp};
use serde::{Deserialize, Serialize};
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
    payload: Option<Callback>,
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

struct Reply<'a> {
    parent: &'a str,
    ts: &'a str,
    at: DateTime<Utc>,
    text: &'a str,
}

fn owner_reply<'a>(env: &'a Envelope, cfg: &Slack, bot: &Bot) -> Option<Reply<'a>> {
    let payload = env.payload.as_ref()?;
    let msg = payload.event.as_ref()?;
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
    let parent = msg.thread_ts.as_deref()?;
    let root = slack_timestamp(parent)?;
    let ts = msg.ts.as_deref()?;
    let at = slack_timestamp(ts)?;
    let text = msg.text.as_deref()?;
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
pub async fn run(bus: &Bus, cfg: Slack, secret_dir: &Path) -> Result<()> {
    let app = secret(secret_dir, APP_TOKEN_FILE, "xapp-")?;
    let bot = secret(secret_dir, BOT_TOKEN_FILE, "xoxb-")?;
    let adapter = Adapter::new(
        bus,
        cfg,
        Web::new("https://slack.com/api".into(), app, bot)?,
    )
    .await?;
    tokio::join!(adapter.outbound_loop(), adapter.socket_loop());
    Ok(())
}

struct Adapter<'a> {
    bus: &'a Bus,
    cfg: Slack,
    bot: Bot,
    web: Web,
}

#[derive(Serialize, Deserialize)]
struct Delivery {
    event: String,
    started: i64,
    attempted: i64,
    ts: Option<String>,
}

impl<'a> Adapter<'a> {
    async fn new(bus: &'a Bus, cfg: Slack, web: Web) -> Result<Self> {
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
        Ok(Self { bus, cfg, bot, web })
    }

    fn key(&self, kind: &str, id: &str) -> String {
        format!("{}.{}.{kind}.{id}", self.cfg.team, self.cfg.channel)
    }

    async fn question_for(&self, parent: &str) -> Result<Option<String>> {
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
        if !marker.question.starts_with("q-") || hgl_ai_owner::check_id(&marker.question).is_err() {
            return Ok(None);
        }
        let delivery = self
            .bus
            .slack_get::<Delivery>(&self.key("question", &marker.question))
            .await?;
        Ok(delivery
            .filter(|(record, _)| {
                record.event == marker.event && record.ts.as_deref().is_none_or(|ts| ts == parent)
            })
            .map(|_| marker.question))
    }

    async fn ingest(&self, envelope: &Envelope) -> Result<()> {
        let Some(reply) = owner_reply(envelope, &self.cfg, &self.bot) else {
            return Ok(());
        };
        let Some(id) = self.question_for(reply.parent).await? else {
            return Ok(());
        };
        let subject = event::slack_subject(
            &id,
            &self.cfg.team,
            &self.cfg.channel,
            &timestamp_key(reply.ts),
        );
        let event = Event {
            v: event::EVENT_VERSION,
            id,
            sender: OWNER.into(),
            user: Some(format!("slack:{}:{}", self.cfg.team, self.cfg.user)),
            handle: Some(self.cfg.handle.clone()),
            at: utc_stamp(reply.at),
            body: Body::OwnerMessage {
                body: reply.text.into(),
            },
        };
        self.bus
            .publish_slack_event(&subject, &serde_json::to_vec(&event)?)
            .await?;
        Ok(())
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
            if let Err(e) = self.ingest(&envelope).await {
                // Slack retries an unacknowledged envelope.
                tracing::warn!("saving Slack owner message: {e}");
                continue;
            }
            let ack = serde_json::json!({ "envelope_id": id });
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
                ],
            )
            .await?)
    }

    async fn deliver(&self, message: &StreamMsg) -> Result<()> {
        let Ok(ev) = serde_json::from_slice::<Event>(&message.payload) else {
            return Ok(());
        };
        if !ev.id.starts_with("q-")
            || hgl_ai_owner::check_id(&ev.id).is_err()
            || message.subject != ev.subject()
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
        let root_key = self.key("question", &ev.id);
        let event_id = ev.msg_id();
        match &ev.body {
            Body::Ask(ask) if ask.validate().is_ok() => {
                let text = question_text(&ev.id, ask);
                let marker = Marker {
                    event: event_id,
                    question: ev.id.clone(),
                };
                let ts = self.post_once(&root_key, None, marker, &text).await?;
                self.map_thread(&ev.id, &ts).await?;
            }
            Body::Message { .. } | Body::OwnerMessage { .. } | Body::Answer { .. } => {
                let Some((root, _)) = self.bus.slack_get::<Delivery>(&root_key).await? else {
                    return Ok(());
                };
                let parent = root.ts.context("the Slack question is still pending")?;
                let Some(text) = reply_text(&ev) else {
                    return Ok(());
                };
                let key = self.key("out", &event_id);
                let marker = Marker {
                    event: event_id,
                    question: ev.id,
                };
                self.post_once(&key, Some(&parent), marker, &text).await?;
            }
            _ => {}
        }
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
    ) -> Result<String> {
        let now = Utc::now().timestamp();
        let (mut delivery, revision) = match self.bus.slack_get::<Delivery>(key).await? {
            Some((delivery, revision)) => {
                if let Some(ts) = delivery.ts {
                    return Ok(ts);
                }
                ensure!(
                    delivery.event == marker.event,
                    "the original Slack question is still pending"
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
            None => (
                Delivery {
                    event: marker.event.clone(),
                    started: now,
                    attempted: now,
                    ts: None,
                },
                0,
            ),
        };
        delivery.attempted = Utc::now().timestamp();
        let claim = self.bus.slack_put(key, &delivery, revision).await?;
        let ts = self
            .web
            .post(&self.cfg.channel, parent, marker, &slack_text(text))
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

fn question_text(id: &str, ask: &hgl_ai_owner::Ask) -> String {
    let mut text = format!("{id} · {} · {}", ask.priority, ask.title);
    if let Some(issue) = &ask.issue {
        text.push_str(&format!(" ({issue})"));
    }
    text.push_str(&format!("\n\n{}", ask.question));
    for option in &ask.options {
        let suggested = if ask.suggested.as_deref() == Some(&option.label) {
            " (suggested)"
        } else {
            ""
        };
        text.push_str(&format!("\n{}{}  {}", option.label, suggested, option.line));
    }
    if !ask.context.trim().is_empty() {
        text.push_str(&format!("\n\n{}", ask.context));
    }
    text
}

fn reply_text(ev: &Event) -> Option<String> {
    let who = ev.handle.as_deref().unwrap_or(&ev.sender);
    match &ev.body {
        Body::Message { body } | Body::OwnerMessage { body } if !body.trim().is_empty() => {
            Some(format!("{who}\n{body}"))
        }
        Body::Answer {
            choice,
            option,
            text,
            note,
        } => {
            let answer = match choice {
                Choice::Accept => "Accepted the suggested option".into(),
                Choice::Option => format!("Chose option {}", option.as_deref()?),
                Choice::Reject => "Rejected".into(),
                Choice::Other if !text.trim().is_empty() => text.clone(),
                Choice::Other => return None,
            };
            Some(format!("{who}\nAnswer: {answer}\n{note}").trim_end().into())
        }
        _ => None,
    }
}

fn slack_text(text: &str) -> String {
    const CLIPPED: &str = "\n… (full text in hgl-ai)";
    let mut escaped = String::new();
    for ch in text.chars() {
        let extra = match ch {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            _ => "",
        };
        if escaped.len() + ch.len_utf8().max(extra.len()) > SLACK_TEXT_LIMIT - CLIPPED.len() {
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
