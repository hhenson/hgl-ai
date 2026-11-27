//! Events of the owner channel (docs/owner.md §4) and how the console folds them into
//! thread records. Agents publish on `hgl.console.>`; only the console's hub user may
//! publish on `hgl.owner.>`, so an event's subject says whose words it carries (OWN-5).
//! The fold refuses an owner event on an agent subject and the reverse.

use hgl_ai_proto::{CONSOLE_PREFIX, Kind, Note, OWNER_PREFIX, Seat};
use serde::{Deserialize, Serialize};

use crate::model::{
    Answer, Choice, Message, Priority, QOption, Thread, ThreadKind, ThreadState, check_id,
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
    subject
        .strip_prefix(OWNER_PREFIX)
        .and_then(|r| r.strip_prefix(".thread."))
        .map(|id| (Origin::Owner, id))
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
    // OWN-5: owner events only from the owner's subject, agent events never from it.
    if ev.body.origin() != origin {
        return Err(refuse(format!(
            "a {:?} event on {subject}: owner events come only from the console",
            ev.body.origin()
        )));
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
    let user = || ev.user.clone().unwrap_or_else(|| "unknown".into());
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
                        at: ev.at.clone(),
                        body: body.clone(),
                    }],
                    seq,
                };
                fx.notes.push(owner_note(
                    &t,
                    controller,
                    Kind::Note,
                    &ev.at,
                    format!(
                        "[owner] Conversation {}: {}\n\n{}\n\nFrom {} through the console. Reply with `hgl-ai thread reply {}`.",
                        t.id,
                        t.title,
                        body.trim_end(),
                        user(),
                        t.id
                    ),
                )?);
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
                at: ev.at.clone(),
            };
            let said = t.answer_text(&a);
            t.state = ThreadState::Answered;
            t.answer = Some(a);
            t.messages.push(Message {
                sender: hgl_ai_proto::OWNER.into(),
                user: Some(user()),
                at: ev.at.clone(),
                body: format!("Answer: {said}"),
            });
            let to = Seat::new(t.seat.clone()).map_err(|e| refuse(e.to_string()))?;
            fx.notes.push(owner_note(
                &t,
                &to,
                Kind::Answer,
                &ev.at,
                format!(
                    "[owner] Answer: {} {} ({}): {said}\n\nQuestion: {}\n\nPost a ruling once on the owning issue, quoting these words and naming {} (OWN-4).",
                    t.id,
                    t.title,
                    t.issue.as_deref().unwrap_or("no issue"),
                    t.question.trim(),
                    t.id
                ),
            )?);
            fx.decision = Some(format!(
                "{} {} {} owner ({}): {said}",
                ev.at,
                t.id,
                t.issue.as_deref().unwrap_or("-"),
                user()
            ));
        }
        Body::OwnerMessage { body } => {
            if body.trim().is_empty() {
                return Err(refuse("an empty message"));
            }
            t.messages.push(Message {
                sender: hgl_ai_proto::OWNER.into(),
                user: Some(user()),
                at: ev.at.clone(),
                body: body.clone(),
            });
            let to = match t.kind {
                ThreadKind::Conversation => controller.clone(),
                ThreadKind::Question => {
                    Seat::new(t.seat.clone()).map_err(|e| refuse(e.to_string()))?
                }
            };
            fx.notes.push(owner_note(
                &t,
                &to,
                Kind::Note,
                &ev.at,
                format!(
                    "[owner] Message in {} {}:\n\n{}\n\nFrom {} through the console. Reply with `hgl-ai thread reply {}`.",
                    t.id,
                    t.title,
                    body.trim_end(),
                    user(),
                    t.id
                ),
            )?);
        }
        Body::Close => {
            if t.kind != ThreadKind::Conversation || !t.is_open() {
                return Err(refuse("only an open conversation is closed"));
            }
            t.state = ThreadState::Closed;
        }
    }
    Ok((t, fx))
}

/// An owner note: sender `owner`, so it travels on `hgl.notes.owner.<seat>`; created at
/// the event's time, so a refold makes the same note and the hub drops the resend.
fn owner_note(t: &Thread, to: &Seat, kind: Kind, at: &str, body: String) -> Result<Note, Refused> {
    let mut n = Note::new(Seat::owner(), to.clone(), kind, body);
    n.created_at = at.to_string();
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
        assert!(n.body.starts_with("[owner] Answer: q-0123abcd Pick the lowering rule (hgl-compiler#980): accept a: The first (note: keep it simple) [by github:hhenson"), "{}", n.body);
        assert_eq!(n.created_at, "20261127T110000Z");
        assert!(
            fx.decision
                .unwrap()
                .contains("q-0123abcd hgl-compiler#980 owner (github:hhenson): accept a")
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
        assert!(e.0.contains("only from the console"), "{e}");
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
        let reply = Event {
            v: EVENT_VERSION,
            id: "c-00000001".into(),
            sender: "claude".into(),
            user: None,
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
        let close = owner("c-00000001", Body::Close);
        let (t, _) = fold(Some(&t), &close.subject(), 8, &close, &ctl()).unwrap();
        assert_eq!(t.state, ThreadState::Closed);
        // An agent cannot start a conversation in the owner's name.
        let mut forged = start.clone();
        forged.id = "c-00000002".into();
        assert!(fold(None, &agent_subject("c-00000002"), 9, &forged, &ctl()).is_err());
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
