//! `gh land REPO PR HEAD`: one landing at a time per repository, on any host (the landing
//! lock, a hub lease: [`crate::lock`]). The gate runs until it merges or refuses for good: it
//! is tried again while it waits for checks, a review or an update. After the merge, still
//! under the lock: `landed.log` and `main` (docs/protocol.md §8), the `landed` notes (to the
//! author's seat and to `all`; the bridges remove the task's worktrees on them), one GitHub
//! comment for the note (PROTO-1). Then, without it, the wrapper's pins when the repository
//! is one they follow (ROLE-12).
//!
//! Each step is recorded on the hub as it is done ([`crate::progress`]): a retry that finds
//! the pull request merged finishes the steps an earlier attempt left, rather than report
//! success with them undone.

use std::time::{Duration, Instant};

use hgl_ai_bus::Bus;
use hgl_ai_proto::{Kind as NoteKind, Note, Seat, utc_now_stamp};

use crate::bot::{Comment, ready_signers};
use crate::exec::{self, Gh};
use crate::gate::{self, Kind, Opts, Outcome};
use crate::lock::{LandLock, holder};
use crate::progress::{self, Progress};
use crate::table::{self, TableWord, hold_active};
use crate::{Error, Result, Settings, pins, short, state, store};

#[derive(Debug, Clone)]
pub struct LandOpts {
    pub gate: Opts,
    /// The issue the pull request closes; else the issue in its branch name.
    pub closes: Option<u64>,
    /// What `main` gained, for the note and the pin pull request; else the title.
    pub what: Option<String>,
    /// Author seats to tell; else found from the trees, the branch and the `Ready` comment.
    pub seats: Vec<Seat>,
    pub timeout: Duration,
    pub interval: Duration,
    pub lock_wait: Duration,
    /// Post the `landed` note's GitHub comment.
    pub comment: bool,
    /// Send `landed` notes.
    pub notify: bool,
    /// Raise the wrapper's pins after a merge in a repository they follow.
    pub sync_pins: bool,
    /// Extra lines for a pin commit and pull request (attribution).
    pub trailer: Option<String>,
    pub footer: Option<String>,
    /// Ask builders for the local build verdicts the gate lacks (BUILD-15); `None`: only
    /// read them.
    pub builds: Option<std::sync::Arc<gate::GateBuilds>>,
}

impl Default for LandOpts {
    fn default() -> Self {
        LandOpts {
            gate: Opts::default(),
            closes: None,
            what: None,
            seats: Vec::new(),
            timeout: Duration::from_secs(50 * 60),
            interval: Duration::from_secs(240),
            lock_wait: Duration::from_secs(2 * 3600),
            comment: true,
            notify: true,
            sync_pins: true,
            trailer: None,
            footer: None,
            builds: None,
        }
    }
}

/// What a landing did.
#[derive(Debug, Clone)]
pub struct Landing {
    pub outcome: Outcome,
    /// One line for each thing done (or, in a dry run, to be done) after the merge.
    pub after: Vec<String>,
}

/// Wait on the shared table until it says the pull request can land, before any GitHub
/// call (the shell `land-when-ready`). `Some` is a decision without GitHub: the bot's limit.
async fn wait_for_table(
    bus: &Bus,
    repo: &str,
    number: u64,
    head: &str,
    triaged: bool,
    deadline: Instant,
) -> Result<Option<Outcome>> {
    let mut shown = String::new();
    loop {
        let text = bus.state_get(state::KEY_PRS).await?.map(|e| e.value);
        match table::table_word(
            text.as_deref(),
            repo,
            number,
            head,
            triaged,
            chrono::Utc::now(),
        ) {
            TableWord::Ask(_) => return Ok(None),
            TableWord::Limit => {
                let line =
                    format!("{repo} #{number}: the review bot reports its usage limit BOTLIMIT");
                return Ok(Some(Outcome {
                    kind: Kind::BotLimit,
                    line,
                    retry: false,
                    merge_commit: None,
                    head: head.to_string(),
                }));
            }
            TableWord::NotYet(why) => {
                if Instant::now() + TABLE_POLL >= deadline {
                    return Ok(None);
                }
                if why != shown {
                    eprintln!("{repo} #{number}: the table says not yet ({why}); waiting");
                    shown = why;
                }
                tokio::time::sleep(TABLE_POLL).await;
            }
        }
    }
}

