//! Sign-in (OWN-6): passkeys (WebAuthn), with a user's first sign-in and each new
//! device's passkey enrolled through GitHub or Sign in with Apple. Only identities on
//! `[console] users` get in. Sessions are random ids in HMAC-signed cookies (HttpOnly,
//! Secure, SameSite=Strict) kept in memory; each carries a CSRF token for its forms.
//!
//! The OAuth flows bind the browser that started them with a short-lived cookie that is
//! `SameSite=None`, because Apple returns with a cross-site form POST; that cookie only
//! ever names a pending flow, never a session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use hgl_ai_config::{AppleOAuth, Console, GithubOAuth, Identity};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use webauthn_rs::prelude::{
    CreationChallengeResponse, Passkey, PasskeyAuthentication, PasskeyRegistration,
    PublicKeyCredential, RegisterPublicKeyCredential, RequestChallengeResponse, Url, Uuid,
    Webauthn, WebauthnBuilder,
};

pub const SESSION_COOKIE: &str = "hgl_session";
pub const FLOW_COOKIE: &str = "hgl_flow";
pub const ENROL_COOKIE: &str = "hgl_enrol";
pub const LOGIN_COOKIE: &str = "hgl_login";
/// How long an OAuth flow, an enrolment or a passkey challenge stays valid.
pub const PENDING_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0}")]
    Setup(String),
    /// The request is not acceptable (bad state, stale flow, failed ceremony).
    #[error("{0}")]
    Bad(String),
    /// The identity is not on the allow-list.
    #[error("{0} may not sign in to this console")]
    NotAllowed(String),
    /// The provider failed or answered oddly.
    #[error("{0}")]
    Provider(String),
}

fn bad(m: impl Into<String>) -> AuthError {
    AuthError::Bad(m.into())
}

/// 32 random bytes as hex.
pub fn random_hex() -> String {
    use std::io::Read as _;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("reading /dev/urandom");
    hex::encode(b)
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// A signed-in user: the user's key (their email, `[console] users`), how they signed
/// in, the form token, and when it ends (UTC seconds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// The id in the cookie; never stored on the hub, where a session is keyed by its
    /// SHA-256.
    #[serde(skip, default)]
    pub id: String,
    pub user: String,
    /// `github:<login>`, `apple:<email>`, `google:<email>` or `passkey`.
    pub via: String,
    pub csrf: String,
    pub expires_at: i64,
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `session.<sha256 of the id>`: a session's key in `HGL_CONSOLE`.
pub fn session_key(id: &str) -> String {
    use sha2::Digest as _;
    format!("session.{}", hex::encode(Sha256::digest(id.as_bytes())))
}

/// A write of the console's state to the hub (`HGL_CONSOLE`), done in the background so
/// a sign-in never waits on it.
#[derive(Debug, Clone)]
pub enum Persist {
    Put(String, Session),
    Del(String),
    /// The pending `p2` batch: thread ids and titles.
    P2(Vec<(String, String)>),
    /// The `p1` notifications no channel took yet: thread ids and titles.
    P1(Vec<(String, String)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Github,
    Apple,
    Google,
}

struct Flow {
    provider: Provider,
    binding: String,
    nonce: String,
    /// PKCE `code_verifier` (Google).
    verifier: String,
    started: Instant,
}

struct Enrolment {
    user: String,
    via: Identity,
    reg: Option<PasskeyRegistration>,
    started: Instant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPasskey {
    passkey: Passkey,
    added_at: String,
    #[serde(default)]
    last_used: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredUser {
    /// The user's key (email).
    user: String,
    uuid: Uuid,
    passkeys: Vec<StoredPasskey>,
}

/// `<data_dir>/console/passkeys.json` (mode 0600): each user's passkeys.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    users: Vec<StoredUser>,
}

/// Write `data` to `path` atomically with mode 0600.
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write as _;
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let mut f = o.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    std::fs::rename(&tmp, path)
}

/// A secret file: trimmed content, refused when group or others may read it.
pub fn read_secret(path: &Path) -> Result<String, AuthError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let m = std::fs::metadata(path)
            .map_err(|e| AuthError::Setup(format!("{}: {e}", path.display())))?
            .permissions()
            .mode();
        if m & 0o077 != 0 {
            return Err(AuthError::Setup(format!(
                "{}: mode {:o} is open to group or others; a secret needs 0600",
                path.display(),
                m & 0o777
            )));
        }
    }
    let s = std::fs::read_to_string(path)
        .map_err(|e| AuthError::Setup(format!("{}: {e}", path.display())))?;
    let s = s.trim().to_string();
    if s.is_empty() {
        return Err(AuthError::Setup(format!("{} is empty", path.display())));
    }
    Ok(s)
}

fn expand(p: &str) -> PathBuf {
    hgl_ai_config::expand(p).unwrap_or_else(|_| PathBuf::from(p))
}

