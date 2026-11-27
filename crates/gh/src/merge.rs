//! Merging a pull request at its exact head. The plain REST merge first; a pull request in a
//! stack refuses it ("must be merged using the asynchronous merge REST API"), and is merged
//! through `pulls/N/merge-async` with the same `sha`, polled until it reports `merged` or
//! `failed` (#21). Whether it merged is read back from the pull request by the caller.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::exec::{self, Gh};
use crate::{Error, Result};

/// How often a queued asynchronous merge is asked about.
pub const POLL_EVERY: Duration = Duration::from_secs(3);
/// How long an asynchronous merge may stay queued before the attempt gives up.
pub const POLL_LIMIT: Duration = Duration::from_secs(600);

/// Whether a refused merge says to use the asynchronous merge API.
pub fn wants_async(e: &Error) -> bool {
    let m = e.to_string().to_ascii_lowercase();
    m.contains("asynchronous merge") || m.contains("merge-async")
}

/// Merge `repo` (`owner/name`) #`number` at `head`; `Ok` when GitHub accepted the merge.
pub async fn merge<G: Gh>(gh: &G, full: &str, number: u64, head: &str) -> Result<()> {
    merge_every(gh, full, number, head, POLL_EVERY, POLL_LIMIT).await
}

pub(crate) async fn merge_every<G: Gh>(
    gh: &G,
    full: &str,
    number: u64,
    head: &str,
    every: Duration,
    limit: Duration,
) -> Result<()> {
    let path = format!("repos/{full}/pulls/{number}/merge");
    match exec::write(
        gh,
        "PUT",
        &path,
        &[("merge_method", "merge"), ("sha", head)],
    )
    .await
    {
        Err(e) if wants_async(&e) => merge_async(gh, &path, head, every, limit).await,
        other => other.map(drop),
    }
}

/// The status of an asynchronous merge (`pending`, `merged`, `enqueued`, `failed`), lower case.
fn status(v: &Value) -> Option<String> {
    v.get("status")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
}

/// The asynchronous merge's id, in `details.uuid` while it is pending.
fn uuid(v: &Value) -> Option<String> {
    v.pointer("/details/uuid")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .map(String::from)
}

/// What the answer says (`details.message`), else the answer itself.
fn message(v: &Value) -> String {
    v.pointer("/details/message")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| v.to_string())
}

