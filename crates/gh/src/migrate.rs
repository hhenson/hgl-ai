//! Public repositories only (the owner's ruling on hgl-ai#181; the controller's, on the same
//! PR, that it covers all free text of a private repository): the viewer-readable values hold
//! no title and no label name of a private repository, or of one whose visibility is
//! unknown. `backlog`, `prs`, `tasks` and `landed.log` carry titles (`backlog` label names
//! too); `HGL_METRICS` samples label names. Values written before, or before a repository
//! turned private, are rewritten here, and every older revision of each key dropped in the
//! same write (a rollup), a deleted key's history purged: a key's history is readable by
//! anyone who may watch it.
//!
//! Callers: every path that provisions the hub (`hgl-ai init`, `server install`) runs
//! [`strip_once`]: no GitHub there, so every title and label name goes, once per hub
//! ([`MARKER`]). The coordinator, at its start ([`at_start`]; on a GitHub sign-in failure as
//! if every repository were private) and whenever a round finds a repository no longer
//! public ([`observe`]), strips what GitHub does not call public; its rounds write the
//! public titles again.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use hgl_ai_bus::{Bus, BusError};
use serde_json::Value;

use crate::backlog::{PRIVATE_LABEL, public_labels};
use crate::exec::{self, Gh};
use crate::{Result, Settings, state};

/// The `HGL_STATE` keys that carry titles.
pub const KEYS: [&str; 4] = [
    state::KEY_BACKLOG,
    state::KEY_PRS,
    state::KEY_TASKS,
    state::KEY_LANDED,
];
/// Set once every stored title has been stripped on this hub (not a key the viewer reads).
pub const MARKER: &str = "migrated.public-titles";

const ATTEMPTS: u32 = 8;

/// Each repository's `isPrivate` as the coordinator's newest round read it, shared with the
/// task-title loop: a title shows only for a repository it holds as public. Writers of a
/// title hold it read while they write, so a change to it is never overtaken.
pub type Visibility = Arc<tokio::sync::RwLock<BTreeMap<String, bool>>>;

/// Whether `vis` holds `repo` as public (`isPrivate` false); unknown is not.
pub fn is_public(vis: &BTreeMap<String, bool>, repo: &str) -> bool {
    vis.get(repo) == Some(&false)
}

/// `text`, the value of `key`, with every title and label name of a repository `public`
/// does not call public removed (labels as one [`PRIVATE_LABEL`] with their sum). Failing
/// closed: a line of a form not known is dropped.
pub fn strip(key: &str, text: &str, public: &dyn Fn(&str) -> bool) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    // `backlog`: the label lines of each repository that is not public, summed.
    let mut sums: BTreeMap<&str, u64> = BTreeMap::new();
    if key == state::KEY_BACKLOG {
        for l in &lines {
            if let Some((repo, _, n)) = label_line(l)
                && !public(repo)
            {
                *sums.entry(repo).or_default() += n;
            }
        }
    }
    let mut out = String::new();
    for line in lines {
        let l = match label_line(line).filter(|_| key == state::KEY_BACKLOG) {
            Some((repo, _, _)) if !public(repo) => sums
                .remove(repo)
                .map(|n| format!("# label {repo} {PRIVATE_LABEL} {n}")),
            _ => strip_line(key, line, public),
        };
        if let Some(l) = l {
            out.push_str(&l);
            out.push('\n');
        }
    }
    out
}

/// `# label REPO LABEL N` (a label may hold spaces).
fn label_line(line: &str) -> Option<(&str, &str, u64)> {
    let rest = line.strip_prefix("# label ")?;
    let (repo, rest) = rest.split_once(' ')?;
    let (label, n) = rest.rsplit_once(' ')?;
    Some((repo, label, n.parse().ok()?))
}