/// The console's sign-in state.
pub struct Auth {
    console: Console,
    origin: String,
    webauthn: Webauthn,
    key: Vec<u8>,
    store_path: PathBuf,
    store: Mutex<Store>,
    /// By [`session_key`].
    sessions: Mutex<HashMap<String, Session>>,
    persist: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Persist>>>,
    flows: Mutex<HashMap<String, Flow>>,
    enrolments: Mutex<HashMap<String, Enrolment>>,
    logins: Mutex<HashMap<String, (PasskeyAuthentication, Instant)>>,
    http: reqwest::Client,
    jwks: crate::jwt::JwksCache,
}

impl Auth {
    /// `dir` holds the passkeys and the cookie key (`<data_dir>/console`).
    pub fn new(console: &Console, dir: &Path) -> Result<Auth, AuthError> {
        let origin = console.origin().to_string();
        let url = Url::parse(&origin).map_err(|e| AuthError::Setup(format!("console.url: {e}")))?;
        let rp_id = console.rp_id().map_err(AuthError::Setup)?;
        let webauthn = WebauthnBuilder::new(&rp_id, &url)
            .and_then(|b| b.rp_name("hgl-ai console").build())
            .map_err(|e| AuthError::Setup(format!("passkeys for {origin}: {e}")))?;
        let key_path = dir.join("cookie.key");
        let key = match read_secret(&key_path) {
            Ok(k) => hex::decode(k)
                .map_err(|e| AuthError::Setup(format!("{}: {e}", key_path.display())))?,
            Err(_) if !key_path.exists() => {
                let k = random_hex();
                write_private(&key_path, format!("{k}\n").as_bytes())
                    .map_err(|e| AuthError::Setup(format!("{}: {e}", key_path.display())))?;
                hex::decode(k).expect("hex")
            }
            Err(e) => return Err(e),
        };
        let store_path = dir.join("passkeys.json");
        let store = if store_path.exists() {
            let text = std::fs::read_to_string(&store_path)
                .map_err(|e| AuthError::Setup(format!("{}: {e}", store_path.display())))?;
            serde_json::from_str(&text)
                .map_err(|e| AuthError::Setup(format!("{}: {e}", store_path.display())))?
        } else {
            Store::default()
        };
        // TLS through ring, as everywhere in hgl-ai (no other provider is linked).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("hgl-ai-console")
            .build()
            .map_err(|e| AuthError::Setup(format!("HTTP client: {e}")))?;
        Ok(Auth {
            console: console.clone(),
            origin,
            webauthn,
            key,
            store_path,
            store: Mutex::new(store),
            sessions: Mutex::default(),
            persist: Mutex::default(),
            flows: Mutex::default(),
            enrolments: Mutex::default(),
            logins: Mutex::default(),
            http,
            jwks: crate::jwt::JwksCache::default(),
        })
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn has_github(&self) -> bool {
        self.console.github.is_some()
    }
    pub fn has_apple(&self) -> bool {
        self.console.apple.is_some()
    }
    pub fn has_google(&self) -> bool {
        self.console.google.is_some()
    }

    /// The handle that signs this user's words (OWN-10), if `[console] users` gives one.
    pub fn handle_of(&self, user: &str) -> Option<String> {
        self.console.member(user)?.handle
    }

    /// The user as the telemetry page's header shows them: the handle only, never the
    /// email (the page is a client of the hub too); `signed in` without a handle.
    pub fn handle_only(&self, user: &str) -> String {
        self.handle_of(user).unwrap_or_else(|| "signed in".into())
    }

    /// `This Dude (owner@example.com)`, or the key alone without a handle.
    pub fn display(&self, user: &str) -> String {
        match self.handle_of(user) {
            Some(h) => format!("{h} ({user})"),
            None => user.to_string(),
        }
    }

    // ---- cookies ----

    fn mac(&self, name: &str, value: &str) -> String {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.key).expect("any key length");
        m.update(name.as_bytes());
        m.update(b"=");
        m.update(value.as_bytes());
        hex::encode(&m.finalize().into_bytes()[..16])
    }

    /// `<value>.<mac>`: the cookie value for `name`.
    pub fn sign(&self, name: &str, value: &str) -> String {
        format!("{value}.{}", self.mac(name, value))
    }

