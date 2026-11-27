//! Owner console notifications (docs/owner.md OWN-9).
//!
//! [`Notifier::send`] tries the configured channels in order and stops at the first
//! that accepts: an ntfy topic, a Slack incoming webhook, or email over SMTP. A
//! [`Notification`] is the title, the priority and the link to the thread, never the
//! context.
//!
//! Secrets (the ntfy token, the webhook URL, the SMTP password) are read from their
//! files at send time, trimmed, and never logged or kept. On unix a secret file that
//! group or others may access is refused for its channel. Each attempt has
//! [`ATTEMPT_TIMEOUT`]; each failure is logged with `tracing::warn!`.

use std::time::Duration;

use hgl_ai_config::Notify;

mod email;
mod http;
mod secret;

/// Bound on one channel attempt, the secret read included.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a channel delivers: nothing more than this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub priority: Priority,
    /// The console URL of the thread.
    pub link: String,
}

/// A `p1` question is [`Urgent`](Priority::Urgent); an hourly `p2` batch is
/// [`High`](Priority::High).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Priority {
    Urgent,
    High,
    Normal,
}

/// Why one channel did not accept a notification. Never holds a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// `ntfy`, `slack` or `email`.
    pub kind: &'static str,
    pub reason: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.reason)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no notification channel is configured")]
    NoChannels,
    /// Every channel failed, in the configured order.
    #[error("every notification channel failed: {}", join(.0))]
    Failed(Vec<Failure>),
    #[error("building the HTTP client: {0}")]
    Client(String),
}

fn join(failures: &[Failure]) -> String {
    failures
        .iter()
        .map(Failure::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// The channels of `console.notify` and one HTTP client. Holds no secret.
pub struct Notifier {
    channels: Vec<Notify>,
    http: reqwest::Client,
    timeout: Duration,
}

impl std::fmt::Debug for Notifier {
    // An ntfy URL without its query, which may carry a token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let channels: Vec<String> = self
            .channels
            .iter()
            .map(|c| match c {
                Notify::Ntfy { url, .. } => format!("ntfy {}", http::shown_url(url)),
                Notify::Slack { webhook_file } => format!("slack {webhook_file}"),
                Notify::Email { smtp, .. } => format!("email {smtp}"),
            })
            .collect();
        f.debug_struct("Notifier")
            .field("channels", &channels)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Notifier {
    /// Installs the `ring` rustls provider as the process default unless one is set.
    /// Fails only when the HTTP client cannot be built (on Linux, a system store without
    /// CA certificates). The client follows no redirect.
    pub fn new(channels: Vec<Notify>) -> Result<Notifier, Error> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(concat!("hgl-ai/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::Client(chain(&e.without_url())))?;
        Ok(Notifier {
            channels,
            http,
            timeout: ATTEMPT_TIMEOUT,
        })
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Try each channel in order; the first that accepts is used. Returns its kind
    /// (`ntfy`, `slack`, `email`). Err only when every channel failed (all reasons in
    /// it), or there are no channels.
    pub async fn send(&self, n: &Notification) -> Result<&'static str, Error> {
        if self.channels.is_empty() {
            return Err(Error::NoChannels);
        }
        let mut failures = Vec::new();
        for channel in &self.channels {
            let kind = channel.kind();
            let outcome = tokio::time::timeout(self.timeout, self.attempt(channel, n))
                .await
                .unwrap_or_else(|_| Err(format!("no answer within {:?}", self.timeout)));
            match outcome {
                Ok(()) => {
                    tracing::debug!(channel = kind, "notification sent");
                    return Ok(kind);
                }
                Err(reason) => {
                    tracing::warn!(channel = kind, %reason, "notification channel failed");
                    failures.push(Failure { kind, reason });
                }
            }
        }
        Err(Error::Failed(failures))
    }

    async fn attempt(&self, channel: &Notify, n: &Notification) -> Result<(), String> {
        match channel {
            Notify::Ntfy { url, token_file } => {
                http::ntfy(&self.http, url, token_file.as_deref(), n)
                    .await
                    .map_err(|e| format!("{}: {e}", http::shown_url(url)))
            }
            Notify::Slack { webhook_file } => http::slack(&self.http, webhook_file, n).await,
            Notify::Email {
                smtp,
                tls,
                from,
                to,
                user,
                password_file,
            } => email::send(
                &email::Channel {
                    smtp,
                    tls: *tls,
                    from,
                    to,
                    user: user.as_deref(),
                    password_file: password_file.as_deref(),
                },
                n,
                self.timeout,
            )
            .await
            .map_err(|e| format!("{smtp}: {e}")),
        }
    }
}

/// `e` and its sources, each once.
fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let t = cause.to_string();
        if !s.contains(&t) {
            s.push_str(": ");
            s.push_str(&t);
        }
        source = cause.source();
    }
    s
}

/// `s` on one line: control characters become spaces.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests;
