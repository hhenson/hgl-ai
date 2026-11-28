//! Landing candidates (docs/work.md WORK-6): the open pull requests the coordinator may land
//! itself, in Ready order. A candidate is in a repository that opts in
//! (`[github.gate.<repo>] coordinator_lands`, never one the owner merges), is not a draft,
//! its latest note comment is `ready` or `approved`, and it is Ready at its head: a `Ready`
//! comment at the head that no later note comment cleared ([`crate::bot::ready_mark`]), or
//! else the latest `ready` note to the controller at that exact head (HGL_NOTES), when it is
//! later than any comment that cleared Ready. Ready order is the time of that comment or
//! note, oldest first; equal times keep the round's order (repositories as configured, then
//! by number).
//!
//! The gates are not judged here: each candidate in turn goes through them on the `main` it
//! merges onto (ROLE-10).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use hgl_ai_bus::Bus;
use hgl_ai_proto::{Kind as NoteKind, Note};

use crate::bot::NoteState;
use crate::table::{PrInfo, parse_time};
use crate::{Result, Settings};

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

/// The latest `ready` note's time for each repository and head (lowercase).
pub type ReadyNotes = BTreeMap<(String, String), DateTime<Utc>>;

/// The `ready` notes among `notes`, by repository and exact head: the latest of each.
pub fn ready_notes<'a>(notes: impl IntoIterator<Item = &'a Note>) -> ReadyNotes {
    let mut out = ReadyNotes::new();
    for n in notes {
        let (NoteKind::Ready, Some(repo), Some(head)) = (n.kind, &n.repo, &n.head) else {
            continue;
        };
        let Some(t) = parse_time(&n.created_at) else {
            continue;
        };
        let e = out
            .entry((repo.clone(), head.to_ascii_lowercase()))
            .or_insert(t);
        *e = (*e).max(t);
    }
    out
}

/// Notes read per page of the controller's notes.
const NOTES_PAGE: usize = 1000;

/// The `ready` notes to the controller on the hub (HGL_NOTES keeps 14 days).
pub async fn read_ready_notes(bus: &Bus, s: &Settings) -> Result<ReadyNotes> {
    let mut notes = Vec::new();
    let mut from = 1;
    loop {
        let (got, last) = bus
            .read_page(std::slice::from_ref(&s.controller), from, NOTES_PAGE)
            .await?;
        let Some(last) = last else {
            return Ok(ready_notes(&notes));
        };
        from = last + 1;
        notes.extend(
            got.into_iter()
                .map(|sn| sn.note)
                .filter(|n| n.kind == NoteKind::Ready),
        );
    }
}

/// When `i` became Ready at its head: its standing `Ready` comment, or else its `ready` note
/// when later than the comment that last cleared Ready.
fn ready_time(i: &PrInfo, notes: &ReadyNotes) -> Option<DateTime<Utc>> {
    if let Some(t) = i.ready_at.as_deref().and_then(parse_time) {
        return Some(t);
    }
    let t = *notes.get(&(i.row.repo.clone(), i.row.head.to_ascii_lowercase()))?;
    match i.ready_cleared.as_deref() {
        None => Some(t),
        Some(c) => parse_time(c).filter(|c| t > *c).map(|_| t),
    }
}