    /// The value of a signed cookie, if its signature holds.
    pub fn verify(&self, name: &str, signed: &str) -> Option<String> {
        let (v, mac) = signed.rsplit_once('.')?;
        let want = self.mac(name, v);
        // Constant time over equal lengths.
        let ok = want.len() == mac.len()
            && want
                .bytes()
                .zip(mac.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;
        ok.then(|| v.to_string())
    }

    pub fn set_cookie(&self, name: &str, value: &str, max_age: Duration) -> String {
        let (path, same_site) = match name {
            FLOW_COOKIE => ("/auth", "None"),
            LOGIN_COOKIE => ("/auth", "Strict"),
            _ => ("/", "Strict"),
        };
        format!(
            "{name}={}; Path={path}; HttpOnly; Secure; SameSite={same_site}; Max-Age={}",
            self.sign(name, value),
            max_age.as_secs()
        )
    }

    pub fn clear_cookie(&self, name: &str) -> String {
        let path = if matches!(name, FLOW_COOKIE | LOGIN_COOKIE) {
            "/auth"
        } else {
            "/"
        };
        format!("{name}=; Path={path}; HttpOnly; Secure; SameSite=Strict; Max-Age=0")
    }

    /// The verified value of cookie `name` in a `Cookie` header.
    pub fn cookie(&self, header: Option<&str>, name: &str) -> Option<String> {
        header?
            .split(';')
            .filter_map(|c| c.trim().split_once('='))
            .filter(|(k, _)| *k == name)
            .find_map(|(_, v)| self.verify(name, v))
    }

    // ---- sessions ----

    fn new_session(&self, user: String, via: String) -> Session {
        let s = Session {
            id: random_hex(),
            user,
            via,
            csrf: random_hex(),
            expires_at: now_secs() + self.console.session_ttl().as_secs() as i64,
        };
        let key = session_key(&s.id);
        self.sessions
            .lock()
            .expect("sessions")
            .insert(key.clone(), s.clone());
        self.persist(Persist::Put(key, s.clone()));
        s
    }

    /// Queue a write to the hub; nothing while the hub is not connected yet.
    pub fn persist(&self, p: Persist) {
        if let Some(tx) = self.persist.lock().expect("persist").as_ref() {
            let _ = tx.send(p);
        }
    }

    /// Take the sessions kept on the hub (a restart), then send every change there.
    /// Sessions made before the hub answered are written now.
    pub fn attach(
        &self,
        stored: Vec<(String, Session)>,
        tx: tokio::sync::mpsc::UnboundedSender<Persist>,
    ) {
        let now = now_secs();
        let mut all = self.sessions.lock().expect("sessions");
        for (key, s) in stored {
            if s.expires_at > now && self.console.member(&s.user).is_some() {
                all.entry(key).or_insert(s);
            } else {
                let _ = tx.send(Persist::Del(key));
            }
        }
        for (key, s) in all.iter() {
            let _ = tx.send(Persist::Put(key.clone(), s.clone()));
        }
        *self.persist.lock().expect("persist") = Some(tx);
    }

    /// The session cookie for a new sign-in.
    pub fn session_cookie(&self, s: &Session) -> String {
        self.set_cookie(SESSION_COOKIE, &s.id, self.console.session_ttl())
    }

    /// The live session a `Cookie` header names; an identity no longer on the allow-list
    /// has none.
    pub fn session(&self, cookie_header: Option<&str>) -> Option<Session> {
        let id = self.cookie(cookie_header, SESSION_COOKIE)?;
        let key = session_key(&id);
        let mut all = self.sessions.lock().expect("sessions");
        let mut s = all.get(&key)?.clone();
        if s.expires_at <= now_secs() || self.console.member(&s.user).is_none() {
            all.remove(&key);
            drop(all);
            self.persist(Persist::Del(key));
            return None;
        }
        s.id = id;
        Some(s)
    }

    pub fn sign_out(&self, id: &str) {
        let key = session_key(id);
        self.sessions.lock().expect("sessions").remove(&key);
        self.persist(Persist::Del(key));
    }

    fn sweep(&self) {
        let now = Instant::now();
        self.flows
            .lock()
            .expect("flows")
            .retain(|_, f| now.duration_since(f.started) < PENDING_TTL);
        self.enrolments
            .lock()
            .expect("enrolments")
            .retain(|_, e| now.duration_since(e.started) < PENDING_TTL);
        self.logins
            .lock()
            .expect("logins")
            .retain(|_, (_, t)| now.duration_since(*t) < PENDING_TTL);
        let mut gone = Vec::new();
        self.sessions.lock().expect("sessions").retain(|k, s| {
            let keep = s.expires_at > now_secs();
            if !keep {
                gone.push(k.clone());
            }
            keep
        });
        for k in gone {
            self.persist(Persist::Del(k));
        }
    }

    // ---- OAuth ----

    fn redirect_uri(&self, p: Provider) -> String {
        match p {
            Provider::Github => format!("{}/auth/github/callback", self.origin),
            Provider::Apple => format!("{}/auth/apple/callback", self.origin),
            Provider::Google => format!("{}/auth/google/callback", self.origin),
        }
    }

    /// Start an OAuth flow: the provider URL to send the browser to, and the value of the
    /// flow cookie that binds the browser.
    pub fn oauth_start(&self, p: Provider) -> Result<(String, String), AuthError> {
        self.sweep();
        let state = random_hex();
        let binding = random_hex();
        let nonce = random_hex();
        let verifier = b64(&hex::decode(random_hex()).expect("hex"));
        let redirect = self.redirect_uri(p);
        let url = match p {
            Provider::Github => {
                let g = self
                    .console
                    .github
                    .as_ref()
                    .ok_or_else(|| bad("GitHub sign-in is not configured"))?;
                let mut u =
                    Url::parse(&g.authorize_url).map_err(|e| AuthError::Setup(e.to_string()))?;
                // `user:email`: the verified emails a user without a configured login
                // is found by.
                u.query_pairs_mut()
                    .append_pair("client_id", &g.client_id)
                    .append_pair("redirect_uri", &redirect)
                    .append_pair("scope", "user:email")
                    .append_pair("state", &state)
                    .append_pair("allow_signup", "false");
                u.to_string()
            }
            Provider::Apple => {
                let a = self
                    .console
                    .apple
                    .as_ref()
                    .ok_or_else(|| bad("Sign in with Apple is not configured"))?;
                let mut u =
                    Url::parse(&a.authorize_url).map_err(|e| AuthError::Setup(e.to_string()))?;
                u.query_pairs_mut()
                    .append_pair("client_id", &a.client_id)
                    .append_pair("redirect_uri", &redirect)
                    .append_pair("response_type", "code")
                    .append_pair("scope", "email")
                    .append_pair("response_mode", "form_post")
                    .append_pair("state", &state)
                    .append_pair("nonce", &nonce);
                u.to_string()
            }
            Provider::Google => {
                let g = self
                    .console
                    .google
                    .as_ref()
                    .ok_or_else(|| bad("Google sign-in is not configured"))?;
                let mut u =
                    Url::parse(&g.authorize_url).map_err(|e| AuthError::Setup(e.to_string()))?;
                use sha2::Digest as _;
                let challenge = b64(&Sha256::digest(verifier.as_bytes()));
                u.query_pairs_mut()
                    .append_pair("client_id", &g.client_id)
                    .append_pair("redirect_uri", &redirect)
                    .append_pair("response_type", "code")
                    .append_pair("scope", "openid email")
                    .append_pair("state", &state)
                    .append_pair("nonce", &nonce)
                    .append_pair("code_challenge", &challenge)
                    .append_pair("code_challenge_method", "S256")
                    .append_pair("prompt", "select_account");
                u.to_string()
            }
        };
        self.flows.lock().expect("flows").insert(
            state,
            Flow {
                provider: p,
                binding: binding.clone(),
                nonce,
                verifier,
                started: Instant::now(),
            },
        );
        Ok((url, binding))
    }

    fn take_flow(
        &self,
        p: Provider,
        state: &str,
        binding: Option<&str>,
    ) -> Result<Flow, AuthError> {
        let f = self
            .flows
            .lock()
            .expect("flows")
            .remove(state)
            .ok_or_else(|| bad("this sign-in link is unknown or used; start again"))?;
        if f.provider != p || f.started.elapsed() >= PENDING_TTL {
            return Err(bad("this sign-in expired; start again"));
        }
        if binding != Some(f.binding.as_str()) {
            return Err(bad(
                "this sign-in was started in another browser; start again",
            ));
        }
        Ok(f)
    }

    /// GitHub's callback: the user behind `code`, by login or by a verified email.
    pub async fn github_callback(
        &self,
        code: &str,
        state: &str,
        binding: Option<&str>,
    ) -> Result<(String, Identity), AuthError> {
        self.take_flow(Provider::Github, state, binding)?;
        let g = self
            .console
            .github
            .as_ref()
            .ok_or_else(|| bad("GitHub sign-in is not configured"))?;
        let secret = read_secret(&expand(&g.secret_file))?;
        let (login, token) = github_login(
            &self.http,
            g,
            &secret,
            code,
            &self.redirect_uri(Provider::Github),
        )
        .await?;
        let emails = if self.console.github_needs_emails(&login) {
            github_emails(&self.http, g, &token).await?
        } else {
            Vec::new()
        };
        self.allowed(Identity::Github(login), &emails)
    }

    /// Apple's callback (a form POST): the account email behind `code`.
    pub async fn apple_callback(
        &self,
        code: &str,
        state: &str,
        binding: Option<&str>,
    ) -> Result<(String, Identity), AuthError> {
        let flow = self.take_flow(Provider::Apple, state, binding)?;
        let a = self
            .console
            .apple
            .as_ref()
            .ok_or_else(|| bad("Sign in with Apple is not configured"))?;
        let key = std::fs::read_to_string(expand(&a.key_file))
            .map_err(|e| AuthError::Setup(format!("{}: {e}", a.key_file)))?;
        read_secret(&expand(&a.key_file))?;
        let secret = apple_client_secret(a, &key, chrono::Utc::now().timestamp())?;
        let t = token_reply(
            self.http
                .post(&a.token_url)
                .header("Accept", "application/json")
                .form(&[
                    ("client_id", a.client_id.as_str()),
                    ("client_secret", secret.as_str()),
                    ("code", code),
                    ("grant_type", "authorization_code"),
                    ("redirect_uri", &self.redirect_uri(Provider::Apple)),
                ])
                .send()
                .await,
            "Apple",
        )
        .await?;
        let email = self
            .id_token_email(
                "Apple",
                &t,
                &a.jwks_url,
                &[APPLE_ISSUER],
                &a.client_id,
                &flow.nonce,
            )
            .await?;
        self.allowed(Identity::Apple(email), &[])
    }

    /// Google's callback: the account email behind `code` (OpenID Connect, PKCE).
    pub async fn google_callback(
        &self,
        code: &str,
        state: &str,
        binding: Option<&str>,
    ) -> Result<(String, Identity), AuthError> {
        let flow = self.take_flow(Provider::Google, state, binding)?;
        let g = self
            .console
            .google
            .as_ref()
            .ok_or_else(|| bad("Google sign-in is not configured"))?;
        let secret = read_secret(&expand(&g.secret_file))?;
        let t = token_reply(
            self.http
                .post(&g.token_url)
                .header("Accept", "application/json")
                .form(&[
                    ("client_id", g.client_id.as_str()),
                    ("client_secret", secret.as_str()),
                    ("code", code),
                    ("code_verifier", flow.verifier.as_str()),
                    ("grant_type", "authorization_code"),
                    ("redirect_uri", &self.redirect_uri(Provider::Google)),
                ])
                .send()
                .await,
            "Google",
        )
        .await?;
        let email = self
            .id_token_email(
                "Google",
                &t,
                &g.jwks_url,
                &GOOGLE_ISSUERS,
                &g.client_id,
                &flow.nonce,
            )
            .await?;
        self.allowed(Identity::Google(email), &[])
    }

    /// The verified email of a token reply's `id_token`: its signature against the
    /// provider's published keys, then issuer, audience, expiry, nonce, `email_verified`.
    async fn id_token_email(
        &self,
        who: &str,
        t: &TokenReply,
        jwks_url: &str,
        issuers: &[&str],
        audience: &str,
        nonce: &str,
    ) -> Result<String, AuthError> {
        let idt = t
            .id_token
            .clone()
            .ok_or_else(|| AuthError::Provider(format!("{who}: {}", t.failure())))?;
        let p = |m: String| AuthError::Provider(format!("{who}: {m}"));
        let keys = self
            .jwks
            .keys(&self.http, jwks_url, crate::jwt::kid(&idt).as_deref())
            .await
            .map_err(p)?;
        let claims = crate::jwt::verify(&idt, &keys).map_err(p)?;
        crate::jwt::check(
            &claims,
            &crate::jwt::Expect {
                issuers,
                audience,
                nonce,
                now: chrono::Utc::now().timestamp(),
            },
        )
        .map_err(p)
    }

    /// The user an identity signs in as (`[console] users`); none, or two, is refused.
    fn allowed(
        &self,
        id: Identity,
        github_emails: &[String],
    ) -> Result<(String, Identity), AuthError> {
        match self.console.resolve(&id, github_emails) {
            Ok(m) => Ok((m.key, id)),
            Err(hgl_ai_config::Unresolved::NotListed) => {
                tracing::warn!(identity = %id, "refused a sign-in not on [console] users");
                Err(AuthError::NotAllowed(id.to_string()))
            }
            Err(hgl_ai_config::Unresolved::Ambiguous(users)) => {
                tracing::warn!(identity = %id, "refused a sign-in that matches {users:?}");
                Err(AuthError::Bad(format!(
                    "{id} matches more than one console user; link it in [console] users"
                )))
            }
        }
    }

    // ---- passkeys ----

    /// After a provider vouched for `user` (signed in `via`): an enrolment ticket (the
    /// enrol cookie).
    pub fn enrol_ticket(&self, user: String, via: Identity) -> String {
        let t = random_hex();
        self.enrolments.lock().expect("enrolments").insert(
            t.clone(),
            Enrolment {
                user,
                via,
                reg: None,
                started: Instant::now(),
            },
        );
        t
    }

    fn enrolment_user(&self, ticket: &str) -> Option<String> {
        let e = self.enrolments.lock().expect("enrolments");
        e.get(ticket)
            .filter(|e| e.started.elapsed() < PENDING_TTL)
            .map(|e| e.user.clone())
    }

    pub fn register_start(&self, ticket: &str) -> Result<CreationChallengeResponse, AuthError> {
        let user = self
            .enrolment_user(ticket)
            .ok_or_else(|| bad("this enrolment expired; sign in with a provider again"))?;
        let (uuid, exclude) = {
            let st = self.store.lock().expect("store");
            match st.users.iter().find(|u| u.user == user) {
                Some(u) => (
                    u.uuid,
                    u.passkeys
                        .iter()
                        .map(|p| p.passkey.cred_id().clone())
                        .collect(),
                ),
                None => (Uuid::from_bytes(random_bytes16()), Vec::new()),
            }
        };
        let shown = self.handle_of(&user).unwrap_or_else(|| user.clone());
        let (ccr, reg) = self
            .webauthn
            .start_passkey_registration(uuid, &user, &shown, Some(exclude))
            .map_err(|e| bad(format!("starting the passkey: {e}")))?;
        let mut all = self.enrolments.lock().expect("enrolments");
        let e = all
            .get_mut(ticket)
            .ok_or_else(|| bad("this enrolment expired"))?;
        e.reg = Some(reg);
        // Keep the user's id for finish.
        drop(all);
        self.pending_uuid(&user, uuid);
        Ok(ccr)
    }

    fn pending_uuid(&self, user: &str, uuid: Uuid) {
        let mut st = self.store.lock().expect("store");
        if !st.users.iter().any(|u| u.user == user) {
            st.users.push(StoredUser {
                user: user.to_string(),
                uuid,
                passkeys: Vec::new(),
            });
        }
    }

    /// Store the new passkey and sign the user in.
    pub fn register_finish(
        &self,
        ticket: &str,
        cred: &RegisterPublicKeyCredential,
    ) -> Result<Session, AuthError> {
        let expired = || bad("this enrolment expired; sign in with a provider again");
        let (user, via, reg) = {
            let mut all = self.enrolments.lock().expect("enrolments");
            let e = all.get(ticket).ok_or_else(expired)?;
            if e.started.elapsed() >= PENDING_TTL {
                all.remove(ticket);
                return Err(expired());
            }
            let reg = e
                .reg
                .clone()
                .ok_or_else(|| bad("the passkey was not started"))?;
            (e.user.clone(), e.via.clone(), reg)
        };
        // A failed ceremony keeps the ticket: the browser may start the passkey again.
        let pk = self
            .webauthn
            .finish_passkey_registration(cred, &reg)
            .map_err(|err| bad(format!("the passkey did not register: {err}")))?;
        self.enrolments.lock().expect("enrolments").remove(ticket);
        let e = Enrolment {
            user,
            via,
            reg: None,
            started: Instant::now(),
        };
        {
            let mut st = self.store.lock().expect("store");
            let u = st
                .users
                .iter_mut()
                .find(|u| u.user == e.user)
                .ok_or_else(|| bad("no such user"))?;
            u.passkeys.push(StoredPasskey {
                passkey: pk,
                added_at: hgl_ai_proto::utc_now_stamp(),
                last_used: None,
            });
            self.save(&st)?;
        }
        tracing::info!(user = %e.user, via = %e.via, "registered a passkey");
        Ok(self.new_session(e.user, e.via.to_string()))
    }

    fn save(&self, st: &Store) -> Result<(), AuthError> {
        let text = serde_json::to_vec_pretty(st).map_err(|e| AuthError::Setup(e.to_string()))?;
        write_private(&self.store_path, &text)
            .map_err(|e| AuthError::Setup(format!("{}: {e}", self.store_path.display())))
    }

    /// Whether any allowed user has a passkey.
    pub fn any_passkey(&self) -> bool {
        let st = self.store.lock().expect("store");
        st.users
            .iter()
            .any(|u| self.console.member(&u.user).is_some() && !u.passkeys.is_empty())
    }

    /// A passkey challenge over every allowed user's passkeys, and the login cookie value.
    pub fn login_start(&self) -> Result<(RequestChallengeResponse, String), AuthError> {
        self.sweep();
        let keys: Vec<Passkey> = {
            let st = self.store.lock().expect("store");
            st.users
                .iter()
                .filter(|u| self.console.member(&u.user).is_some())
                .flat_map(|u| u.passkeys.iter().map(|p| p.passkey.clone()))
                .collect()
        };
        if keys.is_empty() {
            return Err(bad(
                "no passkey is registered yet: sign in with GitHub or Apple first",
            ));
        }
        let (rcr, st) = self
            .webauthn
            .start_passkey_authentication(&keys)
            .map_err(|e| bad(format!("starting the sign-in: {e}")))?;
        let id = random_hex();
        self.logins
            .lock()
            .expect("logins")
            .insert(id.clone(), (st, Instant::now()));
        Ok((rcr, id))
    }

    pub fn login_finish(
        &self,
        login: &str,
        cred: &PublicKeyCredential,
    ) -> Result<Session, AuthError> {
        let (st, started) = self
            .logins
            .lock()
            .expect("logins")
            .remove(login)
            .ok_or_else(|| bad("this sign-in expired; try again"))?;
        if started.elapsed() >= PENDING_TTL {
            return Err(bad("this sign-in expired; try again"));
        }
        let res = self
            .webauthn
            .finish_passkey_authentication(cred, &st)
            .map_err(|e| bad(format!("the passkey was not accepted: {e}")))?;
        let mut store = self.store.lock().expect("store");
        let user = {
            let (u, p) = store
                .users
                .iter_mut()
                .find_map(|u| {
                    let i = u
                        .passkeys
                        .iter()
                        .position(|p| p.passkey.cred_id() == res.cred_id())?;
                    Some((u, i))
                })
                .ok_or_else(|| bad("unknown passkey"))?;
            let sp = &mut u.passkeys[p];
            sp.passkey.update_credential(&res);
            sp.last_used = Some(hgl_ai_proto::utc_now_stamp());
            u.user.clone()
        };
        if self.console.member(&user).is_none() {
            return Err(AuthError::NotAllowed(user));
        }
        self.save(&store)?;
        drop(store);
        Ok(self.new_session(user, "passkey".into()))
    }
}

fn random_bytes16() -> [u8; 16] {
    let h = random_hex();
    let mut b = [0u8; 16];
    b.copy_from_slice(&hex::decode(&h[..32]).expect("hex"));
    b
}

#[derive(Deserialize)]
struct TokenReply {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

impl TokenReply {
    fn failure(&self) -> String {
        format!(
            "{}{}",
            self.error.as_deref().unwrap_or("no token"),
            self.error_description
                .as_deref()
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        )
    }
}

async fn token_reply(
    resp: reqwest::Result<reqwest::Response>,
    who: &str,
) -> Result<TokenReply, AuthError> {
    let r = resp.map_err(|e| AuthError::Provider(format!("{who}: {}", e.without_url())))?;
    let status = r.status();
    let text = r
        .text()
        .await
        .map_err(|e| AuthError::Provider(format!("{who}: {}", e.without_url())))?;
    serde_json::from_str(&text)
        .map_err(|_| AuthError::Provider(format!("{who}: unexpected answer (HTTP {status})")))
}

/// Exchange a GitHub code for the signed-in login.
async fn github_login(
    http: &reqwest::Client,
    g: &GithubOAuth,
    secret: &str,
    code: &str,
    redirect: &str,
) -> Result<(String, String), AuthError> {
    let t = token_reply(
        http.post(&g.token_url)
            .header("Accept", "application/json")
            .form(&[
                ("client_id", g.client_id.as_str()),
                ("client_secret", secret),
                ("code", code),
                ("redirect_uri", redirect),
            ])
            .send()
            .await,
        "GitHub",
    )
    .await?;
    let token = t
        .access_token
        .clone()
        .ok_or_else(|| AuthError::Provider(format!("GitHub: {}", t.failure())))?;
    #[derive(Deserialize)]
    struct User {
        login: String,
    }
    let u: User = http
        .get(format!("{}/user", g.api_url.trim_end_matches('/')))
        .header("Accept", "application/vnd.github+json")
        .bearer_auth(&token)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| AuthError::Provider(format!("GitHub user: {}", e.without_url())))?
        .json()
        .await
        .map_err(|e| AuthError::Provider(format!("GitHub user: {}", e.without_url())))?;
    if u.login.is_empty() {
        return Err(AuthError::Provider("GitHub: no login".into()));
    }
    Ok((u.login, token))
}

/// The GitHub account's verified emails (`GET /user/emails`, scope `user:email`).
async fn github_emails(
    http: &reqwest::Client,
    g: &GithubOAuth,
    token: &str,
) -> Result<Vec<String>, AuthError> {
    #[derive(Deserialize)]
    struct Email {
        email: String,
        #[serde(default)]
        verified: bool,
    }
    let v: Vec<Email> = http
        .get(format!("{}/user/emails", g.api_url.trim_end_matches('/')))
        .header("Accept", "application/vnd.github+json")
        .bearer_auth(token)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| AuthError::Provider(format!("GitHub emails: {}", e.without_url())))?
        .json()
        .await
        .map_err(|e| AuthError::Provider(format!("GitHub emails: {}", e.without_url())))?;
    Ok(v.into_iter()
        .filter(|e| e.verified)
        .map(|e| e.email)
        .collect())
}

