//! Request handlers: the owner's pages and the sign-in endpoints.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};

use axum::response::{Html, IntoResponse, Redirect, Response};

use hgl_ai_owner::{self as owner, Body, Choice, Priority, Thread, ThreadKind, ThreadState};
use serde::Deserialize;
use webauthn_rs::prelude::{PublicKeyCredential, RegisterPublicKeyCredential};

use crate::App;
use crate::auth::{
    AuthError, ENROL_COOKIE, FLOW_COOKIE, LOGIN_COOKIE, PENDING_TTL, Provider, SESSION_COOKIE,
    Session,
};
use crate::html::{Nav, csrf, esc, page, text_block, time};

type AppState = State<Arc<App>>;

/// How long a page waits for the console's own fold of the owner's event.
const FOLD_WAIT: Duration = Duration::from_secs(5);

fn cookies(h: &HeaderMap) -> Option<&str> {
    h.get(header::COOKIE).and_then(|v| v.to_str().ok())
}

/// A POST from one of our own pages: `Origin` is the console's (or, without it, the
/// browser says the request is same-origin).
fn same_origin(app: &App, h: &HeaderMap) -> bool {
    match h.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        Some(o) => o == app.auth.origin(),
        None => h
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s == "same-origin"),
    }
}

fn message_page(status: StatusCode, title: &str, text: &str, nav: Option<&Nav>) -> Response {
    let body = format!(
        "<p>{}</p><p><a href=\"/\">Back to the console</a></p>",
        esc(text)
    );
    (status, Html(page(title, nav, None, &body))).into_response()
}

fn forbidden(text: &str) -> Response {
    message_page(StatusCode::FORBIDDEN, "Not allowed", text, None)
}

/// The session of a page view; else the sign-in page.
// The error is the response itself, returned at once by the handler.
#[allow(clippy::result_large_err)]
fn viewer(app: &App, h: &HeaderMap) -> Result<Session, Response> {
    app.auth
        .session(cookies(h))
        .ok_or_else(|| Redirect::to("/signin").into_response())
}

/// The session of a form POST: same origin and the session's CSRF token.
#[allow(clippy::result_large_err)]
fn poster(app: &App, h: &HeaderMap, token: &str) -> Result<Session, Response> {
    if !same_origin(app, h) {
        return Err(forbidden(
            "This request did not come from the console's own page.",
        ));
    }
    let s = app
        .auth
        .session(cookies(h))
        .ok_or_else(|| forbidden("Sign in first."))?;
    if token.is_empty() || token != s.csrf {
        return Err(forbidden(
            "The form is stale; reload the page and try again.",
        ));
    }
    Ok(s)
}

async fn open_questions(app: &App) -> usize {
    match app.hub() {
        Some(b) => owner::list(b)
            .await
            .map(|v| {
                v.iter()
                    .filter(|t| t.kind == ThreadKind::Question && t.is_open())
                    .count()
            })
            .unwrap_or(0),
        None => 0,
    }
}

async fn framed(
    app: &App,
    s: &Session,
    current: &str,
    title: &str,
    body: &str,
    banner: Option<String>,
) -> Response {
    let user = app.auth.display(&s.user);
    let nav = Nav {
        user: &user,
        csrf: &s.csrf,
        open_questions: open_questions(app).await,
        current,
    };
    let banner = banner
        .or_else(|| {
            app.hub().is_none().then(|| {
                "The hub is not reachable; the console retries every few seconds.".to_string()
            })
        })
        .or_else(|| crate::service::undelivered(app));
    Html(page(title, Some(&nav), banner.as_deref(), body)).into_response()
}

fn prio(p: Option<Priority>) -> String {
    match p {
        Some(p) => format!("<span class=\"prio {p}\">{p}</span>"),
        None => String::new(),
    }
}

fn thread_item(t: &Thread) -> String {
    format!(
        "<li><a href=\"/t/{id}\">{title}</a> {prio} <span class=\"meta\">{state} · {issue} · {seat} · {at}</span></li>",
        id = esc(&t.id),
        title = esc(&t.title),
        prio = prio(t.priority),
        state = esc(t.state.as_str()),
        issue = esc(t.issue.as_deref().unwrap_or("no issue")),
        seat = esc(&t.seat),
        at = time(&t.created_at),
    )
}

// ---- owner pages ----

