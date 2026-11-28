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
    /// The hub `viewer` user's credential, which the console hands to a signed-in page
    /// for reading the telemetry over WebSocket (docs/owner.md §3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer_credential: Option<String>,
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
            viewer_credential: None,
        }
    }
}

/// One entry of `[console] users`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum User {
    /// A person, keyed by email, with the identities they sign in with.
    Person(Person),
    /// An earlier form: `{ id, handle }`; no email (`doctor` fails, naming it).
    Linked(LinkedEntry),
    /// The earliest form: a bare identity; no email or handle.
    Plain(String),
}

/// `{ email = "...", handle = "...", github = "<login>", apple = "...", google = "..." }`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Person {
    /// The user's key (lower-cased) and the default address of the email channel.
    pub email: String,
    /// The name the user chooses to sign with (OWN-10): 1 to 31 letters, digits, spaces
    /// or `-_.'`, unique among users ignoring case and repeated spaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    /// A GitHub login; without one, a GitHub account whose verified emails include
    /// `email` signs in as this user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    /// The email Apple gives (it may be a private relay address); default `email`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apple: Option<String>,
    /// The Google account email; default `email`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub google: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkedEntry {
    pub id: Ids,
    pub handle: String,
}

/// One identity or several.
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
    /// `{ email, handle }`: Apple and Google by that email, GitHub by its verified emails.
    pub fn person(email: impl Into<String>, handle: impl Into<String>) -> User {
        User::Person(Person {
            email: email.into(),
            handle: Some(handle.into()),
            github: None,
            apple: None,
            google: None,
        })
    }
}

/// One user as the console knows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The user's key: the email, lower-cased; for an entry in an earlier form, its
    /// first identity (`github:<login>`).
    pub key: String,
    pub email: Option<String>,
    pub handle: Option<String>,
    pub github: Option<String>,
    pub apple: Option<String>,
    pub google: Option<String>,
}

impl Member {
    /// How the user signs: the handle, else the key.
    pub fn signature(&self) -> &str {
        self.handle.as_deref().unwrap_or(&self.key)
    }
}

/// Why an identity signs in as nobody.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unresolved {
    NotListed,
    /// More than one user matches: refused rather than guessed.
    Ambiguous(Vec<String>),
}

fn lower(s: &str) -> String {
    s.trim().to_lowercase()
}

