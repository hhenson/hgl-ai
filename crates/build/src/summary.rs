//! Summaries (BUILD-4): gathering the steps' parsed output into a [`Summary`], the
//! bounded copy kept on the hub, and the text an agent reads (at most [`TEXT_LIMIT`]).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use hgl_ai_proto::build::{BuildStatus, Diag, StepStatus, StepSummary, Summary, TestFailure};

use crate::parse::{Found, Outcome, Parsed};

/// Errors and failing tests kept in `summary.json` and receipts.
pub const KEEP: usize = 200;
/// Errors and failing tests shown in the text and kept on the hub.
pub const SHOWN: usize = 20;
/// Bound on the text summary, in bytes.
pub const TEXT_LIMIT: usize = 4096;
/// Bound on one message in the hub's copy.
const HUB_MESSAGE: usize = 300;

/// Everything the steps reported, deduplicated across steps.
#[derive(Debug, Default)]
pub struct Collect {
    errors: Vec<(String, Found)>,
    warnings: Vec<(String, Found)>,
    passed: u64,
    failed: u64,
    skipped: u64,
    failures: Vec<TestFailure>,
}

impl Collect {
    /// Add one step's findings; returns its distinct error and warning counts.
    pub fn add(&mut self, step: &str, p: Parsed) -> (u64, u64) {
        let counts = (p.errors.len() as u64, p.warnings.len() as u64);
        self.errors
            .extend(p.errors.into_iter().map(|f| (step.to_string(), f)));
        self.warnings
            .extend(p.warnings.into_iter().map(|f| (step.to_string(), f)));
        for t in p.tests {
            match t.outcome {
                Outcome::Pass => self.passed += 1,
                Outcome::Skip => self.skipped += 1,
                Outcome::Fail => {
                    self.failed += 1;
                    if self.failures.len() < KEEP {
                        self.failures.push(TestFailure {
                            step: step.to_string(),
                            name: t.name,
                            message: t.message,
                            start: t.start,
                            end: t.end,
                            source: t.source,
                        });
                    }
                }
            }
        }
        self.passed += p.unnamed.0;
        self.failed += p.unnamed.1;
        self.skipped += p.unnamed.2;
        counts
    }

    /// Write the errors, tests and warnings into `s`.
    pub fn fill(&self, s: &mut Summary) {
        let errors = fold(&self.errors);
        s.errors_total = errors.len() as u64;
        s.errors = errors.into_iter().take(KEEP).collect();
        let warnings = fold(&self.warnings);
        s.warnings_total = warnings.len() as u64;
        let mut by_kind = BTreeMap::new();
        for w in &warnings {
            let kind = w.code.clone().unwrap_or_else(|| "warning".into());
            *by_kind.entry(kind).or_insert(0) += 1;
        }
        s.warnings = by_kind;
        s.tests.passed = self.passed;
        s.tests.failed = self.failed;
        s.tests.skipped = self.skipped;
        s.tests.failures = self.failures.clone();
    }
}

/// Deduplicate across steps on file, line, column, code and message; the first step's
/// occurrence stands and counts the rest.
fn fold(items: &[(String, Found)]) -> Vec<Diag> {
    let mut out: Vec<Diag> = Vec::new();
    let mut at: HashMap<crate::parse::DiagKey, usize> = HashMap::new();
    for (step, f) in items {
        let k = (
            f.file.clone(),
            f.line,
            f.col,
            f.code.clone(),
            f.message.clone(),
        );
        match at.get(&k) {
            Some(&i) => out[i].count += f.count,
            None => {
                at.insert(k, out.len());
                out.push(Diag {
                    step: step.clone(),
                    file: f.file.clone(),
                    line: f.line,
                    col: f.col,
                    code: f.code.clone(),
                    message: f.message.clone(),
                    count: f.count,
                    start: f.start,
                    end: f.end,
                    source: f.source.clone(),
                });
            }
        }
    }
    out
}

/// The hub's copy: the first [`SHOWN`] errors and failures, messages shortened.
pub fn bounded(s: &Summary) -> Summary {
    let mut b = s.clone();
    b.errors.truncate(SHOWN);
    b.tests.failures.truncate(SHOWN);
    for e in &mut b.errors {
        e.message = cut(&e.message, HUB_MESSAGE);
    }
    for f in &mut b.tests.failures {
        f.message = cut(&f.message, HUB_MESSAGE);
    }
    b
}

/// The overall status from the steps: a failure wins, then a missing need, then a stop.
pub fn overall(steps: &[StepSummary]) -> BuildStatus {
    let has = |st: StepStatus| steps.iter().any(|s| s.status == st);
    if has(StepStatus::Fail) {
        BuildStatus::Fail
    } else if has(StepStatus::NotRun) {
        BuildStatus::NotRun
    } else if has(StepStatus::Stopped) {
        BuildStatus::Stopped
    } else if has(StepStatus::Running) {
        BuildStatus::Running
    } else if steps.is_empty() {
        BuildStatus::NotRun
    } else {
        BuildStatus::Pass
    }
}

