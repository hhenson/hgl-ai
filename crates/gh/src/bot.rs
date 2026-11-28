//! The review bot's state at an exact head, read from its comments and reviews.
//!
//! The Codex connector speaks in three ways, all kept from the shell gate:
//! - one summary comment it rewrites, holding `codex-review-state:v1 {json}` with a
//!   `commit_sha`, `has_any_findings` and `status` per review kind;
//! - a receipt comment `Didn't find any major issues` naming ``**Reviewed commit:** `<10 hex>` ``
//!   (its state record is sometimes left with no verdict for a review that found nothing);
//! - a pull request review on the commit when it has findings;
//! - and, when its quota is spent, a comment saying `reached your Codex usage limits`.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

const STATE_MARKER: &str = "codex-review-state:v1";
const CLEAN_PHRASE: &str = "find any major issues";
const LIMIT_PHRASE: &str = "reached your Codex usage limits";
/// What a comment says to ask the bot for a review.
pub const REQUEST: &str = "@codex review";

static REVIEWED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Reviewed commit:\*\*\s*`([0-9a-f]{7,40})`").unwrap());

/// One comment, from REST or GraphQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One pull request review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub author: String,
    pub commit: Option<String>,
    pub state: String,
    pub url: Option<String>,
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut v = v;
    for p in path {
        v = v.get(p)?;
    }
    v.as_str()
}

impl Comment {
    /// From a REST issue comment (`user.login`, `created_at`).
    pub fn from_rest(v: &Value) -> Comment {
        Comment {
            author: str_at(v, &["user", "login"]).unwrap_or("").to_string(),
            body: str_at(v, &["body"]).unwrap_or("").to_string(),
            created_at: str_at(v, &["created_at"]).unwrap_or("").to_string(),
            updated_at: str_at(v, &["updated_at"]).unwrap_or("").to_string(),
        }
    }
    /// From a GraphQL `IssueComment` (`author.login`, `createdAt`).
    pub fn from_graphql(v: &Value) -> Comment {
        Comment {
            author: str_at(v, &["author", "login"]).unwrap_or("").to_string(),
            body: str_at(v, &["body"]).unwrap_or("").to_string(),
            created_at: str_at(v, &["createdAt"]).unwrap_or("").to_string(),
            updated_at: str_at(v, &["updatedAt"]).unwrap_or("").to_string(),
        }
    }
}

impl Review {
    pub fn from_rest(v: &Value) -> Review {
        Review {
            author: str_at(v, &["user", "login"]).unwrap_or("").to_string(),
            commit: str_at(v, &["commit_id"]).map(String::from),
            state: str_at(v, &["state"]).unwrap_or("").to_string(),
            url: str_at(v, &["html_url"]).map(String::from),
        }
    }
    pub fn from_graphql(v: &Value) -> Review {
        Review {
            author: str_at(v, &["author", "login"]).unwrap_or("").to_string(),
            commit: str_at(v, &["commit", "oid"]).map(String::from),
            state: str_at(v, &["state"]).unwrap_or("").to_string(),
            url: str_at(v, &["url"]).map(String::from),
        }
    }
}

/// Whether `login` is the configured bot; REST adds `[bot]`, GraphQL does not.
pub fn is_bot(login: &str, bot: &str) -> bool {
    !bot.is_empty() && login.strip_suffix("[bot]").unwrap_or(login) == bot
}

/// The bot's state at one head (docs/protocol.md §8 `BOT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotState {
    Clean,
    Findings,
    Running,
    None,
    Limit,
}

impl BotState {
    pub fn as_str(self) -> &'static str {
        match self {
            BotState::Clean => "clean",
            BotState::Findings => "findings",
            BotState::Running => "running",
            BotState::None => "none",
            BotState::Limit => "limit",
        }
    }
    pub fn parse(s: &str) -> Option<BotState> {
        Some(match s {
            "clean" => BotState::Clean,
            "findings" => BotState::Findings,
            "running" => BotState::Running,
            "none" => BotState::None,
            "limit" => BotState::Limit,
            _ => return None,
        })
    }
}

/// One review kind in the bot's state record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRecord {
    pub kind: String,
    pub commit_sha: Option<String>,
    pub has_any_findings: Option<bool>,
    pub status: Option<String>,
}

