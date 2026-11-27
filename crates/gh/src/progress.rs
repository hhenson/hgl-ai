//! What one landing has done, on the hub: `HGL_STATE/landing.<repo>.<pr>` (docs/protocol.md
//! §8). Written under the landing lock: the intent right before the merge call, the merge
//! commit once GitHub reports the pull request merged, then each step after it. A landing
//! that is tried again and finds the pull request merged resumes from here, so a failure
//! after the merge never leaves `landed.log`, `main`, the `landed` notes, the comment or the
//! pins undone while the command reports success. The record is deleted once every step
//! is done.

use hgl_ai_bus::Bus;
use serde::{Deserialize, Serialize};

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
    /// The merge commit, once the pull request is merged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<String>,
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
    pub fn new(head: &str, merge: Option<String>) -> Progress {
        Progress {
            head: head.to_string(),
            merge,
            done: Vec::new(),
            notes_at: None,
            updated_at: hgl_ai_proto::utc_now_stamp(),
        }
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
    let number = format!("#{pr}");
    log.lines().any(|l| {
        let w: Vec<&str> = l.split_whitespace().collect();
        w.len() >= 4 && w[1] == repo && w[2] == number && merge.is_none_or(|m| w[3] == m)
    })
}

/// [`log_has`] over the hub's `landed.log`.
pub async fn landed(bus: &Bus, repo: &str, pr: u64, merge: Option<&str>) -> Result<bool> {
    Ok(bus
        .state_get(state::KEY_LANDED)
        .await?
        .is_some_and(|e| log_has(&e.value, repo, pr, merge)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_the_log() {
        let mut p = Progress::new("h", None);
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
    }
}
