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
    /// Who may sign in, each with a handle (OWN-10): `{ id = "github:<login>",
    /// handle = "HPH" }`; `id` is one identity or a list (`github:<login>`,
    /// `apple:<email>`, `google:<email>`). The older form, a bare identity, has no handle.
    #[serde(default)]
    pub users: Vec<User>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub google: Option<GoogleOAuth>,
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
            google: None,
            notify: Vec::new(),
        }
    }
}

/// One entry of `[console] users`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum User {
    /// An identity with its owner's handle.
    Owner(OwnerEntry),
    /// The older form: an identity without a handle (`doctor` warns).
    Plain(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerEntry {
    pub id: Ids,
    /// The name the owner chooses to sign with (OWN-10): 1 to 31 letters, digits, spaces
    /// or `-_.'`, unique among users ignoring case and repeated spaces.
    pub handle: String,
}

/// One identity or several (one owner signing in with GitHub and Google, say).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Ids {
    One(String),
    Many(Vec<String>),
}

impl Ids {
    pub fn all(&self) -> Vec<&str> {
        match self {
            Ids::One(s) => vec![s.as_str()],
            Ids::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

impl User {
    /// `{ id, handle }`
    pub fn new(id: impl Into<String>, handle: impl Into<String>) -> User {
        User::Owner(OwnerEntry {
            id: Ids::One(id.into()),
            handle: handle.into(),
        })
    }
    fn ids(&self) -> Vec<&str> {
        match self {
            User::Owner(o) => o.id.all(),
            User::Plain(s) => vec![s.as_str()],
        }
    }
    pub fn handle(&self) -> Option<&str> {
        match self {
            User::Owner(o) => Some(&o.handle),
            User::Plain(_) => None,
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
    /// Apple's signing keys; the `id_token`'s signature is checked against them.
    #[serde(default = "apple_jwks_url", skip_serializing_if = "is_apple_jwks_url")]
    pub jwks_url: String,
}

fn apple_jwks_url() -> String {
    "https://appleid.apple.com/auth/keys".into()
}
fn is_apple_jwks_url(s: &str) -> bool {
    s == apple_jwks_url()
}

/// Google sign-in: an OAuth 2.0 client (OpenID Connect, authorization code with PKCE).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleOAuth {
    /// `<id>.apps.googleusercontent.com`
    pub client_id: String,
    /// The client secret, mode 0600.
    #[serde(default = "google_secret_file")]
    pub secret_file: String,
    #[serde(
        default = "google_authorize_url",
        skip_serializing_if = "is_google_authorize_url"
    )]
    pub authorize_url: String,
    #[serde(
        default = "google_token_url",
        skip_serializing_if = "is_google_token_url"
    )]
    pub token_url: String,
    /// Google's signing keys (JWKS); the `id_token`'s signature is checked against them.
    #[serde(
        default = "google_jwks_url",
        skip_serializing_if = "is_google_jwks_url"
    )]
    pub jwks_url: String,
}

pub fn google_secret_file() -> String {
    "~/.config/hgl-ai/console/google.secret".into()
}
fn google_authorize_url() -> String {
    "https://accounts.google.com/o/oauth2/v2/auth".into()
}
fn is_google_authorize_url(s: &str) -> bool {
    s == google_authorize_url()
}
fn google_token_url() -> String {
    "https://oauth2.googleapis.com/token".into()
}
fn is_google_token_url(s: &str) -> bool {
    s == google_token_url()
}
fn google_jwks_url() -> String {
    "https://www.googleapis.com/oauth2/v3/certs".into()
}
fn is_google_jwks_url(s: &str) -> bool {
    s == google_jwks_url()
}