fn strip_line(key: &str, line: &str, public: &dyn Fn(&str) -> bool) -> Option<String> {
    let f: Vec<&str> = line.split('\t').collect();
    let keep = |prefixes: &[&str]| prefixes.iter().any(|p| line.starts_with(p));
    match key {
        state::KEY_BACKLOG => {
            if line.starts_with('#') {
                return keep(&["# read ", "# open ", "# label ", "# priority "])
                    .then(|| line.to_string());
            }
            // REPO NUMBER CLASS CREATED TITLE
            let [repo, number, class, created, title] =
                line.splitn(5, '\t').collect::<Vec<_>>()[..]
            else {
                return None;
            };
            let title = if public(repo) { title } else { "" };
            Some(format!("{repo}\t{number}\t{class}\t{created}\t{title}"))
        }
        state::KEY_PRS => {
            if let Some(rest) = line.strip_prefix("# pr\t") {
                // # pr REPO NUMBER PASSED TOTAL TITLE
                let [repo, number, passed, total, title] =
                    rest.splitn(5, '\t').collect::<Vec<_>>()[..]
                else {
                    return None;
                };
                let title = if public(repo) { title } else { "" };
                return Some(format!(
                    "# pr\t{repo}\t{number}\t{passed}\t{total}\t{title}"
                ));
            }
            if line.starts_with('#') {
                return keep(&["# read ", "# cost "]).then(|| line.to_string());
            }
            // REPO NUMBER HEAD STATE BOT THREADS CHECKS UPDATED: no free text.
            (f.len() == 8).then(|| line.to_string())
        }
        state::KEY_TASKS => {
            if let Some(rest) = line.strip_prefix("# title\t") {
                let repo = rest.split('\t').next().unwrap_or("");
                return public(repo).then(|| line.to_string());
            }
            if line.starts_with('#') {
                return keep(&["# tree\t"]).then(|| line.to_string());
            }
            // REPO ISSUE KIND SENDER RECIPIENT AT: no free text.
            (f.len() == 6).then(|| line.to_string())
        }
        state::KEY_LANDED => {
            // <UTC> <repo> #<pr> <merge sha> [<title>]; a line of another form goes.
            let w: Vec<&str> = line.split_whitespace().collect();
            let pr = w.get(2).and_then(|p| p.strip_prefix('#'));
            if w.len() < 4
                || !pr.is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            {
                return None;
            }
            Some(if public(w[1]) {
                line.to_string()
            } else {
                w[..4].join(" ")
            })
        }
        _ => Some(line.to_string()),
    }
}

/// Rewrite each of [`KEYS`] by [`strip`] and drop its older revisions, in one
/// compare-and-set write per key (a rollup; a key left empty is purged); purge the history
/// of a key that only a delete marker hides. Then [`strip_metrics`]. Returns the keys
/// rewritten.
pub async fn strip_private(bus: &Bus, public: &dyn Fn(&str) -> bool) -> Result<Vec<String>> {
    let mut done = Vec::new();
    for key in KEYS {
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            let res = match bus.state_get(key).await? {
                Some(e) => {
                    let text = strip(key, &e.value, public);
                    if text.trim().is_empty() {
                        // An empty value purges the key and its history, at this revision only.
                        bus.state_update(key, "", e.revision).await
                    } else {
                        bus.state_rollup(key, &text, e.revision).await
                    }
                }
                None => {
                    // A delete marker hides the key, not what its history holds: purge at
                    // the marker's revision.
                    let held = bus.state_history(key).await?;
                    match bus.state_revision(key).await? {
                        Some(rev) if held.iter().any(|v| !v.is_empty()) => {
                            bus.state_update(key, "", rev).await
                        }
                        _ => {
                            last = None;
                            break;
                        }
                    }
                }
            };
            match res {
                Err(c @ BusError::Conflict { .. }) => {
                    last = Some(c);
                    tokio::time::sleep(Duration::from_millis(50 << attempt.min(5))).await;
                }
                other => {
                    other?;
                    last = None;
                    done.push(key.to_string());
                    break;
                }
            }
        }
        if let Some(e) = last {
            return Err(e.into());
        }
    }
    strip_metrics(bus, public).await?;
    Ok(done)
}

