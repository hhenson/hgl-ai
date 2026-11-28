//! Runtime adapters (docs/design.md §4): the command line of one headless turn, and what
//! its streamed JSON says about the session, success and quota refusals.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub use hgl_ai_config::{CODEX_PROFILE, codex_profile};
use hgl_ai_config::{Resolved, model_args};
use hgl_ai_proto::Runtime;
use serde_json::Value;

use crate::proc::CmdSpec;

/// What one turn needs from its seat and task.
#[derive(Debug, Clone)]
pub struct TurnInput<'a> {
    pub prompt: &'a str,
    /// Runtime, model and effort; their flags come from [`model_args`], shared with
    /// interactive sessions.
    pub resolved: &'a Resolved,
    /// The session of this task's previous turn on the same runtime.
    pub resume: Option<&'a str>,
    /// The tree's `src/` (or the seat's inbox directory).
    pub cwd: &'a Path,
    /// Writable beyond `cwd`: the task directory and the shared clone (worktree metadata).
    pub writable: &'a [PathBuf],
}

/// The agent command line of a headless turn. The prompt goes on stdin
/// ([`CmdSpec::input`]), never in an argument: Linux caps one argument at 128 KiB, and a
/// note may be up to 1 MiB. The caller adds environment and the cap.
pub fn command(program: &Path, t: &TurnInput) -> CmdSpec {
    let mut a: Vec<OsString> = Vec::new();
    let mut input = t.prompt.as_bytes().to_vec();
    let s = |a: &mut Vec<OsString>, v: &[&str]| a.extend(v.iter().map(OsString::from));
    let models = model_args(t.resolved);
    let models: Vec<&str> = models.iter().map(String::as_str).collect();
    let add_dirs = |a: &mut Vec<OsString>| {
        for d in t.writable {
            a.push("--add-dir".into());
            a.push(d.into());
        }
    };
    match t.resolved.runtime {
        Runtime::Claude => {
            // `-p` without a prompt argument reads the prompt from stdin.
            s(
                &mut a,
                &["-p", "--output-format", "stream-json", "--verbose"],
            );
            s(&mut a, &models);
            if let Some(id) = t.resume {
                s(&mut a, &["--resume", id]);
            }
            s(&mut a, &["--permission-mode", "bypassPermissions"]);
            add_dirs(&mut a);
        }
        Runtime::Codex => {
            s(&mut a, &["exec"]);
            if t.resume.is_some() {
                s(&mut a, &["resume"]);
            }
            // `--strict-config`: a CLI that does not know `default_permissions` or
            // `permissions` (no profiles) fails the turn instead of ignoring them and
            // running read-only, without network (hgl-ai#59). `doctor` probes both.
            s(&mut a, &["--json", "--strict-config"]);
            s(&mut a, &models);
            for c in codex_profile(t.writable) {
                s(&mut a, &["-c", &c]);
            }
            s(&mut a, &["--skip-git-repo-check"]);
            // `exec resume` takes no `-C`; the cwd is the process's.
            match t.resume {
                Some(id) => s(&mut a, &[id]),
                None => {
                    a.push("-C".into());
                    a.push(t.cwd.into());
                }
            }
            // `-`: the prompt from stdin.
            a.push("-".into());
        }
        Runtime::Agy => {
            // `-p` takes the prompt as its value and reads no stdin; with
            // `--input-format stream-json` agy runs headless, one turn per stdin line.
            s(
                &mut a,
                &[
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                ],
            );
            input = agy_input(t.prompt);
            s(&mut a, &models);
            if let Some(id) = t.resume {
                s(&mut a, &["--conversation", id]);
            }
            s(&mut a, &["--dangerously-skip-permissions"]);
            add_dirs(&mut a);
        }
    }
    CmdSpec {
        program: program.to_path_buf(),
        args: a,
        env: Vec::new(),
        cwd: t.cwd.to_path_buf(),
        input,
    }
}

/// agy's `stream-json` input: one user message holding the prompt, on one line.
fn agy_input(prompt: &str) -> Vec<u8> {
    let msg = serde_json::json!({
        "event": "user",
        "message": {"role": "user", "content": prompt},
    });
    let mut line = msg.to_string().into_bytes();
    line.push(b'\n');
    line
}

/// What a turn's output said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    pub session_id: Option<String>,
    /// The structured result: `Some(true)` success, `Some(false)` an error result, `None` none seen.
    pub result_ok: Option<bool>,
    /// The turn was refused for quota or rate limits (only when it did not succeed).
    pub quota: bool,
    /// Any output beyond the known refusal protocol may represent work.
    pub activity: bool,
    pub error: Option<String>,
}