/// How often the shared table is read while it says not yet (it costs no GitHub call).
const TABLE_POLL: Duration = Duration::from_secs(60);

/// Wait while `HGL_STATE/hold` holds (ROLE-19), but not past `deadline`: `Some(value)`
/// when the hold is still active at the deadline.
async fn wait_for_hold(bus: &Bus, deadline: Instant) -> Result<Option<String>> {
    loop {
        let Some(h) = bus.state_get(state::KEY_HOLD).await? else {
            return Ok(None);
        };
        if !hold_active(&h.value, chrono::Utc::now()) {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Ok(Some(h.value.trim().to_string()));
        }
        eprintln!("hold until {}: waiting", h.value.trim());
        tokio::time::sleep(
            Duration::from_secs(60).min(deadline.saturating_duration_since(Instant::now())),
        )
        .await;
    }
}

/// The gate until it is decided: merged, or refused in a way waiting does not change, or
/// the time is up. A pull request merged by an earlier attempt whose steps after the merge
/// are not all done has them finished instead (its record, [`crate::progress`]).
pub async fn land<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    repo: &str,
    number: u64,
    head: &str,
    o: &LandOpts,
) -> Result<Landing> {
    hgl_ai_proto::check_head(head)?;
    let who = holder(s, &format!("land {repo}#{number}"));
    let lock = LandLock::acquire(bus, repo, &who, o.lock_wait).await?;
    let held = land_locked(gh, s, bus, repo, number, head, o, &lock).await;
    lock.release().await;
    let (
        Landing {
            mut outcome,
            mut after,
        },
        what,
    ) = held?;
    if !outcome.kind.landed() {
        return Ok(Landing { outcome, after });
    }
    if o.sync_pins && s.pin_sources().contains(&repo) {
        let po = pins::PinOpts {
            what: format!("{what} ({}#{number})", s.full(repo)),
            trailer: o.trailer.clone(),
            footer: o.footer.clone(),
            ..pins::PinOpts::default()
        };
        // Boxed: a pin pull request is landed by `land` itself.
        let r = Box::pin(pins::sync(gh, s, Some(bus), &po, false)).await?;
        after.push(r.line());
        if !r.ok() {
            // The record stays with the pins not done: the same command finishes them.
            outcome.kind = Kind::Partial;
            outcome.line = format!(
                "{repo} #{number} is merged, but the {} pins are not raised ({}); run the same `hgl-ai gh land` again to finish PARTIAL",
                s.wrapper,
                r.line()
            );
            return Ok(Landing { outcome, after });
        }
    }
    progress::delete(bus, repo, number).await?;
    Ok(Landing { outcome, after })
}

