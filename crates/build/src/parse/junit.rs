//! `junit`, `catch2`: JUnit XML (`<testcase>` with `<failure>`, `<error>` or `<skipped>`),
//! as CTest's `--output-junit`, Catch2's `--reporter junit` and GoogleTest's
//! `--gtest_output=xml` write it. A small reader of its own: elements, attributes, text,
//! CDATA, comments and the five entities; anything else is skipped.

use super::{Found, Outcome, Parsed, Severity, TestCase, clip};

pub fn parse(xml: &[u8], source: Option<&str>) -> Parsed {
    let mut p = Parsed::default();
    // In a log, the report starts at its declaration or its first element.
    let from = find(xml, b"<?xml")
        .or_else(|| find(xml, b"<testsuite"))
        .unwrap_or(xml.len());
    let mut r = Reader { b: xml, pos: from };
    let mut case: Option<Case> = None;
    let mut in_result: Option<(Outcome, String, String)> = None;
    while let Some(tok) = r.next() {
        match tok {
            Tok::Open {
                name,
                attrs,
                start,
                empty,
            } => match name.as_str() {
                "testcase" => {
                    let n = attr(&attrs, "name").unwrap_or_default();
                    let class = attr(&attrs, "classname").unwrap_or_default();
                    let full = if class.is_empty() || class.ends_with(".global") || class == n {
                        n
                    } else {
                        format!("{class}.{n}")
                    };
                    let skipped = matches!(
                        attr(&attrs, "status").as_deref(),
                        Some("notrun" | "skipped" | "disabled")
                    );
                    let c = Case {
                        name: full,
                        outcome: if skipped {
                            Outcome::Skip
                        } else {
                            Outcome::Pass
                        },
                        message: String::new(),
                        start,
                        end: r.pos as u64,
                    };
                    if empty {
                        p.tests.push(c.finish(source));
                    } else {
                        case = Some(c);
                    }
                }
                "failure" | "error" | "skipped" if case.is_some() => {
                    let outcome = if name == "skipped" {
                        Outcome::Skip
                    } else {
                        Outcome::Fail
                    };
                    let msg = attr(&attrs, "message").unwrap_or_default();
                    let kind = attr(&attrs, "type").unwrap_or_default();
                    if empty {
                        apply(case.as_mut(), outcome, &msg, &kind, "");
                    } else {
                        in_result = Some((outcome, msg, kind));
                    }
                }
                _ => {}
            },
            Tok::Text(t) => {
                if in_result.is_some()
                    && let Some(c) = case.as_mut()
                {
                    c.message.push_str(&t);
                }
            }
            Tok::Close { name, end } => match name.as_str() {
                "failure" | "error" | "skipped" => {
                    if let Some((outcome, msg, kind)) = in_result.take()
                        && let Some(c) = case.as_mut()
                    {
                        let text = std::mem::take(&mut c.message);
                        apply(Some(c), outcome, &msg, &kind, &text);
                    }
                }
                "testcase" => {
                    if let Some(mut c) = case.take() {
                        c.end = end;
                        p.tests.push(c.finish(source));
                    }
                }
                _ => {}
            },
        }
    }
    if from == xml.len() && source.is_some() {
        let mut f = Found::new(Severity::Error, "the report is not JUnit XML", 0, 0);
        f.source = source.map(String::from);
        p.errors.push(f);
    }
    p
}

struct Case {
    name: String,
    outcome: Outcome,
    message: String,
    start: u64,
    end: u64,
}

impl Case {
    fn finish(self, source: Option<&str>) -> TestCase {
        TestCase {
            name: self.name,
            outcome: self.outcome,
            message: clip(self.message.trim()),
            start: self.start,
            end: self.end,
            source: source.map(String::from),
            group: None,
        }
    }
}

