//! `[console]`: the owner's web console (docs/owner.md, OWN-6, OWN-9). Only the hub host
//! that runs `hgl-ai console run` has this section. Secrets live in files of mode 0600
//! beside the configuration; this file holds only their paths. The console's hub
//! credential (role `console`) reaches it through `HGL_AI_HUB_CREDENTIAL`, which its
//! service sets.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::parse_duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Console {
    /// `ADDR:PORT` the console listens on: loopback or a tailnet address, never a public
    /// interface. Without `tls_cert`, only a loopback address (behind a local TLS proxy).
    pub listen: String,
    /// The origin browsers use, `https://<stable name>[:port]`: passkeys are bound to it.
    pub url: String,
    /// PEM certificate chain and key for the stable name; omit both behind a TLS proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key: Option<String>,
    /// Who may sign in: GitHub logins and Apple account emails.
    #[serde(default)]
    pub users: Vec<String>,
    /// How often the controller gets a `tick` note for its status report (OWN-1).
    #[serde(default = "status_every", skip_serializing_if = "is_status_every")]
    pub status_every: String,
    /// How long a sign-in lasts.
    #[serde(default = "session_ttl", skip_serializing_if = "is_session_ttl")]
    pub session_ttl: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<GithubOAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apple: Option<AppleOAuth>,
    /// Notification channels, tried in this order; the first that accepts is used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify: Vec<Notify>,
}

pub fn status_every() -> String {
    "2h".into()
}
fn is_status_every(s: &str) -> bool {
    s == status_every()
}
pub fn session_ttl() -> String {
    "7d".into()
}
fn is_session_ttl(s: &str) -> bool {
    s == session_ttl()
}

impl Default for Console {
    fn default() -> Self {
        Console {
            listen: "127.0.0.1:8443".into(),
            url: "https://console.example.com".into(),
            tls_cert: None,
            tls_key: None,
            users: Vec::new(),
            status_every: status_every(),
            session_ttl: session_ttl(),
            github: None,
            apple: None,
            notify: Vec::new(),
        }
    }
}

/// A GitHub OAuth app (first sign-in and new-device enrolment).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubOAuth {
    pub client_id: String,
    /// The client secret, mode 0600.
    #[serde(default = "github_secret_file")]
    pub secret_file: String,
    #[serde(
        default = "github_authorize_url",
        skip_serializing_if = "is_github_authorize_url"
    )]
    pub authorize_url: String,
    #[serde(
        default = "github_token_url",
        skip_serializing_if = "is_github_token_url"
    )]
    pub token_url: String,
    /// The REST API root; `<api_url>/user` names the signed-in login.
    #[serde(default = "github_api_url", skip_serializing_if = "is_github_api_url")]
    pub api_url: String,
}

pub fn github_secret_file() -> String {
    "~/.config/hgl-ai/console/github.secret".into()
}
fn github_authorize_url() -> String {
    "https://github.com/login/oauth/authorize".into()
}
fn is_github_authorize_url(s: &str) -> bool {
    s == github_authorize_url()
}
fn github_token_url() -> String {
    "https://github.com/login/oauth/access_token".into()
}
fn is_github_token_url(s: &str) -> bool {
    s == github_token_url()
}
fn github_api_url() -> String {
    "https://api.github.com".into()
}
fn is_github_api_url(s: &str) -> bool {
    s == github_api_url()
}

impl GithubOAuth {
    pub fn new(client_id: impl Into<String>) -> Self {
        GithubOAuth {
            client_id: client_id.into(),
            secret_file: github_secret_file(),
            authorize_url: github_authorize_url(),
            token_url: github_token_url(),
            api_url: github_api_url(),
        }
    }
}

/// Sign in with Apple: a Services ID and its signing key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppleOAuth {
    /// The Services ID, such as `com.example.hgl-console`.
    pub client_id: String,
    pub team_id: String,
    pub key_id: String,
    /// The `.p8` private key of `key_id`, mode 0600.
    #[serde(default = "apple_key_file")]
    pub key_file: String,
    #[serde(
        default = "apple_authorize_url",
        skip_serializing_if = "is_apple_authorize_url"
    )]
    pub authorize_url: String,
    #[serde(
        default = "apple_token_url",
        skip_serializing_if = "is_apple_token_url"
    )]
    pub token_url: String,
}

pub fn apple_key_file() -> String {
    "~/.config/hgl-ai/console/apple.p8".into()
}
fn apple_authorize_url() -> String {
    "https://appleid.apple.com/auth/authorize".into()
}
fn is_apple_authorize_url(s: &str) -> bool {
    s == apple_authorize_url()
}
fn apple_token_url() -> String {
    "https://appleid.apple.com/auth/token".into()
}
fn is_apple_token_url(s: &str) -> bool {
    s == apple_token_url()
}

