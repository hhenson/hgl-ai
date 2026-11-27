//! Formats of `HGL_STATE/main` and `HGL_STATE/landed.log` (docs/protocol.md §8), the
//! verdict keys the gate reads, and who a `landed` note goes to.

use std::sync::LazyLock;

use hgl_ai_proto::{Family, ReviewReceipt, Seat, TreeRecord};
use regex::Regex;

use crate::short;

pub const KEY_MAIN: &str = "main";
pub const KEY_LANDED: &str = "landed.log";
pub const KEY_PRS: &str = "prs";
pub const KEY_HOLD: &str = "hold";
/// The claimable queue and open issues by label (`backlog::Backlog`).
pub const KEY_BACKLOG: &str = "backlog";
/// Lines of `landed.log` kept: far below the bus's 1 MiB value limit; GitHub holds the rest.
pub const LANDED_KEEP: usize = 2000;

/// `<UTC> <repo> #<pr> <merge sha>`
pub fn landed_line(stamp: &str, repo: &str, pr: u64, merge: &str) -> String {
    format!("{stamp} {repo} #{pr} {merge}")
}

/// `updated <UTC>`, `<repo> main <sha>` per repository, then `<wrapper> pins <path> <sha>`.
pub fn render_main(
    stamp: &str,
    mains: &[(String, String)],
    wrapper: &str,
    pins: &[(String, String)],
) -> String {
    let mut s = format!("updated {stamp}\n");
    for (repo, sha) in mains {
        s.push_str(&format!("{repo} main {sha}\n"));
    }
    for (path, sha) in pins {
        s.push_str(&format!("{wrapper} pins {path} {sha}\n"));
    }
    s
}

/// State key of one local build verdict: `verdict.<repo>.<pr>.<name>`.
pub fn verdict_key(repo: &str, pr: u64, name: &str) -> String {
    format!("verdict.{repo}.{pr}.{name}")
}

/// A verdict value `<head> <main> pass|fail [detail]` for this head on this main:
/// `Some(true)` pass, `Some(false)` fail, `None` absent or for another pair.
pub fn verdict_for(value: Option<&str>, head: &str, main: &str) -> Option<(bool, String)> {
    let v = value?.trim();
    let mut w = v.split_whitespace();
    let (h, m, r) = (w.next()?, w.next()?, w.next()?);
    if h != head || m != main {
        return None;
    }
    match r {
        "pass" => Some((true, v.to_string())),
        "fail" => Some((false, v.to_string())),
        _ => None,
    }
}

static ISSUE_IN_BRANCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^q?([0-9]+)(?:[-_/]|$)").unwrap());

/// The issue a branch is for: `<prefix><issue>-<slug>` (docs/design.md §5), or the older
/// `<prefix>q<issue>-<slug>`.
pub fn issue_from_branch(branch: &str, prefix: &str) -> Option<u64> {
    let rest = branch.strip_prefix(prefix)?;
    ISSUE_IN_BRANCH.captures(rest)?[1].parse().ok()
}

/// The seats a `landed` note should reach besides `all`: the implementor trees registered
/// for this branch or issue; else a known seat named as the branch's last part
/// (`codex/q695-helper-w5`); else the seat that signed the `Ready` comment.
pub fn author_seats(
    trees: &[TreeRecord],
    repo: &str,
    branch: &str,
    issue: u64,
    known: &[String],
    signers: &[String],
) -> Vec<Seat> {
    fn add(out: &mut Vec<Seat>, s: &str) {
        if let Ok(seat) = Seat::new(s)
            && !seat.is_broadcast()
            && !out.contains(&seat)
        {
            out.push(seat);
        }
    }
    let mut out: Vec<Seat> = Vec::new();
    for t in trees.iter().filter(|t| t.task.repo == repo && !t.review) {
        if t.branch.as_deref() == Some(branch) || t.task.issue == issue {
            add(&mut out, t.seat.as_str());
        }
    }
    if out.is_empty()
        && let Some(last) = branch.rsplit(['-', '/']).next()
        && known.iter().any(|k| k == last)
    {
        add(&mut out, last);
    }
    if out.is_empty() {
        for s in signers
            .iter()
            .filter(|s| known.is_empty() || known.contains(s))
        {
            add(&mut out, s);
        }
    }
    out
}

