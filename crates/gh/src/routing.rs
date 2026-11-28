//! Route routine review notes against the coordinator's current pull request batch.

use std::collections::{BTreeMap, BTreeSet};

use hgl_ai_bus::{Bus, BusError, Lease, StreamNote};
use hgl_ai_config::Github;
use hgl_ai_proto::{Family, HostRecord, Kind, Note, Role, Seat, TaskKey, TreeRecord, Verdict};
use serde::{Deserialize, Serialize};

use crate::bot::{self, Comment, NoteState};
use crate::exec::{self, Gh};
use crate::table::PrInfo;
use crate::{Error, Result, Settings, invalid, short, state, verdict};

const CHECKPOINT: &str = "coord.routing";
const LOCK: &str = "lock.coord.routing";
const ROUND_NOTES: usize = 64;
const HISTORY_PAGE: usize = 512;

/// Keep the ordinary sender command usable on both routed and controller-managed repos.
pub fn redirect(g: &Github, note: &mut Note) -> Result<()> {
    if note.recipient.as_str() == g.controller
        && note.task().is_some_and(|task| g.routes_notes(&task.repo))
        && (note.kind == Kind::Ready || state::routed_receipt(note).is_some())
    {
        let seat = Seat::new(&g.coord_seat)?;
        if seat.is_reserved() || seat == note.recipient {
            return Err(invalid(
                "github.coord_seat must be a separate seat for review routing",
            ));
        }
        note.recipient = seat;
    }
    Ok(())
}

struct Pull<'a> {
    pr: &'a PrInfo,
    issue: u64,
}

impl Pull<'_> {
    fn number(&self) -> u64 {
        self.pr.row.number
    }

    fn matches_tree(&self, t: &TreeRecord) -> bool {
        !t.review
            && t.task.repo == self.pr.row.repo
            && (t.task.issue == self.issue || t.branch.as_deref() == Some(&self.pr.branch))
    }

    fn current_head(&self, repo: &str, head: &str) -> bool {
        self.pr.row.repo == repo
            && self.pr.row.head == head
            && !self.pr.row.draft
            && self.pr.base == "main"
    }

    fn authors(&self, trees: &[TreeRecord]) -> Vec<Family> {
        state::author_families(trees, &self.pr.row.repo, &self.pr.branch, self.issue)
    }

    fn current_seats(&self, trees: &[TreeRecord]) -> Vec<Seat> {
        state::current_seats(
            trees,
            &self.pr.row.repo,
            &self.pr.branch,
            self.issue,
            &[],
            &[],
        )
    }
}

#[derive(Default)]
struct History {
    pulls: BTreeMap<TaskKey, PullHistory>,
}

#[derive(Default)]
struct PullHistory {
    reviews: BTreeSet<(String, Seat)>,
    receipts: BTreeMap<(String, Seat), u64>,
    sendbacks: BTreeMap<String, Seat>,
}