/// The landing candidates among one round's pull requests, in Ready order. Empty when no
/// repository opts in or nothing is Ready.
pub fn select(s: &Settings, infos: &[PrInfo], notes: &ReadyNotes) -> Vec<Candidate> {
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
                ready_at: ready_time(i, notes)?,
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

    /// The test settings with hgl-ai opted in, as the hub's configuration does.
    fn hgl_ai_on() -> Settings {
        with_coordinator_lands(settings(), &["hgl-ai"], true)
    }

    fn none() -> ReadyNotes {
        ReadyNotes::new()
    }

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
            ready_cleared: None,
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
    fn off_by_default() {
        let s = settings();
        for r in [
            "hgl-ai",
            "hgl-compiler",
            "hgl_spec",
            "hgl_core",
            "hgl-project",
            "homebrew-hgraph",
            "x",
        ] {
            assert!(!s.coordinator_lands(r), "{r}");
        }
        assert!(hgl_ai_on().coordinator_lands("hgl-ai"));
        // A repository the owner merges stays off even when its entry says otherwise, in
        // any letter case.
        let forced = [
            "hgl_spec",
            "hgl_core",
            "HGL_SPEC",
            "Hgl_Core",
            "hgl-compiler",
        ];
        let s = with_coordinator_lands(settings(), &forced, true);
        for r in &forced[..4] {
            assert!(!s.coordinator_lands(r), "{r}");
        }
        assert!(s.coordinator_lands("hgl-compiler"));
        let s = with_coordinator_lands(hgl_ai_on(), &["hgl-ai"], false);
        assert!(!s.coordinator_lands("hgl-ai"));
    }

    #[test]
    fn several_ready_land_in_ready_order() {
        let s = hgl_ai_on();
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
        let c = select(&s, &infos, &none());
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
        let s = hgl_ai_on();
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
        assert_eq!(picked(&select(&s, &infos, &none())), [("hgl-ai", 1)]);
        // Forcing the spec repositories on changes nothing; an opted-in compiler joins.
        let s = with_coordinator_lands(s, &["hgl_spec", "hgl_core", "hgl-compiler"], true);
        assert_eq!(
            picked(&select(&s, &infos, &none())),
            [("hgl-ai", 1), ("hgl-compiler", 9)]
        );
    }

    #[test]
    fn an_empty_queue() {
        let s = hgl_ai_on();
        assert!(select(&s, &[], &none()).is_empty());
        let none_ready = [
            pr("hgl-ai", 1, None, None),
            pr("hgl-ai", 2, Some(NoteState::Sendback), None),
        ];
        assert!(select(&s, &none_ready, &none()).is_empty());
        // Nothing opts in (the default): Ready pull requests are left to the controller.
        let ready = [pr("hgl-ai", 1, READY, Some("2026-11-28T10:00:00Z"))];
        assert!(select(&settings(), &ready, &none()).is_empty());
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
        let c = select(&s, &infos, &none());
        assert_eq!(picked(&c), [("alpha", 11)]);
        assert_eq!(c[0].ready_at, parse_time("2026-01-05T09:00:00Z").unwrap());
    }

    fn ready_note(repo: &str, head: &str, at: &str) -> Note {
        let mut n = Note::new(
            hgl_ai_proto::Seat::new("w1").unwrap(),
            hgl_ai_proto::Seat::new("controller").unwrap(),
            NoteKind::Ready,
            "[codex] Ready",
        );
        n.repo = Some(repo.into());
        n.issue = 1;
        n.head = Some(head.into());
        n.created_at = at.into();
        n
    }

    #[test]
    fn ready_notes_keep_the_latest_per_head() {
        let h = format!("{:0>40}", 1);
        let mut other = ready_note("hgl-ai", &h, "20261128T100000Z");
        other.kind = NoteKind::Receipt;
        let mut bad = ready_note("hgl-ai", &h, "not a time");
        bad.issue = 2;
        let notes = [
            ready_note("hgl-ai", &h, "20261128T090000Z"),
            ready_note("hgl-ai", &h.to_uppercase(), "20261128T093000Z"),
            ready_note("hgl-ai", &h, "20261128T080000Z"),
            ready_note("hgl-compiler", &h, "20261128T110000Z"),
            other,
            bad,
        ];
        let r = ready_notes(&notes);
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(
            r[&("hgl-ai".to_string(), h.clone())],
            parse_time("20261128T093000Z").unwrap()
        );
        assert_eq!(
            r[&("hgl-compiler".to_string(), h)],
            parse_time("20261128T110000Z").unwrap()
        );
    }

    #[test]
    fn a_ready_note_outside_the_comments_read() {
        let s = hgl_ai_on();
        let head = |n: u64| format!("{n:0>40}");
        let notes = ready_notes(&[
            ready_note("hgl-ai", &head(1), "20261128T090000Z"),
            ready_note("hgl-ai", &head(2), "20261128T090000Z"),
            ready_note("hgl-ai", &head(3), "20261128T090000Z"),
            ready_note("hgl-ai", &head(5).to_uppercase(), "20261128T070000Z"),
            // At another head than #6's.
            ready_note("hgl-ai", &head(66), "20261128T070000Z"),
        ]);
        // Approved, its Ready comment not among those read: the note places it.
        let one = pr("hgl-ai", 1, APPROVED, None);
        // Sent back after the note, then approved at the same head: not Ready.
        let mut two = pr("hgl-ai", 2, APPROVED, None);
        two.ready_cleared = Some("2026-11-28T09:30:00Z".into());
        // Cleared before the note: the note stands.
        let mut three = pr("hgl-ai", 3, APPROVED, None);
        three.ready_cleared = Some("2026-11-28T08:00:00Z".into());
        // A Ready comment that stands wins over the note.
        let four = pr("hgl-ai", 4, READY, Some("2026-11-28T08:30:00Z"));
        // A note at the head in another letter case.
        let five = pr("hgl-ai", 5, APPROVED, None);
        let six = pr("hgl-ai", 6, APPROVED, None);
        // A note does not make a sent-back pull request Ready.
        let seven = pr("hgl-ai", 1, Some(NoteState::Sendback), None);
        let mut eight = pr("hgl-ai", 1, APPROVED, None);
        eight.row.repo = "hgl-compiler".into();
        let infos = [one, two, three, four, five, six, seven, eight];
        let c = select(&s, &infos, &notes);
        assert_eq!(
            picked(&c),
            [("hgl-ai", 5), ("hgl-ai", 4), ("hgl-ai", 1), ("hgl-ai", 3)]
        );
        assert_eq!(c[2].ready_at, parse_time("20261128T090000Z").unwrap());
        // Without the notes only the standing comment counts.
        assert_eq!(picked(&select(&s, &infos, &none())), [("hgl-ai", 4)]);
    }
}
