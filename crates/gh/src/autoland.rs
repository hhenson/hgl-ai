//! The coordinator's landings (hgl-ai#123): in a repository that opts in
//! (`[github.gate.<repo>] coordinator_lands`), `gh coord run` lands each pull request whose
//! gates all pass, in Ready order ([`crate::candidates`]), one gate attempt per candidate per
//! round under the landing lock, which it does not wait for. The gate is the controller's
//! (exact head, current `main` and its merge test, an other-family clean receipt, checks,
//! the bot and threads), and it also confirms that the pull request is still Ready at that
//! head ([`gate::Opts::ready_at_head`]). A local build verdict it lacks is asked of a builder
//! (BUILD-15) and read in a later round.
//!
//! First in each round, a landing record left in an opted-in repository
//! ([`crate::progress`]) is finished by the same attempt when its merge is confirmed: the
//! record holds the merge commit, or GitHub reports the pull request merged. A record whose
//! pull request is open is the candidates' to decide; one closed without a merge is left,
//! and not asked about again.
//!
//! Every landing it does or finishes is reported in two more steps of its record, under
//! the same lock and before the record is deleted: a `note` to the controller, then a
//! `decisions.log` line. A step that fails leaves the record, and the next round does only
//! what is left. A `PARTIAL` goes to the controller once. The wrapper's pins are left to the
//! controller.

use std::collections::BTreeSet;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use hgl_ai_bus::Bus;
use hgl_ai_proto::{Kind as NoteKind, Note};

use crate::bot::{BotState, NoteState};
use crate::builds::GateBuilds;
use crate::candidates::{self, Candidate, ReadyNotes};
use crate::exec::Gh;
use crate::gate::{self, Kind};
use crate::land::{self, LandOpts, Landing};
use crate::lock::{LandLock, holder};
use crate::progress::{self, Progress};
use crate::table::{Checks, PrInfo, Row};
use crate::{Error, Result, Settings, short};

/// How long one gate attempt may wait on a merge GitHub accepted but has not done; past
/// it the record is kept and a later round finishes the landing.
pub const MERGE_WAIT: Duration = Duration::from_secs(5 * 60);
/// The landing record's steps for the coordinator's reports, after the landing's own.
pub const STEP_NOTE: &str = "coord.note";
pub const STEP_DECISION: &str = "coord.decision";

/// The coordinator's landing state across rounds.
pub struct AutoLand {
    /// The gate builds asked for, so a verdict on its way is not asked for again.
    builds: Arc<GateBuilds>,
    /// `PARTIAL` lines sent to the controller: each once.
    partial: BTreeSet<String>,
    /// Records whose pull request GitHub showed closed without a merge, by repository,
    /// number and head: not asked about again.
    closed: BTreeSet<(String, u64, String)>,
}

/// One pull request to try: a candidate, or a landing record to finish.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Try {
    repo: String,
    number: u64,
    head: String,
}

impl AutoLand {
    pub fn new(s: &Settings) -> AutoLand {
        AutoLand {
            builds: Arc::new(GateBuilds::new(s.coord_seat.clone(), "coordinator")),
            partial: BTreeSet::new(),
            closed: BTreeSet::new(),
        }
    }

    /// The options of one attempt: Ready at the head asked for, the gate builds asked for,
    /// the record kept for the reports, the pins left to the controller.
    fn opts(&self) -> LandOpts {
        LandOpts {
            gate: gate::Opts {
                ready_at_head: true,
                ..gate::Opts::default()
            },
            timeout: MERGE_WAIT,
            lock_wait: Duration::ZERO,
            sync_pins: false,
            keep_record: true,
            builds: Some(self.builds.clone()),
            ..LandOpts::default()
        }
    }