impl GoogleOAuth {
    pub fn new(client_id: impl Into<String>) -> Self {
        GoogleOAuth {
            client_id: client_id.into(),
            secret_file: google_secret_file(),
            authorize_url: google_authorize_url(),
            token_url: google_token_url(),
            jwks_url: google_jwks_url(),
        }
    }
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
            jwks_url: apple_jwks_url(),
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
        let owners = self.owners()?;
        let mut handles = std::collections::BTreeSet::new();
        let mut ids = std::collections::BTreeSet::new();
        for o in &owners {
            if let Some(h) = &o.handle {
                hgl_ai_proto::check_handle(h).map_err(|e| format!("console.users: {e}"))?;
                if !handles.insert(hgl_ai_proto::handle_key(h)) {
                    return Err(format!("console.users: handle {h} is taken twice"));
                }
            }
            for i in &o.ids {
                if !ids.insert(i.folded()) {
                    return Err(format!("console.users: {i} is listed twice"));
                }
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

    /// The users, parsed.
    pub fn owners(&self) -> Result<Vec<Owner>, String> {
        self.users
            .iter()
            .map(|u| {
                let ids = u
                    .ids()
                    .into_iter()
                    .map(Identity::parse)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("console.users: {e}"))?;
                if ids.is_empty() {
                    return Err("console.users: an entry with no id".to_string());
                }
                Ok(Owner {
                    ids,
                    handle: u.handle().map(String::from),
                })
            })
            .collect()
    }

    /// The user `identity` signs in as, if the allow-list has it. Logins and emails
    /// compare without regard to case; the provider must match.
    pub fn owner(&self, identity: &Identity) -> Option<Owner> {
        let want = identity.folded();
        self.owners()
            .ok()?
            .into_iter()
            .find(|o| o.ids.iter().any(|i| i.folded() == want))
    }

    /// Whether `identity` may sign in.
    pub fn allows(&self, identity: &Identity) -> bool {
        self.owner(identity).is_some()
    }

    /// The handle that signs `identity`'s words: its handle, or for an entry in the
    /// older form without one, the identity itself.
    pub fn handle_of(&self, identity: &Identity) -> Option<String> {
        self.owner(identity)?.handle
    }

    /// Entries in the older form, without a handle.
    pub fn missing_handles(&self) -> Vec<String> {
        self.users
            .iter()
            .filter(|u| u.handle().is_none())
            .flat_map(|u| u.ids().into_iter().map(String::from))
            .collect()
    }
}

/// One person on the allow-list: their identities and handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub ids: Vec<Identity>,
    pub handle: Option<String>,
}

/// Who signed in, as the provider named them. Three kinds, never confused:
/// `github:<login>`, `apple:<email>`, `google:<email>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "provider", content = "name", rename_all = "lowercase")]
pub enum Identity {
    Github(String),
    Apple(String),
    Google(String),
}

impl Identity {
    /// `github:<login>`, `apple:<email>`, `google:<email>`; a bare login is GitHub's. A
    /// bare email is refused: it could be Apple's or Google's.
    pub fn parse(s: &str) -> Result<Identity, String> {
        let s = s.trim();
        let (kind, name) = match s.split_once(':') {
            Some((k, n)) => (k, n),
            None if s.contains('@') => {
                return Err(format!(
                    "{s:?}: an email needs its provider, apple:{s} or google:{s}"
                ));
            }
            None => ("github", s),
        };
        let email = name.contains('@') && !name.starts_with('@') && !name.ends_with('@');
        let ok_chars = !name.is_empty() && !name.chars().any(char::is_whitespace);
        match kind {
            "github" if ok_chars && !name.contains('@') => Ok(Identity::Github(name.into())),
            "apple" if ok_chars && email => Ok(Identity::Apple(name.into())),
            "google" if ok_chars && email => Ok(Identity::Google(name.into())),
            _ => Err(format!(
                "{s:?}: expected github:<login>, apple:<email> or google:<email>"
            )),
        }
    }

    /// For comparison: the kind and the name in lower case.
    fn folded(&self) -> (u8, String) {
        match self {
            Identity::Github(n) => (0, n.to_ascii_lowercase()),
            Identity::Apple(n) => (1, n.to_ascii_lowercase()),
            Identity::Google(n) => (2, n.to_ascii_lowercase()),
        }
    }
}

