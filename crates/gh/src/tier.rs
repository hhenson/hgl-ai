//! Review routing for an exact pull request head (hgl-ai#195).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{Error, GateRules, Result, Settings, exec, invalid, state};

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Assessment {
    pub head: String,
    pub tier: u8,
    pub paths: BTreeSet<String>,
    /// A PR or an associated issue carries `risk-high`.
    pub risk_high: bool,
}

#[derive(Deserialize)]
struct Pull {
    head: Head,
    base: Base,
    changed_files: usize,
    labels: Vec<Label>,
}

#[derive(Deserialize, PartialEq, Eq)]
struct Head {
    sha: String,
    #[serde(rename = "ref")]
    branch: String,
    repo: Option<Repository>,
}

#[derive(Deserialize, PartialEq, Eq)]
struct Base {
    sha: String,
    repo: Repository,
}

#[derive(Deserialize, PartialEq, Eq)]
struct Repository {
    #[serde(alias = "nameWithOwner")]
    full_name: String,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

fn has_label(labels: &[Label], name: &str) -> bool {
    labels
        .iter()
        .any(|label| label.name.eq_ignore_ascii_case(name))
}

#[derive(Deserialize)]
struct ChangedFile {
    filename: String,
    previous_filename: Option<String>,
    status: String,
    additions: u64,
    deletions: u64,
}

#[derive(Deserialize)]
struct Comparison {
    files: Vec<ChangedFile>,
}

impl ChangedFile {
    fn paths(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.filename.as_str()).chain(self.previous_filename.as_deref())
    }
}

fn test_only(path: &str) -> bool {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    name == "tests.rs" || name.ends_with("_tests.rs") || dir.split('/').any(|p| p == "tests")
}

fn tier_two_floor(path: &str) -> bool {
    matches!(path, "docs/roles.md" | "docs/protocol.md")
        || matches!(
            path.rsplit('/').next(),
            Some("AGENTS.md" | "Cargo.toml" | "Cargo.lock")
        )
        || path.starts_with(".github/workflows/")
        || path.starts_with("recipes/")
        || path.starts_with("skill/")
}

fn docs_or_tests(path: &str) -> bool {
    path == "README.md" || path.starts_with("docs/") || test_only(path)
}

fn classify<'a>(
    repo: &str,
    rules: &GateRules,
    files: &'a [ChangedFile],
    risk_high: bool,
    bug: bool,
) -> (u8, BTreeSet<&'a str>) {
    let paths = || files.iter().flat_map(ChangedFile::paths);
    let mut high: BTreeSet<_> = paths()
        .filter(|path| {
            rules
                .tier3_paths
                .iter()
                .any(|prefix| path.starts_with(prefix))
        })
        .collect();
    if repo == "hgl-ai" {
        high.extend(paths().filter(|path| {
            hgl_ai_config::github::unlisted_sensitive_path(path)
                && !rules
                    .tier3_new_exceptions
                    .iter()
                    .any(|except| except == path)
        }));
    }
    if risk_high || !high.is_empty() {
        return (3, high);
    }
    let ordinary: BTreeSet<_> = paths()
        .filter(|path| tier_two_floor(path) || !docs_or_tests(path))
        .collect();
    let small_fix = bug
        && files.len() <= 3
        && !paths().any(tier_two_floor)
        && files
            .iter()
            .filter(|file| !file.paths().all(test_only))
            .fold(0u64, |sum, file| {
                sum.saturating_add(file.additions)
                    .saturating_add(file.deletions)
            })
            <= 40;
    if !files.is_empty() && (ordinary.is_empty() || small_fix) {
        (1, paths().collect())
    } else {
        (2, ordinary)
    }
}

fn decode<T: DeserializeOwned>(what: &str, value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|e| Error::Response {
        what: what.into(),
        msg: e.to_string(),
    })
}

const CLOSING_ISSUES: &str = "query($owner: String!, $name: String!, $endCursor: String) {
  repository(owner: $owner, name: $name) { pullRequest(number: PR_NUMBER) {
    closingIssuesReferences(first: 100, after: $endCursor) {
      pageInfo { hasNextPage endCursor }
      nodes { number repository { nameWithOwner } }
    }
  } }
}";

#[derive(Deserialize)]
struct LinkedIssue {
    number: u64,
    repository: Repository,
}

#[derive(Deserialize)]
struct Issue {
    labels: Vec<Label>,
    pull_request: Option<Value>,
    repository_url: String,
}

