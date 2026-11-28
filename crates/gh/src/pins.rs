//! `gh pins sync`: raise the wrapper's submodule pins (ROLE-12). A pin follows a repository's
//! `main`, or the commit a submodule path holds in another pin's new commit (the wrapper pins
//! the specification and core the compiler builds against, not their own `main`).
//!
//! When the pins are current nothing happens. Otherwise an open pin pull request for the new
//! commit is landed, or one is made: a commit on the wrapper's `main` that changes only the
//! gitlinks (GitHub's git data API, so no clone is needed), a branch
//! `<branch_prefix><pin_branch>` and a pull request, which then goes through `gh land`.

use std::time::Duration;

use hgl_ai_bus::Bus;
use hgl_ai_config::Pin;
use serde_json::{Value, json};

use crate::exec::{self, Gh};
use crate::gate::{self, Outcome};
use crate::land::{self, LandOpts};
use crate::progress;
use crate::{Error, Result, Settings, invalid, short};

#[derive(Debug, Clone)]
pub struct PinOpts {
    /// What the followed repository's `main` gained, one clause.
    pub what: String,
    pub trailer: Option<String>,
    pub footer: Option<String>,
    pub timeout: Duration,
    pub interval: Duration,
}

impl Default for PinOpts {
    fn default() -> Self {
        PinOpts {
            what: String::new(),
            trailer: None,
            footer: None,
            timeout: Duration::from_secs(25 * 60),
            interval: Duration::from_secs(90),
        }
    }
}

/// The commits the pins should hold, and what they hold now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// (wrapper path, wanted commit, where it comes from)
    pub want: Vec<(String, String, String)>,
    /// (wrapper path, commit on the wrapper's main)
    pub have: Vec<(String, Option<String>)>,
}

impl Plan {
    pub fn current(&self) -> bool {
        self.want.iter().all(|(p, sha, _)| {
            self.have
                .iter()
                .any(|(q, h)| q == p && h.as_deref() == Some(sha.as_str()))
        })
    }
    /// The first pin that follows a repository's `main`: it names the branch and the title.
    pub fn primary(&self) -> Option<&(String, String, String)> {
        self.want.first()
    }
}

/// The wanted commit of every pin, given the `main` heads of the followed repositories and
/// the submodule commits read at them. Pins with a `repo` come first.
pub fn plan(
    pins: &[Pin],
    mains: &[(String, String)],
    subs: &[((String, String), String)],
    have: Vec<(String, Option<String>)>,
) -> Result<Plan> {
    let mut want = Vec::new();
    for p in pins.iter().filter(|p| p.repo.is_some()) {
        let repo = p.repo.as_deref().unwrap_or_default();
        let sha = mains
            .iter()
            .find(|(r, _)| r == repo)
            .map(|(_, s)| s.clone())
            .ok_or_else(|| invalid(format!("no main of {repo}")))?;
        want.push((p.path.clone(), sha, format!("{repo} main")));
    }
    for p in pins.iter().filter(|p| p.repo.is_none()) {
        let (Some(from), Some(sub)) = (&p.from, &p.submodule) else {
            continue;
        };
        let sha = subs
            .iter()
            .find(|((f, s), _)| f == from && s == sub)
            .map(|(_, sha)| sha.clone())
            .ok_or_else(|| invalid(format!("no commit of {sub} in {from}")))?;
        want.push((p.path.clone(), sha, format!("{sub} in {from}")));
    }
    Ok(Plan { want, have })
}

fn fill(template: &str, sha: &str) -> String {
    template.replace("{sha8}", short(sha))
}

