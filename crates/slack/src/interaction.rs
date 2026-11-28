//! Block Kit controls on mapped roots and their Socket Mode callbacks.

use anyhow::Result;
use hgl_ai_owner::{Body, Choice, Thread, ThreadKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Adapter, Delivery, slack_timestamp, timestamp_key};

const CLOSE: &str = "hgl_close";
const ANSWER: &str = "hgl_answer";
const ANSWER_VIEW: &str = "hgl_answer_view";
const MAX_OPTION_BUTTONS: usize = 100;

pub fn text_blocks(text: &str) -> Vec<Value> {
    let mut blocks = Vec::new();
    let mut remaining = text;
    // Keep question text above its controls and fit Slack's section and message limits.
    for part in 0..12 {
        if remaining.is_empty() {
            break;
        }
        let split = remaining
            .char_indices()
            .nth(2900)
            .map_or(remaining.len(), |(i, _)| i);
        let (section, rest) = remaining.split_at(split);
        let section = if part == 11 && !rest.is_empty() {
            format!("{section}\n… (full text in hgl-ai)")
        } else {
            section.into()
        };
        blocks.push(json!({ "type": "section", "text": plain(&section) }));
        remaining = rest;
    }
    blocks
}

pub fn root_blocks(thread: &Thread, text: &str) -> Vec<Value> {
    let mut blocks = text_blocks(text);
    let buttons = match thread.kind {
        ThreadKind::Conversation => vec![button(CLOSE, "Close", "close")],
        ThreadKind::Question => {
            let mut buttons = Vec::new();
            if thread.suggested.is_some() {
                buttons.push(button("hgl_accept", "Accept suggestion", "accept"));
            }
            for option in thread.options.iter().take(MAX_OPTION_BUTTONS) {
                let label: String = format!("{}: {}", option.label, option.line)
                    .chars()
                    .take(60)
                    .collect();
                buttons.push(button(
                    &format!("hgl_option_{}", option.label),
                    &label,
                    &format!("option:{}", option.label),
                ));
            }
            if thread.options.len() > MAX_OPTION_BUTTONS {
                buttons.push(button("hgl_pick", "Pick by label", "option"));
            }
            buttons.push(button("hgl_reject", "Reject", "reject"));
            buttons.push(button(ANSWER, "Answer in my own words", "other"));
            buttons
        }
    };
    for (i, elements) in buttons.chunks(25).enumerate() {
        blocks.push(json!({ "type": "actions", "block_id": format!("hgl_actions_{i}"), "elements": elements }));
    }
    blocks
}

fn plain(text: &str) -> Value {
    json!({ "type": "plain_text", "text": text })
}

pub fn console_link(url: &str) -> Value {
    json!({ "type": "section", "text": {
        "type": "mrkdwn", "text": format!("<{url}|Open in console>"), "verbatim": true
    } })
}

fn button(action: &str, label: &str, value: &str) -> Value {
    json!({ "type": "button", "action_id": action, "text": plain(label), "value": value })
}

#[derive(Deserialize)]
struct Identity {
    id: String,
}

#[derive(Deserialize)]
struct User {
    id: String,
    team_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Callback {
    BlockActions {
        team: Identity,
        user: User,
        channel: Identity,
        container: Container,
        message: Source,
        actions: Vec<Click>,
        trigger_id: Option<String>,
    },
    ViewSubmission {
        team: Identity,
        user: User,
        view: SubmittedView,
    },
}

#[derive(Deserialize)]
struct Container {
    #[serde(rename = "type")]
    kind: String,
    channel_id: String,
    message_ts: String,
    #[serde(default)]
    is_ephemeral: bool,
    #[serde(default)]
    is_app_unfurl: bool,
}

#[derive(Deserialize)]
struct Source {
    ts: String,
    thread_ts: Option<String>,
    user: Option<String>,
    bot_id: Option<String>,
}

#[derive(Deserialize)]
struct Click {
    #[serde(rename = "type")]
    kind: String,
    action_id: String,
    block_id: String,
    value: String,
    action_ts: String,
}

#[derive(Deserialize)]
struct SubmittedView {
    callback_id: String,
    private_metadata: String,
    state: ViewState,
}

#[derive(Deserialize)]
struct ViewState {
    values: Value,
}

#[derive(Deserialize, Serialize)]
struct AnswerForm {
    channel: String,
    parent: String,
    action_ts: String,
    choice: Choice,
    option: Option<String>,
}

impl AnswerForm {
    fn view(&self) -> Result<Value> {
        let mut blocks = Vec::new();
        if self.choice == Choice::Other {
            blocks.push(input("text", "Your answer", false));
        }
        if self.choice == Choice::Option && self.option.is_none() {
            blocks.push(input("option", "Option label", false));
        }
        blocks.push(input("note", "Note", true));
        let title = match (&self.choice, &self.option) {
            (Choice::Accept, _) => "Accept suggestion".into(),
            (Choice::Option, Some(label)) => format!("Choose {label}"),
            (Choice::Option, None) => "Choose an option".into(),
            (Choice::Reject, _) => "Reject".into(),
            (Choice::Other, _) => "Answer".into(),
        };
        Ok(json!({
            "type": "modal", "callback_id": ANSWER_VIEW, "private_metadata": serde_json::to_string(self)?,
            "title": plain(&title), "submit": plain("Send answer"), "blocks": blocks
        }))
    }
}

fn input(id: &str, label: &str, optional: bool) -> Value {
    json!({
        "type": "input", "block_id": id, "label": plain(label), "optional": optional,
        "element": { "type": "plain_text_input", "action_id": id, "multiline": id != "option" }
    })
}

fn value<'a>(view: &'a SubmittedView, id: &str) -> &'a str {
    view.state.values[id][id]["value"]
        .as_str()
        .unwrap_or_default()
}

