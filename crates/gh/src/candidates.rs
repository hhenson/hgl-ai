//! Landing candidates (docs/work.md WORK-6): the open pull requests the coordinator may land
//! itself, in Ready order. A candidate is in a repository that opts in
//! (`[github.gate.<repo>] coordinator_lands`, never one the owner merges), is not a draft,
//! and its latest note comment is `ready` or `approved` with a `Ready` comment at its head.
//! Ready order is the time of that `Ready` comment, oldest first; equal times keep the
//! round's order (repositories as configured, then by number).
//!
//! The gates are not judged here: each candidate in turn goes through them on the `main` it
//! merges onto (ROLE-10).

use chrono::{DateTime, Utc};

use crate::Settings;
use crate::bot::NoteState;
use crate::table::{PrInfo, parse_time};

/// One pull request the coordinator may land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub repo: String,
    pub number: u64,
    pub head: String,
    pub note: NoteState,
    /// When it became Ready at `head`.
    pub ready_at: DateTime<Utc>,
    pub url: String,
}

/// The landing candidates among one round's pull requests, in Ready order. Empty when no
/// repository opts in or nothing is Ready.
pub fn select(s: &Settings, infos: &[PrInfo]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = infos
        .iter()
        .filter(|i| s.coordinator_lands(&i.row.repo) && !i.row.draft)
        .filter_map(|i| {
            let note = i
                .note
                .filter(|n| matches!(n, NoteState::Ready | NoteState::Approved))?;
            Some(Candidate {
                repo: i.row.repo.clone(),
                number: i.row.number,
                head: i.row.head.clone(),
                note,
                ready_at: parse_time(i.ready_at.as_deref()?)?,
                url: i.url.clone(),
            })
        })
        .collect();
    out.sort_by_key(|c| c.ready_at);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::BotState;
    use crate::table::{Checks, Row};
    use crate::testutil::{settings, with_coordinator_lands};

    fn pr(repo: &str, number: u64, note: Option<NoteState>, ready_at: Option<&str>) -> PrInfo {
        PrInfo {
            row: Row {
                repo: repo.into(),
                number,
                head: format!("{number:0>40}"),
                draft: false,
                bot: BotState::Clean,
                unresolved: 0,
                checks: Checks::Pass,
                updated: "20261128T120000Z".into(),
            },
            note,
            ready_at: ready_at.map(String::from),
            url: format!("https://github.com/o/{repo}/pull/{number}"),
            review_url: None,
        }
    }

    fn picked(c: &[Candidate]) -> Vec<(&str, u64)> {
        c.iter().map(|c| (c.repo.as_str(), c.number)).collect()
    }

    const READY: Option<NoteState> = Some(NoteState::Ready);
    const APPROVED: Option<NoteState> = Some(NoteState::Approved);

    #[test]
    fn only_hgl_ai_by_default() {
        let s = settings();
        assert!(s.coordinator_lands("hgl-ai"));
        for r in [
            "hgl-compiler",
            "hgl_spec",
            "hgl_core",
            "hgl-project",
            "homebrew-hgraph",
            "x",
        ] {
            assert!(!s.coordinator_lands(r), "{r}");
        }
        // A repository the owner merges stays off even when its entry says otherwise.
        let s = with_coordinator_lands(settings(), &["hgl_spec", "hgl_core", "hgl-compiler"], true);
        assert!(!s.coordinator_lands("hgl_spec") && !s.coordinator_lands("hgl_core"));
        assert!(s.coordinator_lands("hgl-compiler"));
        let s = with_coordinator_lands(settings(), &["hgl-ai"], false);
        assert!(!s.coordinator_lands("hgl-ai"));
    }

    #[test]
    fn several_ready_land_in_ready_order() {
        let s = settings();
        let t = |m: u32| format!("2026-11-28T10:{m:02}:00Z");
        let (t1, t3, t4) = (t(1), t(3), t(4));
        let infos = vec![
            // In the round's order: by number, not by readiness.
            pr("hgl-ai", 120, APPROVED, Some(&t3)),
            pr("hgl-ai", 121, READY, Some(&t1)),
            pr("hgl-ai", 122, READY, Some(&t4)),
            pr("hgl-ai", 123, READY, Some("20261128T100200Z")),
            // Equal times keep the round's order.
            pr("hgl-ai", 124, READY, Some(&t4)),
        ];
        let c = select(&s, &infos);
        assert_eq!(
            picked(&c),
            [
                ("hgl-ai", 121),
                ("hgl-ai", 123),
                ("hgl-ai", 120),
                ("hgl-ai", 122),
                ("hgl-ai", 124)
            ]
        );
        assert_eq!(c[0].ready_at, parse_time(&t1).unwrap());
        assert_eq!(c[0].head, format!("{:0>40}", 121));
        assert_eq!(c[2].note, NoteState::Approved);
        assert_eq!(c[0].url, "https://github.com/o/hgl-ai/pull/121");
        assert!(
            c[1].ready_at < c[2].ready_at,
            "the compact form is read too"
        );
    }

    #[test]
    fn what_is_left_out() {
        let s = settings();
        let at = Some("2026-11-28T10:00:00Z");
        let mut draft = pr("hgl-ai", 2, READY, at);
        draft.row.draft = true;
        let infos = vec![
            pr("hgl-ai", 1, READY, at),
            draft,
            pr("hgl-ai", 3, Some(NoteState::Sendback), at),
            pr("hgl-ai", 4, Some(NoteState::Blocked), at),
            pr("hgl-ai", 5, Some(NoteState::Landed), at),
            pr("hgl-ai", 6, None, None),
            // Ready, but not at its head.
            pr("hgl-ai", 7, READY, None),
            pr("hgl-ai", 8, READY, Some("not a time")),
            // Repositories that do not opt in, and ones the owner merges.
            pr("hgl-compiler", 9, READY, at),
            pr("hgl-project", 10, APPROVED, at),
            pr("homebrew-hgraph", 11, READY, at),
            pr("hgl_spec", 12, READY, at),
            pr("hgl_core", 13, APPROVED, at),
        ];
        assert_eq!(picked(&select(&s, &infos)), [("hgl-ai", 1)]);
        // Forcing the spec repositories on changes nothing; an opted-in compiler joins.
        let s = with_coordinator_lands(settings(), &["hgl_spec", "hgl_core", "hgl-compiler"], true);
        assert_eq!(
            picked(&select(&s, &infos)),
            [("hgl-ai", 1), ("hgl-compiler", 9)]
        );
    }

    #[test]
    fn an_empty_queue() {
        let s = settings();
        assert!(select(&s, &[]).is_empty());
        let none_ready = [
            pr("hgl-ai", 1, None, None),
            pr("hgl-ai", 2, Some(NoteState::Sendback), None),
        ];
        assert!(select(&s, &none_ready).is_empty());
        // Nothing opts in: Ready pull requests are left to the controller.
        let s = with_coordinator_lands(settings(), &["hgl-ai"], false);
        let ready = [pr("hgl-ai", 1, READY, Some("2026-11-28T10:00:00Z"))];
        assert!(select(&s, &ready).is_empty());
    }

    #[test]
    fn from_a_round() {
        let mut s = with_coordinator_lands(settings(), &["alpha"], true);
        s.repos = ["alpha", "beta", "gamma"].map(String::from).to_vec();
        let (infos, ..) = crate::table::parse_batch(
            &crate::testutil::json("coord-batch.json"),
            &s.repos,
            &s.review_bot,
            "20260105T120000Z",
        )
        .unwrap();
        // alpha#12 is approved with no Ready comment read; #13 has no note; #14 is a draft.
        let c = select(&s, &infos);
        assert_eq!(picked(&c), [("alpha", 11)]);
        assert_eq!(c[0].ready_at, parse_time("2026-01-05T09:00:00Z").unwrap());
    }
}