impl History {
    fn record(&mut self, item: StreamNote, pulls: &[Pull<'_>], settings: &Settings) {
        let n = &item.note;
        if !matches!(n.kind, Kind::Review | Kind::Receipt | Kind::Sendback) {
            return;
        }
        let (Some(key), Some(head)) = (n.task(), n.head.clone()) else {
            return;
        };
        if n.kind == Kind::Sendback {
            if n.sender != settings.controller && n.sender != settings.coord_seat {
                return;
            }
            for p in pulls.iter().filter(|p| {
                p.pr.row.repo == key.repo && (p.issue == n.issue || p.number() == n.issue)
            }) {
                let key = TaskKey {
                    repo: key.repo.clone(),
                    issue: p.number(),
                };
                self.pulls
                    .entry(key)
                    .or_default()
                    .sendbacks
                    .insert(head.clone(), n.recipient.clone());
            }
            return;
        }
        if !pulls
            .iter()
            .any(|p| p.pr.row.repo == key.repo && p.number() == key.issue)
        {
            return;
        }
        match n.kind {
            Kind::Review if n.sender == settings.controller || n.sender == settings.coord_seat => {
                self.pulls
                    .entry(key)
                    .or_default()
                    .reviews
                    .insert((head, n.recipient.clone()));
            }
            Kind::Receipt if state::routed_receipt(n).is_some() => {
                self.pulls
                    .entry(key)
                    .or_default()
                    .receipts
                    .insert((head, n.sender.clone()), item.seq);
            }
            _ => {}
        }
    }

    fn load(&self, seat: &Seat) -> usize {
        self.pulls
            .values()
            .map(|p| {
                p.reviews
                    .iter()
                    .filter(|review| &review.1 == seat && !p.receipts.contains_key(*review))
                    .count()
            })
            .sum()
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum Action {
    Forward,
    Review {
        pr: u64,
        to: Seat,
        earlier: Option<String>,
    },
    Sendback {
        pr: u64,
    },
    Candidate {
        pr: u64,
    },
    AlreadyRouted,
}

struct Facts<'a> {
    settings: &'a Settings,
    pulls: Vec<Pull<'a>>,
    hosts: Vec<HostRecord>,
    trees: Vec<TreeRecord>,
    history: History,
}

impl Facts<'_> {
    fn plan(&self, n: &Note) -> Action {
        let (Some(key), Some(head)) = (n.task(), n.head.as_deref()) else {
            return Action::Forward;
        };
        let Some(pool) = self
            .settings
            .review_pools
            .get(&key.repo)
            .filter(|p| p.automatic(&key.repo))
        else {
            return Action::Forward;
        };
        let mut matching = self.pulls.iter().filter(|p| {
            p.current_head(&key.repo, head)
                && match n.kind {
                    Kind::Ready => p.number() == key.issue || p.issue == key.issue,
                    Kind::Receipt => p.number() == key.issue,
                    _ => false,
                }
        });
        let Some(pull) = matching.next() else {
            return Action::Forward;
        };
        if matching.next().is_some() {
            return Action::Forward;
        }
        let pr = pull.number();
        let authors = pull.authors(&self.trees);
        if authors.is_empty() {
            return Action::Forward;
        }
        let current = pull.current_seats(&self.trees);
        let key = TaskKey { issue: pr, ..key };
        let empty = PullHistory::default();
        let past = self.history.pulls.get(&key).unwrap_or(&empty);

        match n.kind {
            Kind::Ready if current.as_slice() == std::slice::from_ref(&n.sender) => {
                if past.reviews.iter().any(|(sha, _)| sha == head) {
                    return Action::AlreadyRouted;
                }
                self.pick_reviewer(pull, head, &pool.seats, &authors, past)
            }
            Kind::Receipt => {
                let Some(receipt) = state::routed_receipt(n) else {
                    return Action::Forward;
                };
                if !past.reviews.contains(&(receipt.head, n.sender.clone()))
                    || authors.contains(&receipt.family)
                    || self
                        .trees
                        .iter()
                        .any(|t| pull.matches_tree(t) && t.seat == n.sender)
                {
                    return Action::Forward;
                }
                match receipt.verdict {
                    Verdict::Clean => Action::Candidate { pr },
                    Verdict::Findings
                        if past.sendbacks.is_empty()
                            && current.len() == 1
                            && !receipt.evidence.trim().is_empty() =>
                    {
                        Action::Sendback { pr }
                    }
                    Verdict::Findings => Action::Forward,
                }
            }
            _ => Action::Forward,
        }
    }

    fn eligible(&self, pull: &Pull<'_>, seat: &Seat, authors: &[Family]) -> bool {
        let mut records = self
            .hosts
            .iter()
            .flat_map(|h| &h.seats)
            .filter(|s| &s.seat == seat)
            .peekable();
        records.peek().is_some()
            && records.all(|s| s.role == Role::Reviewer && !authors.contains(&s.family))
            && !self
                .trees
                .iter()
                .any(|t| pull.matches_tree(t) && &t.seat == seat)
    }

    fn pick_reviewer(
        &self,
        pull: &Pull<'_>,
        head: &str,
        pool: &[Seat],
        authors: &[Family],
        past: &PullHistory,
    ) -> Action {
        let earlier = past
            .receipts
            .iter()
            .filter(|((sha, by), _)| {
                sha != head
                    && pool.contains(by)
                    && self.eligible(pull, by, authors)
                    && past.reviews.contains(&(sha.clone(), by.clone()))
            })
            .max_by_key(|(_, seq)| **seq);
        if let Some(((sha, by), _)) = earlier {
            return Action::Review {
                pr: pull.number(),
                to: by.clone(),
                earlier: Some(sha.clone()),
            };
        }
        pool.iter()
            .filter(|seat| self.eligible(pull, seat, authors))
            .min_by_key(|seat| (self.history.load(seat), *seat))
            .map_or(Action::Forward, |to| Action::Review {
                pr: pull.number(),
                to: to.clone(),
                earlier: None,
            })
    }

    async fn load<'a>(bus: &Bus, settings: &'a Settings, infos: &'a [PrInfo]) -> Result<Facts<'a>> {
        let (hosts, trees) = tokio::try_join!(bus.hosts(), bus.trees())?;
        let pulls: Vec<_> = infos
            .iter()
            .map(|pr| {
                let issue = state::issue_from_branch(&pr.branch, &settings.branch_prefix)
                    .unwrap_or(pr.row.number);
                Pull { pr, issue }
            })
            .collect();
        let seats: Vec<_> = settings
            .review_pools
            .values()
            .flat_map(|p| p.seats.iter().cloned())
            .chain(
                hosts
                    .iter()
                    .flat_map(|h| h.seats.iter().map(|s| s.seat.clone())),
            )
            .chain(trees.iter().map(|t| t.seat.clone()))
            .chain([settings.coord_seat.clone(), settings.controller.clone()])
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        let mut history = History::default();
        let mut from = 1;
        while let (notes, Some(last)) = bus.read_page(&seats, from, HISTORY_PAGE).await? {
            for n in notes {
                history.record(n, &pulls, settings);
            }
            from = last + 1;
        }

        Ok(Facts {
            settings,
            pulls,
            hosts,
            trees,
            history,
        })
    }
}

// The pending plan is saved before its effects. It only stores routing metadata: the
// input note, including the full findings, stays in the JetStream history.
#[derive(Default, Serialize, Deserialize)]
struct Checkpoint {
    after: u64,
    pending: Option<(u64, Action)>,
    digest: Vec<String>,
}

impl Checkpoint {
    async fn save(&self, bus: &Bus, lease: &mut Lease, revision: &mut u64) -> Result<()> {
        lease.refresh().await?;
        *revision = bus
            .state_update(
                CHECKPOINT,
                &serde_json::to_string(self).expect("routing checkpoint encodes"),
                *revision,
            )
            .await?;
        Ok(())
    }

