//! Output parsers (BUILD-2): each turns a step's log into errors, warnings and test
//! results, with the byte range of each in the log. Parsers read the log as bytes, line by
//! line, so offsets are exact whatever the encoding; ANSI colours and a trailing `\r` are
//! ignored when matching.

mod cargo;
mod cc;
mod ctest;
mod hgltest;
mod junit;
mod lines;
mod msvc;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::recipe::{ParserKind, Patterns};

/// Longest message kept for one error or test.
pub const MAX_MESSAGE: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// One error or warning as found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub severity: Severity,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub col: Option<u32>,
    pub code: Option<String>,
    pub message: String,
    pub start: u64,
    pub end: u64,
    pub count: u64,
    /// The file the range is in, when not the log (an XML report).
    pub source: Option<String>,
}

impl Found {
    pub fn new(severity: Severity, message: impl Into<String>, start: u64, end: u64) -> Found {
        Found {
            severity,
            file: None,
            line: None,
            col: None,
            code: None,
            message: clip(message.into().trim()),
            start,
            end,
            count: 1,
            source: None,
        }
    }

    /// Kind for counting warnings: the code (a lint, a `-W` flag), else `warning`.
    pub fn kind(&self) -> String {
        self.code.clone().unwrap_or_else(|| "warning".into())
    }

    fn key(&self) -> (Option<&str>, Option<u32>, Option<u32>, Option<&str>, &str) {
        (
            self.file.as_deref(),
            self.line,
            self.col,
            self.code.as_deref(),
            &self.message,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestCase {
    pub name: String,
    pub outcome: Outcome,
    pub message: String,
    pub start: u64,
    pub end: u64,
    pub source: Option<String>,
}

/// What one step's output said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    pub errors: Vec<Found>,
    pub warnings: Vec<Found>,
    pub tests: Vec<TestCase>,
    /// `hit`, `partial` or `miss`.
    pub cache: Option<&'static str>,
    /// Tests counted without names (passed, failed, skipped), from a tool's totals.
    pub unnamed: (u64, u64, u64),
}

impl Parsed {
    fn push(&mut self, f: Found) {
        match f.severity {
            Severity::Error => self.errors.push(f),
            Severity::Warning => self.warnings.push(f),
        }
    }

    /// Fold identical errors and warnings (same file, line, column, code and message)
    /// into the first, counting them, and keep one result per test name (the last).
    pub fn dedupe(&mut self) {
        self.errors = dedupe(std::mem::take(&mut self.errors));
        self.warnings = dedupe(std::mem::take(&mut self.warnings));
        let mut at: HashMap<String, usize> = HashMap::new();
        let mut tests: Vec<TestCase> = Vec::new();
        for t in std::mem::take(&mut self.tests) {
            match at.get(&t.name) {
                Some(&i) => tests[i] = t,
                None => {
                    at.insert(t.name.clone(), tests.len());
                    tests.push(t);
                }
            }
        }
        self.tests = tests;
    }
}

/// What makes two findings the same: file, line, column, code and message.
pub type DiagKey = (
    Option<String>,
    Option<u32>,
    Option<u32>,
    Option<String>,
    String,
);

pub fn dedupe(items: Vec<Found>) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::with_capacity(items.len());
    let mut at: HashMap<DiagKey, usize> = HashMap::new();
    for f in items {
        let (file, line, col, code, msg) = f.key();
        let k = (
            file.map(String::from),
            line,
            col,
            code.map(String::from),
            msg.to_string(),
        );
        match at.get(&k) {
            Some(&i) => out[i].count += f.count,
            None => {
                at.insert(k, out.len());
                out.push(f);
            }
        }
    }
    out
}

/// Inputs beside the log: XML reports kept as artifacts (path shown, bytes).
pub struct Reports<'a> {
    pub files: Vec<(String, &'a [u8])>,
}

/// Parse `log` with `kind`. `reports` are the step's XML artifacts, which `junit` and
/// `catch2` read in place of the log when there are any.
pub fn parse(
    kind: ParserKind,
    log: &[u8],
    patterns: Option<&Patterns>,
    reports: &Reports<'_>,
) -> Parsed {
    let mut p = match kind {
        ParserKind::Cargo => cargo::parse(log),
        ParserKind::Gcc | ParserKind::Clang => cc::parse(log),
        ParserKind::Msvc => msvc::parse(log),
        ParserKind::Ctest => ctest::parse(log),
        ParserKind::Junit | ParserKind::Catch2 => {
            let xml: Vec<_> = reports
                .files
                .iter()
                .filter(|(n, _)| n.ends_with(".xml"))
                .collect();
            if xml.is_empty() {
                junit::parse(log, None)
            } else {
                let mut all = Parsed::default();
                for (name, bytes) in xml {
                    let p = junit::parse(bytes, Some(name));
                    all.errors.extend(p.errors);
                    all.tests.extend(p.tests);
                }
                all
            }
        }
        ParserKind::HglTest => hgltest::parse(log),
        ParserKind::Lines => lines::parse(log, patterns),
    };
    p.dedupe();
    p
}

/// One line of a log: its byte range (without the newline) and its text for matching
/// (lossy UTF-8, ANSI escapes and a trailing `\r` removed).
#[derive(Debug, Clone)]
pub struct Line<'a> {
    pub start: u64,
    pub end: u64,
    pub text: Cow<'a, str>,
}