/// `land` under the landing lock: the gate until it decides, and after a merge the records,
/// notes and comment; also returns what `main` gained.
#[allow(clippy::too_many_arguments)]
async fn land_locked<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    repo: &str,
    number: u64,
    head: &str,
    o: &LandOpts,
    lock: &LandLock,
) -> Result<(Landing, String)> {
    if let Some(done) = resume_recorded(gh, s, bus, repo, number, o, lock).await? {
        return Ok(done);
    }
    let deadline = Instant::now() + o.timeout;
    let gate_opts = with_deadline(&o.gate, deadline);
    if let Some(outcome) = wait_for_table(bus, repo, number, head, o.gate.triaged, deadline).await?
    {
        let after = Vec::new();
        return Ok((Landing { outcome, after }, String::new()));
    }
    let mut shown = String::new();
    let outcome = loop {
        if let Some(until) = wait_for_hold(bus, deadline).await? {
            // The time is up under the hold: stop, and do not spend the call it defers.
            let outcome = Outcome {
                kind: Kind::Hold,
                line: format!(
                    "{repo} #{number}: hold still active at the deadline (hold until {until}) HOLD"
                ),
                retry: false,
                merge_commit: None,
                head: head.to_string(),
            };
            return Ok((
                Landing {
                    outcome,
                    after: Vec::new(),
                },
                String::new(),
            ));
        }
        let out = match gate::evaluate(
            gh,
            s,
            Some(bus),
            repo,
            number,
            head,
            &gate_opts,
            true,
            Some(lock),
            o.builds.as_deref(),
        )
        .await
        {
            Ok(out) => out,
            Err(Error::RateLimited(m)) if Instant::now() + o.interval < deadline => {
                eprintln!("{repo} #{number}: GitHub rate limit ({m}); trying again");
                tokio::time::sleep(o.interval).await;
                continue;
            }
            Err(e) => return Err(e),
        };
        if out.line != shown {
            eprintln!("{}", out.line);
            shown = out.line.clone();
        }
        if !out.retry || Instant::now() + o.interval >= deadline {
            break out;
        }
        tokio::time::sleep(o.interval).await;
    };
    settle(gh, s, bus, repo, number, outcome, o, lock).await
}

/// A landing whose record holds the merge commit: an earlier attempt merged and stopped
/// before every step after the merge was done. Finish them, without the gate.
async fn resume_recorded<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    repo: &str,
    number: u64,
    o: &LandOpts,
    lock: &LandLock,
) -> Result<Option<(Landing, String)>> {
    let Some(p) = progress::read(bus, repo, number).await? else {
        return Ok(None);
    };
    // Only the intent: whether the merge happened is GitHub's to say (the gate asks).
    let Some(merge) = p.merge.clone() else {
        return Ok(None);
    };
    let mut outcome = Outcome::new(
        Kind::Resumed,
        format!(
            "{repo} #{number} was merged at {} by an earlier attempt; finishing its landing MERGED",
            short(&merge)
        ),
        false,
        &p.head,
    );
    outcome.merge_commit = Some(merge);
    eprintln!("{}", outcome.line);
    let (after, what) = after_merge(gh, s, bus, repo, number, &outcome, o, lock, Some(p)).await?;
    Ok(Some((Landing { outcome, after }, what)))
}

/// The gate's options with the landing's `deadline`, which bounds the poll of an accepted
/// merge (with the lock's lease, taken at the merge: [`crate::merge::until`]).
fn with_deadline(gate: &gate::Opts, deadline: Instant) -> gate::Opts {
    gate::Opts {
        merge_until: Some(tokio::time::Instant::from_std(deadline)),
        ..*gate
    }
}

/// After the gate decided, under the lock: a merge's steps; a pull request found merged
/// has its landing finished, unless `landed.log` shows it landed (and no record is left).
#[allow(clippy::too_many_arguments)]
async fn settle<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    repo: &str,
    number: u64,
    outcome: Outcome,
    o: &LandOpts,
    lock: &LandLock,
) -> Result<(Landing, String)> {
    match outcome.kind {
        Kind::Merged => {
            let p = progress::read(bus, repo, number).await?;
            let (after, what) = after_merge(gh, s, bus, repo, number, &outcome, o, lock, p).await?;
            Ok((Landing { outcome, after }, what))
        }
        Kind::AlreadyMerged => {
            let p = progress::read(bus, repo, number).await?;
            if p.is_none() && progress::landed(bus, repo, number, None).await? {
                let after = vec![format!(
                    "landed earlier: {repo} #{number} is in {}",
                    state::KEY_LANDED
                )];
                return Ok((Landing { outcome, after }, String::new()));
            }
            // Merged, and its landing not finished (an earlier attempt stopped right after
            // the merge, or it was merged without one): finish it now.
            let pr = gate::fetch_pr(gh, s, repo, number).await?;
            let merge = pr.merge_commit.clone().unwrap_or_default();
            let head = p
                .as_ref()
                .map(|p| p.head.clone())
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| pr.head.clone());
            let mut resumed = Outcome::new(
                Kind::Resumed,
                format!(
                    "{repo} #{number} is merged at {} and its landing was not finished; finishing it MERGED",
                    short(&merge)
                ),
                false,
                &head,
            );
            resumed.merge_commit = Some(merge.clone());
            eprintln!("{}", resumed.line);
            let p = p.unwrap_or_else(|| Progress::new(&head, Some(merge)));
            let (after, what) =
                after_merge(gh, s, bus, repo, number, &resumed, o, lock, Some(p)).await?;
            Ok((
                Landing {
                    outcome: resumed,
                    after,
                },
                what,
            ))
        }
        _ => Ok((
            Landing {
                outcome,
                after: Vec::new(),
            },
            String::new(),
        )),
    }
}

