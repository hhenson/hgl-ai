use serde::{Deserialize, Serialize};

pub const BOT_TOKEN_FILE: &str = "slack-bot.token";
pub const APP_TOKEN_FILE: &str = "slack-app.token";

/// One workspace, an owner account for Q&A and an optional bot-only status channel.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Slack {
    pub team: String,
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_channel: Option<String>,
    pub user: String,
    pub handle: String,
}

impl Slack {
    pub fn link_key(&self, id: &str) -> String {
        format!("links.{}_{}.{id}", self.team, self.channel)
    }

    pub fn validate(&self) -> Result<(), String> {
        for (field, value, prefixes) in [
            ("team", self.team.as_str(), "T"),
            ("channel", self.channel.as_str(), "CG"),
            ("user", self.user.as_str(), "UW"),
        ]
        .into_iter()
        .chain(
            self.status_channel
                .as_deref()
                .map(|id| ("status_channel", id, "CG")),
        ) {
            let bytes = value.as_bytes();
            if !(2..=32).contains(&bytes.len())
                || !prefixes.as_bytes().contains(&bytes[0])
                || !bytes
                    .iter()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            {
                return Err(format!("[slack] {field} must be a Slack ID"));
            }
        }
        if self.status_channel.as_ref() == Some(&self.channel) {
            return Err("[slack] status_channel must differ from the Q&A channel".into());
        }
        hgl_ai_proto::check_handle(&self.handle).map_err(|e| format!("[slack] handle: {e}"))
    }
}

/// A message link returned by Slack, safe to put in a console link.
pub fn valid_permalink(link: &str) -> bool {
    let Some((host, path)) = link
        .strip_prefix("https://")
        .and_then(|url| url.split_once('/'))
    else {
        return false;
    };
    host.ends_with(".slack.com")
        && host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
        && path.starts_with("archives/")
        && !path.chars().any(char::is_whitespace)
}