pub fn lines(log: &[u8]) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for raw in log.split(|b| *b == b'\n') {
        let start = pos;
        pos += raw.len() + 1;
        if start >= log.len() && raw.is_empty() {
            break;
        }
        let mut t = String::from_utf8_lossy(raw);
        if t.ends_with('\r') {
            t.to_mut().pop();
        }
        if t.contains('\x1b') {
            t = Cow::Owned(strip_ansi(&t));
        }
        out.push(Line {
            start: start as u64,
            end: (start + raw.len()) as u64,
            text: t,
        });
    }
    out
}

static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07").unwrap());

pub fn strip_ansi(s: &str) -> String {
    ANSI.replace_all(s, "").into_owned()
}

/// Cut a message to [`MAX_MESSAGE`] bytes on a character boundary.
pub fn clip(s: &str) -> String {
    if s.len() <= MAX_MESSAGE {
        return s.to_string();
    }
    let mut end = MAX_MESSAGE;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// `CMake Error at CMakeLists.txt:12 (find_package):` and the indented lines after it,
/// shared by the C and C++ parsers. Returns the finding and the lines it used.
fn cmake(all: &[Line<'_>], i: usize) -> Option<(Found, usize)> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^CMake (?P<sev>Error|Warning)(?: \(dev\))?(?: at (?P<file>[^:]+):(?P<line>\d+)(?: \((?P<cmd>[^)]*)\))?)?:\s*(?P<msg>.*)$").unwrap()
    });
    let c = RE.captures(&all[i].text)?;
    let mut msg = c["msg"].trim().to_string();
    let mut j = i + 1;
    let mut end = all[i].end;
    while j < all.len() && (all[j].text.starts_with("  ") || all[j].text.is_empty()) {
        if all[j].text.is_empty() && (j + 1 >= all.len() || !all[j + 1].text.starts_with("  ")) {
            break;
        }
        let t = all[j].text.trim();
        if !t.is_empty() {
            if !msg.is_empty() {
                msg.push(' ');
            }
            msg.push_str(t);
        }
        end = all[j].end;
        j += 1;
    }
    let sev = if &c["sev"] == "Error" {
        Severity::Error
    } else {
        Severity::Warning
    };
    let mut f = Found::new(sev, msg, all[i].start, end);
    f.file = c.name("file").map(|m| m.as_str().to_string());
    f.line = c.name("line").and_then(|m| m.as_str().parse().ok());
    f.code = Some(if sev == Severity::Error {
        "cmake".into()
    } else {
        "cmake-warning".into()
    });
    Some((f, j - i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_offsets_are_exact() {
        let log = b"a\r\n\xffb\n\x1b[31mred\x1b[0m\nlast";
        let ls = lines(log);
        assert_eq!(ls.len(), 4);
        assert_eq!((ls[0].start, ls[0].end, ls[0].text.as_ref()), (0, 2, "a"));
        assert_eq!((ls[1].start, ls[1].end), (3, 5));
        assert_eq!(ls[2].text, "red");
        assert_eq!(&log[ls[3].start as usize..ls[3].end as usize], b"last");
        assert_eq!(lines(b"x\n").len(), 1);
        assert!(lines(b"").is_empty());
    }

    #[test]
    fn dedupe_counts() {
        let a = Found::new(Severity::Error, "boom", 0, 4);
        let mut b = a.clone();
        b.start = 10;
        let mut c = a.clone();
        c.line = Some(3);
        let d = dedupe(vec![a, b, c]);
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].count, d[0].start), (2, 0));
    }

    #[test]
    fn clips_on_a_boundary() {
        let s = "é".repeat(MAX_MESSAGE);
        let c = clip(&s);
        assert!(c.len() <= MAX_MESSAGE + 3 && c.ends_with('…'));
    }
}
