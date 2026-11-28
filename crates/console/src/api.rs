//! The page's JSON API (docs/owner.md §3): who is signed in, and the hub credential the
//! page reads the telemetry with. Everything else the page shows it reads from the hub
//! itself, over WebSocket.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use hgl_ai_owner::{self as owner, Priority, ThreadKind};
use serde::Serialize;
use serde_json::json;

use crate::App;

type AppState = State<Arc<App>>;

fn error(status: StatusCode, text: &str) -> Response {
    (status, Json(json!({ "error": text }))).into_response()
}

#[allow(clippy::result_large_err)]
fn session(app: &App, h: &HeaderMap) -> Result<crate::Session, Response> {
    let cookies = h.get(header::COOKIE).and_then(|v| v.to_str().ok());
    app.auth
        .session(cookies)
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "not signed in"))
}

#[derive(Serialize)]
pub struct SessionOut {
    /// The user as the pages show them: `Handle (email)`.
    pub user: String,
    /// For the page's POSTs (sign-out, and later answers).
    pub csrf: String,
    /// Open questions by priority (`p1`, `p2`, `p3`); absent while the hub is away.
    pub open_questions: Option<std::collections::BTreeMap<String, usize>>,
    /// `[limits] stale_after`: a worktree older than this is stale.
    pub stale_after_secs: u64,
    /// How long a host or builder record lives without a heartbeat. Its expiry sends a
    /// watch nothing, so the page drops a record older than this itself.
    pub hosts_ttl_secs: u64,
}

/// `GET /api/session`
pub async fn session_info(State(app): AppState, h: HeaderMap) -> Response {
    let s = match session(&app, &h) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let open_questions = match app.hub() {
        Some(b) => owner::list(b).await.ok().map(|v| {
            Priority::ALL
                .iter()
                .map(|p| {
                    let n = v
                        .iter()
                        .filter(|t| {
                            t.kind == ThreadKind::Question && t.is_open() && t.priority == Some(*p)
                        })
                        .count();
                    (p.to_string(), n)
                })
                .collect()
        }),
        None => None,
    };
    Json(SessionOut {
        user: app.auth.display(&s.user),
        csrf: s.csrf,
        open_questions,
        stale_after_secs: app.cfg.stale_after().as_secs(),
        hosts_ttl_secs: hgl_ai_bus::HOSTS_TTL.as_secs(),
    })
    .into_response()
}

#[derive(Serialize)]
pub struct ViewerOut {
    /// The hub user (`viewer`), read-only on the telemetry buckets.
    pub user: String,
    /// Its NKey seed: the page keeps it in memory only.
    pub seed: String,
    /// Where the hub's WebSocket listener is, on the console's own origin.
    pub path: &'static str,
}

/// The path the TLS proxy serves the hub's WebSocket listener at (docs/install.md).
pub const NATS_PATH: &str = "/nats";

/// `GET /api/viewer`: the `viewer` credential, for a signed-in page only.
pub async fn viewer(State(app): AppState, h: HeaderMap) -> Response {
    if let Err(r) = session(&app, &h) {
        return r;
    }
    let Some(path) = app.console.viewer_credential.as_deref() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no [console] viewer_credential: rerun `hgl-ai server install` (docs/install.md)",
        );
    };
    let file = hgl_ai_config::expand(path).unwrap_or_else(|_| path.into());
    match hgl_ai_bus::read_credential(&file) {
        Ok(Some(hgl_ai_bus::Credential::Nkey { seed, user, .. })) => Json(ViewerOut {
            user: user.unwrap_or_else(|| "viewer".into()),
            seed,
            path: NATS_PATH,
        })
        .into_response(),
        Ok(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the viewer credential is not a hub user's",
        ),
        Err(e) => {
            tracing::warn!("reading the viewer credential: {e}");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "the viewer credential cannot be read",
            )
        }
    }
}