/// `PUT pulls/N/merge-async` with the exact head, then `GET merge-async/{uuid}` until it is
/// `merged` or `failed`. A merge already pending for the pull request (HTTP 409) answers
/// with that request's uuid, which is polled the same way.
async fn merge_async<G: Gh>(
    gh: &G,
    path: &str,
    head: &str,
    every: Duration,
    limit: Duration,
) -> Result<()> {
    let path = format!("{path}-async");
    let mut args: Vec<String> = ["api", "-X", "PUT", path.as_str()]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for f in [
        format!("sha={head}"),
        "merge_method=merge".into(),
        "merge_action=direct_merge".into(),
    ] {
        args.push("-f".into());
        args.push(f);
    }
    let shown = args.join(" ");
    let out = gh.exec(args, None).await?;
    let answer: Option<Value> = serde_json::from_str(out.stdout.trim()).ok();
    let mut v = match answer {
        Some(v) if out.ok => v,
        Some(v) if uuid(&v).is_some() => v,
        _ if !out.ok => return Err(exec::failure(shown, &out)),
        _ => {
            return Err(Error::Response {
                what: path,
                msg: format!("not JSON: {}", out.stdout.trim()),
            });
        }
    };
    let deadline = Instant::now() + limit;
    let mut poll: Option<String> = None;
    loop {
        match status(&v).as_deref() {
            Some("merged") => return Ok(()),
            Some("failed") => {
                return Err(Error::Command {
                    args: shown,
                    msg: format!("asynchronous merge failed: {}", message(&v)),
                });
            }
            Some("enqueued") => {
                return Err(Error::Command {
                    args: shown,
                    msg: format!(
                        "added to a merge queue, not merged directly: {}",
                        message(&v)
                    ),
                });
            }
            _ => {}
        }
        if poll.is_none() {
            let id = uuid(&v).ok_or_else(|| Error::Response {
                what: path.clone(),
                msg: format!("no details.uuid in {v}"),
            })?;
            poll = Some(format!("{path}/{id}"));
        }
        if Instant::now() >= deadline {
            return Err(Error::Command {
                args: shown,
                msg: format!(
                    "asynchronous merge not done in {limit:?} (last status {})",
                    status(&v).unwrap_or_else(|| "none".into())
                ),
            });
        }
        tokio::time::sleep(every).await;
        v = exec::get(gh, poll.as_deref().unwrap_or_default()).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::fake::FakeGh;

    const HEAD: &str = "1f461fa77bba922fe81383df4c475feb58b7c233";
    const STACKED: &str = "gh: Pull request is part of a stack and must be merged using the asynchronous merge REST API (HTTP 405)";
    const MS: Duration = Duration::from_millis(1);
    const LONG: Duration = Duration::from_secs(5);

    fn answer(status: &str, details: &str) -> String {
        format!("{{\"status\":\"{status}\",\"details\":{details}}}")
    }

    fn pending() -> String {
        answer(
            "pending",
            &format!(
                "{{\"uuid\":\"u-1\",\"message\":\"accepted\",\"merge_method\":\"merge\",\"merge_action\":\"direct_merge\",\"expected_head_sha\":\"{HEAD}\"}}"
            ),
        )
    }

    fn stacked() -> FakeGh {
        FakeGh::default()
            .fail("pulls/7/merge -f", STACKED)
            .on("pulls/7/merge-async -f", &pending())
    }

    fn calls(gh: &FakeGh, part: &str) -> Vec<String> {
        gh.calls()
            .into_iter()
            .filter(|c| c.contains(part))
            .collect()
    }

    #[tokio::test]
    async fn plain_merge_first() {
        let gh = FakeGh::default().on("pulls/7/merge", "{\"merged\":true}");
        merge_every(&gh, "o/r", 7, HEAD, MS, MS).await.unwrap();
        let c = gh.calls();
        assert_eq!(c.len(), 1);
        assert!(c[0].contains("-X PUT repos/o/r/pulls/7/merge -f merge_method=merge"));
        assert!(c[0].contains(&format!("sha={HEAD}")));
        // Another refusal is returned as it is, and merge-async is not tried.
        let gh = FakeGh::default().fail("pulls/7/merge", "gh: Head branch was modified (HTTP 409)");
        let e = merge_every(&gh, "o/r", 7, HEAD, MS, MS).await.unwrap_err();
        assert!(e.to_string().contains("409"), "{e}");
        assert!(calls(&gh, "merge-async").is_empty());
    }

    #[tokio::test]
    async fn stacked_goes_async_and_polls_until_merged() {
        let merged = answer("merged", "{\"message\":\"merged\",\"sha\":\"abc\"}");
        let gh = stacked()
            .once("merge-async/u-1", &pending())
            .on("merge-async/u-1", &merged.replace("merged\",", "MERGED\","));
        merge_every(&gh, "o/r", 7, HEAD, MS, LONG).await.unwrap();
        let put = calls(&gh, "-X PUT repos/o/r/pulls/7/merge-async");
        assert_eq!(put.len(), 1, "{:?}", gh.calls());
        for f in [
            format!("sha={HEAD}"),
            "merge_method=merge".into(),
            "merge_action=direct_merge".into(),
        ] {
            assert!(put[0].contains(&f), "{f} in {}", put[0]);
        }
        let polls = calls(&gh, "api repos/o/r/pulls/7/merge-async/u-1");
        assert_eq!(polls.len(), 2, "{:?}", gh.calls());
        assert!(polls.iter().all(|p| !p.contains("-X")), "polls only read");
    }

    #[tokio::test]
    async fn stacked_failures() {
        let gh = stacked().on(
            "merge-async/u-1",
            &answer("failed", "{\"message\":\"Merge conflict\"}"),
        );
        let e = merge_every(&gh, "o/r", 7, HEAD, MS, LONG)
            .await
            .unwrap_err();
        assert!(
            e.to_string()
                .contains("asynchronous merge failed: Merge conflict"),
            "{e}"
        );

        let gh = stacked().on(
            "merge-async/u-1",
            &answer("enqueued", "{\"message\":\"q\"}"),
        );
        let e = merge_every(&gh, "o/r", 7, HEAD, MS, LONG)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("merge queue"), "{e}");

        let gh = stacked().on("merge-async/u-1", &pending());
        let e = merge_every(&gh, "o/r", 7, HEAD, MS, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not done"), "{e}");
        assert!(e.to_string().contains("pending"), "{e}");

        // Accepted with no uuid: refused, never polled blindly.
        let gh = FakeGh::default().fail("pulls/7/merge -f", STACKED).on(
            "pulls/7/merge-async",
            "{\"status\":\"pending\",\"details\":{}}",
        );
        let e = merge_every(&gh, "o/r", 7, HEAD, MS, MS).await.unwrap_err();
        assert!(e.to_string().contains("no details.uuid"), "{e}");
        assert!(calls(&gh, "merge-async/").is_empty());

        // Refused with no usable body (HTTP 400: a draft): the refusal itself.
        let gh = FakeGh::default().fail("pulls/7/merge -f", STACKED).fail(
            "pulls/7/merge-async",
            "gh: Pull request is a draft (HTTP 400)",
        );
        let e = merge_every(&gh, "o/r", 7, HEAD, MS, MS).await.unwrap_err();
        assert!(e.to_string().contains("HTTP 400"), "{e}");
    }

    #[tokio::test]
    async fn already_final_or_already_pending() {
        // Already merged (HTTP 200): no poll.
        let gh = FakeGh::default().fail("pulls/7/merge -f", STACKED).on(
            "pulls/7/merge-async",
            &answer("merged", "{\"message\":\"m\"}"),
        );
        merge_every(&gh, "o/r", 7, HEAD, MS, MS).await.unwrap();
        assert!(calls(&gh, "merge-async/u-1").is_empty());

        // A merge already pending (HTTP 409) answers with its uuid, which is polled.
        let conflict = crate::exec::Output {
            ok: false,
            stdout: pending(),
            stderr: "gh: a merge request is already pending (HTTP 409)".into(),
        };
        let gh = FakeGh::default().fail("pulls/7/merge -f", STACKED);
        gh.rules
            .lock()
            .unwrap()
            .push(("pulls/7/merge-async -f".into(), conflict, false));
        let gh = gh.on("merge-async/u-1", &answer("merged", "{\"message\":\"m\"}"));
        merge_every(&gh, "o/r", 7, HEAD, MS, LONG).await.unwrap();
        assert_eq!(calls(&gh, "merge-async/u-1").len(), 1);
    }

    #[tokio::test]
    async fn dry_run_refuses_merge_async() {
        let gh = crate::exec::ReadOnly(stacked());
        assert!(merge_every(&gh, "o/r", 7, HEAD, MS, MS).await.is_err());
        assert!(gh.0.calls().is_empty());
    }
}
