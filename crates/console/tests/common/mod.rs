//! A console on loopback (plain HTTP, as behind a TLS proxy), a cookie jar, mock OAuth
//! providers and a software passkey. No real network.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Form, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use base64::Engine as _;
use hgl_ai_bus::Credential;
use hgl_ai_config::{AppleOAuth, Config, Console, GithubOAuth, GoogleOAuth, Person, User};
use hgl_ai_console::App;
use hgl_ai_install::nats::{Auth, render_conf};
use hgl_ai_install::users::{Registry, Role};
use webauthn_authenticator_rs::WebauthnAuthenticator as _;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;
use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse, Url};

pub const ORIGIN: &str = "https://console.example.com";
pub const APPLE_CLIENT: &str = "com.example.console";
pub const GOOGLE_CLIENT: &str = "0123-example.apps.googleusercontent.com";

pub fn write_secret(p: &Path, text: &str) {
    std::fs::write(p, text).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(p, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
}

fn b64(d: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(d)
}

/// Mock GitHub, Apple and Google: token, user and signing-key endpoints.
pub struct Providers {
    pub addr: SocketAddr,
    /// The Apple client secrets the token endpoint received.
    pub apple_secrets: Arc<Mutex<Vec<String>>>,
    pub apple_key: p256::ecdsa::VerifyingKey,
    /// How often each provider's keys were fetched.
    pub jwks_fetches: Arc<Mutex<BTreeMap<String, u32>>>,
}

#[derive(Clone)]
struct Mock {
    apple_secrets: Arc<Mutex<Vec<String>>>,
    /// The provider's signing key (ES256) and a key nobody published.
    signer: Arc<p256::ecdsa::SigningKey>,
    stranger: Arc<p256::ecdsa::SigningKey>,
    jwks_fetches: Arc<Mutex<BTreeMap<String, u32>>>,
}

fn ec_key() -> p256::ecdsa::SigningKey {
    loop {
        let mut b = [0u8; 32];
        use std::io::Read as _;
        std::fs::File::open("/dev/urandom")
            .unwrap()
            .read_exact(&mut b)
            .unwrap();
        if let Ok(k) = p256::ecdsa::SigningKey::from_slice(&b) {
            return k;
        }
    }
}

/// An ES256 JWT signed with `key` under `kid`.
fn es256(key: &p256::ecdsa::SigningKey, kid: &str, claims: &serde_json::Value) -> String {
    use p256::ecdsa::signature::Signer as _;
    let head = b64(serde_json::json!({ "alg": "ES256", "kid": kid })
        .to_string()
        .as_bytes());
    let si = format!("{head}.{}", b64(claims.to_string().as_bytes()));
    let sig: p256::ecdsa::Signature = key.sign(si.as_bytes());
    format!("{si}.{}", b64(&sig.to_bytes()))
}

fn jwks_of(key: &p256::ecdsa::SigningKey) -> serde_json::Value {
    let point = key.verifying_key().to_sec1_point(false);
    let p = point.as_bytes();
    serde_json::json!({ "keys": [{
        "kty": "EC", "crv": "P-256", "alg": "ES256", "use": "sig", "kid": "mock-ec",
        "x": b64(&p[1..33]), "y": b64(&p[33..65]),
    }]})
}

async fn jwks(
    State(m): State<Mock>,
    axum::extract::Path(who): axum::extract::Path<String>,
) -> impl axum::response::IntoResponse {
    *m.jwks_fetches.lock().unwrap().entry(who).or_default() += 1;
    (
        [("cache-control", "public, max-age=300")],
        axum::Json(jwks_of(&m.signer)),
    )
}

/// Google's token endpoint. The test puts what it wants in the code:
/// `good:<email>:<nonce>:<code_challenge>[:badsig|aud|unverified]`.
async fn google_token(
    State(m): State<Mock>,
    Form(f): Form<BTreeMap<String, String>>,
) -> axum::Json<serde_json::Value> {
    use sha2::Digest as _;
    let g = |k: &str| f.get(k).cloned().unwrap_or_default();
    let code = g("code");
    let parts: Vec<&str> = code.split(':').collect();
    let pkce_ok =
        parts.len() >= 4 && b64(&sha2::Sha256::digest(g("code_verifier").as_bytes())) == parts[3];
    if parts[0] != "good"
        || !pkce_ok
        || g("client_secret") != "google-secret"
        || g("client_id") != GOOGLE_CLIENT
        || g("grant_type") != "authorization_code"
        || g("redirect_uri") != "https://console.example.com/auth/google/callback"
    {
        return axum::Json(serde_json::json!({ "error": "invalid_grant" }));
    }
    let flag = parts.get(4).copied().unwrap_or("");
    let claims = serde_json::json!({
        "iss": "https://accounts.google.com",
        "aud": if flag == "aud" { "someone-else.apps.googleusercontent.com" } else { GOOGLE_CLIENT },
        "sub": "1234567890",
        "iat": chrono::Utc::now().timestamp(),
        "exp": chrono::Utc::now().timestamp() + 600,
        "nonce": parts[2],
        "email": parts[1],
        "email_verified": flag != "unverified",
    });
    let key = if flag == "badsig" {
        &m.stranger
    } else {
        &m.signer
    };
    axum::Json(
        serde_json::json!({ "id_token": es256(key, "mock-ec", &claims), "access_token": "a" }),
    )
}

async fn gh_token(Form(f): Form<BTreeMap<String, String>>) -> axum::Json<serde_json::Value> {
    let ok = f.get("client_secret").map(String::as_str) == Some("gh-secret")
        && f.get("client_id").map(String::as_str) == Some("Iv1.example")
        && f.get("redirect_uri").map(String::as_str)
            == Some("https://console.example.com/auth/github/callback");
    let code = f.get("code").cloned().unwrap_or_default();
    axum::Json(if ok && code.starts_with("good-") {
        serde_json::json!({ "access_token": format!("tok-{}", &code[5..]) })
    } else {
        serde_json::json!({ "error": "bad_verification_code" })
    })
}

/// The verified emails of the mock accounts.
async fn gh_emails(h: HeaderMap) -> Result<axum::Json<serde_json::Value>, axum::http::StatusCode> {
    let auth = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let login = auth
        .strip_prefix("Bearer tok-")
        .ok_or(axum::http::StatusCode::UNAUTHORIZED)?;
    let e = |email: &str, verified: bool| serde_json::json!({ "email": email, "verified": verified, "primary": false });
    Ok(axum::Json(serde_json::Value::Array(match login {
        "second" => vec![e("Second@example.com", true), e("other@example.com", true)],
        "both" => vec![e("c@example.com", true), e("d@example.com", true)],
        "unverified" => vec![e("second@example.com", false)],
        _ => vec![],
    })))
}

async fn gh_user(h: HeaderMap) -> Result<axum::Json<serde_json::Value>, axum::http::StatusCode> {
    let auth = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    match auth.strip_prefix("Bearer tok-") {
        Some(login) => Ok(axum::Json(serde_json::json!({ "login": login }))),
        None => Err(axum::http::StatusCode::UNAUTHORIZED),
    }
}

async fn apple_token(
    State(m): State<Mock>,
    Form(f): Form<BTreeMap<String, String>>,
) -> axum::Json<serde_json::Value> {
    m.apple_secrets
        .lock()
        .unwrap()
        .push(f.get("client_secret").cloned().unwrap_or_default());
    // The test puts the email and nonce in the code: `good:<email>:<nonce>[:badsig]`.
    let code = f.get("code").cloned().unwrap_or_default();
    let parts: Vec<&str> = code.split(':').collect();
    if parts.len() < 3 || parts[0] != "good" {
        return axum::Json(serde_json::json!({ "error": "invalid_grant" }));
    }
    let claims = serde_json::json!({
        "iss": "https://appleid.apple.com",
        "aud": APPLE_CLIENT,
        "exp": chrono::Utc::now().timestamp() + 600,
        "nonce": parts[2],
        "email": parts[1],
        "email_verified": "true",
    });
    let key = if parts.get(3) == Some(&"badsig") {
        &m.stranger
    } else {
        &m.signer
    };
    axum::Json(
        serde_json::json!({ "id_token": es256(key, "mock-ec", &claims), "access_token": "a" }),
    )
}

impl Providers {
    pub async fn start(apple_key: p256::ecdsa::VerifyingKey) -> Providers {
        let m = Mock {
            apple_secrets: Arc::default(),
            signer: Arc::new(ec_key()),
            stranger: Arc::new(ec_key()),
            jwks_fetches: Arc::default(),
        };
        let app = Router::new()
            .route("/login/oauth/access_token", post(gh_token))
            .route("/user", get(gh_user))
            .route("/user/emails", get(gh_emails))
            .route("/auth/token", post(apple_token))
            .route("/google/token", post(google_token))
            .route("/keys/{who}", get(jwks))
            .with_state(m.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await });
        Providers {
            addr,
            apple_secrets: m.apple_secrets,
            apple_key,
            jwks_fetches: m.jwks_fetches,
        }
    }
}

