//! Runtime adapters (docs/design.md §4): the command line of one headless turn, and what
//! its streamed JSON says about the session, success and quota refusals.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

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

/// The agent command line of a headless turn; the caller adds environment and the cap.
pub fn command(program: &Path, t: &TurnInput) -> CmdSpec {
    let mut a: Vec<OsString> = Vec::new();
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
            s(
                &mut a,
                &[
                    "-p",
                    t.prompt,
                    "--output-format",
                    "stream-json",
                    "--verbose",
                ],
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
            s(&mut a, &["--json"]);
            s(&mut a, &models);
            if t.resume.is_some() {
                // `exec resume` takes no `--sandbox` or `-C`; the cwd is the process's.
                s(&mut a, &["-c", "sandbox_mode=\"workspace-write\""]);
            } else {
                s(&mut a, &["--sandbox", "workspace-write"]);
            }
            let roots: Vec<String> = t
                .writable
                .iter()
                .map(|p| toml_str(&p.to_string_lossy()))
                .collect();
            let roots = format!(
                "sandbox_workspace_write.writable_roots=[{}]",
                roots.join(",")
            );
            s(
                &mut a,
                &[
                    "-c",
                    "sandbox_workspace_write.network_access=true",
                    "-c",
                    &roots,
                    "--skip-git-repo-check",
                ],
            );
            match t.resume {
                Some(id) => s(&mut a, &[id]),
                None => {
                    a.push("-C".into());
                    a.push(t.cwd.into());
                }
            }
            a.push(t.prompt.into());
        }
        Runtime::Agy => {
            s(&mut a, &["-p", t.prompt, "--output-format", "stream-json"]);
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
    }
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

/// What a turn's output said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    pub session_id: Option<String>,
    /// The structured result: `Some(true)` success, `Some(false)` an error result, `None` none seen.
    pub result_ok: Option<bool>,
    /// The turn was refused for quota or rate limits (only when it did not succeed).
    pub quota: bool,
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
            return;
        };
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

    /// The verdict once the process ended (`exited_ok`: exit status 0).
    pub fn finish(mut self, exited_ok: bool, stderr_tail: &str) -> Parsed {
        let success = exited_ok && self.p.result_ok == Some(true);
        if !success {
            if is_quota(stderr_tail) {
                self.quota_hint = true;
            }
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

/// Quota and rate-limit wording of the three CLIs and their providers.
pub fn is_quota(text: &str) -> bool {
    const PATTERNS: [&str; 12] = [
        "usage limit",
        "rate limit",
        "rate_limit",
        "quota",
        "resource_exhausted",
        "resource exhausted",
        "too many requests",
        "hit your limit",
        "limit reached",
        "out of credits",
        "credit balance is too low",
        "429",
    ];
    let t = text.to_lowercase();
    PATTERNS.iter().any(|p| t.contains(p))
}

#[cfg(test)]
mod tests {
    //! Fixtures: `*-ok.jsonl` are real captures of one `reply with OK` turn of each CLI
    //! (claude 2.1.332, codex-cli 0.155.1, agy 1.5.18), trimmed, ids and paths replaced.
    //! `*.synthetic.jsonl` are hand-written in the same shapes; no real refusal was triggered.
    use super::*;
    use hgl_ai_proto::Family;

    const CLAUDE_OK: &str = include_str!("../fixtures/claude-ok.jsonl");
    const CODEX_OK: &str = include_str!("../fixtures/codex-ok.jsonl");
    const AGY_OK: &str = include_str!("../fixtures/agy-ok.jsonl");
    const CLAUDE_QUOTA: &str = include_str!("../fixtures/claude-quota.synthetic.jsonl");
    const CODEX_QUOTA: &str = include_str!("../fixtures/codex-quota.synthetic.jsonl");
    const AGY_QUOTA: &str = include_str!("../fixtures/agy-quota.synthetic.jsonl");

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
        // Quota wording only on stderr, nothing structured.
        assert!(
            parse(
                Runtime::Codex,
                "",
                false,
                "ERROR: You've hit your usage limit."
            )
            .quota
        );
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
        assert_eq!(
            strs(&c),
            [
                "-p",
                "PROMPT",
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
        let w = [PathBuf::from("/w/w1/r-1")];
        let r = resolved(Runtime::Codex, Family::Gpt, "gpt-6.1-sol", Some("high"));
        let t = TurnInput {
            prompt: "PROMPT",
            resolved: &r,
            resume: None,
            cwd: Path::new("/w/w1/r-1/src"),
            writable: &w,
        };
        assert_eq!(
            strs(&command(Path::new("codex"), &t)),
            [
                "exec",
                "--json",
                "-m",
                "gpt-6.1-sol",
                "-c",
                "model_reasoning_effort=high",
                "--sandbox",
                "workspace-write",
                "-c",
                "sandbox_workspace_write.network_access=true",
                "-c",
                "sandbox_workspace_write.writable_roots=[\"/w/w1/r-1\"]",
                "--skip-git-repo-check",
                "-C",
                "/w/w1/r-1/src",
                "PROMPT",
            ]
        );
        let plain = resolved(Runtime::Codex, Family::Gpt, "gpt-6.1-sol", None);
        let r = TurnInput {
            resume: Some("T1"),
            resolved: &plain,
            ..t
        };
        assert_eq!(
            strs(&command(Path::new("codex"), &r)),
            [
                "exec",
                "resume",
                "--json",
                "-m",
                "gpt-6.1-sol",
                "-c",
                "sandbox_mode=\"workspace-write\"",
                "-c",
                "sandbox_workspace_write.network_access=true",
                "-c",
                "sandbox_workspace_write.writable_roots=[\"/w/w1/r-1\"]",
                "--skip-git-repo-check",
                "T1",
                "PROMPT",
            ]
        );
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
        let t = TurnInput {
            prompt: "PROMPT",
            resolved: &r,
            resume: Some("C1"),
            cwd: Path::new("/x"),
            writable: &w,
        };
        assert_eq!(
            strs(&command(Path::new("agy"), &t)),
            [
                "-p",
                "PROMPT",
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
        assert!(!a.contains(&"--effort".to_string()));
    }
}
