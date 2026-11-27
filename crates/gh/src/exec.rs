//! Running `gh`. The trait is the only effect the GitHub logic has; [`ReadOnly`] wraps any
//! runner so that a dry run cannot change GitHub.

use std::future::Future;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::{Error, Result};

/// What one `gh` run returned.
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `gh` with arguments and optional standard input.
pub trait Gh: Sync {
    fn exec(
        &self,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
    ) -> impl Future<Output = Result<Output>> + Send;
}

/// The real `gh` from `PATH`, each run bounded in time (the scripts used `timeout 60`).
#[derive(Debug, Clone)]
pub struct GhCli {
    pub program: String,
    pub timeout: Duration,
}

impl Default for GhCli {
    fn default() -> Self {
        GhCli {
            program: "gh".into(),
            timeout: Duration::from_secs(90),
        }
    }
}

impl Gh for GhCli {
    async fn exec(&self, args: Vec<String>, stdin: Option<Vec<u8>>) -> Result<Output> {
        let shown = args.join(" ");
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1")
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| Error::Command {
            args: shown.clone(),
            msg: e.to_string(),
        })?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(&input).await.map_err(|e| Error::Command {
                args: shown.clone(),
                msg: e.to_string(),
            })?;
        }
        let out = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| Error::Command {
                args: shown.clone(),
                msg: format!("no answer in {:?}", self.timeout),
            })?
            .map_err(|e| Error::Command {
                args: shown.clone(),
                msg: e.to_string(),
            })?;
        Ok(Output {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// Refuses every run that could change GitHub: `gh api` with a method other than `GET`
/// (a GraphQL query is allowed, a mutation is not) and every other subcommand except
/// downloads and `run view`.
#[derive(Debug, Clone)]
pub struct ReadOnly<G>(pub G);

/// Whether these `gh` arguments only read.
pub fn is_read_only(args: &[String]) -> bool {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.first() {
        Some(&"api") => {
            let mut method = None;
            let mut has_fields = false;
            let mut i = 1;
            while i < a.len() {
                match a[i] {
                    "-X" | "--method" => {
                        method = a.get(i + 1).map(|m| m.to_ascii_uppercase());
                        i += 1;
                    }
                    "-f" | "-F" | "--field" | "--raw-field" | "--input" => {
                        has_fields = true;
                        i += 1;
                    }
                    s if s.starts_with("--method=") => method = Some(s[9..].to_ascii_uppercase()),
                    _ => {}
                }
                i += 1;
            }
            if a.get(1) == Some(&"graphql") {
                let query = a
                    .iter()
                    .find_map(|s| s.strip_prefix("query="))
                    .unwrap_or("");
                return !query.trim_start().starts_with("mutation");
            }
            match method.as_deref() {
                Some("GET") => true,
                Some(_) => false,
                None => !has_fields,
            }
        }
        Some(&"auth") => a.get(1) == Some(&"status"),
        Some(&"run") => matches!(a.get(1), Some(&"download") | Some(&"view") | Some(&"list")),
        Some(&"release") => matches!(a.get(1), Some(&"download") | Some(&"view") | Some(&"list")),
        _ => false,
    }
}

impl<G: Gh> Gh for ReadOnly<G> {
    async fn exec(&self, args: Vec<String>, stdin: Option<Vec<u8>>) -> Result<Output> {
        if !is_read_only(&args) {
            return Err(Error::Invalid(format!(
                "dry run: refused `gh {}`",
                args.join(" ")
            )));
        }
        self.0.exec(args, stdin).await
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn first_lines(t: &str) -> String {
    t.lines()
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Whether a failed run's message is GitHub's rate limit (primary or secondary).
pub fn is_rate_limit_text(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    t.contains("rate limit") || t.contains("rate_limited") || t.contains("submitted too quickly")
}

/// Whether a failed run's message says the credentials are missing or refused.
pub fn is_auth_text(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    t.contains("http 401") || t.contains("bad credentials") || t.contains("gh auth login")
}

/// Fail clearly unless `gh` runs and is signed in to github.com.
pub async fn check_auth<G: Gh>(gh: &G) -> Result<()> {
    let args = s(&["auth", "status", "--hostname", "github.com"]);
    let out = match gh.exec(args, None).await {
        Ok(o) => o,
        Err(e) => {
            return Err(Error::Auth(format!(
                "cannot run the GitHub CLI `gh` ({e}); install it and run `gh auth login`"
            )));
        }
    };
    if out.ok {
        return Ok(());
    }
    let why = first_lines(&format!("{}\n{}", out.stderr, out.stdout));
    Err(Error::Auth(format!(
        "`gh` is not signed in to github.com for this user ({why}); run `gh auth login` or set GH_TOKEN"
    )))
}

/// Run and return standard output, failing on a non-zero exit.
pub async fn text<G: Gh>(gh: &G, args: Vec<String>) -> Result<String> {
    text_in(gh, args, None).await
}

async fn text_in<G: Gh>(gh: &G, args: Vec<String>, stdin: Option<Vec<u8>>) -> Result<String> {
    let shown = args.join(" ");
    let out = gh.exec(args, stdin).await?;
    if out.ok {
        return Ok(out.stdout);
    }
    let msg = first_lines(&format!("{}\n{}", out.stderr, out.stdout));
    if is_rate_limit_text(&msg) {
        return Err(Error::RateLimited(msg));
    }
    if is_auth_text(&msg) {
        return Err(Error::Auth(format!(
            "`gh {shown}`: {msg}; run `gh auth login` or set GH_TOKEN"
        )));
    }
    Err(Error::Command { args: shown, msg })
}

/// Parse a sequence of JSON values, as `gh api --paginate` prints them (`[..][..]`).
pub fn parse_stream(what: &str, text: &str) -> Result<Vec<Value>> {
    serde_json::Deserializer::from_str(text)
        .into_iter::<Value>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Response {
            what: what.into(),
            msg: e.to_string(),
        })
}

/// Concatenate the arrays of a paginated list.
pub fn flatten(pages: Vec<Value>) -> Vec<Value> {
    pages
        .into_iter()
        .flat_map(|p| match p {
            Value::Array(v) => v,
            other => vec![other],
        })
        .collect()
}

/// `gh api PATH` (GET) as JSON.
pub async fn get<G: Gh>(gh: &G, path: &str) -> Result<Value> {
    let t = text(gh, s(&["api", path])).await?;
    serde_json::from_str(&t).map_err(|e| Error::Response {
        what: path.into(),
        msg: e.to_string(),
    })
}

/// `gh api PATH` (GET) as text with an `Accept` header, for diffs and raw files.
pub async fn get_raw<G: Gh>(gh: &G, path: &str, accept: &str) -> Result<String> {
    text(gh, s(&["api", path, "-H", &format!("Accept: {accept}")])).await
}

/// `gh api --paginate PATH` (GET): every element of every page.
pub async fn get_list<G: Gh>(gh: &G, path: &str) -> Result<Vec<Value>> {
    Ok(flatten(get_pages(gh, path).await?))
}

/// `gh api --paginate PATH` (GET): each page as returned.
pub async fn get_pages<G: Gh>(gh: &G, path: &str) -> Result<Vec<Value>> {
    let t = text(gh, s(&["api", "--paginate", path])).await?;
    parse_stream(path, &t)
}

/// A request that changes something, with string fields (`-f`).
pub async fn write<G: Gh>(
    gh: &G,
    method: &str,
    path: &str,
    fields: &[(&str, &str)],
) -> Result<Value> {
    let mut args = s(&["api", "-X", method, path]);
    for (k, v) in fields {
        args.push("-f".into());
        args.push(format!("{k}={v}"));
    }
    let t = text(gh, args).await?;
    if t.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&t).map_err(|e| Error::Response {
        what: path.into(),
        msg: e.to_string(),
    })
}

/// A request that changes something, with a JSON body.
pub async fn write_json<G: Gh>(gh: &G, method: &str, path: &str, body: &Value) -> Result<Value> {
    let args = s(&["api", "-X", method, path, "--input", "-"]);
    let t = text_in(gh, args, Some(body.to_string().into_bytes())).await?;
    serde_json::from_str(&t).map_err(|e| Error::Response {
        what: path.into(),
        msg: e.to_string(),
    })
}

/// A GraphQL query with string variables. With `paginate`, the query takes `$endCursor`
/// and every page is returned. A response that carries `errors` fails, as a rate limit
/// where GitHub says so.
pub async fn graphql<G: Gh>(
    gh: &G,
    query: &str,
    vars: &[(&str, String)],
    paginate: bool,
) -> Result<Vec<Value>> {
    let mut args = s(&["api", "graphql"]);
    if paginate {
        args.push("--paginate".into());
    }
    args.push("-f".into());
    args.push(format!("query={query}"));
    for (k, v) in vars {
        args.push("-f".into());
        args.push(format!("{k}={v}"));
    }
    let out = gh.exec(args, None).await?;
    let pages = parse_stream("graphql", &out.stdout).unwrap_or_default();
    let errors: Vec<&Value> = pages.iter().filter_map(|p| p.get("errors")).collect();
    let rate_limited = errors
        .iter()
        .any(|e| e.to_string().contains("RATE_LIMITED"));
    if rate_limited || (!out.ok && is_rate_limit_text(&out.stderr)) {
        return Err(Error::RateLimited(first_lines(&format!(
            "{}\n{}",
            out.stderr, out.stdout
        ))));
    }
    if !out.ok && is_auth_text(&out.stderr) {
        return Err(Error::Auth(format!(
            "GraphQL: {}; run `gh auth login` or set GH_TOKEN",
            first_lines(&out.stderr)
        )));
    }
    if !out.ok || !errors.is_empty() || pages.is_empty() {
        let msg = if errors.is_empty() {
            first_lines(&out.stderr)
        } else {
            first_lines(&errors[0].to_string())
        };
        return Err(Error::Response {
            what: "graphql".into(),
            msg,
        });
    }
    Ok(pages)
}

#[cfg(test)]
pub(crate) mod fake {
    //! A scripted `gh` for orchestration tests: each run is matched by a substring of its
    //! joined arguments, in order of the rules.
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    pub struct FakeGh {
        /// (pattern, output, used once)
        pub rules: Mutex<Vec<(String, Output, bool)>>,
        pub calls: Mutex<Vec<String>>,
    }

    impl FakeGh {
        fn push(self, pattern: &str, out: Output, once: bool) -> Self {
            self.rules.lock().unwrap().push((pattern.into(), out, once));
            self
        }
        pub fn on(self, pattern: &str, stdout: &str) -> Self {
            self.push(
                pattern,
                Output {
                    ok: true,
                    stdout: stdout.into(),
                    stderr: String::new(),
                },
                false,
            )
        }
        /// Answers the first matching run only.
        pub fn once(self, pattern: &str, stdout: &str) -> Self {
            self.push(
                pattern,
                Output {
                    ok: true,
                    stdout: stdout.into(),
                    stderr: String::new(),
                },
                true,
            )
        }
        pub fn fail(self, pattern: &str, stderr: &str) -> Self {
            self.push(
                pattern,
                Output {
                    ok: false,
                    stdout: String::new(),
                    stderr: stderr.into(),
                },
                false,
            )
        }
        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Gh for FakeGh {
        async fn exec(&self, args: Vec<String>, _stdin: Option<Vec<u8>>) -> Result<Output> {
            let joined = args.join(" ");
            self.calls.lock().unwrap().push(joined.clone());
            let mut rules = self.rules.lock().unwrap();
            match rules
                .iter()
                .position(|(p, _, _)| joined.contains(p.as_str()))
            {
                Some(i) => {
                    let out = rules[i].1.clone();
                    if rules[i].2 {
                        rules.remove(i);
                    }
                    Ok(out)
                }
                None => Err(Error::Command {
                    args: joined,
                    msg: "no fake rule".into(),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &str) -> Vec<String> {
        v.split(' ').map(String::from).collect()
    }

    #[test]
    fn read_only_guard() {
        assert!(is_read_only(&a("api repos/o/r/pulls/1")));
        assert!(is_read_only(&a(
            "api --paginate repos/o/r/issues/1/comments"
        )));
        assert!(is_read_only(&a("api -X GET search/issues -f q=x")));
        assert!(!is_read_only(&a(
            "api -X PUT repos/o/r/pulls/1/merge -f sha=x"
        )));
        assert!(
            !is_read_only(&a("api repos/o/r/issues/1/comments -f body=x")),
            "fields imply POST"
        );
        assert!(!is_read_only(&a("api --method=POST repos/o/r/git/refs")));
        assert!(is_read_only(&[
            "api".into(),
            "graphql".into(),
            "-f".into(),
            "query=query{viewer{login}}".into()
        ]));
        assert!(!is_read_only(&[
            "api".into(),
            "graphql".into(),
            "-f".into(),
            "query=mutation{x}".into()
        ]));
        assert!(!is_read_only(&a("workflow run x.yml")));
        assert!(!is_read_only(&a("pr comment 1")));
        assert!(is_read_only(&a("run download 5")));
        assert!(is_read_only(&a("release download 1.0")));
    }

    #[test]
    fn pages_and_rate_limits() {
        let v = parse_stream("x", "[1,2][3]\n").unwrap();
        assert_eq!(
            flatten(v),
            vec![Value::from(1), Value::from(2), Value::from(3)]
        );
        assert!(parse_stream("x", "[1,").is_err());
        assert!(is_rate_limit_text(
            "gh: API rate limit exceeded for user ID 1. (HTTP 403)"
        ));
        assert!(is_rate_limit_text("{\"type\":\"RATE_LIMITED\"}"));
        assert!(!is_rate_limit_text("HTTP 404: Not Found"));
        assert!(is_auth_text("gh: Bad credentials (HTTP 401)"));
        assert!(is_auth_text(
            "To get started with GitHub CLI, please run:  gh auth login"
        ));
        assert!(!is_auth_text("HTTP 404: Not Found"));
        assert!(is_read_only(&a("auth status --hostname github.com")));
        assert!(!is_read_only(&a("auth login")));
    }

    #[tokio::test]
    async fn graphql_errors() {
        let gh = fake::FakeGh::default()
            .on(
                "RL",
                r#"{"errors":[{"type":"RATE_LIMITED","message":"API rate limit exceeded"}]}"#,
            )
            .on("OK", r#"{"data":{"x":1}}"#);
        let e = graphql(&gh, "query RL{x}", &[], false).await.unwrap_err();
        assert!(matches!(e, Error::RateLimited(_)), "{e}");
        let ok = graphql(&gh, "query OK{x}", &[("a", "b".into())], false)
            .await
            .unwrap();
        assert_eq!(ok[0]["data"]["x"], 1);
        assert!(gh.calls()[1].contains("-f a=b"));
        let signed_out = fake::FakeGh::default().fail(
            "auth status",
            "You are not logged into any GitHub hosts. To log in, run: gh auth login",
        );
        assert!(
            matches!(check_auth(&signed_out).await, Err(Error::Auth(m)) if m.contains("not signed in"))
        );
        assert!(
            check_auth(&fake::FakeGh::default().on("auth status", ""))
                .await
                .is_ok()
        );
        let ro = ReadOnly(fake::FakeGh::default().on("", "{}"));
        assert!(
            write(&ro, "PUT", "repos/o/r/pulls/1/merge", &[])
                .await
                .is_err()
        );
        assert!(get(&ro, "repos/o/r").await.is_ok());
    }
}