/// Branch, title, commit message and pull request body of the pin change.
pub fn texts(s: &Settings, p: &Plan, o: &PinOpts) -> Result<(String, String, String, String)> {
    let (_, sha, from) = p.primary().ok_or_else(|| invalid("no pins configured"))?;
    let branch = format!("{}{}", s.branch_prefix, fill(&s.pin_branch, sha));
    let title = fill(&s.pin_title, sha);
    let what = if o.what.is_empty() {
        String::new()
    } else {
        format!(": {}", o.what)
    };
    let primary_repo = from.trim_end_matches(" main");
    let mut lines = vec![format!(
        "`{primary_repo}` `main` at `{}`{what}.",
        short(sha)
    )];
    let others: Vec<String> = p
        .want
        .iter()
        .skip(1)
        .map(|(path, sha, _)| format!("`{path}` at `{}`", short(sha)))
        .collect();
    if !others.is_empty() {
        lines.push(format!(
            "{}: the commits `{primary_repo}` builds against.",
            others.join(" and ")
        ));
    }
    let plain = lines.join("\n").replace('`', "");
    let mut message = format!("{title}\n\n{plain}\n");
    if let Some(t) = &o.trailer {
        message.push_str(&format!("\n{}\n", t.trim_end()));
    }
    let mut body = lines.join("\n\n");
    body.push('\n');
    if let Some(f) = &o.footer {
        body.push_str(&format!("\n{}\n", f.trim_end()));
    }
    Ok((branch, title, message, body))
}

/// The body of the git data API's tree request: the gitlinks on top of `base_tree`.
pub fn tree_request(base_tree: &str, p: &Plan) -> Value {
    let entries: Vec<Value> = p
        .want
        .iter()
        .map(|(path, sha, _)| json!({"path": path, "mode": "160000", "type": "commit", "sha": sha}))
        .collect();
    json!({"base_tree": base_tree, "tree": entries})
}

