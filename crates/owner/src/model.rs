//! Threads (docs/owner.md §1, §4): `HGL_THREADS/<id>`, one record per question or
//! conversation, written only by the console as it folds the events of `HGL_OWNER`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

macro_rules! word_enum {
    ($(#[$m:meta])* $name:ident, $what:literal, { $($var:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name { $($var),+ }
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$var),+];
            pub fn as_str(self) -> &'static str { match self { $($name::$var => $s),+ } }
        }
        impl FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, String> {
                match s { $($s => Ok($name::$var),)+ _ => Err(format!("unknown {} {s:?}", $what)) }
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
        }
    };
}

word_enum!(
    /// A question from the controller or triage, or a conversation the owner started.
    ThreadKind, "thread kind", { Question => "question", Conversation => "conversation" });

word_enum!(
    /// `p1` blocks work now, `p2` blocks work soon, `p3` whenever (OWN-2).
    Priority, "priority", { P1 => "p1", P2 => "p2", P3 => "p3" });

word_enum!(
    /// A question is open until answered or withdrawn; a conversation until closed.
    ThreadState, "thread state", {
    Open => "open", Answered => "answered", Withdrawn => "withdrawn", Closed => "closed" });

word_enum!(
    /// How the owner answered (OWN-2).
    Choice, "answer", { Accept => "accept", Option => "option", Reject => "reject", Other => "other" });

/// One option of a question: a short label and its line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QOption {
    pub label: String,
    pub line: String,
}

impl QOption {
    /// `a:Use the first rule` (the `--option` form).
    pub fn parse(s: &str) -> Result<QOption, String> {
        let (label, line) = s
            .split_once(':')
            .ok_or_else(|| format!("option {s:?}: expected LABEL:LINE"))?;
        let (label, line) = (label.trim(), line.trim());
        if !valid_label(label) {
            return Err(format!(
                "option label {label:?}: use [A-Za-z0-9_-], at most 16"
            ));
        }
        if line.is_empty() {
            return Err(format!("option {label}: empty line"));
        }
        Ok(QOption {
            label: label.to_string(),
            line: line.to_string(),
        })
    }
}

pub fn valid_label(l: &str) -> bool {
    !l.is_empty()
        && l.len() <= 16
        && l.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The owner's answer to a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub choice: Choice,
    /// The option taken: the suggested one for `accept`, the picked one for `option`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option: Option<String>,
    /// The owner's own answer, for `other`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// The signed-in user who answered (`github:<login>`, `apple:<email>`).
    pub user: String,
    pub at: String,
}

/// One message of a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// A seat, or `owner`.
    pub sender: String,
    /// For the owner's messages, the signed-in user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub at: String,
    pub body: String,
}

/// `HGL_THREADS/<id>`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    pub kind: ThreadKind,
    pub title: String,
    /// The issue it concerns (`repo#number`) or another identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    pub state: ThreadState,
    /// The seat that asked; for a conversation, the seat the owner talks to.
    pub seat: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<QOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<Answer>,
    #[serde(default)]
    pub messages: Vec<Message>,
    /// The `HGL_OWNER` sequence of the last event folded in.
    pub seq: u64,
}

impl Thread {
    pub fn is_open(&self) -> bool {
        self.state == ThreadState::Open
    }

    pub fn option(&self, label: &str) -> Option<&QOption> {
        self.options.iter().find(|o| o.label == label)
    }

    /// `repo#number` split, when the issue is one.
    pub fn repo_issue(&self) -> Option<(String, u64)> {
        parse_issue(self.issue.as_deref()?)
    }

    /// Whether `q` matches the description, issue or id (any case).
    pub fn matches(&self, q: &str) -> bool {
        let q = q.to_lowercase();
        self.title.to_lowercase().contains(&q)
            || self.id.to_lowercase().contains(&q)
            || self
                .issue
                .as_deref()
                .is_some_and(|i| i.to_lowercase().contains(&q))
    }

