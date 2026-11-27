//! The usage collector (docs/protocol.md PROTO-21, PROTO-22): read each signed-in CLI of
//! this host's agent user and make one [`UsageRecord`] per login.
//!
//! | Runtime | Limits | Account id (fingerprinted, never published) |
//! | --- | --- | --- |
//! | claude | `claude -p "/usage"` | `orgId/email` of `claude auth status` |
//! | codex | the last `rate_limits` of the newest `~/.codex/sessions/**/rollout-*.jsonl` | `tokens.account_id` of `~/.codex/auth.json` |
//! | agy | `agy -p "/usage"` | the `applyAuthResult: email=` line of its newest log |
//!
//! Probing costs no model turn: slash commands only. A source that fails or does not parse
//! skips its runtime or keeps the raw lines; it never fails the collection. No token is
//! read into a kept value: the account files are parsed into the id fields alone.

pub mod parse;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use hgl_ai_proto::{Runtime, UsageRecord, account_fingerprint, parse_stamp, utc_stamp};

use crate::parse::Parsed;

/// Codex rollouts read at most, newest first, for the account's newest rate limits.
const ROLLOUTS_TRIED: usize = 200;
/// A rollout last written this long before the newest observation found cannot hold a
/// newer one: the search stops there.
const ROLLOUT_MTIME_SLACK_S: i64 = 300;
/// The tail of a rollout read first; doubled, up to [`MAX_FILE_BYTES`], while it holds no
/// rate limits.
const ROLLOUT_TAIL_BYTES: u64 = 8 << 20;
/// The most of a rollout's first line (its `session_meta`, naming the account) read.
const ROLLOUT_HEAD_BYTES: u64 = 1 << 20;
/// agy logs read, newest first, looking for its sign-in line.
const AGY_LOGS_TRIED: usize = 8;
/// The most of a file read: account files, a rollout, a log.
const MAX_FILE_BYTES: u64 = 64 << 20;

/// Where the sources are; [`Sources::from_env`] gives the agent user's.
#[derive(Debug, Clone)]
pub struct Sources {
    pub claude: PathBuf,
    pub agy: PathBuf,
    pub codex_home: Option<PathBuf>,
    pub agy_home: Option<PathBuf>,
    /// Each CLI call's limit.
    pub timeout: Duration,
}

impl Sources {
    /// `claude` and `agy` on `PATH` (or `HGL_AI_USAGE_CLAUDE`, `HGL_AI_USAGE_AGY`),
    /// `$CODEX_HOME` or `~/.codex`, and `HGL_AI_USAGE_AGY_HOME` or
    /// `~/.gemini/antigravity-cli`.
    pub fn from_env() -> Sources {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        let home = var("HOME").map(PathBuf::from);
        Sources {
            claude: var("HGL_AI_USAGE_CLAUDE").map_or_else(|| "claude".into(), PathBuf::from),
            agy: var("HGL_AI_USAGE_AGY").map_or_else(|| "agy".into(), PathBuf::from),
            codex_home: var("CODEX_HOME")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|h| h.join(".codex"))),
            agy_home: var("HGL_AI_USAGE_AGY_HOME")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|h| h.join(".gemini/antigravity-cli"))),
            timeout: Duration::from_secs(120),
        }
    }
}

/// One runtime's outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum Collected {
    Record(Box<UsageRecord>),
    Skipped { runtime: Runtime, why: String },
}

/// Collect `runtimes` (every runtime when empty), concurrently.
pub async fn collect(src: &Sources, runtimes: &[Runtime], host: &str) -> Vec<Collected> {
    let want = |r: Runtime| runtimes.is_empty() || runtimes.contains(&r);
    let one = |r: Runtime| async move {
        if !want(r) {
            return None;
        }
        let got = match r {
            Runtime::Claude => claude(src, host).await,
            Runtime::Codex => codex(src, host).await,
            Runtime::Agy => agy(src, host).await,
        };
        Some(got.map_or_else(
            |why| Collected::Skipped { runtime: r, why },
            |r| Collected::Record(Box::new(r)),
        ))
    };
    let (c, x, a) = tokio::join!(one(Runtime::Claude), one(Runtime::Codex), one(Runtime::Agy));
    [c, x, a].into_iter().flatten().collect()
}

fn record(
    runtime: Runtime,
    id: &str,
    host: &str,
    plan: Option<String>,
    observed_at: DateTime<Utc>,
    parsed: Parsed,
    limit_reached: Option<String>,
) -> UsageRecord {
    let collected_at = Utc::now().max(observed_at);
    UsageRecord {
        runtime,
        account: account_fingerprint(id),
        label: None,
        host: host.to_string(),
        plan,
        observed_at: utc_stamp(observed_at),
        collected_at: utc_stamp(collected_at),
        source_age_s: u64::try_from((collected_at - observed_at).num_seconds()).unwrap_or(0),
        buckets: parsed.buckets,
        limit_reached,
        unparsed: parsed.unparsed,
    }
}