pub async fn status(State(app): AppState, h: HeaderMap) -> Response {
    let s = match viewer(&app, &h) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let mut body = String::new();
    let mut banner = None;
    if let Some(b) = app.hub() {
        let since = chrono::Utc::now() - chrono::Duration::days(30);
        let (reports, decisions) =
            tokio::join!(owner::statuses(b, since, 50), owner::decisions_tail(b, 25));
        match reports {
            Ok(v) if v.is_empty() => body.push_str("<p>No status report in the last 30 days.</p>"),
            Ok(v) => {
                for (seq, st) in v {
                    let _ = write!(
                        body,
                        "<article id=\"status-{seq}\" class=\"report\"><h2>{} from {}</h2>{}</article>",
                        time(&st.at),
                        esc(st.sender.as_str()),
                        text_block(&st.body)
                    );
                }
            }
            Err(e) => banner = Some(format!("Reading the status reports: {e}")),
        }
        match decisions {
            Ok(lines) if !lines.is_empty() => {
                body.push_str("<details id=\"decisions\"><summary>Recent decisions</summary>");
                body.push_str(&text_block(&lines.join("\n")));
                body.push_str("</details>");
            }
            Err(e) if banner.is_none() => banner = Some(format!("Reading the decision log: {e}")),
            _ => {}
        }
    }
    framed(&app, &s, "/", "Status", &body, banner).await
}

pub async fn questions(State(app): AppState, h: HeaderMap) -> Response {
    let s = match viewer(&app, &h) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let mut body = String::new();
    let mut banner = None;
    if let Some(b) = app.hub() {
        match owner::list(b).await {
            Ok(v) => {
                let qs: Vec<Thread> = owner::by_urgency(
                    v.into_iter()
                        .filter(|t| t.kind == ThreadKind::Question)
                        .collect(),
                );
                let (open, done): (Vec<&Thread>, Vec<&Thread>) =
                    qs.iter().partition(|t| t.is_open());
                body.push_str("<h2>Open</h2>");
                if open.is_empty() {
                    body.push_str("<p>No open question.</p>");
                } else {
                    body.push_str("<ul class=\"threads\">");
                    for t in open {
                        body.push_str(&thread_item(t));
                    }
                    body.push_str("</ul>");
                }
                body.push_str("<h2>Answered or withdrawn</h2><ul class=\"threads\">");
                for t in done.iter().take(50) {
                    body.push_str(&thread_item(t));
                }
                body.push_str("</ul>");
            }
            Err(e) => banner = Some(format!("Reading the threads: {e}")),
        }
    }
    framed(&app, &s, "/questions", "Questions", &body, banner).await
}

#[derive(Deserialize, Default)]
pub struct Search {
    #[serde(default)]
    q: String,
    #[serde(default)]
    issue: String,
    #[serde(default)]
    state: String,
    /// `YYYY-MM-DD`
    #[serde(default)]
    since: String,
    #[serde(default)]
    until: String,
}

fn day_stamp(d: &str) -> Option<String> {
    let d = d.trim();
    (d.len() == 10 && d.as_bytes()[4] == b'-' && d.as_bytes()[7] == b'-')
        .then(|| d.replace('-', ""))
        .filter(|s| s.chars().all(|c| c.is_ascii_digit()))
}