impl AppleOAuth {
    pub fn new(
        client_id: impl Into<String>,
        team_id: impl Into<String>,
        key_id: impl Into<String>,
    ) -> Self {
        AppleOAuth {
            client_id: client_id.into(),
            team_id: team_id.into(),
            key_id: key_id.into(),
            key_file: apple_key_file(),
            authorize_url: apple_authorize_url(),
            token_url: apple_token_url(),
        }
    }
}

/// How an email channel reaches its SMTP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtpTls {
    /// Plain connection upgraded with STARTTLS (port 587).
    #[default]
    Starttls,
    /// TLS from the first byte (port 465).
    Tls,
    /// No TLS: only for a relay on loopback.
    None,
}

/// One notification channel (OWN-9). A notification is the title, priority and a link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Notify {
    /// A phone push: POST to an ntfy topic URL.
    Ntfy {
        /// `https://ntfy.example.com/<topic>`
        url: String,
        /// An access token for a protected topic, mode 0600.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_file: Option<String>,
    },
    /// A Slack incoming webhook; the URL is a secret, kept in a file of mode 0600.
    Slack { webhook_file: String },
    /// Email over SMTP: the last resort.
    Email {
        /// `host:port`
        smtp: String,
        #[serde(default)]
        tls: SmtpTls,
        from: String,
        to: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        /// The SMTP password, mode 0600.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password_file: Option<String>,
    },
}

impl Notify {
    pub fn kind(&self) -> &'static str {
        match self {
            Notify::Ntfy { .. } => "ntfy",
            Notify::Slack { .. } => "slack",
            Notify::Email { .. } => "email",
        }
    }
}

/// True for a loopback address (`127.0.0.0/8`, `::1`).
pub fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

impl Console {
    pub fn listen_addr(&self) -> Result<SocketAddr, String> {
        self.listen
            .parse::<SocketAddr>()
            .map_err(|e| format!("console.listen {:?}: {e} (use ADDR:PORT)", self.listen))
    }

    /// The host part of `url`: the WebAuthn relying party id.
    pub fn rp_id(&self) -> Result<String, String> {
        let rest = self
            .url
            .strip_prefix("https://")
            .ok_or_else(|| format!("console.url {:?}: passkeys need https://", self.url))?;
        let authority = rest.split('/').next().unwrap_or("");
        let host = if authority.starts_with('[') {
            authority
                .split(']')
                .next()
                .map(|h| h.trim_start_matches('['))
                .unwrap_or("")
        } else {
            authority.split(':').next().unwrap_or("")
        };
        if host.is_empty() || rest.trim_end_matches('/') != authority {
            return Err(format!(
                "console.url {:?}: give the origin only, https://<name>[:port]",
                self.url
            ));
        }
        Ok(host.to_ascii_lowercase())
    }

    /// `url` without a trailing slash.
    pub fn origin(&self) -> &str {
        self.url.trim_end_matches('/')
    }

