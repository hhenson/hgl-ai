//! The Homebrew tap.
//!
//! - `gh tap formula --version V --checksums DIR` renders `Formula/hgl-ai.rb` from
//!   `packaging/hgl-ai.rb.tmpl` (compiled in) with the four release archives' URLs and
//!   checksums; the release workflow runs it with the binary it just built.
//! - `gh tap release VERSION` is the bottle flow of the tap's main formula: after the tap
//!   pull request's commit is reviewed clean, run the source repository's bottle workflow
//!   for that commit, check the published bottle against the stanza it gave, put the stanza
//!   on the pull request's branch and ask for a review. Landing is then `gh land`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use base64::Engine as _;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::bot::{self, BotView, Comment, state_records};
use crate::exec::{self, Gh};
use crate::{Error, Result, Settings, invalid, io_err, short};

/// The release targets, as in the archive names `hgl-ai-X.Y.Z-<target>.tar.gz`.
pub const TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
];

pub const TEMPLATE: &str = include_str!("../../../packaging/hgl-ai.rb.tmpl");

/// `owner/name` of this repository, whose releases the formula installs.
pub fn this_repository() -> &'static str {
    env!("CARGO_PKG_REPOSITORY")
        .trim_start_matches("https://github.com/")
        .trim_end_matches('/')
}

static HEX64: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{64}$").unwrap());
static VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$").unwrap());

pub fn archive_name(version: &str, target: &str) -> String {
    format!("hgl-ai-{version}-{target}.tar.gz")
}

pub fn archive_url(repo: &str, version: &str, target: &str) -> String {
    format!(
        "https://github.com/{repo}/releases/download/v{version}/{}",
        archive_name(version, target)
    )
}

/// Checksums from `<dir>/hgl-ai-V-<target>.tar.gz.sha256` (`sha256sum` output).
pub fn read_checksums(dir: &Path, version: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for t in TARGETS {
        let p = dir.join(format!("{}.sha256", archive_name(version, t)));
        let text =
            std::fs::read_to_string(&p).map_err(io_err(format!("reading {}", p.display())))?;
        let sum = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if !HEX64.is_match(&sum) {
            return Err(invalid(format!("{}: not a SHA-256", p.display())));
        }
        out.insert(t.to_string(), sum);
    }
    Ok(out)
}

/// The formula for `version`, from `template`.
pub fn render_formula(
    template: &str,
    repo: &str,
    version: &str,
    sums: &BTreeMap<String, String>,
) -> Result<String> {
    let version = version.strip_prefix('v').unwrap_or(version);
    if !VERSION.is_match(version) {
        return Err(invalid(format!("version {version:?}: expected X.Y.Z")));
    }
    let mut s = template
        .replace("{{repo}}", repo)
        .replace("{{version}}", version);
    for t in TARGETS {
        let sum = sums
            .get(t)
            .ok_or_else(|| invalid(format!("no checksum for {t}")))?;
        s = s.replace(
            &format!("{{{{url:{t}}}}}", t = t),
            &archive_url(repo, version, t),
        );
        s = s.replace(&format!("{{{{sha256:{t}}}}}", t = t), sum);
    }
    if let Some(i) = s.find("{{") {
        let rest: String = s[i..].chars().take(40).collect();
        return Err(invalid(format!("template placeholder left: {rest}")));
    }
    Ok(s)
}

