//! `lines`: lines matching the step's `patterns` (named groups `file`, `line`, `col`,
//! `code`, `message`; the whole line is the message without one). Without patterns,
//! lines that start with `error`, `fatal` or `warning`.

use std::sync::LazyLock;

use regex::Regex;

use super::{Found, Parsed, Severity, lines};
use crate::recipe::Patterns;

static DEFAULT_ERROR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*(?:error|fatal(?: error)?)(?:\[(?P<code>[^\]]+)\])?\s*[:!]\s*(?P<message>.+)$",
    )
    .unwrap()
});
static DEFAULT_WARNING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*warning(?:\[(?P<code>[^\]]+)\])?\s*:\s*(?P<message>.+)$").unwrap()
});

pub fn parse(log: &[u8], patterns: Option<&Patterns>) -> Parsed {
    let compile =
        |v: &[String]| -> Vec<Regex> { v.iter().filter_map(|r| Regex::new(r).ok()).collect() };
    let (errors, warnings) = match patterns {
        Some(p) if !p.error.is_empty() || !p.warning.is_empty() => {
            (compile(&p.error), compile(&p.warning))
        }
        _ => (vec![DEFAULT_ERROR.clone()], vec![DEFAULT_WARNING.clone()]),
    };
    let mut p = Parsed::default();
    for l in lines(log) {
        let found = errors
            .iter()
            .find_map(|r| r.captures(&l.text).map(|c| (Severity::Error, c)))
            .or_else(|| {
                warnings
                    .iter()
                    .find_map(|r| r.captures(&l.text).map(|c| (Severity::Warning, c)))
            });
        let Some((sev, c)) = found else { continue };
        let msg = c
            .name("message")
            .map_or_else(|| l.text.trim().to_string(), |m| m.as_str().to_string());
        let mut f = Found::new(sev, msg, l.start, l.end);
        f.file = c.name("file").map(|m| m.as_str().to_string());
        f.line = c.name("line").and_then(|m| m.as_str().parse().ok());
        f.col = c.name("col").and_then(|m| m.as_str().parse().ok());
        f.code = c.name("code").map(|m| m.as_str().to_string());
        p.push(f);
    }
    p
}
