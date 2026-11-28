//! The rules and decisions stored in HGL_SCREEN.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Kind, ProtoError, Role, parse_stamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenAction {
    Close,
    Answer,
    Forward,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenMode {
    #[default]
    Shadow,
    Act,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenRule {
    pub id: String,
    pub kinds: Vec<Kind>,
    pub roles: Vec<Role>,
    pub when: String,
    pub action: ScreenAction,
    #[serde(default)]
    pub reply: String,
    pub cites: String,
    #[serde(default)]
    pub mode: ScreenMode,
    pub expires: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<String>,
}

impl ScreenRule {
    pub fn validate(&self) -> Result<(), ProtoError> {
        check_rule_id(&self.id)?;
        unique_nonempty(&self.kinds, "kinds")?;
        unique_nonempty(&self.roles, "roles")?;
        for (field, value) in [("when", &self.when), ("cites", &self.cites)] {
            if value.trim().is_empty() || value.contains(['\n', '\r']) {
                return Err(invalid(format!("{field} must be one nonempty line")));
            }
        }
        match self.action {
            ScreenAction::Close if self.reply.is_empty() => {}
            ScreenAction::Answer if !self.reply.trim().is_empty() => {}
            _ => {
                return Err(invalid(
                    "a rule must close without a reply or answer with a reply",
                ));
            }
        }
        if parse_stamp(&self.expires).is_none() {
            return Err(invalid(
                "expires must be a UTC timestamp (YYYYMMDDTHHMMSSZ)",
            ));
        }
        if let Some(stamp) = &self.retired_at
            && parse_stamp(stamp).is_none()
        {
            return Err(invalid(
                "retired_at must be a UTC timestamp (YYYYMMDDTHHMMSSZ)",
            ));
        }
        Ok(())
    }

    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        self.retired_at.is_none() && parse_stamp(&self.expires).is_some_and(|t| t > now)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenLog {
    pub seq: u64,
    pub rule: Option<String>,
    pub action: ScreenAction,
    #[serde(default)]
    pub reply: String,
    pub mode: ScreenMode,
    pub model: String,
}

impl ScreenLog {
    pub fn validate(&self) -> Result<(), ProtoError> {
        if self.seq == 0 || self.model.trim().is_empty() || self.model.contains(['\n', '\r']) {
            return Err(invalid(
                "a log entry needs a positive sequence and a model name",
            ));
        }
        if let Some(id) = &self.rule {
            check_rule_id(id)?;
        }
        match (self.action, &self.rule) {
            (ScreenAction::Close, Some(_)) | (ScreenAction::Forward, None)
                if self.reply.is_empty() =>
            {
                Ok(())
            }
            (ScreenAction::Answer, Some(_)) if !self.reply.trim().is_empty() => Ok(()),
            _ => Err(invalid("log action, rule and reply do not agree")),
        }
    }
}

pub fn check_rule_id(id: &str) -> Result<(), ProtoError> {
    if id.len() <= 32
        && id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-_".contains(&c))
    {
        Ok(())
    } else {
        Err(invalid("id must match ^[a-z][a-z0-9_-]{0,31}$"))
    }
}

fn unique_nonempty<T: Eq + std::hash::Hash>(values: &[T], field: &str) -> Result<(), ProtoError> {
    if values.is_empty() || values.iter().collect::<HashSet<_>>().len() != values.len() {
        return Err(invalid(format!(
            "{field} must be nonempty and contain no duplicates"
        )));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> ProtoError {
    ProtoError::Screen(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> ScreenRule {
        serde_json::from_str(
            r#"{
            "id":"progress", "kinds":["note"], "roles":["implementor"],
            "when":"The note reports routine progress.", "action":"close",
            "cites":"hgl-ai#185", "expires":"20300101T000000Z"
        }"#,
        )
        .unwrap()
    }

    #[test]
    fn rule_checks_and_expiry() {
        let base = rule();
        base.validate().unwrap();
        assert_eq!(base.mode, ScreenMode::Shadow);
        assert!(base.is_active(parse_stamp("20291231T235959Z").unwrap()));
        assert!(!base.is_active(parse_stamp("20300101T000000Z").unwrap()));
        for invalid in [
            ScreenRule {
                id: "rule.>".into(),
                ..rule()
            },
            ScreenRule {
                kinds: vec![],
                ..rule()
            },
            ScreenRule {
                roles: vec![Role::Reviewer; 2],
                ..rule()
            },
            ScreenRule {
                when: "Two\nlines".into(),
                ..rule()
            },
            ScreenRule {
                cites: " ".into(),
                ..rule()
            },
            ScreenRule {
                action: ScreenAction::Forward,
                ..rule()
            },
            ScreenRule {
                action: ScreenAction::Answer,
                ..rule()
            },
            ScreenRule {
                reply: "reply on close".into(),
                ..rule()
            },
            ScreenRule {
                expires: "20300230T000000Z".into(),
                ..rule()
            },
        ] {
            assert!(invalid.validate().is_err(), "{invalid:?}");
        }
        let retired = ScreenRule {
            retired_at: Some("20291231T235959Z".into()),
            ..base
        };
        retired.validate().unwrap();
        assert!(!retired.is_active(parse_stamp("20291231T235959Z").unwrap()));
    }

    #[test]
    fn log_record_checks() {
        let rec = ScreenLog {
            seq: 3,
            rule: Some("known-block".into()),
            action: ScreenAction::Answer,
            reply: "A known issue.".into(),
            mode: ScreenMode::Shadow,
            model: "test-model".into(),
        };
        rec.validate().unwrap();
        let encoded = serde_json::to_string(&rec).unwrap();
        assert_eq!(serde_json::from_str::<ScreenLog>(&encoded).unwrap(), rec);
        for invalid in [
            ScreenLog {
                seq: 0,
                ..rec.clone()
            },
            ScreenLog {
                rule: None,
                ..rec.clone()
            },
            ScreenLog {
                reply: String::new(),
                ..rec.clone()
            },
            ScreenLog {
                model: " ".into(),
                ..rec.clone()
            },
            ScreenLog {
                action: ScreenAction::Forward,
                ..rec
            },
        ] {
            assert!(invalid.validate().is_err(), "{invalid:?}");
        }
    }
}
