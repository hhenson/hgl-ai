//! Every channel against an in-process fake on 127.0.0.1; no real network.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use hgl_ai_config::{Notify, SmtpTls};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::*;

const TOKEN: &str = "tok-ntfy-SECRET-7f3a";
const HOOK_PATH: &str = "/services/T0/B0/hook-SECRET-91c2";
const PASSWORD: &str = "pw-smtp-SECRET-55d0";
const LINK: &str = "https://console.example.com/threads/q-42";

fn note(title: &str, priority: Priority) -> Notification {
    Notification {
        title: title.into(),
        priority,
        link: LINK.into(),
    }
}

/// A file of `mode` holding `text`.
fn secret_file(dir: &Path, name: &str, text: &str, mode: u32) -> String {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = mode;
    p.to_str().unwrap().to_string()
}

fn ntfy(url: String, token_file: Option<String>) -> Notify {
    Notify::Ntfy { url, token_file }
}

fn slack(webhook_file: String) -> Notify {
    Notify::Slack { webhook_file }
}

fn email(smtp: String, tls: SmtpTls, password_file: Option<String>) -> Notify {
    Notify::Email {
        smtp,
        tls,
        from: "console@example.com".into(),
        to: vec!["owner@example.com".into(), "second@example.com".into()],
        user: password_file.as_ref().map(|_| "console".into()),
        password_file,
    }
}

fn failures(e: Error) -> Vec<Failure> {
    match e {
        Error::Failed(f) => f,
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// A port nothing listens on.
async fn closed_port() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap()
}

// ---- fake HTTP server ----

#[derive(Debug, Clone)]
struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Records each request and answers `status` with `body`; with `status` 0 it never answers.
struct FakeHttp {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<Request>>>,
    task: JoinHandle<()>,
}

impl FakeHttp {
    async fn start(status: u16, body: &'static str) -> FakeHttp {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn({
            let log = log.clone();
            async move {
                while let Ok((mut s, _)) = listener.accept().await {
                    let Some(req) = read_request(&mut s).await else {
                        continue;
                    };
                    log.lock().unwrap().push(req);
                    if status == 0 {
                        std::future::pending::<()>().await;
                    }
                    let resp = format!(
                        "HTTP/1.1 {status} Fake\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                    let _ = s.shutdown().await;
                }
            }
        });
        FakeHttp { addr, log, task }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn requests(&self) -> Vec<Request> {
        self.log.lock().unwrap().clone()
    }
}

impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(s: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8(buf[..head_end].to_vec()).ok()?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

// ---- fake SMTP server ----

#[derive(Debug, Clone, Default)]
struct Mail {
    ehlo: String,
    /// The decoded AUTH PLAIN response, `\0user\0password`.
    auth: Option<String>,
    from: String,
    rcpt: Vec<String>,
    data: String,
}

/// Enough of RFC 5321 for lettre without TLS. `auth`: offer AUTH PLAIN and accept
/// (`Some(true)`), refuse with 535 (`Some(false)`), or offer none (`None`).
struct FakeSmtp {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<Mail>>>,
    task: JoinHandle<()>,
}

impl FakeSmtp {
    async fn start(auth: Option<bool>) -> FakeSmtp {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn({
            let log = log.clone();
            async move {
                while let Ok((s, _)) = listener.accept().await {
                    if let Some(m) = smtp_session(s, auth).await {
                        log.lock().unwrap().push(m);
                    }
                }
            }
        });
        FakeSmtp { addr, log, task }
    }

    fn smtp(&self) -> String {
        self.addr.to_string()
    }

    fn mails(&self) -> Vec<Mail> {
        self.log.lock().unwrap().clone()
    }
}

impl Drop for FakeSmtp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One session; the mail when DATA completed.
async fn smtp_session(s: TcpStream, auth: Option<bool>) -> Option<Mail> {
    let (r, mut w) = s.into_split();
    let mut r = BufReader::new(r);
    let mut m = Mail::default();
    let mut done = false;
    w.write_all(b"220 fake.example.com ESMTP\r\n").await.ok()?;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).await.ok()? == 0 {
            break;
        }
        let cmd = line.trim_end().to_string();
        let upper = cmd.to_ascii_uppercase();
        let reply: &[u8] = if upper.starts_with("EHLO") {
            m.ehlo = cmd[4..].trim().to_string();
            if auth.is_some() {
                b"250-fake.example.com\r\n250 AUTH PLAIN\r\n"
            } else {
                b"250 fake.example.com\r\n"
            }
        } else if let Some(b64) = upper.strip_prefix("AUTH PLAIN ").map(|_| &cmd[11..]) {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .ok()?;
            m.auth = Some(String::from_utf8(raw).ok()?);
            if auth == Some(true) {
                b"235 2.7.0 accepted\r\n"
            } else {
                b"535 5.7.8 authentication failed\r\n"
            }
        } else if upper.starts_with("MAIL FROM:") {
            m.from = cmd[10..].trim().to_string();
            b"250 ok\r\n"
        } else if upper.starts_with("RCPT TO:") {
            m.rcpt.push(cmd[8..].trim().to_string());
            b"250 ok\r\n"
        } else if upper == "DATA" {
            w.write_all(b"354 go ahead\r\n").await.ok()?;
            loop {
                line.clear();
                if r.read_line(&mut line).await.ok()? == 0 {
                    return None;
                }
                if line == ".\r\n" {
                    break;
                }
                m.data.push_str(&line);
            }
            done = true;
            b"250 queued\r\n"
        } else if upper == "QUIT" {
            let _ = w.write_all(b"221 bye\r\n").await;
            break;
        } else if upper == "RSET" || upper == "NOOP" {
            b"250 ok\r\n"
        } else {
            b"502 not implemented\r\n"
        };
        w.write_all(reply).await.ok()?;
    }
    done.then_some(m)
}