pub async fn conversations(
    State(app): AppState,
    h: HeaderMap,
    Query(f): Query<Search>,
) -> Response {
    let s = match viewer(&app, &h) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let mut body = format!(
        "<section aria-labelledby=\"new\"><h2 id=\"new\">Start a conversation</h2>\
         <form method=\"post\" action=\"/conversations\" class=\"stack\">{}\
         <label for=\"c-title\">Short description</label><input id=\"c-title\" name=\"title\" required maxlength=\"200\">\
         <label for=\"c-issue\">Issue (repo#number, optional)</label><input id=\"c-issue\" name=\"issue\" maxlength=\"100\">\
         <label for=\"c-body\">Message</label><textarea id=\"c-body\" name=\"body\" required rows=\"5\"></textarea>\
         <button type=\"submit\">Start</button></form></section>",
        csrf(&s.csrf)
    );
    let states = ["", "open", "closed", "answered", "withdrawn"];
    let mut opts = String::new();
    for st in states {
        let sel = if st == f.state { " selected" } else { "" };
        let label = if st.is_empty() { "any" } else { st };
        let _ = write!(opts, "<option value=\"{st}\"{sel}>{label}</option>");
    }
    let _ = write!(
        body,
        "<section aria-labelledby=\"find\"><h2 id=\"find\">Find threads</h2>\
         <form method=\"get\" action=\"/conversations\" class=\"grid\">\
         <label for=\"s-q\">Description</label><input id=\"s-q\" name=\"q\" value=\"{}\">\
         <label for=\"s-issue\">Issue</label><input id=\"s-issue\" name=\"issue\" value=\"{}\">\
         <label for=\"s-state\">State</label><select id=\"s-state\" name=\"state\">{opts}</select>\
         <label for=\"s-since\">From</label><input id=\"s-since\" name=\"since\" type=\"date\" value=\"{}\">\
         <label for=\"s-until\">To</label><input id=\"s-until\" name=\"until\" type=\"date\" value=\"{}\">\
         <button type=\"submit\">Search</button></form>",
        esc(&f.q),
        esc(&f.issue),
        esc(&f.since),
        esc(&f.until)
    );
    let mut banner = None;
    if let Some(b) = app.hub() {
        match owner::list(b).await {
            Ok(v) => {
                let since = day_stamp(&f.since);
                let until = day_stamp(&f.until);
                let hits: Vec<&Thread> = v
                    .iter()
                    .filter(|t| {
                        f.q.trim().is_empty()
                            || t.title.to_lowercase().contains(&f.q.trim().to_lowercase())
                    })
                    .filter(|t| {
                        f.issue.trim().is_empty()
                            || t.issue.as_deref().is_some_and(|i| {
                                i.to_lowercase().contains(&f.issue.trim().to_lowercase())
                            })
                    })
                    .filter(|t| f.state.is_empty() || t.state.as_str() == f.state)
                    .filter(|t| {
                        since
                            .as_deref()
                            .is_none_or(|d| t.created_at.get(..8).is_some_and(|c| c >= d))
                    })
                    .filter(|t| {
                        until
                            .as_deref()
                            .is_none_or(|d| t.created_at.get(..8).is_some_and(|c| c <= d))
                    })
                    .collect();
                let _ = write!(
                    body,
                    "<p class=\"meta\">{} found</p><ul class=\"threads\">",
                    hits.len()
                );
                for t in hits.iter().take(200) {
                    body.push_str(&thread_item(t));
                }
                body.push_str("</ul>");
            }
            Err(e) => banner = Some(format!("Reading the threads: {e}")),
        }
    }
    body.push_str("</section>");
    framed(&app, &s, "/conversations", "Conversations", &body, banner).await
}

#[derive(Deserialize)]
pub struct StartForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    issue: String,
    #[serde(default)]
    body: String,
}

fn hub_down() -> Response {
    message_page(
        StatusCode::SERVICE_UNAVAILABLE,
        "Hub unreachable",
        "The hub is not reachable; nothing was sent. Try again shortly.",
        None,
    )
}

fn failed(e: impl std::fmt::Display) -> Response {
    message_page(StatusCode::BAD_REQUEST, "Not sent", &e.to_string(), None)
}

pub async fn start(State(app): AppState, h: HeaderMap, Form(f): Form<StartForm>) -> Response {
    let s = match poster(&app, &h, &f.csrf) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(bus) = app.hub() else {
        return hub_down();
    };
    let issue = Some(f.issue.trim().to_string()).filter(|i| !i.is_empty());
    let handle = app.auth.handle_of(&s.user);
    match owner::start_conversation(
        bus,
        &app.controller,
        &s.user,
        handle.as_deref(),
        &f.title,
        issue,
        &f.body,
    )
    .await
    {
        Ok((id, seq)) => {
            let _ = owner::wait_folded(bus, &id, seq, FOLD_WAIT).await;
            Redirect::to(&format!("/t/{id}")).into_response()
        }
        Err(e) => failed(e),
    }
}

