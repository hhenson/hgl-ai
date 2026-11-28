//! Route routine review notes against the coordinator's current pull request batch.

use std::collections::{BTreeMap, BTreeSet};

use hgl_ai_bus::{Bus, BusError, Lease, StreamNote, check_note};
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
    if note.recipient.as_str() != g.controller {
        return Ok(());
    }
    let mut routed = note.clone();
    if routed.kind == Kind::Receipt {
        let Some(receipt) = state::parse_receipt(&routed.body) else {
            return Ok(());
        };
        routed.repo.get_or_insert_with(|| receipt.repo.clone());
        if routed.issue == 0 {
            routed.issue = receipt.pr;
        }
        if !state::matches_receipt(&routed, &receipt) {
            return Ok(());
        }
    } else if routed.kind != Kind::Ready {
        return Ok(());
    }
    if !routed
        .repo
        .as_deref()
        .is_some_and(|repo| g.routes_notes(repo))
    {
        return Ok(());
    }
    let seat = Seat::new(&g.coord_seat)?;
    if seat.is_reserved() || seat == note.recipient {
        return Err(invalid(
            "github.coord_seat must be a separate seat for review routing",
        ));
    }
    check_note(&routed)?; // The coordinator may need to forward it to the controller.
    routed.recipient = seat;
    check_note(&routed)?;
    *note = routed;
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
    receipts: BTreeMap<(String, Seat), (u64, Verdict)>,
    sendbacks: BTreeSet<String>,
    earlier: BTreeMap<Seat, EarlierReview>,
    sent_back: bool,
}

struct EarlierReview {
    seq: u64,
    focused_head: Option<String>,
}