    async fn flush(
        &mut self,
        bus: &Bus,
        settings: &Settings,
        lease: &mut Lease,
        revision: &mut u64,
    ) -> Result<()> {
        if !self.digest.is_empty() {
            lease.refresh().await?;
            let note = Note::new(
                settings.coord_seat.clone(),
                settings.controller.clone(),
                Kind::Note,
                format!("[coord] Review routing\n\n- {}\n", self.digest.join("\n- ")),
            );
            bus.send_with_id(&note, &format!("coord-routing-digest-{}", self.after))
                .await?;
            self.digest.clear();
            self.save(bus, lease, revision).await?;
        }
        Ok(())
    }
}

pub(crate) async fn round<G: Gh>(
    gh: &G,
    settings: &Settings,
    bus: &Bus,
    infos: &[PrInfo],
) -> Result<()> {
    if settings.review_pools.is_empty() || settings.coord_seat == settings.controller {
        return Ok(());
    }
    let mut lease = match bus
        .lock_acquire(LOCK, &crate::lock::holder(settings, "review routing"))
        .await
    {
        Ok(lease) => lease,
        Err(BusError::Held { .. }) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let result = run(gh, settings, bus, infos, &mut lease).await;
    let released = lease.release().await;
    result.and(released.map(|_| ()).map_err(Error::from))
}

async fn run<G: Gh>(
    gh: &G,
    settings: &Settings,
    bus: &Bus,
    infos: &[PrInfo],
    lease: &mut Lease,
) -> Result<()> {
    let (mut c, mut revision) = if let Some(e) = bus.state_get(CHECKPOINT).await? {
        let c = serde_json::from_str::<Checkpoint>(&e.value)
            .map_err(|e| invalid(format!("{CHECKPOINT}: {e}")))?;
        (c, e.revision)
    } else {
        (Checkpoint::default(), 0)
    };
    c.flush(bus, settings, lease, &mut revision).await?;
    let (notes, Some(last)) = bus
        .read_page(
            std::slice::from_ref(&settings.coord_seat),
            c.after + 1,
            ROUND_NOTES,
        )
        .await?
    else {
        return Ok(());
    };
    let mut facts = Facts::load(bus, settings, infos).await?;
    for item in notes {
        if c.pending.as_ref().is_none_or(|(seq, _)| *seq != item.seq) {
            c.pending = Some((item.seq, facts.plan(&item.note)));
            c.save(bus, lease, &mut revision).await?;
        }
        let (_, action) = c.pending.as_ref().expect("input has a plan");
        let (line, sent) = execute(gh, bus, lease, &facts, &item, action).await?;
        if let Some(note) = sent {
            facts.history.record(note, &facts.pulls, settings);
        }
        if let Some(line) = line
            && !c.digest.contains(&line)
        {
            c.digest.push(line);
        }
        c.after = item.seq;
        c.pending = None;
        c.save(bus, lease, &mut revision).await?;
    }
    if c.after != last {
        c.after = last;
        c.pending = None;
        c.save(bus, lease, &mut revision).await?;
    }
    c.flush(bus, settings, lease, &mut revision).await
}

async fn execute<G: Gh>(
    gh: &G,
    bus: &Bus,
    lease: &mut Lease,
    facts: &Facts<'_>,
    item: &StreamNote,
    action: &Action,
) -> Result<(Option<String>, Option<StreamNote>)> {
    let s = facts.settings;
    let n = &item.note;
    let (repo, head) = (
        n.repo.as_deref().unwrap_or(""),
        n.head.as_deref().unwrap_or(""),
    );
    let pr = match action {
        Action::Review { pr, .. } | Action::Candidate { pr } | Action::Sendback { pr, .. } => *pr,
        Action::AlreadyRouted => return Ok((None, None)),
        Action::Forward => 0,
    };
    let label = format!("{repo}#{pr} at {head}");
    let pull = facts
        .pulls
        .iter()
        .find(|p| p.number() == pr && p.current_head(repo, head));
    let history = facts.history.pulls.get(&TaskKey {
        repo: repo.into(),
        issue: pr,
    });
    let pool = s.review_pools.get(repo).filter(|pool| pool.automatic(repo));
    let (line, note) = match (action, pull, pool) {
        (Action::Review { to, earlier, .. }, Some(pull), Some(pool)) => {
            let line = Some(format!("{label} → review by {to}"));
            if history.is_some_and(|p| p.reviews.contains(&(head.into(), to.clone()))) {
                return Ok((line, None));
            }
            let authors = pull.authors(&facts.trees);
            if authors.is_empty()
                || !pool.seats.contains(to)
                || !facts.eligible(pull, to, &authors)
                || pull.current_seats(&facts.trees).as_slice() != std::slice::from_ref(&n.sender)
            {
                return deliver(bus, lease, item.seq, None, forward(s, n)).await;
            }
            let prior = earlier.as_ref().map_or_else(String::new, |sha| {
                format!("\nEarlier reviewed head: {sha}. Check the fixes and their diff if the changes are limited to the findings (hgl-ai#118).")
            });
            let body = format!(
                "[coord] Review: {repo}#{pr} at {}\n\nReady from {}.\n{}\n{prior}",
                short(head),
                n.sender,
                pull.pr.url,
            );
            let note = routed_note(s, to, n, Kind::Review, pr, body);
            (line, note)
        }
        (Action::Sendback { .. }, Some(pull), Some(_)) => {
            if let Some(to) = history.and_then(|p| p.sendbacks.get(head)) {
                return Ok((Some(format!("{label} → sendback to {to}")), None));
            }
            let receipt = state::routed_receipt(n).expect("sendback comes from a review receipt");
            let authors = pull.authors(&facts.trees);
            if authors.is_empty()
                || authors.contains(&receipt.family)
                || facts
                    .trees
                    .iter()
                    .any(|t| pull.matches_tree(t) && t.seat == n.sender)
                || history.is_some_and(|p| !p.sendbacks.is_empty())
            {
                return deliver(bus, lease, item.seq, None, forward(s, n)).await;
            }
            let trees = bus.trees().await?;
            let current = pull.current_seats(&trees);
            let [to] = current.as_slice() else {
                return deliver(bus, lease, item.seq, None, forward(s, n)).await;
            };
            let findings = format!(
                "Review findings from {} ({} {}).\n\n{}\n\nLimitations: {}",
                receipt.reviewer,
                receipt.family,
                receipt.model,
                receipt.evidence,
                receipt.limitations
            );
            let body = verdict::with_first_line(
                &findings,
                s.coord_seat.as_str(),
                Kind::Sendback,
                repo,
                pr,
                head,
            );
            if comment(gh, s, lease, pull, item.seq, &body).await? {
                let note = routed_note(s, to, n, Kind::Sendback, pull.issue, body);
                (Some(format!("{label} → sendback to {to}")), note)
            } else {
                (None, forward(s, n))
            }
        }
        (Action::Candidate { .. }, Some(_), Some(_)) => {
            return Ok((
                Some(format!(
                    "{label} → landing candidate (clean receipt by {})",
                    n.sender
                )),
                None,
            ));
        }
        _ => (None, forward(s, n)),
    };
    deliver(bus, lease, item.seq, line, note).await
}

async fn deliver(
    bus: &Bus,
    lease: &mut Lease,
    seq: u64,
    line: Option<String>,
    note: Note,
) -> Result<(Option<String>, Option<StreamNote>)> {
    lease.refresh().await?;
    let sent = bus
        .send_with_id(&note, &format!("coord-routing-{seq}"))
        .await?;
    Ok((
        line,
        Some(StreamNote {
            seq: sent.seq,
            note,
        }),
    ))
}

fn forward(s: &Settings, n: &Note) -> Note {
    Note {
        recipient: s.controller.clone(),
        ..n.clone()
    }
}

fn routed_note(
    s: &Settings,
    to: &Seat,
    source: &Note,
    kind: Kind,
    issue: u64,
    body: String,
) -> Note {
    let mut note = Note::new(s.coord_seat.clone(), to.clone(), kind, body);
    note.created_at.clone_from(&source.created_at);
    note.repo.clone_from(&source.repo);
    note.head.clone_from(&source.head);
    note.issue = issue;
    note
}

async fn comment<G: Gh>(
    gh: &G,
    s: &Settings,
    lease: &mut Lease,
    pull: &Pull<'_>,
    seq: u64,
    body: &str,
) -> Result<bool> {
    let (repo, pr, head) = (&pull.pr.row.repo, pull.number(), &pull.pr.row.head);
    let path = format!("repos/{}/issues/{pr}/comments", s.full(repo));
    let marker = format!("<!-- hgl-ai coordinator sendback {head} {seq} -->");
    let posted = format!("{body}\n{marker}\n");
    if posted.len() > 65_536 {
        return Ok(false);
    }
    let comments = exec::get_list(gh, &format!("{path}?per_page=100")).await?;
    if comments.iter().any(|c| {
        c["body"]
            .as_str()
            .is_some_and(|body| body.trim_end().ends_with(&marker))
    }) {
        return Ok(true);
    }
    if comments
        .iter()
        .map(Comment::from_rest)
        .any(|c| bot::note_state(&[c]) == Some(NoteState::Sendback))
    {
        return Ok(false);
    }
    let current = crate::gate::fetch_pr(gh, s, repo, pr).await?;
    if &current.head != head
        || current.state != "open"
        || current.merged
        || current.draft
        || current.base != "main"
    {
        return Ok(false);
    }
    lease.refresh().await?;
    match exec::write_json(gh, "POST", &path, &serde_json::json!({"body": posted})).await {
        Ok(_) => Ok(true),
        Err(Error::Command { msg, .. }) if msg.contains("HTTP 403") || msg.contains("HTTP 404") => {
            tracing::warn!(
                "{repo}#{pr}: coordinator cannot comment ({msg}); forwarding the findings"
            );
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hgl_ai_config::ReviewPool;

    #[test]
    fn redirect_leaves_manual_work_and_desk_receipts_with_the_controller() {
        let controller = Seat::new("controller").unwrap();
        let mut g = Github::default();
        let pool = ReviewPool {
            seats: vec![Seat::new("r1").unwrap()],
            manual: false,
        };
        g.review_pools.insert("hgl-compiler".into(), pool.clone());
        g.review_pools.insert("hgl_spec".into(), pool);
        let make = |kind, repo: &str, body: &str| {
            let mut n = Note::new(Seat::new("w1").unwrap(), controller.clone(), kind, body);
            n.repo = Some(repo.into());
            n.issue = 17;
            n.head = Some("a".repeat(40));
            n
        };
        let mut ready = make(Kind::Ready, "hgl-compiler", "Ready");
        redirect(&g, &mut ready).unwrap();
        assert_eq!(ready.recipient.as_str(), g.coord_seat);

        for mut n in [
            make(Kind::Ready, "hgl_spec", "Spec ready"),
            make(Kind::Blocked, "hgl-compiler", "Blocked"),
            make(Kind::Question, "hgl-compiler", "Question"),
            make(Kind::Receipt, "hgl-compiler", "Desk receipt"),
        ] {
            let original = n.clone();
            redirect(&g, &mut n).unwrap();
            assert_eq!(n, original);
        }
    }
}