async fn claude(src: &Sources, host: &str) -> Result<UsageRecord, String> {
    let login = parse::claude_login(&run(&src.claude, &["auth", "status"], src.timeout).await?)?;
    let text = run(&src.claude, &["-p", "/usage"], src.timeout).await?;
    let now = Utc::now();
    Ok(record(
        Runtime::Claude,
        &login.id,
        host,
        login.plan,
        now,
        parse::claude_usage(&text, now),
        None,
    ))
}

async fn agy(src: &Sources, host: &str) -> Result<UsageRecord, String> {
    let text = run(&src.agy, &["-p", "/usage"], src.timeout).await?;
    let now = Utc::now();
    let home = src.agy_home.clone().ok_or("no home for agy's logs")?;
    let id = blocking(move || agy_login(&home)).await?;
    Ok(record(
        Runtime::Agy,
        &id,
        host,
        None,
        now,
        parse::agy_usage(&text),
        None,
    ))
}

fn agy_login(home: &Path) -> Result<String, String> {
    let dir = home.join("log");
    let logs = newest(&dir, |n| n.starts_with("cli-") && n.ends_with(".log"))
        .map_err(|e| format!("agy's logs at {}: {e}", dir.display()))?;
    logs.iter()
        .take(AGY_LOGS_TRIED)
        .find_map(|p| read_bounded(p).ok().and_then(|t| parse::agy_account(&t)))
        .ok_or_else(|| "agy's newest logs name no signed-in account".to_string())
}

async fn codex(src: &Sources, host: &str) -> Result<UsageRecord, String> {
    let home = src.codex_home.clone().ok_or("no codex home")?;
    let host = host.to_string();
    blocking(move || {
        let auth = home.join("auth.json");
        let id = parse::codex_account(
            &read_bounded(&auth).map_err(|e| format!("{}: {e}", auth.display()))?,
        )?;
        let limits = codex_limits(&home.join("sessions"), &id)?;
        Ok(record(
            Runtime::Codex,
            &id,
            &host,
            limits.plan,
            limits.observed_at,
            limits.parsed,
            limits.limit_reached,
        ))
    })
    .await
}

/// The account's newest rate limits: the latest `observed_at` over the recently written
/// rollouts not of another account (several seats may share `~/.codex/sessions`).
fn codex_limits(sessions: &Path, id: &str) -> Result<parse::CodexLimits, String> {
    codex_limits_in(sessions, id, ROLLOUT_TAIL_BYTES)
}

fn codex_limits_in(sessions: &Path, id: &str, tail: u64) -> Result<parse::CodexLimits, String> {
    let files = newest_tree(sessions, |n| {
        n.starts_with("rollout-") && n.ends_with(".jsonl")
    })
    .map_err(|e| format!("codex sessions at {}: {e}", sessions.display()))?;
    let mut best: Option<parse::CodexLimits> = None;
    for (written, p) in files.iter().take(ROLLOUTS_TRIED) {
        if let Some(b) = &best {
            let written = DateTime::<Utc>::from(*written);
            if (b.observed_at - written).num_seconds() > ROLLOUT_MTIME_SLACK_S {
                break;
            }
        }
        let Ok(r) = read_rollout(p, tail) else {
            continue;
        };
        if r.account_id.as_deref().is_some_and(|a| a != id) {
            continue;
        }
        if let Some(l) = r.limits
            && best.as_ref().is_none_or(|b| l.observed_at > b.observed_at)
        {
            best = Some(l);
        }
    }
    best.ok_or_else(|| "no recent Codex turn of this account recorded rate limits".into())
}

/// A rollout's account (its first line) and last rate limits (its tail, read further back
/// while it holds none): a long session is never read from the front only.
fn read_rollout(p: &Path, tail: u64) -> std::io::Result<parse::Rollout> {
    use std::io::{BufRead as _, Read as _, Seek as _, SeekFrom};
    let mut f = std::fs::File::open(p)?;
    let len = f.metadata()?.len();
    let mut head = Vec::new();
    std::io::BufReader::new((&mut f).take(ROLLOUT_HEAD_BYTES)).read_until(b'\n', &mut head)?;
    let account_id = parse::codex_rollout(&String::from_utf8_lossy(&head)).account_id;
    let mut want = tail.max(1);
    loop {
        let start = len.saturating_sub(want);
        f.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::new();
        (&mut f).take(want).read_to_end(&mut buf)?;
        let text = String::from_utf8_lossy(&buf);
        // A tail starting inside a line drops that partial line.
        let text = if start == 0 {
            &text[..]
        } else {
            text.split_once('\n').map_or("", |(_, rest)| rest)
        };
        let r = parse::codex_rollout(text);
        if r.limits.is_some() || start == 0 || want >= MAX_FILE_BYTES {
            return Ok(parse::Rollout {
                account_id: account_id.or(r.account_id),
                limits: r.limits,
            });
        }
        want = (want * 2).min(MAX_FILE_BYTES);
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("the collector task failed: {e}"))?
}

