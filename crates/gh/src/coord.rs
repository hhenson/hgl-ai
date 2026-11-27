//! The coordinator service, `gh coord run`: one batched GraphQL read per round (at most one
//! every five minutes) into `HGL_STATE/prs`; `hold` while the GraphQL allowance is spent
//! (and no round while a `hold` holds, whoever wrote it); and an immediate `note` to the
//! controller when a `ready` or `approved` pull request gains a bot finding or a failing
//! check between rounds. The previous table (from the bus at start, then from memory) is
//! what a round is compared with.
//!
//! An alert is kept until its send succeeds: in memory and on the hub
//! (`HGL_STATE/coord.alerts`, the notes not sent yet), and sent again each round, so a hub
//! write that fails once loses no alert, nor does a restart of the service (hgl-ai#8).

use std::io::Write;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use hgl_ai_bus::Bus;
use hgl_ai_proto::{Kind, Note, utc_now_stamp};
use serde_json::Value;

use crate::exec::{self, Gh};
use crate::table::{self, Alert, HoldChange, PrInfo, RateLimit, Row, Table};
use crate::{Error, Result, Settings, state, store};

/// The shortest interval between rounds (docs/protocol.md §8).
pub const MIN_INTERVAL: Duration = Duration::from_secs(300);
/// `HGL_STATE` key of the alerts not sent yet: a JSON array of their notes.
pub const KEY_UNSENT: &str = "coord.alerts";
/// Most alerts kept unsent; the oldest go first.
pub const UNSENT_KEEP: usize = 100;

#[derive(Debug, Clone)]
pub struct CoordOpts {
    pub once: bool,
    /// Print the table and the alerts; touch no state and send nothing.
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
    s: &Settings,
    t: &Table,
    rate: &RateLimit,
    hold: Option<&str>,
    now: DateTime<Utc>,
) -> Result<()> {
    store::put_latest(b, state::KEY_PRS, &t.render()).await?;
    match table::hold_after(rate, s.hold_below, hold, now) {
        HoldChange::Set(v) => {
            store::put_latest(b, state::KEY_HOLD, &v).await?;
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

/// Run rounds until stopped (or once). Dry-run output goes to `out`. Fails at once when
/// `gh` cannot run or is not signed in; a failed round or hub write is logged and the
/// next round comes as usual.
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
            "coordinator: {} repositories of {} every {:?} into {}/{}",
            s.repos.len(),
            s.owner,
            o.interval,
            hgl_ai_proto::KV_STATE,
            state::KEY_PRS
        );
    }
    let mut last_rate: Option<RateLimit> = None;
    loop {
        let now = Utc::now();
        let hold = match bus {
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
                        let res = publish(b, s, &t, &rate, hold.as_deref(), now)
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
            }
            Err(Error::RateLimited(m)) => {
                let v = table::hold_on_limit(graphql_reset(gh).await, last_rate.as_ref(), now);
                tracing::warn!("rate limited ({m}): hold until {v}");
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
                if o.once {
                    return Err(Error::RateLimited(m));
                }
            }
            Err(e @ Error::Auth(_)) => return Err(e),
            Err(e) if o.once => return Err(e),
            Err(e) => tracing::warn!("round failed: {e}"),
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
    use crate::testutil::{fixture, settings};

    #[tokio::test]
    async fn dry_run_once_prints_the_table() {
        let mut s = settings();
        s.repos = ["alpha", "beta", "gamma"].map(String::from).to_vec();
        let gh = crate::exec::ReadOnly(
            FakeGh::default()
                .on("auth status", "")
                .on("graphql", &fixture("coord-batch.json")),
        );
        let o = CoordOpts {
            once: true,
            dry_run: true,
            interval: MIN_INTERVAL,
        };
        let mut out = Vec::new();
        run(&gh, &s, None, &o, &mut out).await.unwrap();
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
    async fn rate_limited_round() {
        let s = settings();
        let gh = FakeGh::default()
            .on("auth status", "")
            .fail("graphql", "gh: API rate limit exceeded for user ID 1.")
            .on(
                "rate_limit",
                "{\"resources\":{\"graphql\":{\"reset\":4102444800}}}",
            );
        let o = CoordOpts {
            once: true,
            dry_run: true,
            interval: MIN_INTERVAL,
        };
        let mut out = Vec::new();
        assert!(matches!(
            run(&gh, &s, None, &o, &mut out).await,
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
            ("coord", "claude", Kind::Note)
        );
        assert_eq!((n.repo.as_deref(), n.issue), (Some("alpha"), 11));
        assert!(
            n.body
                .starts_with("[coord] Alert: alpha#11 at 11111111 (ready)")
        );
    }
}
