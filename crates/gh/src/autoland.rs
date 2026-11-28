//! The coordinator's landings (hgl-ai#123): in a repository that opts in
//! (`[github.gate.<repo>] coordinator_lands`), `gh coord run` lands each pull request whose
//! gates all pass, in Ready order ([`crate::candidates`]), one gate attempt per candidate per
//! round ([`land::merge_once`], the landing lock not waited for). The gate is the
//! controller's: exact head, current `main` and its merge test, an other-family clean
//! receipt, checks, the bot and threads. A local build verdict it lacks is asked of a builder
//! (BUILD-15) and read in a later round.
//!
//! First in each round, a landing record left in an opted-in repository
//! ([`crate::progress`]: merged, or its pull request no longer open) is finished by the same
//! call, so a coordinator stopped mid-landing resumes where it stopped.
//!
//! Every landing it does, or finishes, goes to the controller as a `note` (through the
//! coordinator's unsent queue, so a failed send is sent again) and to `decisions.log`; the
//! landing's own steps write `landed.log`, `main`, the `landed` notes and the comment. A
//! `PARTIAL` goes to the controller once. The wrapper's pins are left to the controller.

use std::collections::BTreeSet;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use hgl_ai_bus::Bus;
use hgl_ai_proto::{Kind as NoteKind, Note, utc_now_stamp};

use crate::builds::GateBuilds;
use crate::candidates::{self, Candidate, ReadyNotes};
use crate::exec::Gh;
use crate::gate::Kind;
use crate::land::{self, LandOpts, Landing};
use crate::table::{self, PrInfo, TableWord};
use crate::{Error, Result, Settings, progress, short};

/// How long one gate attempt may wait on a merge GitHub accepted but has not done; past
/// it the record is kept and a later round finishes the landing.
pub const MERGE_WAIT: Duration = Duration::from_secs(5 * 60);

/// The coordinator's landing state across rounds.
pub struct AutoLand {
    /// The gate builds asked for, so a verdict on its way is not asked for again.
    builds: Arc<GateBuilds>,
    /// `decisions.log` lines not written yet.
    decisions: Vec<String>,
    /// `PARTIAL` lines sent to the controller: each once.
    partial: BTreeSet<String>,
    /// Notes for the controller not taken yet ([`AutoLand::take_reports`]): kept whatever
    /// ends the round, so a landing done before a later failure is still reported.
    reports: Vec<Note>,
}

/// One pull request to try: a candidate, or a landing record to finish.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Try {
    repo: String,
    number: u64,
    head: String,
    /// The row's table rule decides first (a record's pull request has no row).
    candidate: bool,
}

impl AutoLand {
    pub fn new(s: &Settings) -> AutoLand {
        AutoLand {
            builds: Arc::new(GateBuilds::new(s.coord_seat.clone(), "coordinator")),
            decisions: Vec::new(),
            partial: BTreeSet::new(),
            reports: Vec::new(),
        }
    }

    /// The notes for the controller made so far; the caller queues them (`coord.alerts`).
    pub fn take_reports(&mut self) -> Vec<Note> {
        std::mem::take(&mut self.reports)
    }

    /// The options of one attempt: no wait for the lock, the gate builds asked for, the
    /// pins left to the controller.
    fn opts(&self) -> LandOpts {
        LandOpts {
            timeout: MERGE_WAIT,
            lock_wait: Duration::ZERO,
            sync_pins: false,
            builds: Some(self.builds.clone()),
            ..LandOpts::default()
        }
    }

    /// One round: finish the landing records left, then try each candidate in Ready order.
    /// `table` is this round's table. Stops at a rate limit, a sign-in failure or an
    /// unreachable hub; any other failure of one pull request is logged and the next is
    /// tried. Either way the landings done are in [`AutoLand::take_reports`] and their
    /// `decisions.log` lines are written (or kept for the next round).
    pub async fn round<G: Gh>(
        &mut self,
        gh: &G,
        s: &Settings,
        bus: &Bus,
        infos: &[PrInfo],
        table: &str,
    ) -> Result<()> {
        self.write_decisions(bus).await;
        let r = self.attempts(gh, s, bus, infos, table).await;
        self.write_decisions(bus).await;
        r
    }