/// One gate attempt under the landing lock (`gh merge-if-clean`), and after a merge the same
/// records, notes and comment as `land`, still under the lock; the pins are left to
/// `gh pins sync` or `gh land`.
pub async fn merge_once<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    repo: &str,
    number: u64,
    head: &str,
    o: &LandOpts,
) -> Result<Landing> {
    hgl_ai_proto::check_head(head)?;
    let who = holder(s, &format!("merge-if-clean {repo}#{number}"));
    let lock = LandLock::acquire(bus, repo, &who, o.lock_wait).await?;
    let held = merge_locked(gh, s, bus, repo, number, head, o, &lock).await;
    lock.release().await;
    held
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn merge_locked<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    repo: &str,
    number: u64,
    head: &str,
    o: &LandOpts,
    lock: &LandLock,
) -> Result<Landing> {
    let (Landing { outcome, mut after }, _) =
        match resume_recorded(gh, s, bus, repo, number, o, lock).await? {
            Some(done) => done,
            None => {
                let gate_opts = with_deadline(&o.gate, Instant::now() + o.timeout);
                let outcome = gate::evaluate(
                    gh,
                    s,
                    Some(bus),
                    repo,
                    number,
                    head,
                    &gate_opts,
                    true,
                    Some(lock),
                    o.builds.as_deref(),
                )
                .await?;
                settle(gh, s, bus, repo, number, outcome, o, lock).await?
            }
        };
    if outcome.kind.landed() {
        if s.pin_sources().contains(&repo) {
            after.push(format!(
                "the {} pins are not raised: run `hgl-ai gh pins sync`",
                s.wrapper
            ));
        }
        progress::delete(bus, repo, number).await?;
    }
    Ok(Landing { outcome, after })
}