/// The task's author families (ROLE-11): the families recorded in the implementor trees of
/// `repo` registered for this branch or issue (`HGL_TREES`, written by the bridges).
pub fn author_families(trees: &[TreeRecord], repo: &str, branch: &str, issue: u64) -> Vec<Family> {
    let mut out: Vec<Family> = trees
        .iter()
        .filter(|t| t.task.repo == repo && !t.review)
        .filter(|t| t.branch.as_deref() == Some(branch) || t.task.issue == issue)
        .flat_map(|t| t.families.iter().copied())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The review receipt (PROTO-19) a `receipt` note's body carries: the body as JSON, or the
/// JSON object inside it (a fenced block, a line of text around it). `None` for a body that
/// holds none (a desk receipt, PROTO-18).
pub fn parse_receipt(body: &str) -> Option<ReviewReceipt> {
    if let Ok(r) = serde_json::from_str(body.trim()) {
        return Some(r);
    }
    let (a, z) = (body.find('{')?, body.rfind('}')?);
    (a < z).then(|| serde_json::from_str(&body[a..=z]).ok())?
}

/// Body of a `landed` note (PROTO-11).
pub fn landed_body(
    runtime: &str,
    repo: &str,
    pr: u64,
    merge: &str,
    what: &str,
    closes: Option<u64>,
) -> String {
    let closes = closes.map(|i| format!(" Closes #{i}.")).unwrap_or_default();
    let what = if what.is_empty() {
        String::new()
    } else {
        format!(" ({what})")
    };
    format!(
        "[{runtime}] Landed on {repo} main: #{pr} at {}{what}.{closes} Heads are in state/main.\n",
        short(merge)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hgl_ai_proto::TaskKey;

    #[test]
    fn landed_log() {
        let l = landed_line("20260105T120000Z", "alpha", 12, &"c".repeat(40));
        assert_eq!(l, format!("20260105T120000Z alpha #12 {}", "c".repeat(40)));
        assert!(crate::store::newest_lines(&"x\n".repeat(LANDED_KEEP + 1), LANDED_KEEP).is_some());
    }

    #[test]
    fn main_state() {
        let m = render_main(
            "20260105T120000Z",
            &[
                ("alpha".into(), "a".repeat(40)),
                ("beta".into(), "b".repeat(40)),
            ],
            "wrap",
            &[("alpha".into(), "a".repeat(40))],
        );
        let want = format!(
            "updated 20260105T120000Z\nalpha main {a}\nbeta main {b}\nwrap pins alpha {a}\n",
            a = "a".repeat(40),
            b = "b".repeat(40)
        );
        assert_eq!(m, want);
    }

    #[test]
    fn verdicts() {
        let (h, m) = ("h".repeat(40), "m".repeat(40));
        assert_eq!(verdict_key("alpha", 7, "linux"), "verdict.alpha.7.linux");
        assert!(
            hgl_ai_proto::check_state_key(&verdict_key("hgl-compiler", 1037, "windows")).is_ok()
        );
        assert_eq!(
            verdict_for(Some(&format!("{h} {m} pass")), &h, &m).map(|v| v.0),
            Some(true)
        );
        assert_eq!(
            verdict_for(Some(&format!("{h} {m} fail tests 3")), &h, &m).map(|v| v.0),
            Some(false)
        );
        assert_eq!(
            verdict_for(Some(&format!("{h} {} pass", "o".repeat(40))), &h, &m),
            None,
            "another main"
        );
        assert_eq!(verdict_for(Some("junk"), &h, &m), None);
        assert_eq!(verdict_for(None, &h, &m), None);
    }

    #[test]
    fn branches() {
        assert_eq!(issue_from_branch("codex/980-fix-x", "codex/"), Some(980));
        assert_eq!(
            issue_from_branch("codex/q695-module-helper-w5", "codex/"),
            Some(695)
        );
        assert_eq!(
            issue_from_branch("codex/pins-compiler-1e6311b6", "codex/"),
            None
        );
        assert_eq!(issue_from_branch("dependabot/cargo/x", "codex/"), None);
    }

    fn tree(seat: &str, repo: &str, issue: u64, branch: &str, review: bool) -> TreeRecord {
        TreeRecord {
            host: "h".into(),
            seat: Seat::new(seat).unwrap(),
            task: TaskKey {
                repo: repo.into(),
                issue,
            },
            review,
            path: "p".into(),
            branch: Some(branch.into()),
            created_at: "x".into(),
            size_bytes: 0,
            families: Vec::new(),
            class: None,
        }
    }

    #[test]
    fn authors_and_receipts() {
        let mut a = tree("w1", "alpha", 980, "codex/q980-x", false);
        a.families = vec![Family::Gpt];
        let mut b = tree("w2", "alpha", 980, "", false);
        b.families = vec![Family::Claude, Family::Gpt];
        let mut r = tree("r1", "alpha", 980, "", true);
        r.families = vec![Family::Gemini];
        let mut other = tree("w3", "beta", 980, "codex/q980-x", false);
        other.families = vec![Family::Gemini];
        let trees = vec![a, b, r, other];
        assert_eq!(
            author_families(&trees, "alpha", "codex/q980-x", 980),
            [Family::Claude, Family::Gpt],
            "the implementor trees of the task; a review tree is not an author"
        );
        assert!(author_families(&trees, "alpha", "codex/q7-y", 7).is_empty());

        let json = r#"{"repo":"alpha","pr":7,"head":"h","main":"m","reviewer":"r1","family":"gemini","model":"gemini-4-neon","verdict":"clean","evidence":"e"}"#;
        let r = parse_receipt(json).unwrap();
        assert_eq!(
            (r.pr, r.family, r.limitations.as_str()),
            (7, Family::Gemini, "")
        );
        let wrapped = format!("[agy] Receipt: alpha#7\n\n```json\n{json}\n```\n");
        assert_eq!(parse_receipt(&wrapped), Some(r));
        assert_eq!(
            parse_receipt("[desk] Sets: alpha#7 at 1f461fa7 on main 0a0a0a0a, exit 0: r"),
            None
        );
        assert_eq!(parse_receipt(r#"{"repo":"alpha"}"#), None);
        assert_eq!(parse_receipt(&json.replace("clean", "maybe")), None);
    }

    #[test]
    fn landed_recipients() {
        let known: Vec<String> = ["w1", "w5", "r1"].map(String::from).to_vec();
        let trees = vec![
            tree("w1", "alpha", 980, "codex/980-x", false),
            tree("r1", "alpha", 981, "", true),
            tree("w9", "beta", 980, "", false),
        ];
        let seats = |v: Vec<Seat>| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            seats(author_seats(
                &trees,
                "alpha",
                "codex/980-x",
                980,
                &known,
                &[]
            )),
            ["w1"]
        );
        assert_eq!(
            seats(author_seats(
                &[],
                "alpha",
                "codex/q695-helper-w5",
                695,
                &known,
                &[]
            )),
            ["w5"]
        );
        assert_eq!(
            seats(author_seats(
                &[],
                "alpha",
                "codex/695-helper",
                695,
                &known,
                &["w5".into()]
            )),
            ["w5"]
        );
        assert!(
            author_seats(
                &[],
                "alpha",
                "codex/695-helper",
                695,
                &known,
                &["w8".into()]
            )
            .is_empty()
        );
        assert!(author_seats(&[], "alpha", "codex/695-x", 695, &known, &["all".into()]).is_empty());
    }

    #[test]
    fn landed_note_body() {
        let b = landed_body(
            "claude",
            "alpha",
            12,
            &"c".repeat(40),
            "format patterns",
            Some(980),
        );
        assert_eq!(
            b,
            "[claude] Landed on alpha main: #12 at cccccccc (format patterns). Closes #980. Heads are in state/main.\n"
        );
        assert_eq!(
            landed_body("claude", "a", 1, "abc", "", None),
            "[claude] Landed on a main: #1 at abc. Heads are in state/main.\n"
        );
    }
}