    async fn attempts<G: Gh>(
        &mut self,
        gh: &G,
        s: &Settings,
        bus: &Bus,
        infos: &[PrInfo],
        table: &str,
    ) -> Result<()> {
        if !s.repos.iter().any(|r| s.coordinator_lands(r)) {
            return Ok(());
        }
        let mut tries = records(s, bus, infos).await?;
        let notes = if needs_notes(s, infos) {
            candidates::read_ready_notes(bus, s).await?
        } else {
            ReadyNotes::new()
        };
        for c in candidates::select(s, infos, &notes) {
            let t = Try::from(&c);
            if !tries
                .iter()
                .any(|x| x.repo == t.repo && x.number == t.number)
            {
                tries.push(t);
            }
        }
        let o = self.opts();
        for t in tries {
            if t.candidate {
                match table::table_word(Some(table), &t.repo, t.number, &t.head, false, Utc::now())
                {
                    TableWord::Ask(_) => {}
                    TableWord::NotYet(why) => {
                        tracing::info!("{} #{}: not yet ({why})", t.repo, t.number);
                        continue;
                    }
                    TableWord::Limit => {
                        tracing::info!("{} #{}: the review bot is at its limit", t.repo, t.number);
                        continue;
                    }
                }
            }
            match land::merge_once(gh, s, bus, &t.repo, t.number, &t.head, &o).await {
                Ok(l) => {
                    tracing::info!("{}", l.outcome.line);
                    if let Some(n) = self.report(s, &t, &l) {
                        self.reports.push(n);
                    }
                }
                Err(Error::Locked(m)) => tracing::info!("{} #{}: {m}", t.repo, t.number),
                Err(e @ (Error::RateLimited(_) | Error::Auth(_))) => return Err(e),
                Err(Error::Bus(e @ hgl_ai_bus::BusError::Unreachable { .. })) => {
                    return Err(Error::Bus(e));
                }
                Err(e) => tracing::warn!("landing {} #{}: {e}", t.repo, t.number),
            }
        }
        Ok(())
    }

    /// What the controller hears of one attempt: a landing done or finished, or a
    /// `PARTIAL` not reported before. A landing also gets its `decisions.log` line.
    fn report(&mut self, s: &Settings, t: &Try, l: &Landing) -> Option<Note> {
        let o = &l.outcome;
        let merge = o.merge_commit.as_deref().unwrap_or_default();
        let first = match o.kind {
            Kind::Merged | Kind::Resumed => {
                let how = if o.kind == Kind::Resumed {
                    ", finishing an earlier attempt"
                } else {
                    ""
                };
                let line = format!(
                    "landed {}#{} at {} as {}{how}",
                    t.repo,
                    t.number,
                    short(&o.head),
                    short(merge)
                );
                self.decisions
                    .push(format!("{} {}: {line}", utc_now_stamp(), s.coord_seat));
                format!("[coord] Landed: {}", &line["landed ".len()..])
            }
            Kind::Partial if self.partial.insert(o.line.clone()) => {
                format!("[coord] Partial: {}", o.line)
            }
            _ => return None,
        };
        let mut body = first;
        for a in &l.after {
            body.push_str("\n- ");
            body.push_str(a);
        }
        let mut n = Note::new(
            s.coord_seat.clone(),
            s.controller.clone(),
            NoteKind::Note,
            body,
        );
        n.repo = Some(t.repo.clone());
        n.issue = t.number;
        n.head = Some(o.head.clone()).filter(|h| hgl_ai_proto::check_head(h).is_ok());
        match n.validate() {
            Ok(()) => Some(n),
            Err(e) => {
                tracing::warn!("a landing report for {} #{}: {e}", t.repo, t.number);
                None
            }
        }
    }

    /// Write the `decisions.log` lines not written yet; those that fail stay for the next
    /// round.
    async fn write_decisions(&mut self, bus: &Bus) {
        let mut kept = Vec::new();
        for line in std::mem::take(&mut self.decisions) {
            if let Err(e) = hgl_ai_owner::decision_once(bus, &line).await {
                tracing::warn!("writing a landing to decisions.log: {e}; kept for the next round");
                kept.push(line);
            }
        }
        self.decisions = kept;
    }
}

impl From<&Candidate> for Try {
    fn from(c: &Candidate) -> Try {
        Try {
            repo: c.repo.clone(),
            number: c.number,
            head: c.head.clone(),
            candidate: true,
        }
    }
}

/// Whether a pull request could be a candidate but for its place in Ready order, which
/// only the controller's `ready` notes can give: they are read only then.
fn needs_notes(s: &Settings, infos: &[PrInfo]) -> bool {
    infos.iter().any(|i| {
        s.coordinator_lands(&i.row.repo)
            && !i.row.draft
            && i.ready_at.is_none()
            && matches!(
                i.note,
                Some(crate::bot::NoteState::Ready | crate::bot::NoteState::Approved)
            )
    })
}

/// `landing.<repo>.<pr>` as its repository and number.
fn parse_record_key(key: &str) -> Option<(String, u64)> {
    let rest = key.strip_prefix("landing.")?;
    let (repo, pr) = rest.rsplit_once('.')?;
    Some((repo.to_string(), pr.parse().ok()?)).filter(|(r, _)| !r.is_empty())
}

