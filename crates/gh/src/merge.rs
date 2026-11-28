//! Merging a pull request at its exact head, through the one path that serves every pull
//! request (#21): `PUT pulls/N/merge-async` with the head's `sha`, a merge commit, a direct
//! merge (not a queue) and the ruleset bypass (`merge_by_admin`), then
//! `GET merge-async/{uuid}` until it reports `merged` or `failed`. It is the API GitHub
//! requires for a stacked pull request; for one, it also merges the open pull requests
//! below it, which the gate rules out by requiring the base `main`. Whether the pull
//! request merged is read back from it by the caller.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::exec::{self, Gh};
use crate::{Error, Result};

/// How often an accepted merge is asked about.
pub const POLL_EVERY: Duration = Duration::from_secs(3);
/// How long an accepted merge is polled when the caller gives no deadline.
pub const POLL_LIMIT: Duration = Duration::from_secs(600);

/// The latest an accepted merge started `now` is polled until: the landing's `deadline`
/// (else [`POLL_LIMIT`] from now), and no later than the landing lock's `lease` from now
/// (its `max_age`; zero is no expiry). Taken at the merge, so that a gate that waited
/// longer than the lease does not start the merge already past its bound.
pub fn until(now: Instant, deadline: Option<Instant>, lease: Option<Duration>) -> Instant {
    let u = deadline.unwrap_or(now + POLL_LIMIT);
    match lease.filter(|l| !l.is_zero()) {
        Some(l) => u.min(now + l),
        None => u,
    }
}

/// What a merge request came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merge {
    /// GitHub reported the merge done.
    Merged,
    /// Nothing of ours is pending: refused before it was accepted, or reported `failed`,
    /// `enqueued` or a status this does not know.
    Refused(String),
    /// Accepted and not reported done by the deadline, or its result unreadable: GitHub may
    /// still merge it.
    Pending(String),
}

/// The request's body: `bypass_rules` is a JSON boolean (`-f` would send the string).
pub fn body(head: &str) -> Value {
    json!({
        "sha": head,
        "merge_method": "merge",
        "merge_action": "direct_merge",
        "bypass_rules": true,
    })
}

