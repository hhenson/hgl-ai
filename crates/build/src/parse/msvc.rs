//! `msvc`: `file(line,col): error C2065: message`, linker and tool lines
//! (`a.obj : error LNK2019: ...`, `LINK : fatal error LNK1120: ...`), MSBuild's trailing
//! `[project.vcxproj]` removed, and CMake's errors.

use std::sync::LazyLock;

use regex::Regex;

use super::{Found, Parsed, Severity, cmake, lines};

static DIAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:\d+>)?(?P<file>[^()\s][^()]*?)\((?P<line>\d+)(?:,(?P<col>\d+))?(?:,\d+,\d+)?\)\s*:\s*(?P<sev>fatal error|error|warning)\s+(?P<code>[A-Z]+\d+)\s*:\s*(?P<msg>.*?)(?:\s+\[[^\]]+\])?\s*$").unwrap()
});
static TOOL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:\d+>)?(?P<file>[^:()]+?)\s*:\s*(?:command line )?(?P<sev>fatal error|error|warning)\s+(?P<code>[A-Z]+\d+)\s*:\s*(?P<msg>.*?)(?:\s+\[[^\]]+\])?\s*$").unwrap()
});

pub fn parse(log: &[u8]) -> Parsed {
    let all = lines(log);
    let mut p = Parsed::default();
    let mut i = 0;
    while i < all.len() {
        let l = &all[i];
        let t = l.text.as_ref();
        let caps = DIAG.captures(t).or_else(|| TOOL.captures(t));
        if let Some(c) = caps {
            let sev = if &c["sev"] == "warning" {
                Severity::Warning
            } else {
                Severity::Error
            };
            let mut f = Found::new(sev, &c["msg"], l.start, l.end);
            let file = c["file"].trim();
            if !matches!(file, "LINK" | "cl" | "lib" | "link" | "CL") {
                f.file = Some(file.to_string());
            }
            f.line = c.name("line").and_then(|m| m.as_str().parse().ok());
            f.col = c.name("col").and_then(|m| m.as_str().parse().ok());
            f.code = Some(c["code"].to_string());
            p.push(f);
        } else if let Some((f, used)) = cmake(&all, i) {
            p.push(f);
            i += used;
            continue;
        }
        i += 1;
    }
    p
}