/// Apple's issuer and the audience of its client secrets.
pub const APPLE_ISSUER: &str = "https://appleid.apple.com";

/// The client secret Apple's token endpoint takes: an ES256 JWT signed with the
/// Services ID's key, valid five minutes from `now`.
pub fn apple_client_secret(a: &AppleOAuth, key_pem: &str, now: i64) -> Result<String, AuthError> {
    use p256::ecdsa::signature::Signer as _;
    use p256::pkcs8::DecodePrivateKey as _;
    let key = p256::ecdsa::SigningKey::from_pkcs8_pem(key_pem)
        .map_err(|e| AuthError::Setup(format!("{}: not a P-256 PKCS#8 key: {e}", a.key_file)))?;
    let header = serde_json::json!({ "alg": "ES256", "kid": a.key_id });
    let claims = serde_json::json!({
        "iss": a.team_id,
        "iat": now,
        "exp": now + 300,
        "aud": APPLE_ISSUER,
        "sub": a.client_id,
    });
    let signing_input = format!(
        "{}.{}",
        b64(header.to_string().as_bytes()),
        b64(claims.to_string().as_bytes())
    );
    let sig: p256::ecdsa::Signature = key.sign(signing_input.as_bytes());
    Ok(format!("{signing_input}.{}", b64(&sig.to_bytes())))
}