/// The commit a submodule path holds in `repo` at `rev`, if it is a submodule there.
pub async fn submodule_at<G: Gh>(
    gh: &G,
    s: &Settings,
    repo: &str,
    path: &str,
    rev: &str,
) -> Result<Option<String>> {
    let v = match exec::get(
        gh,
        &format!("repos/{}/contents/{path}?ref={rev}", s.full(repo)),
    )
    .await
    {
        Ok(v) => v,
        Err(Error::Command { msg, .. }) if msg.contains("404") => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(
        if v.get("type").and_then(Value::as_str) == Some("submodule") {
            v.get("sha").and_then(Value::as_str).map(String::from)
        } else {
            None
        },
    )
}

/// What a sync did.
#[derive(Debug, Clone)]
pub enum Synced {
    Current(String),
    /// Dry run: what it would do.
    Would(String),
    Landed(Outcome),
    NotLanded(Outcome),
}

impl Synced {
    pub fn line(&self) -> String {
        match self {
            Synced::Current(l) | Synced::Would(l) => l.clone(),
            Synced::Landed(o) | Synced::NotLanded(o) => o.line.clone(),
        }
    }
    pub fn ok(&self) -> bool {
        !matches!(self, Synced::NotLanded(_))
    }
}

/// Read the pins and what they should be.
pub async fn read_plan<G: Gh>(gh: &G, s: &Settings) -> Result<Plan> {
    let mut mains = Vec::new();
    for repo in s.pin_sources() {
        mains.push((repo.to_string(), gate::main_head(gh, s, repo).await?));
    }
    let mut subs = Vec::new();
    for p in &s.pins {
        if let (Some(from), Some(sub)) = (&p.from, &p.submodule) {
            let src = s
                .pins
                .iter()
                .find(|q| &q.path == from)
                .and_then(|q| q.repo.clone())
                .unwrap_or_default();
            let at = mains
                .iter()
                .find(|(r, _)| *r == src)
                .map(|(_, sha)| sha.clone())
                .unwrap_or_default();
            let sha = submodule_at(gh, s, &src, sub, &at)
                .await?
                .ok_or_else(|| invalid(format!("{sub} is not a submodule of {src}")))?;
            subs.push(((from.clone(), sub.clone()), sha));
        }
    }
    let mut have = Vec::new();
    for p in &s.pins {
        have.push((
            p.path.clone(),
            submodule_at(gh, s, &s.wrapper, &p.path, "main").await?,
        ));
    }
    plan(&s.pins, &mains, &subs, have)
}

/// Raise the pins. Without `bus`, or with `dry_run`, only says what it would do.
pub async fn sync<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: Option<&Bus>,
    o: &PinOpts,
    dry_run: bool,
) -> Result<Synced> {
    // A pin pull request merged by an earlier sync whose landing stopped after the merge:
    // finish it first, or the pins read current and its landing is never finished.
    let mut resumed = None;
    if let Some(b) = bus {
        for (n, head) in unfinished(gh, s, b).await? {
            if dry_run {
                return Ok(Synced::Would(format!(
                    "would finish the landing of the pin pull request {}#{n}",
                    s.wrapper
                )));
            }
            let l = Box::pin(land::land(
                gh,
                s,
                b,
                &s.wrapper,
                n,
                &head,
                &land_opts(o, None),
            ))
            .await?;
            if !l.outcome.kind.landed() {
                return Ok(Synced::NotLanded(l.outcome));
            }
            resumed = Some(l.outcome);
        }
    }
    let p = read_plan(gh, s).await?;
    let Some((_, primary, _)) = p.primary().cloned() else {
        return Ok(Synced::Current("no pins configured".into()));
    };
    if p.current() {
        return Ok(match resumed {
            Some(o) => Synced::Landed(o),
            None => Synced::Current(format!("pins current at {}", short(&primary))),
        });
    }
    let (branch, title, message, body) = texts(s, &p, o)?;
    let w = s.full(&s.wrapper);
    let open = exec::get(
        gh,
        &format!("repos/{w}/pulls?state=open&head={}:{branch}", s.owner),
    )
    .await?;
    let existing = open.as_array().and_then(|a| a.first()).and_then(|pr| {
        Some((
            pr.get("number")?.as_u64()?,
            pr.pointer("/head/sha")?.as_str()?.to_string(),
        ))
    });
    let changes: Vec<String> = p
        .want
        .iter()
        .map(|(path, sha, _)| format!("{path} {}", short(sha)))
        .collect();
    let bus = match bus {
        Some(b) if !dry_run => b,
        _ => {
            let what = match &existing {
                Some((n, _)) => format!("would land the open pin pull request {}#{n}", s.wrapper),
                None => format!(
                    "would open \"{title}\" on {branch} ({}) and land it",
                    changes.join(", ")
                ),
            };
            return Ok(Synced::Would(what));
        }
    };
    let (number, head) = match existing {
        Some(e) => e,
        None => {
            let main = gate::main_head(gh, s, &s.wrapper).await?;
            let commit = exec::get(gh, &format!("repos/{w}/git/commits/{main}")).await?;
            let base_tree = commit
                .pointer("/tree/sha")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("no tree of main"))?;
            let tree = exec::write_json(
                gh,
                "POST",
                &format!("repos/{w}/git/trees"),
                &tree_request(base_tree, &p),
            )
            .await?;
            let tree = tree
                .get("sha")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("no tree sha"))?;
            let c = exec::write_json(
                gh,
                "POST",
                &format!("repos/{w}/git/commits"),
                &json!({"message": message, "tree": tree, "parents": [main]}),
            )
            .await?;
            let sha = c
                .get("sha")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("no commit sha"))?
                .to_string();
            point_branch(gh, &w, &branch, &sha).await?;
            let pr = exec::write_json(
                gh,
                "POST",
                &format!("repos/{w}/pulls"),
                &json!({"title": title, "head": branch, "base": "main", "body": body}),
            )
            .await?;
            let n = pr
                .get("number")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("no pull request number"))?;
            eprintln!("opened {}#{n} at {}", s.wrapper, short(&sha));
            (n, sha)
        }
    };
    let l = land::land(
        gh,
        s,
        bus,
        &s.wrapper,
        number,
        &head,
        &land_opts(o, Some(title)),
    )
    .await?;
    Ok(if l.outcome.kind.landed() {
        Synced::Landed(l.outcome)
    } else {
        Synced::NotLanded(l.outcome)
    })
}

/// How a pin pull request is landed.
fn land_opts(o: &PinOpts, what: Option<String>) -> LandOpts {
    LandOpts {
        // Made by the tool, not an agent: no author family for a review receipt to differ
        // from (ROLE-11); the review bot and the checks still gate it.
        gate: gate::Opts {
            tool_made: true,
            ..gate::Opts::default()
        },
        timeout: o.timeout,
        interval: o.interval,
        comment: false,
        notify: false,
        sync_pins: false,
        what,
        ..LandOpts::default()
    }
}

