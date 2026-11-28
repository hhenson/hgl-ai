//! Owner-authenticated, exact-head Codex review requests, serialized by a hub lease.
use std::time::Duration;

use crate::{
    Result, Settings,
    bot::{self, BotView, Comment, Review},
    exec::{self, Gh},
    gate, invalid,
    lock::{LandLock, holder},
};
use hgl_ai_bus::Bus;

#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Needed,
    Already,
    Recorded(String),
    Refused(String),
    Posted,
}

/// An owner-authored exact-head request marker.
pub fn requested(comments: &[Comment], owner: &str, head: &str) -> bool {
    let marker = format!("<!-- hgl-ai-bot-request:v1 {head} -->");
    comments
        .iter()
        .any(|c| c.author.eq_ignore_ascii_case(owner) && c.body.contains(&marker))
}

pub fn body(head: &str) -> String {
    format!("{}\n\n<!-- hgl-ai-bot-request:v1 {head} -->", bot::REQUEST)
}

/// Reject a fleet or other credential before any owner action or decision record.
pub async fn verify_owner<G: Gh>(gh: &G, s: &Settings) -> Result<()> {
    let user = exec::get(gh, "user").await?;
    let login = user["login"].as_str().unwrap_or("");
    if login.is_empty() || !login.eq_ignore_ascii_case(&s.owner) {
        return Err(invalid(format!(
            "authenticated GitHub login {login:?} is not configured owner {:?}",
            s.owner
        )));
    }
    Ok(())
}

fn validate(s: &Settings, repo: &str, pr: u64, head: &str) -> Result<()> {
    hgl_ai_proto::check_head(head)?;
    if !s.repos.iter().any(|r| r == repo) || pr == 0 {
        return Err(invalid(
            "repository is not in the fleet or PR number is zero",
        ));
    }
    Ok(())
}

pub async fn check<G: Gh>(
    gh: &G,
    s: &Settings,
    repo: &str,
    pr: u64,
    head: &str,
) -> Result<Request> {
    validate(s, repo, pr, head)?;
    let path = format!("repos/{}/issues/{pr}/comments?per_page=100", s.full(repo));
    let comments: Vec<Comment> = exec::get_list(gh, &path)
        .await?
        .iter()
        .map(Comment::from_rest)
        .collect();
    let marker = format!("<!-- hgl-ai-bot-request:v1 {head} -->");
    if let Some(c) = comments
        .iter()
        .find(|c| c.author.eq_ignore_ascii_case(&s.owner) && c.body.contains(&marker))
    {
        if c.created_at.is_empty() {
            return Err(invalid("owner request comment lacks created_at"));
        }
        return Ok(Request::Recorded(c.created_at.clone()));
    }
    let pull = gate::fetch_pr(gh, s, repo, pr).await?;
    if pull.merged || pull.state != "open" || pull.head != head {
        return Ok(Request::Refused(format!(
            "{repo}#{pr} is closed, merged, or its head moved"
        )));
    }
    let path = format!("repos/{}/pulls/{pr}/reviews?per_page=100", s.full(repo));
    let reviews: Vec<Review> = exec::get_list(gh, &path)
        .await?
        .iter()
        .map(Review::from_rest)
        .collect();
    let view = BotView::of(&s.review_bot, &comments, &reviews);
    if view.clean_on(head) || view.findings_on(head) || view.running_on(head) {
        return Ok(Request::Already);
    }
    Ok(Request::Needed)
}

