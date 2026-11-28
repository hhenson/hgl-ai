//! The hub's pre-check rule list and its validation.
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleList {
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub status: Status,
    pub kind: Kind,
    pub catches: String,
    pub check: String,
    pub applies: Vec<String>,
    pub ask: String,
    pub example: String,
    pub sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repos: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Trial,
    Active,
    Retired,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Mechanical,
    Judgement,
}

impl RuleList {
    pub fn validate(&self) -> Result<(), String> {
        if self.rules.len() > 30 {
            return Err("at most 30 rules are allowed".into());
        }
        let mut ids = HashSet::new();
        for r in &self.rules {
            let id =
                r.id.strip_prefix("PC-")
                    .filter(|n| !n.starts_with('0') && n.bytes().all(|c| c.is_ascii_digit()))
                    .filter(|n| !n.is_empty());
            if id.is_none() || !ids.insert(&r.id) {
                return Err(format!("invalid or duplicate rule id {:?}", r.id));
            }
            if [&r.catches, &r.check, &r.ask, &r.example]
                .iter()
                .any(|s| s.trim().is_empty())
                || [&r.applies, &r.sources]
                    .iter()
                    .any(|v| v.is_empty() || v.iter().any(|s| s.trim().is_empty()))
                || r.repos
                    .as_ref()
                    .is_some_and(|v| v.is_empty() || v.iter().any(|s| s.trim().is_empty()))
            {
                return Err(format!("rule {} has empty fields", r.id));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredRules {
    pub reason: String,
    pub rules: Vec<Rule>,
}

/// Counters for one rule, stored independently of the list.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleCounters {
    pub fires: u64,
    pub catches: u64,
    pub false_positives: u64,
    pub misses: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rule() -> Rule {
        serde_json::from_str(r#"{"id":"PC-1","status":"trial","kind":"judgement","catches":"x","check":"x","applies":["*"],"ask":"x","example":"x","sources":["example"]}"#).unwrap()
    }
    #[test]
    fn validates_rule_count_ids_and_fields() {
        let mut list = RuleList {
            rules: vec![rule()],
        };
        assert!(list.validate().is_ok());
        list.rules.push(rule());
        assert!(list.validate().is_err());
        list.rules = (1..=31)
            .map(|i| {
                let mut r = rule();
                r.id = format!("PC-{i}");
                r
            })
            .collect();
        assert!(list.validate().is_err());
        list.rules = vec![rule()];
        list.rules[0].check = " ".into();
        assert!(list.validate().is_err());
        assert!(serde_json::from_str::<Rule>(r#"{"id":"PC-1","status":"invented"}"#).is_err());
    }
}
