//! The build driver (BUILD-4): a recipe's steps for one platform, in order, each with its
//! needs checked, run by an executor into `<log_dir>/<step>.log`, parsed, and gathered
//! into a [`Summary`]. It stops at the first failing step unless told to keep going.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use hgl_ai_proto::build::{BUILD_VERSION, BuildStatus, StepStatus, StepSummary, Summary, Tests};
use hgl_ai_proto::utc_now_stamp;

use crate::exec::{Cancel, Executor, StepSpec, Workspace, expand_vars};
use crate::parse::{self, Reports};
use crate::recipe::{Need, Recipe, sdk_var};
use crate::summary::{Collect, overall};

/// Largest part of a step log that is parsed.
pub const PARSE_LIMIT: u64 = 256 << 20;

/// What the summary says about a run, besides its steps.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub id: String,
    pub repo: String,
    pub head: Option<String>,
    pub base: Option<String>,
    pub dirty: bool,
    /// Label of the builder or host that runs it.
    pub host: String,
    pub proxy: Option<String>,
    pub requester: Option<String>,
    pub recipe_rev: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Only these steps.
    pub only: Option<Vec<String>>,
    pub keep_going: bool,
    /// Environment of every step, before the recipe's (the caller's build variables).
    pub env: Vec<(String, String)>,
    /// Where the step logs and artifacts go (a local directory).
    pub log_dir: PathBuf,
}

/// A summary with no steps yet.
pub fn start(meta: &Meta, exec: &dyn Executor, log_dir: &Path, status: BuildStatus) -> Summary {
    Summary {
        v: BUILD_VERSION,
        id: meta.id.clone(),
        repo: meta.repo.clone(),
        head: meta.head.clone(),
        base: meta.base.clone(),
        dirty: meta.dirty,
        host: meta.host.clone(),
        platform: exec.platform(),
        executor: exec.kind().to_string(),
        proxy: meta.proxy.clone(),
        requester: meta.requester.clone(),
        status,
        reason: None,
        recipe_rev: meta.recipe_rev,
        started_at: utc_now_stamp(),
        ended_at: None,
        wall_ms: 0,
        log_dir: log_dir.display().to_string(),
        steps: Vec::new(),
        errors: Vec::new(),
        errors_total: 0,
        tests: Tests::default(),
        warnings_total: 0,
        warnings: BTreeMap::new(),
    }
}

/// A finished summary of a build that did not run.
pub fn not_run(mut s: Summary, reason: impl Into<String>) -> Summary {
    s.status = BuildStatus::NotRun;
    s.reason = Some(reason.into());
    s.ended_at = Some(utc_now_stamp());
    s
}

/// How [`drive`] ended.
pub struct Driven {
    pub summary: Summary,
    /// The remote dropped during a step; the summary is partial and the build is to be
    /// retried, not reported as failed.
    pub unreachable: bool,
}

/// The variables every step sees: `HGL_AI_BUILD_ID`, `HGL_AI_SRC`, `HGL_AI_BUILD_DIR`,
/// and for a job's own checkout `CARGO_TARGET_DIR` and `TMPDIR` inside it.
pub fn build_env(id: &str, ws: &Workspace) -> Vec<(String, String)> {
    let mut env = vec![
        ("HGL_AI_BUILD_ID".to_string(), id.to_string()),
        ("HGL_AI_SRC".into(), ws.src.clone()),
        ("HGL_AI_BUILD_DIR".into(), ws.build.clone()),
    ];
    if ws.job.is_some() {
        env.push(("CARGO_TARGET_DIR".into(), format!("{}/target", ws.build)));
        env.push(("TMPDIR".into(), ws.tmp.clone()));
    }
    env
}

fn read_log(p: &Path) -> Vec<u8> {
    use std::io::Read;
    let mut b = Vec::new();
    if let Ok(f) = std::fs::File::open(p) {
        let _ = f.take(PARSE_LIMIT).read_to_end(&mut b);
    }
    b
}