/// The records of every `codex-review-state:v1 {...}` in a body.
pub fn state_records(body: &str) -> Vec<StateRecord> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find(STATE_MARKER) {
        rest = &rest[i + STATE_MARKER.len()..];
        let Some(Ok(v)) = serde_json::Deserializer::from_str(rest.trim_start())
            .into_iter::<Value>()
            .next()
        else {
            continue;
        };
        if let Some(obj) = v.as_object() {
            for (kind, r) in obj {
                out.push(StateRecord {
                    kind: kind.clone(),
                    commit_sha: r
                        .get("commit_sha")
                        .and_then(Value::as_str)
                        .map(String::from),
                    has_any_findings: r.get("has_any_findings").and_then(Value::as_bool),
                    status: r.get("status").and_then(Value::as_str).map(String::from),
                });
            }
        }
    }
    out
}

/// The bot's comments and reviews on one pull request.
#[derive(Debug, Clone, Default)]
pub struct BotView {
    /// The bot's comments, in any order.
    pub comments: Vec<Comment>,
    /// The bot's reviews.
    pub reviews: Vec<Review>,
}

impl BotView {
    /// Keep only what `bot` wrote.
    pub fn of(bot: &str, comments: &[Comment], reviews: &[Review]) -> BotView {
        BotView {
            comments: comments
                .iter()
                .filter(|c| is_bot(&c.author, bot))
                .cloned()
                .collect(),
            reviews: reviews
                .iter()
                .filter(|r| is_bot(&r.author, bot))
                .cloned()
                .collect(),
        }
    }

    fn records(&self) -> impl Iterator<Item = StateRecord> + '_ {
        self.comments.iter().flat_map(|c| state_records(&c.body))
    }

    /// Reviewed clean at `sha`: a state record with no findings, or a receipt saying so.
    pub fn clean_on(&self, sha: &str) -> bool {
        if sha.is_empty() {
            return false;
        }
        self.records()
            .any(|r| r.commit_sha.as_deref() == Some(sha) && r.has_any_findings == Some(false))
            || self.comments.iter().any(|c| {
                c.body.contains(CLEAN_PHRASE)
                    && REVIEWED
                        .captures_iter(&c.body)
                        .any(|m| sha.starts_with(&m[1]))
            })
    }

    /// Reviewed at `sha` with findings: a state record that says so, or a review on that commit.
    pub fn findings_on(&self, sha: &str) -> bool {
        self.records()
            .any(|r| r.commit_sha.as_deref() == Some(sha) && r.has_any_findings == Some(true))
            || self.reviewed_at(sha) > 0
    }

    /// How many reviews the bot left on `sha` (a review is how it posts findings).
    pub fn reviewed_at(&self, sha: &str) -> usize {
        self.reviews
            .iter()
            .filter(|r| r.commit.as_deref() == Some(sha))
            .count()
    }

    pub fn running_on(&self, sha: &str) -> bool {
        self.records()
            .any(|r| r.commit_sha.as_deref() == Some(sha) && r.status.as_deref() == Some("running"))
    }

    /// The bot's last comment (by creation) is its usage-limit message.
    pub fn limited(&self) -> bool {
        self.comments
            .iter()
            .max_by(|a, b| a.created_at.cmp(&b.created_at))
            .is_some_and(|c| c.body.contains(LIMIT_PHRASE))
    }

    /// `clean`, `findings`, `limit`, `running` or `none` at `sha`, in that order of precedence
    /// (as the shell gate: a clean review decides; the limit is the bot's last word otherwise).
    pub fn state_at(&self, sha: &str) -> BotState {
        if self.clean_on(sha) {
            BotState::Clean
        } else if self.findings_on(sha) {
            BotState::Findings
        } else if self.limited() {
            BotState::Limit
        } else if self.running_on(sha) {
            BotState::Running
        } else {
            BotState::None
        }
    }

    /// Link of the bot's latest review on `sha`, for alerts.
    pub fn review_url(&self, sha: &str) -> Option<&str> {
        self.reviews
            .iter()
            .rev()
            .find(|r| r.commit.as_deref() == Some(sha))
            .and_then(|r| r.url.as_deref())
    }
}

/// The state a pull request's latest note comment gives it (PROTO-1: `ready`, `blocked`,
/// `sendback`, `approved` and `landed` notes are mirrored as one comment each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteState {
    Ready,
    Approved,
    Sendback,
    Blocked,
    Landed,
}

static NOTE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\[[a-z0-9_.-]+\](?:\s*\([^)]*\))?\s*(ready|approved|sent back|send-back|sendback|blocked|landed)\b")
        .unwrap()
});

