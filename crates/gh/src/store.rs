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

/// Set `HGL_STATE/hold` to `value` (a time), compare-and-set, unless a hold already
/// there holds at least as long: one with no time, or a later one (another writer's, the
/// controller's or the owner's, is never shortened). Returns the value in effect.
pub async fn raise_hold(bus: &Bus, value: &str) -> Result<String> {
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        let cur = bus.state_get(crate::state::KEY_HOLD).await?;
        let rev = cur.as_ref().map(|e| e.revision).unwrap_or(0);
        if let Some(e) = &cur
            && holds_as_long(&e.value, value)
        {
            return Ok(e.value.trim().to_string());
        }
        match bus.state_update(crate::state::KEY_HOLD, value, rev).await {
            Err(e @ BusError::Conflict { .. }) => {
                last = Some(e);
                tokio::time::sleep(backoff(attempt)).await;
            }
            other => {
                other?;
                return Ok(value.to_string());
            }
        }
    }
    Err(last.expect("at least one attempt").into())
}

/// Whether the hold `current` holds at least until `value` does.
fn holds_as_long(current: &str, value: &str) -> bool {
    use crate::table::{hold_active, parse_time};
    let now = chrono::Utc::now();
    if !hold_active(current, now) {
        return false;
    }
    let first = |v: &str| v.split_whitespace().next().and_then(parse_time);
    match (first(current), first(value)) {
        (None, _) => true,
        (Some(c), Some(v)) => c >= v,
        (Some(_), None) => false,
    }
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

    /// Review (r6): the coordinator's hold never shortens another writer's.
    #[test]
    fn a_hold_is_only_raised() {
        let later = "2999-01-01T00:00:00Z";
        let sooner = "2998-01-01T00:00:00Z";
        assert!(holds_as_long(later, sooner));
        assert!(holds_as_long(later, later));
        assert!(!holds_as_long(sooner, later));
        assert!(holds_as_long("owner freeze", later), "no time: holds");
        assert!(holds_as_long(&format!("{later} release"), sooner));
        assert!(!holds_as_long("2000-01-01T00:00:00Z", later), "passed");
        assert!(!holds_as_long("", later));
    }
}
