//! The owner channel (docs/owner.md): questions, conversations and status reports
//! between the agents and the owner, and the controller's decision log.
//!
//! - [`model`]: the thread record of `HGL_THREADS`.
//! - [`event`]: the events of `HGL_OWNER` and the fold the console applies to them.
//! - The functions below are the operations the command line (agents) and the console
//!   (the owner) use. Agents publish on `hgl.console.>`; the console alone publishes the
//!   owner's words on `hgl.owner.>` and writes `HGL_THREADS` (OWN-5).

pub mod event;
pub mod model;

use std::time::Duration;

use hgl_ai_bus::{Bus, BusError, StreamMsg};
use hgl_ai_proto::{STREAM_OWNER, Seat, utc_now_stamp};

pub use event::{Ask, Body, EVENT_VERSION, Effects, Event, Origin, Refused, Status, fold};
pub use model::{
    Answer, Choice, Message, Priority, QOption, Thread, ThreadKind, ThreadState, check_id, new_id,
    parse_issue,
};

/// The controller's decision log in `HGL_STATE` (OWN-8).
pub const KEY_DECISIONS: &str = "decisions.log";
/// Lines of `decisions.log` kept; the owning issues hold the full text.
pub const DECISIONS_KEEP: usize = 2000;
/// The console's durable feed over `HGL_OWNER`.
pub const FEED: &str = "console";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Bus(#[from] BusError),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

fn sender_ok(s: &Seat) -> Result<()> {
    if s.is_reserved() {
        return Err(invalid(format!("{s} is not a seat that can post")));
    }
    Ok(())
}

async fn publish(bus: &Bus, ev: &Event) -> Result<u64> {
    let payload = serde_json::to_vec(ev).map_err(|e| invalid(e.to_string()))?;
    Ok(bus
        .publish_event(&ev.subject(), &payload, &ev.msg_id())
        .await?
        .seq)
}

fn agent_event(sender: &Seat, id: &str, body: Body) -> Event {
    Event {
        v: EVENT_VERSION,
        id: id.to_string(),
        sender: sender.to_string(),
        user: None,
        handle: None,
        at: utc_now_stamp(),
        body,
    }
}

/// Open a question (OWN-2); returns its id. The console indexes it as it folds the event.
pub async fn ask(bus: &Bus, sender: &Seat, a: Ask) -> Result<String> {
    sender_ok(sender)?;
    a.validate().map_err(Error::Invalid)?;
    let id = new_id(ThreadKind::Question);
    publish(bus, &agent_event(sender, &id, Body::Ask(a))).await?;
    Ok(id)
}

/// An agent's message in a thread.
pub async fn reply(bus: &Bus, sender: &Seat, id: &str, body: &str) -> Result<u64> {
    sender_ok(sender)?;
    check_id(id).map_err(Error::Invalid)?;
    if body.trim().is_empty() {
        return Err(invalid("an empty message"));
    }
    let ev = agent_event(
        sender,
        id,
        Body::Message {
            body: body.to_string(),
        },
    );
    publish(bus, &ev).await
}

/// Withdraw an open question.
pub async fn withdraw(bus: &Bus, sender: &Seat, id: &str, reason: &str) -> Result<u64> {
    sender_ok(sender)?;
    check_id(id).map_err(Error::Invalid)?;
    let ev = agent_event(
        sender,
        id,
        Body::Withdraw {
            reason: reason.to_string(),
        },
    );
    publish(bus, &ev).await
}

/// Post a status report (OWN-1).
pub async fn status_post(bus: &Bus, sender: &Seat, body: &str) -> Result<u64> {
    sender_ok(sender)?;
    if body.trim().is_empty() {
        return Err(invalid("an empty status report"));
    }
    let st = Status {
        v: EVENT_VERSION,
        sender: sender.clone(),
        at: utc_now_stamp(),
        body: body.to_string(),
    };
    let payload = serde_json::to_vec(&st).map_err(|e| invalid(e.to_string()))?;
    let id = format!("status-{}-{}", st.sender, st.at);
    Ok(bus
        .publish_event(&event::status_subject(), &payload, &id)
        .await?
        .seq)
}

/// Status reports stored since `since`, newest first, at most `max`.
pub async fn statuses(
    bus: &Bus,
    since: chrono::DateTime<chrono::Utc>,
    max: usize,
) -> Result<Vec<(u64, Status)>> {
    let msgs = bus
        .history(STREAM_OWNER, &event::status_subject(), since, max)
        .await?;
    let mut out: Vec<(u64, Status)> = msgs
        .iter()
        .filter_map(|m| match serde_json::from_slice::<Status>(&m.payload) {
            Ok(s) => Some((m.seq, s)),
            Err(e) => {
                tracing::warn!(seq = m.seq, "skipping an unreadable status report: {e}");
                None
            }
        })
        .collect();
    out.reverse();
    Ok(out)
}

/// Every thread, newest first.
pub async fn list(bus: &Bus) -> Result<Vec<Thread>> {
    let mut v: Vec<Thread> = bus.threads().await?;
    v.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    Ok(v)
}

/// Open questions first, by priority then age; then the rest, newest first.
pub fn by_urgency(mut v: Vec<Thread>) -> Vec<Thread> {
    v.sort_by(|a, b| {
        let key = |t: &Thread| (!t.is_open(), t.priority.unwrap_or(Priority::P3));
        key(a).cmp(&key(b)).then_with(|| {
            if a.is_open() {
                a.created_at.cmp(&b.created_at)
            } else {
                b.updated_at.cmp(&a.updated_at)
            }
        })
    });
    v
}

pub async fn show(bus: &Bus, id: &str) -> Result<Option<Thread>> {
    check_id(id).map_err(Error::Invalid)?;
    Ok(bus.thread_get::<Thread>(id).await?.map(|(t, _)| t))
}

/// The owner's event in a thread, stamped with the signed-in `user` and their `handle`
/// (OWN-10): the console's hub user alone may publish it (OWN-5, OWN-6). Returns the
/// stream sequence.
pub async fn owner_event(
    bus: &Bus,
    id: &str,
    user: &str,
    handle: Option<&str>,
    body: Body,
) -> Result<u64> {
    check_id(id).map_err(Error::Invalid)?;
    if body.origin() != Origin::Owner {
        return Err(invalid("not an owner event"));
    }
    let ev = Event {
        v: EVENT_VERSION,
        id: id.to_string(),
        sender: hgl_ai_proto::OWNER.to_string(),
        user: Some(user.to_string()),
        handle: handle.map(String::from),
        at: utc_now_stamp(),
        body,
    };
    publish(bus, &ev).await
}

/// Start a conversation (OWN-3); returns its id and the event's sequence.
pub async fn start_conversation(
    bus: &Bus,
    user: &str,
    handle: Option<&str>,
    title: &str,
    issue: Option<String>,
    body: &str,
) -> Result<(String, u64)> {
    if title.trim().is_empty() || title.len() > 200 || body.trim().is_empty() {
        return Err(invalid(
            "a conversation needs a one-line description and a message",
        ));
    }
    let id = new_id(ThreadKind::Conversation);
    let seq = owner_event(
        bus,
        &id,
        user,
        handle,
        Body::Start {
            title: title.trim().to_string(),
            issue,
            body: body.to_string(),
        },
    )
    .await?;
    Ok((id, seq))
}

/// What folding one feed message did.
#[derive(Debug)]
pub enum Applied {
    Folded {
        thread: Box<Thread>,
        new_question: Option<Priority>,
    },
    /// A status report: nothing to fold.
    Status,
    /// Refused or already folded; the reason.
    Skipped(String),
}

/// Fold one `HGL_OWNER` message into its thread record (the console's loop): the owner's
/// notes are sent first, then the record is written, then a ruling goes to the decision
/// log. A failure leaves the message for redelivery; a refold makes the same notes,
/// which the hub drops as duplicates.
pub async fn apply(bus: &Bus, controller: &Seat, msg: &StreamMsg) -> Result<Applied> {
    if msg.subject == event::status_subject() {
        return Ok(Applied::Status);
    }
    let Some((_, id)) = event::parse_subject(&msg.subject) else {
        return Ok(Applied::Skipped(format!(
            "not a thread subject: {}",
            msg.subject
        )));
    };
    let ev: Event = match serde_json::from_slice(&msg.payload) {
        Ok(e) => e,
        Err(e) => return Ok(Applied::Skipped(format!("not an event: {e}"))),
    };
    for _ in 0..8 {
        let rec = bus.thread_get::<Thread>(id).await?;
        let (t, fx) = match fold(
            rec.as_ref().map(|(t, _)| t),
            &msg.subject,
            msg.seq,
            &ev,
            controller,
        ) {
            Ok(x) => x,
            Err(r) => return Ok(Applied::Skipped(r.0)),
        };
        // Every effect before the record, each one idempotent: a fold that fails part
        // way is folded again, and only the record's write makes it done.
        for n in &fx.notes {
            bus.send(n).await?;
        }
        if let Some(line) = &fx.decision {
            decision_once(bus, line).await?;
        }
        match bus
            .thread_put(id, &t, rec.as_ref().map_or(0, |(_, r)| *r))
            .await
        {
            Ok(_) => {}
            Err(BusError::Conflict { .. }) => continue,
            Err(e) => return Err(e.into()),
        }
        return Ok(Applied::Folded {
            thread: Box::new(t),
            new_question: fx.new_question,
        });
    }
    Err(invalid(format!(
        "thread {id}: another writer keeps changing it"
    )))
}

/// Wait until the record of `id` has folded the event at `seq` (the console, after
/// publishing the owner's event), up to `wait`.
pub async fn wait_folded(bus: &Bus, id: &str, seq: u64, wait: Duration) -> Result<Option<Thread>> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let t = show(bus, id).await?;
        if t.as_ref().is_some_and(|t| t.seq >= seq) || tokio::time::Instant::now() >= deadline {
            return Ok(t);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Append one line to `decisions.log`, keeping its newest [`DECISIONS_KEEP`] lines in
/// the same compare-and-set (OWN-8; like `landed.log`), so the value never outgrows the
/// bucket. Newlines in `line` become spaces.
pub async fn decision_append(bus: &Bus, line: &str) -> Result<()> {
    append_decision(bus, line, false).await
}

/// [`decision_append`], unless the log already holds `line` (a fold done again).
pub async fn decision_once(bus: &Bus, line: &str) -> Result<()> {
    append_decision(bus, line, true).await
}

/// The log's bytes kept at most: well inside the bucket's value limit.
pub const DECISIONS_MAX_BYTES: usize = 768 << 10;

/// `text` with `line` appended, then its newest lines only: at most [`DECISIONS_KEEP`]
/// of them and [`DECISIONS_MAX_BYTES`] in all.
fn appended(text: &str, line: &str) -> String {
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.push(line);
    let mut skip = lines.len().saturating_sub(DECISIONS_KEEP);
    let mut size: usize = lines[skip..].iter().map(|l| l.len() + 1).sum();
    while size > DECISIONS_MAX_BYTES && skip + 1 < lines.len() {
        size -= lines[skip].len() + 1;
        skip += 1;
    }
    let mut value = lines[skip..].join("\n");
    value.push('\n');
    value
}

async fn append_decision(bus: &Bus, line: &str, once: bool) -> Result<()> {
    let line = line.replace(['\n', '\r'], " ");
    let line = line.trim();
    for _ in 0..64 {
        let cur = bus.state_get(KEY_DECISIONS).await?;
        let (text, rev) = cur.map_or((String::new(), 0), |e| (e.value, e.revision));
        if once && text.lines().any(|l| l == line) {
            return Ok(());
        }
        let value = appended(&text, line);
        match bus.state_update(KEY_DECISIONS, &value, rev).await {
            Err(BusError::Conflict { .. }) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            other => {
                other?;
                return Ok(());
            }
        }
    }
    Err(invalid(format!(
        "{KEY_DECISIONS}: other writers keep changing it"
    )))
}

/// The newest `n` lines of `decisions.log`, oldest first.
pub async fn decisions_tail(bus: &Bus, n: usize) -> Result<Vec<String>> {
    let Some(e) = bus.state_get(KEY_DECISIONS).await? else {
        return Ok(Vec::new());
    };
    let lines: Vec<String> = e
        .value
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect();
    let skip = lines.len().saturating_sub(n);
    Ok(lines.into_iter().skip(skip).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_stays_bounded() {
        let mut text = String::new();
        for i in 0..2100 {
            text = appended(&text, &format!("{i} short"));
        }
        assert_eq!(text.lines().count(), DECISIONS_KEEP);
        assert!(text.starts_with("100 short\n") && text.ends_with("2099 short\n"));
        let long = "x".repeat(4000);
        for _ in 0..400 {
            text = appended(&text, &long);
        }
        assert!(text.len() <= DECISIONS_MAX_BYTES, "{}", text.len());
        assert!(text.ends_with(&format!("{long}\n")));
    }

    #[test]
    fn urgency_order() {
        let t = |id: &str, p: Option<Priority>, st: ThreadState, at: &str| Thread {
            id: id.into(),
            kind: ThreadKind::Question,
            title: id.into(),
            issue: None,
            priority: p,
            state: st,
            seat: "claude".into(),
            created_at: at.into(),
            updated_at: at.into(),
            context: String::new(),
            question: "q".into(),
            options: vec![],
            suggested: None,
            answer: None,
            messages: vec![],
            seq: 1,
        };
        let v = by_urgency(vec![
            t("q-00000001", Some(Priority::P3), ThreadState::Open, "1"),
            t("q-00000002", Some(Priority::P1), ThreadState::Answered, "9"),
            t("q-00000003", Some(Priority::P1), ThreadState::Open, "5"),
            t("q-00000004", Some(Priority::P1), ThreadState::Open, "2"),
            t("q-00000005", None, ThreadState::Closed, "8"),
        ]);
        let ids: Vec<&str> = v.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "q-00000004",
                "q-00000003",
                "q-00000001",
                "q-00000002",
                "q-00000005"
            ]
        );
    }
}
