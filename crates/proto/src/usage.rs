//! Subscription usage records (PROTO-23, PROTO-24): one record per login, keyed by the
//! runtime and a fingerprint of the runtime's own account id. No id, email or token is
//! ever part of a record.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ProtoError, Runtime};

/// The latest record per login, `HGL_USAGE/<runtime>.<account>`; labels at `label.<account>`.
pub const KV_USAGE: &str = "HGL_USAGE";
/// Every record that won its key, on `hgl.usage.<runtime>.<account>` (the burn-down history).
pub const STREAM_USAGE: &str = "HGL_USAGE_LOG";
pub const USAGE_PREFIX: &str = "hgl.usage";
/// Hex digits of SHA-256 kept in an account fingerprint.
pub const ACCOUNT_HEX: usize = 12;
/// Lines of a source's text kept when they do not parse, and their length.
pub const MAX_UNPARSED: usize = 20;
pub const MAX_UNPARSED_CHARS: usize = 240;

/// The fingerprint of a runtime's account id: the first [`ACCOUNT_HEX`] hex digits of its
/// SHA-256.
pub fn account_fingerprint(id: &str) -> String {
    let mut h = hex::encode(Sha256::digest(id.as_bytes()));
    h.truncate(ACCOUNT_HEX);
    h
}

pub fn check_account(a: &str) -> Result<(), ProtoError> {
    if a.len() == ACCOUNT_HEX
        && a.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(ProtoError::Unknown {
            what: "account fingerprint (12 lowercase hex digits)",
            value: a.to_string(),
        })
    }
}

/// Owner-given labels: `[A-Za-z0-9][A-Za-z0-9_.-]{0,31}`.
pub fn check_label(l: &str) -> Result<(), ProtoError> {
    let ok = !l.is_empty()
        && l.len() <= 32
        && l.starts_with(|c: char| c.is_ascii_alphanumeric())
        && l.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(ProtoError::Unknown {
            what: "usage label ([A-Za-z0-9][A-Za-z0-9_.-]{0,31})",
            value: l.to_string(),
        })
    }
}

pub fn usage_key(runtime: Runtime, account: &str) -> String {
    format!("{runtime}.{account}")
}

pub fn usage_label_key(account: &str) -> String {
    format!("label.{account}")
}

/// One limit of a login.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageBucket {
    /// `all`, a model (`fable`), `gemini`, `claude+gpt`.
    pub scope: String,
    /// `5h`, `week`, or `<minutes>m` for another window.
    pub window: String,
    /// Percent used, 0 to 100; remaining is 100 − used.
    pub used_pct: f64,
    /// When the window resets; absent when the source gives none (a window not started).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
}

/// `HGL_USAGE/<runtime>.<account>` (PROTO-23).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub runtime: Runtime,
    /// [`account_fingerprint`] of the runtime's account id.
    pub account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The reporting host's label.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// When the source's numbers were true: the probe for claude and agy, the last Codex
    /// turn's event for codex. The hub keeps the newest (PROTO-24).
    pub observed_at: String,
    /// When the collector ran.
    pub collected_at: String,
    /// `collected_at − observed_at`, seconds.
    pub source_age_s: u64,
    pub buckets: Vec<UsageBucket>,
    /// The limit the source says was reached, if any (codex `rate_limit_reached_type`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_reached: Option<String>,
    /// Source lines that did not parse, kept as they were (at most [`MAX_UNPARSED`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unparsed: Vec<String>,
}

impl UsageRecord {
    pub fn key(&self) -> String {
        usage_key(self.runtime, &self.account)
    }

    pub fn subject(&self) -> String {
        format!("{USAGE_PREFIX}.{}", self.key())
    }

    pub fn validate(&self) -> Result<(), ProtoError> {
        check_account(&self.account)?;
        if let Some(l) = &self.label {
            check_label(l)?;
        }
        crate::check_stamp(&self.observed_at)?;
        crate::check_stamp(&self.collected_at)?;
        for b in &self.buckets {
            if let Some(r) = &b.resets_at {
                crate::check_stamp(r)?;
            }
        }
        Ok(())
    }
}

/// `HGL_USAGE/label.<account>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLabel {
    pub label: String,
    pub set_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_and_keys() {
        let fp = account_fingerprint("org-1/someone@example.com");
        assert_eq!(fp.len(), ACCOUNT_HEX);
        assert!(check_account(&fp).is_ok());
        assert_eq!(fp, account_fingerprint("org-1/someone@example.com"));
        assert_ne!(fp, account_fingerprint("org-2/someone@example.com"));
        assert_eq!(account_fingerprint(""), "e3b0c44298fc");
        for bad in [
            "",
            "E3B0C44298FC",
            "e3b0c44298f",
            "e3b0c44298fcx",
            "e3b0c4429.fc",
        ] {
            assert!(check_account(bad).is_err(), "{bad}");
        }
        assert_eq!(usage_key(Runtime::Codex, &fp), format!("codex.{fp}"));
        assert_eq!(usage_label_key(&fp), format!("label.{fp}"));
    }

    #[test]
    fn labels() {
        for ok in ["max-1", "a", "Team_B.2"] {
            assert!(check_label(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-a", "a b", "a*", &"x".repeat(33)] {
            assert!(check_label(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn record_round_trip() {
        let r = UsageRecord {
            runtime: Runtime::Agy,
            account: account_fingerprint("x"),
            label: None,
            host: "build-1".into(),
            plan: None,
            observed_at: "20261127T175500Z".into(),
            collected_at: "20261127T175500Z".into(),
            source_age_s: 0,
            buckets: vec![UsageBucket {
                scope: "gemini".into(),
                window: "week".into(),
                used_pct: 19.0,
                resets_at: Some("20261203T135517Z".into()),
            }],
            limit_reached: None,
            unparsed: vec![],
        };
        r.validate().unwrap();
        let j = serde_json::to_string(&r).unwrap();
        assert!(!j.contains("label") && !j.contains("unparsed"), "{j}");
        assert_eq!(serde_json::from_str::<UsageRecord>(&j).unwrap(), r);
        assert_eq!(r.subject(), format!("hgl.usage.agy.{}", r.account));
        let mut bad = r.clone();
        bad.observed_at = "2026-11-27".into();
        assert!(bad.validate().is_err());
    }
}
