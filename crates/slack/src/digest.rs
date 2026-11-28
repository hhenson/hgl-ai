//! Outbound-only delivery of console-generated status digests.

use anyhow::Result;
use chrono::{DateTime, Utc};
use hgl_ai_bus::{Feed, StreamMsg};
use hgl_ai_owner::check_id;
use hgl_ai_owner::digest::{self, Digest};
use hgl_ai_proto::{SLACK_STATUS_FEED, parse_stamp};
use serde_json::Value;

use crate::{Adapter, Backoff, Marker, Post, interaction, pause};

impl Adapter<'_> {
    pub(crate) async fn status_loop(&self) {
        let Some(channel) = self.cfg.status_channel.as_deref() else {
            return;
        };
        let mut retry = Backoff::default();
        while let Err(e) = self.web.verify_channel(channel).await {
            pause(&mut retry, Some(e), "Slack status channel").await;
        }
        self.outbound_loop(Some(channel)).await;
    }

    pub(crate) async fn open_status_feed(&self, channel: &str) -> Result<Feed> {
        let subject = digest::subject(&self.cfg.team, channel);
        Ok(self
            .bus
            .owner_feed(SLACK_STATUS_FEED, vec![subject])
            .await?)
    }

    pub(crate) async fn deliver_digest(&self, channel: &str, message: &StreamMsg) -> Result<()> {
        if message.subject != digest::subject(&self.cfg.team, channel) {
            return Ok(());
        }
        let Ok(batch) = serde_json::from_slice::<Digest>(&message.payload) else {
            tracing::warn!(seq = message.seq, "unreadable Slack digest");
            return Ok(());
        };
        let Some(through) = DateTime::from_timestamp(batch.through, 0) else {
            return Ok(());
        };
        if batch.v != digest::VERSION || batch.is_empty() {
            return Ok(());
        }
        let (text, blocks) = render(&batch, through, &self.console_origin);
        let event = format!("status-{}-{channel}-{}", self.cfg.team, batch.through);
        let key = format!("{}.{}.digest.{}", self.cfg.team, channel, batch.through);
        let marker = Marker {
            event,
            question: "status".into(),
            state_event: None,
        };
        self.post_once(
            &key,
            marker,
            Post {
                channel,
                parent: None,
                text: &text,
                blocks: &blocks,
            },
            None,
        )
        .await?;
        Ok(())
    }
}

fn render(batch: &Digest, through: DateTime<Utc>, console: &str) -> (String, Vec<Value>) {
    let title = format!("Status digest · {} UTC", through.format("%d %b %H:%M"));
    let mut text = String::new();
    let mut blocks = Vec::new();
    section(&mut text, &mut blocks, &title);

    for report in &batch.reports {
        let at = parse_stamp(&report.at).unwrap_or(through);
        let report_text = format!("{} UTC\n{}", at.format("%d %b %H:%M"), report.body);
        section(&mut text, &mut blocks, &report_text);
        blocks.push(interaction::console_link(&format!(
            "{console}/#status-{}",
            report.seq
        )));
    }
    if batch.more_reports > 0 {
        section(
            &mut text,
            &mut blocks,
            &format!("{} more status reports in the console", batch.more_reports),
        );
    }
    if !batch.decisions.is_empty() {
        section(&mut text, &mut blocks, "Decisions");
        for decision in &batch.decisions {
            section(&mut text, &mut blocks, &decision.preview);
            if check_id(&decision.thread).is_ok() {
                blocks.push(interaction::console_link(&format!(
                    "{console}/t/{}",
                    decision.thread
                )));
            }
        }
    }
    if batch.more_decisions > 0 {
        section(
            &mut text,
            &mut blocks,
            &format!("{} more entries in the decision log", batch.more_decisions),
        );
    }
    if !batch.decisions.is_empty() {
        blocks.push(interaction::console_link(&format!("{console}/#decisions")));
    }
    if !batch.questions.is_empty() {
        section(&mut text, &mut blocks, "Open p2 questions");
        for question in &batch.questions {
            section(&mut text, &mut blocks, &question.title);
            if check_id(&question.id).is_ok() {
                blocks.push(interaction::console_link(&format!(
                    "{console}/t/{}",
                    question.id
                )));
            }
        }
    }
    if batch.more_questions > 0 {
        section(
            &mut text,
            &mut blocks,
            &format!("{} more p2 questions", batch.more_questions),
        );
        blocks.push(interaction::console_link(&format!("{console}/questions")));
    }
    blocks.push(interaction::console_link(&format!("{console}/")));
    (text, blocks)
}

fn section(text: &mut String, blocks: &mut Vec<Value>, part: &str) {
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(part);
    blocks.extend(interaction::text_blocks(part));
}