/// Run a CLI (stdin closed, in the home directory) and return its standard output.
async fn run(prog: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let shown = format!("{} {}", prog.display(), args.join(" "));
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(h) = std::env::var_os("HOME").filter(|h| Path::new(h).is_dir()) {
        cmd.current_dir(h);
    }
    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => return Err(format!("`{shown}` took over {timeout:?}")),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("{} is not installed", prog.display()));
        }
        Ok(Err(e)) => return Err(format!("`{shown}`: {e}")),
        Ok(Ok(o)) => o,
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(format!("`{shown}` failed ({}): {first}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn read_bounded(p: &Path) -> std::io::Result<String> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    std::fs::File::open(p)?
        .take(MAX_FILE_BYTES)
        .read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Files of `dir` whose names pass `keep`, newest first (by modification time).
fn newest(dir: &Path, keep: impl Fn(&str) -> bool) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let ok = e.file_name().to_str().is_some_and(&keep);
        if ok && e.file_type()?.is_file() {
            found.push((mtime(&e.path()), e.path()));
        }
    }
    Ok(sorted(found).into_iter().map(|(_, p)| p).collect())
}

/// [`newest`] over `dir` and its subdirectories (symbolic links not followed), with each
/// file's modification time.
fn newest_tree(
    dir: &Path,
    keep: impl Fn(&str) -> bool,
) -> std::io::Result<Vec<(SystemTime, PathBuf)>> {
    let mut found = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    std::fs::read_dir(dir)?;
    while let Some(d) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                todo.push(e.path());
            } else if t.is_file() && e.file_name().to_str().is_some_and(&keep) {
                found.push((mtime(&e.path()), e.path()));
            }
        }
    }
    Ok(sorted(found))
}

fn mtime(p: &Path) -> SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn sorted(mut v: Vec<(SystemTime, PathBuf)>) -> Vec<(SystemTime, PathBuf)> {
    v.sort_by(|a, b| b.cmp(a));
    v
}

// ---- usage show ---------------------------------------------------------------------

/// Hours from `now` to a bucket's reset, one decimal; negative once passed.
pub fn hours_to(resets_at: Option<&str>, now: DateTime<Utc>) -> Option<f64> {
    let secs = (parse_stamp(resets_at?)? - now).num_seconds();
    let h = (secs as f64 / 360.0).round() / 10.0;
    // Passed by under six minutes still reads as passed: never `-0.0` or `0.0`.
    Some(if secs <= 0 && h == 0.0 { -0.1 } else { h })
}

fn reset_cell(resets_at: Option<&str>, now: DateTime<Utc>) -> String {
    match hours_to(resets_at, now) {
        None => "-".to_string(),
        Some(h) if h < 0.0 => "passed".to_string(),
        Some(h) => format!("{h:.1}h"),
    }
}

fn age(secs: i64) -> String {
    match secs.max(0) {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// The `usage show` table: one row per bucket, then the records' notes.
pub fn table(records: &[UsageRecord], now: DateTime<Utc>) -> String {
    let head = [
        "RUNTIME", "ACCOUNT", "LABEL", "HOST", "PLAN", "SCOPE", "WINDOW", "USED", "RESET", "AGE",
    ];
    let mut rows: Vec<Vec<String>> = vec![head.iter().map(|s| s.to_string()).collect()];
    let mut notes = Vec::new();
    for r in records {
        let age = parse_stamp(&r.observed_at).map_or("?".into(), |t| age((now - t).num_seconds()));
        let base = |scope: String, window: String, used: String, reset: String| {
            vec![
                r.runtime.to_string(),
                r.account.clone(),
                r.label.clone().unwrap_or_else(|| "-".into()),
                r.host.clone(),
                r.plan.clone().unwrap_or_else(|| "-".into()),
                scope,
                window,
                used,
                reset,
                age.clone(),
            ]
        };
        if r.buckets.is_empty() {
            rows.push(base("-".into(), "-".into(), "-".into(), "-".into()));
        }
        for b in &r.buckets {
            let reset = reset_cell(b.resets_at.as_deref(), now);
            rows.push(base(
                b.scope.clone(),
                b.window.clone(),
                format!("{:.0}%", b.used_pct),
                reset,
            ));
        }
        let key = r.key();
        if let Some(l) = &r.limit_reached {
            notes.push(format!("{key}: limit reached: {l}"));
        }
        for u in &r.unparsed {
            notes.push(format!("{key}: unparsed: {u}"));
        }
    }
    let widths: Vec<usize> = (0..head.len())
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for r in &rows {
        let cells: Vec<String> = r
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    for n in notes {
        out.push_str(&n);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests;
