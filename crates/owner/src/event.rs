//! Events of the owner channel (docs/owner.md §4) and how the console folds them into
//! thread records. Agents publish on `hgl.console.>`; the console and Slack have
//! separate owner subjects, so an event's subject says whose words it carries (OWN-5).
//! The fold refuses an owner event on an agent subject and the reverse.

use hgl_ai_proto::{CONSOLE_PREFIX, Kind, Note, OWNER_PREFIX, Seat};
use serde::{Deserialize, Serialize};

use hgl_ai_bus::MAX_NOTE_BYTES;

use crate::model::{
    Answer, CLIPPED, Choice, Message, Priority, QOption, QUOTE_MAX, Thread, ThreadKind,
    ThreadState, check_id, clipped, shorten_with,
};

pub const EVENT_VERSION: u32 = 1;

/// `hgl.console.status`
pub fn status_subject() -> String {
    format!("{CONSOLE_PREFIX}.status")
}
/// `hgl.console.thread.<id>`: an agent's event in a thread.
pub fn agent_subject(id: &str) -> String {
    format!("{CONSOLE_PREFIX}.thread.{id}")
}
/// `hgl.owner.thread.<id>`: the owner's event in a thread.
pub fn owner_subject(id: &str) -> String {
    format!("{OWNER_PREFIX}.thread.{id}")
}

/// An owner event from Slack, keyed by workspace, channel and reply or action ts.
pub fn slack_subject(id: &str, team: &str, channel: &str, source_key: &str) -> String {
    format!("{}.slack.{team}.{channel}.{source_key}", owner_subject(id))
}

/// Who published an event, by its subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Agent,
    Owner,
}

/// The origin and thread id a subject names; `None` for status reports and others.
pub fn parse_subject(subject: &str) -> Option<(Origin, &str)> {
    if let Some(id) = subject
        .strip_prefix(CONSOLE_PREFIX)
        .and_then(|r| r.strip_prefix(".thread."))
    {
        return Some((Origin::Agent, id));
    }
    let tail = subject
        .strip_prefix(OWNER_PREFIX)
        .and_then(|r| r.strip_prefix(".thread."))?;
    let id = match tail.split_once(".slack.") {
        Some((id, key)) if key.split('.').count() == 3 => id,
        Some(_) => return None,
        None => tail,
    };
    Some((Origin::Owner, id))
}

/// A status report (OWN-1) on `hgl.console.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub v: u32,
    pub sender: Seat,
    pub at: String,
    pub body: String,
}

/// A question as asked (OWN-2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    pub title: String,
    pub priority: Priority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    #[serde(default)]
    pub context: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<QOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested: Option<String>,
}