/// The landing records to finish in opted-in repositories: each one whose merge is
/// recorded, or whose pull request is not open any more (merged meanwhile, or never). One
/// whose pull request is still open and has no merge is the candidates' to decide.
async fn records(s: &Settings, bus: &Bus, infos: &[PrInfo]) -> Result<Vec<Try>> {
    let mut out = Vec::new();
    for key in bus.state_keys("landing.").await? {
        let Some((repo, number)) = parse_record_key(&key) else {
            continue;
        };
        if !s.coordinator_lands(&repo) {
            continue;
        }
        let Some(p) = progress::read(bus, &repo, number).await? else {
            continue;
        };
        let open = infos
            .iter()
            .any(|i| i.row.repo == repo && i.row.number == number);
        if p.merge.is_none() && open {
            continue;
        }
        if hgl_ai_proto::check_head(&p.head).is_err() {
            tracing::warn!("{key}: no head recorded; left to the controller");
            continue;
        }
        out.push(Try {
            repo,
            number,
            head: p.head,
            candidate: false,
        });
    }
    Ok(out)
}

/// A dry run's line for each candidate.
pub fn dry_run(s: &Settings, infos: &[PrInfo], out: &mut impl Write) {
    for c in candidates::select(s, infos, &ReadyNotes::new()) {
        let _ = writeln!(
            out,
            "would try landing {}#{} at {} (Ready {})",
            c.repo,
            c.number,
            short(&c.head),
            c.ready_at.format("%Y%m%dT%H%M%SZ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::settings;

    #[test]
    fn record_keys() {
        assert_eq!(
            parse_record_key("landing.hgl-ai.138"),
            Some(("hgl-ai".into(), 138))
        );
        assert_eq!(
            parse_record_key("landing.a.b.7"),
            Some(("a.b".into(), 7)),
            "a dot in the name"
        );
        for k in ["landing.hgl-ai", "landing..7", "landing.x.y", "main"] {
            assert_eq!(parse_record_key(k), None, "{k}");
        }
    }

    fn landing(kind: Kind, line: &str, after: &[&str]) -> Landing {
        let mut outcome = crate::gate::Outcome::new(kind, line.into(), false, &"a".repeat(40));
        outcome.merge_commit = Some("b".repeat(40));
        Landing {
            outcome,
            after: after.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn one() -> Try {
        Try {
            repo: "hgl-ai".into(),
            number: 7,
            head: "a".repeat(40),
            candidate: true,
        }
    }

    #[test]
    fn what_is_reported() {
        let s = settings();
        let mut a = AutoLand::new(&s);
        let n = a
            .report(
                &s,
                &one(),
                &landing(Kind::Merged, "merged MERGED", &["landed.log", "main"]),
            )
            .unwrap();
        assert_eq!(
            n.body,
            "[coord] Landed: hgl-ai#7 at aaaaaaaa as bbbbbbbb\n- landed.log\n- main\n"
        );
        assert_eq!(
            (n.kind, n.recipient.to_string(), n.issue),
            (NoteKind::Note, "controller".into(), 7)
        );
        assert_eq!(n.head.as_deref(), Some("a".repeat(40).as_str()));
        assert_eq!(a.decisions.len(), 1);
        assert!(
            a.decisions[0].ends_with(" coord: landed hgl-ai#7 at aaaaaaaa as bbbbbbbb"),
            "{:?}",
            a.decisions
        );
        let n = a
            .report(&s, &one(), &landing(Kind::Resumed, "MERGED", &[]))
            .unwrap();
        assert!(
            n.body.ends_with(", finishing an earlier attempt\n"),
            "{}",
            n.body
        );
        assert_eq!(a.decisions.len(), 2);
        // A PARTIAL once per line; no decision line for it.
        let p = landing(Kind::Partial, "the comment is not posted PARTIAL", &[]);
        let n = a.report(&s, &one(), &p).unwrap();
        assert_eq!(
            n.body,
            "[coord] Partial: the comment is not posted PARTIAL\n"
        );
        assert!(a.report(&s, &one(), &p).is_none());
        assert_eq!(a.decisions.len(), 2);
        // Nothing for a gate not met, nor for a landing done earlier by another.
        for k in [
            Kind::Wait,
            Kind::Blocked,
            Kind::Behind,
            Kind::Updated,
            Kind::AlreadyMerged,
        ] {
            assert!(a.report(&s, &one(), &landing(k, "x", &[])).is_none());
        }
        assert_eq!(a.decisions.len(), 2);
    }

    #[test]
    fn opts_wait_for_nothing() {
        let o = AutoLand::new(&settings()).opts();
        assert_eq!(o.lock_wait, Duration::ZERO);
        assert_eq!(o.timeout, MERGE_WAIT);
        assert!(!o.sync_pins && o.notify && o.comment);
        assert!(o.builds.is_some());
        assert!(!o.gate.triaged && !o.gate.hygiene_read);
    }
}