pub fn ms(ms: u64) -> String {
    if ms < 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 120_000 {
        format!("{}s", ms / 1000)
    } else if ms < 7_200_000 {
        format!("{}m{:02}s", ms / 60_000, (ms / 1000) % 60)
    } else {
        format!("{}h{:02}m", ms / 3_600_000, (ms / 60_000) % 60)
    }
}

fn size(b: u64) -> String {
    hgl_ai_cap::format_size(b)
}

/// One line of at most `width` bytes: newlines become ` | `.
fn cut(s: &str, width: usize) -> String {
    let flat: String = s
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if flat.len() <= width {
        return flat;
    }
    let mut end = width.saturating_sub(3);
    while !flat.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &flat[..end])
}

fn log_name(s: &Summary, step: &str, source: Option<&str>) -> String {
    if let Some(src) = source {
        return src.rsplit(['/', '\\']).next().unwrap_or(src).to_string();
    }
    s.steps
        .iter()
        .find(|x| x.name == step)
        .and_then(|x| x.log.as_deref())
        .map(|l| l.rsplit(['/', '\\']).next().unwrap_or(l).to_string())
        .unwrap_or_else(|| format!("{step}.log"))
}

/// The text summary, at most [`TEXT_LIMIT`] bytes.
pub fn render(s: &Summary) -> String {
    for width in [220, 160, 110, 80] {
        let t = render_at(s, width, SHOWN);
        if t.len() <= TEXT_LIMIT {
            return t;
        }
    }
    for n in (0..SHOWN).rev() {
        let t = render_at(s, 80, n);
        if t.len() <= TEXT_LIMIT {
            return t;
        }
    }
    let mut t = render_at(s, 60, 0);
    if t.len() > TEXT_LIMIT {
        let mut end = TEXT_LIMIT - 40;
        while !t.is_char_boundary(end) {
            end -= 1;
        }
        t.truncate(end);
        t.push_str("\n... cut; read summary.json\n");
    }
    t
}

