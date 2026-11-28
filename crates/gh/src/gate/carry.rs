//! Prove a receipt can carry using the exact commits read from GitHub.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use hgl_ai_proto::Verdict;

type Proof<T> = Result<T, String>;

pub(super) async fn from_remote(
    url: &str,
    head: &str,
    main: &str,
    verdicts: &BTreeMap<&str, Verdict>,
) -> Proof<Option<String>> {
    for sha in [head, main] {
        hgl_ai_proto::check_head(sha).map_err(|e| e.to_string())?;
    }
    // Use a disposable bare repository so neither a seat's checkout nor its refs are needed.
    let scratch = tempfile::tempdir().map_err(|e| format!("could not create Git scratch: {e}"))?;
    let git = scratch.path();
    success(
        git,
        &[
            "init",
            "--bare",
            "--quiet",
            "--template=",
            "--object-format=sha1",
        ],
    )
    .await?;
    success(
        git,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-auto-gc",
            "--",
            url,
            head,
            main,
        ],
    )
    .await?;

    let main_history = success(git, &["rev-list", "--first-parent", main]).await?;
    let main_history: BTreeSet<_> = main_history.lines().collect();
    let mut commit = head.to_string();
    loop {
        let info = success(
            git,
            &["show", "-s", "--no-notes", "--format=%T %P", &commit, "--"],
        )
        .await?;
        let fields: Vec<&str> = info.split_whitespace().collect();
        let [tree, parent, merged] = fields.as_slice() else {
            return Ok(None);
        };
        if !main_history.contains(merged) {
            return Ok(None);
        }
        // A successful remerge with the recorded tree has an empty remerge-diff.
        // The exit code also rules out conflicts even if their temporary tree was committed.
        // Read the tracked attributes Git would see when merging from the first parent.
        let automatic = run(
            git,
            &["merge-tree", "--write-tree", parent, merged],
            Some(parent),
        )
        .await?;
        if automatic.status.code() == Some(1) {
            return Ok(None);
        }
        if stdout("merge-tree", automatic)?.trim() != *tree {
            return Ok(None);
        }
        match verdicts.get(parent) {
            Some(Verdict::Clean) => return Ok(Some((*parent).to_string())),
            Some(Verdict::Findings) => return Ok(None),
            None => commit = (*parent).to_string(),
        }
    }
}

async fn success(git: &Path, args: &[&str]) -> Proof<String> {
    stdout(args[0], run(git, args, None).await?)
}

fn stdout(command: &str, out: Output) -> Proof<String> {
    if !out.status.success() {
        // Git errors can contain the clone URL, including embedded credentials.
        return Err(format!("git {command} exited with {}", out.status));
    }
    String::from_utf8(out.stdout).map_err(|_| format!("git {command} gave invalid output"))
}