/// The wrapper's pin pull requests with a landing record that are merged and not finished,
/// as (number, head): a record with the merge commit, or only the intent of a pull request
/// GitHub shows merged. An intent whose pull request is not merged is left to the sync
/// itself; any other wrapper pull request's landing to its own command, which knows its
/// notes and comment.
async fn unfinished<G: Gh>(gh: &G, s: &Settings, bus: &Bus) -> Result<Vec<(u64, String)>> {
    let prefix = progress::key(&s.wrapper, 0);
    let prefix = prefix.trim_end_matches('0');
    let mut out = Vec::new();
    for key in bus.state_keys(prefix).await? {
        let Some(n) = key.strip_prefix(prefix).and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        let Some(p) = progress::read(bus, &s.wrapper, n).await? else {
            continue;
        };
        let pr = gate::fetch_pr(gh, s, &s.wrapper, n).await?;
        if !is_pin_branch(s, &pr.branch) || (p.merge.is_none() && !pr.merged) {
            continue;
        }
        out.push((n, if p.head.is_empty() { pr.head } else { p.head }));
    }
    Ok(out)
}

/// Whether `branch` is a pin pull request's: `<branch_prefix><pin_branch>` with `{sha8}`
/// as eight hex digits.
pub(crate) fn is_pin_branch(s: &Settings, branch: &str) -> bool {
    let template = format!("{}{}", s.branch_prefix, s.pin_branch);
    let Some((before, after)) = template.split_once("{sha8}") else {
        return branch == template;
    };
    branch
        .strip_prefix(before)
        .and_then(|b| b.strip_suffix(after))
        .is_some_and(|sha| sha.len() == 8 && sha.bytes().all(|c| c.is_ascii_hexdigit()))
}