/// Each `HGL_METRICS` sample of a repository `public` does not call public, with its label
/// names as one [`PRIVATE_LABEL`] (the bucket keeps one revision per key).
pub async fn strip_metrics(bus: &Bus, public: &dyn Fn(&str) -> bool) -> Result<usize> {
    let mut n = 0;
    for mut m in bus.metrics().await? {
        if public(&m.repo) {
            continue;
        }
        let labels = public_labels(Some(true), m.labels.clone());
        if labels != m.labels {
            m.labels = labels;
            bus.metrics_put(&m).await?;
            n += 1;
        }
    }
    Ok(n)
}

/// Every provisioning path's step: once per hub, strip every title and label name (no
/// GitHub to say which repositories are public). Returns the keys rewritten, `None` when
/// done before. The marker is set only once all of it is done; a failure is returned.
pub async fn strip_once(bus: &Bus) -> Result<Option<Vec<String>>> {
    if bus.state_get(MARKER).await?.is_some() {
        return Ok(None);
    }
    let done = strip_private(bus, &|_| false).await?;
    bus.state_put(MARKER, &hgl_ai_proto::utc_now_stamp())
        .await?;
    Ok(Some(done))
}

/// Each configured repository's `isPrivate`, in one GraphQL call; a repository GitHub did
/// not answer for is absent (private, for titles).
pub async fn visibility<G: Gh>(gh: &G, s: &Settings) -> Result<BTreeMap<String, bool>> {
    if s.repos.is_empty() {
        return Ok(BTreeMap::new());
    }
    let vars: String = (0..s.repos.len())
        .map(|i| format!(", $r{i}: String!"))
        .collect();
    let fields: String = (0..s.repos.len())
        .map(|i| format!(" r{i}: repository(owner: $owner, name: $r{i}) {{ isPrivate }}"))
        .collect();
    let q = format!("query Visibility($owner: String!{vars}) {{{fields} }}");
    let mut v: Vec<(String, String)> = vec![("owner".into(), s.owner.clone())];
    v.extend(
        s.repos
            .iter()
            .enumerate()
            .map(|(i, r)| (format!("r{i}"), r.clone())),
    );
    let vars: Vec<(&str, String)> = v.iter().map(|(k, x)| (k.as_str(), x.clone())).collect();
    let pages = exec::graphql(gh, &q, &vars, false).await?;
    Ok(parse_visibility(
        pages.first().unwrap_or(&Value::Null),
        &s.repos,
    ))
}

/// `data.r<i>.isPrivate` of a batched response, by repository; absent when not given.
pub(crate) fn parse_visibility(v: &Value, repos: &[String]) -> BTreeMap<String, bool> {
    repos
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let p = v.pointer(&format!("/data/r{i}/isPrivate"))?.as_bool()?;
            Some((r.clone(), p))
        })
        .collect()
}

/// The coordinator's step at its start: strip what GitHub does not call public, and return
/// the visibility read. With GitHub not answering, or not asked during a `hold`, every
/// title (failing closed; the rounds write them again) and no visibility.
pub async fn at_start<G: Gh>(
    gh: &G,
    s: &Settings,
    bus: &Bus,
) -> Result<(Vec<String>, BTreeMap<String, bool>)> {
    let hold = bus.state_get(state::KEY_HOLD).await?;
    let vis = if hold.is_some_and(|h| crate::table::hold_active(&h.value, chrono::Utc::now())) {
        BTreeMap::new()
    } else {
        visibility(gh, s).await.unwrap_or_else(|e| {
            tracing::warn!("repository visibility: {e}; stripping every title");
            BTreeMap::new()
        })
    };
    let keys = strip_private(bus, &|r: &str| is_public(&vis, r)).await?;
    Ok((keys, vis))
}