/// The bottle checksum for `tag` in the stanza `bottle.rb`.
pub fn bottle_sha(stanza: &str, tag: &str) -> Option<String> {
    let re = Regex::new(&format!(r#"{}:\s*"([0-9a-f]{{64}})""#, regex::escape(tag))).ok()?;
    re.captures(stanza).map(|m| m[1].to_string())
}

/// Whether the stanza's `root_url` is the release `version`.
pub fn root_url_ok(stanza: &str, version: &str) -> bool {
    stanza.contains(&format!("releases/download/{version}\""))
}

/// The formula with the stanza after `anchor` (which must occur once, in a formula that has
/// no bottle yet).
pub fn place_stanza(formula: &str, stanza: &str, anchor: &str) -> Result<String> {
    if formula.matches(anchor).count() != 1 {
        return Err(invalid(format!(
            "the formula does not hold {anchor:?} exactly once"
        )));
    }
    if formula.contains("bottle do") {
        return Err(invalid("the formula already has a bottle stanza"));
    }
    Ok(formula.replacen(
        anchor,
        &format!("{anchor}{}\n\n", stanza.trim_end_matches('\n')),
        1,
    ))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The bot's word on `sha`: `Some(true)` clean, `Some(false)` findings, `None` not yet.
pub fn review_of(comments: &[Comment], bot_login: &str, sha: &str) -> Option<bool> {
    let v = BotView::of(bot_login, comments, &[]);
    if v.clean_on(sha) {
        return Some(true);
    }
    let findings = v
        .comments
        .iter()
        .flat_map(|c| state_records(&c.body))
        .any(|r| r.commit_sha.as_deref() == Some(sha) && r.has_any_findings == Some(true));
    if findings { Some(false) } else { None }
}

#[derive(Debug, Clone)]
pub struct TapOpts {
    pub pr: Option<u64>,
    pub sha: Option<String>,
    pub skip_review_wait: bool,
    pub review_wait: Duration,
    pub run_timeout: Duration,
    pub trailer: Option<String>,
    pub dry_run: bool,
}

impl Default for TapOpts {
    fn default() -> Self {
        TapOpts {
            pr: None,
            sha: None,
            skip_review_wait: false,
            review_wait: Duration::from_secs(30 * 60),
            run_timeout: Duration::from_secs(90 * 60),
            trailer: None,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapOutcome {
    /// The stanza is on the branch and a review was asked for.
    Pushed(String),
    Would(String),
    /// Something is not as expected; nothing after it was done.
    Stop(String),
}

fn stop(msg: impl Into<String>) -> Result<TapOutcome> {
    Ok(TapOutcome::Stop(format!("STOP: {}", msg.into())))
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_file(&p, name) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n == name) {
            return Some(p);
        }
    }
    None
}

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// The bottle flow for `version` of the tap's formula.
pub async fn release<G: Gh>(
    gh: &G,
    s: &Settings,
    version: &str,
    o: &TapOpts,
) -> Result<TapOutcome> {
    let t = &s.tap;
    let tap = s.full(&t.repo);
    let source = s.full(&t.source_repo);
    let branch = format!("{}{}-{version}", s.branch_prefix, t.formula);
    let (number, head) = match o.pr {
        Some(n) => {
            let pr = exec::get(gh, &format!("repos/{tap}/pulls/{n}")).await?;
            (
                n,
                pr.pointer("/head/sha")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )
        }
        None => {
            let open = exec::get(
                gh,
                &format!("repos/{tap}/pulls?state=open&head={}:{branch}", s.owner),
            )
            .await?;
            let Some(pr) = open.as_array().and_then(|a| a.first()).cloned() else {
                return stop(format!("no open pull request on {tap} from {branch}"));
            };
            let n = pr.get("number").and_then(Value::as_u64).unwrap_or(0);
            (
                n,
                pr.pointer("/head/sha")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )
        }
    };
    let sha = o.sha.clone().unwrap_or(head);
    hgl_ai_proto::check_head(&sha)?;

    if !o.skip_review_wait {
        let deadline = Instant::now() + o.review_wait;
        loop {
            let list = exec::get_list(
                gh,
                &format!("repos/{tap}/issues/{number}/comments?per_page=100"),
            )
            .await?;
            let comments: Vec<Comment> = list.iter().map(Comment::from_rest).collect();
            match review_of(&comments, &s.review_bot, &sha) {
                Some(true) => break,
                Some(false) => return stop(format!("the bot has findings on {}", short(&sha))),
                None if o.dry_run || Instant::now() >= deadline => {
                    return stop(format!(
                        "no clean review of {} after {:?}",
                        short(&sha),
                        o.review_wait
                    ));
                }
                None => tokio::time::sleep(Duration::from_secs(60)).await,
            }
        }
    }
    let asset = format!("{}-{version}.{}.bottle.tar.gz", t.formula, t.bottle_tag);
    if o.dry_run {
        return Ok(TapOutcome::Would(format!(
            "would run {} on {source} for release {version} at tap commit {}, check {asset} against its stanza, put the stanza on {tap}#{number} and ask for a review",
            t.bottle_workflow,
            short(&sha)
        )));
    }

    // The bottle run.
    let started = chrono::Utc::now() - chrono::Duration::seconds(5);
    let dispatch = args(&[
        "workflow",
        "run",
        &t.bottle_workflow,
        "--repo",
        &source,
        "--ref",
        "main",
        "-f",
    ]);
    let mut dispatch = dispatch;
    dispatch.extend([
        format!("release_tag={version}"),
        "-f".into(),
        format!("tap_sha={sha}"),
        "-F".into(),
        "publish=true".into(),
    ]);
    if exec::text(gh, dispatch).await.is_err() {
        return stop("could not start the bottle workflow");
    }
    let runs_path = format!(
        "repos/{source}/actions/workflows/{}/runs?event=workflow_dispatch&per_page=10",
        t.bottle_workflow
    );
    let mut run_id = None;
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let v = exec::get(gh, &runs_path).await?;
        run_id = v
            .get("workflow_runs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find_map(|r| {
                let created = r
                    .get("created_at")
                    .and_then(Value::as_str)
                    .and_then(crate::table::parse_time)?;
                if created >= started {
                    r.get("id").and_then(Value::as_u64)
                } else {
                    None
                }
            });
        if run_id.is_some() {
            break;
        }
    }
    let Some(run_id) = run_id else {
        return stop("the bottle run did not appear");
    };
    let deadline = Instant::now() + o.run_timeout;
    let conclusion = loop {
        let r = exec::get(gh, &format!("repos/{source}/actions/runs/{run_id}")).await?;
        if r.get("status").and_then(Value::as_str) == Some("completed") {
            break r
                .get("conclusion")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        if Instant::now() >= deadline {
            return stop(format!(
                "bottle run {run_id} not finished after {:?}",
                o.run_timeout
            ));
        }
        tokio::time::sleep(Duration::from_secs(20)).await;
    };
    if conclusion != "success" {
        return stop(format!("bottle run {run_id} ended {conclusion}"));
    }

    // The stanza it gave, against the published bottle.
    std::fs::create_dir_all(s.data_dir.join("gh"))
        .map_err(io_err("creating the download directory"))?;
    let tmp = tempfile::tempdir_in(s.data_dir.join("gh"))
        .map_err(io_err("making a download directory"))?;
    let (dl, published) = (tmp.path().join("run"), tmp.path().join("published"));
    let rid = run_id.to_string();
    let dl_s = dl.display().to_string();
    if exec::text(
        gh,
        args(&["run", "download", &rid, "-R", &source, "-D", &dl_s]),
    )
    .await
    .is_err()
    {
        return stop(format!("could not download the artifacts of run {run_id}"));
    }
    let Some(rb) = find_file(&dl, "bottle.rb") else {
        return stop(format!("no bottle.rb in run {run_id}"));
    };
    let stanza = std::fs::read_to_string(&rb).map_err(io_err("reading bottle.rb"))?;
    let Some(want) = bottle_sha(&stanza, &t.bottle_tag) else {
        return stop(format!("no {} checksum in the stanza", t.bottle_tag));
    };
    if !root_url_ok(&stanza, version) {
        return stop(format!("the stanza's root_url is not release {version}"));
    }
    let pub_s = published.display().to_string();
    if exec::text(
        gh,
        args(&[
            "release", "download", version, "-R", &source, "-p", &asset, "-D", &pub_s,
        ]),
    )
    .await
    .is_err()
    {
        return stop(format!("could not download {asset}"));
    }
    let bytes =
        std::fs::read(published.join(&asset)).map_err(io_err(format!("reading {asset}")))?;
    let got = sha256_hex(&bytes);
    if got != want {
        return stop(format!("published bottle {got} is not the stanza's {want}"));
    }

    // The stanza on the pull request's branch, then a review.
    let path = format!("repos/{tap}/contents/Formula/{}.rb", t.formula);
    let file = exec::get(gh, &format!("{path}?ref={branch}")).await?;
    let blob = file
        .get("sha")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("no blob sha of the formula"))?
        .to_string();
    let content: String = file
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .split_whitespace()
        .collect();
    let text = base64::engine::general_purpose::STANDARD
        .decode(content)
        .map_err(|e| Error::Response {
            what: path.clone(),
            msg: e.to_string(),
        })
        .and_then(|b| {
            String::from_utf8(b).map_err(|e| Error::Response {
                what: path.clone(),
                msg: e.to_string(),
            })
        })?;
    let new_text = match place_stanza(&text, &stanza, &t.anchor) {
        Ok(x) => x,
        Err(e) => return stop(format!("could not place the stanza: {e}")),
    };
    let mut message = format!(
        "{} {version}: the {} bottle\n\nThe stanza the bottle workflow's publish run gave for the {version} release. The bottle's SHA-256 is that of the published file.\n",
        t.formula, t.bottle_tag
    );
    if let Some(tr) = &o.trailer {
        message.push_str(&format!("\n{}\n", tr.trim_end()));
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(new_text.as_bytes());
    exec::write(
        gh,
        "PUT",
        &path,
        &[
            ("message", &message),
            ("content", &encoded),
            ("sha", &blob),
            ("branch", &branch),
        ],
    )
    .await?;
    let body = format!(
        "[{}] The bottle workflow ran against this pull request's commit `{}`: it built the `{}` bottle and published it to the {version} release. The stanza it gave is now here. The published bottle's SHA-256 is `{got}`, the stanza's.\n\n{}",
        s.controller_runtime,
        short(&sha),
        t.bottle_tag,
        bot::REQUEST
    );
    exec::write(
        gh,
        "POST",
        &format!("repos/{tap}/issues/{number}/comments"),
        &[("body", &body)],
    )
    .await?;
    Ok(TapOutcome::Pushed(format!(
        "STANZA-PUSHED bottle {got} on {}#{number}",
        t.repo
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{fixture, json};

    fn sums() -> BTreeMap<String, String> {
        TARGETS
            .iter()
            .zip(['a', 'b', 'c', 'd'])
            .map(|(t, c)| (t.to_string(), c.to_string().repeat(64)))
            .collect()
    }

    #[test]
    fn formula_golden() {
        let golden = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/hgl-ai.rb"),
        )
        .unwrap();
        let got = render_formula(TEMPLATE, "hhenson/hgl-ai", "v0.1.0", &sums()).unwrap();
        assert_eq!(got, golden);
        assert!(got.contains("bin.install \"hgl-ai\"") && !got.contains("depends_on"));
        assert_eq!(this_repository(), "hhenson/hgl-ai");
    }

    #[test]
    fn formula_refusals() {
        assert!(render_formula(TEMPLATE, "o/r", "latest", &sums()).is_err());
        let mut few = sums();
        few.remove("x86_64-apple-darwin");
        assert!(render_formula(TEMPLATE, "o/r", "0.1.0", &few).is_err());
        assert!(render_formula("{{nope}}", "o/r", "0.1.0", &sums()).is_err());
    }

    #[test]
    fn checksums_from_files() {
        let dir = tempfile::tempdir().unwrap();
        for (t, c) in TARGETS.iter().zip(['a', 'b', 'c', 'd']) {
            let name = archive_name("0.1.0", t);
            std::fs::write(
                dir.path().join(format!("{name}.sha256")),
                format!("{}  {name}\n", c.to_string().repeat(64)),
            )
            .unwrap();
        }
        assert_eq!(read_checksums(dir.path(), "0.1.0").unwrap(), sums());
        std::fs::write(
            dir.path()
                .join(format!("{}.sha256", archive_name("0.1.0", TARGETS[0]))),
            "xyz  f\n",
        )
        .unwrap();
        assert!(read_checksums(dir.path(), "0.1.0").is_err());
        assert!(read_checksums(dir.path(), "0.2.0").is_err());
    }

    #[test]
    fn bottle_stanza() {
        let stanza = fixture("bottle.rb");
        assert_eq!(
            bottle_sha(&stanza, "arm64_tahoe").as_deref(),
            Some("e14fa4f10c6286176dd6fe221515e15cb5668ca10a2ab0359338f15d1035e2f3")
        );
        assert_eq!(bottle_sha(&stanza, "x86_64_linux"), None);
        assert!(root_url_ok(&stanza, "0.2.12"));
        assert!(!root_url_ok(&stanza, "0.2.1"));
        let formula = fixture("formula-no-bottle.rb");
        let placed = place_stanza(&formula, &stanza, "  license \"MIT\"\n\n").unwrap();
        assert_eq!(placed, fixture("formula-with-bottle.rb"));
        assert!(
            place_stanza(&placed, &stanza, "  license \"MIT\"\n\n").is_err(),
            "already has one"
        );
        assert!(place_stanza(&formula, &stanza, "  license \"GPL\"\n\n").is_err());
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn review_before_the_bottle() {
        let c: Vec<Comment> = json("comments-clean.json")
            .as_array()
            .unwrap()
            .iter()
            .map(Comment::from_rest)
            .collect();
        let bot = "chatgpt-codex-connector";
        assert_eq!(
            review_of(&c, bot, "1f461fa77bba922fe81383df4c475feb58b7c233"),
            Some(true)
        );
        assert_eq!(review_of(&c, bot, &"0".repeat(40)), None);
        let f: Vec<Comment> = json("comments-running.json")
            .as_array()
            .unwrap()
            .iter()
            .map(Comment::from_rest)
            .collect();
        assert_eq!(
            review_of(&f, bot, "9a8b7c6d5e4f30211203f4e5d6c7b8a9a0b1c2d3"),
            None
        );
        let mut g = f.clone();
        g[1].body = g[1]
            .body
            .replace("\"has_any_findings\":null", "\"has_any_findings\":true");
        assert_eq!(
            review_of(&g, bot, "9a8b7c6d5e4f30211203f4e5d6c7b8a9a0b1c2d3"),
            Some(false)
        );
    }

    #[tokio::test]
    async fn dry_run_stops_before_the_workflow() {
        let s = crate::testutil::settings();
        let tap = crate::exec::ReadOnly(
            crate::exec::fake::FakeGh::default()
                .on("pulls?state=open&head=hhenson:codex/hgl-0.2.12", "[{\"number\":5,\"head\":{\"sha\":\"1f461fa77bba922fe81383df4c475feb58b7c233\"}}]")
                .on("issues/5/comments", &fixture("comments-clean.json")),
        );
        let o = TapOpts {
            dry_run: true,
            ..TapOpts::default()
        };
        let r = release(&tap, &s, "0.2.12", &o).await.unwrap();
        assert!(
            matches!(&r, TapOutcome::Would(w) if w.contains("homebrew-bottle.yml") && w.contains("hgl-0.2.12.arm64_tahoe.bottle.tar.gz")),
            "{r:?}"
        );
        let none = crate::exec::ReadOnly(
            crate::exec::fake::FakeGh::default().on("pulls?state=open", "[]"),
        );
        assert!(matches!(
            release(&none, &s, "0.2.12", &o).await.unwrap(),
            TapOutcome::Stop(_)
        ));
    }
}
