//! The coordinator service, `gh coord run`: one batched GraphQL read per round (at most one
//! every five minutes) into `HGL_STATE/prs`; `hold` while the GraphQL allowance is spent
//! (and no round while a `hold` holds, whoever wrote it); and an immediate `note` to the
//! controller when a `ready` or `approved` pull request gains a bot finding or a failing
//! check between rounds. The previous table (from the bus at start, then from memory) is
//! what a round is compared with. After the pull request round, every
//! [`backlog::BACKLOG_EVERY`], the backlog round: `HGL_STATE/backlog` and the day's
//! `HGL_METRICS` samples. Intake for triage runs on the same cadence and under the same
//! `hold`, with its own cursor in `HGL_STATE/coord.intake`.
//!
//! An alert is kept until its send succeeds: in memory and on the hub
//! (`HGL_STATE/coord.alerts`, the notes not sent yet), and sent again each round, so a hub
//! write that fails once loses no alert, nor does a restart of the service (hgl-ai#8).

use std::io::Write;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use hgl_ai_bus::Bus;
use hgl_ai_proto::{Kind, Note, utc_now_stamp};
use serde_json::Value;

use crate::backlog::{self, BACKLOG_EVERY};
use crate::exec::{self, Gh};
use crate::routing;
use crate::table::{self, Alert, HoldChange, PrInfo, RateLimit, Row, Table};
use crate::{Error, Result, Settings, intake, state, store};

/// The shortest interval between rounds (docs/protocol.md §8).
pub const MIN_INTERVAL: Duration = Duration::from_secs(300);
/// `HGL_STATE` key of the alerts not sent yet: a JSON array of their notes.
pub const KEY_UNSENT: &str = "coord.alerts";
/// Most alerts kept unsent; the oldest go first.
pub const UNSENT_KEEP: usize = 100;

#[derive(Debug, Clone)]
pub struct CoordOpts {
    pub once: bool,
    /// Print the planned writes and notes without changing hub state.
    pub dry_run: bool,
    pub interval: Duration,
}

/// One round's read: the pull requests, the allowance, and repositories with no answer.
pub async fn round<G: Gh>(gh: &G, s: &Settings) -> Result<(Vec<PrInfo>, RateLimit, Vec<String>)> {
    let q = table::batch_query(s.repos.len());
    let mut vars = vec![("owner".to_string(), s.owner.clone())];
    for (i, r) in s.repos.iter().enumerate() {
        vars.push((format!("r{i}"), r.clone()));
    }
    let vars: Vec<(&str, String)> = vars.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    let pages = exec::graphql(gh, &q, &vars, false).await?;
    table::parse_batch(&pages[0], &s.repos, &s.review_bot, &utc_now_stamp())
}

/// The GraphQL reset time from the REST `rate_limit` endpoint (it costs nothing), when a
/// round was refused and gave none.
pub async fn graphql_reset<G: Gh>(gh: &G) -> Option<DateTime<Utc>> {
    let v: Value = exec::get(gh, "rate_limit").await.ok()?;
    let epoch = v.pointer("/resources/graphql/reset")?.as_i64()?;
    Utc.timestamp_opt(epoch, 0).single()
}

pub fn alert_note(s: &Settings, a: &Alert) -> Result<Note> {
    let mut n = Note::new(
        s.coord_seat.clone(),
        s.controller.clone(),
        Kind::Note,
        a.body(),
    );
    n.repo = Some(a.repo.clone());
    n.issue = a.number;
    n.head = Some(a.head.clone()).filter(|h| hgl_ai_proto::check_head(h).is_ok());
    n.validate()?;
    Ok(n)
}

/// Write what a round's allowance means for `hold`.
async fn write_hold(
    b: &Bus,
    change: &HoldChange,
    rate: &RateLimit,
    hold: Option<&str>,
) -> Result<()> {
    match change {
        HoldChange::Set(v) => {
            store::put_latest(b, state::KEY_HOLD, v).await?;
            tracing::warn!("GraphQL allowance {} left: hold until {v}", rate.remaining);
        }
        HoldChange::Clear => {
            if store::clear_if(b, state::KEY_HOLD, hold.unwrap_or_default()).await? {
                tracing::info!("hold cleared");
            }
        }
        HoldChange::Keep => {}
    }
    Ok(())
}

