//! Sending a `build` job to a builder (BUILD-6): choosing a builder of the platform, the
//! queued summary on the hub, and the note. `build run --platform` and the merge gate's
//! gate builds (BUILD-15) send through here.

use hgl_ai_bus::Bus;
use hgl_ai_proto::build::{
    BUILD_VERSION, BuildJob, BuildStatus, BuilderRecord, Platform, Summary, Tests,
};
use hgl_ai_proto::{Kind, Note, Seat, utc_now_stamp};

use crate::store::{self, StoreError};

/// The builder of `platform` to send to: `label` if given, else a ready one (the least
/// busy), else a reachable one, else any active one (the job waits for it).
pub fn choose(
    builders: &[BuilderRecord],
    platform: Platform,
    label: Option<&str>,
) -> Option<BuilderRecord> {
    let mut c: Vec<&BuilderRecord> = builders
        .iter()
        .filter(|b| b.platform == platform && b.state != "retired")
        .filter(|b| label.is_none_or(|l| b.label == l))
        .collect();
    if label.is_some() {
        return c.first().map(|b| (*b).clone());
    }
    c.retain(|b| b.state == "active");
    c.sort_by_key(|b| {
        (
            !b.ready(),
            !b.reachable,
            b.busy * 1000 / b.slots.max(1),
            b.label.clone(),
        )
    });
    c.first().map(|b| (*b).clone())
}

/// The summary of a job sent and not started, as `build status` shows it until the builder
/// takes it.
pub fn queued(job: &BuildJob, b: &BuilderRecord) -> Summary {
    Summary {
        v: BUILD_VERSION,
        id: job.id.clone(),
        repo: job.repo.clone(),
        head: Some(job.head.clone()),
        base: job.base.clone(),
        base_sha: None,
        dirty: false,
        host: b.label.clone(),
        platform: b.platform,
        executor: b.executor.clone(),
        proxy: b.proxy.clone(),
        requester: Some(job.requester.clone()),
        status: BuildStatus::Queued,
        reason: None,
        recipe_rev: None,
        started_at: utc_now_stamp(),
        ended_at: None,
        wall_ms: 0,
        log_dir: String::new(),
        steps: Vec::new(),
        errors: Vec::new(),
        errors_total: 0,
        tests: Tests::default(),
        warnings_total: 0,
        warnings: Default::default(),
    }
}

/// Keep the queued summary and send `job` from `sender` to builder `b`.
pub async fn send(
    bus: &Bus,
    sender: Seat,
    b: &BuilderRecord,
    job: &BuildJob,
) -> Result<(), StoreError> {
    send_at(bus, sender, b, job, None).await
}

/// [`send`] with the note's `created_at` given: sent again with the same one, the note is
/// the same message (PROTO-9) and the hub drops the copy, so a requester that does not
/// know whether its send went out may send again.
pub async fn send_at(
    bus: &Bus,
    sender: Seat,
    b: &BuilderRecord,
    job: &BuildJob,
    created_at: Option<&str>,
) -> Result<(), StoreError> {
    // Created only if absent: sent again, it never sets back a summary the builder has
    // moved on (running, or final).
    store::run_create(bus, &queued(job, b)).await?;
    let body = serde_json::to_string(job).map_err(|e| StoreError::Other(e.to_string()))?;
    let mut n = Note::new(sender, b.inbox.clone(), Kind::Build, body);
    if let Some(at) = created_at {
        n.created_at = at.to_string();
    }
    n.repo = Some(job.repo.clone());
    n.head = Some(job.head.clone());
    n.base = job.base.clone();
    n.issue = job.issue;
    bus.send(&n).await?;
    Ok(())
}
