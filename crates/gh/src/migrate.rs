//! Public repositories only (the owner's ruling on hgl-ai#181): the viewer-readable keys that
//! carry titles (`backlog`, `prs`, `tasks`, `landed.log`) hold none of a private repository's,
//! or of one whose visibility is unknown. Rows written before are rewritten here, and every
//! older revision of each key dropped in the same write (a rollup): a key's history is
//! readable by anyone who may watch it.
//!
//! Two callers: `hgl-ai init` (an upgrade's step; no GitHub) strips every title once, as if
//! every repository were private, and records that in [`MARKER`]; the coordinator, at each
//! start, strips what GitHub says is not public. The coordinator's next rounds write the
//! public titles again.

use std::collections::BTreeMap;
use std::time::Duration;

use hgl_ai_bus::{Bus, BusError};
use serde_json::Value;

use crate::exec::{self, Gh};
use crate::{Result, Settings, state};

/// The keys that carry titles.
pub const KEYS: [&str; 4] = [
    state::KEY_BACKLOG,
    state::KEY_PRS,
    state::KEY_TASKS,
    state::KEY_LANDED,
];
/// Set once `hgl-ai init` has stripped every title (not a key the viewer reads).
pub const MARKER: &str = "migrated.public-titles";

const ATTEMPTS: u32 = 8;

/// `text`, the value of `key`, with every title of a repository `public` does not call
/// public removed. Failing closed: a line of a form not known is dropped.
pub fn strip(key: &str, text: &str, public: &dyn Fn(&str) -> bool) -> String {
    let mut out = String::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if let Some(l) = strip_line(key, line, public) {
            out.push_str(&l);
            out.push('\n');
        }
    }
    out
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
            // <UTC> <repo> #<pr> <merge sha> [<title>]
            let w: Vec<&str> = line.split_whitespace().collect();
            if w.len() > 4 && !public(w[1]) {
                Some(w[..4].join(" "))
            } else {
                Some(line.to_string())
            }
        }
        _ => Some(line.to_string()),
    }
}

/// Rewrite each of [`KEYS`] by [`strip`] and drop its older revisions, in one
/// compare-and-set write per key (a rollup; a key left empty is purged). Returns the keys
/// rewritten.
pub async fn strip_private(bus: &Bus, public: &dyn Fn(&str) -> bool) -> Result<Vec<String>> {
    let mut done = Vec::new();
    for key in KEYS {
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            let Some(e) = bus.state_get(key).await? else {
                last = None;
                break;
            };
            let text = strip(key, &e.value, public);
            let res = if text.trim().is_empty() {
                // An empty value purges the key and its history, at this revision only.
                bus.state_update(key, "", e.revision).await
            } else {
                bus.state_rollup(key, &text, e.revision).await
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
    Ok(done)
}

/// `hgl-ai init`'s step: once per hub, strip every title of [`KEYS`] (no GitHub to say
/// which repositories are public). Returns the keys rewritten, `None` when done before.
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

fn parse_visibility(v: &Value, repos: &[String]) -> BTreeMap<String, bool> {
    repos
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let p = v.pointer(&format!("/data/r{i}/isPrivate"))?.as_bool()?;
            Some((r.clone(), p))
        })
        .collect()
}

/// The coordinator's step at each start: strip what GitHub does not say is public; with
/// GitHub not answering, or not asked during a `hold`, every title (failing closed; the
/// rounds write them again).
pub async fn at_start<G: Gh>(gh: &G, s: &Settings, bus: &Bus) -> Result<Vec<String>> {
    let hold = bus.state_get(state::KEY_HOLD).await?;
    if hold.is_some_and(|h| crate::table::hold_active(&h.value, chrono::Utc::now())) {
        return strip_private(bus, &|_| false).await;
    }
    let vis = match visibility(gh, s).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("repository visibility: {e}; stripping every title");
            BTreeMap::new()
        }
    };
    strip_private(bus, &|r: &str| vis.get(r) == Some(&false)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Old rows of every key: a private (`priv`) or unknown repository's title goes, a
    /// public (`pub`) one's stays; lines of no known form go.
    #[test]
    fn stripping_old_rows() {
        let public = |r: &str| r == "pub";
        let backlog = "# read 20260101T000000Z\n# open priv 3\n# label priv bug 1\n# priority pub\n\
            pub\t1\tcodex\t20260101T000000Z\tPublic title\n\
            priv\t2\tcodex\t20260101T000000Z\tDEPLOY_TOKEN: Bluebird\n\
            other\t3\tcodex\t20260101T000000Z\tUnknown Bluebird\n# junk Bluebird\nnot a row Bluebird\n";
        assert_eq!(
            strip(state::KEY_BACKLOG, backlog, &public),
            "# read 20260101T000000Z\n# open priv 3\n# label priv bug 1\n# priority pub\n\
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
        let landed = "S1 priv #1 m1 Bluebird title\nS2 pub #2 m2 Public title\nS3 priv #3 m3\n";
        assert_eq!(
            strip(state::KEY_LANDED, landed, &public),
            "S1 priv #1 m1\nS2 pub #2 m2 Public title\nS3 priv #3 m3\n"
        );
        // Every repository private: nothing of a title anywhere.
        for (k, t) in [
            (state::KEY_BACKLOG, backlog),
            (state::KEY_PRS, prs),
            (state::KEY_TASKS, tasks),
            (state::KEY_LANDED, landed),
        ] {
            let s = strip(k, t, &|_| false);
            assert!(!s.contains("Bluebird") && !s.contains("Public"), "{k}: {s}");
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
    }
}
