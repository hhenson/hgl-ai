//! `gcc`, `clang`: `file:line:col: error|warning: message [-Wflag]`, linker errors, and
//! CMake's errors. The `In file included from` chain, `In function` context and notes
//! are left out: the error line names the file that has the fault.

use std::sync::LazyLock;

use regex::Regex;

use super::{Found, Parsed, Severity, cmake, lines};

static DIAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<file>(?:[A-Za-z]:)?[^:\s][^:]*?):(?P<line>\d+):(?:(?P<col>\d+):)?\s*(?P<sev>fatal error|error|warning):\s*(?P<msg>.*?)(?:\s+\[(?P<flag>-W[^\]]+)\])?$").unwrap()
});
/// `main.cpp:(.text+0x5): undefined reference to 'f()'` (GNU ld).
static LD_REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\S*ld(?:\.\w+)?: )?(?P<obj>[^:\s]+):\([^)]*\): (?P<msg>(?:undefined reference to|multiple definition of).*)$").unwrap()
});
/// `ld.lld: error: ...`, `collect2: error: ld returned 1 exit status`, `c++: fatal error: ...`.
static TOOL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<tool>[\w./+-]*(?:ld|ld\.lld|ld\.gold|collect2|cc1plus|cc1|gcc|g\+\+|c\+\+|cc|clang|clang\+\+|ar)(?:-\d+)?): (?:(?P<sev>fatal error|error|warning): )?(?P<msg>.+)$").unwrap()
});
/// macOS ld64: `Undefined symbols for architecture arm64:` then `  "_f", referenced from:`.
static UNDEFINED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Undefined symbols for architecture (?P<arch>\S+):$").unwrap());
static SYMBOL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s+"(?P<sym>[^"]+)", referenced from:$"#).unwrap());

pub fn parse(log: &[u8]) -> Parsed {
    let all = lines(log);
    let mut p = Parsed::default();
    let mut i = 0;
    while i < all.len() {
        let l = &all[i];
        let t = l.text.as_ref();
        if let Some(c) = DIAG.captures(t) {
            let sev = if &c["sev"] == "warning" {
                Severity::Warning
            } else {
                Severity::Error
            };
            let mut f = Found::new(sev, &c["msg"], l.start, l.end);
            f.file = Some(c["file"].to_string());
            f.line = c["line"].parse().ok();
            f.col = c.name("col").and_then(|m| m.as_str().parse().ok());
            f.code = c.name("flag").map(|m| m.as_str().to_string());
            if c["sev"].starts_with("fatal") && f.code.is_none() {
                f.code = Some("fatal".into());
            }
            p.push(f);
        } else if let Some(c) = LD_REF.captures(t) {
            let mut f = Found::new(Severity::Error, &c["msg"], l.start, l.end);
            f.file = Some(c["obj"].to_string());
            f.code = Some("ld".into());
            p.push(f);
        } else if let Some(c) = UNDEFINED.captures(t) {
            let mut syms = Vec::new();
            let mut end = l.end;
            for n in all.iter().skip(i + 1).take(64) {
                if let Some(s) = SYMBOL.captures(&n.text) {
                    syms.push(s["sym"].to_string());
                    end = n.end;
                } else if !n.text.starts_with("  ") {
                    break;
                }
            }
            let mut f = Found::new(
                Severity::Error,
                format!(
                    "undefined symbols for architecture {}: {}",
                    &c["arch"],
                    syms.join(", ")
                ),
                l.start,
                end,
            );
            f.code = Some("ld".into());
            p.push(f);
        } else if let Some((f, used)) = cmake(&all, i) {
            p.push(f);
            i += used;
            continue;
        } else if let Some(c) = TOOL.captures(t) {
            let msg = c["msg"].trim();
            let sev = match c.name("sev").map(|m| m.as_str()) {
                Some("warning") => Some(Severity::Warning),
                Some(_) => Some(Severity::Error),
                // `ld: symbol(s) not found`, `ld: library not found for -lx`
                None if c["tool"].ends_with("ld")
                    && (msg.contains("not found") || msg.starts_with("cannot find")) =>
                {
                    Some(Severity::Error)
                }
                None => None,
            };
            if let Some(sev) = sev
                && !msg.starts_with("in function")
            {
                let mut f = Found::new(sev, msg, l.start, l.end);
                f.code = Some(c["tool"].rsplit('/').next().unwrap_or_default().to_string());
                p.push(f);
            }
        }
        i += 1;
    }
    p
}
