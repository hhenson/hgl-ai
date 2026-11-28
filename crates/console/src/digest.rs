//! Build an hourly Slack batch from the hub. The batch itself checkpoints its sources.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use chrono::{DateTime, Utc};
use hgl_ai_bus::{Bus, BusError};
use hgl_ai_owner::digest::{Digest, Question, Report};
use hgl_ai_owner::{self as owner, Body, EVENT_VERSION, Event, Priority, Status, Thread, event};
use hgl_ai_proto::{CONSOLE_PREFIX, parse_stamp};
use sha2::{Digest as _, Sha256};

use crate::App;
use crate::service::{P2_BATCH, next_tick, p2_sent};

const REPORTS: usize = 6;
const DECISIONS: usize = 6;
const QUESTIONS: usize = 8;

pub async fn run(app: &App, bus: &Bus, subject: &str) {
    loop {
        let now = Utc::now();
        let through = next_tick(now, P2_BATCH) - P2_BATCH;
        match round(app, bus, subject, through).await {
            Ok(_) => {
                let wait = (next_tick(now, P2_BATCH) - Utc::now())
                    .to_std()
                    .unwrap_or_default();
                tokio::time::sleep(wait).await;
            }
            Err(e) => {
                tracing::warn!("preparing Slack status digest: {e}");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

pub(crate) async fn round(
    app: &App,
    bus: &Bus,
    subject: &str,
    through: DateTime<Utc>,
) -> owner::Result<bool> {
    let prior = bus.owner_event_last(subject).await?;
    let last = prior
        .as_ref()
        .map(|message| serde_json::from_slice::<Digest>(&message.payload))
        .transpose()
        .map_err(|e| owner::Error::Invalid(format!("unreadable Slack digest: {e}")))?;
    if last.as_ref().is_some_and(|batch| batch.v != EVENT_VERSION) {
        return Err(owner::Error::Invalid(
            "unsupported Slack digest version".into(),
        ));
    }
    let legacy = app.pending_p2.lock().expect("pending").clone();
    if last.is_some() && !legacy.is_empty() {
        // The initial digest already took responsibility for the old notification queue.
        p2_sent(
            app,
            &legacy.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        );
    }
    if last
        .as_ref()
        .is_some_and(|batch| batch.through >= through.timestamp())
    {
        return Ok(false);
    }
    let since = last
        .as_ref()
        .map(|batch| {
            DateTime::from_timestamp(batch.through, 0)
                .ok_or_else(|| owner::Error::Invalid("invalid Slack digest checkpoint".into()))
        })
        .transpose()?
        .unwrap_or(through - P2_BATCH);

    let mut reports = VecDeque::new();
    let mut report_count = 0;
    let mut questions = if last.is_none() {
        legacy.iter().cloned().collect::<BTreeMap<_, _>>()
    } else {
        BTreeMap::new()
    };
    let status_subject = event::status_subject();
    bus.owner_window(
        vec![status_subject.clone(), format!("{CONSOLE_PREFIX}.thread.*")],
        since,
        through,
        |message| {
            if message.subject == status_subject {
                let Ok(status) = serde_json::from_slice::<Status>(&message.payload) else {
                    return;
                };
                if status.v == EVENT_VERSION
                    && status.sender == app.controller
                    && parse_stamp(&status.at).is_some()
                    && !status.body.trim().is_empty()
                {
                    if reports.len() == REPORTS {
                        reports.pop_front();
                    }
                    reports.push_back(Report {
                        seq: message.seq,
                        at: status.at,
                        body: preview(&status.body, 800),
                    });
                    report_count += 1;
                }
                return;
            }
            let Ok(event) = serde_json::from_slice::<Event>(&message.payload) else {
                return;
            };
            if event.id.starts_with("q-")
                && event::validate_event(&message.subject, &event).is_ok()
                && let Body::Ask(ask) = event.body
                && ask.priority == Priority::P2
                && ask.validate().is_ok()
            {
                questions.entry(event.id).or_insert(ask.title);
            }
        },
    )
    .await?;

    let mut open = Vec::new();
    let mut open_count = 0;
    for (id, title) in questions {
        if owner::show(bus, &id)
            .await?
            .as_ref()
            .is_none_or(Thread::is_open)
        {
            open_count += 1;
            if open.len() < QUESTIONS {
                open.push(Question { id, title });
            }
        }
    }

    let all_decisions = owner::decisions_tail(bus, owner::DECISIONS_KEEP).await?;
    let (decision_tip, decisions, more_decisions) =
        new_decisions(&all_decisions, last.as_ref(), since);
    let batch = Digest {
        v: EVENT_VERSION,
        through: through.timestamp(),
        decision_tip,
        more_reports: report_count - reports.len(),
        more_decisions,
        more_questions: open_count - open.len(),
        reports: reports.into(),
        decisions,
        questions: open,
    };
    if batch.is_empty() {
        return Ok(false);
    }
    let bytes = serde_json::to_vec(&batch).map_err(|e| owner::Error::Invalid(e.to_string()))?;
    match bus
        .publish_event_after(subject, &bytes, prior.map_or(0, |m| m.seq))
        .await
    {
        Ok(_) => {}
        Err(BusError::Conflict { .. }) => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    if !legacy.is_empty() && last.is_none() {
        p2_sent(
            app,
            &legacy.into_iter().map(|(id, _)| id).collect::<Vec<_>>(),
        );
    }
    Ok(true)
}

fn new_decisions(
    lines: &[String],
    last: Option<&Digest>,
    since: DateTime<Utc>,
) -> (Option<String>, Vec<String>, usize) {
    let tip = lines.last().map(|line| line_id(line));
    let start = last
        .and_then(|batch| batch.decision_tip.as_ref())
        .and_then(|tip| lines.iter().rposition(|line| &line_id(line) == tip))
        .map_or(0, |index| index + 1);
    let selected: Vec<_> = lines[start..]
        .iter()
        .filter(|line| {
            last.is_some()
                || line
                    .split_whitespace()
                    .next()
                    .and_then(parse_stamp)
                    .is_some_and(|at| at >= since)
        })
        .collect();
    let more = selected.len().saturating_sub(DECISIONS);
    (
        tip,
        selected[more..]
            .iter()
            .map(|line| preview(line, 360))
            .collect(),
        more,
    )
}

fn line_id(line: &str) -> String {
    hex::encode(Sha256::digest(line.as_bytes()))
}

fn preview(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.into(),
    }
}