/// Record a `<failure>`/`<error>`/`<skipped>`: its message attribute, else the first
/// lines of its text, and a Catch2 `at file:line` when the text has one.
fn apply(c: Option<&mut Case>, outcome: Outcome, msg: &str, kind: &str, text: &str) {
    let Some(c) = c else { return };
    if c.outcome == Outcome::Fail {
        return; // the first failure is the one to show
    }
    c.outcome = outcome;
    let text = text.trim();
    let mut m = if !msg.trim().is_empty() {
        msg.trim().to_string()
    } else {
        text.lines().take(3).collect::<Vec<_>>().join("\n")
    };
    if m.is_empty() {
        m = kind.to_string();
    }
    if let Some(at) = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with("at ") && l.contains(':'))
        && !m.contains(at)
    {
        m.push('\n');
        m.push_str(at);
    }
    c.message = m;
}

fn attr(attrs: &[(String, String)], k: &str) -> Option<String> {
    attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

enum Tok {
    Open {
        name: String,
        attrs: Vec<(String, String)>,
        start: u64,
        empty: bool,
    },
    Close {
        name: String,
        end: u64,
    },
    Text(String),
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn next(&mut self) -> Option<Tok> {
        loop {
            if self.pos >= self.b.len() {
                return None;
            }
            let rest = &self.b[self.pos..];
            if rest.starts_with(b"<!--") {
                self.skip_past(b"-->");
                continue;
            }
            if rest.starts_with(b"<![CDATA[") {
                let s = self.pos + 9;
                let e = find(&self.b[s..], b"]]>").map_or(self.b.len(), |i| s + i);
                self.pos = (e + 3).min(self.b.len());
                return Some(Tok::Text(
                    String::from_utf8_lossy(&self.b[s..e]).into_owned(),
                ));
            }
            if rest.starts_with(b"<?") || rest.starts_with(b"<!") {
                self.skip_past(b">");
                continue;
            }
            if rest.starts_with(b"</") {
                let s = self.pos + 2;
                let e = find(&self.b[s..], b">").map_or(self.b.len(), |i| s + i);
                let name = String::from_utf8_lossy(&self.b[s..e]).trim().to_string();
                self.pos = (e + 1).min(self.b.len());
                return Some(Tok::Close {
                    name,
                    end: self.pos as u64,
                });
            }
            if rest.starts_with(b"<") {
                let start = self.pos as u64;
                let e = self.tag_end(self.pos + 1);
                let inner = &self.b[self.pos + 1..e];
                self.pos = (e + 1).min(self.b.len());
                let empty = inner.ends_with(b"/");
                let inner = if empty {
                    &inner[..inner.len() - 1]
                } else {
                    inner
                };
                let text = String::from_utf8_lossy(inner);
                let (name, attrs) = parse_tag(&text);
                return Some(Tok::Open {
                    name,
                    attrs,
                    start,
                    empty,
                });
            }
            let e = find(rest, b"<").map_or(self.b.len(), |i| self.pos + i);
            let t = unescape(&String::from_utf8_lossy(&self.b[self.pos..e]));
            self.pos = e;
            return Some(Tok::Text(t));
        }
    }

    /// The `>` that ends a start tag, past quoted attribute values.
    fn tag_end(&self, mut i: usize) -> usize {
        let mut quote = None;
        while i < self.b.len() {
            let c = self.b[i];
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => {}
                None if c == b'"' || c == b'\'' => quote = Some(c),
                None if c == b'>' => return i,
                None => {}
            }
            i += 1;
        }
        self.b.len()
    }

    fn skip_past(&mut self, end: &[u8]) {
        self.pos =
            find(&self.b[self.pos..], end).map_or(self.b.len(), |i| self.pos + i + end.len());
    }
}

fn parse_tag(t: &str) -> (String, Vec<(String, String)>) {
    let t = t.trim();
    let (name, mut rest) = t.split_once(|c: char| c.is_whitespace()).unwrap_or((t, ""));
    let mut attrs = Vec::new();
    loop {
        rest = rest.trim_start();
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().to_string();
        let after = rest[eq + 1..].trim_start();
        let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            break;
        };
        let body = &after[1..];
        let Some(close) = body.find(q) else { break };
        attrs.push((key, unescape(&body[..close])));
        rest = &body[close + 1..];
    }
    (name.to_string(), attrs)
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';').filter(|n| *n <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let ch = match ent {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if ent.starts_with("#x") => u32::from_str_radix(&ent[2..], 16)
                .ok()
                .and_then(char::from_u32),
            _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}
