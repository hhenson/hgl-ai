//! The HTTP channels: an ntfy topic and a Slack incoming webhook.

use std::fmt::Write as _;

use crate::{Notification, Priority, chain, one_line, secret};

/// Bound on the response text kept in a failure reason.
const REASON_TEXT: usize = 200;

/// `POST <url>`: the body is the title and the link; `Title`, `Priority`, `Click` and
/// `Tags` headers; a bearer token from `token_file` for a protected topic.
pub(crate) async fn ntfy(
    http: &reqwest::Client,
    url: &str,
    token_file: Option<&str>,
    n: &Notification,
) -> Result<(), String> {
    let mut req = http
        .post(url)
        .header("Title", header_text(&n.title))
        .header("Priority", ntfy_priority(n.priority))
        .header("Click", header_url(&n.link))
        .header("Tags", "hgl-ai")
        .body(format!("{}\n{}\n", n.title, n.link));
    if let Some(f) = token_file {
        req = req.bearer_auth(secret::read(f).await?);
    }
    accept(req.send().await.map_err(|e| chain(&e.without_url()))?).await
}

fn ntfy_priority(p: Priority) -> &'static str {
    match p {
        Priority::Urgent => "5",
        Priority::High => "4",
        Priority::Normal => "3",
    }
}

/// `POST` `{"text": "<title>\n<link>"}` to the webhook URL held in `webhook_file`. The
/// URL is a secret: no reason or log names it.
pub(crate) async fn slack(
    http: &reqwest::Client,
    webhook_file: &str,
    n: &Notification,
) -> Result<(), String> {
    let url = secret::read(webhook_file).await?;
    match reqwest::Url::parse(&url) {
        Ok(u) if matches!(u.scheme(), "https" | "http") => {}
        _ => return Err(format!("{webhook_file}: not an http(s) URL")),
    }
    let body = serde_json::json!({ "text": format!("{}\n{}", slack_escape(&n.title), n.link) });
    let resp = http
        .post(url)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| chain(&e.without_url()))?;
    accept(resp).await
}

/// Slack's control characters in message text.
pub(crate) fn slack_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A 2xx status accepts; otherwise the status and the start of the response text.
async fn accept(resp: reqwest::Response) -> Result<(), String> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.bytes().await.unwrap_or_default();
    let text: String = one_line(String::from_utf8_lossy(&body).trim())
        .chars()
        .take(REASON_TEXT)
        .collect();
    Err(if text.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {text}")
    })
}

/// `url` fit for a log: no credentials, query or fragment.
pub(crate) fn shown_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => "<invalid url>".into(),
    }
}

/// `s` as a header value: control characters become spaces, other non-ASCII `?`.
pub(crate) fn header_text(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' '..='~' => c,
            c if c.is_control() => ' ',
            _ => '?',
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// `s` as a header value, bytes outside printable ASCII percent-encoded.
pub(crate) fn header_url(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if (0x21..0x7f).contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}
