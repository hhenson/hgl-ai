//! The owner's web console (docs/owner.md): status reports, questions, conversations and
//! live telemetry, behind passkey sign-in (OWN-6). `hgl-ai console run` serves it; its
//! user service gives it the console's own hub user, the only one that may publish the
//! owner's words (OWN-5).
//!
//! Status, questions and conversations are rendered on the server, with one small
//! embedded script for the passkey ceremonies and the answer form. Telemetry is a page
//! of its own (`web/`, React and TypeScript, built by Vite into `web/dist`, embedded
//! here): it reads the hub's buckets over WebSocket as the `viewer` user, whose
//! credential [`api`] hands to a signed-in page.

pub mod api;
pub mod auth;
mod digest;
mod html;
pub mod jwt;
mod pages;
pub mod service;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hgl_ai_bus::Bus;
use hgl_ai_config::{Config, Console};
use hgl_ai_proto::Seat;

pub use auth::{Auth, AuthError, Session};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Config(String),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("{0}")]
    Io(String),
}

/// The console's state, shared by every request and background task.
pub struct App {
    pub cfg: Config,
    pub console: Console,
    pub auth: Auth,
    pub controller: Seat,
    hub: tokio::sync::OnceCell<Arc<Bus>>,
    pub(crate) notifier: Option<hgl_ai_notify::Notifier>,
    /// `p2` questions waiting for the hourly notification: id and title.
    pub(crate) pending_p2: Mutex<Vec<(String, String)>>,
    /// `p1` notifications no channel took yet, retried with backoff: id and title.
    pub(crate) unsent_p1: Mutex<Vec<(String, String)>>,
    /// The state kept on the hub has been taken back.
    pub(crate) restored: std::sync::atomic::AtomicBool,
    /// This app's fold, watched and steered by its own test only.
    #[cfg(test)]
    pub(crate) fold_probe: service::tests::FoldProbe,
}

impl App {
    /// From the host configuration, which must have `[console]`.
    pub fn new(cfg: Config) -> Result<App, Error> {
        let console = cfg.console.clone().ok_or_else(|| {
            Error::Config("no [console] in the configuration (docs/install.md)".into())
        })?;
        console.validate().map_err(Error::Config)?;
        let dir = cfg
            .console_dir()
            .map_err(|e| Error::Config(e.to_string()))?;
        let auth = Auth::new(&console, &dir)?;
        let controller = cfg.controller().map_err(|e| Error::Config(e.to_string()))?;
        let notifier = if console.notify.is_empty() {
            None
        } else {
            Some(
                hgl_ai_notify::Notifier::new(channels(&console))
                    .map_err(|e| Error::Config(format!("notifications: {e}")))?,
            )
        };
        Ok(App {
            cfg,
            console,
            auth,
            controller,
            hub: tokio::sync::OnceCell::new(),
            notifier,
            pending_p2: Mutex::default(),
            unsent_p1: Mutex::default(),
            restored: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            fold_probe: Default::default(),
        })
    }

    /// The hub, once connected.
    pub fn hub(&self) -> Option<&Arc<Bus>> {
        self.hub.get()
    }

    /// Use this connection to the hub (the service connects by itself).
    pub fn set_hub(&self, bus: Arc<Bus>) {
        let _ = self.hub.set(bus);
    }

    /// `<url>/t/<id>`
    pub fn thread_link(&self, id: &str) -> String {
        format!("{}/t/{id}", self.auth.origin())
    }
}

/// The notification channels, an email channel without `to` addressed to every user's
/// email (OWN-9).
pub fn channels(c: &Console) -> Vec<hgl_ai_config::Notify> {
    let emails = c.emails();
    c.notify
        .iter()
        .cloned()
        .map(|mut n| {
            if let hgl_ai_config::Notify::Email { to, .. } = &mut n
                && to.is_empty()
            {
                to.clone_from(&emails);
            }
            n
        })
        .collect()
}

