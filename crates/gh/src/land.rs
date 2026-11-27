//! `gh land REPO PR HEAD`: one landing at a time per repository (the landing lock). The gate
//! runs until it merges or refuses for good: it is tried again while it waits for checks, a
//! review or an update. After the merge: `landed.log` and `main` (docs/protocol.md §8), the
//! `landed` notes (to the author's seat and to `all`; the bridges remove the task's
//! worktrees on them), one GitHub comment for the note (PROTO-1), then the wrapper's pins
//! when the repository is one they follow (ROLE-10).

use std::time::{Duration, Instant};

use hgl_ai_bus::Bus;
use hgl_ai_proto::{Kind as NoteKind, Note, Seat, utc_now_stamp};

use crate::bot::{Comment, ready_signers};
use crate::exec::{self, Gh};
use crate::gate::{self, Kind, Opts, Outcome};
use crate::lock::Lock;
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

/// Wait while `HGL_STATE/hold` holds (ROLE-17), but not past `deadline`.
async fn wait_for_hold(bus: &Bus, deadline: Instant) -> Result<()> {
    loop {
        let Some(h) = bus.state_get(state::KEY_HOLD).await? else {
            return Ok(());
        };
        if !hold_active(&h.value, chrono::Utc::now()) || Instant::now() >= deadline {
            return Ok(());
        }
        eprintln!("hold until {}: waiting", h.value.trim());
        tokio::time::sleep(
            Duration::from_secs(60).min(deadline.saturating_duration_since(Instant::now())),
        )
        .await;
    }
}

/// The gate until it is decided: merged, or refused in a way waiting does not change, or
/// the time is up.
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
    let lock = Lock::acquire(&s.lock_path(repo), o.lock_wait).await?;
    let deadline = Instant::now() + o.timeout;
    if let Some(outcome) = wait_for_table(bus, repo, number, head, o.gate.triaged, deadline).await?
    {
        return Ok(Landing {
            outcome,
            after: Vec::new(),
        });
    }
    let mut shown = String::new();
    let outcome = loop {
        wait_for_hold(bus, deadline).await?;
        let out = match gate::evaluate(gh, s, Some(bus), repo, number, head, &o.gate, true).await {
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
    if outcome.kind != Kind::Merged {
        return Ok(Landing {
            outcome,
            after: Vec::new(),
        });
    }
    let (mut after, what) = after_merge(gh, s, bus, repo, number, &outcome, o).await?;
    drop(lock);
    if o.sync_pins && s.pin_sources().contains(&repo) {
        let what = format!("{what} ({}#{number})", s.full(repo));
        let po = pins::PinOpts {
            what,
            trailer: o.trailer.clone(),
            footer: o.footer.clone(),
            ..pins::PinOpts::default()
        };
        // Boxed: a pin pull request is landed by `land` itself.
        after.push(
            Box::pin(pins::sync(gh, s, Some(bus), &po, false))
                .await?
                .line(),
        );
    }
    Ok(Landing { outcome, after })
}

/// One gate attempt under the landing lock (`gh merge-if-clean`), and after a merge the same
/// records, notes and comment as `land`; the pins are left to `gh pins sync` or `gh land`.
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
    let _lock = Lock::acquire(&s.lock_path(repo), o.lock_wait).await?;
    let outcome = gate::evaluate(gh, s, Some(bus), repo, number, head, &o.gate, true).await?;
    if outcome.kind != Kind::Merged {
        return Ok(Landing {
            outcome,
            after: Vec::new(),
        });
    }
    let (mut after, _) = after_merge(gh, s, bus, repo, number, &outcome, o).await?;
    if s.pin_sources().contains(&repo) {
        after.push(format!(
            "the {} pins are not raised: run `hgl-ai gh pins sync`",
            s.wrapper
        ));
    }
    Ok(Landing { outcome, after })
}

/// `landed.log`, `main`, the `landed` notes and their comment; also returns what `main` gained.
async fn after_merge<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    repo: &str,
    number: u64,
    outcome: &Outcome,
    o: &LandOpts,
) -> Result<(Vec<String>, String)> {
    let merge = outcome.merge_commit.clone().unwrap_or_default();
    let mut after = vec![record(gh, s, bus, repo, number, &merge).await?];
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
    if o.notify {
        let seats = if o.seats.is_empty() {
            author_seats(gh, s, bus, repo, &pr.branch, number, issue).await?
        } else {
            o.seats.clone()
        };
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
            )
            .await?,
        );
    }
    if o.comment {
        let path = format!("repos/{}/issues/{number}/comments", s.full(repo));
        exec::write(gh, "POST", &path, &[("body", &body)]).await?;
        after.push(format!("commented on {repo}#{number}"));
    }
    Ok((after, what))
}

/// What `land` would do now, without changing anything: the gate's dry run and the steps
/// that would follow a merge.
pub async fn plan<G: Gh>(
    gh: &G,
    s: &Settings,
    repo: &str,
    number: u64,
    head: &str,
    o: &LandOpts,
) -> Result<Landing> {
    let outcome = gate::evaluate(gh, s, None, repo, number, head, &o.gate, false).await?;
    let mut after = Vec::new();
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

/// Append the landing to `landed.log` and rewrite `main`.
pub async fn record<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    repo: &str,
    number: u64,
    merge: &str,
) -> Result<String> {
    let line = state::landed_line(&utc_now_stamp(), repo, number, merge);
    store::append_bounded(bus, state::KEY_LANDED, &line, state::LANDED_KEEP).await?;
    refresh_main(gh, s, bus).await?;
    Ok(format!(
        "recorded {repo} #{number} at {} in {} and {}",
        short(merge),
        state::KEY_LANDED,
        state::KEY_MAIN
    ))
}

/// The heads of `main` of every configured repository and the wrapper's pins, as text.
pub async fn main_text<G: Gh>(gh: &G, s: &Settings) -> Result<String> {
    let mut mains = Vec::new();
    for repo in &s.repos {
        match gate::main_head(gh, s, repo).await {
            Ok(sha) => mains.push((repo.clone(), sha)),
            Err(e @ Error::RateLimited(_)) => return Err(e),
            Err(e) => eprintln!("{repo}: no main read: {e}"),
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

pub async fn refresh_main<G: Gh>(gh: &G, s: &Settings, bus: &Bus) -> Result<()> {
    let text = main_text(gh, s).await?;
    store::put_latest(bus, state::KEY_MAIN, &text).await?;
    Ok(())
}

/// The author's seats: the trees registry, the branch name, or the `Ready` comment's signer
/// (a seat some host advertises).
pub(crate) async fn author_seats<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    repo: &str,
    branch: &str,
    number: u64,
    issue: Option<u64>,
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
    Ok(state::author_seats(
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
    use crate::testutil::{fixture, settings};

    const HEAD: &str = "1f461fa77bba922fe81383df4c475feb58b7c233";

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
                .on("git/ref/heads/main", &main)
                .on("graphql", "{\"data\":{\"repository\":{\"pullRequest\":{\"reviewThreads\":{\"nodes\":[]}}}}}"),
        );
        let s = settings();
        let l = plan(&gh, &s, "hgl_spec", 7, HEAD, &LandOpts::default())
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