// ---- channels ----

#[tokio::test]
async fn ntfy_request_shape() {
    let dir = tempfile::tempdir().unwrap();
    let token = secret_file(dir.path(), "ntfy.token", &format!("  {TOKEN}\n"), 0o600);
    let fake = FakeHttp::start(200, "{}").await;
    let notifier = Notifier::new(vec![ntfy(fake.url("/hgl-topic"), Some(token))]).unwrap();
    let title = "Merge ü\n& <ok>?";
    assert_eq!(
        notifier.send(&note(title, Priority::Urgent)).await.unwrap(),
        "ntfy"
    );
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/hgl-topic"));
    assert_eq!(r.header("title"), Some("Merge ? & <ok>?"));
    assert_eq!(r.header("priority"), Some("5"));
    assert_eq!(r.header("click"), Some(LINK));
    assert_eq!(r.header("tags"), Some("hgl-ai"));
    assert_eq!(
        r.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(
        String::from_utf8(r.body.clone()).unwrap(),
        format!("{title}\n{LINK}\n")
    );
}

#[tokio::test]
async fn ntfy_priorities_and_no_token() {
    let fake = FakeHttp::start(200, "").await;
    let notifier = Notifier::new(vec![ntfy(fake.url("/t"), None)]).unwrap();
    for p in [Priority::High, Priority::Normal] {
        notifier.send(&note("x", p)).await.unwrap();
    }
    let reqs = fake.requests();
    let got: Vec<_> = reqs.iter().map(|r| r.header("priority").unwrap()).collect();
    assert_eq!(got, ["4", "3"]);
    assert!(reqs.iter().all(|r| r.header("authorization").is_none()));
}

#[tokio::test]
async fn slack_request_shape() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeHttp::start(200, "ok").await;
    let hook = secret_file(
        dir.path(),
        "slack.url",
        &format!("{}\n", fake.url(HOOK_PATH)),
        0o600,
    );
    let notifier = Notifier::new(vec![slack(hook)]).unwrap();
    assert_eq!(
        notifier
            .send(&note("a & b <c>", Priority::High))
            .await
            .unwrap(),
        "slack"
    );
    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", HOOK_PATH));
    assert_eq!(r.header("content-type"), Some("application/json"));
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(
        v,
        serde_json::json!({ "text": format!("a &amp; b &lt;c&gt;\n{LINK}") })
    );
}