/// The latest note state among comments whose first line is a note's first line
/// (`[codex] Ready: ...`, `[claude] (w3) Approved ...`).
pub fn note_state(comments: &[Comment]) -> Option<NoteState> {
    let mut found: Vec<(&str, NoteState)> = comments
        .iter()
        .filter_map(|c| {
            let first = c.body.lines().next()?.trim();
            let m = NOTE_LINE.captures(first)?;
            let st = match m[1].to_ascii_lowercase().as_str() {
                "ready" => NoteState::Ready,
                "approved" => NoteState::Approved,
                "blocked" => NoteState::Blocked,
                "landed" => NoteState::Landed,
                _ => NoteState::Sendback,
            };
            Some((c.created_at.as_str(), st))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(b.0));
    found.last().map(|f| f.1)
}

/// When the pull request became Ready at `head` (WORK-6 Ready order): the `created_at` of
/// its latest `Ready` comment, when that comment names `head` (in full or by its first eight
/// characters) and the latest note state is still `ready` or `approved`.
pub fn ready_at<'a>(comments: &'a [Comment], head: &str) -> Option<&'a str> {
    if !matches!(
        note_state(comments),
        Some(NoteState::Ready | NoteState::Approved)
    ) {
        return None;
    }
    let ready = comments
        .iter()
        .filter(|c| {
            let first = c.body.lines().next().unwrap_or("").trim();
            NOTE_LINE
                .captures(first)
                .is_some_and(|m| m[1].eq_ignore_ascii_case("ready"))
        })
        .max_by(|a, b| a.created_at.cmp(&b.created_at))?;
    let short = head.get(..8)?;
    ready
        .body
        .contains(short)
        .then_some(ready.created_at.as_str())
}

static SIGNED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\[[a-z0-9_.-]+\]\s*\((?:worker\s+([0-9]+)|([a-z0-9][a-z0-9_-]{0,31}))\)")
        .unwrap()
});