pub fn drive(
    exec: &dyn Executor,
    ws: &Workspace,
    recipe: &Recipe,
    meta: &Meta,
    opts: &Options,
    cancel: &Cancel,
    progress: &mut dyn FnMut(&Summary),
) -> Driven {
    let started = Instant::now();
    let log_dir = opts.log_dir.as_path();
    let mut s = start(meta, exec, log_dir, BuildStatus::Running);
    let platform = exec.platform();
    let plan = match recipe.plan(platform, opts.only.as_deref()) {
        Ok(p) if p.is_empty() => {
            return Driven {
                summary: not_run(
                    s,
                    format!("the recipe of {} has no step for {platform}", recipe.repo),
                ),
                unreachable: false,
            };
        }
        Ok(p) => p,
        Err(e) => {
            return Driven {
                summary: not_run(s, e.to_string()),
                unreachable: false,
            };
        }
    };
    if let Err(e) = std::fs::create_dir_all(log_dir) {
        return Driven {
            summary: not_run(s, format!("{}: {e}", log_dir.display())),
            unreachable: false,
        };
    }
    let mut collect = Collect::default();
    let mut stop_rest: Option<String> = None;
    for step in plan {
        let mut st = StepSummary::new(&step.name, StepStatus::Running);
        if cancel.is_cancelled() {
            st.status = StepStatus::Stopped;
            st.reason = Some("stopped before it started".into());
            s.steps.push(st);
            continue;
        }
        if let Some(why) = &stop_rest {
            st.status = StepStatus::Skipped;
            st.reason = Some(why.clone());
            s.steps.push(st);
            continue;
        }
        // Needs.
        let mut env = opts.env.clone();
        let mut missing = None;
        for need in step.needs() {
            match &need {
                Need::Step(dep) => {
                    if let Some(d) = s.steps.iter().find(|x| &x.name == dep)
                        && d.status != StepStatus::Pass
                    {
                        st.status = StepStatus::Skipped;
                        st.reason = Some(format!("needs step {dep}, which did not pass"));
                    }
                }
                Need::Sdk(n) => match exec.sdk(n) {
                    Some(p) => env.push((sdk_var(n), p)),
                    None => {
                        missing = Some(format!(
                            "needs {need}: set its prefix in [builder.sdk] of this host"
                        ))
                    }
                },
                Need::Tool(t) => match exec.has_tool(t) {
                    Ok(true) => {}
                    Ok(false) => missing = Some(format!("needs {need}: not on PATH")),
                    Err(crate::exec::HostError::Unreachable(_)) => {
                        s.steps.push(st);
                        return Driven {
                            summary: s,
                            unreachable: true,
                        };
                    }
                    Err(crate::exec::HostError::Failed(e)) => {
                        missing = Some(format!("needs {need}: {e}"))
                    }
                },
            }
            if missing.is_some() {
                break;
            }
        }
        if let Some(why) = missing {
            st.status = StepStatus::NotRun;
            st.reason = Some(why);
            s.steps.push(st);
            if !opts.keep_going {
                stop_rest = Some(format!("{} did not run", step.name));
            }
            continue;
        }
        if st.status == StepStatus::Skipped {
            s.steps.push(st);
            continue;
        }
        for (k, v) in recipe.env.iter().chain(&step.env) {
            let v = expand_vars(v, &env);
            env.push((k.clone(), v));
        }
        let log = log_dir.join(format!("{}.log", step.name));
        st.log = Some(log.display().to_string());
        s.steps.push(st.clone());
        progress(&s);
        s.steps.pop();
        let spec = StepSpec {
            name: step.name.clone(),
            script: step
                .run_for(platform)
                .expect("planned steps run here")
                .to_string(),
            cwd: step.cwd.clone(),
            env: env.clone(),
            mem_bytes: step.memcap_bytes(),
            time: step.time_limit(),
            address_limit: step.address_limit.unwrap_or(true),
        };
        let r = exec.run_step(ws, &spec, &log, cancel);
        if r.unreachable {
            st.status = StepStatus::Stopped;
            st.reason = Some("the remote dropped".into());
            s.steps.push(st);
            return Driven {
                summary: s,
                unreachable: true,
            };
        }
        st.exit = r.exit;
        st.wall_ms = r.wall.as_millis() as u64;
        st.cpu_ms = r.cpu.map(|d| d.as_millis() as u64);
        st.peak_bytes = r.peak;
        st.cap_bytes = r.cap;
        st.limit = r.limit.map(String::from);
        st.log_bytes = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
        st.status = if r.stopped {
            StepStatus::Stopped
        } else if r.error.is_none() && r.exit == Some(0) && r.limit.is_none() {
            StepStatus::Pass
        } else {
            StepStatus::Fail
        };
        st.reason = match (&r.error, r.limit) {
            (Some(e), _) => Some(e.clone()),
            (None, Some("memory")) => Some(format!(
                "reached its {} memory cap",
                hgl_ai_cap::format_size(spec.mem_bytes)
            )),
            (None, Some(_)) => Some(format!(
                "reached its {} time limit",
                hgl_ai_cap::format_duration(spec.time)
            )),
            _ => None,
        };
        // Artifacts, then the parse (which may read XML reports among them).
        let mut reports: Vec<(String, Vec<u8>)> = Vec::new();
        if !step.artifacts.is_empty() {
            let dest = log_dir.join("artifacts").join(&step.name);
            for p in exec.collect(ws, &step.cwd, &step.artifacts, &env, &dest) {
                let rel = p
                    .strip_prefix(log_dir)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| p.display().to_string());
                if rel.ends_with(".xml") {
                    reports.push((rel.clone(), read_log(&p)));
                }
                st.artifacts.push(rel);
            }
        }
        let bytes = read_log(&log);
        let parsed = parse::parse(
            step.parser,
            &bytes,
            step.patterns.as_ref(),
            &Reports {
                files: reports
                    .iter()
                    .map(|(n, b)| (n.clone(), b.as_slice()))
                    .collect(),
            },
        );
        st.cache = parsed.cache.map(String::from);
        let (e, w) = collect.add(&step.name, parsed);
        st.errors = e;
        st.warnings = w;
        let failed = st.status != StepStatus::Pass;
        let name = st.name.clone();
        s.steps.push(st);
        collect.fill(&mut s);
        s.wall_ms = started.elapsed().as_millis() as u64;
        if failed && !opts.keep_going {
            stop_rest = Some(format!("{name} did not pass"));
        }
        progress(&s);
    }
    collect.fill(&mut s);
    s.status = overall(&s.steps);
    s.ended_at = Some(utc_now_stamp());
    s.wall_ms = started.elapsed().as_millis() as u64;
    Driven {
        summary: s,
        unreachable: false,
    }
}