#[tokio::test]
async fn email_request_shape() {
    let dir = tempfile::tempdir().unwrap();
    let pw = secret_file(dir.path(), "smtp.pw", &format!("{PASSWORD}\n"), 0o600);
    let fake = FakeSmtp::start(Some(true)).await;
    let notifier = Notifier::new(vec![email(fake.smtp(), SmtpTls::None, Some(pw))]).unwrap();
    assert_eq!(
        notifier
            .send(&note("Question: pick a name", Priority::Urgent))
            .await
            .unwrap(),
        "email"
    );
    let mails = fake.mails();
    assert_eq!(mails.len(), 1);
    let m = &mails[0];
    assert_eq!(
        m.auth.as_deref(),
        Some(format!("\0console\0{PASSWORD}").as_str())
    );
    assert_eq!(m.from, "<console@example.com>");
    assert_eq!(m.rcpt, ["<owner@example.com>", "<second@example.com>"]);
    let (head, body) = m.data.split_once("\r\n\r\n").unwrap();
    assert!(
        head.contains("Subject: Question: pick a name\r\n"),
        "{head}"
    );
    assert!(head.contains("From: console@example.com\r\n"), "{head}");
    assert!(
        head.contains("To: owner@example.com, second@example.com\r\n"),
        "{head}"
    );
    assert!(head.contains("Message-ID: <"), "{head}");
    assert!(head.contains("@example.com>"), "{head}");
    assert_eq!(body, format!("Question: pick a name\r\n\r\n{LINK}\r\n"));
}

#[tokio::test]
async fn email_without_auth() {
    let fake = FakeSmtp::start(None).await;
    let channel = email(
        format!("localhost:{}", fake.addr.port()),
        SmtpTls::None,
        None,
    );
    let notifier = Notifier::new(vec![channel]).unwrap();
    notifier.send(&note("t", Priority::Normal)).await.unwrap();
    let mails = fake.mails();
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].auth, None);
}

#[tokio::test]
async fn starttls_required() {
    let fake = FakeSmtp::start(Some(true)).await;
    let dir = tempfile::tempdir().unwrap();
    let pw = secret_file(dir.path(), "smtp.pw", PASSWORD, 0o600);
    let notifier = Notifier::new(vec![email(fake.smtp(), SmtpTls::Starttls, Some(pw))]).unwrap();
    let f = failures(
        notifier
            .send(&note("t", Priority::Urgent))
            .await
            .unwrap_err(),
    );
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].kind, "email");
    assert!(fake.mails().is_empty(), "no mail without STARTTLS");
}

// ---- order, fallback, refusals ----

#[tokio::test]
async fn first_that_accepts_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let down = FakeHttp::start(500, "boom").await;
    let up = FakeHttp::start(200, "ok").await;
    let hook = secret_file(dir.path(), "slack.url", &up.url(HOOK_PATH), 0o600);
    let smtp = FakeSmtp::start(None).await;
    let notifier = Notifier::new(vec![
        ntfy(down.url("/t"), None),
        slack(hook),
        email(smtp.smtp(), SmtpTls::None, None),
    ])
    .unwrap();
    assert_eq!(
        notifier.send(&note("t", Priority::Urgent)).await.unwrap(),
        "slack"
    );
    assert_eq!(down.requests().len(), 1);
    assert_eq!(up.requests().len(), 1);
    assert!(smtp.mails().is_empty(), "later channels are not tried");
}

