//! The email channel: one message to every recipient over SMTP.

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hgl_ai_config::SmtpTls;
use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::{Notification, chain, one_line, secret};

/// The fields of a `kind = "email"` channel.
pub(crate) struct Channel<'a> {
    pub smtp: &'a str,
    pub tls: SmtpTls,
    pub from: &'a str,
    pub to: &'a [String],
    pub user: Option<&'a str>,
    pub password_file: Option<&'a str>,
}

/// Subject the title; text body the title and the link. `tls = "none"` only to a
/// loopback relay.
pub(crate) async fn send(
    c: &Channel<'_>,
    n: &Notification,
    timeout: Duration,
) -> Result<(), String> {
    let (host, port) = host_port(c.smtp)?;
    let tls = match c.tls {
        SmtpTls::Starttls => Tls::Required(tls_params(host)?),
        SmtpTls::Tls => Tls::Wrapper(tls_params(host)?),
        SmtpTls::None if is_loopback_host(host) => Tls::None,
        SmtpTls::None => {
            return Err(format!(
                "tls = \"none\" sends in the clear: only to a loopback relay, not {host}"
            ));
        }
    };
    let credentials = match (c.user, c.password_file) {
        (Some(user), Some(file)) => Some(Credentials::new(user.into(), secret::read(file).await?)),
        (None, None) => None,
        _ => return Err("give `user` and `password_file` together".into()),
    };
    let message = message(c.from, c.to, n)?;
    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
        .port(port)
        .tls(tls)
        .timeout(Some(timeout));
    if let Some(credentials) = credentials {
        builder = builder.credentials(credentials);
    }
    builder
        .build()
        .send(message)
        .await
        .map(drop)
        .map_err(|e| chain(&e))
}

fn tls_params(host: &str) -> Result<TlsParameters, String> {
    TlsParameters::new(host.into()).map_err(|e| chain(&e))
}

fn message(from: &str, to: &[String], n: &Notification) -> Result<Message, String> {
    let from: Mailbox = from.parse().map_err(|e| format!("from {from:?}: {e}"))?;
    if to.is_empty() {
        return Err("no `to` address".into());
    }
    let mut b = Message::builder()
        .message_id(Some(message_id(from.email.domain())))
        .from(from)
        .subject(one_line(&n.title))
        .header(ContentType::TEXT_PLAIN);
    for t in to {
        b = b.to(t.parse().map_err(|e| format!("to {t:?}: {e}"))?);
    }
    b.body(format!("{}\n\n{}\n", n.title, n.link))
        .map_err(|e| chain(&e))
}

/// `<time.pid.count@domain>`: unique per message.
fn message_id(domain: &str) -> String {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let count = COUNT.fetch_add(1, Ordering::Relaxed);
    format!(
        "<{nanos:x}.{:x}.{count:x}.hgl-ai@{domain}>",
        std::process::id()
    )
}

/// `host:port`, `[v6]:port` too.
pub(crate) fn host_port(smtp: &str) -> Result<(&str, u16), String> {
    let bad = || format!("smtp {smtp:?}: expected host:port");
    let (host, port) = smtp.rsplit_once(':').ok_or_else(bad)?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let port = port.parse::<u16>().map_err(|_| bad())?;
    if host.is_empty() || port == 0 {
        return Err(bad());
    }
    Ok((host, port))
}

/// `localhost` or a loopback address.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}