async fn issue_labels<G: exec::Gh>(
    gh: &G,
    s: &Settings,
    repo: &str,
    number: u64,
    pr: &Pull,
) -> Result<(bool, bool)> {
    let args = [("owner", s.owner.clone()), ("name", repo.to_string())];
    let query = CLOSING_ISSUES.replace("PR_NUMBER", &number.to_string());
    let pages = exec::graphql(gh, &query, &args, true).await?;
    let mut linked = BTreeSet::new();
    for page in pages {
        let issues = page
            .pointer("/data/repository/pullRequest/closingIssuesReferences/nodes")
            .cloned()
            .unwrap_or(Value::Null);
        for issue in decode::<Vec<LinkedIssue>>("linked issues", issues)? {
            linked.insert((
                issue.repository.full_name.to_ascii_lowercase(),
                issue.number,
            ));
        }
    }
    let full = pr.base.repo.full_name.to_ascii_lowercase();
    let same_repo = pr
        .head
        .repo
        .as_ref()
        .is_some_and(|source| source.full_name.eq_ignore_ascii_case(&full));
    let task = if same_repo {
        state::issue_from_branch(&pr.head.branch, &s.branch_prefix).map(|n| (full.clone(), n))
    } else {
        None
    };
    let mut risk_high = false;
    let mut bug = same_repo && (!linked.is_empty() || task.is_some());
    for key in linked
        .iter()
        .chain(task.iter().filter(|key| !linked.contains(*key)))
    {
        let path = format!("repos/{}/issues/{}", key.0, key.1);
        let issue = match exec::get(gh, &path).await {
            Ok(value) => decode::<Issue>(&path, value)?,
            Err(e) if !linked.contains(key) && exec::is_not_found(&e) => {
                bug = false;
                continue;
            }
            Err(e) => return Err(e),
        };
        if issue.pull_request.is_some() {
            bug = false;
            continue;
        }
        risk_high |= has_label(&issue.labels, "risk-high");
        // The issue endpoint can return the target of a redirection.
        let returned_repo = issue
            .repository_url
            .rsplit_once("/repos/")
            .map(|(_, name)| name);
        bug &= key.0 == full
            && returned_repo.is_some_and(|name| name.eq_ignore_ascii_case(&full))
            && has_label(&issue.labels, "bug");
    }
    Ok((risk_high, bug))
}