impl Ask {
    pub fn validate(&self) -> Result<(), String> {
        if self.title.trim().is_empty() || self.title.len() > 200 || self.title.contains('\n') {
            return Err("a title is one line of at most 200 bytes".into());
        }
        if self.question.trim().is_empty() {
            return Err("an empty question".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for o in &self.options {
            if !crate::model::valid_label(&o.label) || !seen.insert(o.label.as_str()) {
                return Err(format!("option label {:?} invalid or repeated", o.label));
            }
        }
        if let Some(s) = &self.suggested
            && !self.options.iter().any(|o| &o.label == s)
        {
            return Err(format!("suggested option {s:?} is not one of the options"));
        }
        Ok(())
    }
}

/// One event. Agent events: `ask`, `message`, `withdraw`. Owner events: `answer`,
/// `owner_message`, `start`, `close`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Body {
    Ask(Ask),
    Message {
        body: String,
    },
    Withdraw {
        #[serde(default)]
        reason: String,
    },
    Answer {
        choice: Choice,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        option: Option<String>,
        #[serde(default)]
        text: String,
        #[serde(default)]
        note: String,
    },
    OwnerMessage {
        body: String,
    },
    Start {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        issue: Option<String>,
        body: String,
    },
    Close,
}

impl Body {
    pub fn origin(&self) -> Origin {
        match self {
            Body::Ask(_) | Body::Message { .. } | Body::Withdraw { .. } => Origin::Agent,
            _ => Origin::Owner,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub v: u32,
    pub id: String,
    /// The agent seat for agent events; `owner` for the owner's.
    pub sender: String,
    /// For the owner's events, the signed-in user (OWN-6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// For the owner's events, that user's handle (OWN-10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    pub at: String,
    #[serde(flatten)]
    pub body: Body,
}

impl Event {
    pub fn subject(&self) -> String {
        match self.body.origin() {
            Origin::Agent => agent_subject(&self.id),
            Origin::Owner => owner_subject(&self.id),
        }
    }

    /// A message id for the stream's duplicate window: a resend of this event is dropped.
    pub fn msg_id(&self) -> String {
        use sha2::Digest as _;
        let j = serde_json::to_vec(self).unwrap_or_default();
        format!(
            "{}-{}-{}",
            self.id,
            self.at,
            &hex::encode(sha2::Sha256::digest(&j))[..12]
        )
    }
}

/// What folding an event asks of the console besides the record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Effects {
    /// Notes to send: the owner's words to a seat, on the owner's subject.
    pub notes: Vec<Note>,
    /// A line for `HGL_STATE/decisions.log` (an owner ruling, OWN-8).
    pub decision: Option<String>,
    /// A new question to notify the owner of (OWN-9).
    pub new_question: Option<Priority>,
    /// The encoded size of a note too large for the bus, which the fold shortened: the
    /// console refuses such an event (`owner_event`), so only one it never checked has it.
    pub oversize: Option<usize>,
}

impl Effects {
    /// Add `n`, shortened to the bus's limit if it is over it (noted in `oversize`): a
    /// note that cannot be sent would stop the feed.
    fn add_note(&mut self, mut n: Note) -> Result<(), Refused> {
        let size = encoded_len(&n);
        if size > MAX_NOTE_BYTES {
            self.oversize = Some(self.oversize.unwrap_or(0).max(size));
            for _ in 0..8 {
                let over = encoded_len(&n).saturating_sub(MAX_NOTE_BYTES);
                if over == 0 {
                    break;
                }
                // Cut in proportion to what the body costs encoded (escapes cost more),
                // and room for the marker.
                let raw = n.body.len();
                let cost = serde_json::to_string(&n.body).map_or(raw, |b| b.len());
                let cut = ((over + 2 * CLIPPED.len()) as u128 * raw as u128)
                    .div_ceil(cost.max(1) as u128) as usize;
                shorten_with(
                    &mut n.body,
                    raw.saturating_sub(cut) + CLIPPED.len(),
                    CLIPPED,
                );
            }
            if encoded_len(&n) > MAX_NOTE_BYTES {
                return Err(refuse(format!("a note of {size} bytes does not fit")));
            }
        }
        self.notes.push(n);
        Ok(())
    }
}

fn encoded_len(n: &Note) -> usize {
    serde_json::to_vec(n).map_or(usize::MAX, |v| v.len())
}

/// Why an event was not folded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Refused(pub String);

fn refuse(m: impl Into<String>) -> Refused {
    Refused(m.into())
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

/// Fold `ev`, published on `subject` and stored at `seq`, into `rec` (absent before
/// the thread's first event). `controller` is the seat conversations talk to. Returns the
/// new record and its effects, or why the event is refused. Pure: no I/O.
pub fn fold(
    rec: Option<&Thread>,
    subject: &str,
    seq: u64,
    ev: &Event,
    controller: &Seat,
) -> Result<(Thread, Effects), Refused> {
    let (origin, sid) = parse_subject(subject).ok_or_else(|| refuse("not a thread subject"))?;
    if sid != ev.id {
        return Err(refuse(format!("event for {} on {subject}", ev.id)));
    }
    check_id(&ev.id).map_err(refuse)?;
    if ev.v != EVENT_VERSION {
        return Err(refuse(format!("unsupported event version {}", ev.v)));
    }
    hgl_ai_proto::check_stamp(&ev.at).map_err(|e| refuse(e.to_string()))?;
    // OWN-5: owner events only from the owner's subject, agent events never from it.
    if ev.body.origin() != origin {
        return Err(refuse(format!(
            "a {:?} event on the wrong subject {subject}",
            ev.body.origin()
        )));
    }
    if origin == Origin::Owner
        && subject != owner_subject(sid)
        && !matches!(
            ev.body,
            Body::OwnerMessage { .. } | Body::Answer { .. } | Body::Close
        )
    {
        return Err(refuse("Slack actions require an existing thread"));
    }
    match origin {
        Origin::Owner if ev.sender != hgl_ai_proto::OWNER => {
            return Err(refuse("an owner event whose sender is not `owner`"));
        }
        Origin::Agent => {
            let s = Seat::new(ev.sender.clone()).map_err(|e| refuse(e.to_string()))?;
            if s.is_reserved() {
                return Err(refuse(format!("an agent event from {s}")));
            }
        }
        Origin::Owner => {}
    }
    if let Some(h) = &ev.handle {
        if origin != Origin::Owner {
            return Err(refuse("only the owner's events carry a handle"));
        }
        hgl_ai_proto::check_handle(h).map_err(|e| refuse(e.to_string()))?;
    }
    let user = || ev.user.clone().unwrap_or_else(|| "unknown".into());
    let handle = || ev.handle.clone();
    // Who signs the owner's words: the handle, else the signed-in identity.
    let who = ev.handle.clone().unwrap_or_else(user);
    let quote = |id: &str| {
        format!(
            "Quote it on the owning issue as `{who}, {}: \"...\"`, naming {id} (OWN-4, OWN-10).",
            crate::model::quote_date(&ev.at)
        )
    };
    let mut fx = Effects::default();

    let Some(old) = rec else {
        let t = match &ev.body {
            Body::Ask(a) => {
                if !ev.id.starts_with("q-") {
                    return Err(refuse("a question's id starts with q-"));
                }
                a.validate().map_err(refuse)?;
                fx.new_question = Some(a.priority);
                Thread {
                    id: ev.id.clone(),
                    kind: ThreadKind::Question,
                    title: a.title.trim().to_string(),
                    issue: a.issue.clone(),
                    priority: Some(a.priority),
                    state: ThreadState::Open,
                    seat: ev.sender.clone(),
                    created_at: ev.at.clone(),
                    updated_at: ev.at.clone(),
                    context: a.context.clone(),
                    question: a.question.clone(),
                    options: a.options.clone(),
                    suggested: a.suggested.clone(),
                    answer: None,
                    messages: Vec::new(),
                    seq,
                    older: 0,
                    cut: false,
                }
            }
            Body::Start { title, issue, body } => {
                if !ev.id.starts_with("c-") {
                    return Err(refuse("a conversation's id starts with c-"));
                }
                if title.trim().is_empty() || body.trim().is_empty() {
                    return Err(refuse("a conversation needs a description and a message"));
                }
                let t = Thread {
                    id: ev.id.clone(),
                    kind: ThreadKind::Conversation,
                    title: first_line(title).to_string(),
                    issue: issue.clone(),
                    priority: None,
                    state: ThreadState::Open,
                    seat: controller.to_string(),
                    created_at: ev.at.clone(),
                    updated_at: ev.at.clone(),
                    context: String::new(),
                    question: String::new(),
                    options: Vec::new(),
                    suggested: None,
                    answer: None,
                    messages: vec![Message {
                        sender: hgl_ai_proto::OWNER.into(),
                        user: Some(user()),
                        handle: handle(),
                        at: ev.at.clone(),
                        body: body.clone(),
                    }],
                    seq,
                    older: 0,
                    cut: false,
                };
                fx.add_note(owner_note(
                    &t,
                    controller,
                    Kind::Note,
                    ev,
                    format!(
                        "[owner] Conversation from {who}: {} {}\n\n{}\n\nReply with `hgl-ai thread reply {}`. {}",
                        t.id,
                        t.title,
                        body.trim_end(),
                        t.id,
                        quote(&t.id)
                    ),
                )?)?;
                t
            }
            _ => return Err(refuse(format!("no thread {}", ev.id))),
        };
        return Ok((t, fx));
    };

    if seq <= old.seq {
        return Err(refuse(format!("already folded (seq {seq} <= {})", old.seq)));
    }
    let mut t = old.clone();
    t.seq = seq;
    t.updated_at = ev.at.clone();
    match &ev.body {
        Body::Ask(_) | Body::Start { .. } => {
            return Err(refuse(format!("thread {} exists", ev.id)));
        }
        Body::Message { body } => {
            if body.trim().is_empty() {
                return Err(refuse("an empty message"));
            }
            t.messages.push(Message {
                sender: ev.sender.clone(),
                user: None,
                handle: None,
                at: ev.at.clone(),
                body: body.clone(),
            });
        }
        Body::Withdraw { reason } => {
            if t.kind != ThreadKind::Question || !t.is_open() {
                return Err(refuse("only an open question is withdrawn"));
            }
            t.state = ThreadState::Withdrawn;
            let why = if reason.trim().is_empty() {
                "withdrawn".to_string()
            } else {
                format!("withdrawn: {}", reason.trim())
            };
            t.messages.push(Message {
                sender: ev.sender.clone(),
                user: None,
                handle: None,
                at: ev.at.clone(),
                body: why,
            });
        }
        Body::Answer {
            choice,
            option,
            text,
            note,
        } => {
            if t.kind != ThreadKind::Question || !t.is_open() {
                return Err(refuse("only an open question is answered"));
            }
            let option = match choice {
                Choice::Accept => Some(
                    t.suggested
                        .clone()
                        .ok_or_else(|| refuse("accept: the question suggests no option"))?,
                ),
                Choice::Option => {
                    let o = option.clone().ok_or_else(|| refuse("option: which one?"))?;
                    if t.option(&o).is_none() {
                        return Err(refuse(format!("option {o:?} is not one of the options")));
                    }
                    Some(o)
                }
                Choice::Reject => None,
                Choice::Other => {
                    if text.trim().is_empty() {
                        return Err(refuse("other: the answer is empty"));
                    }
                    None
                }
            };
            let a = Answer {
                choice: *choice,
                option,
                text: if *choice == Choice::Other {
                    text.clone()
                } else {
                    String::new()
                },
                note: note.clone(),
                user: user(),
                handle: handle(),
                at: ev.at.clone(),
            };
            let said = t.answer_text(&a);
            t.state = ThreadState::Answered;
            t.answer = Some(a);
            t.messages.push(Message {
                sender: hgl_ai_proto::OWNER.into(),
                user: Some(user()),
                handle: handle(),
                at: ev.at.clone(),
                body: format!("Answer: {said}"),
            });
            let to = Seat::new(t.seat.clone()).map_err(|e| refuse(e.to_string()))?;
            fx.add_note(owner_note(
                &t,
                &to,
                Kind::Answer,
                ev,
                format!(
                    "[owner] Answer from {who}: {} {} ({}): {said}\n\nQuestion: {}\n\nPost the ruling once on the owning issue. {}",
                    t.id,
                    t.title,
                    t.issue.as_deref().unwrap_or("no issue"),
                    clipped(t.question.trim(), QUOTE_MAX),
                    quote(&t.id)
                ),
            )?)?;
            let line = format!(
                "{} {} {} {who} ({}): {said}",
                ev.at,
                t.id,
                t.issue.as_deref().unwrap_or("-"),
                user()
            )
            .replace(['\n', '\r'], " ");
            fx.decision = Some(clipped(&line, crate::DECISION_LINE_MAX).into_owned());
        }
        Body::OwnerMessage { body } => {
            if body.trim().is_empty() {
                return Err(refuse("an empty message"));
            }
            t.messages.push(Message {
                sender: hgl_ai_proto::OWNER.into(),
                user: Some(user()),
                handle: handle(),
                at: ev.at.clone(),
                body: body.clone(),
            });
            let to = match t.kind {
                ThreadKind::Conversation => controller.clone(),
                ThreadKind::Question => {
                    Seat::new(t.seat.clone()).map_err(|e| refuse(e.to_string()))?
                }
            };
            fx.add_note(owner_note(
                &t,
                &to,
                Kind::Note,
                ev,
                format!(
                    "[owner] Message from {who} in {} {}:\n\n{}\n\nReply with `hgl-ai thread reply {}`. {}",
                    t.id,
                    t.title,
                    body.trim_end(),
                    t.id,
                    quote(&t.id)
                ),
            )?)?;
        }
        Body::Close => {
            if t.kind != ThreadKind::Conversation || !t.is_open() {
                return Err(refuse("only an open conversation is closed"));
            }
            t.state = ThreadState::Closed;
            t.messages.push(Message {
                sender: hgl_ai_proto::OWNER.into(),
                user: Some(user()),
                handle: handle(),
                at: ev.at.clone(),
                body: "closed".into(),
            });
        }
    }
    Ok((t, fx))
}

/// An owner note: sender `owner`, so it travels on `hgl.notes.owner.<seat>`, and `by`
/// the writer's handle (OWN-10); created at the event's time, so a refold makes the same
/// note and the hub drops the resend.
fn owner_note(
    t: &Thread,
    to: &Seat,
    kind: Kind,
    ev: &Event,
    body: String,
) -> Result<Note, Refused> {
    let mut n = Note::new(Seat::owner(), to.clone(), kind, body);
    n.created_at = ev.at.clone();
    n.by = ev.handle.clone();
    if let Some((repo, issue)) = t.repo_issue() {
        n.repo = Some(repo);
        n.issue = issue;
    }
    n.validate().map_err(|e| refuse(e.to_string()))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctl() -> Seat {
        Seat::new("claude").unwrap()
    }

    fn ask_event(id: &str) -> Event {
        Event {
            v: EVENT_VERSION,
            id: id.into(),
            sender: "claude".into(),
            user: None,
            handle: None,
            at: "20261127T100000Z".into(),
            body: Body::Ask(Ask {
                title: "Pick the lowering rule".into(),
                priority: Priority::P1,
                issue: Some("hgl-compiler#980".into()),
                context: "Two rules apply.".into(),
                question: "Which rule lowers first?".into(),
                options: vec![
                    QOption::parse("a:The first").unwrap(),
                    QOption::parse("b:The second").unwrap(),
                ],
                suggested: Some("a".into()),
            }),
        }
    }

    fn owner(id: &str, body: Body) -> Event {
        Event {
            v: EVENT_VERSION,
            id: id.into(),
            sender: "owner".into(),
            user: Some("github:hhenson".into()),
            handle: Some("HPH".into()),
            at: "20261127T110000Z".into(),
            body,
        }
    }

    const ID: &str = "q-0123abcd";

    #[test]
    fn a_question_answered() {
        let ask = ask_event(ID);
        let (t, fx) = fold(None, &ask.subject(), 1, &ask, &ctl()).unwrap();
        assert_eq!(t.state, ThreadState::Open);
        assert_eq!(fx.new_question, Some(Priority::P1));
        assert!(fx.notes.is_empty());

        let ans = owner(
            ID,
            Body::Answer {
                choice: Choice::Accept,
                option: None,
                text: String::new(),
                note: "keep it simple".into(),
            },
        );
        let (t2, fx) = fold(Some(&t), &ans.subject(), 2, &ans, &ctl()).unwrap();
        assert_eq!(t2.state, ThreadState::Answered);
        assert_eq!(t2.answer.as_ref().unwrap().option.as_deref(), Some("a"));
        let n = &fx.notes[0];
        assert_eq!(
            (n.sender.as_str(), n.recipient.as_str(), n.kind),
            ("owner", "claude", Kind::Answer)
        );
        assert_eq!(n.subject(), "hgl.notes.owner.claude");
        assert_eq!((n.repo.as_deref(), n.issue), (Some("hgl-compiler"), 980));
        assert!(
            n.body.starts_with("[owner] Answer from HPH: q-0123abcd Pick the lowering rule (hgl-compiler#980): accept a: The first (note: keep it simple) [HPH, 27 Nov 11:00]"),
            "{}",
            n.body
        );
        assert!(n.body.contains("`HPH, 2026-11-27: \"...\"`"), "{}", n.body);
        assert_eq!(n.by.as_deref(), Some("HPH"));
        assert_eq!(t2.answer.as_ref().unwrap().handle.as_deref(), Some("HPH"));
        assert_eq!(t2.messages.last().unwrap().signature(), "HPH, 27 Nov 11:00");
        assert_eq!(n.created_at, "20261127T110000Z");
        assert!(
            fx.decision
                .unwrap()
                .contains("q-0123abcd hgl-compiler#980 HPH (github:hhenson): accept a")
        );

        // Refolding the same event is refused; a second answer too.
        assert!(fold(Some(&t2), &ans.subject(), 2, &ans, &ctl()).is_err());
        assert!(fold(Some(&t2), &ans.subject(), 3, &ans, &ctl()).is_err());
        // The same fold makes the same note (the hub drops the resend).
        let (_, again) = fold(Some(&t), &ans.subject(), 2, &ans, &ctl()).unwrap();
        assert_eq!(again.notes[0].msg_id(), fx.notes[0].msg_id());
    }

    #[test]
    fn owner_words_only_on_the_owner_subject() {
        let ask = ask_event(ID);
        let (t, _) = fold(None, &ask.subject(), 1, &ask, &ctl()).unwrap();
        let ans = owner(
            ID,
            Body::Answer {
                choice: Choice::Reject,
                option: None,
                text: String::new(),
                note: String::new(),
            },
        );
        // A seat publishing the owner's answer on the agent subject it may use.
        let e = fold(Some(&t), &agent_subject(ID), 2, &ans, &ctl()).unwrap_err();
        assert!(e.0.contains("wrong subject"), "{e}");
        // An agent event on the owner's subject (only the console could put it there).
        let msg = Event {
            body: Body::Message { body: "hi".into() },
            ..ask_event(ID)
        };
        assert!(fold(Some(&t), &owner_subject(ID), 2, &msg, &ctl()).is_err());
        // An agent event claiming the owner as sender.
        let mut forged = msg.clone();
        forged.sender = "owner".into();
        assert!(fold(Some(&t), &agent_subject(ID), 2, &forged, &ctl()).is_err());
        // Another thread's id on this subject.
        assert!(fold(Some(&t), &agent_subject("q-ffffffff"), 2, &msg, &ctl()).is_err());
    }

    #[test]
    fn answers_are_checked() {
        let ask = ask_event(ID);
        let (t, _) = fold(None, &ask.subject(), 1, &ask, &ctl()).unwrap();
        let bad = |choice, option: Option<&str>, text: &str| {
            let e = owner(
                ID,
                Body::Answer {
                    choice,
                    option: option.map(String::from),
                    text: text.into(),
                    note: String::new(),
                },
            );
            fold(Some(&t), &e.subject(), 2, &e, &ctl()).is_err()
        };
        assert!(bad(Choice::Option, Some("z"), ""));
        assert!(bad(Choice::Option, None, ""));
        assert!(bad(Choice::Other, None, "  "));
        assert!(!bad(Choice::Option, Some("b"), ""));
        assert!(!bad(Choice::Other, None, "Neither: fold them"));
    }

    #[test]
    fn conversations() {
        let start = owner(
            "c-00000001",
            Body::Start {
                title: "Release timing".into(),
                issue: None,
                body: "Can we tag this week?".into(),
            },
        );
        let (t, fx) = fold(None, &start.subject(), 5, &start, &ctl()).unwrap();
        assert_eq!(
            (t.kind, t.seat.as_str()),
            (ThreadKind::Conversation, "claude")
        );
        assert_eq!(fx.notes[0].recipient.as_str(), "claude");
        assert_eq!(fx.notes[0].kind, Kind::Note);
        assert_eq!(fx.notes[0].by.as_deref(), Some("HPH"), "start");
        assert!(
            fx.notes[0]
                .body
                .starts_with("[owner] Conversation from HPH: c-00000001 Release timing")
        );
        assert_eq!(t.messages[0].handle.as_deref(), Some("HPH"));
        let reply = Event {
            v: EVENT_VERSION,
            id: "c-00000001".into(),
            sender: "claude".into(),
            user: None,
            handle: None,
            at: "20261127T120000Z".into(),
            body: Body::Message {
                body: "Thursday, after #980 lands.".into(),
            },
        };
        let (t, fx) = fold(Some(&t), &reply.subject(), 6, &reply, &ctl()).unwrap();
        assert!(fx.notes.is_empty());
        assert_eq!(t.messages.len(), 2);
        let more = owner(
            "c-00000001",
            Body::OwnerMessage {
                body: "Good.".into(),
            },
        );
        let (t, fx) = fold(Some(&t), &more.subject(), 7, &more, &ctl()).unwrap();
        assert_eq!(fx.notes[0].recipient.as_str(), "claude");
        assert_eq!(fx.notes[0].by.as_deref(), Some("HPH"), "message");
        assert!(
            fx.notes[0]
                .body
                .starts_with("[owner] Message from HPH in c-00000001")
        );
        assert_eq!(t.messages.last().unwrap().handle.as_deref(), Some("HPH"));
        assert!(
            t.render().contains("HPH, 27 Nov 11:00 (owner):\nGood."),
            "{}",
            t.render()
        );
        let close = owner("c-00000001", Body::Close);
        let (t, _) = fold(Some(&t), &close.subject(), 8, &close, &ctl()).unwrap();
        assert_eq!(t.state, ThreadState::Closed);
        assert_eq!(
            t.messages.last().unwrap().handle.as_deref(),
            Some("HPH"),
            "close"
        );
        // Without a handle (a user in the older form), the identity signs.
        let mut plain = owner(
            "c-00000003",
            Body::Start {
                title: "t".into(),
                issue: None,
                body: "b".into(),
            },
        );
        plain.handle = None;
        let (_, fx) = fold(None, &plain.subject(), 9, &plain, &ctl()).unwrap();
        assert_eq!(fx.notes[0].by, None);
        assert!(
            fx.notes[0]
                .body
                .starts_with("[owner] Conversation from github:hhenson:")
        );
        // A handle is checked, and only the owner's events carry one.
        let mut bad = owner(
            "c-00000004",
            Body::Start {
                title: "t".into(),
                issue: None,
                body: "b".into(),
            },
        );
        bad.handle = Some("H@P".into());
        assert!(fold(None, &bad.subject(), 10, &bad, &ctl()).is_err());
        let mut agent = reply.clone();
        agent.handle = Some("HPH".into());
        assert!(fold(Some(&t), &agent.subject(), 11, &agent, &ctl()).is_err());
        // An agent cannot start a conversation in the owner's name.
        let mut forged = start.clone();
        forged.id = "c-00000002".into();
        assert!(fold(None, &agent_subject("c-00000002"), 9, &forged, &ctl()).is_err());
    }

    fn encoded(n: &Note) -> usize {
        serde_json::to_vec(n).unwrap().len()
    }

    /// The agent's text a note quotes is bounded, so choosing an option always fits; a
    /// note the owner's words make too large is shortened and reported, never unsendable.
    #[test]
    fn derived_notes_fit_the_bus() {
        let ask = ask_event(ID);
        let (mut t, _) = fold(None, &ask.subject(), 1, &ask, &ctl()).unwrap();
        t.question = format!("{}END-OF-QUESTION", "q".repeat(600 << 10));
        t.options[0].line = format!("{}END-OF-OPTION", "o".repeat(600 << 10));
        let accept = owner(
            ID,
            Body::Answer {
                choice: Choice::Accept,
                option: None,
                text: String::new(),
                note: String::new(),
            },
        );
        let (_, fx) = fold(Some(&t), &accept.subject(), 2, &accept, &ctl()).unwrap();
        let n = &fx.notes[0];
        assert!(fx.oversize.is_none());
        assert!(encoded(n) <= MAX_NOTE_BYTES && n.body.len() > 2 * QUOTE_MAX);
        assert!(!n.body.contains("END-OF-") && n.body.contains(CLIPPED));
        let line = fx.decision.unwrap();
        assert!(line.len() <= crate::DECISION_LINE_MAX && line.ends_with(CLIPPED));

        // Up to the limit the owner's words go whole; past it the note is cut to fit.
        let said = |body: String| {
            let m = owner(ID, Body::OwnerMessage { body });
            fold(Some(&t), &m.subject(), 2, &m, &ctl()).unwrap().1
        };
        let fx = said(format!("{}END", "w".repeat((1 << 20) - 1024)));
        assert!(fx.oversize.is_none() && fx.notes[0].body.contains("wEND"));
        for body in ["w".repeat(1 << 20), "\"\u{1}".repeat(400 << 10)] {
            let fx = said(body);
            let n = &fx.notes[0];
            assert!(
                fx.oversize.is_some_and(|s| s > MAX_NOTE_BYTES),
                "{:?}",
                fx.oversize
            );
            assert!(encoded(n) <= MAX_NOTE_BYTES && encoded(n) > MAX_NOTE_BYTES - 1024);
            assert!(n.body.ends_with(CLIPPED));
        }
    }

    /// Two 600 KiB replies and many more: the record stays within its bound; what it
    /// leaves out is counted (and stays in the stream).
    #[test]
    fn records_stay_bounded() {
        let ask = ask_event(ID);
        let (mut t, _) = fold(None, &ask.subject(), 1, &ask, &ctl()).unwrap();
        let big = "x".repeat(600 << 10);
        for i in 0..2u64 {
            let ev = Event {
                body: Body::Message { body: big.clone() },
                ..ask_event(ID)
            };
            t = fold(Some(&t), &ev.subject(), 2 + i, &ev, &ctl()).unwrap().0;
        }
        assert!(
            t.encoded_len() > crate::model::RECORD_MAX,
            "unbounded, it is too big"
        );
        t.bound();
        assert!(
            t.encoded_len() <= crate::model::RECORD_MAX,
            "{}",
            t.encoded_len()
        );
        assert!(t.cut && t.messages.len() == 2 && t.partial());
        for i in 0..60u64 {
            let ev = Event {
                body: Body::Message {
                    body: format!("m{i}"),
                },
                ..ask_event(ID)
            };
            t = fold(Some(&t), &ev.subject(), 10 + i, &ev, &ctl())
                .unwrap()
                .0;
            t.bound();
        }
        assert_eq!(t.messages.len(), crate::model::MESSAGES_KEPT);
        assert_eq!(t.older, 12);
        assert_eq!(t.messages.last().unwrap().body, "m59");
        assert!(t.encoded_len() <= crate::model::RECORD_MAX);
    }

    #[test]
    fn withdraw_and_json() {
        let ask = ask_event(ID);
        let (t, _) = fold(None, &ask.subject(), 1, &ask, &ctl()).unwrap();
        let w = Event {
            body: Body::Withdraw {
                reason: "settled on GitHub".into(),
            },
            ..ask_event(ID)
        };
        let (t, _) = fold(Some(&t), &w.subject(), 2, &w, &ctl()).unwrap();
        assert_eq!(t.state, ThreadState::Withdrawn);
        let j = serde_json::to_value(&ask).unwrap();
        assert_eq!(j["type"], "ask");
        assert_eq!(j["priority"], "p1");
        let back: Event = serde_json::from_value(j).unwrap();
        assert_eq!(back, ask);
        assert_eq!(parse_subject("hgl.console.status"), None);
    }
}