pub async fn thread(State(app): AppState, h: HeaderMap, Path(id): Path<String>) -> Response {
    let s = match viewer(&app, &h) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(bus) = app.hub() else {
        return framed(&app, &s, "", "Thread", "", None).await;
    };
    // A long thread's record leaves out its oldest messages: the page shows them all.
    let t = match owner::show_full(bus, &id).await {
        Ok(Some(t)) => t,
        Ok(None) | Err(owner::Error::Invalid(_)) => {
            return message_page(
                StatusCode::NOT_FOUND,
                "No such thread",
                "No thread has that id.",
                None,
            );
        }
        Err(e) => return framed(&app, &s, "", "Thread", "", Some(e.to_string())).await,
    };
    let mut body = format!(
        "<p class=\"meta\">{} {} · {} · {} · seat {} · opened {}</p>",
        esc(t.kind.as_str()),
        prio(t.priority),
        esc(t.state.as_str()),
        esc(t.issue.as_deref().unwrap_or("no issue")),
        esc(&t.seat),
        time(&t.created_at)
    );
    if let Some(slack) = &app.cfg.slack
        && let Ok(Some((link, _))) = bus.slack_get::<String>(&slack.link_key(&id)).await
        && hgl_ai_config::slack::valid_permalink(&link)
    {
        let _ = write!(body, "<p><a href=\"{}\">Open in Slack</a></p>", esc(&link));
    }
    if !t.context.is_empty() {
        let _ = write!(body, "<h2>Context</h2>{}", text_block(&t.context));
    }
    if !t.question.is_empty() {
        let _ = write!(body, "<h2>Question</h2>{}", text_block(&t.question));
    }
    if !t.options.is_empty() {
        body.push_str("<h2>Options</h2><dl class=\"options\">");
        for o in &t.options {
            let sug = if t.suggested.as_deref() == Some(o.label.as_str()) {
                " <strong>(suggested)</strong>"
            } else {
                ""
            };
            let _ = write!(
                body,
                "<dt>{}</dt><dd>{}{sug}</dd>",
                esc(&o.label),
                esc(&o.line)
            );
        }
        body.push_str("</dl>");
    }
    if let Some(a) = &t.answer {
        let _ = write!(body, "<h2>Answer</h2><p>{}</p>", esc(&t.answer_text(a)));
    }
    if !t.messages.is_empty() {
        body.push_str("<h2>Messages</h2><ol class=\"messages\">");
        for m in &t.messages {
            // `This Dude, 27 Nov 14:05` (OWN-10); an agent's messages by seat.
            let who = if m.sender == hgl_ai_proto::OWNER {
                format!("{} (owner)", m.signature())
            } else {
                m.signature()
            };
            let _ = write!(
                body,
                "<li><p class=\"meta\">{} · {}</p>{}</li>",
                esc(&who),
                m.source(),
                text_block(&m.body)
            );
        }
        body.push_str("</ol>");
    }
    let tok = csrf(&s.csrf);
    let id = esc(&t.id);
    if t.kind == ThreadKind::Question && t.is_open() {
        let mut radios = String::new();
        let mut first = true;
        for (v, label, show) in [
            (
                "accept",
                "Accept the suggestion".to_string(),
                t.suggested.is_some(),
            ),
            (
                "option",
                "Pick an option".to_string(),
                !t.options.is_empty(),
            ),
            ("reject", "Reject".to_string(), true),
            ("other", "Answer in my own words".to_string(), true),
        ] {
            if !show {
                continue;
            }
            let checked = if first { " checked" } else { "" };
            first = false;
            let _ = write!(
                radios,
                "<label class=\"choice\"><input type=\"radio\" name=\"choice\" value=\"{v}\"{checked}> {}</label>",
                esc(&label)
            );
        }
        let mut select = String::new();
        for o in &t.options {
            let _ = write!(
                select,
                "<option value=\"{0}\">{0}: {1}</option>",
                esc(&o.label),
                esc(&o.line)
            );
        }
        let _ = write!(
            body,
            "<section aria-labelledby=\"ans\"><h2 id=\"ans\">Answer</h2>\
             <form method=\"post\" action=\"/t/{id}/answer\" class=\"stack answer\">{tok}\
             <fieldset><legend>Your answer</legend>{radios}</fieldset>\
             <div class=\"when-option\"><label for=\"a-option\">Option</label><select id=\"a-option\" name=\"option\">{select}</select></div>\
             <div class=\"when-other\"><label for=\"a-text\">Your answer</label><textarea id=\"a-text\" name=\"text\" rows=\"4\"></textarea></div>\
             <label for=\"a-note\">Note (optional)</label><textarea id=\"a-note\" name=\"note\" rows=\"2\"></textarea>\
             <button type=\"submit\">Send the answer</button></form></section>"
        );
    }
    if t.state == ThreadState::Open || t.kind == ThreadKind::Question {
        let _ = write!(
            body,
            "<section aria-labelledby=\"rep\"><h2 id=\"rep\">Reply</h2>\
             <form method=\"post\" action=\"/t/{id}/message\" class=\"stack\">{tok}\
             <label for=\"m-body\">Message to {}</label><textarea id=\"m-body\" name=\"body\" required rows=\"4\"></textarea>\
             <button type=\"submit\">Send</button></form></section>",
            esc(&t.seat)
        );
    }
    if t.kind == ThreadKind::Conversation && t.is_open() {
        let _ = write!(
            body,
            "<form method=\"post\" action=\"/t/{id}/close\">{tok}<button type=\"submit\" class=\"secondary\">Close the conversation</button></form>"
        );
    }
    let title = t.title.clone();
    framed(&app, &s, "", &title, &body, None).await
}