    pub fn validate(&self) -> Result<(), String> {
        let addr = self.listen_addr()?;
        if addr.ip().is_unspecified() {
            return Err(format!(
                "console.listen {}: listen on loopback or a tailnet address, never every interface",
                self.listen
            ));
        }
        match (&self.tls_cert, &self.tls_key) {
            (Some(_), Some(_)) => {}
            (None, None) if is_loopback(&addr) => {}
            (None, None) => {
                return Err(format!(
                    "console.listen {}: without tls_cert and tls_key the console serves plain \
                     HTTP, so only on loopback behind a TLS proxy",
                    self.listen
                ));
            }
            _ => return Err("console: give both tls_cert and tls_key, or neither".into()),
        }
        self.rp_id()?;
        for (what, d) in [
            ("status_every", &self.status_every),
            ("session_ttl", &self.session_ttl),
        ] {
            parse_duration(d)
                .filter(|d| !d.is_zero())
                .ok_or_else(|| format!("console.{what}: duration {d:?}"))?;
        }
        for u in &self.users {
            if u.trim().is_empty() || u.trim() != u {
                return Err(format!("console.users: {u:?} is not a login or email"));
            }
        }
        for n in &self.notify {
            match n {
                Notify::Ntfy { url, .. }
                    if !url.starts_with("https://") && !url.starts_with("http://") =>
                {
                    return Err(format!(
                        "console.notify ntfy url {url:?}: expected http(s)://"
                    ));
                }
                Notify::Email { smtp, to, .. } => {
                    if !smtp.contains(':') {
                        return Err(format!(
                            "console.notify email smtp {smtp:?}: expected host:port"
                        ));
                    }
                    if to.is_empty() {
                        return Err("console.notify email: no `to` address".into());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn status_every(&self) -> std::time::Duration {
        parse_duration(&self.status_every).expect("validated")
    }
    pub fn session_ttl(&self) -> std::time::Duration {
        parse_duration(&self.session_ttl).expect("validated")
    }

    /// Whether `identity` may sign in: a GitHub login or an Apple email on the allow-list.
    /// Logins and emails compare without regard to case.
    pub fn allows(&self, identity: &Identity) -> bool {
        let want = match identity {
            Identity::Github(l) => l,
            Identity::Apple(e) => e,
        };
        self.users.iter().any(|u| {
            let is_email = u.contains('@');
            let kind_ok = match identity {
                Identity::Github(_) => !is_email,
                Identity::Apple(_) => is_email,
            };
            kind_ok && u.eq_ignore_ascii_case(want)
        })
    }
}

/// Who signed in, as the provider named them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "provider", content = "name", rename_all = "lowercase")]
pub enum Identity {
    Github(String),
    Apple(String),
}

impl std::fmt::Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Identity::Github(l) => write!(f, "github:{l}"),
            Identity::Apple(e) => write!(f, "apple:{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console() -> Console {
        Console {
            url: "https://hub.example.com".into(),
            users: vec!["hhenson".into(), "Owner@Example.com".into()],
            ..Console::default()
        }
    }

    #[test]
    fn validation() {
        let c = console();
        c.validate().unwrap();
        assert_eq!(c.rp_id().unwrap(), "hub.example.com");
        let mut bad = c.clone();
        bad.listen = "0.0.0.0:8443".into();
        assert!(bad.validate().is_err(), "every interface");
        let mut bad = c.clone();
        bad.listen = "192.0.2.4:8443".into();
        assert!(bad.validate().is_err(), "plain HTTP off loopback");
        bad.tls_cert = Some("c.pem".into());
        assert!(bad.validate().is_err(), "cert without key");
        bad.tls_key = Some("k.pem".into());
        bad.validate().unwrap();
        let mut bad = c.clone();
        bad.url = "http://hub.example.com".into();
        assert!(bad.validate().is_err(), "passkeys need https");
        bad.url = "https://hub.example.com/console".into();
        assert!(bad.validate().is_err(), "an origin, not a path");
        bad.url = "https://hub.example.com:8443/".into();
        assert_eq!(bad.rp_id().unwrap(), "hub.example.com");
        assert_eq!(bad.origin(), "https://hub.example.com:8443");
        let mut bad = c.clone();
        bad.status_every = "0s".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn allow_list() {
        let c = console();
        assert!(c.allows(&Identity::Github("HHenson".into())));
        assert!(c.allows(&Identity::Apple("owner@example.com".into())));
        assert!(!c.allows(&Identity::Github("owner@example.com".into())));
        assert!(!c.allows(&Identity::Apple("hhenson".into())));
        assert!(!c.allows(&Identity::Github("someone".into())));
        assert_eq!(
            Identity::Github("hhenson".into()).to_string(),
            "github:hhenson"
        );
    }

    #[test]
    fn toml_shape() {
        let t = r#"
listen = "127.0.0.1:8443"
url = "https://hub.example.com"
users = ["hhenson"]

[github]
client_id = "Iv1.example"

[apple]
client_id = "com.example.console"
team_id = "TEAM123456"
key_id = "KEY1234567"

[[notify]]
kind = "ntfy"
url = "https://ntfy.example.com/topic"

[[notify]]
kind = "slack"
webhook_file = "~/.config/hgl-ai/console/slack.url"

[[notify]]
kind = "email"
smtp = "smtp.example.com:587"
from = "console@example.com"
to = ["owner@example.com"]
"#;
        let c: Console = toml::from_str(t).unwrap();
        c.validate().unwrap();
        assert_eq!(c.github.as_ref().unwrap().secret_file, github_secret_file());
        assert_eq!(c.notify.len(), 3);
        assert!(matches!(
            c.notify[2],
            Notify::Email {
                tls: SmtpTls::Starttls,
                ..
            }
        ));
        let back = toml::to_string_pretty(&c).unwrap();
        assert!(!back.contains("authorize_url"), "{back}");
        assert_eq!(toml::from_str::<Console>(&back).unwrap(), c);
        assert!(toml::from_str::<Console>("listen = \"x\"\nurl = \"y\"\nbogus = 1\n").is_err());
        let bad = "listen = \"127.0.0.1:1\"\nurl = \"https://a\"\n[[notify]]\nkind = \"ntfy\"\nurl = \"https://x\"\nextra = 1\n";
        assert!(
            toml::from_str::<Console>(bad).is_err(),
            "unknown channel field"
        );
    }
}
