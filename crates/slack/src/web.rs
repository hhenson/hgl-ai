use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use reqwest::{Client, RequestBuilder, StatusCode, header::RETRY_AFTER};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep_until};
use tokio_websockets::{ClientBuilder, Limits, MaybeTlsStream, WebSocketStream};

use crate::retry::RetryAfter;
use crate::slack_timestamp;

const TIMEOUT: Duration = Duration::from_secs(10);
const METADATA_TYPE: &str = "hgl_ai_question_event";

#[derive(Clone)]
pub struct Web {
    client: Client,
    base: String,
    app: String,
    bot: String,
    limited: Arc<Mutex<HashMap<&'static str, Instant>>>,
    next_post: Arc<tokio::sync::Mutex<Instant>>,
}

#[derive(Deserialize)]
pub struct Bot {
    pub team_id: String,
    pub user_id: String,
    pub bot_id: String,
}

#[derive(Serialize, Deserialize)]
struct Metadata {
    event_type: String,
    event_payload: Marker,
}

#[derive(Serialize, Deserialize)]
pub struct Marker {
    pub event: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_event: Option<String>,
}

#[derive(Deserialize)]
pub struct Posted {
    pub ts: String,
    user: Option<String>,
    bot_id: Option<String>,
    metadata: Option<serde_json::Value>,
}

impl Posted {
    pub fn marker(&self, bot: &Bot) -> Option<Marker> {
        if (self.user.as_deref() != Some(&bot.user_id)
            && self.bot_id.as_deref() != Some(&bot.bot_id))
            || slack_timestamp(&self.ts).is_none()
        {
            return None;
        }
        let metadata: Metadata = serde_json::from_value(self.metadata.clone()?).ok()?;
        (metadata.event_type == METADATA_TYPE).then_some(metadata.event_payload)
    }
}

#[derive(Deserialize)]
struct History {
    messages: Vec<Posted>,
    #[serde(default)]
    has_more: bool,
    response_metadata: Option<Page>,
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    next_cursor: String,
}