    /// One round over this round's open pull requests: finish the landing records left,
    /// then try each candidate in Ready order. Stops at a rate limit, a sign-in failure or
    /// an unreachable hub; any other failure of one pull request is logged and the next is
    /// tried. What a landing reported is in its record, so a stop loses none of it.
    pub async fn round<G: Gh>(
        &mut self,
        gh: &G,
        s: &Settings,
        bus: &Bus,
        infos: &[PrInfo],
    ) -> Result<()> {
        if !s.repos.iter().any(|r| s.coordinator_lands(r)) {
            return Ok(());
        }
        let mut tries = self.records(gh, s, bus, infos).await?;
        let notes = if needs_notes(s, infos) {
            candidates::read_ready_notes(bus, s).await?
        } else {
            ReadyNotes::new()
        };
        for c in to_try(s, infos, &notes) {
            let t = Try::from(&c);
            if !tries
                .iter()
                .any(|x| x.repo == t.repo && x.number == t.number)
            {
                tries.push(t);
            }
        }
        for t in tries {
            match self.attempt(gh, s, bus, &t).await {
                Ok(()) => {}
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

    /// One gate attempt under the landing lock, and a landing's reports under it too.
    async fn attempt<G: Gh>(&mut self, gh: &G, s: &Settings, bus: &Bus, t: &Try) -> Result<()> {
        let who = holder(s, &format!("coordinator {}#{}", t.repo, t.number));
        let lock = LandLock::acquire(bus, &t.repo, &who, Duration::ZERO).await?;
        let r = self.attempt_locked(gh, s, bus, t, &lock).await;
        lock.release().await;
        r
    }

    async fn attempt_locked<G: Gh>(
        &mut self,
        gh: &G,
        s: &Settings,
        bus: &Bus,
        t: &Try,
        lock: &LandLock,
    ) -> Result<()> {
        let l =
            land::merge_locked(gh, s, bus, &t.repo, t.number, &t.head, &self.opts(), lock).await?;
        tracing::info!("{}", l.outcome.line);
        match l.outcome.kind {
            Kind::Merged | Kind::Resumed => report(s, bus, t, &l, lock).await,
            Kind::Partial if self.partial.insert(l.outcome.line.clone()) => {
                let body = with_after(format!("[coord] Partial: {}", l.outcome.line), &l);
                let n = note(s, t, &l.outcome.head, body, None)?;
                if let Err(e) = bus.send(&n).await {
                    self.partial.remove(&l.outcome.line);
                    return Err(e.into());
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// The landing records to finish in opted-in repositories: each whose merge is
    /// confirmed, by the record or by GitHub. A record of a pull request among the open
    /// ones with no merge recorded is the candidates' to decide; absence from this round's
    /// read is not taken as closed (it reads at most 50 per repository, and a repository
    /// may give no answer), so GitHub is asked.
    async fn records<G: Gh>(
        &mut self,
        gh: &G,
        s: &Settings,
        bus: &Bus,
        infos: &[PrInfo],
    ) -> Result<Vec<Try>> {
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
            if hgl_ai_proto::check_head(&p.head).is_err() {
                tracing::warn!("{key}: no head recorded; left to the controller");
                continue;
            }
            let t = Try {
                repo,
                number,
                head: p.head,
            };
            if p.merge.is_none() {
                let open = infos
                    .iter()
                    .any(|i| i.row.repo == t.repo && i.row.number == t.number);
                let seen = (t.repo.clone(), t.number, t.head.clone());
                if open || self.closed.contains(&seen) {
                    continue;
                }
                let pr = match gate::fetch_pr(gh, s, &t.repo, t.number).await {
                    Ok(pr) => pr,
                    Err(e @ (Error::RateLimited(_) | Error::Auth(_))) => return Err(e),
                    Err(e) => {
                        tracing::warn!("{key}: reading the pull request: {e}");
                        continue;
                    }
                };
                if !pr.merged {
                    if pr.state != "open" {
                        tracing::info!("{key}: closed without a merge; left");
                        self.closed.insert(seen);
                    }
                    continue;
                }
            }
            out.push(t);
        }
        Ok(out)
    }
}

/// A landing's reports, each a step of its record, then the record's removal: the note to
/// the controller (with the landing's `landed` notes' `created_at`, so a resend carries the
/// same message id), then the `decisions.log` line (stamped the same, so it is written once).
async fn report(s: &Settings, bus: &Bus, t: &Try, l: &Landing, lock: &LandLock) -> Result<()> {
    let o = &l.outcome;
    let mut p = progress::read(bus, &t.repo, t.number)
        .await?
        .unwrap_or_else(|| Progress::new(&o.head, o.merge_commit.clone()));
    let merge = o
        .merge_commit
        .clone()
        .or_else(|| p.merge.clone())
        .unwrap_or_default();
    let stamp = p.notes_at.clone().unwrap_or_else(|| p.updated_at.clone());
    let line = format!(
        "landed {}#{} at {} as {}",
        t.repo,
        t.number,
        short(&o.head),
        short(&merge)
    );
    if !p.has(STEP_NOTE) {
        let how = if o.kind == Kind::Resumed {
            ", finishing an earlier attempt"
        } else {
            ""
        };
        let body = with_after(
            format!("[coord] Landed: {}{how}", &line["landed ".len()..]),
            l,
        );
        let n = note(s, t, &o.head, body, Some(&stamp))?;
        lock.confirm("reporting the landing").await?;
        bus.send(&n).await?;
        p.mark(STEP_NOTE);
        progress::write(bus, &t.repo, t.number, &p).await?;
    }
    if !p.has(STEP_DECISION) {
        let entry = format!("{stamp} {}: {line}", s.coord_seat);
        hgl_ai_owner::decision_once(bus, &entry)
            .await
            .map_err(|e| Error::Invalid(format!("writing decisions.log: {e}")))?;
        p.mark(STEP_DECISION);
        progress::write(bus, &t.repo, t.number, &p).await?;
    }
    progress::delete(bus, &t.repo, t.number).await
}

fn with_after(mut body: String, l: &Landing) -> String {
    for a in &l.after {
        body.push_str("\n- ");
        body.push_str(a);
    }
    body
}

/// A note from the coordinator to the controller about `t`.
fn note(s: &Settings, t: &Try, head: &str, body: String, at: Option<&str>) -> Result<Note> {
    let mut n = Note::new(
        s.coord_seat.clone(),
        s.controller.clone(),
        NoteKind::Note,
        body,
    );
    n.repo = Some(t.repo.clone());
    n.issue = t.number;
    n.head = Some(head.to_string()).filter(|h| hgl_ai_proto::check_head(h).is_ok());
    if let Some(at) = at {
        n.created_at = at.to_string();
    }
    n.validate()?;
    Ok(n)
}

impl From<&Candidate> for Try {
    fn from(c: &Candidate) -> Try {
        Try {
            repo: c.repo.clone(),
            number: c.number,
            head: c.head.clone(),
        }
    }
}

/// What in the round's row holds a candidate back with no GitHub call: a failed check,
/// an unresolved thread, bot findings, or the bot at its limit. A row the bot has not
/// reviewed (`none`, `running`) goes to the gate, which decides whether a reviewed parent
/// carries over (ROLE-10).
fn held_back(r: &Row) -> Option<String> {
    let mut why = Vec::new();
    if r.checks == Checks::Fail {
        why.push("a check failed".to_string());
    }
    if r.unresolved != 0 {
        why.push(format!("{} unresolved", r.unresolved));
    }
    if matches!(r.bot, BotState::Findings | BotState::Limit) {
        why.push(format!("bot {}", r.bot.as_str()));
    }
    (!why.is_empty()).then(|| why.join(", "))
}

/// The candidates to try this round, in Ready order: those the row does not hold back.
fn to_try(s: &Settings, infos: &[PrInfo], notes: &ReadyNotes) -> Vec<Candidate> {
    candidates::select(s, infos, notes)
        .into_iter()
        .filter(|c| {
            let row = infos
                .iter()
                .map(|i| &i.row)
                .find(|r| r.repo == c.repo && r.number == c.number);
            match row.and_then(held_back) {
                Some(why) => {
                    tracing::info!("{} #{}: not yet ({why})", c.repo, c.number);
                    false
                }
                None => true,
            }
        })
        .collect()
}

/// Whether a pull request could be a candidate but for its place in Ready order, which
/// only the controller's `ready` notes can give: they are read only then.
fn needs_notes(s: &Settings, infos: &[PrInfo]) -> bool {
    infos.iter().any(|i| {
        s.coordinator_lands(&i.row.repo)
            && !i.row.draft
            && i.ready_at.is_none()
            && matches!(i.note, Some(NoteState::Ready | NoteState::Approved))
    })
}

/// `landing.<repo>.<pr>` as its repository and number.
fn parse_record_key(key: &str) -> Option<(String, u64)> {
    let rest = key.strip_prefix("landing.")?;
    let (repo, pr) = rest.rsplit_once('.')?;
    Some((repo.to_string(), pr.parse().ok()?)).filter(|(r, _)| !r.is_empty())
}

/// A dry run's line for each candidate the round would try (the `ready` notes are not
/// read, so only a standing `Ready` comment places one).
pub fn dry_run(s: &Settings, infos: &[PrInfo], out: &mut impl Write) {
    for c in to_try(s, infos, &ReadyNotes::new()) {
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
    use crate::testutil::{settings, with_coordinator_lands};

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

    #[test]
    fn opts_wait_for_nothing() {
        let o = AutoLand::new(&settings()).opts();
        assert_eq!(o.lock_wait, Duration::ZERO);
        assert_eq!(o.timeout, MERGE_WAIT);
        assert!(!o.sync_pins && o.notify && o.comment && o.keep_record);
        assert!(o.builds.is_some());
        assert!(o.gate.ready_at_head);
        assert!(!o.gate.triaged && !o.gate.hygiene_read);
    }

    fn info(number: u64, bot: BotState, checks: Checks, unresolved: u64) -> PrInfo {
        PrInfo {
            row: Row {
                repo: "hgl-ai".into(),
                number,
                head: format!("{number:0>40}"),
                draft: false,
                bot,
                unresolved,
                checks,
                updated: "20261128T120000Z".into(),
            },
            note: Some(NoteState::Ready),
            ready_at: Some(format!("2026-11-28T10:{number:02}:00Z")),
            ready_cleared: None,
            url: String::new(),
            review_url: None,
        }
    }

    /// The row holds back only what the gate would refuse; a head the bot has not
    /// reviewed goes to the gate (ROLE-10), and the dry run says the same.
    #[test]
    fn what_the_row_holds_back() {
        let s = with_coordinator_lands(settings(), &["hgl-ai"], true);
        let infos = [
            info(1, BotState::Clean, Checks::Pass, 0),
            info(2, BotState::None, Checks::Pending, 0),
            info(3, BotState::Running, Checks::None, 0),
            info(4, BotState::Clean, Checks::Fail, 0),
            info(5, BotState::Clean, Checks::Pass, 2),
            info(6, BotState::Findings, Checks::Pass, 0),
            info(7, BotState::Limit, Checks::Pass, 0),
        ];
        let tried: Vec<u64> = to_try(&s, &infos, &ReadyNotes::new())
            .iter()
            .map(|c| c.number)
            .collect();
        assert_eq!(tried, [1, 2, 3]);
        let mut out = Vec::new();
        dry_run(&s, &infos, &mut out);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out.lines().count(), 3, "{out}");
        assert!(
            out.starts_with("would try landing hgl-ai#1 at 00000000 (Ready 20261128T100100Z)"),
            "{out}"
        );
        assert!(!out.contains("hgl-ai#4"), "{out}");
    }
}