/// Point `refs/heads/<branch>` of `w` at `sha`: a branch left by an earlier attempt (or one
/// whose pull request was closed) is moved there, forced; else it is made.
async fn point_branch<G: Gh>(gh: &G, w: &str, branch: &str, sha: &str) -> Result<()> {
    match exec::get(gh, &format!("repos/{w}/git/ref/heads/{branch}")).await {
        Ok(_) => {
            exec::write_json(
                gh,
                "PATCH",
                &format!("repos/{w}/git/refs/heads/{branch}"),
                &json!({"sha": sha, "force": true}),
            )
            .await?;
        }
        Err(e) if exec::is_not_found(&e) => {
            exec::write_json(
                gh,
                "POST",
                &format!("repos/{w}/git/refs"),
                &json!({"ref": format!("refs/heads/{branch}"), "sha": sha}),
            )
            .await?;
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::fake::FakeGh;
    use crate::testutil::settings;

    fn sha(c: char) -> String {
        c.to_string().repeat(40)
    }

    fn the_plan(have_compiler: char) -> Plan {
        let s = settings();
        let mains = vec![("hgl-compiler".to_string(), sha('c'))];
        let subs = vec![
            (
                ("hgl-compiler".to_string(), "spec/hgl_spec".to_string()),
                sha('5'),
            ),
            (
                ("hgl-compiler".to_string(), "spec/hgl_core".to_string()),
                sha('6'),
            ),
        ];
        let have = vec![
            ("hgl-compiler".to_string(), Some(sha(have_compiler))),
            ("hgl_spec".to_string(), Some(sha('5'))),
            ("hgl_core".to_string(), Some(sha('6'))),
        ];
        plan(&s.pins, &mains, &subs, have).unwrap()
    }

    #[test]
    fn current_or_not() {
        assert!(the_plan('c').current());
        let p = the_plan('b');
        assert!(!p.current());
        assert_eq!(
            p.want[0],
            ("hgl-compiler".into(), sha('c'), "hgl-compiler main".into())
        );
        assert_eq!(p.want[2].2, "spec/hgl_core in hgl-compiler");
        let s = settings();
        assert!(
            plan(&s.pins, &[], &[], vec![]).is_err(),
            "a followed repository without its main"
        );
    }

    #[test]
    fn pin_texts() {
        let s = settings();
        let o = PinOpts {
            what: "format patterns (hhenson/hgl-compiler#7)".into(),
            trailer: Some("Co-Authored-By: X".into()),
            ..PinOpts::default()
        };
        let (branch, title, message, body) = texts(&s, &the_plan('b'), &o).unwrap();
        assert_eq!(branch, "codex/pins-compiler-cccccccc");
        assert_eq!(title, "Pin the compiler at cccccccc");
        assert_eq!(
            message,
            "Pin the compiler at cccccccc\n\nhgl-compiler main at cccccccc: format patterns (hhenson/hgl-compiler#7).\nhgl_spec at 55555555 and hgl_core at 66666666: the commits hgl-compiler builds against.\n\nCo-Authored-By: X\n"
        );
        assert!(body.starts_with("`hgl-compiler` `main` at `cccccccc`: format patterns"));
        let t = tree_request("t0", &the_plan('b'));
        assert_eq!(t["base_tree"], "t0");
        assert_eq!(
            t["tree"][1],
            json!({"path": "hgl_spec", "mode": "160000", "type": "commit", "sha": sha('5')})
        );
    }

    #[test]
    fn pin_branches() {
        let s = settings();
        assert!(is_pin_branch(&s, "codex/pins-compiler-cccccccc"));
        assert!(is_pin_branch(&s, "codex/pins-compiler-01e99ea5"));
        for b in [
            "codex/980-format-patterns",
            "codex/pins-compiler-",
            "codex/pins-compiler-cccccccc0",
            "codex/pins-compiler-zzzzzzzz",
            "pins-compiler-cccccccc",
        ] {
            assert!(!is_pin_branch(&s, b), "{b}");
        }
    }

    #[tokio::test]
    async fn a_branch_left_behind_is_moved_not_made_again() {
        let w = "hhenson/hgl-project";
        let b = "codex/pins-compiler-cccccccc";
        // Left by an earlier attempt: forced to the new commit, no `POST git/refs` (HTTP 422).
        let gh = FakeGh::default()
            .on(
                &format!("{w}/git/ref/heads/{b}"),
                &format!(
                    "{{\"ref\":\"refs/heads/{b}\",\"object\":{{\"sha\":\"{}\"}}}}",
                    sha('1')
                ),
            )
            .on("-X PATCH", "{}");
        point_branch(&gh, w, b, &sha('2')).await.unwrap();
        let calls = gh.calls();
        assert!(
            calls
                .iter()
                .any(|c| c.contains("-X PATCH") && c.contains(&format!("git/refs/heads/{b}"))),
            "{calls:?}"
        );
        assert!(!calls.iter().any(|c| c.contains("-X POST")), "{calls:?}");
        // Not there: made.
        let gh = FakeGh::default()
            .fail("git/ref/heads/", "gh: Not Found (HTTP 404)")
            .on("-X POST", "{}");
        point_branch(&gh, w, b, &sha('2')).await.unwrap();
        assert!(
            gh.calls()
                .iter()
                .any(|c| c.contains("-X POST") && c.contains("git/refs"))
        );
        // Any other failure stops it.
        let gh = FakeGh::default().fail("git/ref/heads/", "gh: Server Error (HTTP 502)");
        assert!(point_branch(&gh, w, b, &sha('2')).await.is_err());
        assert_eq!(gh.calls().len(), 1);
    }

    #[tokio::test]
    async fn dry_run_reads_only() {
        let s = settings();
        let sub = |c: char| format!("{{\"type\":\"submodule\",\"sha\":\"{}\"}}", sha(c));
        let gh = crate::exec::ReadOnly(
            FakeGh::default()
                .on(
                    "hgl-compiler/git/ref/heads/main",
                    &format!("{{\"object\":{{\"sha\":\"{}\"}}}}", sha('c')),
                )
                .on("hgl-compiler/contents/spec/hgl_spec", &sub('5'))
                .on("hgl-compiler/contents/spec/hgl_core", &sub('6'))
                .on("hgl-project/contents/hgl-compiler", &sub('b'))
                .on("hgl-project/contents/hgl_spec", &sub('5'))
                .on("hgl-project/contents/hgl_core", &sub('6'))
                .on("hgl-project/pulls?state=open", "[]"),
        );
        let r = sync(&gh, &s, None, &PinOpts::default(), true)
            .await
            .unwrap();
        assert!(
            matches!(&r, Synced::Would(l) if l.contains("would open \"Pin the compiler at cccccccc\"")),
            "{r:?}"
        );
        assert!(
            gh.0.calls()
                .iter()
                .any(|c| c.contains("contents/spec/hgl_spec?ref=cccccccc"))
        );
    }
}