/// Add the notes of new alerts to the unsent ones (one per pull request, head and body),
/// keeping the newest [`UNSENT_KEEP`].
pub fn queue(unsent: &mut Vec<Note>, new: Vec<Note>) {
    for n in new {
        let same = |u: &Note| {
            u.repo == n.repo && u.issue == n.issue && u.head == n.head && u.body == n.body
        };
        if !unsent.iter().any(same) {
            unsent.push(n);
        }
    }
    let excess = unsent.len().saturating_sub(UNSENT_KEEP);
    unsent.drain(..excess);
}

/// The alerts left unsent by an earlier round or run.
async fn load_unsent(b: &Bus) -> Vec<Note> {
    match b.state_get(KEY_UNSENT).await {
        Ok(Some(e)) => serde_json::from_str(&e.value).unwrap_or_else(|err| {
            tracing::warn!("{KEY_UNSENT} is not readable ({err}); dropped");
            Vec::new()
        }),
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::warn!("reading {KEY_UNSENT}: {e}");
            Vec::new()
        }
    }
}

/// Send every unsent alert; the ones whose send failed stay, and the list is written to the
/// hub when it changed. The first failure is returned.
async fn send_unsent(b: &Bus, unsent: &mut Vec<Note>, stored: &mut bool) -> Result<()> {
    let mut failed = None;
    let before = unsent.len();
    let mut kept = Vec::new();
    for n in std::mem::take(unsent) {
        match b.send(&n).await {
            Ok(_) => tracing::info!("alert sent: {}", n.body.lines().next().unwrap_or("")),
            Err(e) => {
                tracing::warn!("sending an alert: {e}; kept for the next round");
                failed.get_or_insert(e);
                kept.push(n);
            }
        }
    }
    *unsent = kept;
    if unsent.is_empty() {
        if *stored {
            b.state_del(KEY_UNSENT).await?;
            *stored = false;
        }
    } else if unsent.len() != before || !*stored {
        let text = serde_json::to_string(&*unsent).expect("notes encode");
        store::put_latest(b, KEY_UNSENT, &text).await?;
        *stored = true;
    }
    match failed {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

/// Write a round to the hub: the table and `hold`. The alerts go separately
/// ([`send_unsent`]), so one failure does not take the other with it.
async fn publish(
    b: &Bus,
    t: &Table,
    rate: &RateLimit,
    hold: Option<&str>,
    change: &HoldChange,
) -> Result<()> {
    store::put_latest(b, state::KEY_PRS, &t.render()).await?;
    write_hold(b, change, rate, hold).await
}

/// A refused round: `hold` until the reset time (written, or printed in a dry run).
async fn on_rate_limit<G: Gh>(
    gh: &G,
    bus: Option<&Bus>,
    last_rate: Option<&RateLimit>,
    msg: &str,
    now: DateTime<Utc>,
    out: &mut impl Write,
) -> String {
    let v = table::hold_on_limit(graphql_reset(gh).await, last_rate, now);
    tracing::warn!("rate limited ({msg}): hold until {v}");
    match bus {
        Some(b) => {
            if let Err(e) = store::put_latest(b, state::KEY_HOLD, &v).await {
                tracing::warn!("writing {}: {e}", state::KEY_HOLD);
            }
        }
        None => {
            let _ = writeln!(out, "rate limited: would hold until {v}");
        }
    }
    v
}

/// The backlog round: read every repository's issues, then write `backlog` and the day's
/// samples (or print them in a dry run). `hold` and `last_rate` follow its allowance.
async fn backlog_round<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: Option<&Bus>,
    hold: &mut Option<String>,
    last_rate: &mut Option<RateLimit>,
    out: &mut impl Write,
) -> Result<()> {
    let now = Utc::now();
    let r = match backlog::read(gh, s, now).await {
        Ok(r) => r,
        Err(Error::RateLimited(m)) => {
            *hold = Some(on_rate_limit(gh, bus, last_rate.as_ref(), &m, now, out).await);
            return Err(Error::RateLimited(m));
        }
        Err(e) => return Err(e),
    };
    for m in &r.missing {
        tracing::warn!("{}: no backlog answer for this repository", s.full(m));
    }
    let change = table::hold_after(&r.rate, s.hold_below, hold.as_deref(), now);
    match bus {
        None => {
            let _ = out.write_all(r.dry_run().as_bytes());
            let _ = out.flush();
        }
        Some(b) => {
            backlog::publish(b, &r).await?;
            write_hold(b, &change, &r.rate, hold.as_deref()).await?;
            tracing::info!(
                "backlog: {} claimable, {} samples, cost {}, {} left",
                r.backlog.queue.len(),
                r.samples.len(),
                r.rate.cost,
                r.rate.remaining
            );
        }
    }
    match change {
        HoldChange::Set(v) => *hold = Some(v),
        HoldChange::Clear => *hold = None,
        HoldChange::Keep => {}
    }
    *last_rate = Some(r.rate);
    Ok(())
}

async fn intake_round<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: Option<&Bus>,
    hold: &mut Option<String>,
    last_rate: &mut Option<RateLimit>,
    out: &mut impl Write,
) -> Result<bool> {
    let now = Utc::now();
    let round = intake::Round::load(bus, now).await?;
    let Some(from) = round.from() else {
        if let Some(b) = bus {
            round.retry(b).await?;
        }
        return Ok(true);
    };
    let reading = match intake::read(gh, s, from).await {
        Ok(reading) => reading,
        Err(Error::RateLimited(m)) => {
            *hold = Some(on_rate_limit(gh, bus, last_rate.as_ref(), &m, now, out).await);
            return Err(Error::RateLimited(m));
        }
        Err(e) => return Err(e),
    };
    let change = table::hold_after(&reading.rate, s.hold_below, hold.as_deref(), now);
    if let Some(b) = bus {
        write_hold(b, &change, &reading.rate, hold.as_deref()).await?;
    }
    match change {
        HoldChange::Set(v) => *hold = Some(v),
        HoldChange::Clear => *hold = None,
        HoldChange::Keep => {}
    }
    *last_rate = Some(reading.rate);
    let Some(items) = reading.items else {
        tracing::info!("intake paused before all pages were read");
        return Ok(false);
    };
    round.finish(bus, s, items, out).await?;
    Ok(true)
}

