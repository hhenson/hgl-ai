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

/// `text` with `line` appended, then cut to its newest `keep` lines and to at most
/// `max_bytes` (whole lines, newest kept; a line longer than that alone is cut to it).
pub fn appended(text: &str, line: &str, keep: usize, max_bytes: usize) -> String {
    let line = line.trim_end_matches('\n');
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.push(line);
    let mut out: Vec<&str> = Vec::new();
    let mut bytes = 0;
    for l in lines.iter().rev().take(keep) {
        if bytes + l.len() + 1 > max_bytes {
            if out.is_empty() {
                // One line over the bound alone: its first bytes, on a char boundary.
                let mut end = max_bytes.saturating_sub(1).min(l.len());
                while !l.is_char_boundary(end) {
                    end -= 1;
                }
                out.push(&l[..end]);
            }
            break;
        }
        bytes += l.len() + 1;
        out.push(l);
    }
    out.reverse();
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Append a line, keeping the key to its newest `keep` lines and at most `max_bytes`, in
/// one compare-and-set write: the value never goes over the bound, not even between an
/// append and a trim. GitHub holds the full record.
pub async fn append_bounded(
    bus: &Bus,
    key: &str,
    line: &str,
    keep: usize,
    max_bytes: usize,
) -> Result<()> {
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        let (text, rev) = match bus.state_get(key).await? {
            Some(e) => (e.value, e.revision),
            None => (String::new(), 0),
        };
        match bus
            .state_update(key, &appended(&text, line, keep, max_bytes), rev)
            .await
        {
            Err(e @ BusError::Conflict { .. }) => {
                last = Some(e);
                tokio::time::sleep(backoff(attempt)).await;
            }
            other => {
                other?;
                return Ok(());
            }
        }
    }
    Err(last.expect("at least one attempt").into())
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

    #[test]
    fn appending_within_lines_and_bytes() {
        assert_eq!(appended("", "a", 3, 100), "a\n");
        assert_eq!(appended("a\nb\nc\n", "d", 3, 100), "b\nc\nd\n");
        // Bytes bind before lines: "ccc\nddd\n" is 8.
        assert_eq!(appended("aaa\nbbb\nccc\n", "ddd", 10, 8), "ccc\nddd\n");
        assert_eq!(appended("aaa\nbbb\nccc\n", "ddd", 10, 9), "ccc\nddd\n");
        assert_eq!(
            appended("a\n", "ééé", 10, 6),
            "éé\n",
            "cut on a char boundary"
        );
        // 2,000 lines of 200 four-byte characters stay under the bus's limit.
        let long = "\u{1F600}".repeat(200);
        let mut t = String::new();
        for _ in 0..2100 {
            t = appended(&t, &long, 2000, crate::state::LANDED_MAX_BYTES);
        }
        assert!(t.len() <= crate::state::LANDED_MAX_BYTES);
        assert!(t.len() < hgl_ai_bus::MAX_VALUE_BYTES);
    }
}