#[derive(Deserialize)]
pub struct AnswerForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    choice: String,
    #[serde(default)]
    option: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    note: String,
}

async fn send_owner(app: &App, s: &Session, id: &str, body: Body) -> Response {
    let Some(bus) = app.hub() else {
        return hub_down();
    };
    let before = owner::show(bus, id).await.ok().flatten();
    if before.is_none() {
        return message_page(
            StatusCode::NOT_FOUND,
            "No such thread",
            "No thread has that id.",
            None,
        );
    }
    let handle = app.auth.handle_of(&s.user);
    match owner::owner_event(bus, &app.controller, id, &s.user, handle.as_deref(), body).await {
        Ok(seq) => {
            let after = owner::wait_folded(bus, id, seq, FOLD_WAIT)
                .await
                .ok()
                .flatten();
            if after.is_some_and(|t| t.seq >= seq) {
                Redirect::to(&format!("/t/{id}")).into_response()
            } else {
                message_page(
                    StatusCode::ACCEPTED,
                    "Sent",
                    "Sent; the thread shows it once the console has filed it (or it was refused: see the console's log).",
                    None,
                )
            }
        }
        Err(e) => failed(e),
    }
}

pub async fn answer(
    State(app): AppState,
    h: HeaderMap,
    Path(id): Path<String>,
    Form(f): Form<AnswerForm>,
) -> Response {
    let s = match poster(&app, &h, &f.csrf) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let choice: Choice = match f.choice.parse() {
        Ok(c) => c,
        Err(e) => return failed(e),
    };
    let body = Body::Answer {
        choice,
        option: (choice == Choice::Option).then(|| f.option.trim().to_string()),
        text: f.text,
        note: f.note,
    };
    send_owner(&app, &s, &id, body).await
}

#[derive(Deserialize)]
pub struct MessageForm {
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    body: String,
}

pub async fn message(
    State(app): AppState,
    h: HeaderMap,
    Path(id): Path<String>,
    Form(f): Form<MessageForm>,
) -> Response {
    let s = match poster(&app, &h, &f.csrf) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if f.body.trim().is_empty() {
        return failed("An empty message.");
    }
    send_owner(&app, &s, &id, Body::OwnerMessage { body: f.body }).await
}

#[derive(Deserialize)]
pub struct CsrfForm {
    #[serde(default)]
    csrf: String,
}

pub async fn close(
    State(app): AppState,
    h: HeaderMap,
    Path(id): Path<String>,
    Form(f): Form<CsrfForm>,
) -> Response {
    let s = match poster(&app, &h, &f.csrf) {
        Ok(s) => s,
        Err(r) => return r,
    };
    send_owner(&app, &s, &id, Body::Close).await
}

// ---- sign-in ----

pub async fn signin(State(app): AppState, h: HeaderMap) -> Response {
    if app.auth.session(cookies(&h)).is_some() {
        return Redirect::to("/").into_response();
    }
    let mut body = String::from(
        "<p>Sign in with the passkey you registered on this device.</p>\
         <p><button type=\"button\" id=\"passkey-signin\">Sign in with a passkey</button></p>\
         <p id=\"auth-status\" role=\"status\" aria-live=\"polite\"></p>\
         <noscript><p>Passkeys need JavaScript.</p></noscript>\
         <h2>First sign-in, or a new device</h2><p>Prove who you are once; the console then registers a passkey for this device.</p><ul class=\"providers\">",
    );
    if app.auth.has_github() {
        body.push_str(
            "<li><a class=\"button\" href=\"/auth/github\">Continue with GitHub</a></li>",
        );
    }
    if app.auth.has_apple() {
        body.push_str("<li><a class=\"button\" href=\"/auth/apple\">Sign in with Apple</a></li>");
    }
    if app.auth.has_google() {
        body.push_str("<li><a class=\"button\" href=\"/auth/google\">Sign in with Google</a></li>");
    }
    if !app.auth.has_github() && !app.auth.has_apple() && !app.auth.has_google() {
        body.push_str("<li>No provider is configured: set <code>[console.github]</code>, <code>[console.apple]</code> or <code>[console.google]</code>.</li>");
    }
    body.push_str("</ul>");
    Html(page("Sign in", None, None, &body)).into_response()
}

