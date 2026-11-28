//! Build an hourly Slack batch from the hub. The batch itself checkpoints its sources.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use chrono::{DateTime, Utc};
use hgl_ai_bus::{Bus, BusError};
use hgl_ai_owner::digest::{self, Decision, Digest, Question, Report};
use hgl_ai_owner::{self as owner, Body, EVENT_VERSION, Event, Priority, Status, Thread, event};
use hgl_ai_proto::{CONSOLE_PREFIX, OWNER_PREFIX, SLACK_OWNER_FILTER, check_handle, parse_stamp};
use serde::Deserialize;

use crate::App;
use crate::service::{P2_BATCH, next_tick, p2_sent};

const REPORTS: usize = 6;
const DECISIONS: usize = 6;
const QUESTIONS: usize = 8;

// Legacy digests can checkpoint the window. Their unsanitized content has no role here.
#[derive(Deserialize)]
struct Checkpoint {
    v: u32,
    through: i64,
}

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
        .map(|message| serde_json::from_slice::<Checkpoint>(&message.payload))
        .transpose()
        .map_err(|e| owner::Error::Invalid(format!("unreadable Slack digest: {e}")))?;
    if last
        .as_ref()
        .is_some_and(|batch| ![1, digest::VERSION].contains(&batch.v))
    {
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
    let mut migrated = if last.is_none() {
        legacy.iter().cloned().collect::<BTreeMap<_, _>>()
    } else {
        BTreeMap::new()
    };
    let mut questions = BTreeMap::new();
    let mut answers = Vec::new();
    // On first activation, check which pending questions actually belong to later
    // windows. Their Ask events remain in the stream and will be picked up then.
    let scan_before = if migrated.is_empty() {
        through
    } else {
        DateTime::<Utc>::MAX_UTC
    };
    let status_subject = event::status_subject();
    bus.owner_window(
        vec![
            status_subject.clone(),
            format!("{CONSOLE_PREFIX}.thread.*"),
            format!("{OWNER_PREFIX}.thread.*"),
            SLACK_OWNER_FILTER.into(),
        ],
        since,
        scan_before,
        |message| {
            let in_window = message.time < through;
            if message.subject == status_subject {
                if !in_window {
                    return;
                }
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
            if !event.id.starts_with("q-")
                || event::validate_event(&message.subject, &event).is_err()
            {
                return;
            }
            match event.body {
                Body::Ask(ask) if ask.priority == Priority::P2 && ask.validate().is_ok() => {
                    if in_window {
                        questions.entry(event.id).or_insert(ask.title);
                    } else {
                        migrated.remove(&event.id);
                    }
                }
                Body::Answer { .. } if in_window => {
                    answers.push((message.seq, event.id));
                }
                _ => {}
            }
        },
    )
    .await?;

    for (id, title) in migrated {
        questions.entry(id).or_insert(title);
    }
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

    let mut decisions = VecDeque::new();
    let mut decision_count = 0;
    for (seq, id) in answers {
        let Some(thread) = owner::thread_at(bus, &app.controller, &id, seq).await? else {
            continue;
        };
        // A refused answer leaves the reconstructed thread at the preceding event.
        if thread.seq != seq {
            continue;
        }
        let Some(decision) = decision_preview(&thread) else {
            continue;
        };
        if decisions.len() == DECISIONS {
            decisions.pop_front();
        }
        decisions.push_back(decision);
        decision_count += 1;
    }
    let batch = Digest {
        v: digest::VERSION,
        through: through.timestamp(),
        more_reports: report_count - reports.len(),
        more_decisions: decision_count - decisions.len(),
        more_questions: open_count - open.len(),
        reports: reports.into(),
        decisions: decisions.into(),
        questions: open,
    };
    let has_content = !batch.is_empty();
    if !has_content && last.is_some() {
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
    Ok(has_content)
}

fn decision_preview(thread: &Thread) -> Option<Decision> {
    let answer = thread.answer.as_ref()?;
    let author = answer
        .handle
        .as_deref()
        .filter(|handle| check_handle(handle).is_ok())
        .unwrap_or("owner");
    let issue = thread.issue.as_deref().unwrap_or("-");
    let line = format!(
        "{} {} {issue} {author}: {}",
        answer.at,
        thread.id,
        thread.answer_content(answer)
    );
    Some(Decision {
        thread: thread.id.clone(),
        preview: preview(&line.replace(['\n', '\r'], " "), 360),
    })
}

fn preview(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.into(),
    }
}