/// The steps after a merge, each under the landing lock and recorded as done in the
/// landing's record as it is done: `landed.log`, `main`, the `landed` notes and their
/// comment. Steps the record (`prog`) shows done are skipped; on a resume the line is not
/// appended twice, the notes carry their first `created_at` (so the hub drops copies) and
/// a comment already posted is not posted again. Also returns what `main` gained.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn after_merge<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    repo: &str,
    number: u64,
    outcome: &Outcome,
    o: &LandOpts,
    lock: &LandLock,
    prog: Option<Progress>,
) -> Result<(Vec<String>, String)> {
    let merge = outcome.merge_commit.clone().unwrap_or_default();
    let resumed = outcome.kind == Kind::Resumed;
    let mut p = prog.unwrap_or_else(|| Progress::new(&outcome.head, Some(merge.clone())));
    p.merge = Some(merge.clone());
    let merged = format!("{repo} #{number} is merged at {}", short(&merge));
    let mut after = Vec::new();
    let mut recorded = false;
    if !p.has(progress::LANDED_LOG) {
        lock.confirm(&format!(
            "writing {}: {merged}; {}, {} and the landed notes are not written",
            state::KEY_LANDED,
            state::KEY_LANDED,
            state::KEY_MAIN
        ))
        .await?;
        append_landed(bus, repo, number, &merge).await?;
        p.mark(progress::LANDED_LOG);
        progress::write(bus, repo, number, &p).await?;
        recorded = true;
    }
    if !p.has(progress::MAIN) {
        let text = main_text(gh, s).await?;
        lock.confirm(&format!(
            "writing {}: {merged} and in {}; {} and the landed notes are not written",
            state::KEY_MAIN,
            state::KEY_LANDED,
            state::KEY_MAIN
        ))
        .await?;
        store::put_latest(bus, state::KEY_MAIN, &text).await?;
        p.mark(progress::MAIN);
        progress::write(bus, repo, number, &p).await?;
        recorded = true;
    }
    if recorded {
        after.push(format!(
            "recorded {repo} #{number} at {} in {} and {}",
            short(&merge),
            state::KEY_LANDED,
            state::KEY_MAIN
        ));
    }
    let pr = gate::fetch_pr(gh, s, repo, number).await?;
    let what = o.what.clone().unwrap_or_else(|| pr.title.clone());
    let issue = o
        .closes
        .or_else(|| state::issue_from_branch(&pr.branch, &s.branch_prefix));
    let body = state::landed_body(
        &s.controller_runtime,
        repo,
        number,
        &merge,
        &what,
        issue.filter(|i| *i != number),
    );
    if o.notify && !p.has(progress::NOTES) {
        let seats = if o.seats.is_empty() {
            author_seats(gh, s, bus, repo, &pr.branch, number, issue, false).await?
        } else {
            o.seats.clone()
        };
        let at = match &p.notes_at {
            Some(at) => at.clone(),
            None => {
                let at = utc_now_stamp();
                p.notes_at = Some(at.clone());
                progress::write(bus, repo, number, &p).await?;
                at
            }
        };
        lock.confirm(&format!(
            "sending the landed notes: {merged} and recorded in {} and {}",
            state::KEY_LANDED,
            state::KEY_MAIN
        ))
        .await?;
        after.extend(
            send_landed(
                s,
                bus,
                repo,
                number,
                issue,
                &outcome.head,
                &merge,
                &body,
                &seats,
                &at,
            )
            .await?,
        );
        p.mark(progress::NOTES);
        progress::write(bus, repo, number, &p).await?;
    }
    if o.comment && !p.has(progress::COMMENT) {
        let first = body.lines().next().unwrap_or_default();
        if resumed && commented(gh, s, repo, number, first).await? {
            after.push(format!("commented on {repo}#{number} earlier"));
        } else {
            let path = format!("repos/{}/issues/{number}/comments", s.full(repo));
            exec::write(gh, "POST", &path, &[("body", &body)]).await?;
            after.push(format!("commented on {repo}#{number}"));
        }
        p.mark(progress::COMMENT);
        progress::write(bus, repo, number, &p).await?;
    }
    Ok((after, what))
}

/// Whether the pull request has a comment whose first line is `first`.
async fn commented<G: Gh>(
    gh: &G,
    s: &Settings,
    repo: &str,
    number: u64,
    first: &str,
) -> Result<bool> {
    let list = exec::get_list(
        gh,
        &format!(
            "repos/{}/issues/{number}/comments?per_page=100",
            s.full(repo)
        ),
    )
    .await?;
    Ok(list
        .iter()
        .map(Comment::from_rest)
        .any(|c| c.body.lines().next().map(str::trim) == Some(first.trim())))
}

/// Append the landing to `landed.log`, unless the line is there already (a resumed landing).
async fn append_landed(bus: &Bus, repo: &str, number: u64, merge: &str) -> Result<()> {
    if progress::landed(bus, repo, number, Some(merge)).await? {
        return Ok(());
    }
    let line = state::landed_line(&utc_now_stamp(), repo, number, merge);
    store::append_bounded(bus, state::KEY_LANDED, &line, state::LANDED_KEEP).await
}

