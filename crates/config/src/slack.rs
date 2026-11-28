use serde::{Deserialize, Serialize};

pub const BOT_TOKEN_FILE: &str = "slack-bot.token";
pub const APP_TOKEN_FILE: &str = "slack-app.token";

/// Stage 1 permits one private Q&A channel in one workspace and one owner account.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Slack {
    pub team: String,
    pub channel: String,
    pub user: String,
    pub handle: String,
}

impl Slack {
    pub fn validate(&self) -> Result<(), String> {
        for (field, value, prefixes) in [
            ("team", self.team.as_str(), "T"),
            ("channel", self.channel.as_str(), "CG"),
            ("user", self.user.as_str(), "UW"),
        ] {
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
        hgl_ai_proto::check_handle(&self.handle).map_err(|e| format!("[slack] handle: {e}"))
    }
}