    /// One line: `id state kind priority issue title`.
    pub fn line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            self.id,
            self.state,
            self.kind,
            self.priority.map_or("-", Priority::as_str),
            self.issue.as_deref().unwrap_or("-"),
            self.title
        )
    }

    /// The whole thread as text, for `thread show` and the controller's prompt.
    pub fn render(&self) -> String {
        let mut s = format!(
            "{} {} ({}{}), {} {}\n",
            self.kind,
            self.id,
            self.state,
            self.priority.map(|p| format!(", {p}")).unwrap_or_default(),
            self.issue.as_deref().unwrap_or("no issue"),
            self.title
        );
        s.push_str(&format!("seat {}, opened {}\n", self.seat, self.created_at));
        if !self.context.is_empty() {
            s.push_str(&format!("\nContext:\n{}\n", self.context.trim_end()));
        }
        if !self.question.is_empty() {
            s.push_str(&format!("\nQuestion: {}\n", self.question.trim_end()));
        }
        for o in &self.options {
            let mark = if self.suggested.as_deref() == Some(o.label.as_str()) {
                " (suggested)"
            } else {
                ""
            };
            s.push_str(&format!("  {}: {}{mark}\n", o.label, o.line));
        }
        if let Some(a) = &self.answer {
            s.push_str(&format!("\nAnswer: {}\n", self.answer_text(a)));
        }
        for m in &self.messages {
            let who = match &m.user {
                Some(u) => format!("{} ({u})", m.sender),
                None => m.sender.clone(),
            };
            s.push_str(&format!("\n[{}] {who}:\n{}\n", m.at, m.body.trim_end()));
        }
        s
    }

    /// `accept a: <line>`, `option b: <line>`, `reject`, `other: <text>`, then the note.
    pub fn answer_text(&self, a: &Answer) -> String {
        let mut s = match (a.choice, a.option.as_deref()) {
            (Choice::Accept | Choice::Option, Some(l)) => format!(
                "{} {l}: {}",
                a.choice,
                self.option(l).map(|o| o.line.as_str()).unwrap_or("")
            ),
            (Choice::Other, _) => format!("other: {}", a.text.trim()),
            (c, _) => c.to_string(),
        };
        if !a.note.trim().is_empty() {
            s.push_str(&format!(" (note: {})", a.note.trim()));
        }
        s.push_str(&format!(" [by {} at {}]", a.user, a.at));
        s
    }
}

/// `repo#number` → `(repo, number)`.
pub fn parse_issue(s: &str) -> Option<(String, u64)> {
    let (r, n) = s.rsplit_once('#')?;
    let ok = !r.is_empty()
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    let n: u64 = n.parse().ok()?;
    (ok && n > 0).then(|| (r.to_string(), n))
}

/// Thread ids: `q-` or `c-` and eight hex digits; also a subject token and a bucket key.
pub fn check_id(id: &str) -> Result<(), String> {
    let ok = id.len() == 10
        && (id.starts_with("q-") || id.starts_with("c-"))
        && id[2..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    if ok {
        Ok(())
    } else {
        Err(format!(
            "thread id {id:?}: expected q-XXXXXXXX or c-XXXXXXXX"
        ))
    }
}

/// A fresh id for a thread of `kind`.
pub fn new_id(kind: ThreadKind) -> String {
    let p = match kind {
        ThreadKind::Question => "q",
        ThreadKind::Conversation => "c",
    };
    format!("{p}-{}", hex::encode(random_bytes::<4>()))
}

pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    use std::io::Read as _;
    let mut b = [0u8; N];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        // No random device: the clock and process, hashed.
        use sha2::Digest as _;
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = sha2::Sha256::digest(format!("{t}-{}", std::process::id()));
        b.copy_from_slice(&d[..N]);
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_ids_issues() {
        let o = QOption::parse("a: Use the first rule").unwrap();
        assert_eq!(
            (o.label.as_str(), o.line.as_str()),
            ("a", "Use the first rule")
        );
        assert!(QOption::parse("nolabel").is_err());
        assert!(QOption::parse("a b:x").is_err());
        assert!(QOption::parse("a:").is_err());
        let id = new_id(ThreadKind::Question);
        check_id(&id).unwrap();
        assert!(id.starts_with("q-"));
        assert_ne!(id, new_id(ThreadKind::Question));
        assert!(check_id("q-1234").is_err() && check_id("x-12345678").is_err());
        assert!(check_id("q-1234567G").is_err());
        assert_eq!(
            parse_issue("hgl-compiler#980"),
            Some(("hgl-compiler".into(), 980))
        );
        assert_eq!(parse_issue("spec/rule-7"), None);
        assert_eq!(parse_issue("a#0"), None);
        assert!("p2".parse::<Priority>().unwrap() < Priority::P3);
    }
}