/// The lease spans the check, post, and decision record. A failed post never logs an action.
pub async fn request<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    repo: &str,
    pr: u64,
    head: &str,
) -> Result<Request> {
    validate(s, repo, pr, head)?;
    verify_owner(gh, s).await?;
    let lock = LandLock::acquire_named(
        bus,
        &format!("lock.bot.{repo}.{pr}"),
        &holder(s, "bot request"),
        Duration::ZERO,
    )
    .await?;
    let result = async {
        let state = check(gh, s, repo, pr, head).await?;
        let (created, outcome) = match state {
            Request::Recorded(created) => (created, Request::Already),
            Request::Needed => {
                let pull = gate::fetch_pr(gh, s, repo, pr).await?;
                if pull.merged || pull.state != "open" || pull.head != head {
                    return Ok(Request::Refused(format!("{repo}#{pr} is closed, merged, or its head moved")));
                }
                lock.confirm("bot review request").await?;
                let posted = exec::write(gh, "POST", &format!("repos/{}/issues/{pr}/comments", s.full(repo)), &[("body", &body(head))]).await?;
                let created = posted["created_at"].as_str().filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("request posted but response lacks created_at; retry to recover its decision record"))?;
                (created.to_string(), Request::Posted)
            }
            other => return Ok(other),
        };
        lock.confirm("bot request decision record").await?;
        let line = format!("{created} {}: bot review request for {repo}#{pr} at {head} (owner action)", s.controller);
        hgl_ai_owner::decision_once(bus, &line).await.map_err(|e| invalid(format!("request posted but decision log failed; retry to recover: {e}")))?;
        Ok(outcome)
    }
    .await;
    lock.release().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        exec::fake::FakeGh,
        testutil::{fixture, settings},
    };
    const HEAD: &str = "1f461fa77bba922fe81383df4c475feb58b7c233";
    fn setup(comments: &str, reviews: &str) -> (Settings, FakeGh) {
        let mut s = settings();
        s.repos = vec!["alpha".into()];
        s.owner = "owner".into();
        s.review_bot = "chatgpt-codex-connector".into();
        let gh = FakeGh::default()
            .on("pulls/7/reviews", reviews)
            .on("pulls/7", &fixture("pr-open.json"))
            .on("issues/7/comments", comments);
        (s, gh)
    }
    #[tokio::test]
    async fn rejects_other_authentication() {
        let (s, _) = setup("[]", "[]");
        let gh = FakeGh::default().on("api user", r#"{"login":"fleet"}"#);
        assert!(verify_owner(&gh, &s).await.is_err());
        assert_eq!(gh.calls().len(), 1);
        let gh = FakeGh::default().on("api user", r#"{"login":"Owner"}"#);
        verify_owner(&gh, &s).await.unwrap();
    }
    #[tokio::test]
    async fn exact_owner_marker_only() {
        let comments = serde_json::json!([
            {"user":{"login":"stranger"}, "body":body(HEAD), "created_at":"2026-01-01T00:00:00Z"},
            {"user":{"login":"owner"}, "body":format!("Skipped @codex review on {HEAD}")}
        ])
        .to_string();
        let (s, gh) = setup(&comments, "[]");
        assert_eq!(
            check(&gh, &s, "alpha", 7, HEAD).await.unwrap(),
            Request::Needed
        );
        let comments = serde_json::json!([{"user":{"login":"owner"}, "body":body(HEAD), "created_at":"2026-01-01T00:00:00Z"}]).to_string();
        let (s, gh) = setup(&comments, "[]");
        assert!(matches!(
            check(&gh, &s, "alpha", 7, HEAD).await.unwrap(),
            Request::Recorded(_)
        ));
        assert!(matches!(
            check(
                &gh,
                &s,
                "alpha",
                7,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )
            .await
            .unwrap(),
            Request::Refused(_)
        ));
    }
    #[tokio::test]
    async fn bot_review_and_running_stop_requests() {
        let receipt = serde_json::json!([{"user":{"login":"chatgpt-codex-connector[bot]"}, "body":"Didn't find any major issues. **Reviewed commit:** `1f461fa77b`"}]).to_string();
        let (s, gh) = setup(&receipt, "[]");
        assert_eq!(
            check(&gh, &s, "alpha", 7, HEAD).await.unwrap(),
            Request::Already
        );
        let running = fixture("comments-running.json")
            .replace("9a8b7c6d5e4f30211203f4e5d6c7b8a9a0b1c2d3", HEAD);
        let (s, gh) = setup(&running, "[]");
        assert_eq!(
            check(&gh, &s, "alpha", 7, HEAD).await.unwrap(),
            Request::Already
        );
        let reviews = serde_json::json!([{"user":{"login":"chatgpt-codex-connector[bot]"}, "commit_id":HEAD}]).to_string();
        let (s, gh) = setup("[]", &reviews);
        assert_eq!(
            check(&gh, &s, "alpha", 7, HEAD).await.unwrap(),
            Request::Already
        );
    }
}

/// One pass over the coordinator's persisted table. Each candidate uses the same
/// identity, lease and exact-head checks as the individual request command.
pub async fn pass<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
    dry_run: bool,
) -> Result<Vec<(String, u64, String, Result<Request>)>> {
    use crate::{bot::BotState, state, table::Table};
    let entry = bus
        .state_get(state::KEY_PRS)
        .await?
        .ok_or_else(|| invalid("no coordinator PR table; wait for a coordinator round"))?;
    let table = Table::parse(&entry.value);
    let mut outcomes = Vec::new();
    for row in table
        .rows
        .into_iter()
        .filter(|r| !r.draft && r.bot == BotState::None && s.repos.contains(&r.repo))
    {
        let result = if dry_run {
            match verify_owner(gh, s).await {
                Ok(()) => check(gh, s, &row.repo, row.number, &row.head).await,
                Err(e) => Err(e),
            }
        } else {
            request(gh, s, bus, &row.repo, row.number, &row.head).await
        };
        outcomes.push((row.repo, row.number, row.head, result));
    }
    Ok(outcomes)
}
