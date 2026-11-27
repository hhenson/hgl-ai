//! `ctest`: one result per `N/M Test #k: name .... Passed|***Failed|...` line, the failing
//! test's message from the output `--output-on-failure` prints after it, and CTest's own
//! errors.

use std::sync::LazyLock;

use regex::Regex;

use super::{Found, Outcome, Parsed, Severity, TestCase, clip, lines};

static RESULT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*\d+/\d+ Test\s+#\d+: (?P<name>\S+) \.*\s*(?P<status>.*?)\s+[\d.]+ sec\s*$")
        .unwrap()
});
static START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*Start\s+\d+: ").unwrap());
static TOTAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+% tests passed, \d+ tests? failed out of \d+").unwrap());
static TELLING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(error|fail|assert|expected|abort|exception|segmentation|sanitizer)").unwrap()
});

pub fn parse(log: &[u8]) -> Parsed {
    let all = lines(log);
    let mut p = Parsed::default();
    let mut i = 0;
    while i < all.len() {
        let l = &all[i];
        let t = l.text.as_ref();
        if let Some(c) = RESULT.captures(t) {
            let status = c["status"].trim_start_matches('*').trim().to_string();
            let outcome = if status == "Passed" {
                Outcome::Pass
            } else if status.contains("Skipped") || status.contains("Disabled") {
                Outcome::Skip
            } else {
                Outcome::Fail
            };
            let mut tc = TestCase {
                name: c["name"].to_string(),
                outcome,
                message: String::new(),
                start: l.start,
                end: l.end,
                source: None,
                group: None,
            };
            if outcome == Outcome::Fail {
                // Its output, up to the next test, start line or total.
                let mut j = i + 1;
                let mut telling = None;
                while j < all.len()
                    && !RESULT.is_match(&all[j].text)
                    && !START.is_match(&all[j].text)
                    && !TOTAL.is_match(&all[j].text)
                {
                    let s = all[j].text.trim();
                    if telling.is_none() && TELLING.is_match(s) {
                        telling = Some(s.to_string());
                    }
                    tc.end = all[j].end;
                    j += 1;
                }
                tc.message = clip(&match telling {
                    Some(s) => format!("{status}: {s}"),
                    None => status.clone(),
                });
            }
            p.tests.push(tc);
        } else if let Some(m) = t.strip_prefix("CMake Error") {
            p.push(Found::new(
                Severity::Error,
                format!("CMake Error{m}"),
                l.start,
                l.end,
            ));
        } else if t.starts_with("Unable to find executable:")
            || t.starts_with("Could not find executable")
        {
            p.push(Found::new(Severity::Error, t, l.start, l.end));
        } else if t.starts_with("No tests were found") {
            let mut f = Found::new(Severity::Error, t, l.start, l.end);
            f.code = Some("ctest".into());
            p.push(f);
        }
        i += 1;
    }
    p
}