/// A fresh P-256 key as PKCS#8 PEM (Apple's `.p8` form), and its public half.
pub fn apple_key() -> (String, p256::ecdsa::VerifyingKey) {
    use p256::pkcs8::EncodePrivateKey as _;
    loop {
        let mut b = [0u8; 32];
        use std::io::Read as _;
        std::fs::File::open("/dev/urandom")
            .unwrap()
            .read_exact(&mut b)
            .unwrap();
        if let Ok(k) = p256::ecdsa::SigningKey::from_slice(&b) {
            let pem = k.to_pkcs8_pem(p256::pkcs8::LineEnding::LF).unwrap();
            return (pem.to_string(), *k.verifying_key());
        }
    }
}

pub struct Harness {
    pub dir: tempfile::TempDir,
    pub cfg: Config,
}

impl Harness {
    pub fn new(providers: &Providers, apple_pem: &str) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let gh_secret = dir.path().join("github.secret");
        write_secret(&gh_secret, "gh-secret\n");
        let p8 = dir.path().join("apple.p8");
        write_secret(&p8, apple_pem);
        let base = format!("http://{}", providers.addr);
        let mut github = GithubOAuth::new("Iv1.example");
        github.secret_file = gh_secret.display().to_string();
        github.authorize_url = format!("{base}/login/oauth/authorize");
        github.token_url = format!("{base}/login/oauth/access_token");
        github.api_url = base.clone();
        let mut apple = AppleOAuth::new(APPLE_CLIENT, "TEAM123456", "KEY1234567");
        apple.key_file = p8.display().to_string();
        apple.authorize_url = format!("{base}/auth/authorize");
        apple.token_url = format!("{base}/auth/token");
        apple.jwks_url = format!("{base}/keys/apple");
        let g_secret = dir.path().join("google.secret");
        write_secret(&g_secret, "google-secret\n");
        let mut google = GoogleOAuth::new(GOOGLE_CLIENT);
        google.secret_file = g_secret.display().to_string();
        google.authorize_url = format!("{base}/google/auth");
        google.token_url = format!("{base}/google/token");
        google.jwks_url = format!("{base}/keys/google");
        let mut cfg = Config::new("hub");
        cfg.paths.data_dir = dir.path().join("data").display().to_string();
        cfg.paths.work_root = dir.path().join("work").display().to_string();
        cfg.console = Some(Console {
            listen: "127.0.0.1:8443".into(),
            url: ORIGIN.into(),
            users: vec![
                // GitHub by login, Apple through a private relay address, Google by email.
                User::Person(Person {
                    email: "Owner@example.com".into(),
                    handle: Some("This Dude".into()),
                    github: Some("hhenson".into()),
                    apple: Some("relay@privaterelay.example.com".into()),
                    google: None,
                }),
                // GitHub by a verified email: no login configured.
                User::person("second@example.com", "Two"),
                // Two users one GitHub account has verified emails for.
                User::person("c@example.com", "Cee"),
                User::person("d@example.com", "Dee"),
            ],
            github: Some(github),
            apple: Some(apple),
            google: Some(google),
            ..Console::default()
        });
        Harness { dir, cfg }
    }
}