fn is_email(s: &str) -> bool {
    let s = s.trim();
    matches!(s.split_once('@'), Some((a, b)) if !a.is_empty() && b.contains('.') && !s.contains(char::is_whitespace))
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
        /// Recipients; default every user's `email` (OWN-9).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
        let members = self.members()?;
        let mut seen = std::collections::BTreeMap::new();
        for m in &members {
            if let Some(h) = &m.handle {
                hgl_ai_proto::check_handle(h).map_err(|e| format!("console.users: {e}"))?;
                if seen
                    .insert(format!("handle {}", hgl_ai_proto::handle_key(h)), ())
                    .is_some()
                {
                    return Err(format!("console.users: handle {h:?} is taken twice"));
                }
            }
            for (what, v) in [
                ("user", Some(&m.key)),
                ("github", m.github.as_ref()),
                ("apple", m.apple.as_ref()),
                ("google", m.google.as_ref()),
            ] {
                if let Some(v) = v
                    && seen.insert(format!("{what} {}", lower(v)), ()).is_some()
                {
                    return Err(format!("console.users: {what} {v} is listed twice"));
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
                    if to.is_empty() && !members.iter().any(|m| m.email.is_some()) {
                        return Err(
                            "console.notify email: no `to` address and no user email".into()
                        );
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

    /// The users, parsed and checked one by one.
    pub fn members(&self) -> Result<Vec<Member>, String> {
        let mut out = Vec::new();
        for u in &self.users {
            out.push(match u {
                User::Person(p) => {
                    if !is_email(&p.email) {
                        return Err(format!(
                            "console.users: email {:?} is not an email",
                            p.email
                        ));
                    }
                    for (what, v) in [("apple", &p.apple), ("google", &p.google)] {
                        if let Some(v) = v
                            && !is_email(v)
                        {
                            return Err(format!("console.users: {what} {v:?} is not an email"));
                        }
                    }
                    if let Some(g) = &p.github
                        && (g.trim().is_empty() || g.contains(['@', ':', ' ']))
                    {
                        return Err(format!("console.users: github {g:?} is not a login"));
                    }
                    let email = lower(&p.email);
                    Member {
                        key: email.clone(),
                        email: Some(email.clone()),
                        handle: p.handle.clone(),
                        github: p.github.as_deref().map(lower),
                        apple: Some(
                            p.apple
                                .as_deref()
                                .map(lower)
                                .unwrap_or_else(|| email.clone()),
                        ),
                        google: Some(p.google.as_deref().map(lower).unwrap_or(email)),
                    }
                }
                User::Linked(l) => legacy(&l.id.all(), Some(l.handle.clone()))?,
                User::Plain(s) => legacy(&[s.as_str()], None)?,
            });
        }
        Ok(out)
    }

    /// The user an identity signs in as. GitHub matches a configured login; failing
    /// that, a user without one whose `email` is among `github_emails` (the account's
    /// verified emails). Apple and Google match their links (by default the email). No
    /// match, or two, is refused.
    pub fn resolve(&self, id: &Identity, github_emails: &[String]) -> Result<Member, Unresolved> {
        let members = self.members().map_err(|_| Unresolved::NotListed)?;
        let pick = |v: Vec<&Member>| match v.len() {
            0 => Err(Unresolved::NotListed),
            1 => Ok(v[0].clone()),
            _ => Err(Unresolved::Ambiguous(
                v.iter().map(|m| m.key.clone()).collect(),
            )),
        };
        match id {
            Identity::Github(login) => {
                let l = lower(login);
                let by_login: Vec<&Member> = members
                    .iter()
                    .filter(|m| m.github.as_deref() == Some(l.as_str()))
                    .collect();
                if !by_login.is_empty() {
                    return pick(by_login);
                }
                let verified: Vec<String> = github_emails.iter().map(|e| lower(e)).collect();
                pick(
                    members
                        .iter()
                        .filter(|m| m.github.is_none())
                        .filter(|m| m.email.as_ref().is_some_and(|e| verified.contains(e)))
                        .collect(),
                )
            }
            Identity::Apple(e) => {
                let e = lower(e);
                pick(
                    members
                        .iter()
                        .filter(|m| m.apple.as_deref() == Some(e.as_str()))
                        .collect(),
                )
            }
            Identity::Google(e) => {
                let e = lower(e);
                pick(
                    members
                        .iter()
                        .filter(|m| m.google.as_deref() == Some(e.as_str()))
                        .collect(),
                )
            }
        }
    }

    /// The user with this key, if still listed.
    pub fn member(&self, key: &str) -> Option<Member> {
        self.members().ok()?.into_iter().find(|m| m.key == key)
    }

    /// Whether a GitHub sign-in needs the account's verified emails: no user has its login.
    pub fn github_needs_emails(&self, login: &str) -> bool {
        let l = lower(login);
        !self
            .members()
            .unwrap_or_default()
            .iter()
            .any(|m| m.github.as_deref() == Some(l.as_str()))
    }

    /// Entries in an earlier form, without an email: what `doctor` names.
    pub fn without_email(&self) -> Vec<String> {
        self.members()
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m.email.is_none())
            .map(|m| m.key)
            .collect()
    }

    /// Users without a handle.
    pub fn missing_handles(&self) -> Vec<String> {
        self.members()
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m.handle.is_none())
            .map(|m| m.key)
            .collect()
    }

    /// Every user's email: the email channel's default recipients (OWN-9).
    pub fn emails(&self) -> Vec<String> {
        self.members()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| m.email)
            .collect()
    }
}

/// An entry in an earlier form: its identities, keyed by the first.
fn legacy(ids: &[&str], handle: Option<String>) -> Result<Member, String> {
    let mut m = Member {
        key: String::new(),
        email: None,
        handle,
        github: None,
        apple: None,
        google: None,
    };
    for raw in ids {
        let id = Identity::parse(raw).map_err(|e| format!("console.users: {e}"))?;
        if m.key.is_empty() {
            m.key = id.to_string().to_lowercase();
        }
        match id {
            Identity::Github(l) => m.github = Some(lower(&l)),
            Identity::Apple(e) => m.apple = Some(lower(&e)),
            Identity::Google(e) => m.google = Some(lower(&e)),
        }
    }
    if m.key.is_empty() {
        return Err("console.users: an entry with no id".into());
    }
    Ok(m)
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

    fn person(email: &str, handle: &str, github: Option<&str>, apple: Option<&str>) -> User {
        User::Person(Person {
            email: email.into(),
            handle: Some(handle.into()),
            github: github.map(String::from),
            apple: apple.map(String::from),
            google: None,
        })
    }

    fn console() -> Console {
        Console {
            url: "https://hub.example.com".into(),
            users: vec![
                person(
                    "Owner@Example.com",
                    "This Dude",
                    Some("hhenson"),
                    Some("relay@privaterelay.example.com"),
                ),
                person("second@example.com", "Two", None, None),
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
    fn users_by_email() {
        let c = console();
        let key = |id: Identity, emails: &[&str]| {
            let e: Vec<String> = emails.iter().map(|s| s.to_string()).collect();
            c.resolve(&id, &e).map(|m| m.key)
        };
        // Each provider reaches the same user.
        let me = Ok("owner@example.com".to_string());
        assert_eq!(key(Identity::Github("HHenson".into()), &[]), me, "by login");
        assert_eq!(
            key(Identity::Google("owner@example.com".into()), &[]),
            me,
            "google defaults to email"
        );
        assert_eq!(
            key(
                Identity::Apple("relay@privaterelay.example.com".into()),
                &[]
            ),
            me,
            "apple relay link"
        );
        assert_eq!(
            key(Identity::Apple("owner@example.com".into()), &[]),
            Err(Unresolved::NotListed),
            "apple is linked to the relay address"
        );
        // GitHub by a verified email, for a user without a login.
        assert_eq!(
            key(Identity::Github("someone".into()), &["SECOND@example.com"]),
            Ok("second@example.com".to_string())
        );
        assert_eq!(
            key(Identity::Github("someone".into()), &[]),
            Err(Unresolved::NotListed)
        );
        // A login takes precedence; the first user's email does not match by email
        // because it has a login configured.
        assert_eq!(
            key(Identity::Github("someone".into()), &["owner@example.com"]),
            Err(Unresolved::NotListed)
        );
        assert!(c.github_needs_emails("someone") && !c.github_needs_emails("hhenson"));
        // Two users whose emails one GitHub account has verified: refused.
        let two = Console {
            users: vec![
                person("a@example.com", "A", None, None),
                person("b@example.com", "B", None, None),
            ],
            ..console()
        };
        assert!(matches!(
            two.resolve(
                &Identity::Github("x".into()),
                &["a@example.com".into(), "b@example.com".into()]
            ),
            Err(Unresolved::Ambiguous(_))
        ));
        let m = c.member("owner@example.com").unwrap();
        assert_eq!(m.signature(), "This Dude");
        assert_eq!(c.emails(), ["owner@example.com", "second@example.com"]);
        assert!(c.without_email().is_empty() && c.missing_handles().is_empty());
        assert_eq!(
            Identity::Github("hhenson".into()).to_string(),
            "github:hhenson"
        );
        for bad in [
            "owner@example.com",
            "github:a@b.c",
            "apple:nobody",
            "gitlab:x",
            "google:",
        ] {
            assert!(Identity::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn earlier_forms() {
        let old: Console = toml::from_str(
            "listen = \"127.0.0.1:1\"\nurl = \"https://a.example.com\"\nusers = [\"hhenson\", { id = \"google:o@example.com\", handle = \"OW\" }]\n",
        )
        .unwrap();
        old.validate().unwrap();
        assert_eq!(
            old.resolve(&Identity::Github("hhenson".into()), &[])
                .unwrap()
                .key,
            "github:hhenson"
        );
        assert_eq!(
            old.resolve(&Identity::Google("O@example.com".into()), &[])
                .unwrap()
                .handle
                .as_deref(),
            Some("OW")
        );
        assert_eq!(
            old.without_email(),
            ["github:hhenson", "google:o@example.com"]
        );
        assert_eq!(old.missing_handles(), ["github:hhenson"]);
        let bare_email: Console = toml::from_str(
            "listen = \"127.0.0.1:1\"\nurl = \"https://a.example.com\"\nusers = [\"o@example.com\"]\n",
        )
        .unwrap();
        assert!(bare_email.validate().is_err(), "apple: or google:?");
    }

    #[test]
    fn users_are_checked() {
        let with = |users: Vec<User>| Console { users, ..console() };
        let p = |e: &str, h: &str| User::person(e, h);
        assert!(with(vec![p("not-an-email", "A")]).validate().is_err());
        assert!(
            with(vec![p("a@example.com", "")]).validate().is_err(),
            "empty handle"
        );
        assert!(
            with(vec![p("a@example.com", &"x".repeat(32))])
                .validate()
                .is_err(),
            "too long"
        );
        assert!(
            with(vec![p("a@example.com", "H@P")]).validate().is_err(),
            "letters, digits, -_.'"
        );
        assert!(
            with(vec![p("a@example.com", " Dude")]).validate().is_err(),
            "a leading space"
        );
        with(vec![p("a@example.com", "This Dude")])
            .validate()
            .unwrap();
        assert!(
            with(vec![
                p("a@example.com", "This  Dude"),
                p("b@example.com", "this dude")
            ])
            .validate()
            .is_err(),
            "handles are the same ignoring case and repeated spaces"
        );
        assert!(
            with(vec![p("A@example.com", "One"), p("a@example.com", "Two")])
                .validate()
                .is_err(),
            "one email, one user"
        );
        assert!(
            with(vec![
                person("a@example.com", "One", Some("hh"), None),
                person("b@example.com", "Two", Some("HH"), None)
            ])
            .validate()
            .is_err(),
            "one GitHub login, one user"
        );
        assert!(
            with(vec![
                p("a@example.com", "One"),
                person("b@example.com", "Two", None, Some("a@example.com"))
            ])
            .validate()
            .is_err(),
            "an Apple address belongs to one user"
        );
        let t = "listen = \"127.0.0.1:1\"\nurl = \"https://a.example.com\"\nusers = [{ email = \"owner@example.com\", handle = \"This Dude\", github = \"hhenson\", apple = \"r@privaterelay.example.com\" }]\n";
        let c: Console = toml::from_str(t).unwrap();
        c.validate().unwrap();
        let back: Console = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
        assert!(
            toml::from_str::<Console>(
                "listen = \"x\"\nurl = \"y\"\nusers = [{ email = \"a@example.com\", extra = 1 }]\n"
            )
            .is_err()
        );
    }

    #[test]
    fn toml_shape() {
        let t = r#"
listen = "127.0.0.1:8443"
url = "https://hub.example.com"
users = [{ email = "owner@example.com", handle = "This Dude", github = "hhenson" }]

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