/// Incremental parser of one runtime's stream.
#[derive(Debug)]
pub struct Parser {
    runtime: Runtime,
    p: Parsed,
    quota_hint: bool,
}

impl Parser {
    pub fn new(runtime: Runtime) -> Self {
        Parser {
            runtime,
            p: Parsed::default(),
            quota_hint: false,
        }
    }

    pub fn line(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            self.p.activity = true;
            return;
        };
        let quiet = match self.runtime {
            Runtime::Codex => matches!(
                str_at(&v, "type"),
                Some("thread.started" | "turn.started" | "error" | "turn.failed")
            ),
            Runtime::Claude => matches!(
                str_at(&v, "type"),
                Some("system" | "rate_limit_event" | "result")
            ),
            Runtime::Agy => matches!(str_at(&v, "event"), Some("init" | "error" | "result")),
        };
        self.p.activity |= !quiet;
        match self.runtime {
            Runtime::Claude => self.claude(&v),
            Runtime::Codex => self.codex(&v),
            Runtime::Agy => self.agy(&v),
        }
    }

    fn claude(&mut self, v: &Value) {
        if let Some(id) = str_at(v, "session_id") {
            self.p.session_id = Some(id.to_string());
        }
        match str_at(v, "type") {
            Some("rate_limit_event") => {
                let status = v.pointer("/rate_limit_info/status").and_then(Value::as_str);
                if status == Some("rejected") {
                    self.quota_hint = true;
                }
            }
            Some("result") => {
                let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                self.p.result_ok = Some(!is_error);
                if is_error {
                    let msg = str_at(v, "result")
                        .or(str_at(v, "subtype"))
                        .unwrap_or("error result");
                    self.error(msg);
                    if v.get("api_error_status").and_then(Value::as_u64) == Some(429) {
                        self.quota_hint = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn codex(&mut self, v: &Value) {
        match str_at(v, "type") {
            Some("thread.started") => {
                if let Some(id) = str_at(v, "thread_id") {
                    self.p.session_id = Some(id.to_string());
                }
            }
            Some("turn.completed") => {
                if self.p.result_ok.is_none() {
                    self.p.result_ok = Some(true);
                }
            }
            Some("turn.failed") => {
                self.p.result_ok = Some(false);
                let msg = v
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("turn failed");
                self.error(msg);
            }
            Some("error") => {
                let msg = str_at(v, "message").unwrap_or("error");
                self.error(msg);
            }
            _ => {}
        }
    }

    fn agy(&mut self, v: &Value) {
        if let Some(id) = str_at(v, "conversation_id") {
            self.p.session_id = Some(id.to_string());
        }
        match str_at(v, "event") {
            Some("result") => {
                let r = v.get("result").unwrap_or(&Value::Null);
                if let Some(id) = str_at(r, "conversation_id") {
                    self.p.session_id = Some(id.to_string());
                }
                let ok = str_at(r, "status") == Some("SUCCESS");
                self.p.result_ok = Some(ok);
                if !ok {
                    let msg = ["error", "message", "status"]
                        .iter()
                        .find_map(|k| str_at(r, k))
                        .unwrap_or("error result");
                    self.error(msg);
                    if is_quota(&r.to_string()) {
                        self.quota_hint = true;
                    }
                }
            }
            Some("error") => {
                let msg = ["message", "error"]
                    .iter()
                    .find_map(|k| str_at(v, k))
                    .unwrap_or("error");
                self.error(msg);
            }
            _ => {}
        }
    }

    fn error(&mut self, msg: &str) {
        if is_quota(msg) {
            self.quota_hint = true;
        }
        let msg: String = msg.chars().take(500).collect();
        self.p.error = Some(msg);
    }

    /// The verdict once the process ended (`exited_ok`: exit status 0). A quota refusal is
    /// known only from the runtime's structured output: an error event or result and its
    /// status, never from free text on stderr, where a panic at `lib.rs:429` or a word in
    /// a log line would pass for one.
    pub fn finish(mut self, exited_ok: bool, _stderr_tail: &str) -> Parsed {
        let success = exited_ok && self.p.result_ok == Some(true);
        if !success {
            self.p.quota = self.quota_hint;
        }
        self.p
    }
}

impl Parsed {
    pub fn success(&self, exited_ok: bool) -> bool {
        exited_ok && self.result_ok == Some(true)
    }
}

fn str_at<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// Quota and rate-limit wording of the three CLIs and their providers, matched in the
/// message of a structured error only (an error event or an error result). No bare status
/// code or single word: those occur in ordinary errors.
pub fn is_quota(text: &str) -> bool {
    const PATTERNS: [&str; 12] = [
        "usage limit",
        "rate limit",
        "rate_limit",
        "quota exceeded",
        "exceeded your current quota",
        "insufficient_quota",
        "resource_exhausted",
        "resource exhausted",
        "too many requests",
        "hit your limit",
        "out of credits",
        "credit balance is too low",
    ];
    let t = text.to_lowercase();
    PATTERNS.iter().any(|p| t.contains(p))
}

#[cfg(test)]
mod tests {
    //! Fixtures: `*-ok.jsonl` are real captures of one `reply with OK` turn of each CLI
    //! (claude 2.1.332, codex-cli 0.155.1, agy 1.5.18), trimmed, ids and paths replaced.
    //! `*.synthetic.jsonl` are hand-written in the same shapes; no real refusal was triggered.
    //! `*-crash.synthetic.jsonl` are ordinary failures that mention 429 or quota.
    use super::*;
    use hgl_ai_proto::Family;

    const CLAUDE_OK: &str = include_str!("../fixtures/claude-ok.jsonl");
    const CODEX_OK: &str = include_str!("../fixtures/codex-ok.jsonl");
    const AGY_OK: &str = include_str!("../fixtures/agy-ok.jsonl");
    const CLAUDE_QUOTA: &str = include_str!("../fixtures/claude-quota.synthetic.jsonl");
    const CODEX_QUOTA: &str = include_str!("../fixtures/codex-quota.synthetic.jsonl");
    const AGY_QUOTA: &str = include_str!("../fixtures/agy-quota.synthetic.jsonl");
    const CODEX_CRASH: &str = include_str!("../fixtures/codex-crash.synthetic.jsonl");
    const CLAUDE_CRASH: &str = include_str!("../fixtures/claude-crash.synthetic.jsonl");
    const AGY_CRASH: &str = include_str!("../fixtures/agy-crash.synthetic.jsonl");

    fn parse(rt: Runtime, text: &str, exited_ok: bool, stderr: &str) -> Parsed {
        let mut p = Parser::new(rt);
        text.lines().for_each(|l| p.line(l));
        p.finish(exited_ok, stderr)
    }

    fn strs(c: &CmdSpec) -> Vec<String> {
        c.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn real_successes() {
        let c = parse(Runtime::Claude, CLAUDE_OK, true, "");
        assert_eq!(
            c.session_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );
        assert!(c.success(true) && !c.quota);
        let x = parse(
            Runtime::Codex,
            CODEX_OK,
            true,
            "Reading additional input from stdin...\n",
        );
        assert_eq!(
            x.session_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000002")
        );
        assert!(x.success(true) && !x.quota);
        let a = parse(Runtime::Agy, AGY_OK, true, "");
        assert_eq!(
            a.session_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000003")
        );
        assert!(a.success(true) && !a.quota);
        assert!(!a.success(false), "a non-zero exit is never a success");
    }

    #[test]
    fn quota_refusals() {
        for (rt, text) in [
            (Runtime::Claude, CLAUDE_QUOTA),
            (Runtime::Codex, CODEX_QUOTA),
            (Runtime::Agy, AGY_QUOTA),
        ] {
            let p = parse(rt, text, false, "");
            assert!(p.quota, "{rt}");
            assert!(!p.success(false));
            assert!(p.session_id.is_some());
            assert!(p.error.is_some());
        }
        // A rate-limit warning on a turn that then succeeded is not a refusal.
        let warned = format!(
            "{}\n{CLAUDE_OK}",
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}"#
        );
        assert!(!parse(Runtime::Claude, &warned, true, "").quota);
        // Quota wording only on stderr, nothing structured: not a refusal.
        assert!(
            !parse(
                Runtime::Codex,
                "",
                false,
                "ERROR: You've hit your usage limit."
            )
            .quota
        );
    }

    /// Carbon's false positives (hgl-ai PR 1): "429" or "quota" in ordinary failures.
    #[test]
    fn ordinary_failures_that_mention_429_or_quota_are_not_quota() {
        for (rt, text) in [
            (Runtime::Codex, CODEX_CRASH),
            (Runtime::Claude, CLAUDE_CRASH),
            (Runtime::Agy, AGY_CRASH),
        ] {
            let p = parse(
                rt,
                text,
                false,
                "thread 'main' panicked at src/lib.rs:429:5\nquota.rs: test failed; commit 8a5c429f",
            );
            assert!(!p.quota, "{rt}: {p:?}");
            assert!(p.error.is_some(), "{rt}");
        }
        assert!(!is_quota("commit 8a5c429f failed"));
        assert!(!is_quota("thread 'main' panicked at src/lib.rs:429:5"));
        assert!(!is_quota("disk quota.rs check failed"));
        assert!(is_quota("RESOURCE_EXHAUSTED: Quota exceeded for model"));
        assert!(is_quota(
            "You exceeded your current quota, please check your plan"
        ));
    }

    #[test]
    fn plain_failures_are_not_quota() {
        let crash = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"s"}"#;
        let p = parse(Runtime::Claude, crash, false, "panic: index out of range");
        assert!(!p.quota && !p.success(false));
        let p = parse(
            Runtime::Codex,
            r#"{"type":"turn.failed","error":{"message":"stream disconnected"}}"#,
            false,
            "",
        );
        assert!(!p.quota);
        assert_eq!(p.error.as_deref(), Some("stream disconnected"));
        let p = parse(Runtime::Agy, "", true, "");
        assert!(!p.success(true), "no result line, no success");
    }

    fn resolved(runtime: Runtime, family: Family, model: &str, effort: Option<&str>) -> Resolved {
        Resolved {
            runtime,
            family,
            model: model.into(),
            effort: effort.map(String::from),
        }
    }

    #[test]
    fn claude_arguments() {
        let w = [PathBuf::from("/w/w1/r-1"), PathBuf::from("/w/clones/r")];
        let r = resolved(
            Runtime::Claude,
            Family::Claude,
            "claude-opus-5-5",
            Some("high"),
        );
        let t = TurnInput {
            prompt: "PROMPT",
            resolved: &r,
            resume: Some("S1"),
            cwd: Path::new("/w/w1/r-1/src"),
            writable: &w,
        };
        let c = command(Path::new("claude"), &t);
        assert_eq!(c.input, b"PROMPT");
        assert_eq!(
            strs(&c),
            [
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "claude-opus-5-5",
                "--effort",
                "high",
                "--resume",
                "S1",
                "--permission-mode",
                "bypassPermissions",
                "--add-dir",
                "/w/w1/r-1",
                "--add-dir",
                "/w/clones/r",
            ]
        );
        assert_eq!(c.cwd, PathBuf::from("/w/w1/r-1/src"));
        let plain = resolved(Runtime::Claude, Family::Claude, "claude-opus-5-5", None);
        let fresh = TurnInput {
            resume: None,
            resolved: &plain,
            ..t
        };
        assert!(!strs(&command(Path::new("claude"), &fresh)).contains(&"--resume".to_string()));
    }

    #[test]
    fn codex_arguments() {
        let w = [PathBuf::from("/w/w1/r-1"), PathBuf::from("/w/clones/r")];
        let r = resolved(Runtime::Codex, Family::Gpt, "gpt-6.1-sol", Some("high"));
        let t = TurnInput {
            prompt: "PROMPT",
            resolved: &r,
            resume: None,
            cwd: Path::new("/w/w1/r-1/src"),
            writable: &w,
        };
        let profile = [
            "-c",
            "default_permissions=\"hgl_task\"",
            "-c",
            "permissions.hgl_task.filesystem={\":root\"=\"read\",\"/w/w1/r-1\"=\"write\",\"/w/clones/r\"=\"write\"}",
            "-c",
            "permissions.hgl_task.network.enabled=true",
        ];
        let c = command(Path::new("codex"), &t);
        assert_eq!(c.input, b"PROMPT");
        let fresh = strs(&c);
        let want: Vec<&str> = [
            "exec",
            "--json",
            "--strict-config",
            "-m",
            "gpt-6.1-sol",
            "-c",
            "model_reasoning_effort=high",
        ]
        .into_iter()
        .chain(profile)
        .chain(["--skip-git-repo-check", "-C", "/w/w1/r-1/src", "-"])
        .collect();
        assert_eq!(fresh, want);
        let plain = resolved(Runtime::Codex, Family::Gpt, "gpt-6.1-sol", None);
        let r = TurnInput {
            resume: Some("T1"),
            resolved: &plain,
            ..t
        };
        let want: Vec<&str> = [
            "exec",
            "resume",
            "--json",
            "--strict-config",
            "-m",
            "gpt-6.1-sol",
        ]
        .into_iter()
        .chain(profile)
        .chain(["--skip-git-repo-check", "T1", "-"])
        .collect();
        let resumed = strs(&command(Path::new("codex"), &r));
        assert_eq!(resumed, want);
        // The legacy policy protects a worktree's gitdir (hgl-ai#29); it must not come back.
        for a in fresh.iter().chain(&resumed) {
            assert!(
                !a.contains("sandbox") && !a.contains("writable_roots"),
                "{a}"
            );
        }
    }

    #[test]
    fn codex_profile_quotes_paths() {
        let w = [PathBuf::from("/a b/\"q\"\\x")];
        assert_eq!(
            codex_profile(&w)[1],
            r#"permissions.hgl_task.filesystem={":root"="read","/a b/\"q\"\\x"="write"}"#
        );
    }

    /// Live: a task tree's git writes under the profile a turn gets, through
    /// `codex sandbox` (no model call). Needs `codex` on `PATH` (or `HGL_AI_CODEX`) and
    /// `git`; run by hand: `cargo test -p hgl-ai-bridge codex_profile_live -- --ignored`.
    /// `codex sandbox` cannot apply the legacy `workspace-write` policy, so this does not
    /// show the old failure; for that, run one `codex exec` turn in a worktree with the
    /// arguments of [`command`], asking it to `git commit`: the legacy arguments give
    /// `index.lock: Read-only file system`, these commit.
    #[test]
    #[ignore = "needs codex and git; run by hand"]
    fn codex_profile_live() {
        use std::process::Command;
        let run = |c: &mut Command| {
            let o = c.output().expect("spawn");
            assert!(
                o.status.success(),
                "{c:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        let base = std::env::temp_dir().join(format!("codex-profile-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (task, clone, outside) = (
            base.join("w1/r-1"),
            base.join("clones/r"),
            base.join("outside"),
        );
        std::fs::create_dir_all(&task).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let git = |dir: &Path| {
            let mut c = Command::new("git");
            c.arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"]);
            c
        };
        run(Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&clone));
        let seed = base.join("seed");
        run(Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&seed));
        run(git(&seed).args(["commit", "-q", "--allow-empty", "-m", "seed"]));
        run(git(&seed).arg("push").arg("-q").arg(&clone).arg("main"));
        let src = task.join("src");
        run(git(&clone)
            .args(["worktree", "add", "-q", "-b", "t"])
            .arg(&src)
            .arg("main"));
        run(git(&src).args(["remote", "add", "origin"]).arg(&clone));

        let program = std::env::var_os("HGL_AI_CODEX").unwrap_or_else(|| "codex".into());
        let mut c = Command::new(program);
        c.args(["sandbox", "-P", CODEX_PROFILE, "-C"]).arg(&src);
        for o in codex_profile(&[task.clone(), clone.clone()]) {
            c.arg("-c").arg(o);
        }
        let script = format!(
            "echo x > f && git add f && git -c user.name=t -c user.email=t@t commit -qm t \
             && git push -q origin HEAD:refs/heads/t && echo GIT-OK; \
             touch '{}/f' 2>/dev/null && echo OUTSIDE-WRITTEN; true",
            outside.display()
        );
        let out = run(c.args(["--", "sh", "-c", &script]));
        assert!(out.contains("GIT-OK"), "{out}");
        assert!(!out.contains("OUTSIDE-WRITTEN"), "{out}");
        assert!(!outside.join("f").exists());
        let pushed = run(git(&clone).args(["log", "--format=%s", "-1", "t"]));
        assert_eq!(pushed.trim(), "t");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn agy_arguments() {
        let w = [PathBuf::from("/w/w1/r-1")];
        let r = resolved(
            Runtime::Agy,
            Family::Gemini,
            "gemini-4-carbon",
            Some("high"),
        );
        let prompt = "PROMPT line 1\n\"quoted\" \\ é\n";
        let t = TurnInput {
            prompt,
            resolved: &r,
            resume: Some("C1"),
            cwd: Path::new("/x"),
            writable: &w,
        };
        let c = command(Path::new("agy"), &t);
        let line = std::str::from_utf8(&c.input).unwrap();
        assert!(
            line.ends_with('\n') && line.lines().count() == 1,
            "{line:?}"
        );
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["event"], "user");
        assert_eq!(v["message"]["content"], prompt);
        assert_eq!(
            strs(&c),
            [
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--model",
                "gemini-4-carbon-high",
                "--effort",
                "high",
                "--conversation",
                "C1",
                "--dangerously-skip-permissions",
                "--add-dir",
                "/w/w1/r-1",
            ]
        );
        let pinned = resolved(Runtime::Agy, Family::Claude, "claude-opus-5-5-high", None);
        let t = TurnInput {
            resolved: &pinned,
            resume: None,
            ..t
        };
        let a = strs(&command(Path::new("agy"), &t));
        assert_eq!(a[4..6], ["--model", "claude-opus-5-5-high"]);
        assert!(!a.iter().any(|x| x.contains("PROMPT")), "never an argument");
        assert!(!a.contains(&"--effort".to_string()));
    }
}