/// Seats that signed a `Ready` comment as `[runtime] (seat)` or `[runtime] (worker N)`.
pub fn ready_signers(comments: &[Comment]) -> Vec<String> {
    let mut out = Vec::new();
    for c in comments {
        let first = c.body.lines().next().unwrap_or("").trim();
        if !NOTE_LINE
            .captures(first)
            .is_some_and(|m| m[1].eq_ignore_ascii_case("ready"))
        {
            continue;
        }
        if let Some(m) = SIGNED.captures(first) {
            let seat = match (m.get(1), m.get(2)) {
                (Some(n), _) => format!("w{}", n.as_str()),
                (None, Some(s)) => s.as_str().to_ascii_lowercase(),
                _ => continue,
            };
            if !out.contains(&seat) {
                out.push(seat);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::json;

    const BOT: &str = "chatgpt-codex-connector";
    const H1: &str = "1f461fa77bba922fe81383df4c475feb58b7c233";

    fn rest_view(comments: &str, reviews: &str) -> BotView {
        let c: Vec<Comment> = json(comments)
            .as_array()
            .unwrap()
            .iter()
            .map(Comment::from_rest)
            .collect();
        let r: Vec<Review> = json(reviews)
            .as_array()
            .unwrap()
            .iter()
            .map(Review::from_rest)
            .collect();
        BotView::of(BOT, &c, &r)
    }

    #[test]
    fn records_parse() {
        let body = "<!-- codex-review-state:v1 {\"code_review\":{\"commit_sha\":\"abc\",\"has_any_findings\":false,\"has_unresolved_findings\":false,\"status\":\"completed\"},\"security_review\":{\"commit_sha\":null,\"has_any_findings\":null,\"status\":null}} -->";
        let r = state_records(body);
        assert_eq!(r.len(), 2);
        let code = r.iter().find(|x| x.kind == "code_review").unwrap();
        assert_eq!(
            (code.commit_sha.as_deref(), code.has_any_findings),
            (Some("abc"), Some(false))
        );
        assert!(state_records("codex-review-state:v1 {broken").is_empty());
    }

    #[test]
    fn clean_by_record_and_by_receipt() {
        let v = rest_view("comments-clean.json", "reviews-none.json");
        assert!(v.clean_on(H1));
        assert_eq!(v.state_at(H1), BotState::Clean);
        assert_eq!(
            v.state_at("0000000000000000000000000000000000000000"),
            BotState::None
        );
        // A receipt alone (the record names another commit) is clean too.
        let receipt_only: Vec<Comment> = v
            .comments
            .iter()
            .filter(|c| !c.body.contains(STATE_MARKER))
            .cloned()
            .collect();
        let w = BotView {
            comments: receipt_only,
            reviews: vec![],
        };
        assert!(w.clean_on(H1));
        assert!(
            !w.clean_on("1f461fa77c000000000000000000000000000000"),
            "the ten characters must match"
        );
    }

    #[test]
    fn findings_by_review() {
        let v = rest_view("comments-findings.json", "reviews-findings.json");
        let head = "c94f7b07c7aa000000000000000000000000beef";
        assert_eq!(v.state_at(head), BotState::Findings);
        assert_eq!(v.reviewed_at(head), 1);
        assert_eq!(
            v.review_url(head),
            Some("https://github.com/o/r/pull/7#pullrequestreview-2")
        );
    }

    #[test]
    fn limit_is_the_last_word() {
        let v = rest_view("comments-limit.json", "reviews-none.json");
        let head = "d5ebabf18f232219e1061d1ec8fb3aead4a03e15";
        assert!(v.limited());
        assert_eq!(v.state_at(head), BotState::Limit);
        // A later receipt clean at the head decides.
        let mut w = v.clone();
        w.comments.push(Comment {
            author: BOT.into(),
            body:
                "Codex Review: Didn't find any major issues.\n\n**Reviewed commit:** `d5ebabf18f`"
                    .into(),
            created_at: "2026-01-02T16:47:12Z".into(),
            updated_at: "2026-01-02T16:47:12Z".into(),
        });
        assert!(!w.limited());
        assert_eq!(w.state_at(head), BotState::Clean);
    }

    #[test]
    fn running() {
        let v = rest_view("comments-running.json", "reviews-none.json");
        assert_eq!(
            v.state_at("9a8b7c6d5e4f30211203f4e5d6c7b8a9a0b1c2d3"),
            BotState::Running
        );
    }

    #[test]
    fn bot_login_forms() {
        assert!(is_bot("chatgpt-codex-connector[bot]", BOT));
        assert!(is_bot("chatgpt-codex-connector", BOT));
        assert!(!is_bot("codex-fan", BOT));
        assert!(!is_bot("x", ""));
    }

    #[test]
    fn note_states_and_signers() {
        let c = |body: &str, at: &str| Comment {
            author: "u".into(),
            body: body.into(),
            created_at: at.into(),
            updated_at: at.into(),
        };
        let cs = vec![
            c(
                "[codex] (worker 5) Ready: o/r#7 at 1f461fa7\n\nchecks pass",
                "2026-01-01T01:00:00Z",
            ),
            c("[codex] (worker 5) @codex review", "2026-01-01T02:00:00Z"),
            c("[claude] Approved at 1f461fa7.", "2026-01-01T03:00:00Z"),
        ];
        assert_eq!(note_state(&cs), Some(NoteState::Approved));
        assert_eq!(note_state(&cs[..2]), Some(NoteState::Ready));
        assert_eq!(note_state(&cs[1..2]), None);
        assert_eq!(
            note_state(&[c("[claude] Sent back: two findings", "x")]),
            Some(NoteState::Sendback)
        );
        assert_eq!(ready_signers(&cs), ["w5"]);
        assert_eq!(ready_signers(&[c("[codex] (w7) Ready: x", "x")]), ["w7"]);
        assert!(ready_signers(&[c("[codex] Ready: x", "x")]).is_empty());
    }

    #[test]
    fn ready_time_at_the_head() {
        let c = |body: &str, at: &str| Comment {
            author: "u".into(),
            body: body.into(),
            created_at: at.into(),
            updated_at: at.into(),
        };
        let first = c("[codex] Ready: o/r#7 at 1f461fa7", "2026-01-01T01:00:00Z");
        let back = c("[claude] Sent back: one finding", "2026-01-01T02:00:00Z");
        let again = c(
            &format!("[codex] (w5) Ready: o/r#7 at {H1}"),
            "2026-01-01T03:00:00Z",
        );
        let approved = c("[claude] Approved at 1f461fa7.", "2026-01-01T04:00:00Z");
        assert_eq!(
            ready_at(std::slice::from_ref(&first), H1),
            Some("2026-01-01T01:00:00Z")
        );
        // The latest Ready counts, and an approval keeps it.
        let all = [first.clone(), back, again, approved];
        assert_eq!(ready_at(&all, H1), Some("2026-01-01T03:00:00Z"));
        assert_eq!(ready_at(&all[..3], H1), Some("2026-01-01T03:00:00Z"));
        // Sent back and not Ready again; Ready at another head; no note at all.
        assert_eq!(ready_at(&all[..2], H1), None);
        assert_eq!(ready_at(&[first], &"2".repeat(40)), None);
        assert_eq!(ready_at(&[c("checks pass", "x")], H1), None);
        assert_eq!(ready_at(&[], H1), None);
    }
}