fn field_error(block: &str, text: &str) -> Option<Value> {
    Some(json!({ "response_action": "errors", "errors": { (block): text } }))
}

impl Adapter<'_> {
    fn is_owner(&self, team: &Identity, user: &User) -> bool {
        team.id == self.cfg.team
            && user.id == self.cfg.user
            && user
                .team_id
                .as_ref()
                .is_none_or(|team| team == &self.cfg.team)
    }

    pub(crate) async fn ingest_interaction(&self, payload: &Value) -> Result<Option<Value>> {
        let Ok(callback) = Callback::deserialize(payload) else {
            return Ok(None);
        };
        match callback {
            Callback::BlockActions {
                team,
                user,
                channel,
                container,
                message,
                actions,
                trigger_id,
            } => {
                if !self.is_owner(&team, &user)
                    || channel.id != self.cfg.channel
                    || container.channel_id != self.cfg.channel
                    || container.kind != "message"
                    || container.is_ephemeral
                    || container.is_app_unfurl
                    || container.message_ts != message.ts
                    || (message.user.as_deref() != Some(&self.bot.user_id)
                        && message.bot_id.as_deref() != Some(&self.bot.bot_id))
                {
                    return Ok(None);
                }
                if let [click] = actions.as_slice() {
                    self.handle_click(&message, click, trigger_id.as_deref())
                        .await?;
                }
                Ok(None)
            }
            Callback::ViewSubmission { team, user, view }
                if self.is_owner(&team, &user) && view.callback_id == ANSWER_VIEW =>
            {
                self.submit_answer(&view).await
            }
            _ => Ok(None),
        }
    }

    async fn handle_click(
        &self,
        message: &Source,
        click: &Click,
        trigger: Option<&str>,
    ) -> Result<()> {
        let parent = message.thread_ts.as_deref().unwrap_or(&message.ts);
        let (Some(root), Some(at)) = (slack_timestamp(parent), slack_timestamp(&click.action_ts))
        else {
            return Ok(());
        };
        if click.kind != "button" || !click.block_id.starts_with("hgl_actions_") || at <= root {
            return Ok(());
        }
        let Some(id) = self.thread_for(parent).await? else {
            return Ok(());
        };
        if click.action_id == CLOSE && click.value == "close" && id.starts_with("c-") {
            self.publish_action(&id, &action_key(&click.action_ts), at, Body::Close)
                .await?;
        } else if let Some(trigger) = trigger
            && id.starts_with("q-")
            && self
                .bus
                .slack_get::<Delivery>(&self.key("state", &id))
                .await?
                .is_none()
        {
            let (choice, option) = match (click.action_id.as_str(), click.value.as_str()) {
                ("hgl_accept", "accept") => (Choice::Accept, None),
                ("hgl_reject", "reject") => (Choice::Reject, None),
                (ANSWER, "other") => (Choice::Other, None),
                ("hgl_pick", "option") => (Choice::Option, None),
                (action, value)
                    if value.strip_prefix("option:").is_some_and(|label| {
                        action.strip_prefix("hgl_option_") == Some(label)
                            && hgl_ai_owner::model::valid_label(label)
                    }) =>
                {
                    (Choice::Option, Some(value["option:".len()..].into()))
                }
                _ => return Ok(()),
            };
            let form = AnswerForm {
                channel: self.cfg.channel.clone(),
                parent: parent.into(),
                action_ts: click.action_ts.clone(),
                choice,
                option,
            };
            // A trigger is short-lived and single-use. A retry of a consumed trigger
            // cannot open a second modal, and the modal itself publishes no event.
            if let Err(e) = self.web.open_modal(trigger, form.view()?).await {
                tracing::warn!("opening a Slack answer form: {e}");
            }
        }
        Ok(())
    }

    async fn submit_answer(&self, view: &SubmittedView) -> Result<Option<Value>> {
        let Ok(form) = serde_json::from_str::<AnswerForm>(&view.private_metadata) else {
            return Ok(None);
        };
        let (Some(root), Some(at)) = (
            slack_timestamp(&form.parent),
            slack_timestamp(&form.action_ts),
        ) else {
            return Ok(None);
        };
        if form.channel != self.cfg.channel || at <= root {
            return Ok(None);
        }
        let Some(id) = self.thread_for(&form.parent).await? else {
            return Ok(None);
        };
        let text = value(view, "text");
        if form.choice == Choice::Other && text.trim().is_empty() {
            return Ok(field_error("text", "Enter your answer."));
        }
        let option = if form.choice == Choice::Option {
            Some(
                form.option
                    .unwrap_or_else(|| value(view, "option").trim().into()),
            )
        } else {
            None
        };
        let body = Body::Answer {
            choice: form.choice,
            option,
            text: text.into(),
            note: value(view, "note").into(),
        };
        if !self
            .publish_action(&id, &action_key(&form.action_ts), at, body)
            .await?
        {
            return Ok(field_error(
                "note",
                "The answer could not be saved. Check the thread state and your answer.",
            ));
        }
        Ok(None)
    }
}

fn action_key(ts: &str) -> String {
    format!("action_{}", timestamp_key(ts))
}
