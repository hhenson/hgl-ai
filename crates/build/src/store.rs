//! Recipes and run summaries on the hub (`HGL_BUILD`), the files a run leaves in its log
//! directory, and build receipts.

use std::path::Path;

use hgl_ai_bus::{Bus, BusError};
use hgl_ai_proto::build::{Summary, recipe_key, run_key};
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