/// Classify GitHub's PR diff at `head` without changing labels or coordinator state.
pub async fn evaluate<G: exec::Gh>(
    gh: &G,
    s: &Settings,
    repo: &str,
    number: u64,
    head: &str,
) -> Result<Assessment> {
    hgl_ai_proto::check_head(head)?;
    let full = s.full(repo);
    let path = format!("repos/{full}/pulls/{number}");
    let pr: Pull = decode(&path, exec::get(gh, &path).await?)?;
    let moved = || {
        invalid(format!(
            "{repo} #{number} moved while checking {head}; refresh its head and base"
        ))
    };
    if pr.head.sha != head {
        return Err(moved());
    }
    hgl_ai_proto::check_head(&pr.base.sha)?;
    // Immutable SHAs keep the files bound to the requested head even if the branch is
    // force-pushed away and back. The first page holds the entire file list, up to 300.
    let compare = format!("repos/{full}/compare/{}...{head}?per_page=1", pr.base.sha);
    let Comparison { files } = decode("comparison", exec::get(gh, &compare).await?)?;
    if files.len() != pr.changed_files {
        return Err(invalid(format!(
            "{repo} #{number} has an incomplete comparison at {head} ({} of {} files)",
            files.len(),
            pr.changed_files
        )));
    }
    if files.iter().any(|file| {
        file.filename.is_empty()
            || (file.status == "renamed"
                && file.previous_filename.as_deref().is_none_or(str::is_empty))
    }) {
        return Err(invalid(format!(
            "{repo} #{number} has a comparison file without a path"
        )));
    }
    let (issue_risk, bug) = issue_labels(gh, s, repo, number, &pr).await?;
    let current: Pull = decode(&path, exec::get(gh, &path).await?)?;
    if current.head != pr.head
        || current.base != pr.base
        || current.changed_files != pr.changed_files
    {
        return Err(moved());
    }
    let risk_high = issue_risk || has_label(&current.labels, "risk-high");
    let (tier, paths) = classify(repo, &s.gate_rules(repo), &files, risk_high, bug);
    Ok(Assessment {
        head: head.to_string(),
        tier,
        paths: paths.into_iter().map(String::from).collect(),
        risk_high,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{ReadOnly, fake::FakeGh};
    use crate::testutil::settings;
    use serde_json::json;

    const HEAD: &str = "1111111111111111111111111111111111111111";
    const BASE: &str = "2222222222222222222222222222222222222222";
    const PR_PATH: &str = "pulls/7";

    fn file(path: &str, changes: u64) -> ChangedFile {
        ChangedFile {
            filename: path.into(),
            previous_filename: None,
            status: "modified".into(),
            additions: changes,
            deletions: 0,
        }
    }

    #[test]
    fn tier_paths_and_precedence() {
        let s = settings();
        let mut rules = s.gate_rules("hgl-ai");
        let classify = |rules: &GateRules, files: &[ChangedFile]| {
            let (tier, paths) = super::classify("hgl-ai", rules, files, false, true);
            (tier, paths.into_iter().collect::<Vec<_>>().join(" "))
        };
        assert_eq!(
            classify(
                &rules,
                &[
                    file("docs/design.md", 100),
                    file("crates/gh/src/live_tests.rs", 100)
                ]
            )
            .0,
            1
        );
        for path in [
            ".github/workflows/ci.yml",
            ".github/workflows/test.yml",
            "docs/protocol.md",
            "docs/roles.md",
            "docs/AGENTS.md",
            "recipes/n.toml",
            "skill/SKILL.md",
            "AGENTS.md",
        ] {
            for bug in [false, true] {
                assert_eq!(
                    super::classify("hgl-ai", &rules, &[file(path, 1)], false, bug),
                    (2, BTreeSet::from([path]))
                );
            }
        }
        let mut rename = file("docs/old-gate.md", 0);
        rename.previous_filename = Some("crates/gh/src/gate.rs".into());
        assert_eq!(
            classify(&rules, &[rename, file("docs/roles.md", 1)]),
            (3, "crates/gh/src/gate.rs".into())
        );
        let path = "crates/gh/tests/new.rs";
        assert_eq!(classify(&rules, &[file(path, 1)]), (3, path.into()));
        rules.tier3_new_exceptions.push(path.into());
        assert_eq!(classify(&rules, &[file(path, 99)]), (1, path.into()));
        rules.tier3_paths.push("crates/gh/tests/".into());
        assert_eq!(classify(&rules, &[file(path, 1)]), (3, path.into()));
    }

    #[test]
    fn small_fix_bounds() {
        let rules = settings().gate_rules("other");
        let mut fix = vec![
            file("src/lib.rs", 39),
            file("src/parser.rs", 0),
            file("tests/parser.rs", 200),
        ];
        fix[1].deletions = 1;
        let tier = |files: &[ChangedFile], bug| classify("other", &rules, files, false, bug).0;
        assert_eq!(tier(&fix, true), 1);
        assert_eq!(tier(&fix, false), 2);
        fix[1].deletions = 2;
        assert_eq!(tier(&fix, true), 2);
        fix[1].deletions = 1;
        fix.push(file("tests/resource.txt", 0));
        assert_eq!(tier(&fix, true), 2);
        assert_eq!(tier(&[file("crates/new/Cargo.toml", 1)], true), 2);
        assert_eq!(tier(&[file("Cargo.lock", 1)], true), 2);
        assert_eq!(tier(&[file("src/contests/example.rs", 60)], true), 2);
    }

    #[test]
    fn cargo_files_have_tier_two_floor() {
        let rules = settings().gate_rules("hgl-ai");
        for path in ["tests/Cargo.toml", "docs/examples/Cargo.lock"] {
            for bug in [false, true] {
                assert_eq!(
                    classify("hgl-ai", &rules, &[file(path, 1)], false, bug),
                    (2, BTreeSet::from([path]))
                );
            }
        }
    }

    #[test]
    fn github_gate_config_is_tier_three() {
        let path = "crates/config/src/github.rs";
        let rules = settings().gate_rules("hgl-ai");
        assert_eq!(
            classify("hgl-ai", &rules, &[file(path, 1)], false, true),
            (3, BTreeSet::from([path]))
        );
    }

    fn pr(head: &str, count: usize, labels: &[Value]) -> Value {
        let full = settings().full("hgl-ai");
        json!({
            "head": {"sha": head, "ref": "codex/q41-fix", "repo": {"full_name": full}},
            "base": {"sha": BASE, "repo": {"full_name": full}},
            "changed_files": count,
            "labels": labels,
        })
    }

    fn fake(links: &[(&str, u64)]) -> FakeGh {
        let nodes: Vec<_> = links
            .iter()
            .map(|(repo, number)| json!({"number": number, "repository": {"nameWithOwner": repo}}))
            .collect();
        FakeGh::default()
            .on(
                &format!("compare/{BASE}...{HEAD}?per_page=1"),
                r#"{"files":[{"filename":"src/lib.rs","status":"modified","additions":2,"deletions":1}]}"#,
            )
            .on(
                "closingIssuesReferences",
                &json!({"data":{"repository":{"pullRequest":{"closingIssuesReferences":{
                    "nodes": nodes
                }}}}}).to_string(),
            )
    }

    fn issue(repo: &str, label: Option<&str>) -> String {
        let labels: Vec<_> = label
            .into_iter()
            .map(|name| json!({"name": name}))
            .collect();
        json!({
            "labels": labels,
            "repository_url": format!("https://api.github.com/repos/{repo}"),
        })
        .to_string()
    }

    #[tokio::test]
    async fn only_issue_bug_lowers_and_risk_high_raises() {
        let s = settings();
        let repo = s.full("hgl-ai");
        let uppercase = repo.to_ascii_uppercase();
        let plain = pr(HEAD, 1, &[json!({"name":"tier-1"}), json!({"name":"bug"})]);
        let risk = pr(HEAD, 1, &[json!({"name":"risk-high"})]);
        let none = issue(&repo, None);
        let bug = issue(&repo, Some("bug"));
        let high = issue(&repo, Some("risk-high"));
        let mut local = plain.clone();
        local["head"]["repo"]["full_name"] = json!(uppercase);
        for (pr, task, linked, expected) in [
            (&local, &bug, &bug, 1),
            (&plain, &bug, &none, 2),
            (&plain, &none, &bug, 2),
            (&plain, &bug, &high, 3),
            (&risk, &none, &none, 3),
        ] {
            let gh = ReadOnly(
                fake(&[(&uppercase, 53), (&repo, 54)])
                    .on(PR_PATH, &pr.to_string())
                    .on("issues/41", task)
                    .on("issues/53", linked)
                    .on("issues/54", &bug),
            );
            let a = evaluate(&gh, &s, "hgl-ai", 7, HEAD).await.unwrap();
            assert_eq!(a.head, HEAD);
            assert_eq!(a.tier, expected);
            assert_eq!(a.risk_high, expected == 3);
            assert_eq!(a.paths.is_empty(), expected == 3);
        }
        let mut without_task = plain.clone();
        without_task["head"]["ref"] = json!("topic");
        for (linked_repo, returned_repo, label, expected) in [
            (repo.as_str(), uppercase.as_str(), "bug", 1),
            ("another/hgl-ai", "another/hgl-ai", "bug", 2),
            ("another/hgl-ai", "another/hgl-ai", "risk-high", 3),
            (repo.as_str(), "another/hgl-ai", "bug", 2),
        ] {
            let gh = ReadOnly(
                fake(&[(linked_repo, 53)])
                    .on(PR_PATH, &without_task.to_string())
                    .on("issues/53", &issue(returned_repo, Some(label))),
            );
            assert_eq!(
                evaluate(&gh, &s, "hgl-ai", 7, HEAD).await.unwrap().tier,
                expected
            );
        }
        let mut fork = plain;
        fork["head"]["repo"]["full_name"] = json!("another/hgl-ai");
        let gh = ReadOnly(
            fake(&[(&repo, 53)])
                .on(PR_PATH, &fork.to_string())
                .on("issues/41", &bug)
                .on("issues/53", &bug),
        );
        assert_eq!(evaluate(&gh, &s, "hgl-ai", 7, HEAD).await.unwrap().tier, 2);
        assert!(gh.0.calls().iter().all(|call| !call.contains("issues/41")));
    }

    #[tokio::test]
    async fn rejects_stale_head_and_partial_diff() {
        let s = settings();
        let stale = fake(&[]).on(PR_PATH, &pr(BASE, 1, &[]).to_string());
        assert!(
            evaluate(&stale, &s, "hgl-ai", 7, HEAD)
                .await
                .unwrap_err()
                .to_string()
                .contains("moved")
        );
        assert_eq!(stale.calls().len(), 1);
        let partial = fake(&[]).on(PR_PATH, &pr(HEAD, 2, &[]).to_string());
        assert!(
            evaluate(&partial, &s, "hgl-ai", 7, HEAD)
                .await
                .unwrap_err()
                .to_string()
                .contains("incomplete")
        );
        let race = fake(&[])
            .once(PR_PATH, &pr(HEAD, 1, &[]).to_string())
            .on(PR_PATH, &pr(BASE, 1, &[]).to_string())
            .on("issues/41", &issue(&s.full("hgl-ai"), None));
        assert!(
            evaluate(&race, &s, "hgl-ai", 7, HEAD)
                .await
                .unwrap_err()
                .to_string()
                .contains("moved")
        );
    }
}