/// Serve the console's router on loopback; returns its base URL.
pub async fn serve(app: Arc<App>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let router = hgl_ai_console::router(app);
    tokio::spawn(async move { axum::serve(l, router).await });
    format!("http://{addr}")
}

/// A browser: cookies by hand (the console's are `Secure`, the test talks plain HTTP).
pub struct Browser {
    pub base: String,
    pub http: reqwest::Client,
    pub jar: BTreeMap<String, String>,
}

impl Browser {
    pub fn new(base: &str) -> Browser {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Browser {
            base: base.to_string(),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            jar: BTreeMap::new(),
        }
    }

    fn cookie_header(&self) -> String {
        self.jar
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn keep(&mut self, r: &reqwest::Response) {
        for v in r.headers().get_all("set-cookie") {
            let v = v.to_str().unwrap();
            assert!(v.contains("HttpOnly; Secure;"), "{v}");
            let first = v.split(';').next().unwrap();
            let (k, val) = first.split_once('=').unwrap();
            if v.contains("Max-Age=0") {
                self.jar.remove(k);
            } else {
                self.jar.insert(k.to_string(), val.to_string());
            }
        }
    }

    pub async fn get(&mut self, path: &str) -> reqwest::Response {
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{path}", self.base)
        };
        let r = self
            .http
            .get(url)
            .header("cookie", self.cookie_header())
            .send()
            .await
            .unwrap();
        self.keep(&r);
        r
    }

    /// A POST from the console's own page (its `Origin`).
    pub async fn post_json(&mut self, path: &str, body: &serde_json::Value) -> reqwest::Response {
        let r = self
            .http
            .post(format!("{}{path}", self.base))
            .header("cookie", self.cookie_header())
            .header("origin", ORIGIN)
            .json(body)
            .send()
            .await
            .unwrap();
        self.keep(&r);
        r
    }