impl std::fmt::Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Identity::Github(l) => write!(f, "github:{l}"),
            Identity::Apple(e) => write!(f, "apple:{e}"),
            Identity::Google(e) => write!(f, "google:{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console() -> Console {
        Console {
            url: "https://hub.example.com".into(),
            users: vec![
                User::new("github:hhenson", "HPH"),
                User::Owner(OwnerEntry {
                    id: Ids::Many(vec![
                        "apple:Owner@Example.com".into(),
                        "google:owner@example.com".into(),
                    ]),
                    handle: "OW".into(),
                }),
            ],
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
        assert!(c.allows(&Identity::Google("Owner@example.com".into())));
        assert!(!c.allows(&Identity::Github("owner@example.com".into())));
        assert!(!c.allows(&Identity::Apple("hhenson".into())));
        assert!(!c.allows(&Identity::Github("someone".into())));
        assert_eq!(
            c.handle_of(&Identity::Github("hhenson".into())).as_deref(),
            Some("HPH")
        );
        assert_eq!(
            c.handle_of(&Identity::Google("owner@example.com".into()))
                .as_deref(),
            Some("OW")
        );
        assert!(c.missing_handles().is_empty());

        // Three kinds of identity, never confused.
        let only_google = Console {
            users: vec![User::new("google:owner@example.com", "GO")],
            ..console()
        };
        assert!(!only_google.allows(&Identity::Apple("owner@example.com".into())));
        assert_eq!(
            Identity::parse("hhenson").unwrap(),
            Identity::Github("hhenson".into())
        );
        for bad in [
            "owner@example.com",
            "github:a@b.c",
            "apple:nobody",
            "gitlab:x",
            "google:",
            "apple: a@b.c",
        ] {
            assert!(Identity::parse(bad).is_err(), "{bad}");
        }

        // The older form still works, without a handle.
        let old: Console = toml::from_str(
            "listen = \"127.0.0.1:1\"\nurl = \"https://a.example.com\"\nusers = [\"hhenson\", \"apple:o@example.com\"]\n",
        )
        .unwrap();
        old.validate().unwrap();
        assert!(old.allows(&Identity::Github("hhenson".into())));
        assert_eq!(old.handle_of(&Identity::Github("hhenson".into())), None);
        assert_eq!(old.missing_handles(), ["hhenson", "apple:o@example.com"]);
        let bare_email: Console = toml::from_str(
            "listen = \"127.0.0.1:1\"\nurl = \"https://a.example.com\"\nusers = [\"o@example.com\"]\n",
        )
        .unwrap();
        assert!(bare_email.validate().is_err(), "apple: or google:?");
    }

    #[test]
    fn handles_are_checked() {
        let with = |users: Vec<User>| Console { users, ..console() };
        assert!(
            with(vec![User::new("github:a", "")]).validate().is_err(),
            "empty"
        );
        assert!(
            with(vec![User::new("github:a", "x".repeat(32))])
                .validate()
                .is_err(),
            "too long"
        );
        assert!(
            with(vec![User::new("github:a", "H@P")]).validate().is_err(),
            "letters, digits, -_.'"
        );
        assert!(
            with(vec![User::new("github:a", " Dude")])
                .validate()
                .is_err(),
            "a leading space"
        );
        with(vec![User::new("github:a", "This Dude")])
            .validate()
            .unwrap();
        assert!(
            with(vec![
                User::new("github:a", "This  Dude"),
                User::new("github:b", "this dude")
            ])
            .validate()
            .is_err(),
            "the same ignoring case and repeated spaces"
        );
        assert!(
            with(vec![
                User::new("github:a", "HPH"),
                User::new("github:b", "hph")
            ])
            .validate()
            .is_err(),
            "unique"
        );
        assert!(
            with(vec![
                User::new("github:a", "HPH"),
                User::new("github:A", "XY")
            ])
            .validate()
            .is_err(),
            "one identity, one user"
        );
        with(vec![
            User::new("github:a", "HPH"),
            User::new("github:b", "X2"),
        ])
        .validate()
        .unwrap();
        let t = "listen = \"127.0.0.1:1\"\nurl = \"https://a.example.com\"\nusers = [{ id = \"github:hhenson\", handle = \"HPH\" }, { id = [\"apple:o@example.com\", \"google:o@example.com\"], handle = \"OW\" }]\n";
        let c: Console = toml::from_str(t).unwrap();
        c.validate().unwrap();
        assert_eq!(
            c.handle_of(&Identity::Google("o@example.com".into()))
                .as_deref(),
            Some("OW")
        );
        let back: Console = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
        assert!(toml::from_str::<Console>("listen = \"x\"\nurl = \"y\"\nusers = [{ id = \"github:a\", handle = \"AB\", extra = 1 }]\n").is_err());
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

[google]
client_id = "0123-example.apps.googleusercontent.com"

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
