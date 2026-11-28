//! Recipes and run summaries on the hub (`HGL_BUILD`), the files a run leaves in its log
//! directory, and build receipts.

use std::path::Path;

use hgl_ai_bus::{Bus, BusError};
use hgl_ai_proto::build::{
    BuildJob, Summary, VerdictValue, VerdictWord, recipe_key, run_key, verdict_key, verdict_prefix,
};
use hgl_ai_proto::{Kind, Note, Seat};

use crate::recipe::{Recipe, RecipeError};
use crate::summary::bounded;

/// Run summaries kept on the hub (BUILD-7); the oldest go first.
pub const RUN_RETENTION: usize = 1000;
/// Largest receipt body; a larger summary is sent bounded.
const RECEIPT_LIMIT: usize = 512 << 10;
pub const STATUS_FILE: &str = "status.json";
pub const SUMMARY_FILE: &str = "summary.json";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Bus(#[from] BusError),
    #[error(transparent)]
    Recipe(#[from] RecipeError),
    #[error("{0}")]
    Other(String),
}

/// A recipe as stored: its text (comments kept), parsed, and its revision.
#[derive(Debug, Clone)]
pub struct Stored {
    pub text: String,
    pub recipe: Recipe,
    pub revision: u64,
    pub created: i64,
}

/// The recipe of `repo`, the latest or revision `rev`.
pub async fn recipe_get(
    bus: &Bus,
    repo: &str,
    rev: Option<u64>,
) -> Result<Option<Stored>, StoreError> {
    let key = recipe_key(repo);
    let e = match rev {
        None => bus.build_get(&key).await?,
        Some(r) => bus.build_get_revision(&key, r).await?,
    };
    let Some(e) = e else { return Ok(None) };
    let text =
        String::from_utf8(e.value).map_err(|_| StoreError::Other(format!("{key} is not UTF-8")))?;
    let recipe = Recipe::parse(&text)?;
    Ok(Some(Stored {
        text,
        recipe,
        revision: e.revision,
        created: e.created,
    }))
}

/// Validate and store `text` as the recipe of the repository it names (or `repo`, which
/// must agree). Returns the repository and the new revision.
pub async fn recipe_put(
    bus: &Bus,
    text: &str,
    repo: Option<&str>,
) -> Result<(String, u64), StoreError> {
    let r = Recipe::parse(text)?;
    if let Some(want) = repo
        && want != r.repo
    {
        return Err(StoreError::Other(format!(
            "the recipe is for {}, not {want}",
            r.repo
        )));
    }
    let rev = bus.build_put(&recipe_key(&r.repo), text.as_bytes()).await?;
    Ok((r.repo, rev))
}

/// Revisions of a recipe kept on the hub: (revision, unix time).
pub async fn recipe_history(bus: &Bus, repo: &str) -> Result<Vec<(u64, i64)>, StoreError> {
    Ok(bus
        .build_history(&recipe_key(repo))
        .await?
        .into_iter()
        .map(|e| (e.revision, e.created))
        .collect())
}

/// Put `run.<id>` (the bounded copy); a final one also trims to [`RUN_RETENTION`].
pub async fn run_put(bus: &Bus, s: &Summary) -> Result<(), StoreError> {
    let b = bounded(s);
    let v = serde_json::to_vec(&b).map_err(|e| StoreError::Other(e.to_string()))?;
    bus.build_put(&run_key(&s.id), &v).await?;
    if s.status.is_final() {
        trim(bus, RUN_RETENTION).await?;
    }
    Ok(())
}

/// Write a gate build's verdict (BUILD-15) from its summary, or `running` (bounded by
/// `until`) while it builds, in the key of what it judges: its head, the `main` commit it
/// was asked to merge onto, and the recipe revision it runs ([`verdict_key`]). In that key
/// `pass` and `fail` are final: a later build of the same pair replaces only `running` or
/// `not-run` (compare-and-set on the revision); `Ok(false)` when the key was final. Nothing
/// for a job that is no gate build, or a build that never read its recipe (the gate reads
/// why from the run).
pub async fn verdict_put(
    bus: &Bus,
    job: &BuildJob,
    s: &Summary,
    running_until: Option<String>,
) -> Result<bool, StoreError> {
    let (Some(name), Some(base), Some(recipe)) = (&job.verdict, &job.base, s.recipe_rev) else {
        return Ok(true);
    };
    if s.base_sha.as_ref().is_some_and(|m| m != base) {
        return Err(StoreError::Other(format!(
            "{}: merged onto {} for a verdict on {base}; not written",
            s.id,
            s.base_sha.as_deref().unwrap_or_default()
        )));
    }
    let mut v = match running_until {
        Some(until) => VerdictValue {
            head: job.head.clone(),
            main: base.clone(),
            word: VerdictWord::Running,
            id: Some(s.id.clone()),
            builder: Some(s.host.clone()),
            recipe: Some(recipe),
            until: Some(until),
            reason: None,
        },
        None => match VerdictValue::of(s) {
            Some(v) => v,
            None => return Ok(true),
        },
    };
    // A build that stopped before its merge still judged nothing about this pair: say so
    // in its key.
    v.main = base.clone();
    let key = verdict_key(&job.repo, job.issue, name, &job.head, base, recipe);
    let text = v.render();
    // Legacy writers arbitrate through HGL_STATE. If the new bucket already has a
    // final value, use that value instead of introducing a contradictory legacy one.
    let mut proposed = false;
    for _ in 0..16 {
        let new = bus.verdict_get(&key).await?;
        let old = bus.state_get(&key).await?;
        let final_value = |value: &str| {
            VerdictValue::parse(value).is_some_and(|v| v.judged() && v.is_for(&job.head, base))
        };
        let legacy_final = old.as_ref().is_some_and(|e| final_value(&e.value));
        let new_final = new.as_ref().is_some_and(|e| final_value(&e.value));
        if legacy_final && new_final {
            // Existing conflicting final values cannot safely be rewritten.
            return Ok(old.as_ref().is_some_and(|e| e.value == text)
                && new.as_ref().is_some_and(|e| e.value == text));
        }
        let desired = if legacy_final {
            &old.as_ref().expect("legacy final").value
        } else if new_final {
            &new.as_ref().expect("new final").value
        } else if proposed {
            match &old {
                Some(e) => &e.value,
                None => return Ok(false),
            }
        } else {
            &text
        };
        if old.as_ref().is_none_or(|e| e.value != *desired) {
            match bus
                .state_update(&key, desired, old.as_ref().map_or(0, |e| e.revision))
                .await
            {
                Ok(_) => {
                    proposed = true;
                    continue;
                }
                Err(BusError::Conflict { .. }) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        if new.as_ref().is_some_and(|e| e.value == *desired) {
            return Ok(*desired == text);
        }
        // Do not mirror a stale non-final value over a newer value. A concurrent
        // mirror after this check changes the new bucket revision and fails our CAS.
        let latest = bus.state_get(&key).await?;
        if latest.as_ref().map(|e| e.revision) != old.as_ref().map(|e| e.revision) {
            continue;
        }
        match bus
            .verdict_update(&key, desired, new.as_ref().map_or(0, |e| e.revision))
            .await
        {
            Ok(_) => return Ok(*desired == text),
            Err(BusError::Conflict { .. }) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(StoreError::Other(format!(
        "{key} kept changing; not mirrored"
    )))
}

/// Whether pull request `repo#pr` is merged or closed, as GitHub says (BUILD-15): `Err`
/// when that could not be told (GitHub unreachable, a `hold`, no such pull request).
/// Verdict keys are purged on this fact only, never on a note's word.
pub type PrEnded = std::sync::Arc<
    dyn Fn(
            String,
            u64,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send>>
        + Send
        + Sync,
>;

/// The pull request a verdict key belongs to: `verdict.<repo>.<pr>.<name>.<pair>` read from
/// the right, since a repository's name may hold dots.
pub fn verdict_pr(key: &str) -> Option<(String, u64)> {
    let rest = key.strip_prefix("verdict.")?;
    let mut w = rest.rsplitn(4, '.');
    let (_pair, _name, pr, repo) = (w.next()?, w.next()?, w.next()?, w.next()?);
    Some((repo.to_string(), pr.parse().ok()?))
}

/// Delete every verdict key of `repo#pr`, if GitHub says it is merged or closed. Returns
/// how many went; `Ok(None)` when it is open or that could not be told (nothing went).
pub async fn verdicts_purge(
    bus: &Bus,
    ended: &PrEnded,
    repo: &str,
    pr: u64,
) -> Result<Option<usize>, StoreError> {
    match ended(repo.to_string(), pr).await {
        Ok(true) => {}
        Ok(false) => return Ok(None),
        Err(e) => {
            tracing::info!("{repo} #{pr}: its state not read ({e}); its verdicts kept");
            return Ok(None);
        }
    }
    let keys = bus.state_keys(&verdict_prefix(repo, pr)).await?;
    let new_keys = bus.verdict_keys(&verdict_prefix(repo, pr)).await?;
    for k in &keys {
        bus.state_del(k).await?;
    }
    for k in &new_keys {
        bus.verdict_del(k).await?;
    }
    Ok(Some(keys.len() + new_keys.len()))
}

/// Sweep the verdict keys: those the hub wrote more than `max_age` seconds before `now`
/// (Unix seconds, the hub's clock; never a requester's build id), and, with `ended`, those
/// of pull requests GitHub reports merged or closed. Returns how many went.
pub async fn verdicts_sweep(
    bus: &Bus,
    now: i64,
    max_age: i64,
    ended: Option<&PrEnded>,
) -> Result<usize, StoreError> {
    let mut n = 0;
    let mut prs = std::collections::BTreeSet::new();
    for k in bus.state_keys("verdict.").await? {
        let Some(e) = bus.state_get(&k).await? else {
            continue;
        };
        if now - e.created > max_age {
            bus.state_del(&k).await?;
            n += 1;
        } else if let Some(p) = verdict_pr(&k) {
            prs.insert(p);
        }
    }
    for k in bus.verdict_keys("verdict.").await? {
        let Some(e) = bus.verdict_get(&k).await? else {
            continue;
        };
        if now - e.created > max_age {
            bus.verdict_del(&k).await?;
            n += 1;
        } else if let Some(p) = verdict_pr(&k) {
            prs.insert(p);
        }
    }
    if let Some(ended) = ended {
        for (repo, pr) in prs {
            n += verdicts_purge(bus, ended, &repo, pr).await?.unwrap_or(0);
        }
    }
    Ok(n)
}

/// Whether this connection's hub user may write the local build verdicts (BUILD-15), as
/// the hub's permissions say, not a credential file (a bare NKey seed names no role): a
/// probe key `verdict._probe.0.<host>`, no repository's, written and deleted.
pub async fn may_write_verdicts(bus: &Bus, host: &str) -> Result<bool, StoreError> {
    let key = format!("verdict._probe.0.{host}");
    match bus.verdict_put(&key, "probe").await {
        Ok(_) => bus.verdict_del(&key).await?,
        Err(BusError::Denied { .. }) => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    match bus.state_put(&key, "probe").await {
        Ok(_) => {
            bus.state_del(&key).await?;
            Ok(true)
        }
        Err(BusError::Denied { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Delete the oldest runs beyond `keep`. Returns how many went.
pub async fn trim(bus: &Bus, keep: usize) -> Result<usize, StoreError> {
    let keys = bus.build_keys("run.").await?;
    let n = keys.len().saturating_sub(keep);
    for k in &keys[..n] {
        bus.build_del(k).await?;
    }
    Ok(n)
}

pub async fn run_get(bus: &Bus, id: &str) -> Result<Option<Summary>, StoreError> {
    let Some(e) = bus.build_get(&run_key(id)).await? else {
        return Ok(None);
    };
    serde_json::from_slice(&e.value)
        .map(Some)
        .map_err(|e| StoreError::Other(format!("run.{id}: {e}")))
}

/// Recent runs, newest first: at most `limit`, of `repo` if given, failed ones only if
/// `failed` (fail and not-run).
pub async fn runs(
    bus: &Bus,
    repo: Option<&str>,
    failed: bool,
    limit: usize,
) -> Result<Vec<Summary>, StoreError> {
    let mut keys = bus.build_keys("run.").await?;
    keys.reverse();
    let mut out = Vec::new();
    for k in keys {
        if out.len() >= limit {
            break;
        }
        let Some(e) = bus.build_get(&k).await? else {
            continue;
        };
        let Ok(s) = serde_json::from_slice::<Summary>(&e.value) else {
            continue;
        };
        if repo.is_some_and(|r| r != s.repo) {
            continue;
        }
        if failed && !matches!(s.status.as_str(), "fail" | "not-run") {
            continue;
        }
        out.push(s);
    }
    Ok(out)
}

fn write_atomic(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, p)
}

/// `<log_dir>/status.json`, rewritten as the run goes.
pub fn write_status(log_dir: &Path, s: &Summary) -> std::io::Result<()> {
    let v = serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?;
    write_atomic(&log_dir.join(STATUS_FILE), &v)
}

/// `<log_dir>/summary.json` and the final `status.json`.
pub fn write_final(log_dir: &Path, s: &Summary) -> std::io::Result<()> {
    let v = serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?;
    write_atomic(&log_dir.join(SUMMARY_FILE), &v)?;
    write_atomic(&log_dir.join(STATUS_FILE), &v)
}

pub fn read_status(log_dir: &Path) -> Option<Summary> {
    let b = std::fs::read(log_dir.join(STATUS_FILE)).ok()?;
    serde_json::from_slice(&b).ok()
}

/// The body of a build receipt: the summary as JSON, bounded when large.
pub fn receipt_body(s: &Summary) -> String {
    let full = serde_json::to_string(s).unwrap_or_default();
    if full.len() <= RECEIPT_LIMIT {
        full
    } else {
        serde_json::to_string(&bounded(s)).unwrap_or_default()
    }
}

/// Send the receipt of `s` from `from` to `to`. A summary without a full head cannot be
/// a receipt (PROTO-10) and is not sent.
pub async fn send_receipt(
    bus: &Bus,
    from: &Seat,
    to: &Seat,
    s: &Summary,
    issue: u64,
) -> Result<(), StoreError> {
    let Some(head) = s
        .head
        .clone()
        .filter(|h| hgl_ai_proto::check_head(h).is_ok())
    else {
        return Err(StoreError::Other(format!(
            "build {} has no commit to name; no receipt sent",
            s.id
        )));
    };
    let mut n = Note::new(from.clone(), to.clone(), Kind::Receipt, receipt_body(s));
    n.repo = Some(s.repo.clone());
    n.head = Some(head);
    n.base = s.base.clone();
    n.issue = issue;
    bus.send(&n).await?;
    Ok(())
}

/// A build summary from a receipt's body.
/// The summary of build `id` from its receipt: a `receipt` from a builder
/// (`builder-*`) whose summary is that build's. Anything else on the reply address is not
/// the build's result.
pub fn parse_receipt(n: &Note, id: &str) -> Result<Summary, StoreError> {
    if n.kind != Kind::Receipt || !n.sender.as_str().starts_with("builder-") {
        return Err(StoreError::Other(format!(
            "a {} from {} is not a builder's receipt",
            n.kind, n.sender
        )));
    }
    let s: Summary = serde_json::from_str(n.body.trim())
        .map_err(|e| StoreError::Other(format!("a build receipt that is not a summary: {e}")))?;
    if s.id != id {
        return Err(StoreError::Other(format!(
            "a receipt of build {}, not {id}",
            s.id
        )));
    }
    Ok(s)
}