    pub async fn post_form(
        &mut self,
        path: &str,
        form: &[(&str, &str)],
        origin: Option<&str>,
    ) -> reqwest::Response {
        let mut req = self
            .http
            .post(format!("{}{path}", self.base))
            .header("cookie", self.cookie_header())
            .form(form);
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        let r = req.send().await.unwrap();
        self.keep(&r);
        r
    }

    /// The CSRF token in a page's forms.
    pub async fn csrf(&mut self, path: &str) -> String {
        let html = self.get(path).await.text().await.unwrap();
        let at = html
            .find("name=\"csrf\" value=\"")
            .expect("a form with a csrf field");
        let rest = &html[at + 19..];
        rest[..rest.find('"').unwrap()].to_string()
    }
}

pub fn origin_url() -> Url {
    Url::parse(ORIGIN).unwrap()
}

/// Register a passkey after an OAuth callback left the enrol cookie.
pub async fn register(b: &mut Browser, key: &mut SoftPasskey) -> reqwest::Response {
    let r = b
        .post_json("/auth/passkey/register/start", &serde_json::json!({}))
        .await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let ccr: CreationChallengeResponse = r.json().await.unwrap();
    let cred = key.do_registration(origin_url(), ccr).unwrap();
    b.post_json(
        "/auth/passkey/register/finish",
        &serde_json::to_value(&cred).unwrap(),
    )
    .await
}

/// Sign in with a registered passkey.
pub async fn sign_in(b: &mut Browser, key: &mut SoftPasskey) -> reqwest::Response {
    let r = b
        .post_json("/auth/passkey/login/start", &serde_json::json!({}))
        .await;
    assert_eq!(r.status(), 200);
    let rcr: RequestChallengeResponse = r.json().await.unwrap();
    let cred = key.do_authentication(origin_url(), rcr).unwrap();
    b.post_json(
        "/auth/passkey/login/finish",
        &serde_json::to_value(&cred).unwrap(),
    )
    .await
}

/// GitHub's round trip: start, then the provider sends the browser back with `code`.
pub async fn github(b: &mut Browser, code: &str) -> reqwest::Response {
    let r = b.get("/auth/github").await;
    assert_eq!(r.status(), 303);
    let loc = r.headers()["location"].to_str().unwrap().to_string();
    let state = Url::parse(&loc)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .to_string();
    b.get(&format!("/auth/github/callback?code={code}&state={state}"))
        .await
}

/// A signed-in browser (GitHub, then a passkey).
pub async fn signed_in(base: &str) -> (Browser, SoftPasskey) {
    let mut b = Browser::new(base);
    let mut key = SoftPasskey::new(true);
    let r = github(&mut b, "good-hhenson").await;
    assert_eq!(r.status(), 200);
    let r = register(&mut b, &mut key).await;
    assert_eq!(r.status(), 200);
    (b, key)
}

pub fn path(p: &Path) -> PathBuf {
    p.to_path_buf()
}

/// A `nats-server` of the test's own, stopped when dropped.
pub struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub struct Hub {
    _server: Server,
    _dir: tempfile::TempDir,
    pub url: String,
    pub creds: std::collections::BTreeMap<&'static str, Credential>,
}

/// A hub on loopback with the `hub` (seat), `console` and `hub-admin` users; `None`
/// (the test skips) without `nats-server` on `PATH`.
pub fn start_hub() -> Option<Hub> {
    let bin = std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("nats-server"))
        .find(|p| p.is_file());
    let Some(bin) = bin else {
        eprintln!("skipped: nats-server is not on PATH");
        return None;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut reg = Registry::default();
    let mut creds = std::collections::BTreeMap::new();
    for (n, r) in [
        ("hub", Role::Seat),
        ("console", Role::Console),
        ("hub-admin", Role::Admin),
    ] {
        let t = reg.issue(n, r, false).unwrap();
        creds.insert(n, hgl_ai_bus::parse_credential(&t).unwrap().unwrap());
    }
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let conf = dir.path().join("nats.conf");
    std::fs::create_dir_all(dir.path().join("store")).unwrap();
    std::fs::write(
        &conf,
        render_conf(
            "127.0.0.1",
            port,
            &dir.path().join("store"),
            &Auth::Users(&reg),
        ),
    )
    .unwrap();
    let child = Command::new(bin)
        .arg("-c")
        .arg(&conf)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server = Server(child);
    let up = (0..200).any(|_| {
        std::thread::sleep(Duration::from_millis(50));
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    assert!(up, "nats-server did not start");
    Some(Hub {
        _server: server,
        _dir: dir,
        url: format!("nats://127.0.0.1:{port}"),
        creds,
    })
}

/// `f`'s value, polled for up to 10 s.
pub async fn until<T, F: std::future::Future<Output = Option<T>>>(
    what: &str,
    mut f: impl FnMut() -> F,
) -> T {
    for _ in 0..100 {
        if let Some(v) = f().await {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}