/// Run rounds until stopped (or once). Dry-run output goes to `out`. Fails at
/// once when `gh` cannot run or is not signed in. A failed round or hub write is
/// logged and tried again at the next interval.
pub async fn run<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: Option<&Bus>,
    o: &CoordOpts,
    out: &mut impl Write,
) -> Result<()> {
    exec::check_auth(gh).await?;
    let bus = if o.dry_run { None } else { bus };
    let mut prev: Vec<Row> = Vec::new();
    let mut unsent: Vec<Note> = Vec::new();
    let mut stored = false;
    if let Some(b) = bus {
        match b.state_get(state::KEY_PRS).await {
            Ok(e) => prev = e.map(|e| Table::parse(&e.value).rows).unwrap_or_default(),
            Err(e) => tracing::warn!("reading the previous table: {e}"),
        }
        unsent = load_unsent(b).await;
        stored = !unsent.is_empty();
        eprintln!(
            "coordinator: {} repositories of {} every {:?} into {st}/{}, every {:?} into {st}/{} and {}",
            s.repos.len(),
            s.owner,
            o.interval,
            state::KEY_PRS,
            BACKLOG_EVERY,
            state::KEY_BACKLOG,
            hgl_ai_proto::KV_METRICS,
            st = hgl_ai_proto::KV_STATE,
        );
    }
    let mut last_rate: Option<RateLimit> = None;
    let mut last_backlog: Option<Instant> = None;
    let mut last_intake: Option<Instant> = None;
    loop {
        let now = Utc::now();
        let mut hold = match bus {
            Some(b) => match b.state_get(state::KEY_HOLD).await {
                Ok(e) => e.map(|e| e.value),
                Err(e) => {
                    tracing::warn!("reading {}: {e}", state::KEY_HOLD);
                    None
                }
            },
            None => None,
        };
        if let Some(h) = hold.as_deref().filter(|h| table::hold_active(h, now)) {
            tracing::info!("hold until {}: no round", h.trim());
            if o.once {
                return Ok(());
            }
            let until = table::parse_time(h.split_whitespace().next().unwrap_or(""))
                .and_then(|t| (t - now).to_std().ok())
                .unwrap_or(o.interval);
            tokio::time::sleep(until.min(o.interval).max(Duration::from_secs(30))).await;
            continue;
        }
        match round(gh, s).await {
            Ok((infos, rate, missing)) => {
                for m in &missing {
                    tracing::warn!("{}: no answer for this repository", s.full(m));
                }
                let t = Table {
                    read: infos
                        .first()
                        .map(|i| i.row.updated.clone())
                        .unwrap_or_else(utc_now_stamp),
                    rate: Some(rate.clone()),
                    rows: infos.iter().map(|i| i.row.clone()).collect(),
                };
                let alerts = table::alerts(&prev, &infos);
                let change = table::hold_after(&rate, s.hold_below, hold.as_deref(), now);
                match bus {
                    None => {
                        let _ = out.write_all(t.render().as_bytes());
                        for a in &alerts {
                            let _ = write!(out, "would alert {}: {}", s.controller, a.body());
                        }
                        let _ = out.flush();
                    }
                    Some(b) => {
                        let mut notes = Vec::with_capacity(alerts.len());
                        for a in &alerts {
                            notes.push(alert_note(s, a)?);
                        }
                        // Captured here: the transition is not looked for again, its note
                        // is kept until a send succeeds.
                        queue(&mut unsent, notes);
                        let sent = send_unsent(b, &mut unsent, &mut stored).await;
                        let res = publish(b, &t, &rate, hold.as_deref(), &change)
                            .await
                            .and(sent);
                        if let Err(e) = res {
                            if o.once {
                                return Err(e);
                            }
                            tracing::warn!("writing the round to the hub: {e}");
                        } else {
                            tracing::info!(
                                "{} open pull requests, cost {}, {} left",
                                t.rows.len(),
                                rate.cost,
                                rate.remaining
                            );
                        }
                    }
                }
                prev = t.rows;
                last_rate = Some(rate);
                match change {
                    HoldChange::Set(v) => hold = Some(v),
                    HoldChange::Clear => hold = None,
                    HoldChange::Keep => {}
                }
                if let Some(b) = bus
                    && !hold
                        .as_deref()
                        .is_some_and(|h| table::hold_active(h, Utc::now()))
                {
                    match routing::round(gh, s, b, &infos).await {
                        Err(Error::RateLimited(m)) => {
                            hold = Some(
                                on_rate_limit(gh, bus, last_rate.as_ref(), &m, Utc::now(), out)
                                    .await,
                            );
                            if o.once {
                                return Err(Error::RateLimited(m));
                            }
                        }
                        Err(e) if o.once => return Err(e),
                        Err(e) => tracing::warn!("routing review notes: {e}"),
                        Ok(()) => {}
                    }
                }
            }
            Err(Error::RateLimited(m)) => {
                hold = Some(on_rate_limit(gh, bus, last_rate.as_ref(), &m, now, out).await);
                if o.once {
                    return Err(Error::RateLimited(m));
                }
            }
            Err(e @ Error::Auth(_)) => return Err(e),
            Err(e) if o.once => return Err(e),
            Err(e) => tracing::warn!("round failed: {e}"),
        }
        if backlog::due(last_backlog, Instant::now(), BACKLOG_EVERY) {
            if let Some(h) = hold
                .as_deref()
                .filter(|h| table::hold_active(h, Utc::now()))
            {
                tracing::info!("hold until {}: no backlog round", h.trim());
            } else {
                let started = Instant::now();
                match backlog_round(gh, s, bus, &mut hold, &mut last_rate, out).await {
                    Ok(()) => last_backlog = Some(started),
                    Err(e @ Error::Auth(_)) => return Err(e),
                    Err(e) if o.once => return Err(e),
                    Err(e) => tracing::warn!("backlog round failed: {e}"),
                }
            }
        }
        let started = Instant::now();
        if backlog::due(last_intake, started, BACKLOG_EVERY)
            && !hold
                .as_deref()
                .is_some_and(|h| table::hold_active(h, Utc::now()))
        {
            match intake_round(gh, s, bus, &mut hold, &mut last_rate, out).await {
                Ok(true) => last_intake = Some(started),
                Ok(false) => {}
                Err(e @ Error::Auth(_)) => return Err(e),
                Err(e) if o.once => return Err(e),
                Err(e) => tracing::warn!("intake round failed: {e}"),
            }
        }
        if o.once {
            return Ok(());
        }
        tokio::time::sleep(o.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::fake::FakeGh;
    use crate::testutil::{empty_intake, fixture, settings};

    fn three() -> Settings {
        let mut s = settings();
        s.repos = ["alpha", "beta", "gamma"].map(String::from).to_vec();
        s
    }

    const ONCE_DRY: CoordOpts = CoordOpts {
        once: true,
        dry_run: true,
        interval: MIN_INTERVAL,
    };

    /// GitHub answering all scheduled queries.
    fn both(prs: &str) -> FakeGh {
        FakeGh::default()
            .on("auth status", "")
            .on("query Backlog", &fixture("backlog-batch.json"))
            .on("query Intake", &empty_intake(3).to_string())
            .on("graphql", prs)
    }

    #[tokio::test]
    async fn dry_run_once_prints_the_table() {
        let s = three();
        let gh = crate::exec::ReadOnly(both(&fixture("coord-batch.json")));
        let mut out = Vec::new();
        run(&gh, &s, None, &ONCE_DRY, &mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        let want = Table::parse(&fixture("coord-batch.prs"));
        let got = Table::parse(&text);
        assert_eq!(
            got.rows
                .iter()
                .map(|r| (&r.repo, r.number, r.bot, r.checks))
                .collect::<Vec<_>>(),
            want.rows
                .iter()
                .map(|r| (&r.repo, r.number, r.bot, r.checks))
                .collect::<Vec<_>>()
        );
        assert_eq!(got.rate, want.rate);
        let call = &gh.0.calls()[1];
        assert!(
            call.contains("-f owner=hhenson") && call.contains("-f r2=gamma"),
            "{call}"
        );
    }

    #[tokio::test]
    async fn dry_run_once_prints_the_backlog_and_samples() {
        let s = three();
        let gh = crate::exec::ReadOnly(both(&fixture("coord-batch.json")));
        let mut out = Vec::new();
        run(&gh, &s, None, &ONCE_DRY, &mut out).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        let calls = gh.0.calls();
        assert_eq!(calls.len(), 4, "{calls:?}");
        assert!(calls[2].contains("query=query Backlog("), "{}", calls[2]);
        assert!(calls[3].contains("query=query Intake("), "{}", calls[3]);
        assert!(!text.contains("[coord] Intake"), "{text}");
        let today = Utc::now().format("%Y-%m-%d").to_string();
        assert!(
            calls[2].contains(&format!(
                "-f opened1=repo:hhenson/beta is:issue created:{today}"
            )) && calls[2].contains(&format!(
                "-f closed2=repo:hhenson/gamma is:issue closed:{today}"
            )),
            "{}",
            calls[2]
        );
        let want = backlog::Backlog::parse(&fixture("backlog-batch.backlog"));
        let got = backlog::Backlog::parse(&text);
        assert_eq!(
            (&got.open, &got.labels, &got.queue),
            (&want.open, &want.labels, &want.queue)
        );
        let samples: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("would write HGL_METRICS/"))
            .collect();
        assert_eq!(samples.len(), 3, "{text}");
        let day = today.replace('-', "");
        assert_eq!(
            samples[1],
            format!("would write HGL_METRICS/beta.{day}: open 5 opened 0 closed 3")
        );
        // The table comes first, then the backlog.
        assert!(text.find("\tdraft\t").unwrap() < text.find("\tcodex-priority\t").unwrap());
    }

    #[tokio::test]
    async fn a_low_allowance_skips_the_backlog() {
        let s = three();
        let low = fixture("coord-batch.json")
            .replace("\"remaining\": 4945", "\"remaining\": 40")
            .replace("2026-01-05T12:55:49Z", "2100-01-01T00:00:00Z");
        let gh = both(&low);
        let mut out = Vec::new();
        run(&gh, &s, None, &ONCE_DRY, &mut out).await.unwrap();
        assert!(
            !gh.calls()
                .iter()
                .any(|c| c.contains("query Backlog") || c.contains("query Intake")),
            "the pull request round set hold: {:?}",
            gh.calls()
        );
        // The allowance is enough for both scheduled scans.
        let gh = both(&fixture("coord-batch.json"));
        run(&gh, &s, None, &ONCE_DRY, &mut Vec::new())
            .await
            .unwrap();
        assert!(gh.calls().iter().any(|c| c.contains("query Backlog")));
        assert!(gh.calls().iter().any(|c| c.contains("query Intake")));
    }

    #[tokio::test]
    async fn a_rate_limited_backlog_round() {
        let s = three();
        let gh = FakeGh::default()
            .on("auth status", "")
            .fail(
                "query Backlog",
                "gh: API rate limit exceeded for user ID 1.",
            )
            .on("graphql", &fixture("coord-batch.json"))
            .on(
                "rate_limit",
                "{\"resources\":{\"graphql\":{\"reset\":4102444800}}}",
            );
        let mut out = Vec::new();
        assert!(matches!(
            run(&gh, &s, None, &ONCE_DRY, &mut out).await,
            Err(Error::RateLimited(_))
        ));
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.ends_with("rate limited: would hold until 2100-01-01T00:00:00Z\n"),
            "{text}"
        );
        assert_eq!(Table::parse(&text).rows.len(), 6, "the table came first");
    }

    #[tokio::test]
    async fn a_failed_backlog_round_fails_once() {
        let s = three();
        let gh = FakeGh::default()
            .on("auth status", "")
            .on(
                "query Backlog",
                "{\"errors\":[{\"message\":\"Something went wrong\"}]}",
            )
            .on("graphql", &fixture("coord-batch.json"));
        let e = run(&gh, &s, None, &ONCE_DRY, &mut Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(&e, Error::Response { .. }), "{e}");
    }

    #[tokio::test]
    async fn rate_limited_round() {
        let s = settings();
        let gh = FakeGh::default()
            .on("auth status", "")
            .fail("graphql", "gh: API rate limit exceeded for user ID 1.")
            .on(
                "rate_limit",
                "{\"resources\":{\"graphql\":{\"reset\":4102444800}}}",
            );
        let mut out = Vec::new();
        assert!(matches!(
            run(&gh, &s, None, &ONCE_DRY, &mut out).await,
            Err(Error::RateLimited(_))
        ));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "rate limited: would hold until 2100-01-01T00:00:00Z\n"
        );
    }

    #[tokio::test]
    async fn signed_out_gh_fails_first() {
        let s = settings();
        let gh = FakeGh::default().fail("auth status", "You are not logged into any GitHub hosts.");
        let o = CoordOpts {
            once: false,
            dry_run: true,
            interval: MIN_INTERVAL,
        };
        let e = run(&gh, &s, None, &o, &mut Vec::new()).await.unwrap_err();
        assert!(
            matches!(&e, Error::Auth(m) if m.contains("gh auth login")),
            "{e}"
        );
        assert_eq!(gh.calls().len(), 1);
    }

    #[test]
    fn alert_notes_validate() {
        let s = settings();
        let a = Alert {
            repo: "alpha".into(),
            number: 11,
            head: "1".repeat(40),
            note: crate::bot::NoteState::Ready,
            finding: true,
            failing: false,
            url: "https://github.com/o/alpha/pull/11".into(),
        };
        let n = alert_note(&s, &a).unwrap();
        assert_eq!(
            (n.sender.as_str(), n.recipient.as_str(), n.kind),
            ("coord", "controller", Kind::Note)
        );
        assert_eq!((n.repo.as_deref(), n.issue), (Some("alpha"), 11));
        assert!(
            n.body
                .starts_with("[coord] Alert: alpha#11 at 11111111 (ready)")
        );
    }
}
