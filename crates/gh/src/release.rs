//! `gh release watch SHA`: hold the repository's landing lock while every workflow run on
//! the commit (of one event, `push` by default) finishes, so `main` does not move under a
//! release. Prints each run and the jobs of the release workflows; the last line is
//! `ALL GREEN on <sha8>` or `NOT ALL GREEN on <sha8>`.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::exec::{self, Gh};
use crate::lock::Lock;
use crate::{Result, Settings, short};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub attempt: u64,
}

/// Runs from `actions/runs` pages.
pub fn runs(pages: &[Value]) -> Vec<Run> {
    pages
        .iter()
        .flat_map(|p| {
            p.get("workflow_runs")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .map(|r| Run {
            id: r.get("id").and_then(Value::as_u64).unwrap_or(0),
            name: r
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            status: r
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            conclusion: r
                .get("conclusion")
                .and_then(Value::as_str)
                .map(String::from),
            attempt: r.get("run_attempt").and_then(Value::as_u64).unwrap_or(1),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watch {
    /// Not finished: why.
    Waiting(String),
    Done {
        green: bool,
    },
}

/// Finished when every expected workflow has a run and no run is still going.
pub fn evaluate(runs: &[Run], expected: &[String]) -> Watch {
    if runs.is_empty() {
        return Watch::Waiting("no runs yet".into());
    }
    let missing: Vec<&str> = expected
        .iter()
        .filter(|w| !runs.iter().any(|r| &r.name == *w))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Watch::Waiting(format!("no run yet of {}", missing.join(", ")));
    }
    let busy: Vec<&str> = runs
        .iter()
        .filter(|r| r.status != "completed")
        .map(|r| r.name.as_str())
        .collect();
    if !busy.is_empty() {
        return Watch::Waiting(format!("running: {}", busy.join(", ")));
    }
    Watch::Done {
        green: runs
            .iter()
            .all(|r| r.conclusion.as_deref() == Some("success")),
    }
}

pub fn render(runs: &[Run]) -> String {
    runs.iter()
        .map(|r| {
            format!(
                "{}\t{}\t{}\t{}\t{}\n",
                r.id,
                r.name,
                r.status,
                r.conclusion.as_deref().unwrap_or("-"),
                r.attempt
            )
        })
        .collect()
}

/// `  job <name>: <conclusion or status>` for each job of a run.
pub fn render_jobs(v: &Value) -> String {
    v.get("jobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|j| {
            let name = j.get("name").and_then(Value::as_str).unwrap_or("");
            let st = j
                .get("conclusion")
                .and_then(Value::as_str)
                .or_else(|| j.get("status").and_then(Value::as_str))
                .unwrap_or("");
            format!("  job {name}: {st}\n")
        })
        .collect()
}

pub fn verdict_line(green: bool, sha: &str) -> String {
    if green {
        format!("ALL GREEN on {}", short(sha))
    } else {
        format!("NOT ALL GREEN on {}", short(sha))
    }
}

#[derive(Debug, Clone)]
pub struct WatchOpts {
    pub repo: String,
    pub event: String,
    pub timeout: Duration,
    pub interval: Duration,
    pub lock_wait: Duration,
    /// Read once, take no lock.
    pub dry_run: bool,
}

/// Watch the runs on `sha`; the report and whether all are green.
pub async fn watch<G: Gh>(
    gh: &G,
    s: &Settings,
    sha: &str,
    o: &WatchOpts,
) -> Result<(String, bool)> {
    hgl_ai_proto::check_head(sha)?;
    let _lock = if o.dry_run {
        None
    } else {
        Some(Lock::acquire(&s.lock_path(&o.repo), o.lock_wait).await?)
    };
    let path = format!(
        "repos/{}/actions/runs?head_sha={sha}&event={}&per_page=50",
        s.full(&o.repo),
        o.event
    );
    let deadline = Instant::now() + o.timeout;
    let (list, green) = loop {
        let list = runs(&[exec::get(gh, &path).await?]);
        match evaluate(&list, &s.release.workflows) {
            Watch::Done { green } => break (list, green),
            Watch::Waiting(why) => {
                if o.dry_run || Instant::now() + o.interval >= deadline {
                    eprintln!("{why}");
                    break (list, false);
                }
                tokio::time::sleep(o.interval).await;
            }
        }
    };
    let mut report = render(&list);
    for r in list
        .iter()
        .filter(|r| s.release.list_jobs.contains(&r.name))
    {
        let jobs = exec::get(
            gh,
            &format!(
                "repos/{}/actions/runs/{}/jobs?per_page=100",
                s.full(&o.repo),
                r.id
            ),
        )
        .await?;
        report.push_str(&render_jobs(&jobs));
    }
    report.push_str(&verdict_line(green, sha));
    report.push('\n');
    Ok((report, green))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{json, settings};

    fn expected() -> Vec<String> {
        settings().release.workflows
    }

    #[test]
    fn finished_and_green() {
        let r = runs(&[json("runs-green.json")]);
        assert_eq!(r.len(), 2);
        assert_eq!(evaluate(&r, &expected()), Watch::Done { green: true });
        assert_eq!(render(&r[..1]), "101\tCI\tcompleted\tsuccess\t1\n");
        assert_eq!(verdict_line(true, &"a".repeat(40)), "ALL GREEN on aaaaaaaa");
    }

    #[test]
    fn waiting_or_red() {
        let r = runs(&[json("runs-green.json")]);
        assert!(
            matches!(evaluate(&r[..1], &expected()), Watch::Waiting(w) if w.contains("Release bundles"))
        );
        assert!(matches!(evaluate(&[], &expected()), Watch::Waiting(_)));
        let mut busy = r.clone();
        busy[1].status = "in_progress".into();
        busy[1].conclusion = None;
        assert!(
            matches!(evaluate(&busy, &expected()), Watch::Waiting(w) if w == "running: Release bundles")
        );
        let mut red = r.clone();
        red[0].conclusion = Some("failure".into());
        assert_eq!(evaluate(&red, &expected()), Watch::Done { green: false });
        assert_eq!(verdict_line(false, "abc"), "NOT ALL GREEN on abc");
    }

    #[test]
    fn jobs() {
        let j = serde_json::json!({"jobs": [{"name": "bundle", "status": "completed", "conclusion": "success"}, {"name": "publish", "status": "queued", "conclusion": null}]});
        assert_eq!(
            render_jobs(&j),
            "  job bundle: success\n  job publish: queued\n"
        );
    }
}