/// What `land` would do now, without changing anything: the gate's dry run and the steps
/// that would follow a merge. With the hub, the local build verdicts and review receipts are
/// read, and a pull request merged with its landing not finished says what would be resumed.
pub async fn plan<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: Option<&Bus>,
    repo: &str,
    number: u64,
    head: &str,
    o: &LandOpts,
) -> Result<Landing> {
    let outcome =
        gate::evaluate(gh, s, bus, repo, number, head, &o.gate, false, None, None).await?;
    let mut after = Vec::new();
    if outcome.kind == Kind::AlreadyMerged
        && let Some(b) = bus
    {
        let p = progress::read(b, repo, number).await?;
        if p.is_none() && progress::landed(b, repo, number, None).await? {
            after.push(format!(
                "landed earlier: {repo} #{number} is in {}",
                state::KEY_LANDED
            ));
        } else {
            let done = p.map(|p| p.done).unwrap_or_default();
            let left: Vec<&str> = [
                progress::LANDED_LOG,
                progress::MAIN,
                progress::NOTES,
                progress::COMMENT,
            ]
            .into_iter()
            .filter(|s| !done.iter().any(|d| d == s))
            .collect();
            after.push(format!(
                "would finish its landing: {}",
                if left.is_empty() {
                    "nothing left but the record".to_string()
                } else {
                    left.join(", ")
                }
            ));
        }
    }
    if outcome.kind == Kind::WouldMerge {
        let pr = gate::fetch_pr(gh, s, repo, number).await?;
        let issue = o
            .closes
            .or_else(|| state::issue_from_branch(&pr.branch, &s.branch_prefix));
        after.push(format!(
            "would append {repo} #{number} to {} and rewrite {}",
            state::KEY_LANDED,
            state::KEY_MAIN
        ));
        if o.notify {
            let to = if o.seats.is_empty() {
                "the author's seat (from the trees, branch or Ready comment)".to_string()
            } else {
                o.seats
                    .iter()
                    .map(Seat::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let task = issue
                .map(|i| format!("{repo}-{i}"))
                .unwrap_or_else(|| format!("{repo}-{number}"));
            after.push(format!("would send landed for {task} to {to} and to all"));
        }
        if o.comment {
            after.push(format!("would comment on {repo}#{number}"));
        }
        if o.sync_pins && s.pin_sources().contains(&repo) {
            after.push(format!("would raise the {} pins", s.wrapper));
        }
    }
    Ok(Landing { outcome, after })
}

/// Append the landing to `landed.log` (unless it is there) and rewrite `main`. Under the
/// landing lock, the lock is confirmed ours right before each write; a lost lock stops with
/// what is left undone.
pub async fn record<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    repo: &str,
    number: u64,
    merge: &str,
    lock: Option<&LandLock>,
) -> Result<String> {
    let merged = format!("{repo} #{number} is merged at {}", short(merge));
    if let Some(l) = lock {
        l.confirm(&format!(
            "writing {}: {merged}; {}, {} and the landed notes are not written",
            state::KEY_LANDED,
            state::KEY_LANDED,
            state::KEY_MAIN
        ))
        .await?;
    }
    append_landed(bus, repo, number, merge).await?;
    let text = main_text(gh, s).await?;
    if let Some(l) = lock {
        l.confirm(&format!(
            "writing {}: {merged} and in {}; {} and the landed notes are not written",
            state::KEY_MAIN,
            state::KEY_LANDED,
            state::KEY_MAIN
        ))
        .await?;
    }
    store::put_latest(bus, state::KEY_MAIN, &text).await?;
    Ok(format!(
        "recorded {repo} #{number} at {} in {} and {}",
        short(merge),
        state::KEY_LANDED,
        state::KEY_MAIN
    ))
}

/// Tries of a read of `main` that failed for a reason a retry may cure.
const MAIN_TRIES: u32 = 3;

/// The heads of `main` of every configured repository and the wrapper's pins, as text. A
/// repository whose `main` cannot be read is never left out: a read that fails (HTTP 5xx,
/// a timeout, an unreadable answer) is tried again, then the whole text fails. Only a
/// repository with no `main` at all (HTTP 404) is left out, with a warning.
pub async fn main_text<G: Gh>(gh: &G, s: &Settings) -> Result<String> {
    let mut mains = Vec::new();
    for repo in &s.repos {
        if let Some(sha) = main_of(gh, s, repo).await? {
            mains.push((repo.clone(), sha));
        }
    }
    let mut pinned = Vec::new();
    for p in &s.pins {
        if let Some(sha) = pins::submodule_at(gh, s, &s.wrapper, &p.path, "main").await? {
            pinned.push((p.path.clone(), sha));
        }
    }
    Ok(state::render_main(
        &utc_now_stamp(),
        &mains,
        &s.wrapper,
        &pinned,
    ))
}

async fn main_of<G: Gh>(gh: &G, s: &Settings, repo: &str) -> Result<Option<String>> {
    let mut pause = s.api_retry;
    let mut tries = 0;
    loop {
        tries += 1;
        match gate::main_head(gh, s, repo).await {
            Ok(sha) => return Ok(Some(sha)),
            Err(e @ (Error::RateLimited(_) | Error::Auth(_))) => return Err(e),
            Err(e) if exec::is_not_found(&e) => {
                eprintln!("{repo}: no main ({e}); left out of {}", state::KEY_MAIN);
                return Ok(None);
            }
            Err(e) if tries < MAIN_TRIES => {
                eprintln!("{repo}: reading main: {e}; trying again in {pause:?}");
                tokio::time::sleep(pause).await;
                pause *= 2;
            }
            Err(e) => return Err(e),
        }
    }
}

/// The author's seats: the trees registry, the branch name, or the `Ready` comment's signer
/// (a seat some host advertises).
/// The seats for `landed` (every implementor tree of the task), or with `current` for a
/// verdict (only the task's current seat, [`state::current_seats`]).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn author_seats<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    repo: &str,
    branch: &str,
    number: u64,
    issue: Option<u64>,
    current: bool,
) -> Result<Vec<Seat>> {
    let trees = bus.trees().await?;
    let known: Vec<String> = bus
        .hosts()
        .await?
        .iter()
        .flat_map(|h| h.seats.iter().map(|r| r.seat.to_string()))
        .collect();
    let list = exec::get_list(
        gh,
        &format!(
            "repos/{}/issues/{number}/comments?per_page=100",
            s.full(repo)
        ),
    )
    .await?;
    let comments: Vec<Comment> = list.iter().map(Comment::from_rest).collect();
    let pick = if current {
        state::current_seats
    } else {
        state::author_seats
    };
    Ok(pick(
        &trees,
        repo,
        branch,
        issue.unwrap_or(number),
        &known,
        &ready_signers(&comments),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn send_landed(
    s: &Settings,
    bus: &Bus,
    repo: &str,
    number: u64,
    issue: Option<u64>,
    head: &str,
    merge: &str,
    body: &str,
    seats: &[Seat],
    created_at: &str,
) -> Result<Vec<String>> {
    let mut sent = Vec::new();
    // The issue's worktrees, and the review trees named by the pull request's number.
    let mut tasks = vec![issue.unwrap_or(number)];
    if !tasks.contains(&number) {
        tasks.push(number);
    }
    for (i, task) in tasks.iter().enumerate() {
        let mut to: Vec<Seat> = if i == 0 { seats.to_vec() } else { Vec::new() };
        to.push(Seat::broadcast());
        for seat in to {
            let mut n = Note::new(s.controller.clone(), seat.clone(), NoteKind::Landed, body);
            // The same stamp on a resend: the same message id, so the hub drops the copy.
            n.created_at = created_at.to_string();
            n.repo = Some(repo.to_string());
            n.issue = *task;
            n.head = Some(head.to_string()).filter(|h| hgl_ai_proto::check_head(h).is_ok());
            n.base = Some(merge.to_string());
            bus.send(&n).await?;
            sent.push(format!("sent landed for {repo}-{task} to {seat}"));
        }
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::fake::FakeGh;
    use crate::testutil::{fixture, settings, without_receipts};

    const HEAD: &str = "1f461fa77bba922fe81383df4c475feb58b7c233";

    /// `main` never leaves a repository out on a failure a retry may cure (Carbon's #5): it
    /// is read again, then the whole text fails; only a repository with no `main` is left out.
    #[tokio::test]
    async fn main_is_whole_or_not_written() {
        let mut s = settings();
        s.repos = vec!["alpha".into(), "beta".into()];
        s.pins = vec![];
        let sha = |c: char| {
            format!(
                "{{\"object\":{{\"sha\":\"{}\"}}}}",
                c.to_string().repeat(40)
            )
        };
        // One 502, then an answer.
        let gh = FakeGh::default()
            .on("alpha/git/ref/heads/main", &sha('a'))
            .push_fail_once("beta/git/ref/heads/main", "gh: Server Error (HTTP 502)")
            .on("beta/git/ref/heads/main", &sha('b'));
        let t = main_text(&gh, &s).await.unwrap();
        assert!(t.contains(&format!("alpha main {}", "a".repeat(40))), "{t}");
        assert!(t.contains(&format!("beta main {}", "b".repeat(40))), "{t}");
        // Failing every time: no text, not a text without beta.
        let gh = FakeGh::default()
            .on("alpha/git/ref/heads/main", &sha('a'))
            .fail("beta/git/ref/heads/main", "gh: Server Error (HTTP 502)");
        let e = main_text(&gh, &s).await.unwrap_err();
        assert!(e.to_string().contains("HTTP 502"), "{e}");
        assert_eq!(
            gh.calls().iter().filter(|c| c.contains("beta/")).count(),
            3,
            "tried three times"
        );
        // A repository with no main is left out.
        let gh = FakeGh::default()
            .on("alpha/git/ref/heads/main", &sha('a'))
            .fail("beta/git/ref/heads/main", "gh: Not Found (HTTP 404)");
        let t = main_text(&gh, &s).await.unwrap();
        assert!(!t.contains("beta"), "{t}");
    }

    #[tokio::test]
    async fn plan_lists_the_steps_after_a_merge() {
        let main = format!("{{\"object\":{{\"sha\":\"{}\"}}}}", "0a".repeat(20));
        let gh = crate::exec::ReadOnly(
            FakeGh::default()
                .on("pulls/7/files", &fixture("pr-files.json"))
                .on("Accept: application/vnd.github.v3.diff", &fixture("pr.diff"))
                .on("pulls/7", &fixture("pr-open.json"))
                .on("issues/7/comments", &fixture("comments-clean.json"))
                .on("compare/", "{\"ahead_by\":0,\"files\":[]}")
                .on("check-runs", "{\"check_runs\":[]}")
                .on("/status?per_page", "{\"statuses\":[]}")
                .on("git/ref/heads/main", &main)
                .on("graphql", "{\"data\":{\"repository\":{\"pullRequest\":{\"reviewThreads\":{\"nodes\":[]}}}}}"),
        );
        // Without a hub, no receipt can be read: blocked, saying so.
        let l = plan(
            &gh,
            &settings(),
            None,
            "hgl_spec",
            7,
            HEAD,
            &LandOpts::default(),
        )
        .await
        .unwrap();
        assert_eq!(l.outcome.kind, Kind::Blocked, "{}", l.outcome.line);
        assert!(
            l.outcome
                .line
                .contains("the authors' families not read (no hub connection)"),
            "{}",
            l.outcome.line
        );
        let s = without_receipts(settings(), &["hgl_spec", "hgl-compiler"]);
        let l = plan(&gh, &s, None, "hgl_spec", 7, HEAD, &LandOpts::default())
            .await
            .unwrap();
        assert_eq!(l.outcome.kind, Kind::WouldMerge, "{}", l.outcome.line);
        assert_eq!(l.after.len(), 3, "{:?}", l.after);
        assert!(
            l.after[1].contains("landed for hgl_spec-980"),
            "{:?}",
            l.after
        );
        let l = plan(
            &gh,
            &s,
            None,
            "hgl-compiler",
            7,
            HEAD,
            &LandOpts {
                gate: Opts::default(),
                ..LandOpts::default()
            },
        )
        .await
        .unwrap();
        // The compiler needs its local build verdicts, which a dry run does not read.
        assert_eq!(l.outcome.kind, Kind::Wait, "{}", l.outcome.line);
        assert!(l.after.is_empty());
    }
}