/// The issuers of Google's `id_token`s.
pub const GOOGLE_ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];

#[cfg(test)]
mod tests {
    use super::*;

    fn console(dir: &Path) -> (Console, Auth) {
        let c = Console {
            url: "https://console.example.com".into(),
            users: vec![hgl_ai_config::User::person(
                "owner@example.com",
                "This Dude",
            )],
            ..Console::default()
        };
        let a = Auth::new(&c, dir).unwrap();
        (c, a)
    }

    #[test]
    fn signed_cookies() {
        let d = tempfile::tempdir().unwrap();
        let (_, a) = console(d.path());
        let v = a.sign(SESSION_COOKIE, "abc");
        assert_eq!(a.verify(SESSION_COOKIE, &v).as_deref(), Some("abc"));
        assert_eq!(a.verify(ENROL_COOKIE, &v), None, "bound to its name");
        assert_eq!(a.verify(SESSION_COOKIE, &v.replace("abc", "abd")), None);
        let set = a.set_cookie(SESSION_COOKIE, "abc", Duration::from_secs(60));
        assert!(
            set.contains("HttpOnly; Secure; SameSite=Strict; Max-Age=60"),
            "{set}"
        );
        let header = format!("x=1; {SESSION_COOKIE}={v}");
        assert_eq!(
            a.cookie(Some(&header), SESSION_COOKIE).as_deref(),
            Some("abc")
        );
        assert!(
            a.set_cookie(FLOW_COOKIE, "f", PENDING_TTL)
                .contains("Path=/auth; HttpOnly; Secure; SameSite=None")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let m = std::fs::metadata(d.path().join("cookie.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(m & 0o777, 0o600);
        }
        // The key survives a restart: an old cookie still verifies.
        let (_, b) = console(d.path());
        assert_eq!(b.verify(SESSION_COOKIE, &v).as_deref(), Some("abc"));
    }

    #[test]
    fn sessions_follow_the_allow_list() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, a) = console(d.path());
        let s = a.new_session("owner@example.com".into(), "passkey".into());
        let header = format!("{SESSION_COOKIE}={}", a.sign(SESSION_COOKIE, &s.id));
        assert!(a.session(Some(&header)).is_some());
        a.sign_out(&s.id);
        assert!(a.session(Some(&header)).is_none());
        c.users.clear();
        let b = Auth::new(&c, d.path()).unwrap();
        let s = b.new_session("owner@example.com".into(), "passkey".into());
        let header = format!("{SESSION_COOKIE}={}", b.sign(SESSION_COOKIE, &s.id));
        assert!(b.session(Some(&header)).is_none(), "removed from users");
    }