fn oauth_start(app: &App, p: Provider) -> Response {
    match app.auth.oauth_start(p) {
        Ok((url, binding)) => {
            let mut r = Redirect::to(&url).into_response();
            if let Ok(v) = app
                .auth
                .set_cookie(FLOW_COOKIE, &binding, PENDING_TTL)
                .parse()
            {
                r.headers_mut().append(header::SET_COOKIE, v);
            }
            r
        }
        Err(e) => message_page(StatusCode::NOT_FOUND, "Sign-in", &e.to_string(), None),
    }
}

pub async fn github_start(State(app): AppState) -> Response {
    oauth_start(&app, Provider::Github)
}

pub async fn apple_start(State(app): AppState) -> Response {
    oauth_start(&app, Provider::Apple)
}

pub async fn google_start(State(app): AppState) -> Response {
    oauth_start(&app, Provider::Google)
}

pub async fn google_callback(
    State(app): AppState,
    h: HeaderMap,
    Query(q): Query<Callback>,
) -> Response {
    if !q.error.is_empty() {
        return auth_failure(AuthError::Bad(format!("Google: {}", q.error)));
    }
    let binding = app.auth.cookie(cookies(&h), FLOW_COOKIE);
    match app
        .auth
        .google_callback(&q.code, &q.state, binding.as_deref())
        .await
    {
        Ok(id) => enrol_page(&app, id),
        Err(e) => auth_failure(e),
    }
}

fn auth_failure(e: AuthError) -> Response {
    let status = match e {
        AuthError::NotAllowed(_) => StatusCode::FORBIDDEN,
        AuthError::Provider(_) => StatusCode::BAD_GATEWAY,
        AuthError::Setup(_) => StatusCode::INTERNAL_SERVER_ERROR,
        AuthError::Bad(_) => StatusCode::BAD_REQUEST,
    };
    let body = format!(
        "<p>{}</p><p><a href=\"/signin\">Back to sign-in</a></p>",
        esc(&e.to_string())
    );
    (status, Html(page("Sign-in failed", None, None, &body))).into_response()
}

/// The page after a provider vouched: register this device's passkey.
fn enrol_page(app: &App, (user, via): (String, hgl_ai_config::Identity)) -> Response {
    let ticket = app.auth.enrol_ticket(user.clone(), via.clone());
    let body = format!(
        "<p>Signed in as <strong>{}</strong> through {}. Register a passkey for this device; you sign in with it from now on.</p>\
         <p><button type=\"button\" id=\"passkey-register\">Register a passkey</button></p>\
         <p id=\"auth-status\" role=\"status\" aria-live=\"polite\"></p>",
        esc(&app.auth.display(&user)),
        esc(&via.to_string())
    );
    let mut r = Html(page("Register a passkey", None, None, &body)).into_response();
    let h = r.headers_mut();
    for c in [
        app.auth.set_cookie(ENROL_COOKIE, &ticket, PENDING_TTL),
        app.auth.clear_cookie(FLOW_COOKIE),
    ] {
        if let Ok(v) = c.parse() {
            h.append(header::SET_COOKIE, v);
        }
    }
    r
}

#[derive(Deserialize)]
pub struct Callback {
    #[serde(default)]
    code: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    error: String,
}

pub async fn github_callback(
    State(app): AppState,
    h: HeaderMap,
    Query(q): Query<Callback>,
) -> Response {
    if !q.error.is_empty() {
        return auth_failure(AuthError::Bad(format!("GitHub: {}", q.error)));
    }
    let binding = app.auth.cookie(cookies(&h), FLOW_COOKIE);
    match app
        .auth
        .github_callback(&q.code, &q.state, binding.as_deref())
        .await
    {
        Ok(id) => enrol_page(&app, id),
        Err(e) => auth_failure(e),
    }
}