fn status(v: &Value) -> String {
    v.get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn detail<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get("details")
        .and_then(|d| d.get(k))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// What the answer says (`details.message`), else the answer itself.
fn message(v: &Value) -> String {
    detail(v, "message")
        .map(String::from)
        .unwrap_or_else(|| v.to_string())
}

/// A final answer, or `None` while it is `pending`.
fn terminal(v: &Value) -> Option<Merge> {
    match status(v).as_str() {
        "pending" => None,
        "merged" => Some(Merge::Merged),
        "failed" => Some(Merge::Refused(format!(
            "asynchronous merge failed: {}",
            message(v)
        ))),
        "enqueued" => Some(Merge::Refused(format!(
            "added to a merge queue, not merged directly: {}",
            message(v)
        ))),
        "" => Some(Merge::Refused(format!(
            "asynchronous merge: no status in {v}"
        ))),
        other => Some(Merge::Refused(format!(
            "asynchronous merge: unknown status {other}: {}",
            message(v)
        ))),
    }
}

/// Merge `full` (`owner/name`) #`number` at `head`, polling every `every` until `deadline`.
/// `Err` only for what the caller handles itself: refused credentials, a rate limit on the
/// request, or a run that did not happen.
pub async fn merge<G: Gh>(
    gh: &G,
    full: &str,
    number: u64,
    head: &str,
    deadline: Instant,
    every: Duration,
) -> Result<Merge> {
    let path = format!("repos/{full}/pulls/{number}/merge-async");
    let (out, shown) = exec::send_json(gh, "PUT", &path, &body(head)).await?;
    let answer: Option<Value> = serde_json::from_str(out.stdout.trim()).ok();
    let v = match answer {
        Some(v) if out.ok => v,
        // HTTP 409: a merge request is already pending; its uuid and options are the body.
        Some(v) if detail(&v, "uuid").is_some() => {
            let sha = detail(&v, "expected_head_sha").unwrap_or("");
            let method = detail(&v, "merge_method").unwrap_or("");
            if sha != head || method != "merge" {
                return Ok(Merge::Refused(format!(
                    "another merge request is pending (head {}, method {}), not this one at {}",
                    crate::short(sha),
                    if method.is_empty() { "none" } else { method },
                    crate::short(head)
                )));
            }
            v
        }
        _ if !out.ok => {
            return match exec::failure(shown, &out) {
                e @ (Error::Auth(_) | Error::RateLimited(_)) => Err(e),
                e => Ok(Merge::Refused(e.to_string())),
            };
        }
        // Accepted (a success) without an answer to read: GitHub may be merging it.
        _ => {
            return Ok(Merge::Pending(format!(
                "{path}: accepted, the answer is not JSON: {}",
                out.stdout.trim()
            )));
        }
    };
    if out.ok && status(&v).is_empty() {
        return Ok(Merge::Pending(format!(
            "{path}: accepted with no status: {v}"
        )));
    }
    if let Some(done) = terminal(&v) {
        return Ok(done);
    }
    let Some(id) = detail(&v, "uuid") else {
        return Ok(Merge::Pending(format!(
            "accepted with no details.uuid to poll: {v}"
        )));
    };
    let poll = format!("{path}/{id}");
    let mut last = "pending".to_string();
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(Merge::Pending(format!("{poll}: {last} at the deadline")));
        }
        tokio::time::sleep(every.min(deadline - now)).await;
        match exec::get(gh, &poll).await {
            Ok(v) => match terminal(&v) {
                Some(done) => return Ok(done),
                None => last = "pending".into(),
            },
            // Refused credentials or a rate limit are the caller's: the merge may still
            // be under way, so the landing's intent stays and a later attempt finds it.
            Err(e @ (Error::Auth(_) | Error::RateLimited(_))) => return Err(e),
            // A 5xx, a timeout, an unreadable answer: asked again.
            Err(e) => last = format!("unreadable ({e})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Output;
    use crate::exec::fake::FakeGh;

    const HEAD: &str = "1f461fa77bba922fe81383df4c475feb58b7c233";
    const OTHER: &str = "2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a";
    const PUT: &str = "-X PUT repos/o/r/pulls/7/merge-async --input -";
    const POLL: &str = "api repos/o/r/pulls/7/merge-async/u-1";
    const MS: Duration = Duration::from_millis(1);

    fn answer(status: &str, details: &str) -> String {
        format!("{{\"status\":\"{status}\",\"details\":{details}}}")
    }

    fn pending_at(sha: &str, method: &str) -> String {
        answer(
            "pending",
            &format!(
                "{{\"uuid\":\"u-1\",\"message\":\"accepted\",\"merge_method\":\"{method}\",\"merge_action\":\"direct_merge\",\"expected_head_sha\":\"{sha}\",\"bypass_rules\":true}}"
            ),
        )
    }

    fn pending() -> String {
        pending_at(HEAD, "merge")
    }

    fn accepted() -> FakeGh {
        FakeGh::default().on(PUT, &pending())
    }

    /// A failed run whose body is `stdout` (as `gh api` prints an error's body).
    fn refused(gh: FakeGh, pattern: &str, stdout: &str, stderr: &str) -> FakeGh {
        gh.rules.lock().unwrap().push((
            pattern.into(),
            Output {
                ok: false,
                stdout: stdout.into(),
                stderr: stderr.into(),
            },
            false,
        ));
        gh
    }

    fn calls(gh: &FakeGh, part: &str) -> usize {
        gh.calls().iter().filter(|c| c.contains(part)).count()
    }

    fn soon(d: Duration) -> Instant {
        Instant::now() + d
    }

    async fn run(gh: &FakeGh, d: Duration) -> Result<Merge> {
        merge(gh, "o/r", 7, HEAD, soon(d), MS).await
    }

    #[tokio::test]
    async fn one_call_with_the_bypass_as_a_boolean() {
        let gh = accepted().once(POLL, &pending()).on(
            POLL,
            &answer("merged", "{\"message\":\"m\",\"sha\":\"abc\"}"),
        );
        assert_eq!(
            run(&gh, Duration::from_secs(5)).await.unwrap(),
            Merge::Merged
        );
        assert_eq!(calls(&gh, "-X PUT"), 1, "{:?}", gh.calls());
        assert_eq!(calls(&gh, "pulls/7/merge "), 0, "no synchronous merge");
        assert_eq!(calls(&gh, POLL), 2);
        assert!(
            gh.calls()
                .iter()
                .filter(|c| c.contains(POLL))
                .all(|c| !c.contains("-X"))
        );
        let inputs = gh.inputs.lock().unwrap().clone();
        assert_eq!(inputs.len(), 1);
        let sent: Value = serde_json::from_str(&inputs[0].1).unwrap();
        assert_eq!(sent["sha"], HEAD);
        assert_eq!(sent["merge_method"], "merge");
        assert_eq!(sent["merge_action"], "direct_merge");
        assert_eq!(
            sent["bypass_rules"],
            Value::Bool(true),
            "a boolean, not \"true\""
        );
    }

    #[tokio::test]
    async fn terminal_answers() {
        let gh = accepted().on(POLL, &answer("failed", "{\"message\":\"Merge conflict\"}"));
        assert_eq!(
            run(&gh, Duration::from_secs(5)).await.unwrap(),
            Merge::Refused("asynchronous merge failed: Merge conflict".into())
        );
        let gh = accepted().on(POLL, &answer("enqueued", "{\"message\":\"q\"}"));
        assert!(
            matches!(run(&gh, Duration::from_secs(5)).await.unwrap(), Merge::Refused(m) if m.contains("merge queue"))
        );
        // An unknown status ends the poll at once.
        let gh = accepted().on(POLL, &answer("cancelled", "{\"message\":\"x\"}"));
        assert!(
            matches!(run(&gh, Duration::from_secs(5)).await.unwrap(), Merge::Refused(m) if m.contains("unknown status cancelled"))
        );
        assert_eq!(calls(&gh, POLL), 1);
        // Already merged (HTTP 200): no poll.
        let gh = FakeGh::default().on(PUT, &answer("merged", "{\"message\":\"m\"}"));
        assert_eq!(run(&gh, MS).await.unwrap(), Merge::Merged);
        assert_eq!(calls(&gh, POLL), 0);
        // Refused before acceptance (HTTP 400, a draft).
        let gh = FakeGh::default().fail(PUT, "gh: Pull request is a draft (HTTP 400)");
        assert!(matches!(run(&gh, MS).await.unwrap(), Merge::Refused(m) if m.contains("HTTP 400")));
        // Refused credentials and a rate limit on the request are the caller's.
        let gh = FakeGh::default().fail(PUT, "gh: Bad credentials (HTTP 401)");
        assert!(matches!(run(&gh, MS).await, Err(Error::Auth(_))));
        let gh = FakeGh::default().fail(PUT, "gh: API rate limit exceeded (HTTP 403)");
        assert!(matches!(run(&gh, MS).await, Err(Error::RateLimited(_))));
    }

    #[tokio::test]
    async fn transient_poll_failures_are_asked_again() {
        let gh = accepted()
            .push_fail_once(POLL, "gh: Server Error (HTTP 502)")
            .on(POLL, &answer("merged", "{\"message\":\"m\"}"));
        assert_eq!(
            run(&gh, Duration::from_secs(5)).await.unwrap(),
            Merge::Merged
        );
        assert_eq!(calls(&gh, POLL), 2);
        // Refused credentials or a rate limit stop the poll and go to the caller (r6): the
        // merge may still happen, so the landing's intent stays. Neither is retried.
        let gh = accepted().fail(POLL, "gh: Bad credentials (HTTP 401)");
        assert!(matches!(
            run(&gh, Duration::from_secs(5)).await,
            Err(Error::Auth(_))
        ));
        assert_eq!(calls(&gh, POLL), 1);
        let gh = accepted().fail(POLL, "gh: API rate limit exceeded (HTTP 403)");
        assert!(matches!(
            run(&gh, Duration::from_secs(5)).await,
            Err(Error::RateLimited(_))
        ));
        assert_eq!(calls(&gh, POLL), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn pending_at_the_deadline() {
        let gh = accepted().on(POLL, &pending());
        let start = Instant::now();
        let d = Duration::from_secs(10);
        let m = merge(&gh, "o/r", 7, HEAD, start + d, Duration::from_secs(3))
            .await
            .unwrap();
        assert!(
            matches!(&m, Merge::Pending(w) if w.contains("pending at the deadline")),
            "{m:?}"
        );
        // Sleeps of 3, 3, 3, then 1 (clamped): four polls, ending at the deadline exactly.
        assert_eq!(calls(&gh, POLL), 4);
        assert_eq!(Instant::now() - start, d);
        // A poll that keeps failing ends pending too, saying why.
        let gh = accepted().fail(POLL, "gh: Server Error (HTTP 502)");
        let m = merge(&gh, "o/r", 7, HEAD, soon(d), Duration::from_secs(3))
            .await
            .unwrap();
        assert!(
            matches!(&m, Merge::Pending(w) if w.contains("HTTP 502")),
            "{m:?}"
        );
        // Accepted with no uuid: pending, never polled blindly.
        let gh = FakeGh::default().on(PUT, "{\"status\":\"pending\",\"details\":{}}");
        assert!(
            matches!(run(&gh, d).await.unwrap(), Merge::Pending(w) if w.contains("no details.uuid"))
        );
        assert_eq!(calls(&gh, "merge-async/"), 0);
    }

    /// r1 on #50, minor 4: a call GitHub accepted whose answer cannot be read is not a
    /// refusal; the merge may be happening.
    #[tokio::test]
    async fn an_accepted_call_without_a_readable_answer_is_pending() {
        for answer in [
            "",
            "<html>accepted</html>",
            "{\"details\":{\"uuid\":\"u-1\"}}",
        ] {
            let gh = FakeGh::default().on(PUT, answer);
            let m = run(&gh, MS).await.unwrap();
            assert!(
                matches!(&m, Merge::Pending(w) if w.contains("accepted")),
                "{answer}: {m:?}"
            );
            assert_eq!(calls(&gh, POLL), 0);
        }
    }

    #[tokio::test]
    async fn an_already_pending_request_is_adopted_only_if_it_is_ours() {
        let busy = "gh: a merge request is already pending (HTTP 409)";
        let gh = refused(FakeGh::default(), PUT, &pending(), busy)
            .on(POLL, &answer("merged", "{\"message\":\"m\"}"));
        assert_eq!(
            run(&gh, Duration::from_secs(5)).await.unwrap(),
            Merge::Merged
        );
        assert_eq!(calls(&gh, POLL), 1);
        for (sha, method) in [(OTHER, "merge"), (HEAD, "squash")] {
            let gh = refused(FakeGh::default(), PUT, &pending_at(sha, method), busy);
            let m = run(&gh, Duration::from_secs(5)).await.unwrap();
            assert!(
                matches!(&m, Merge::Refused(w) if w.contains("another merge request is pending")),
                "{m:?}"
            );
            assert_eq!(calls(&gh, POLL), 0, "not polled");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_bound_is_taken_at_the_merge() {
        let min = |m: u64| Duration::from_secs(60 * m);
        let start = Instant::now();
        let deadline = start + min(50);
        let lease = Some(min(30));
        // The gate waited 40 minutes, longer than the lease's max age: the bound is the
        // landing's deadline, 10 minutes on, not the lease from the start (already past).
        tokio::time::advance(min(40)).await;
        let now = Instant::now();
        assert!(start + min(30) < now);
        assert_eq!(until(now, Some(deadline), lease), deadline);
        // The lease from now when it ends first; the default limit with no deadline; a
        // zero max age is no expiry.
        assert_eq!(until(now, Some(now + min(90)), lease), now + min(30));
        assert_eq!(until(now, None, lease), now + POLL_LIMIT);
        assert_eq!(until(now, Some(deadline), Some(Duration::ZERO)), deadline);
        // And the merge started then polls until it is reported merged.
        let gh = accepted()
            .once(POLL, &pending())
            .on(POLL, &answer("merged", "{\"message\":\"m\"}"));
        let m = merge(
            &gh,
            "o/r",
            7,
            HEAD,
            until(now, Some(deadline), lease),
            POLL_EVERY,
        )
        .await
        .unwrap();
        assert_eq!(m, Merge::Merged);
        assert_eq!(calls(&gh, POLL), 2);
    }

    #[tokio::test]
    async fn dry_run_refuses_it() {
        let gh = crate::exec::ReadOnly(accepted());
        assert!(merge(&gh, "o/r", 7, HEAD, soon(MS), MS).await.is_err());
        assert!(gh.0.calls().is_empty());
    }
}