impl Web {
    pub fn new(base: String, app: String, bot: String) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(TIMEOUT)
                .build()?,
            base,
            app,
            bot,
            limited: Arc::default(),
            next_post: Arc::new(tokio::sync::Mutex::new(Instant::now())),
        })
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &'static str,
        request: RequestBuilder,
    ) -> Result<T> {
        let now = Instant::now();
        if let Some(until) = self.limited.lock().unwrap().get(method).copied()
            && until > now
        {
            return Err(RetryAfter(until - now).into());
        }
        // URLs from Slack and errors from the HTTP client can contain credentials.
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("{method}: request failed"))?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            // Slack sends delta seconds. Keep the method unavailable to both loops.
            let now = Instant::now();
            let until = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
                .and_then(|seconds| now.checked_add(Duration::from_secs(seconds)))
                .unwrap_or(now + Duration::from_secs(30));
            let mut limits = self.limited.lock().unwrap();
            let until = limits
                .entry(method)
                .and_modify(|old| *old = (*old).max(until))
                .or_insert(until);
            return Err(RetryAfter(until.saturating_duration_since(now)).into());
        }
        ensure!(
            response.status().is_success(),
            "{method}: HTTP {}",
            response.status()
        );
        let private_history = if method == "auth.test" {
            let scopes = response
                .headers()
                .get("x-oauth-scopes")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            let scopes: Vec<_> = scopes.split(',').map(str::trim).collect();
            scopes.contains(&"groups:history")
                && !["channels:history", "im:history", "mpim:history"]
                    .iter()
                    .any(|scope| scopes.contains(scope))
        } else {
            true
        };
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|_| anyhow::anyhow!("{method}: unreadable response"))?;
        if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let code = body
                .get("error")
                .and_then(|v| v.as_str())
                .filter(|s| s.len() <= 64 && s.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'))
                .unwrap_or("unknown_error");
            bail!("{method}: {code}");
        }
        ensure!(
            private_history,
            "the Slack bot token must be scoped to private channel history"
        );
        serde_json::from_value(body).map_err(|_| anyhow::anyhow!("{method}: incomplete response"))
    }

    pub async fn identify(&self) -> Result<Bot> {
        let method = "auth.test";
        let bot: Bot = self
            .call(
                method,
                self.client
                    .post(format!("{}/{method}", self.base))
                    .bearer_auth(&self.bot),
            )
            .await?;
        ensure!(
            !bot.user_id.is_empty() && !bot.bot_id.is_empty(),
            "Slack bot identity is missing"
        );
        Ok(bot)
    }

    /// With the history scopes checked at sign-in, a successful read establishes
    /// that the bot belongs to the configured private channel.
    pub async fn verify_channel(&self, channel: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp().to_string();
        self.history(channel, None, &now, None, "")
            .await
            .context("checking the private Slack Q&A channel")?;
        Ok(())
    }

    pub async fn socket(&self) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        #[derive(Deserialize)]
        struct Open {
            url: String,
        }
        let method = "apps.connections.open";
        let open: Open = self
            .call(
                method,
                self.client
                    .post(format!("{}/{method}", self.base))
                    .bearer_auth(&self.app),
            )
            .await?;
        ensure!(
            open.url.starts_with("wss://")
                || (cfg!(test) && open.url.starts_with("ws://127.0.0.1:")),
            "Slack returned an insecure Socket Mode URL"
        );
        let builder = ClientBuilder::new()
            .uri(&open.url)
            .map_err(|_| anyhow::anyhow!("Slack returned an invalid Socket Mode URL"))?
            .limits(Limits::default().max_payload_len(Some(1 << 20)));
        let (socket, _) = tokio::time::timeout(TIMEOUT, builder.connect())
            .await
            .context("Slack Socket Mode connection timed out")?
            .map_err(|_| anyhow::anyhow!("could not connect to Slack Socket Mode"))?;
        Ok(socket)
    }

    pub async fn post(
        &self,
        channel: &str,
        parent: Option<&str>,
        marker: Marker,
        text: &str,
        blocks: &[Value],
    ) -> Result<String> {
        {
            // The configured channel is shared by live traffic and the initial replay.
            let mut next = self.next_post.lock().await;
            sleep_until(*next).await;
            *next = Instant::now() + Duration::from_secs(1);
        }
        #[derive(Serialize)]
        struct Post<'a> {
            channel: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            thread_ts: Option<&'a str>,
            client_msg_id: String,
            text: &'a str,
            #[serde(skip_serializing_if = "<[Value]>::is_empty")]
            blocks: &'a [Value],
            mrkdwn: bool,
            parse: &'a str,
            unfurl_links: bool,
            unfurl_media: bool,
            metadata: Metadata,
        }
        #[derive(Deserialize)]
        struct Sent {
            channel: String,
            ts: String,
        }
        let hash = hex::encode(Sha256::digest(marker.event.as_bytes()));
        let client_msg_id = format!(
            "{}-{}-4{}-8{}-{}",
            &hash[..8],
            &hash[8..12],
            &hash[13..16],
            &hash[17..20],
            &hash[20..32]
        );
        let request = Post {
            channel,
            thread_ts: parent,
            client_msg_id,
            text,
            blocks,
            mrkdwn: false,
            parse: "none",
            unfurl_links: false,
            unfurl_media: false,
            metadata: Metadata {
                event_type: METADATA_TYPE.into(),
                event_payload: marker,
            },
        };
        let method = "chat.postMessage";
        let sent: Sent = self
            .call(
                method,
                self.client
                    .post(format!("{}/{method}", self.base))
                    .bearer_auth(&self.bot)
                    .json(&request),
            )
            .await?;
        ensure!(
            sent.channel == channel && slack_timestamp(&sent.ts).is_some(),
            "{method}: unexpected channel or message timestamp"
        );
        Ok(sent.ts)
    }

    pub async fn permalink(&self, channel: &str, ts: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Link {
            permalink: String,
        }
        let method = "chat.getPermalink";
        let link: Link = self
            .call(
                method,
                self.client
                    .get(format!("{}/{method}", self.base))
                    .bearer_auth(&self.bot)
                    .query(&[("channel", channel), ("message_ts", ts)]),
            )
            .await?;
        ensure!(
            hgl_ai_config::slack::valid_permalink(&link.permalink),
            "Slack returned an invalid permalink"
        );
        Ok(link.permalink)
    }

    pub async fn update(
        &self,
        channel: &str,
        ts: &str,
        marker: Marker,
        text: &str,
        blocks: &[Value],
    ) -> Result<()> {
        let method = "chat.update";
        self.call::<Value>(
            method,
            self.client
                .post(format!("{}/{method}", self.base))
                .bearer_auth(&self.bot)
                .json(&json!({
                    "channel": channel, "ts": ts, "text": text, "mrkdwn": false, "parse": "none",
                "blocks": blocks,
                    "metadata": Metadata { event_type: METADATA_TYPE.into(), event_payload: marker }
                })),
        )
        .await?;
        Ok(())
    }

    pub async fn open_modal(&self, trigger: &str, view: Value) -> Result<()> {
        let method = "views.open";
        self.call::<Value>(
            method,
            self.client
                .post(format!("{}/{method}", self.base))
                .bearer_auth(&self.bot)
                .timeout(Duration::from_secs(2))
                .json(&json!({ "trigger_id": trigger, "view": view })),
        )
        .await?;
        Ok(())
    }

    /// Match a prior attempt by metadata from the authenticated bot in the same channel
    /// or thread. On an incomplete history read, leave the attempt pending.
    pub async fn find(
        &self,
        channel: &str,
        parent: Option<&str>,
        oldest: &str,
        event: &str,
        bot: &Bot,
    ) -> Result<Option<String>> {
        let mut cursor = String::new();
        loop {
            let page = self.history(channel, parent, oldest, None, &cursor).await?;
            for message in page.messages {
                if message.marker(bot).is_some_and(|m| m.event == event) {
                    return Ok(Some(message.ts));
                }
            }
            let next = page
                .response_metadata
                .map_or(String::new(), |p| p.next_cursor);
            if next.is_empty() {
                ensure!(!page.has_more, "Slack returned incomplete message history");
                return Ok(None);
            }
            ensure!(cursor != next, "Slack repeated a message history cursor");
            cursor = next;
        }
    }

    /// Look up a root by its exact timestamp if its reverse mapping is still being written.
    pub async fn message_at(
        &self,
        channel: &str,
        parent: Option<&str>,
        ts: &str,
    ) -> Result<Option<Posted>> {
        let page = self.history(channel, parent, ts, Some(ts), "").await?;
        Ok(page.messages.into_iter().find(|message| message.ts == ts))
    }

    async fn history(
        &self,
        channel: &str,
        parent: Option<&str>,
        oldest: &str,
        latest: Option<&str>,
        cursor: &str,
    ) -> Result<History> {
        let method = if parent.is_some() {
            "conversations.replies"
        } else {
            "conversations.history"
        };
        let mut request = self
            .client
            .get(format!("{}/{method}", self.base))
            .bearer_auth(&self.bot)
            .query(&[
                ("channel", channel),
                ("oldest", oldest),
                ("include_all_metadata", "true"),
            ]);
        if let Some(ts) = parent {
            request = request.query(&[("ts", ts)]);
        }
        if let Some(latest) = latest {
            request = request.query(&[("latest", latest), ("inclusive", "true")]);
        }
        if !cursor.is_empty() {
            request = request.query(&[("cursor", cursor)]);
        }
        self.call(method, request).await
    }
}