pub async fn apple_callback(
    State(app): AppState,
    h: HeaderMap,
    Form(q): Form<Callback>,
) -> Response {
    if !q.error.is_empty() {
        return auth_failure(AuthError::Bad(format!("Apple: {}", q.error)));
    }
    let binding = app.auth.cookie(cookies(&h), FLOW_COOKIE);
    match app
        .auth
        .apple_callback(&q.code, &q.state, binding.as_deref())
        .await
    {
        Ok(id) => enrol_page(&app, id),
        Err(e) => auth_failure(e),
    }
}

fn json_error(status: StatusCode, m: &str) -> Response {
    (status, Json(serde_json::json!({ "error": m }))).into_response()
}

fn json_auth_error(e: AuthError) -> Response {
    let status = match e {
        AuthError::NotAllowed(_) => StatusCode::FORBIDDEN,
        AuthError::Setup(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    json_error(status, &e.to_string())
}

fn signed_in(app: &App, s: &Session, clear: &str) -> Response {
    let mut r = Json(serde_json::json!({ "redirect": "/" })).into_response();
    let h = r.headers_mut();
    for c in [app.auth.session_cookie(s), app.auth.clear_cookie(clear)] {
        if let Ok(v) = c.parse() {
            h.append(header::SET_COOKIE, v);
        }
    }
    r
}

pub async fn register_start(State(app): AppState, h: HeaderMap) -> Response {
    if !same_origin(&app, &h) {
        return json_error(StatusCode::FORBIDDEN, "not from the console's page");
    }
    let Some(t) = app.auth.cookie(cookies(&h), ENROL_COOKIE) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "no enrolment: sign in with GitHub or Apple first",
        );
    };
    match app.auth.register_start(&t) {
        Ok(ccr) => Json(ccr).into_response(),
        Err(e) => json_auth_error(e),
    }
}

pub async fn register_finish(
    State(app): AppState,
    h: HeaderMap,
    Json(cred): Json<RegisterPublicKeyCredential>,
) -> Response {
    if !same_origin(&app, &h) {
        return json_error(StatusCode::FORBIDDEN, "not from the console's page");
    }
    let Some(t) = app.auth.cookie(cookies(&h), ENROL_COOKIE) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "no enrolment: sign in with GitHub or Apple first",
        );
    };
    match app.auth.register_finish(&t, &cred) {
        Ok(s) => signed_in(&app, &s, ENROL_COOKIE),
        Err(e) => json_auth_error(e),
    }
}

pub async fn login_start(State(app): AppState, h: HeaderMap) -> Response {
    if !same_origin(&app, &h) {
        return json_error(StatusCode::FORBIDDEN, "not from the console's page");
    }
    match app.auth.login_start() {
        Ok((rcr, login)) => {
            let mut r = Json(rcr).into_response();
            if let Ok(v) = app
                .auth
                .set_cookie(LOGIN_COOKIE, &login, PENDING_TTL)
                .parse()
            {
                r.headers_mut().append(header::SET_COOKIE, v);
            }
            r
        }
        Err(e) => json_auth_error(e),
    }
}

pub async fn login_finish(
    State(app): AppState,
    h: HeaderMap,
    Json(cred): Json<PublicKeyCredential>,
) -> Response {
    if !same_origin(&app, &h) {
        return json_error(StatusCode::FORBIDDEN, "not from the console's page");
    }
    let Some(l) = app.auth.cookie(cookies(&h), LOGIN_COOKIE) else {
        return json_error(StatusCode::BAD_REQUEST, "the sign-in expired; try again");
    };
    match app.auth.login_finish(&l, &cred) {
        Ok(s) => signed_in(&app, &s, LOGIN_COOKIE),
        Err(e) => json_auth_error(e),
    }
}

pub async fn signout(State(app): AppState, h: HeaderMap, Form(f): Form<CsrfForm>) -> Response {
    let s = match poster(&app, &h, &f.csrf) {
        Ok(s) => s,
        Err(r) => return r,
    };
    app.auth.sign_out(&s.id);
    let mut r = Redirect::to("/signin").into_response();
    if let Ok(v) = app.auth.clear_cookie(SESSION_COOKIE).parse() {
        r.headers_mut().append(header::SET_COOKIE, v);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(day_stamp("2026-11-27").as_deref(), Some("20261127"));
        assert_eq!(day_stamp("2026-1-27"), None);
        assert_eq!(day_stamp("abcd-ef-gh"), None);
    }
}