    #[test]
    fn an_expired_enrolment_registers_nothing() {
        let d = tempfile::tempdir().unwrap();
        let (_, a) = console(d.path());
        let t = a.enrol_ticket(
            "owner@example.com".into(),
            Identity::Github("hhenson".into()),
        );
        a.enrolments.lock().unwrap().get_mut(&t).unwrap().started = Instant::now()
            .checked_sub(PENDING_TTL + Duration::from_secs(1))
            .unwrap();
        let cred: RegisterPublicKeyCredential = serde_json::from_value(serde_json::json!({
            "id": "AA", "rawId": "AA", "type": "public-key", "extensions": {},
            "response": { "attestationObject": "AA", "clientDataJSON": "AA" }
        }))
        .unwrap();
        let e = a.register_finish(&t, &cred).unwrap_err().to_string();
        assert!(e.contains("expired"), "{e}");
        assert!(a.enrolments.lock().unwrap().get(&t).is_none(), "spent");

        // A failed ceremony keeps a live ticket for another try.
        let t = a.enrol_ticket(
            "owner@example.com".into(),
            Identity::Github("hhenson".into()),
        );
        a.register_start(&t).unwrap();
        assert!(a.register_finish(&t, &cred).is_err());
        assert!(a.enrolments.lock().unwrap().get(&t).is_some(), "kept");
    }

    #[cfg(unix)]
    #[test]
    fn secrets_need_mode_600() {
        use std::os::unix::fs::PermissionsExt as _;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s");
        std::fs::write(&p, "x\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_secret(&p).unwrap_err().to_string().contains("0600"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_secret(&p).unwrap(), "x");
    }
}