#[cfg(unix)]
#[tokio::test]
async fn every_channel_failed() {
    let dir = tempfile::tempdir().unwrap();
    let down = FakeHttp::start(500, "boom").await;
    let unused = FakeHttp::start(200, "ok").await;
    let hook = secret_file(dir.path(), "slack.url", &unused.url(HOOK_PATH), 0o644);
    let notifier = Notifier::new(vec![
        ntfy(down.url("/t"), None),
        slack(hook),
        email("smtp.example.com:25".into(), SmtpTls::None, None),
    ])
    .unwrap();
    let err = notifier
        .send(&note("t", Priority::Urgent))
        .await
        .unwrap_err();
    let text = err.to_string();
    let f = failures(err);
    let kinds: Vec<_> = f.iter().map(|f| f.kind).collect();
    assert_eq!(kinds, ["ntfy", "slack", "email"]);
    assert!(f[0].reason.contains("HTTP 500"), "{}", f[0].reason);
    assert!(f[0].reason.contains("boom"), "{}", f[0].reason);
    assert!(f[1].reason.contains("mode 0644"), "{}", f[1].reason);
    assert!(f[2].reason.contains("loopback"), "{}", f[2].reason);
    assert!(f[2].reason.contains("smtp.example.com"), "{}", f[2].reason);
    for k in ["ntfy: ", "slack: ", "email: "] {
        assert!(text.contains(k), "{text}");
    }
    assert!(unused.requests().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn open_secret_refused() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeHttp::start(200, "").await;
    let token = secret_file(dir.path(), "ntfy.token", TOKEN, 0o644);
    let notifier = Notifier::new(vec![ntfy(fake.url("/t"), Some(token.clone()))]).unwrap();
    let f = failures(
        notifier
            .send(&note("t", Priority::Urgent))
            .await
            .unwrap_err(),
    );
    assert!(f[0].reason.contains(&token), "{}", f[0].reason);
    assert!(f[0].reason.contains("mode 0644"), "{}", f[0].reason);
    assert!(!f[0].reason.contains(TOKEN));
    assert!(fake.requests().is_empty(), "nothing sent");
    for mode in [0o640, 0o620, 0o604] {
        let token = secret_file(dir.path(), "ntfy.token", TOKEN, mode);
        let notifier = Notifier::new(vec![ntfy(fake.url("/t"), Some(token))]).unwrap();
        assert!(notifier.send(&note("t", Priority::Urgent)).await.is_err());
    }
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn bad_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent").to_str().unwrap().to_string();
    let empty = secret_file(dir.path(), "empty", " \n", 0o600);
    let not_url = secret_file(dir.path(), "slack.url", "hook-SECRET-91c2", 0o600);
    let notifier = Notifier::new(vec![slack(missing), slack(empty), slack(not_url)]).unwrap();
    let f = failures(
        notifier
            .send(&note("t", Priority::Urgent))
            .await
            .unwrap_err(),
    );
    assert!(f[1].reason.ends_with(": empty"), "{}", f[1].reason);
    assert!(
        f[2].reason.contains("not an http(s) URL"),
        "{}",
        f[2].reason
    );
    assert!(f.iter().all(|f| !f.reason.contains("SECRET")));
}

#[tokio::test]
async fn half_credentials_refused() {
    let fake = FakeSmtp::start(Some(true)).await;
    let mut channel = email(fake.smtp(), SmtpTls::None, None);
    if let Notify::Email { user, .. } = &mut channel {
        *user = Some("console".into());
    }
    let notifier = Notifier::new(vec![channel]).unwrap();
    let f = failures(
        notifier
            .send(&note("t", Priority::Urgent))
            .await
            .unwrap_err(),
    );
    assert!(f[0].reason.contains("together"), "{}", f[0].reason);
    assert!(fake.mails().is_empty());
}

#[tokio::test]
async fn no_channels() {
    let notifier = Notifier::new(Vec::new()).unwrap();
    assert!(matches!(
        notifier.send(&note("t", Priority::Urgent)).await,
        Err(Error::NoChannels)
    ));
}

#[tokio::test]
async fn attempt_times_out() {
    let hang = FakeHttp::start(0, "").await;
    let up = FakeHttp::start(200, "").await;
    let notifier = Notifier::new(vec![ntfy(hang.url("/t"), None), ntfy(up.url("/t"), None)])
        .unwrap()
        .with_timeout(Duration::from_millis(300));
    assert_eq!(
        notifier.send(&note("t", Priority::Urgent)).await.unwrap(),
        "ntfy"
    );
    assert_eq!(hang.requests().len(), 1);
    assert_eq!(up.requests().len(), 1);
    let notifier = Notifier::new(vec![ntfy(hang.url("/t"), None)])
        .unwrap()
        .with_timeout(Duration::from_millis(300));
    let f = failures(
        notifier
            .send(&note("t", Priority::Urgent))
            .await
            .unwrap_err(),
    );
    assert!(
        f[0].reason.contains("no answer within 300ms"),
        "{}",
        f[0].reason
    );
}

// ---- no secret in Debug, errors or logs ----

thread_local! {
    static LOGS: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Writes into this thread's [`LOGS`]: a `#[tokio::test]` runs on one thread.
struct ThreadLog;

impl std::io::Write for ThreadLog {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        LOGS.with(|l| l.borrow_mut().extend_from_slice(b));
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A global subscriber at TRACE (a scoped one can miss events whose callsite another
/// test thread registered first), writing per thread.
fn capture_logs() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| ThreadLog)
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
    });
    LOGS.with(|l| l.borrow_mut().clear());
}