fn render_at(s: &Summary, width: usize, shown: usize) -> String {
    let mut o = String::new();
    let head = s
        .head
        .as_deref()
        .map(|h| format!(" at {}", &h[..h.len().min(12)]))
        .unwrap_or_default();
    let dirty = if s.dirty { "+changes" } else { "" };
    let base = s
        .base
        .as_deref()
        .map(|b| format!(" onto {}", &b[..b.len().min(12)]))
        .unwrap_or_default();
    let via = s
        .proxy
        .as_deref()
        .map(|p| format!(", via {p}"))
        .unwrap_or_default();
    let _ = writeln!(
        o,
        "build {} {}{head}{dirty}{base} on {} ({}{via}): {} in {}",
        s.id,
        s.repo,
        s.host,
        s.platform,
        s.status.as_str().to_uppercase(),
        ms(s.wall_ms)
    );
    if let Some(r) = &s.reason {
        let _ = writeln!(o, "  {}", cut(r, width));
    }
    let wname = s
        .steps
        .iter()
        .map(|x| x.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    for st in &s.steps {
        let mut line = format!(
            "  {:<wname$} {:<8} {:>7}",
            st.name,
            st.status.as_str(),
            if st.wall_ms > 0 || st.status == StepStatus::Pass || st.status == StepStatus::Fail {
                ms(st.wall_ms)
            } else {
                "-".into()
            }
        );
        if let Some(c) = st.cpu_ms {
            let _ = write!(line, " cpu {}", ms(c));
        }
        match (st.peak_bytes, st.cap_bytes) {
            (Some(p), Some(c)) => {
                let _ = write!(line, " peak {}/{}", size(p), size(c));
            }
            (Some(p), None) => {
                let _ = write!(line, " peak {}", size(p));
            }
            _ => {}
        }
        if let Some(c) = &st.cache {
            let _ = write!(line, " cache {c}");
        }
        if let Some(e) = st.exit.filter(|e| *e != 0) {
            let _ = write!(line, " exit {e}");
        }
        if let Some(l) = &st.limit {
            let _ = write!(line, " {l} limit");
        }
        if let Some(r) = &st.reason {
            let _ = write!(line, ": {}", cut(r, width));
        }
        let _ = writeln!(o, "{line}");
    }
    if s.errors_total > 0 {
        let n = s.errors.len().min(shown);
        let _ = writeln!(o, "errors: {} (first {n})", s.errors_total);
        for e in s.errors.iter().take(n) {
            let mut at = String::new();
            if let Some(f) = &e.file {
                at.push_str(f);
                if let Some(l) = e.line {
                    let _ = write!(at, ":{l}");
                    if let Some(c) = e.col {
                        let _ = write!(at, ":{c}");
                    }
                }
                at.push(' ');
            }
            if let Some(c) = &e.code {
                let _ = write!(at, "[{c}] ");
            }
            let times = if e.count > 1 {
                format!(" (x{})", e.count)
            } else {
                String::new()
            };
            let _ = writeln!(
                o,
                "  {at}{}{times}  [{} {}-{}]",
                cut(&e.message, width),
                log_name(s, &e.step, e.source.as_deref()),
                e.start,
                e.end
            );
        }
    }
    let t = &s.tests;
    if t.passed + t.failed + t.skipped > 0 {
        let _ = writeln!(
            o,
            "tests: {} passed, {} failed, {} skipped",
            t.passed, t.failed, t.skipped
        );
        let n = t.failures.len().min(shown);
        for f in t.failures.iter().take(n) {
            let _ = writeln!(
                o,
                "  FAILED {}: {}  [{} {}-{}]",
                f.name,
                cut(&f.message, width),
                log_name(s, &f.step, f.source.as_deref()),
                f.start,
                f.end
            );
        }
        if (t.failed as usize) > n {
            let _ = writeln!(o, "  ... {} more failing", t.failed as usize - n);
        }
    }
    if s.warnings_total > 0 {
        let mut kinds: Vec<(&String, &u64)> = s.warnings.iter().collect();
        kinds.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let shown_kinds: Vec<String> = kinds
            .iter()
            .take(12)
            .map(|(k, n)| format!("{k} {n}"))
            .collect();
        let more = if kinds.len() > 12 { ", ..." } else { "" };
        let _ = writeln!(
            o,
            "warnings: {} ({}{more})",
            s.warnings_total,
            shown_kinds.join(", ")
        );
    }
    let _ = writeln!(
        o,
        "log: {} on {} (summary.json; a range: hgl-ai build log {} STEP --bytes START-END)",
        s.log_dir,
        s.proxy.as_deref().unwrap_or(&s.host),
        s.id
    );
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::Severity;
    use hgl_ai_proto::build::{BUILD_VERSION, Platform, Tests};

    pub(crate) fn sample() -> Summary {
        Summary {
            v: BUILD_VERSION,
            id: "20261127-083000-3fa9c1".into(),
            repo: "demo".into(),
            head: Some("8a5c3d264c2edbbdaa61530d810bff276c3eebba".into()),
            base: None,
            dirty: false,
            host: "lab1".into(),
            platform: Platform::Linux,
            executor: "local".into(),
            proxy: None,
            requester: None,
            status: BuildStatus::Fail,
            reason: None,
            recipe_rev: Some(1),
            started_at: "20261127T083000Z".into(),
            ended_at: None,
            wall_ms: 75_000,
            log_dir: "/w/build-logs/20261127-083000-3fa9c1".into(),
            steps: vec![],
            errors: vec![],
            errors_total: 0,
            tests: Tests::default(),
            warnings_total: 0,
            warnings: BTreeMap::new(),
        }
    }

    #[test]
    fn bounded_text() {
        let mut s = sample();
        let mut c = Collect::default();
        let mut p = Parsed::default();
        for i in 0..300 {
            let mut f = Found::new(Severity::Error, "x".repeat(900) + &i.to_string(), 0, 10);
            f.file = Some(format!("src/f{i}.rs"));
            f.line = Some(i);
            p.errors.push(f.clone());
            if i < 3 {
                p.errors.push(f); // the same again: folded
            }
            p.tests.push(crate::parse::TestCase {
                name: format!("t{i}"),
                outcome: Outcome::Fail,
                message: "assertion failed\nleft: 1\nright: 2".into(),
                start: 0,
                end: 1,
                source: None,
            });
        }
        let mut w = Found::new(Severity::Warning, "unused", 0, 1);
        w.code = Some("unused_variables".into());
        p.warnings.push(w);
        c.add("test", p);
        s.steps.push(StepSummary::new("test", StepStatus::Fail));
        c.fill(&mut s);
        assert_eq!(s.errors_total, 300);
        assert_eq!(s.errors.len(), KEEP);
        assert_eq!(s.errors[0].count, 2);
        assert_eq!(s.tests.failed, 300);
        assert_eq!(s.warnings["unused_variables"], 1);
        let t = render(&s);
        assert!(t.len() <= TEXT_LIMIT, "{}", t.len());
        assert!(t.contains("errors: 300"), "{t}");
        assert!(t.starts_with(
            "build 20261127-083000-3fa9c1 demo at 8a5c3d264c2e on lab1 (linux): FAIL in 75s"
        ));
        let b = bounded(&s);
        assert_eq!(b.errors.len(), SHOWN);
        assert!(b.errors[0].message.len() <= HUB_MESSAGE);
    }

    #[test]
    fn statuses() {
        let st = |x| StepSummary::new("a", x);
        assert_eq!(overall(&[st(StepStatus::Pass)]), BuildStatus::Pass);
        assert_eq!(
            overall(&[st(StepStatus::Pass), st(StepStatus::NotRun)]),
            BuildStatus::NotRun
        );
        assert_eq!(
            overall(&[st(StepStatus::NotRun), st(StepStatus::Fail)]),
            BuildStatus::Fail
        );
        assert_eq!(overall(&[]), BuildStatus::NotRun);
        assert_eq!(ms(1500), "1.5s");
        assert_eq!(ms(75_000), "75s");
        assert_eq!(ms(200_000), "3m20s");
    }
}
