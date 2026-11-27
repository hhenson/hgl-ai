//! `hgl-test`: `hgl test`'s case lines (`passed|FAILED|not run  rejection|executable  id`)
//! with their indented details, its totals when no case line is printed, and the
//! compiler's diagnostics (`error[code]: message` then `┌─ file:line:col` or
//! `--> file:line:col`; `file:line:col: message`; `hgl: message`).

use std::sync::LazyLock;

use regex::Regex;

use super::{Found, Outcome, Parsed, Severity, TestCase, clip, lines};

static CASE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?P<label>passed|FAILED|not run)\s+(?P<kind>rejection|executable)\s+(?P<id>\S.*)$",
    )
    .unwrap()
});
static REJECTION_TOTAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^rejection cases: (?P<p>\d+) passed, (?P<f>\d+) failed$").unwrap()
});
static EXEC_TOTAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^executable cases: (?P<p>\d+) passed, (?P<f>\d+) failed, (?P<s>\d+) not run$")
        .unwrap()
});
static DIAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<sev>error|warning)(?:\[(?P<code>[^\]]+)\])?: (?P<msg>.+)$").unwrap()
});
static AT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:┌─|-->)\s*(?P<file>.+?):(?P<line>\d+):(?P<col>\d+)\s*$").unwrap()
});
static PROBLEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<file>[^\s:][^:]*\.hgl):(?P<line>\d+):(?P<col>\d+): (?P<msg>.+)$").unwrap()
});

pub fn parse(log: &[u8]) -> Parsed {
    let all = lines(log);
    let mut p = Parsed::default();
    let mut totals: Vec<(u64, u64, u64)> = Vec::new();
    let mut i = 0;
    while i < all.len() {
        let l = &all[i];
        let t = l.text.as_ref();
        if let Some(c) = CASE.captures(t) {
            let outcome = match &c["label"] {
                "passed" => Outcome::Pass,
                "FAILED" => Outcome::Fail,
                _ => Outcome::Skip,
            };
            let mut tc = TestCase {
                name: format!("{} {}", &c["kind"], c["id"].trim()),
                outcome,
                message: String::new(),
                start: l.start,
                end: l.end,
                source: None,
            };
            let mut details = Vec::new();
            let mut j = i + 1;
            while j < all.len() && all[j].text.starts_with("    ") {
                details.push(all[j].text.trim().to_string());
                tc.end = all[j].end;
                j += 1;
            }
            tc.message = clip(&details.join("\n"));
            p.tests.push(tc);
            i = j;
            continue;
        }
        if let Some(c) = REJECTION_TOTAL.captures(t) {
            totals.push((c["p"].parse().unwrap_or(0), c["f"].parse().unwrap_or(0), 0));
        } else if let Some(c) = EXEC_TOTAL.captures(t) {
            totals.push((
                c["p"].parse().unwrap_or(0),
                c["f"].parse().unwrap_or(0),
                c["s"].parse().unwrap_or(0),
            ));
        } else if let Some(c) = DIAG.captures(t) {
            let sev = if &c["sev"] == "error" {
                Severity::Error
            } else {
                Severity::Warning
            };
            let mut f = Found::new(sev, &c["msg"], l.start, l.end);
            f.code = c.name("code").map(|m| m.as_str().to_string());
            if let Some((a, n)) = all
                .iter()
                .skip(i + 1)
                .take(3)
                .find_map(|n| AT.captures(&n.text).map(|a| (a, n)))
            {
                f.file = Some(a["file"].to_string());
                f.line = a["line"].parse().ok();
                f.col = a["col"].parse().ok();
                f.end = n.end;
            }
            p.push(f);
        } else if let Some(c) = PROBLEM.captures(t) {
            let mut f = Found::new(Severity::Error, &c["msg"], l.start, l.end);
            f.file = Some(c["file"].to_string());
            f.line = c["line"].parse().ok();
            f.col = c["col"].parse().ok();
            p.push(f);
        } else if let Some(m) = t.strip_prefix("hgl: ") {
            p.push(Found::new(Severity::Error, m, l.start, l.end));
        }
        i += 1;
    }
    if p.tests.is_empty() {
        // Totals alone (a cached run): counts without names.
        for (passed, failed, skipped) in totals {
            p.unnamed.0 += passed;
            p.unnamed.1 += failed;
            p.unnamed.2 += skipped;
        }
    }
    p
}
