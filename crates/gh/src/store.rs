//! Writes of shared `HGL_STATE` keys, all compare-and-set (never a blind put).

use std::time::Duration;

use hgl_ai_bus::{Bus, BusError};

use crate::Result;

const ATTEMPTS: u32 = 8;

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(50 << attempt.min(5))
}

/// Make `key` hold `value`, whatever revision it is at when written. For values that are a
/// fresh reading (`prs`, `main`, `hold`): the newest reading wins, but no write is blind.
pub async fn put_latest(bus: &Bus, key: &str, value: &str) -> Result<u64> {
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        let rev = bus.state_get(key).await?.map(|e| e.revision).unwrap_or(0);
        match bus.state_update(key, value, rev).await {
            Err(e @ BusError::Conflict { .. }) => {
                last = Some(e);
                tokio::time::sleep(backoff(attempt)).await;
            }
            other => return Ok(other?),
        }
    }
    Err(last.expect("at least one attempt").into())
}

/// Delete `key` only if it still holds `value` (a `hold` another writer renewed stays).
/// Returns whether it was deleted.
pub async fn clear_if(bus: &Bus, key: &str, value: &str) -> Result<bool> {
    let Some(e) = bus.state_get(key).await? else {
        return Ok(false);
    };
    if e.value != value {
        return Ok(false);
    }
    match bus.state_update(key, "", e.revision).await {
        Ok(_) => Ok(true),
        Err(BusError::Conflict { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// The newest `keep` lines of a text.
pub fn newest_lines(text: &str, keep: usize) -> Option<String> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() <= keep {
        return None;
    }
    let mut s = lines[lines.len() - keep..].join("\n");
    s.push('\n');
    Some(s)
}

/// Append a line (the bus's own compare-and-set loop), then trim the key to its newest
/// `keep` lines. GitHub holds the full record.
pub async fn append_bounded(bus: &Bus, key: &str, line: &str, keep: usize) -> Result<()> {
    bus.append_line(key, line).await?;
    for attempt in 0..ATTEMPTS {
        let Some(e) = bus.state_get(key).await? else {
            return Ok(());
        };
        let Some(trimmed) = newest_lines(&e.value, keep) else {
            return Ok(());
        };
        match bus.state_update(key, &trimmed, e.revision).await {
            Err(BusError::Conflict { .. }) => tokio::time::sleep(backoff(attempt)).await,
            other => {
                other?;
                return Ok(());
            }
        }
    }
    // Another writer keeps winning; it trims too.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trimming() {
        assert_eq!(newest_lines("a\nb\n", 2), None);
        assert_eq!(newest_lines("a\nb\nc\n", 2).as_deref(), Some("b\nc\n"));
        assert_eq!(newest_lines("a\n\nb\nc", 1).as_deref(), Some("c\n"));
    }
}
