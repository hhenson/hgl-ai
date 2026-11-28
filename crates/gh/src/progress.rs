//! What one landing has done, on the hub: `HGL_STATE/landing.<repo>.<pr>` (docs/protocol.md
//! §8). Written under the landing lock: the intent right before the merge call, the merge
//! commit once GitHub reports the pull request merged, then each step after it. A landing
//! that is tried again and finds the pull request merged resumes from here, so a failure
//! after the merge never leaves `landed.log`, `main`, the `landed` notes, the comment or the
//! pins undone while the command reports success. The record is deleted once every step
//! is done.

use hgl_ai_bus::Bus;
use serde::{Deserialize, Serialize};

use crate::gate::LandedMerge;
use crate::{Result, state, store};

/// The steps after a merge, in order. `pins` only for `gh land` of a repository the
/// wrapper's pins follow; it runs after the landing lock is released.
pub const LANDED_LOG: &str = "landed.log";
pub const MAIN: &str = "main";
pub const NOTES: &str = "notes";
pub const COMMENT: &str = "comment";
pub const PINS: &str = "pins";

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Progress {
    /// The head the merge is (or was) for.
    pub head: String,
    /// The merge commit, once GitHub shows the pull request merged as intended: set only
    /// from a [`LandedMerge`] ([`Progress::merged`], [`Progress::set_merged`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    merge: Option<String>,
    /// Steps done, from the list above.
    #[serde(default)]
    pub done: Vec<String>,
    /// `created_at` of the `landed` notes, so a resend carries the same message ids and the
    /// hub drops the copies (PROTO-9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_at: Option<String>,
    pub updated_at: String,
}

impl Progress {
    /// The intent of a landing about to merge at `head`: no merge yet.
    pub fn intent(head: &str) -> Progress {
        Progress {
            head: head.to_string(),
            merge: None,
            done: Vec::new(),
            notes_at: None,
            updated_at: hgl_ai_proto::utc_now_stamp(),
        }
    }

    /// The record of a merge GitHub showed as intended.
    pub fn merged(m: &LandedMerge) -> Progress {
        let mut p = Progress::intent(m.head());
        p.set_merged(m);
        p
    }

    /// The merge commit recorded, if any: evidence only with a pull request read that
    /// confirms it ([`crate::gate::confirm_merge`]).
    pub fn merge(&self) -> Option<&str> {
        self.merge.as_deref()
    }

    /// Record `m` as this landing's merge: its head and merge commit.
    pub fn set_merged(&mut self, m: &LandedMerge) {
        self.head = m.head().to_string();
        self.merge = Some(m.merge().to_string());
        self.updated_at = hgl_ai_proto::utc_now_stamp();
    }

    /// A record as a test writes it, its merge given as is.
    #[cfg(test)]
    pub(crate) fn recorded(head: &str, merge: Option<&str>) -> Progress {
        let mut p = Progress::intent(head);
        p.merge = merge.map(String::from);
        p
    }

    pub fn has(&self, step: &str) -> bool {
        self.done.iter().any(|d| d == step)
    }

    pub fn mark(&mut self, step: &str) {
        if !self.has(step) {
            self.done.push(step.to_string());
        }
        self.updated_at = hgl_ai_proto::utc_now_stamp();
    }
}

/// `landing.<repo>.<pr>`
pub fn key(repo: &str, pr: u64) -> String {
    format!("landing.{repo}.{pr}")
}

pub async fn read(bus: &Bus, repo: &str, pr: u64) -> Result<Option<Progress>> {
    let Some(e) = bus.state_get(&key(repo, pr)).await? else {
        return Ok(None);
    };
    match serde_json::from_str(&e.value) {
        Ok(p) => Ok(Some(p)),
        Err(err) => {
            // Unreadable: treat as a landing whose steps are not known done.
            tracing::warn!(
                "{}: unreadable landing record ({err}); resuming every step",
                key(repo, pr)
            );
            Ok(Some(Progress::default()))
        }
    }
}

pub async fn write(bus: &Bus, repo: &str, pr: u64, p: &Progress) -> Result<()> {
    let text = serde_json::to_string(p).expect("a landing record encodes");
    store::put_latest(bus, &key(repo, pr), &text).await?;
    Ok(())
}

/// Delete the record: the landing is complete (or its merge was refused).
pub async fn delete(bus: &Bus, repo: &str, pr: u64) -> Result<()> {
    bus.state_del(&key(repo, pr)).await?;
    Ok(())
}

/// Whether `landed.log` holds a line for `repo #pr` (at `merge`, when given).
pub fn log_has(log: &str, repo: &str, pr: u64, merge: Option<&str>) -> bool {
    log_merges(log, repo, pr).any(|m| merge.is_none_or(|want| m == want))
}

/// The merge commit of the newest `landed.log` line for `repo #pr`.
pub fn log_merge(log: &str, repo: &str, pr: u64) -> Option<String> {
    log_merges(log, repo, pr).last().map(String::from)
}

/// The merge commits of the `landed.log` lines for `repo #pr`, oldest first.
fn log_merges<'a>(log: &'a str, repo: &'a str, pr: u64) -> impl Iterator<Item = &'a str> {
    let number = format!("#{pr}");
    log.lines().filter_map(move |l| {
        let w: Vec<&str> = l.split_whitespace().collect();
        (w.len() >= 4 && w[1] == repo && w[2] == number).then(|| w[3])
    })
}

/// [`log_has`] over the hub's `landed.log`.
pub async fn landed(bus: &Bus, repo: &str, pr: u64, merge: Option<&str>) -> Result<bool> {
    Ok(bus
        .state_get(state::KEY_LANDED)
        .await?
        .is_some_and(|e| log_has(&e.value, repo, pr, merge)))
}

/// [`log_merge`] over the hub's `landed.log`.
pub async fn landed_merge(bus: &Bus, repo: &str, pr: u64) -> Result<Option<String>> {
    Ok(bus
        .state_get(state::KEY_LANDED)
        .await?
        .and_then(|e| log_merge(&e.value, repo, pr)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_the_log() {
        let mut p = Progress::intent("h");
        assert!(!p.has(MAIN));
        p.mark(MAIN);
        p.mark(MAIN);
        assert_eq!(p.done, [MAIN]);
        let text = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Progress>(&text).unwrap(), p);
        assert!(hgl_ai_proto::check_state_key(&key("hgl-compiler", 1037)).is_ok());
        let log = "20260105T120000Z alpha #7 m7\n20260105T120100Z alpha #70 m70\n";
        assert!(log_has(log, "alpha", 7, None));
        assert!(log_has(log, "alpha", 7, Some("m7")));
        assert!(!log_has(log, "alpha", 7, Some("m8")));
        assert!(!log_has(log, "alpha", 8, None));
        assert!(!log_has(log, "beta", 7, None));
        assert_eq!(log_merge(log, "alpha", 7).as_deref(), Some("m7"));
        assert_eq!(log_merge(log, "alpha", 70).as_deref(), Some("m70"));
        assert_eq!(log_merge(log, "alpha", 8), None);
    }
}
