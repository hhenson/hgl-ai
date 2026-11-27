//! Leftover trees (PROTO-15): older than `stale_after`, or whose issue or pull request is closed.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};
use hgl_ai_proto::TreeRecord;

/// Answers whether an issue (or, for a reviewer's tree, a pull request) is closed.
/// `None` when it cannot tell; such a tree is judged by age only.
pub trait IsClosed {
    fn is_closed(&self, repo: &str, number: u64, pull_request: bool) -> Option<bool>;
}

impl<F: Fn(&str, u64, bool) -> Option<bool>> IsClosed for F {
    fn is_closed(&self, repo: &str, number: u64, pull_request: bool) -> Option<bool> {
        self(repo, number, pull_request)
    }
}

/// Never asks; used while the GitHub allowance is on hold (ROLE-17).
pub struct Unknown;

impl IsClosed for Unknown {
    fn is_closed(&self, _: &str, _: u64, _: bool) -> Option<bool> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stale {
    pub record: TreeRecord,
    pub age: Option<Duration>,
    pub old: bool,
    pub closed: bool,
}

impl Stale {
    pub fn reason(&self) -> String {
        match (self.old, self.closed) {
            (true, true) => "old, closed".into(),
            (true, false) => "old".into(),
            _ => "closed".into(),
        }
    }
}

/// Parse the protocol's `YYYYMMDDTHHMMSSZ` stamp.
pub fn parse_stamp(s: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ")
        .ok()
        .map(|t| t.and_utc())
}

pub fn age(rec: &TreeRecord, now: DateTime<Utc>) -> Option<Duration> {
    parse_stamp(&rec.created_at).and_then(|t| (now - t).to_std().ok())
}

/// The stale subset of `records`; each issue or pull request is asked about once.
pub fn stale(
    records: &[TreeRecord],
    now: DateTime<Utc>,
    after: Duration,
    closed: &dyn IsClosed,
) -> Vec<Stale> {
    let mut asked: BTreeMap<(String, u64, bool), bool> = BTreeMap::new();
    let mut out = Vec::new();
    for r in records {
        let age = age(r, now);
        let old = age.is_some_and(|a| a > after);
        let key = (r.task.repo.clone(), r.task.issue, r.review);
        let is_closed = *asked.entry(key).or_insert_with(|| {
            closed
                .is_closed(&r.task.repo, r.task.issue, r.review)
                .unwrap_or(false)
        });
        if old || is_closed {
            out.push(Stale {
                record: r.clone(),
                age,
                old,
                closed: is_closed,
            });
        }
    }
    out
}

/// `3d4h`, `5h`, `12m`, `40s`.
pub fn human_age(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d{}h", s / 86400, (s % 86400) / 3600),
    }
}

/// `1.2G`, `340M`, `12K`, `512B`.
pub fn human_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let f = b as f64;
    if f >= K * K * K {
        format!("{:.1}G", f / (K * K * K))
    } else if f >= K * K {
        format!("{:.0}M", f / (K * K))
    } else if f >= K {
        format!("{:.0}K", f / K)
    } else {
        format!("{b}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hgl_ai_proto::{Seat, TaskKey};
    use std::cell::Cell;

    fn rec(issue: u64, created: &str, review: bool) -> TreeRecord {
        TreeRecord {
            host: "h".into(),
            seat: Seat::new("w1").unwrap(),
            task: TaskKey {
                repo: "r".into(),
                issue,
            },
            review,
            path: String::new(),
            branch: None,
            created_at: created.into(),
            size_bytes: 0,
        }
    }

    #[test]
    fn old_or_closed() {
        let now = parse_stamp("20261127T120000Z").unwrap();
        let recs = [
            rec(1, "20261120T120000Z", false),
            rec(2, "20261127T110000Z", false),
            rec(3, "20261127T110000Z", true),
            rec(3, "x", true),
        ];
        let calls = Cell::new(0);
        let closed = |_: &str, n: u64, pr: bool| {
            calls.set(calls.get() + 1);
            Some(n == 3 && pr)
        };
        let s = stale(&recs, now, Duration::from_secs(3 * 86400), &closed);
        let got: Vec<(u64, bool, bool)> = s
            .iter()
            .map(|s| (s.record.task.issue, s.old, s.closed))
            .collect();
        assert_eq!(
            got,
            vec![(1, true, false), (3, false, true), (3, false, true)]
        );
        assert_eq!(calls.get(), 3, "one question per issue or pull request");
        assert_eq!(s[0].reason(), "old");
        assert!(stale(&recs[1..2], now, Duration::from_secs(86400), &Unknown).is_empty());
    }

    #[test]
    fn humans() {
        assert_eq!(human_age(Duration::from_secs(3 * 86400 + 7200)), "3d2h");
        assert_eq!(human_age(Duration::from_secs(59)), "59s");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024 / 2), "1.5G");
        assert_eq!(human_bytes(100), "100B");
    }
}
