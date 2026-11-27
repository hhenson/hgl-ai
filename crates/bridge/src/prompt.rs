//! The prompt of one turn: a short preamble, shared state when set, then each note.

use std::fmt::Write as _;

use hgl_ai_proto::{Family, Note, Role, Runtime, Seat};
use hgl_ai_tree::TaskFile;

/// Upper bound of the whole prompt, which goes on the agent's stdin: one note at the bus's
/// limit fits whole beside the preamble; the notes of a batch share it.
pub const MAX_PROMPT: usize = hgl_ai_bus::MAX_NOTE_BYTES + (64 << 10);

pub struct Context<'a> {
    pub host: &'a str,
    pub seat: &'a Seat,
    /// The controller's seat (`[github] controller`), named so replies reach it.
    pub controller: Option<&'a Seat>,
    pub role: Role,
    pub runtime: Runtime,
    pub family: Family,
    pub model: &'a str,
    /// The task's `task.json`; `None` for a turn in the seat's inbox directory.
    pub task: Option<&'a TaskFile>,
    /// `HGL_STATE/hold`, when present.
    pub hold: Option<&'a str>,
    /// `HGL_STATE/mode`, when present.
    pub mode: Option<&'a str>,
}

/// One delivered note: stream sequence, delivery count, the note.
pub type Item<'a> = (u64, u64, &'a Note);

pub fn build(c: &Context, notes: &[Item]) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "You are seat `{}` ({}) on host `{}`, running {} with {} model `{}`. The hgl-ai bridge started this turn for the notes below.",
        c.seat, c.role, c.host, c.runtime, c.family, c.model
    );
    match c.task {
        Some(t) if t.review => {
            let head = t.head.as_deref().unwrap_or("?");
            let main = t.main.as_deref().unwrap_or("?");
            let _ = writeln!(
                s,
                "Task: review of {}#{} at {head}, merged onto {} ({main}); the detached checkout is the current directory.",
                t.repo, t.issue, t.base
            );
            if !t.conflicts.is_empty() {
                let _ = writeln!(
                    s,
                    "The head does NOT merge onto main (conflicts: {}); the tree is at the head itself. Report that in your receipt.",
                    t.conflicts.join(", ")
                );
            }
        }
        Some(t) => {
            let _ = writeln!(
                s,
                "Task: {}#{} on branch `{}` from {}; the checkout is the current directory.",
                t.repo,
                t.issue,
                t.branch.as_deref().unwrap_or("?"),
                t.base
            );
        }
        None => {
            let _ = writeln!(s, "No task: this turn runs in the seat's inbox directory.");
        }
    }
    if let Some(c) = c.controller {
        let _ = writeln!(s, "The controller is seat `{c}`.");
    }
    s.push_str(
        "The `hgl-ai` skill describes the commands. Reply with `hgl-ai note send`.\n\
         Do not remove worktrees; the bridge does that. Keep all scratch inside $HGL_AI_TASK_DIR \
         (TMPDIR and CARGO_TARGET_DIR point there), never /tmp.\n",
    );
    if let Some(h) = c.hold {
        let _ = writeln!(
            s,
            "State hold: {} (GitHub API allowance spent; defer every GitHub call that can wait).",
            h.trim()
        );
    }
    if let Some(m) = c.mode {
        let m = m.trim();
        let what = if m == "drain" {
            " (finish in-flight work only; take nothing new)"
        } else {
            ""
        };
        let _ = writeln!(s, "State mode: {m}{what}.");
    }
    let budget = MAX_PROMPT.saturating_sub(s.len() + 200 * notes.len()) / notes.len().max(1);
    for (i, (seq, delivered, n)) in notes.iter().enumerate() {
        let _ = write!(
            s,
            "\n--- Note {} of {}: {} from {} to {}",
            i + 1,
            notes.len(),
            n.kind,
            n.sender,
            n.recipient
        );
        if let Some(r) = &n.repo {
            let _ = write!(s, ", {r}");
            if n.issue > 0 {
                let _ = write!(s, "#{}", n.issue);
            }
        }
        if let Some(h) = &n.head {
            let _ = write!(s, ", head {h}");
        }
        if let Some(b) = &n.base {
            let _ = write!(s, ", base {b}");
        }
        let _ = writeln!(
            s,
            ", created {} (seq {seq}, delivery {delivered}) ---",
            n.created_at
        );
        if n.sender.is_owner() {
            let who = match &n.by {
                Some(h) => format!("{h} (owner) says"),
                None => "The owner says".to_string(),
            };
            let _ = writeln!(
                s,
                "{who}, through the console (the hub accepts sender `owner` only there). \
                 Post a ruling once on its issue as `{}, <date>: \"<their words>\"`, naming \
                 the thread (OWN-4, OWN-10):",
                n.by.as_deref().unwrap_or("owner")
            );
        }
        s.push_str(&truncate(n.body.trim_end(), budget));
        s.push('\n');
    }
    s
}

