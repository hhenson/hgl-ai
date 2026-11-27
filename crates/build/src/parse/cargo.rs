//! `cargo`: `--message-format=json` diagnostics (level, code, message, primary span),
//! rustc's human form as a fallback, fresh artifacts for the cache, and `cargo test`'s
//! results with each failure's panic message.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::{Found, Line, Outcome, Parsed, Severity, TestCase, clip, lines};

static TEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^test (?P<name>.+?) \.\.\. (?P<res>ok|FAILED|ignored(?:, .*)?)$").unwrap()
});
/// `Running unittests src/lib.rs (target/debug/deps/demo-1f2e)`, `Doc-tests demo`.
static RUNNING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:Running (?P<what>.+?)(?: \((?P<path>[^)]+)\))?|Doc-tests (?P<doc>\S+))\s*$")
        .unwrap()
});
static SECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^---- (?P<name>.+?) stdout ----$").unwrap());
/// `thread 'x' (1234) panicked at src/lib.rs:3:5:` (Rust 1.89+ name the thread id),
/// `thread 'x' panicked at src/lib.rs:3:5:` (1.73+), or the older
/// `thread 'x' panicked at 'msg', src/lib.rs:3:5`.
static PANIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^thread '(?P<thread>[^']*)'(?: \(\d+\))? panicked at (?:'(?P<old>.*)', )?(?P<at>\S+?):?$",
    )
    .unwrap()
});
static HUMAN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<sev>error|warning)(?:\[(?P<code>[^\]]+)\])?: (?P<msg>.+)$").unwrap()
});
static ARROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*--> (?P<file>.+?):(?P<line>\d+):(?P<col>\d+)$").unwrap());
/// Lines that only say how many came before, or that a crate failed because of them.
static ECHO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(aborting due to|\d+ warnings? emitted|could not compile|test failed, to rerun|\d+ targets? failed|build failed|`[^`]+` \([^)]*\) generated \d+ warnings?|for more information about this error|some errors have detailed explanations)",
    )
    .unwrap()
});

pub fn parse(log: &[u8]) -> Parsed {
    let all = lines(log);
    let mut p = Parsed::default();
    let (mut fresh, mut built) = (0u32, 0u32);
    let mut results: Vec<TestCase> = Vec::new();
    let mut panics: HashMap<(Option<String>, String), (String, u64, u64)> = HashMap::new();
    // The test binary running now; its name keeps tests of two binaries apart.
    let mut group: Option<String> = None;
    let mut i = 0;
    while i < all.len() {
        let l = &all[i];
        let t = l.text.as_ref();
        if t.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<Value>(t) {
                match v["reason"].as_str() {
                    Some("compiler-message") => {
                        if let Some(f) = diagnostic(&v["message"], l) {
                            p.push(f);
                        }
                    }
                    Some("compiler-artifact") => {
                        built += 1;
                        fresh += u32::from(v["fresh"].as_bool() == Some(true));
                    }
                    _ => {}
                }
            }
            i += 1;
            continue;
        }
        if let Some(c) = RUNNING.captures(t) {
            group = Some(match (c.name("doc"), c.name("what"), c.name("path")) {
                (Some(d), _, _) => format!("doc-tests {}", d.as_str()),
                (None, Some(w), Some(p)) => format!("{} in {}", w.as_str(), binary(p.as_str())),
                (None, Some(w), None) => w.as_str().to_string(),
                _ => unreachable!("the pattern has one or the other"),
            });
            i += 1;
            continue;
        }
        if let Some(c) = TEST.captures(t) {
            let outcome = match &c["res"] {
                "ok" => Outcome::Pass,
                "FAILED" => Outcome::Fail,
                _ => Outcome::Skip,
            };
            results.push(TestCase {
                name: c["name"].to_string(),
                outcome,
                message: String::new(),
                start: l.start,
                end: l.end,
                source: None,
                group: group.clone(),
            });
            i += 1;
            continue;
        }
        if let Some(c) = SECTION.captures(t) {
            let name = c["name"].to_string();
            let (msg, end, used) = failure_section(&all, i + 1);
            panics.insert((group.clone(), name), (msg, l.start, end));
            i += 1 + used;
            continue;
        }
        if let Some(c) = HUMAN.captures(t) {
            let msg = c["msg"].trim();
            if !ECHO.is_match(msg) {
                let sev = if &c["sev"] == "error" {
                    Severity::Error
                } else {
                    Severity::Warning
                };
                let mut f = Found::new(sev, msg, l.start, l.end);
                f.code = c.name("code").map(|m| m.as_str().to_string());
                if let Some(a) = all
                    .iter()
                    .skip(i + 1)
                    .take(3)
                    .find_map(|n| ARROW.captures(&n.text))
                {
                    f.file = Some(a["file"].to_string());
                    f.line = a["line"].parse().ok();
                    f.col = a["col"].parse().ok();
                }
                p.push(f);
            }
        }
        i += 1;
    }
    for mut r in results {
        if r.outcome == Outcome::Fail
            && let Some((msg, start, end)) = panics.remove(&(r.group.clone(), r.name.clone()))
        {
            r.message = msg;
            r.start = start;
            r.end = end;
        }
        p.tests.push(r);
    }
    if built > 0 {
        p.cache = Some(if fresh == built {
            "hit"
        } else if fresh == 0 {
            "miss"
        } else {
            "partial"
        });
    }
    p
}

/// A `compiler-message`'s diagnostic, unless it is a note or a count of earlier ones.
fn diagnostic(m: &Value, l: &Line<'_>) -> Option<Found> {
    let sev = match m["level"].as_str()? {
        "error" | "error: internal compiler error" => Severity::Error,
        "warning" => Severity::Warning,
        _ => return None,
    };
    let message = m["message"].as_str()?.trim();
    let spans = m["spans"].as_array().map(Vec::as_slice).unwrap_or_default();
    if spans.is_empty() && ECHO.is_match(message) {
        return None;
    }
    let mut f = Found::new(sev, message, l.start, l.end);
    f.code = m["code"]["code"].as_str().map(String::from);
    if let Some(s) = spans
        .iter()
        .find(|s| s["is_primary"].as_bool() == Some(true))
        .or_else(|| spans.first())
    {
        f.file = s["file_name"].as_str().map(String::from);
        f.line = s["line_start"].as_u64().and_then(|n| u32::try_from(n).ok());
        f.col = s["column_start"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok());
    }
    Some(f)
}

/// The panic message of a `---- name stdout ----` section: the lines after
/// `panicked at`, up to a blank line or the backtrace note. Returns it, the end of the
/// section and the lines used.
fn failure_section(all: &[Line<'_>], from: usize) -> (String, u64, usize) {
    let mut msg: Vec<String> = Vec::new();
    let mut in_panic = false;
    let mut end = all.get(from.saturating_sub(1)).map_or(0, |l| l.end);
    let mut j = from;
    while j < all.len() {
        let t = all[j].text.as_ref();
        if SECTION.is_match(t) || t == "failures:" || t.starts_with("test result:") {
            break;
        }
        end = all[j].end;
        if let Some(c) = PANIC.captures(t) {
            in_panic = true;
            msg.clear();
            if let Some(old) = c.name("old") {
                msg.push(old.as_str().to_string());
                in_panic = false;
            }
            let at = c["at"].trim_end_matches(':');
            msg.push(format!("at {at}"));
        } else if in_panic {
            if t.trim().is_empty() || t.starts_with("note: run with `RUST_BACKTRACE") {
                in_panic = false;
            } else {
                msg.insert(msg.len().saturating_sub(1), t.trim_end().to_string());
            }
        }
        j += 1;
    }
    if msg.is_empty() {
        // No panic line (a test that failed another way): its first output line.
        if let Some(l) = all[from.min(all.len())..j]
            .iter()
            .find(|l| !l.text.trim().is_empty())
        {
            msg.push(l.text.trim().to_string());
        }
    }
    (clip(&msg.join("\n")), end, j - from)
}

/// The crate of a test binary path: `target/debug/deps/demo_cli-1f2e3d` is `demo_cli`.
fn binary(path: &str) -> &str {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let file = file.strip_suffix(".exe").unwrap_or(file);
    match file.rsplit_once('-') {
        Some((name, hash)) if !name.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit()) => {
            name
        }
        _ => file,
    }
}
