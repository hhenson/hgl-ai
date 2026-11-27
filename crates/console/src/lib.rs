//! The owner's web console (docs/owner.md): status reports, questions, conversations and
//! live telemetry, behind passkey sign-in (OWN-6). `hgl-ai console run` serves it; its
//! user service gives it the console's own hub user, the only one that may publish the
//! owner's words (OWN-5).
//!
//! Pages are rendered on the server; the one script, embedded like the stylesheet,
//! drives the passkey ceremonies, live telemetry (server-sent events) and the answer form.

pub mod auth;
mod html;
pub mod jwt;
mod pages;
pub mod service;
pub mod telemetry;

use std::sync::{Arc, Mutex};

use axum::Router;
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
    pub telemetry: tokio::sync::watch::Sender<Arc<telemetry::Snapshot>>,
    pub(crate) notifier: Option<hgl_ai_notify::Notifier>,
    /// `p2` questions waiting for the hourly notification: id and title.
    pub(crate) pending_p2: Mutex<Vec<(String, String)>>,
    /// The state kept on the hub has been taken back.
    pub(crate) restored: std::sync::atomic::AtomicBool,
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
            telemetry: tokio::sync::watch::channel(Arc::new(telemetry::Snapshot::default())).0,
            notifier,
            pending_p2: Mutex::default(),
            restored: std::sync::atomic::AtomicBool::new(false),
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

const STYLE: &str = include_str!("../assets/style.css");
const SCRIPT: &str = include_str!("../assets/app.js");

fn asset(kind: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        body,
    )
        .into_response()
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
        .route("/telemetry", get(pages::telemetry))
        .route("/telemetry.json", get(pages::telemetry_json))
        .route("/telemetry/events", get(pages::telemetry_events))
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
            get(|| async { asset("text/css; charset=utf-8", STYLE) }),
        )
        .route(
            "/assets/app.js",
            get(|| async { asset("text/javascript; charset=utf-8", SCRIPT) }),
        )
        .fallback(|| async { (StatusCode::NOT_FOUND, "not found\n") })
        .layer(axum::middleware::map_response(security_headers))
        .with_state(app)
}