fn truncate(body: &str, max: usize) -> String {
    if body.len() <= max {
        return body.to_string();
    }
    let mut end = max;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[... truncated by the bridge: {} of {} bytes shown]",
        &body[..end],
        end,
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hgl_ai_proto::Kind;
    use std::collections::BTreeMap;

    fn seat(s: &str) -> Seat {
        Seat::new(s).unwrap()
    }

    fn task(review: bool) -> TaskFile {
        TaskFile {
            seat: seat("w1"),
            repo: "hgl-compiler".into(),
            issue: 980,
            review,
            head: review.then(|| "a".repeat(40)),
            branch: (!review).then(|| "codex/980-fix".to_string()),
            base: "origin/main".into(),
            main: Some("b".repeat(40)),
            start: "c".repeat(40),
            created_at: "20261127T000000Z".into(),
            conflicts: if review {
                vec!["src/x.rs".into()]
            } else {
                vec![]
            },
            sessions: BTreeMap::new(),
            seq: None,
        }
    }

    fn ctx<'a>(t: Option<&'a TaskFile>, s: &'a Seat) -> Context<'a> {
        Context {
            host: "build-1",
            seat: s,
            controller: None,
            role: Role::Implementor,
            runtime: Runtime::Codex,
            family: Family::Gpt,
            model: "gpt-6.1-sol",
            task: t,
            hold: None,
            mode: None,
        }
    }

    #[test]
    fn preamble_state_and_notes() {
        let s = seat("w1");
        let t = task(false);
        let mut n = Note::new(
            seat("claude"),
            seat("w1"),
            Kind::Assign,
            "[claude] Assign: hgl-compiler#980 Fix it\n\nDetails.",
        );
        n.repo = Some("hgl-compiler".into());
        n.issue = 980;
        n.created_at = "20261127T041100Z".into();
        let q = Note::new(seat("r1"), seat("w1"), Kind::Question, "Why?");
        let mut c = ctx(Some(&t), &s);
        let boss = seat("boss");
        c.controller = Some(&boss);
        c.hold = Some("20261127T050000Z\n");
        c.mode = Some("drain");
        let p = build(&c, &[(42, 1, &n), (43, 2, &q)]);
        for needle in [
            "You are seat `w1` (implementor) on host `build-1`, running codex with gpt model `gpt-6.1-sol`.",
            "Task: hgl-compiler#980 on branch `codex/980-fix` from origin/main",
            "The controller is seat `boss`.",
            "`hgl-ai note send`",
            "Do not remove worktrees",
            "$HGL_AI_TASK_DIR",
            "State hold: 20261127T050000Z (GitHub",
            "State mode: drain (finish in-flight",
            "--- Note 1 of 2: assign from claude to w1, hgl-compiler#980, created 20261127T041100Z (seq 42, delivery 1) ---\n[claude] Assign: hgl-compiler#980 Fix it\n\nDetails.\n",
            "--- Note 2 of 2: question from r1 to w1, created",
            "(seq 43, delivery 2) ---\nWhy?\n",
        ] {
            assert!(p.contains(needle), "missing {needle:?} in\n{p}");
        }
        let plain = build(&ctx(None, &s), &[(1, 1, &q)]);
        assert!(plain.contains("No task"));
        assert!(!plain.contains("State "), "hold and mode only when set");
        assert!(!plain.contains("(owner) says") && !plain.contains("The owner says"));
        let mut o = Note::new(
            Seat::owner(),
            seat("w1"),
            Kind::Answer,
            "[owner] Answer: accept a",
        );
        o.by = Some("HPH".into());
        let p = build(&ctx(None, &s), &[(2, 1, &o)]);
        assert!(p.contains("answer from owner to w1"));
        assert!(p.contains("HPH (owner) says, through the console"), "{p}");
        assert!(p.contains("`HPH, <date>: \"<their words>\"`"));
        assert!(!p.contains("The owner says"));
        o.by = None;
        let p = build(&ctx(None, &s), &[(2, 1, &o)]);
        assert!(p.contains("The owner says, through the console"));
    }

    #[test]
    fn review_preamble_names_head_main_and_conflicts() {
        let s = seat("r1");
        let t = task(true);
        let n = Note::new(seat("claude"), seat("r1"), Kind::Note, "x");
        let p = build(&ctx(Some(&t), &s), &[(1, 1, &n)]);
        assert!(p.contains(&format!(
            "review of hgl-compiler#980 at {}, merged onto origin/main ({})",
            "a".repeat(40),
            "b".repeat(40)
        )));
        assert!(p.contains("does NOT merge onto main (conflicts: src/x.rs)"));
    }

    #[test]
    fn a_note_at_the_bus_limit_is_whole_and_larger_bodies_are_truncated() {
        let s = seat("w1");
        let body = "é".repeat(hgl_ai_bus::MAX_NOTE_BYTES / 2);
        let n = Note::new(seat("claude"), seat("w1"), Kind::Note, body.clone());
        let p = build(&ctx(None, &s), &[(1, 1, &n)]);
        assert!(p.contains(&format!("---\n{body}\n")));
        assert!(!p.contains("[... truncated by the bridge"));
        let n = Note::new(
            seat("claude"),
            seat("w1"),
            Kind::Note,
            "é".repeat(MAX_PROMPT),
        );
        let p = build(&ctx(None, &s), &[(1, 1, &n)]);
        assert!(p.len() <= MAX_PROMPT + 200, "{}", p.len());
        assert!(p.contains("[... truncated by the bridge"));
    }
}