/// A round's visibility, `now`: it replaces what `vis` held (a repository it lacks is
/// unknown). Any repository `vis` held as public and `now` does not is stripped from every
/// value at once, history included. Returns those repositories.
pub async fn observe(
    vis: &Visibility,
    now: BTreeMap<String, bool>,
    bus: &Bus,
) -> Result<Vec<String>> {
    let turned: Vec<String> = {
        let mut held = vis.write().await;
        let turned = held
            .iter()
            .filter(|(r, p)| !**p && !is_public(&now, r))
            .map(|(r, _)| r.clone())
            .collect();
        *held = now.clone();
        turned
    };
    if !turned.is_empty() {
        strip_private(bus, &|r: &str| is_public(&now, r)).await?;
    }
    Ok(turned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Old rows of every key: a private (`priv`) or unknown repository's title and label
    /// names go, a public (`pub`) one's stay; lines of no known form go.
    #[test]
    fn stripping_old_rows() {
        let public = |r: &str| r == "pub";
        let backlog = "# read 20260101T000000Z\n# open priv 3\n# label priv bug 1\n\
            # label priv Bluebird customer 2\n# label pub bug 4\n# priority pub\n\
            pub\t1\tcodex\t20260101T000000Z\tPublic title\n\
            priv\t2\tcodex\t20260101T000000Z\tDEPLOY_TOKEN: Bluebird\n\
            other\t3\tcodex\t20260101T000000Z\tUnknown Bluebird\n# junk Bluebird\nnot a row Bluebird\n";
        assert_eq!(
            strip(state::KEY_BACKLOG, backlog, &public),
            "# read 20260101T000000Z\n# open priv 3\n# label priv private 3\n\
             # label pub bug 4\n# priority pub\n\
             pub\t1\tcodex\t20260101T000000Z\tPublic title\n\
             priv\t2\tcodex\t20260101T000000Z\t\n\
             other\t3\tcodex\t20260101T000000Z\t\n"
        );
        let prs = "# read R\n# cost 1 remaining 2 resetAt x\n\
            priv\t5\th\topen\tnone\t0\tnone\tT\n# pr\tpriv\t5\t1\t2\tBluebird secret\n\
            pub\t6\th\topen\tnone\t0\tnone\tT\n# pr\tpub\t6\t-\t-\tPublic one\nBluebird\n";
        assert_eq!(
            strip(state::KEY_PRS, prs, &public),
            "# read R\n# cost 1 remaining 2 resetAt x\n\
             priv\t5\th\topen\tnone\t0\tnone\tT\n# pr\tpriv\t5\t1\t2\t\n\
             pub\t6\th\topen\tnone\t0\tnone\tT\n# pr\tpub\t6\t-\t-\tPublic one\n"
        );
        let tasks = "priv\t7\tassign\tc\tw1\tA\n# tree\th\tw1\tpriv\t7\timplement\tC\t-\t-\t-\t-\n\
            # title\tpriv\t7\tBluebird\n# title\tpub\t8\tPublic\n# other Bluebird\n";
        assert_eq!(
            strip(state::KEY_TASKS, tasks, &public),
            "priv\t7\tassign\tc\tw1\tA\n# tree\th\tw1\tpriv\t7\timplement\tC\t-\t-\t-\t-\n\
             # title\tpub\t8\tPublic\n"
        );
        let landed = "S1 priv #1 m1 Bluebird title\nS2 pub #2 m2 Public title\nS3 priv #3 m3\n\
            Bluebird alone\nS4 Bluebird words without a number\nS5 pub #x m Bluebird\n";
        assert_eq!(
            strip(state::KEY_LANDED, landed, &public),
            "S1 priv #1 m1\nS2 pub #2 m2 Public title\nS3 priv #3 m3\n"
        );
        // Every repository private: nothing of a title or a label name anywhere.
        for (k, t) in [
            (state::KEY_BACKLOG, backlog),
            (state::KEY_PRS, prs),
            (state::KEY_TASKS, tasks),
            (state::KEY_LANDED, landed),
        ] {
            let s = strip(k, t, &|_| false);
            for frag in ["Bluebird", "Public", "bug"] {
                assert!(!s.contains(frag), "{k}: {s}");
            }
        }
    }

    #[test]
    fn visibility_from_a_response() {
        let v = serde_json::json!({"data": {
            "r0": {"isPrivate": false}, "r1": {"isPrivate": true}, "r2": null
        }});
        let repos = ["a", "b", "c"].map(String::from);
        let m = parse_visibility(&v, &repos);
        assert_eq!(m.get("a"), Some(&false));
        assert_eq!(m.get("b"), Some(&true));
        assert_eq!(m.get("c"), None, "unknown");
        assert!(is_public(&m, "a") && !is_public(&m, "b") && !is_public(&m, "c"));
    }
}