async fn run(git: &Path, args: &[&str], attr_source: Option<&str>) -> Proof<Output> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(git)
        .arg("--git-dir=.")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_GRAFT_FILE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_SHALLOW_FILE")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if args[0] != "fetch" {
        // Fetch uses the host's Git authentication. The remerge isolates host settings.
        cmd.args(["-c", "core.attributesFile=/dev/null"])
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_ATTR_SOURCE")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", git.join("unused-global-config"))
            .env("GIT_ATTR_NOSYSTEM", "1");
    }
    if let Some(commit) = attr_source {
        cmd.arg(format!("--attr-source={commit}"));
    }
    let command = args[0];
    tokio::time::timeout(Duration::from_secs(60), cmd.args(args).output())
        .await
        .map_err(|_| format!("git {command} timed out"))?
        .map_err(|e| format!("could not run git {command}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct History(tempfile::TempDir);

    impl History {
        fn new() -> Self {
            let repo = Self(tempfile::tempdir().unwrap());
            repo.git(&[
                "init",
                "--quiet",
                "--template=",
                "--object-format=sha1",
                "-b",
                "main",
            ]);
            repo.commit("common", "base");
            repo.git(&["switch", "-c", "pr"]);
            repo
        }

        fn git(&self, args: &[&str]) -> String {
            let out = std::process::Command::new("git")
                .current_dir(self.0.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    self.0.path().join("unused-global-config"),
                )
                .args(["-c", "user.name=Test", "-c", "user.email=test@example.org"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        }

        fn commit(&self, file: &str, contents: &str) -> String {
            std::fs::write(self.0.path().join(file), contents).unwrap();
            self.git(&["add", file]);
            self.git(&["commit", "-m", contents]);
            self.git(&["rev-parse", "HEAD"])
        }

        fn advance_main(&self, file: &str, contents: &str) -> String {
            self.git(&["switch", "main"]);
            let main = self.commit(file, contents);
            self.git(&["switch", "pr"]);
            main
        }

        async fn carry(&self, candidates: &[&str]) -> Option<String> {
            from_remote(
                self.0.path().to_str().unwrap(),
                &self.git(&["rev-parse", "HEAD"]),
                &self.git(&["rev-parse", "main"]),
                &candidates
                    .iter()
                    .map(|&head| (head, Verdict::Clean))
                    .collect(),
            )
            .await
            .unwrap()
        }
    }

    #[tokio::test]
    async fn carries_across_only_clean_first_parent_merges_of_main() {
        let repo = History::new();
        let reviewed = repo.commit("pr", "reviewed");
        let old_main = repo.advance_main("main", "advance");
        repo.git(&["merge", "--no-ff", "--no-edit", "main"]);
        assert_eq!(
            repo.carry(&[&reviewed]).await.as_deref(),
            Some(reviewed.as_str())
        );
        assert_eq!(repo.carry(&[&old_main]).await, None);

        let first_merge = repo.git(&["rev-parse", "HEAD"]);
        repo.advance_main("main", "advance again");
        repo.git(&["merge", "--no-ff", "--no-edit", "main"]);
        assert_eq!(
            repo.carry(&[&reviewed]).await.as_deref(),
            Some(reviewed.as_str())
        );
        assert_eq!(
            repo.carry(&[&reviewed, &first_merge]).await.as_deref(),
            Some(first_merge.as_str())
        );

        repo.commit("pr", "an intervening change");
        assert_eq!(repo.carry(&[&reviewed]).await, None);
        let main = repo.advance_main("main", "third advance");
        repo.git(&["merge", "--no-ff", "--no-edit", "main"]);
        assert_eq!(repo.carry(&[&reviewed]).await, None);

        repo.git(&["switch", "-c", "topic", &main]);
        repo.commit("topic", "unreviewed branch");
        repo.git(&["switch", "pr"]);
        let before_topic = repo.git(&["rev-parse", "HEAD"]);
        repo.git(&["merge", "--no-ff", "--no-edit", "topic"]);
        assert_eq!(repo.carry(&[&before_topic]).await, None);
    }

    #[tokio::test]
    async fn rejects_conflicts_and_edits_in_a_merge() {
        let repo = History::new();
        let reviewed = repo.commit("common", "pr version");
        repo.advance_main("common", "main version");
        // Reproduce a resolved conflict using the first parent's version.
        repo.git(&["merge", "--no-ff", "--no-edit", "-Xours", "main"]);
        let resolved = repo.git(&["rev-parse", "HEAD"]);
        assert_eq!(repo.carry(&[&reviewed]).await, None);

        repo.advance_main("main", "unrelated advance");
        repo.git(&["merge", "--no-ff", "--no-edit", "main"]);
        assert_eq!(repo.carry(&[&reviewed]).await, None);
        assert_eq!(
            repo.carry(&[&resolved]).await.as_deref(),
            Some(resolved.as_str())
        );

        std::fs::write(repo.0.path().join("common"), "edited in merge").unwrap();
        repo.git(&["commit", "-a", "--amend", "--no-edit"]);
        assert_eq!(repo.carry(&[&resolved]).await, None);
    }

    #[tokio::test]
    async fn rejects_a_merge_resolved_by_user_attributes() {
        let repo = History::new();
        let reviewed = repo.commit("common", "pr version\n");
        repo.advance_main("common", "main version\n");
        let attributes = repo.0.path().join("merge-attributes");
        std::fs::write(&attributes, "* merge=union\n").unwrap();
        repo.git(&[
            "-c",
            &format!("core.attributesFile={}", attributes.display()),
            "merge",
            "--no-ff",
            "--no-edit",
            "main",
        ]);
        assert_eq!(repo.carry(&[&reviewed]).await, None);
    }

    #[tokio::test]
    async fn each_merge_uses_its_first_parents_tracked_attributes() {
        let repo = History::new();
        repo.advance_main(".gitattributes", "common merge=union\n");
        repo.git(&["merge", "--ff-only", "main"]);
        let reviewed = repo.commit("common", "pr version\n");
        repo.advance_main("common", "main version\n");
        // This update merges cleanly with the first parent's union driver. It changes the
        // attributes for later merges to use the text driver.
        repo.advance_main(".gitattributes", "common merge=text\n");
        repo.git(&["merge", "--no-ff", "--no-edit", "main"]);
        repo.advance_main("main", "further advance");
        repo.git(&["merge", "--no-ff", "--no-edit", "main"]);

        assert_eq!(
            repo.carry(&[&reviewed]).await.as_deref(),
            Some(reviewed.as_str())
        );
    }
}
