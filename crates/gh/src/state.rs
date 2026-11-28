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
/// The last note of each task with a worktree (`tasks`).
pub const KEY_TASKS: &str = "tasks";
/// Lines of `landed.log` kept: far below the bus's 1 MiB value limit; GitHub holds the rest.
pub const LANDED_KEEP: usize = 2000;
/// Bytes of `landed.log` kept: half the bus's 1 MiB value limit, whatever the lines hold.
pub const LANDED_MAX_BYTES: usize = 512 << 10;

/// `<UTC> <repo> #<pr> <merge sha>`, then ` <title>` when there is one (one line).
pub fn landed_line(stamp: &str, repo: &str, pr: u64, merge: &str, title: &str) -> String {
    let title = crate::backlog::clean_title(title);
    let title = title.trim();
    if title.is_empty() {
        format!("{stamp} {repo} #{pr} {merge}")
    } else {
        format!("{stamp} {repo} #{pr} {merge} {title}")
    }
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

/// The seats a verdict (`approved`, `sendback`) should reach: the task's current seat, the
/// one whose implementor tree ran the newest `assign` or `sendback` (`TreeRecord::handoff_seq`,
/// else the note that made it, `seq`; a task moved to another seat and maybe back, PROTO-21);
/// else, with no tree recording a note, as [`author_seats`].
pub fn current_seats(
    trees: &[TreeRecord],
    repo: &str,
    branch: &str,
    issue: u64,
    known: &[String],
    signers: &[String],
) -> Vec<Seat> {
    let mine = |t: &&TreeRecord| {
        t.task.repo == repo
            && !t.review
            && (t.branch.as_deref() == Some(branch) || t.task.issue == issue)
    };
    let handoff = |t: &TreeRecord| t.handoff_seq.max(t.seq);
    let newest = trees.iter().filter(mine).filter_map(handoff).max();
    match newest {
        Some(n) => {
            let mut out: Vec<Seat> = Vec::new();
            for t in trees.iter().filter(mine).filter(|t| handoff(t) == Some(n)) {
                if !out.contains(&t.seat) {
                    out.push(t.seat.clone());
                }
            }
            out
        }
        None => author_seats(trees, repo, branch, issue, known, signers),
    }
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
        let l = landed_line("20260105T120000Z", "alpha", 12, &"c".repeat(40), "");
        assert_eq!(l, format!("20260105T120000Z alpha #12 {}", "c".repeat(40)));
        let l = landed_line("20260105T120000Z", "alpha", 12, "m", " Fix\tthe\nparser ");
        assert_eq!(l, "20260105T120000Z alpha #12 m Fix the parser");
        assert!(crate::progress::log_has(&l, "alpha", 12, Some("m")));
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
            class_seq: None,
            seq: None,
            handoff_seq: None,
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

    /// r3's major on hgl-ai PR 25: once a task moved seats, a verdict reaches only its current
    /// seat, the one whose tree ran the newest handoff; `landed` still reaches every seat.
    #[test]
    fn a_verdict_goes_to_the_current_seat_only() {
        let known: Vec<String> = ["w1", "w2"].map(String::from).to_vec();
        let at = |seat, seq| TreeRecord {
            seq,
            ..tree(seat, "alpha", 17, "codex/q17-x", false)
        };
        let seats = |v: Vec<Seat>| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let moved = vec![
            at("w1", Some(10)),
            at("w2", Some(40)),
            tree("r1", "alpha", 17, "", true),
        ];
        assert_eq!(
            seats(current_seats(
                &moved,
                "alpha",
                "codex/q17-x",
                17,
                &known,
                &[]
            )),
            ["w2"]
        );
        assert_eq!(
            seats(author_seats(
                &moved,
                "alpha",
                "codex/q17-x",
                17,
                &known,
                &[]
            )),
            ["w1", "w2"],
            "landed cleans up every tree"
        );
        // Records from bridges before `seq`: every seat, as before.
        let old = vec![at("w1", None), at("w2", None)];
        assert_eq!(
            seats(current_seats(&old, "alpha", "codex/q17-x", 17, &known, &[])),
            ["w1", "w2"]
        );
        // One seat on two hosts (a moved seat) is one seat.
        let mut other = at("w2", Some(40));
        other.host = "h2".into();
        let two = vec![at("w1", Some(10)), at("w2", Some(40)), other];
        assert_eq!(
            seats(current_seats(&two, "alpha", "codex/q17-x", 17, &known, &[])),
            ["w2"]
        );
    }

    /// hgl-ai#44: a task moved from w1 to w2 and back reuses w1's older tree; the verdict
    /// follows the newest handoff (w1's), not the newest tree (w2's).
    #[test]
    fn a_verdict_follows_the_newest_handoff_into_a_reused_tree() {
        let known: Vec<String> = ["w1", "w2"].map(String::from).to_vec();
        let at = |seat, seq, handoff_seq| TreeRecord {
            seq,
            handoff_seq,
            ..tree(seat, "alpha", 17, "codex/q17-x", false)
        };
        let seats = |v: &[TreeRecord]| {
            current_seats(v, "alpha", "codex/q17-x", 17, &known, &[])
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        };
        // assign w1 (10), sendback --to w2 (40), sendback --to w1 (60) into w1's tree.
        let back = vec![at("w1", Some(10), Some(60)), at("w2", Some(40), Some(40))];
        assert_eq!(seats(&back), ["w1"]);
        // Before the move back, w2 is current.
        let moved = vec![at("w1", Some(10), Some(10)), at("w2", Some(40), Some(40))];
        assert_eq!(seats(&moved), ["w2"]);
        // A record from a bridge before `handoff_seq` ranks by `seq`.
        let mixed = vec![at("w1", Some(10), Some(60)), at("w2", Some(40), None)];
        assert_eq!(seats(&mixed), ["w1"]);
        let mixed = vec![at("w1", Some(10), Some(30)), at("w2", Some(40), None)];
        assert_eq!(seats(&mixed), ["w2"]);
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