#[tokio::test]
async fn secrets_stay_out_of_debug_errors_and_logs() {
    capture_logs();
    let dir = tempfile::tempdir().unwrap();
    let down = FakeHttp::start(401, "unauthorized").await;
    let token = secret_file(dir.path(), "ntfy.token", TOKEN, 0o600);
    let ntfy_url = format!("{}?auth=query-SECRET-0b1e", down.url("/topic"));
    let refused = closed_port().await;
    let hook_url = format!("http://{refused}{HOOK_PATH}");
    let hook = secret_file(dir.path(), "slack.url", &hook_url, 0o600);
    let smtp = FakeSmtp::start(Some(false)).await;
    let pw = secret_file(dir.path(), "smtp.pw", PASSWORD, 0o600);
    let notifier = Notifier::new(vec![
        ntfy(ntfy_url, Some(token)),
        slack(hook),
        email(smtp.smtp(), SmtpTls::None, Some(pw)),
    ])
    .unwrap();
    let err = notifier
        .send(&note("t", Priority::Urgent))
        .await
        .unwrap_err();
    assert_eq!(
        down.requests()[0].header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str()),
        "the token was used"
    );
    assert_eq!(
        smtp.mails().len(),
        0,
        "authentication was refused, so no mail"
    );

    let logs = LOGS.with(|l| String::from_utf8(l.borrow().clone()).unwrap());
    let outputs = [
        ("logs", logs.clone()),
        ("error", err.to_string()),
        ("error debug", format!("{err:?}")),
        ("notifier debug", format!("{notifier:?}")),
    ];
    for (what, text) in &outputs {
        for secret in [TOKEN, PASSWORD, "SECRET", &hook_url, HOOK_PATH] {
            assert!(!text.contains(secret), "{what} holds {secret:?}:\n{text}");
        }
    }
    assert_eq!(
        logs.matches("notification channel failed").count(),
        3,
        "{logs}"
    );
    assert!(logs.contains(&down.url("/topic")), "{logs}");
    assert!(logs.contains("HTTP 401"), "{logs}");
    assert!(logs.contains("535"), "{logs}");
}

// ---- helpers ----

#[test]
fn header_values() {
    assert_eq!(http::header_text(" a\r\nb\tc é "), "a  b c ?");
    assert_eq!(
        http::header_url("https://x.example.com/a b/é"),
        "https://x.example.com/a%20b/%C3%A9"
    );
    assert_eq!(http::slack_escape("<&>"), "&lt;&amp;&gt;");
    assert_eq!(
        http::shown_url("https://u:p@ntfy.example.com/topic?auth=x#f"),
        "https://ntfy.example.com/topic"
    );
    assert_eq!(http::shown_url("not a url"), "<invalid url>");
}

#[test]
fn smtp_hosts() {
    assert_eq!(
        email::host_port("smtp.example.com:587").unwrap(),
        ("smtp.example.com", 587)
    );
    assert_eq!(email::host_port("[::1]:25").unwrap(), ("::1", 25));
    for bad in ["smtp.example.com", ":25", "h:0", "h:x"] {
        assert!(email::host_port(bad).is_err(), "{bad}");
    }
    for lo in ["localhost", "LocalHost", "127.0.0.1", "127.1.2.3", "::1"] {
        assert!(email::is_loopback_host(lo), "{lo}");
    }
    for far in [
        "smtp.example.com",
        "192.0.2.1",
        "::ffff:127.0.0.1",
        "localhost.example.com",
    ] {
        assert!(!email::is_loopback_host(far), "{far}");
    }
}