/// Headers every response carries.
async fn security_headers(mut r: Response) -> Response {
    let h = r.headers_mut();
    let set = |h: &mut HeaderMap, k: header::HeaderName, v: &'static str| {
        h.entry(k).or_insert(HeaderValue::from_static(v));
    };
    set(
        h,
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; \
         connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'; \
         object-src 'none'",
    );
    set(h, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(h, header::REFERRER_POLICY, "same-origin");
    set(h, header::X_FRAME_OPTIONS, "DENY");
    set(h, header::STRICT_TRANSPORT_SECURITY, "max-age=31536000");
    set(h, header::CACHE_CONTROL, "no-store");
    r
}

const STYLE: &[u8] = include_bytes!("../assets/style.css");
const SCRIPT: &[u8] = include_bytes!("../assets/app.js");

/// The telemetry page: `web/dist`, the output of `npm run build` in `web/`, checked in so
/// that building the binary needs no Node (docs/design.md).
static WEB: include_dir::Dir<'static> = include_dir::include_dir!("$CARGO_MANIFEST_DIR/web/dist");

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    }
}

fn asset(kind: &'static str, cache: &'static str, body: &'static [u8]) -> Response {
    (
        [(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, cache)],
        body,
    )
        .into_response()
}

/// `/app/<file>`: the page's files. Vite names them by their content, so they never change.
async fn web_file(Path(file): Path<String>) -> Response {
    match WEB.get_file(&file) {
        Some(f) if file != "index.html" => asset(
            content_type(&file),
            "public, max-age=31536000, immutable",
            f.contents(),
        ),
        _ => (StatusCode::NOT_FOUND, "not found\n").into_response(),
    }
}

/// `/telemetry`: the page itself, for a signed-in browser.
async fn telemetry(State(app): State<Arc<App>>, h: HeaderMap) -> Response {
    let cookies = h.get(header::COOKIE).and_then(|v| v.to_str().ok());
    if app.auth.session(cookies).is_none() {
        return axum::response::Redirect::to("/signin").into_response();
    }
    match WEB.get_file("index.html") {
        Some(f) => asset(content_type("index.html"), "no-store", f.contents()),
        None => (StatusCode::NOT_FOUND, "not built\n").into_response(),
    }
}

/// Every route of the console.
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(pages::status))
        .route("/questions", get(pages::questions))
        .route(
            "/conversations",
            get(pages::conversations).post(pages::start),
        )
        .route("/t/{id}", get(pages::thread))
        .route("/t/{id}/answer", post(pages::answer))
        .route("/t/{id}/message", post(pages::message))
        .route("/t/{id}/close", post(pages::close))
        .route("/telemetry", get(telemetry))
        .route("/app/{*file}", get(web_file))
        .route("/api/session", get(api::session_info))
        .route("/api/viewer", get(api::viewer))
        .route("/signin", get(pages::signin))
        .route("/auth/github", get(pages::github_start))
        .route("/auth/github/callback", get(pages::github_callback))
        .route("/auth/apple", get(pages::apple_start))
        .route("/auth/apple/callback", post(pages::apple_callback))
        .route("/auth/google", get(pages::google_start))
        .route("/auth/google/callback", get(pages::google_callback))
        .route("/auth/passkey/register/start", post(pages::register_start))
        .route(
            "/auth/passkey/register/finish",
            post(pages::register_finish),
        )
        .route("/auth/passkey/login/start", post(pages::login_start))
        .route("/auth/passkey/login/finish", post(pages::login_finish))
        .route("/auth/signout", post(pages::signout))
        .route("/healthz", get(|| async { "ok\n" }))
        .route(
            "/assets/style.css",
            get(|| async { asset(content_type(".css"), "public, max-age=3600", STYLE) }),
        )
        .route(
            "/assets/app.js",
            get(|| async { asset(content_type(".js"), "public, max-age=3600", SCRIPT) }),
        )
        .fallback(|| async { (StatusCode::NOT_FOUND, "not found\n") })
        .layer(axum::middleware::map_response(security_headers))
        .with_state(app)
}
