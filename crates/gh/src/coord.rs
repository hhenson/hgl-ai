//! The coordinator service, `gh coord run`: one batched GraphQL read per round (at most one
//! every five minutes) into `HGL_STATE/prs`; `hold` while the GraphQL allowance is spent
//! (and no round while a `hold` holds, whoever wrote it); and an immediate `note` to the
//! controller when a `ready` or `approved` pull request gains a bot finding or a failing
//! check between rounds. The previous table (from the bus at start, then from memory) is
//! what a round is compared with.

use std::io::Write;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use hgl_ai_bus::Bus;
use hgl_ai_proto::{Kind, Note, utc_now_stamp};
use serde_json::Value;

use crate::exec::{self, Gh};
use crate::table::{self, Alert, HoldChange, PrInfo, RateLimit, Row, Table};
use crate::{Error, Result, Settings, state};

/// The shortest interval between rounds (docs/protocol.md §8).
pub const MIN_INTERVAL: Duration = Duration::from_secs(300);

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

/// Run rounds until stopped (or once). Dry-run output goes to `out`.
pub async fn run<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: Option<&Bus>,
    o: &CoordOpts,
    out: &mut impl Write,
) -> Result<()> {
    let bus = if o.dry_run { None } else { bus };
    let mut prev: Vec<Row> = match bus {
        Some(b) => b
            .state_get(state::KEY_PRS)
            .await?
            .map(|e| Table::parse(&e.value).rows)
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let mut last_rate: Option<RateLimit> = None;
    loop {
        let now = Utc::now();
        let hold = match bus {
            Some(b) => b.state_get(state::KEY_HOLD).await?.map(|e| e.value),
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
                        b.state_put(state::KEY_PRS, &t.render()).await?;
                        for a in &alerts {
                            b.send(&alert_note(s, a)?).await?;
                            tracing::info!("alert sent: {}", a.body().lines().next().unwrap_or(""));
                        }
                        match table::hold_after(&rate, s.hold_below, hold.as_deref(), now) {
                            HoldChange::Set(v) => {
                                b.state_put(state::KEY_HOLD, &v).await?;
                                tracing::warn!(
                                    "GraphQL allowance {} left: hold until {v}",
                                    rate.remaining
                                );
                            }
                            HoldChange::Clear => {
                                b.state_del(state::KEY_HOLD).await?;
                                tracing::info!("hold cleared");
                            }
                            HoldChange::Keep => {}
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
                        b.state_put(state::KEY_HOLD, &v).await?;
                    }
                    None => {
                        let _ = writeln!(out, "rate limited: would hold until {v}");
                    }
                }
                if o.once {
                    return Err(Error::RateLimited(m));
                }
            }
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
        let gh =
            crate::exec::ReadOnly(FakeGh::default().on("graphql", &fixture("coord-batch.json")));
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
        let call = &gh.0.calls()[0];
        assert!(
            call.contains("-f owner=hhenson") && call.contains("-f r2=gamma"),
            "{call}"
        );
    }

    #[tokio::test]
    async fn rate_limited_round() {
        let s = settings();
        let gh = FakeGh::default()
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
