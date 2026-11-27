//! `gh approve` and `gh sendback`: the controller's verdict on one exact head (ROLE-7). The
//! verdict is refused when the pull request has moved; otherwise it is posted as one GitHub
//! comment (PROTO-1: the record) and then sent as an `approved` or `sendback` note to the
//! author's seat.

use hgl_ai_bus::Bus;
use hgl_ai_proto::{Kind, Note, Seat};

use crate::exec::{self, Gh};
use crate::{Result, Settings, gate, invalid, land, short, state};

#[derive(Debug, Clone)]
pub struct VerdictOpts {
    /// `Approved` or `Sendback`.
    pub kind: Kind,
    /// Seats to send to; else the author's, found as for `landed`.
    pub to: Vec<Seat>,
    /// The task's issue; else the one in the branch name, else the pull request's number.
    pub issue: Option<u64>,
    pub body: String,
    pub comment: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictOutcome {
    Sent(String),
    Would(String),
    /// The verdict does not apply: the line says why.
    Refused(String),
}

/// The body with a first line that names the runtime and the action (PROTO-11), unless it
/// has one already.
pub fn with_first_line(
    body: &str,
    runtime: &str,
    kind: Kind,
    repo: &str,
    pr: u64,
    head: &str,
) -> String {
    let body = body.trim_end();
    if body.starts_with('[') {
        return format!("{body}\n");
    }
    let action = if kind == Kind::Approved {
        "Approved"
    } else {
        "Sent back"
    };
    format!(
        "[{runtime}] {action}: {repo}#{pr} at {}\n\n{body}\n",
        short(head)
    )
}

pub async fn verdict<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: Option<&Bus>,
    repo: &str,
    number: u64,
    head: &str,
    o: &VerdictOpts,
) -> Result<VerdictOutcome> {
    hgl_ai_proto::check_head(head)?;
    if !matches!(o.kind, Kind::Approved | Kind::Sendback) {
        return Err(invalid(format!(
            "a verdict is approved or sendback, not {}",
            o.kind
        )));
    }
    if o.body.trim().is_empty() {
        return Err(invalid("empty verdict body"));
    }
    let pr = gate::fetch_pr(gh, s, repo, number).await?;
    if pr.merged || pr.state != "open" {
        return Ok(VerdictOutcome::Refused(format!(
            "{repo} #{number} is {}",
            if pr.merged { "merged" } else { "closed" }
        )));
    }
    if pr.head != head {
        return Ok(VerdictOutcome::Refused(format!(
            "{repo} #{number} head {} is not {}: head moved, no verdict",
            short(&pr.head),
            short(head)
        )));
    }
    let issue = o
        .issue
        .or_else(|| state::issue_from_branch(&pr.branch, &s.branch_prefix))
        .unwrap_or(number);
    let body = with_first_line(&o.body, &s.controller_runtime, o.kind, repo, number, head);
    let bus = match bus {
        Some(b) if !o.dry_run => b,
        _ => {
            let to = if o.to.is_empty() {
                "the author's seat".to_string()
            } else {
                o.to.iter()
                    .map(Seat::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let comment = if o.comment {
                format!("comment on {repo}#{number} and ")
            } else {
                String::new()
            };
            return Ok(VerdictOutcome::Would(format!(
                "would {comment}send {} for {repo}-{issue} at {} to {to}",
                o.kind,
                short(head)
            )));
        }
    };
    let to = if o.to.is_empty() {
        land::author_seats(gh, s, bus, repo, &pr.branch, number, Some(issue)).await?
    } else {
        o.to.clone()
    };
    if to.is_empty() {
        return Err(invalid(format!(
            "no seat found for {repo}#{number} ({}); give --to",
            pr.branch
        )));
    }
    if o.comment {
        exec::write(
            gh,
            "POST",
            &format!("repos/{}/issues/{number}/comments", s.full(repo)),
            &[("body", &body)],
        )
        .await?;
    }
    let mut sent = Vec::new();
    for seat in &to {
        let mut n = Note::new(s.controller.clone(), seat.clone(), o.kind, body.clone());
        n.repo = Some(repo.to_string());
        n.issue = issue;
        n.head = Some(head.to_string());
        bus.send(&n).await?;
        sent.push(seat.to_string());
    }
    Ok(VerdictOutcome::Sent(format!(
        "{} {repo}#{number} at {}: sent to {}{}",
        o.kind,
        short(head),
        sent.join(", "),
        if o.comment { ", commented" } else { "" }
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::fake::FakeGh;
    use crate::testutil::{fixture, settings};

    const HEAD: &str = "1f461fa77bba922fe81383df4c475feb58b7c233";

    #[test]
    fn first_lines() {
        let b = with_first_line(
            "Read in full; probes pass.",
            "claude",
            Kind::Approved,
            "alpha",
            7,
            HEAD,
        );
        assert_eq!(
            b,
            "[claude] Approved: alpha#7 at 1f461fa7\n\nRead in full; probes pass.\n"
        );
        let b = with_first_line(
            "[claude] Sent back: two findings\n",
            "claude",
            Kind::Sendback,
            "alpha",
            7,
            HEAD,
        );
        assert_eq!(b, "[claude] Sent back: two findings\n");
    }

    fn opts(kind: Kind) -> VerdictOpts {
        VerdictOpts {
            kind,
            to: vec![],
            issue: None,
            body: "ok".into(),
            comment: true,
            dry_run: true,
        }
    }

    #[tokio::test]
    async fn exact_head_or_nothing() {
        let s = settings();
        let gh = crate::exec::ReadOnly(FakeGh::default().on("pulls/7", &fixture("pr-open.json")));
        let r = verdict(&gh, &s, None, "alpha", 7, HEAD, &opts(Kind::Approved))
            .await
            .unwrap();
        assert_eq!(
            r,
            VerdictOutcome::Would("would comment on alpha#7 and send approved for alpha-980 at 1f461fa7 to the author's seat".into())
        );
        let moved = verdict(
            &gh,
            &s,
            None,
            "alpha",
            7,
            &"e".repeat(40),
            &opts(Kind::Sendback),
        )
        .await
        .unwrap();
        assert!(matches!(moved, VerdictOutcome::Refused(l) if l.contains("head moved")));
        let merged =
            crate::exec::ReadOnly(FakeGh::default().on("pulls/7", &fixture("pr-merged.json")));
        assert!(matches!(
            verdict(&merged, &s, None, "alpha", 7, HEAD, &opts(Kind::Approved))
                .await
                .unwrap(),
            VerdictOutcome::Refused(_)
        ));
        assert!(
            verdict(&gh, &s, None, "alpha", 7, HEAD, &opts(Kind::Landed))
                .await
                .is_err()
        );
    }
}