impl History {
    fn record(
        &mut self,
        item: StreamNote,
        pulls: &[Pull<'_>],
        settings: &Settings,
        hosts: &[HostRecord],
    ) {
        let n = &item.note;
        if n.recipient.is_reserved()
            || !matches!(n.kind, Kind::Review | Kind::Receipt | Kind::Sendback)
        {
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
                let past = self.pulls.entry(key).or_default();
                past.sent_back = true;
                past.sendbacks.insert(head.clone());
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
            Kind::Receipt => {
                if let Some(receipt) = state::routed_receipt(n)
                    && state::reviewer_family_matches(hosts, &receipt) == Some(true)
                {
                    self.pulls
                        .entry(key)
                        .or_default()
                        .receipts
                        .insert((head, n.sender.clone()), (item.seq, receipt.verdict));
                }
            }
            _ => {}
        }
    }

    fn keep_current(&mut self, pulls: &[Pull<'_>]) {
        for pull in pulls {
            let key = TaskKey {
                repo: pull.pr.row.repo.clone(),
                issue: pull.number(),
            };
            let Some(past) = self.pulls.get_mut(&key) else {
                continue;
            };
            let head = &pull.pr.row.head;
            past.receipts.retain(|(sha, seat), (seq, verdict)| {
                if sha == head {
                    return true;
                }
                if past.reviews.contains(&(sha.clone(), seat.clone()))
                    && past
                        .earlier
                        .get(seat)
                        .is_none_or(|earlier| *seq > earlier.seq)
                {
                    let focused_head = (key.repo == "hgl-ai"
                        && *verdict == Verdict::Findings
                        && past.sendbacks.contains(sha))
                    .then(|| sha.clone());
                    past.earlier.insert(
                        seat.clone(),
                        EarlierReview {
                            seq: *seq,
                            focused_head,
                        },
                    );
                }
                false
            });
            past.sendbacks.clear();
            past.reviews
                .retain(|(sha, _)| sha == head && pull.current_head(&key.repo, head));
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
    Forward {
        to: Seat,
    },
    Review {
        pr: u64,
        to: Seat,
        earlier: Option<String>,
        url: String,
    },
    Sendback {
        pr: u64,
        issue: u64,
        to: Seat,
        fallback: Seat,
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
    fn forward(&self) -> Action {
        Action::Forward {
            to: self.settings.controller.clone(),
        }
    }

    fn plan(&self, n: &Note) -> Action {
        let (Some(key), Some(head)) = (n.task(), n.head.as_deref()) else {
            return self.forward();
        };
        let Some(pool) = self
            .settings
            .review_pools
            .get(&key.repo)
            .filter(|p| p.automatic(&key.repo, &self.settings.spec_repos))
        else {
            return self.forward();
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
            return self.forward();
        };
        if matching.next().is_some() {
            return self.forward();
        }
        let pr = pull.number();
        let authors = pull.authors(&self.trees);
        if authors.is_empty() {
            return self.forward();
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
                self.pick_reviewer(pull, &pool.seats, &authors, past)
            }
            Kind::Receipt => {
                let Some(receipt) = state::routed_receipt(n) else {
                    return self.forward();
                };
                if state::reviewer_family_matches(&self.hosts, &receipt) != Some(true)
                    || !past.reviews.contains(&(receipt.head, n.sender.clone()))
                    || authors.contains(&receipt.family)
                    || self
                        .trees
                        .iter()
                        .any(|t| pull.matches_tree(t) && t.seat == n.sender)
                {
                    return self.forward();
                }
                match receipt.verdict {
                    Verdict::Clean => Action::Candidate { pr },
                    Verdict::Findings if !past.sent_back && !receipt.evidence.trim().is_empty() => {
                        if let [to] = current.as_slice() {
                            Action::Sendback {
                                pr,
                                issue: pull.issue,
                                to: to.clone(),
                                fallback: self.settings.controller.clone(),
                            }
                        } else {
                            self.forward()
                        }
                    }
                    Verdict::Findings => self.forward(),
                }
            }
            _ => self.forward(),
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
        pool: &[Seat],
        authors: &[Family],
        past: &PullHistory,
    ) -> Action {
        let earlier = past
            .earlier
            .iter()
            .filter(|(seat, _)| pool.contains(seat) && self.eligible(pull, seat, authors))
            .max_by_key(|(_, earlier)| earlier.seq);
        if let Some((seat, earlier)) = earlier {
            return Action::Review {
                pr: pull.number(),
                to: seat.clone(),
                earlier: earlier.focused_head.clone(),
                url: pull.pr.url.clone(),
            };
        }
        pool.iter()
            .filter(|seat| self.eligible(pull, seat, authors))
            .min_by_key(|seat| (self.history.load(seat), *seat))
            .map_or(self.forward(), |to| Action::Review {
                pr: pull.number(),
                to: to.clone(),
                earlier: None,
                url: pull.pr.url.clone(),
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
        let mut history = History::default();
        let mut from = 1;
        while let (notes, Some(last)) = bus.read_note_history_page(from, HISTORY_PAGE).await? {
            for item in notes {
                history.record(item, &pulls, settings, &hosts);
            }
            from = last + 1;
        }
        history.keep_current(&pulls);

        Ok(Facts {
            settings,
            pulls,
            hosts,
            trees,
            history,
        })
    }
}

// Keep the selected delivery and digest line together before any side effect. The
// source body remains in HGL_NOTES, so even a large receipt fits the checkpoint.
#[derive(Serialize, Deserialize)]
struct Pending {
    seq: u64,
    action: Action,
    line: Option<String>,
}

impl Pending {
    fn new(item: &StreamNote, action: Action) -> Self {
        let label = |pr| {
            format!(
                "{}#{pr} at {}",
                item.note.repo.as_deref().unwrap_or(""),
                item.note.head.as_deref().unwrap_or("")
            )
        };
        let line = match &action {
            Action::Review { pr, to, .. } => Some(format!("{} → review by {to}", label(*pr))),
            Action::Sendback { pr, to, .. } => Some(format!("{} → sendback to {to}", label(*pr))),
            Action::Candidate { pr } => Some(format!(
                "{} → landing candidate (clean receipt by {})",
                label(*pr),
                item.note.sender
            )),
            Action::Forward { .. } | Action::AlreadyRouted => None,
        };
        Self {
            seq: item.seq,
            action,
            line,
        }
    }

    fn recipient(&self) -> Option<&Seat> {
        match &self.action {
            Action::Forward { to } | Action::Review { to, .. } => Some(to),
            // Either the implementor got the sendback or the controller got the receipt.
            Action::Sendback { .. } | Action::Candidate { .. } | Action::AlreadyRouted => None,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Checkpoint {
    after: u64,
    digest: Vec<String>,
    #[serde(default)]
    outbox: Option<Pending>,
    // The previous checkpoint format kept a tentative plan. Recover any message
    // it already published before processing the original input again.
    #[serde(rename = "pending", skip_serializing_if = "Option::is_none")]
    legacy: Option<(u64, serde_json::Value)>,
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

    fn finish(&mut self, seq: u64, line: Option<String>) {
        self.after = seq;
        self.outbox = None;
        if let Some(line) = line
            && !self.digest.contains(&line)
        {
            self.digest.push(line);
        }
    }

    async fn flush(
        &mut self,
        bus: &Bus,
        settings: &Settings,
        lease: &mut Lease,
        revision: &mut u64,
        retry: bool,
    ) -> Result<()> {
        if !self.digest.is_empty() {
            let id = format!("coord-routing-digest-{}", self.after);
            if !retry
                || bus
                    .retained_note(Some(&settings.controller), self.after + 1, &id)
                    .await?
                    .is_none()
            {
                let note = Note::new(
                    settings.coord_seat.clone(),
                    settings.controller.clone(),
                    Kind::Note,
                    format!("[coord] Review routing\n\n- {}\n", self.digest.join("\n- ")),
                );
                lease.refresh().await?;
                bus.send_with_id(&note, &id).await?;
            }
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
    if settings.coord_seat == settings.controller || settings.coord_seat.is_reserved() {
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
    c.flush(bus, settings, lease, &mut revision, true).await?;
    let inbox = std::slice::from_ref(&settings.coord_seat);
    if let Some((seq, action)) = c.legacy.take() {
        let id = format!("coord-routing-{seq}");
        if let Some(sent) = bus.retained_note(None, seq + 1, &id).await? {
            let note = sent.note;
            let line = match note.kind {
                Kind::Review | Kind::Sendback if note.sender == settings.coord_seat => {
                    let pr = action
                        .get("Sendback")
                        .and_then(|v| v["pr"].as_u64())
                        .unwrap_or(note.issue);
                    let route = if note.kind == Kind::Review {
                        "review by"
                    } else {
                        "sendback to"
                    };
                    Some(format!(
                        "{}#{pr} at {} → {route} {}",
                        note.repo.as_deref().unwrap_or(""),
                        note.head.as_deref().unwrap_or(""),
                        note.recipient
                    ))
                }
                _ => None,
            };
            c.finish(seq, line);
        }
        c.save(bus, lease, &mut revision).await?;
    }
    if let Some(pending) = &c.outbox {
        let seq = pending.seq;
        let id = format!("coord-routing-{seq}");
        let sent = bus.retained_note(pending.recipient(), seq + 1, &id).await?;
        let line = if let Some(sent) = sent {
            pending.line.clone().filter(|_| {
                matches!(
                    (&pending.action, sent.note.kind),
                    (Action::Review { .. }, Kind::Review)
                        | (Action::Sendback { .. }, Kind::Sendback)
                )
            })
        } else {
            let (notes, _) = bus.read_page(inbox, seq, 1).await?;
            match notes.first().filter(|item| item.seq == seq) {
                Some(item) => execute(gh, settings, bus, lease, item, pending).await?.0,
                None => {
                    tracing::warn!(seq, "routing source is no longer retained");
                    None
                }
            }
        };
        c.finish(seq, line);
        c.save(bus, lease, &mut revision).await?;
    }
    let (notes, Some(last)) = bus.read_page(inbox, c.after + 1, ROUND_NOTES).await? else {
        return c.flush(bus, settings, lease, &mut revision, false).await;
    };
    let mut facts = if settings.review_pools.is_empty() {
        None
    } else {
        Some(Facts::load(bus, settings, infos).await?)
    };
    for item in notes {
        let mut action = facts.as_ref().map_or_else(
            || Action::Forward {
                to: settings.controller.clone(),
            },
            |facts| facts.plan(&item.note),
        );
        if let Action::Sendback { pr, to, .. } = &mut action {
            let repo = item
                .note
                .repo
                .as_deref()
                .expect("sendback has a repository");
            let facts = facts.as_ref().expect("sendback has routing facts");
            let pull = facts
                .pulls
                .iter()
                .find(|p| p.pr.row.repo == repo && p.number() == *pr)
                .expect("sendback has a pull request");
            if let [current] = pull.current_seats(&bus.trees().await?).as_slice() {
                to.clone_from(current);
            } else {
                action = facts.forward();
            }
        }
        c.outbox = Some(Pending::new(&item, action));
        c.save(bus, lease, &mut revision).await?;
        let (line, sent) =
            execute(gh, settings, bus, lease, &item, c.outbox.as_ref().unwrap()).await?;
        if let (Some(facts), Some(sent)) = (&mut facts, sent) {
            facts
                .history
                .record(sent, &facts.pulls, settings, &facts.hosts);
        }
        c.finish(item.seq, line);
        c.save(bus, lease, &mut revision).await?;
    }
    if c.after != last {
        c.after = last;
        c.save(bus, lease, &mut revision).await?;
    }
    c.flush(bus, settings, lease, &mut revision, false).await
}

async fn execute<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    lease: &mut Lease,
    item: &StreamNote,
    pending: &Pending,
) -> Result<(Option<String>, Option<StreamNote>)> {
    let n = &item.note;
    let repo = n.repo.as_deref().unwrap_or("");
    let head = n.head.as_deref().unwrap_or("");
    let mut line = pending.line.clone();
    let note = match &pending.action {
        Action::Forward { to } => forward(to, item),
        Action::Review {
            pr,
            to,
            earlier,
            url,
        } => {
            let prior = earlier.as_ref().map_or_else(String::new, |sha| {
                format!("\nEarlier reviewed head: {sha}. Check the fixes and their diff if the changes are limited to the findings (hgl-ai#118).")
            });
            let body = format!(
                "[coord] Review: {repo}#{pr} at {}\n\nReady from {}.\n{url}\n{prior}",
                short(head),
                n.sender,
            );
            routed_note(to, n, Kind::Review, *pr, body)
        }
        Action::Sendback {
            pr,
            issue,
            to,
            fallback,
        } => {
            let receipt = state::routed_receipt(n).expect("sendback comes from a review receipt");
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
                n.recipient.as_str(),
                Kind::Sendback,
                repo,
                *pr,
                head,
            );
            let sendback = routed_note(to, n, Kind::Sendback, *issue, body);
            if check_note(&sendback).is_ok()
                && comment(gh, s, lease, item, *pr, &sendback.body).await?
            {
                sendback
            } else {
                line = None;
                forward(fallback, item)
            }
        }
        Action::Candidate { .. } | Action::AlreadyRouted => return Ok((line, None)),
    };
    lease.refresh().await?;
    let sent = bus
        .send_with_id(&note, &format!("coord-routing-{}", item.seq))
        .await?;
    Ok((
        line,
        Some(StreamNote {
            seq: sent.seq,
            note,
        }),
    ))
}

fn forward(to: &Seat, item: &StreamNote) -> Note {
    let note = Note {
        recipient: to.clone(),
        ..item.note.clone()
    };
    if let Err(e) = check_note(&note) {
        return Note::new(
            item.note.recipient.clone(),
            to.clone(),
            Kind::Note,
            format!(
                "[coord] Cannot forward note at HGL_NOTES sequence {}: {e}",
                item.seq
            ),
        );
    }
    note
}

fn routed_note(to: &Seat, source: &Note, kind: Kind, issue: u64, body: String) -> Note {
    let mut note = Note::new(source.recipient.clone(), to.clone(), kind, body);
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
    item: &StreamNote,
    pr: u64,
    body: &str,
) -> Result<bool> {
    let repo = item
        .note
        .repo
        .as_deref()
        .expect("sendback has a repository");
    let head = item.note.head.as_deref().expect("sendback has a head");
    let path = format!("repos/{}/issues/{pr}/comments", s.full(repo));
    let marker = format!("<!-- hgl-ai coordinator sendback {head} {} -->", item.seq);
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
    if current.head != head
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
    use hgl_ai_proto::ReviewReceipt;

    #[test]
    fn redirect_leaves_manual_work_and_desk_receipts_with_the_controller() {
        let controller = Seat::new("controller").unwrap();
        let mut g = Github::default();
        let pool = ReviewPool {
            seats: vec![Seat::new("r1").unwrap()],
            manual: false,
        };
        g.review_pools.insert("hgl-compiler".into(), pool.clone());
        for repo in ["hgl_spec", "hgl_core", "custom-spec"] {
            g.review_pools.insert(repo.into(), pool.clone());
        }
        g.repos.push("custom-spec".into());
        g.spec_repos.push("custom-spec".into());
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
            make(Kind::Ready, "hgl_core", "Core ready"),
            make(Kind::Ready, "custom-spec", "Custom spec ready"),
            make(Kind::Blocked, "hgl-compiler", "Blocked"),
            make(Kind::Question, "hgl-compiler", "Question"),
            make(Kind::Receipt, "hgl-compiler", "Desk receipt"),
        ] {
            let original = n.clone();
            redirect(&g, &mut n).unwrap();
            assert_eq!(n, original);
        }
    }
    #[test]
    fn redirect_restores_missing_receipt_metadata_and_checks_encoded_size() {
        let mut g = Github {
            coord_seat: "longer-than-controller".into(),
            ..Github::default()
        };
        g.review_pools.insert(
            "hgl-compiler".into(),
            ReviewPool {
                seats: vec![Seat::new("r1").unwrap()],
                manual: false,
            },
        );
        let reviewer = Seat::new("r1").unwrap();
        let head = "a".repeat(40);
        let receipt = ReviewReceipt {
            repo: "hgl-compiler".into(),
            pr: 17,
            head: head.clone(),
            main: "b".repeat(40),
            reviewer: reviewer.clone(),
            family: Family::Claude,
            model: "claude-test".into(),
            verdict: Verdict::Clean,
            evidence: String::new(),
            limitations: String::new(),
        };
        for (repo, issue) in [(None, 17), (Some("hgl-compiler".into()), 0), (None, 0)] {
            let mut note = Note::new(
                reviewer.clone(),
                Seat::new("controller").unwrap(),
                Kind::Receipt,
                serde_json::to_string(&receipt).unwrap(),
            );
            note.head = Some(head.clone());
            note.repo = repo;
            note.issue = issue;
            redirect(&g, &mut note).unwrap();
            assert_eq!(note.recipient.as_str(), g.coord_seat);
            assert_eq!(state::routed_receipt(&note), Some(receipt.clone()));
        }

        let mut ready = Note::new(reviewer, Seat::new("controller").unwrap(), Kind::Ready, "x");
        ready.repo = Some(receipt.repo);
        ready.issue = receipt.pr;
        ready.head = Some(head);
        let available = hgl_ai_bus::MAX_NOTE_BYTES - serde_json::to_vec(&ready).unwrap().len();
        ready.body.push_str(&"x".repeat(available));
        check_note(&ready).unwrap();
        assert!(redirect(&g, &mut ready).is_err());
        assert_eq!(ready.recipient.as_str(), "controller");
    }

    #[test]
    fn earlier_review_retains_focus_only_for_hgl_ai_findings_with_a_sendback() {
        let head = "a".repeat(40);
        let old = "b".repeat(40);
        let seat = Seat::new("r1").unwrap();
        for (repo, verdict, sendback, focused) in [
            ("hgl-ai", Verdict::Findings, true, true),
            ("hgl-ai", Verdict::Findings, false, false),
            ("hgl-ai", Verdict::Clean, true, false),
            ("example", Verdict::Findings, true, false),
        ] {
            let key = TaskKey {
                repo: repo.into(),
                issue: 7,
            };
            let (infos, _, _) = crate::table::parse_batch(
                &serde_json::json!({"data": {
                    "rateLimit": {"remaining": 1000},
                    "r0": {"pullRequests": {"nodes": [
                        {"number": 7, "headRefOid": head, "baseRefName": "main"}
                    ]}}
                }}),
                std::slice::from_ref(&key.repo),
                "bot",
                "",
            )
            .unwrap();
            let mut history = History::default();
            let past = history.pulls.entry(key.clone()).or_default();
            past.reviews.insert((old.clone(), seat.clone()));
            past.reviews.insert(("c".repeat(40), seat.clone()));
            past.receipts
                .insert((old.clone(), seat.clone()), (1, verdict));
            if sendback {
                past.sendbacks.insert(old.clone());
            }
            history.keep_current(&[Pull {
                pr: &infos[0],
                issue: 7,
            }]);
            assert_eq!(history.load(&seat), 0);
            let past = &history.pulls[&key];
            assert!(past.reviews.is_empty() && past.receipts.is_empty());
            assert_eq!(
                past.earlier[&seat].focused_head.as_ref(),
                focused.then_some(&old)
            );
        }
    }
}
