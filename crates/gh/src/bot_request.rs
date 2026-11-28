//! Exact-head Codex review requests. GitHub comments are the durable retry record.
use crate::{
    Result, Settings, bot,
    exec::{self, Gh},
    gate, invalid,
};

#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Needed,
    Already,
    Refused(String),
}

pub fn body(head: &str) -> String {
    format!("{}\n\n<!-- hgl-ai-bot-request:v1 {head} -->", bot::REQUEST)
}

pub async fn check<G: Gh>(
    gh: &G,
    s: &Settings,
    repo: &str,
    pr: u64,
    head: &str,
) -> Result<Request> {
    hgl_ai_proto::check_head(head)?;
    if !s.repos.iter().any(|r| r == repo) || pr == 0 {
        return Err(invalid(
            "repository is not in the fleet or PR number is zero",
        ));
    }
    let pull = gate::fetch_pr(gh, s, repo, pr).await?;
    if pull.merged || pull.state != "open" || pull.head != head {
        return Ok(Request::Refused(format!(
            "{repo}#{pr} is closed, merged, or its head moved"
        )));
    }
    let path = format!("repos/{}/issues/{pr}/comments?per_page=100", s.full(repo));
    let comments = exec::get_list(gh, &path).await?;
    if comments.iter().any(|c| {
        c["body"]
            .as_str()
            .is_some_and(|b| b.contains(head) && b.contains(bot::REQUEST))
    }) {
        Ok(Request::Already)
    } else {
        Ok(Request::Needed)
    }
}

pub async fn post<G: Gh>(gh: &G, s: &Settings, repo: &str, pr: u64, head: &str) -> Result<()> {
    exec::write(
        gh,
        "POST",
        &format!("repos/{}/issues/{pr}/comments", s.full(repo)),
        &[("body", &body(head))],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        exec::fake::FakeGh,
        testutil::{fixture, settings},
    };
    const HEAD: &str = "1f461fa77bba922fe81383df4c475feb58b7c233";
    #[tokio::test]
    async fn scripted_post_then_retry() {
        let mut s = settings();
        s.repos = vec!["alpha".into()];
        let existing = serde_json::json!([{ "body": body(HEAD) }]).to_string();
        let gh = FakeGh::default()
            .on("pulls/7", &fixture("pr-open.json"))
            .once("--paginate", "[]")
            .on("--paginate", &existing)
            .on("-X POST", "{}");
        assert_eq!(
            check(&gh, &s, "alpha", 7, HEAD).await.unwrap(),
            Request::Needed
        );
        post(&gh, &s, "alpha", 7, HEAD).await.unwrap();
        assert_eq!(
            check(&gh, &s, "alpha", 7, HEAD).await.unwrap(),
            Request::Already
        );
        assert_eq!(
            gh.calls().iter().filter(|c| c.contains("-X POST")).count(),
            1
        );
    }

    #[tokio::test]
    async fn request_is_deduplicated_at_exact_head() {
        let mut s = settings();
        s.repos = vec!["alpha".into()];
        let comment = serde_json::json!([{ "body": body(HEAD) }]).to_string();
        let gh = FakeGh::default()
            .on("pulls/7", &fixture("pr-open.json"))
            .on("issues/7/comments", &comment);
        assert_eq!(
            check(&gh, &s, "alpha", 7, HEAD).await.unwrap(),
            Request::Already
        );
        let gh = FakeGh::default()
            .on("pulls/7", &fixture("pr-open.json"))
            .on("issues/7/comments", "[]");
        assert_eq!(
            check(&gh, &s, "alpha", 7, HEAD).await.unwrap(),
            Request::Needed
        );
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
}
