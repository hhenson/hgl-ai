//! `hgl-ai console run`: the web server and its background work: connecting to the hub,
//! folding the owner channel into threads (and sending the owner's notes), notifying
//! the owner of new questions (OWN-9), the controller's `tick` (OWN-1, OWN-7), and the
//! telemetry snapshot.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use hgl_ai_bus::Bus;
use hgl_ai_notify::{Notification, Priority as NPriority};
use hgl_ai_owner::{self as owner, Applied, Priority, Thread};
use hgl_ai_proto::{CONSOLE_PREFIX, Kind, Note, OWNER_PREFIX, Seat};

use crate::{App, Error};

/// Between attempts to reach the hub.
const RECONNECT: Duration = Duration::from_secs(10);
/// Between telemetry readings.
pub const TELEMETRY_EVERY: Duration = Duration::from_secs(10);
/// `p2` questions are batched into one notification this often (OWN-9).
pub const P2_BATCH: Duration = Duration::from_secs(3600);
/// How long TLS material is kept before it is read again (renewed certificates).
const TLS_RELOAD: Duration = Duration::from_secs(6 * 3600);

/// The first `every`-aligned instant after `now` (whole multiples since the epoch), so
/// ticks fall at the same times across restarts and a resent tick is a duplicate.
pub fn next_tick(now: DateTime<Utc>, every: Duration) -> DateTime<Utc> {
    let e = every.as_secs().max(60) as i64;
    let t = (now.timestamp() / e + 1) * e;
    Utc.timestamp_opt(t, 0).single().unwrap_or(now)
}

/// The `tick` note for the controller at `at` (OWN-7).
pub fn tick_note(controller: &Seat, at: DateTime<Utc>) -> Note {
    let mut n = Note::new(
        Seat::console(),
        controller.clone(),
        Kind::Tick,
        "[console] Tick: a status report is due (OWN-1). Post it with `hgl-ai status post`; \
         read `hgl-ai decision tail` and `hgl-ai thread list --open` first (OWN-8).",
    );
    n.created_at = hgl_ai_proto::utc_stamp(at);
    n
}

/// What to tell the owner about a new question: `p1` at once, `p2` in the hourly batch,
/// `p3` nothing (it shows in the console).
pub fn on_new_question(app: &App, t: &Thread) -> Option<Notification> {
    match t.priority {
        Some(Priority::P1) => Some(Notification {
            title: format!("p1 question: {}", t.title),
            priority: NPriority::Urgent,
            link: app.thread_link(&t.id),
        }),
        Some(Priority::P2) => {
            let mut p = app.pending_p2.lock().expect("pending");
            p.push((t.id.clone(), t.title.clone()));
            app.auth.persist(crate::auth::Persist::P2(p.clone()));
            None
        }
        _ => None,
    }
}

/// The hourly notification of the `p2` questions that came in, if any.
pub fn p2_batch(app: &App) -> Option<Notification> {
    let pending = std::mem::take(&mut *app.pending_p2.lock().expect("pending"));
    if !pending.is_empty() {
        app.auth.persist(crate::auth::Persist::P2(Vec::new()));
    }
    match pending.len() {
        0 => None,
        1 => Some(Notification {
            title: format!("p2 question: {}", pending[0].1),
            priority: NPriority::High,
            link: app.thread_link(&pending[0].0),
        }),
        n => Some(Notification {
            title: format!(
                "{n} p2 questions: {}",
                pending
                    .iter()
                    .map(|(_, t)| t.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            priority: NPriority::High,
            link: format!("{}/questions", app.auth.origin()),
        }),
    }
}

async fn notify(app: &App, n: Notification) {
    let Some(nt) = &app.notifier else {
        tracing::info!("no notification channel: {}", n.title);
        return;
    };
    match nt.send(&n).await {
        Ok(kind) => tracing::info!(channel = kind, "notified: {}", n.title),
        Err(e) => tracing::warn!("notifying the owner failed: {e}"),
    }
}

/// Fold one batch of the owner feed; returns how many messages it handled.
pub async fn fold_batch(
    app: &App,
    bus: &Bus,
    feed: &mut hgl_ai_bus::Feed,
    wait: Duration,
) -> hgl_ai_owner::Result<usize> {
    let msgs = feed.fetch(32, wait).await?;
    for m in &msgs {
        match owner::apply(bus, &app.controller, m).await {
            Ok(Applied::Folded {
                thread,
                new_question,
            }) => {
                tracing::info!(thread = %thread.id, seq = m.seq, "folded");
                if new_question.is_some()
                    && let Some(n) = on_new_question(app, &thread)
                {
                    notify(app, n).await;
                }
                m.ack().await?;
            }
            Ok(Applied::Status) => m.ack().await?,
            Ok(Applied::Skipped(why)) => {
                tracing::warn!(seq = m.seq, subject = %m.subject, "not folded: {why}");
                m.ack().await?;
            }
            Err(e) => {
                tracing::warn!(seq = m.seq, "folding failed, retrying later: {e}");
                m.nak(Duration::from_secs(15)).await?;
            }
        }
    }
    Ok(msgs.len())
}

async fn fold_loop(app: Arc<App>, bus: Arc<Bus>) {
    loop {
        let filters = vec![format!("{CONSOLE_PREFIX}.>"), format!("{OWNER_PREFIX}.>")];
        let mut feed = match bus.owner_feed(owner::FEED, filters).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("opening the owner feed: {e}");
                tokio::time::sleep(RECONNECT).await;
                continue;
            }
        };
        loop {
            if let Err(e) = fold_batch(&app, &bus, &mut feed, Duration::from_secs(5)).await {
                tracing::warn!("owner feed: {e}");
                tokio::time::sleep(RECONNECT).await;
                break;
            }
        }
    }
}

async fn tick_loop(app: Arc<App>, bus: Arc<Bus>) {
    let every = app.console.status_every();
    loop {
        let at = next_tick(Utc::now(), every);
        let wait = (at - Utc::now()).to_std().unwrap_or(Duration::ZERO);
        tokio::time::sleep(wait).await;
        match bus.send(&tick_note(&app.controller, at)).await {
            Ok(r) => tracing::info!(seq = r.seq, "tick sent to {}", app.controller),
            Err(e) => tracing::warn!("sending the tick: {e}"),
        }
    }
}

async fn batch_loop(app: Arc<App>) {
    let mut every = tokio::time::interval(P2_BATCH);
    every.tick().await;
    loop {
        every.tick().await;
        if let Some(n) = p2_batch(&app) {
            notify(&app, n).await;
        }
    }
}

async fn telemetry_loop(app: Arc<App>, bus: Arc<Bus>) {
    let mut notes = crate::telemetry::NotesIndex::default();
    let stale = app.cfg.stale_after();
    loop {
        let snap = crate::telemetry::read(&bus, &mut notes, stale, Utc::now()).await;
        app.telemetry.send_if_modified(|old| {
            let changed = serde_json::to_string(&*snap_view(old)).ok()
                != serde_json::to_string(&*snap_view(&Arc::new(snap.clone()))).ok();
            if changed {
                *old = Arc::new(snap.clone());
            }
            changed
        });
        tokio::time::sleep(TELEMETRY_EVERY).await;
    }
}

/// A snapshot without its read time, to tell whether anything changed.
fn snap_view(s: &Arc<crate::telemetry::Snapshot>) -> Arc<crate::telemetry::Snapshot> {
    let mut c = (**s).clone();
    c.read_at = String::new();
    Arc::new(c)
}

/// The key of the pending `p2` batch in `HGL_CONSOLE`.
const P2_KEY: &str = "p2.pending";

/// Take the sessions and the pending `p2` batch kept on the hub, then keep them there.
pub async fn restore(app: &Arc<App>, bus: &Arc<Bus>) {
    let stored = match bus.console_list::<crate::auth::Session>("session.").await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("reading the kept sessions: {e}");
            Vec::new()
        }
    };
    match bus.console_get::<Vec<(String, String)>>(P2_KEY).await {
        Ok(Some(v)) => {
            let mut p = app.pending_p2.lock().expect("pending");
            for x in v {
                if !p.contains(&x) {
                    p.push(x);
                }
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("reading the pending p2 batch: {e}"),
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tracing::info!("{} sessions kept on the hub", stored.len());
    app.auth.attach(stored, tx);
    let pending = app.pending_p2.lock().expect("pending").clone();
    if !pending.is_empty() {
        app.auth.persist(crate::auth::Persist::P2(pending));
    }
    let b = bus.clone();
    tokio::spawn(async move {
        use crate::auth::Persist;
        while let Some(p) = rx.recv().await {
            let res = match &p {
                Persist::Put(k, s) => b.console_put(k, s).await,
                Persist::Del(k) => b.console_del(k).await,
                Persist::P2(v) if v.is_empty() => b.console_del(P2_KEY).await,
                Persist::P2(v) => b.console_put(P2_KEY, v).await,
            };
            if let Err(e) = res {
                tracing::warn!("keeping the console's state on the hub: {e}");
            }
        }
    });
}

/// Connect to the hub (retrying), then start the background work.
pub async fn start_background(app: Arc<App>) {
    let bus = loop {
        if let Some(b) = app.hub() {
            break b.clone();
        }
        match Bus::connect(&app.cfg).await {
            Ok(b) => {
                tracing::info!("console connected to {} as {}", b.url(), b.who());
                let b = Arc::new(b);
                app.set_hub(b.clone());
                break b;
            }
            Err(e) => {
                tracing::warn!("hub: {e}; retrying");
                tokio::time::sleep(RECONNECT).await;
            }
        }
    };
    restore(&app, &bus).await;
    tokio::spawn(fold_loop(app.clone(), bus.clone()));
    tokio::spawn(tick_loop(app.clone(), bus.clone()));
    tokio::spawn(batch_loop(app.clone()));
    tokio::spawn(telemetry_loop(app, bus));
}

fn expand(p: &str) -> std::path::PathBuf {
    hgl_ai_config::expand(p).unwrap_or_else(|_| std::path::PathBuf::from(p))
}

async fn tls_config(cert: &str, key: &str) -> Result<axum_server::tls_rustls::RustlsConfig, Error> {
    let base = axum_server::tls_rustls::RustlsConfig::from_pem_file(expand(cert), expand(key))
        .await
        .map_err(|e| Error::Io(format!("TLS certificate {cert} and key {key}: {e}")))?;
    let mut sc = (*base.get_inner()).clone();
    sc.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(axum_server::tls_rustls::RustlsConfig::from_config(
        Arc::new(sc),
    ))
}

/// Serve the console until the process is stopped: HTTPS with the configured certificate,
/// or plain HTTP on loopback behind a TLS proxy.
pub async fn run(cfg: hgl_ai_config::Config) -> Result<(), Error> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let app = Arc::new(App::new(cfg)?);
    let addr = app.console.listen_addr().map_err(Error::Config)?;
    tokio::spawn(start_background(app.clone()));
    let router = crate::router(app.clone());
    match (&app.console.tls_cert, &app.console.tls_key) {
        (Some(c), Some(k)) => {
            let tls = tls_config(c, k).await?;
            let (c2, k2, t2) = (c.clone(), k.clone(), tls.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(TLS_RELOAD).await;
                    match tls_config(&c2, &k2).await {
                        Ok(n) => t2.reload_from_config(n.get_inner()),
                        Err(e) => tracing::warn!("reloading TLS: {e}"),
                    }
                }
            });
            eprintln!("console: https on {addr} for {}", app.auth.origin());
            axum_server::bind_rustls(addr, tls)
                .serve(router.into_make_service())
                .await
                .map_err(|e| Error::Io(format!("serving on {addr}: {e}")))
        }
        _ => {
            let l = tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| Error::Io(format!("listening on {addr}: {e}")))?;
            eprintln!(
                "console: http on {addr} (loopback, behind a TLS proxy) for {}",
                app.auth.origin()
            );
            axum::serve(l, router)
                .await
                .map_err(|e| Error::Io(format!("serving on {addr}: {e}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(dir: &std::path::Path) -> App {
        let mut cfg = hgl_ai_config::Config::new("hub");
        cfg.paths.data_dir = dir.display().to_string();
        cfg.console = Some(hgl_ai_config::Console {
            url: "https://console.example.com".into(),
            ..hgl_ai_config::Console::default()
        });
        App::new(cfg).unwrap()
    }

    fn q(id: &str, p: Priority) -> Thread {
        Thread {
            id: id.into(),
            kind: hgl_ai_owner::ThreadKind::Question,
            title: format!("title {id}"),
            issue: None,
            priority: Some(p),
            state: hgl_ai_owner::ThreadState::Open,
            seat: "claude".into(),
            created_at: "20261127T000000Z".into(),
            updated_at: "20261127T000000Z".into(),
            context: "never sent".into(),
            question: "q".into(),
            options: vec![],
            suggested: None,
            answer: None,
            messages: vec![],
            seq: 1,
        }
    }

    #[test]
    fn email_goes_to_the_users_by_default() {
        let c = hgl_ai_config::Console {
            url: "https://console.example.com".into(),
            users: vec![
                hgl_ai_config::User::person("a@example.com", "A"),
                hgl_ai_config::User::person("b@example.com", "B"),
            ],
            notify: vec![hgl_ai_config::Notify::Email {
                smtp: "127.0.0.1:2525".into(),
                tls: hgl_ai_config::SmtpTls::None,
                from: "console@example.com".into(),
                to: vec![],
                user: None,
                password_file: None,
            }],
            ..hgl_ai_config::Console::default()
        };
        c.validate().unwrap();
        match &crate::channels(&c)[0] {
            hgl_ai_config::Notify::Email { to, .. } => {
                assert_eq!(to, &["a@example.com", "b@example.com"])
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn notification_policy() {
        let d = tempfile::tempdir().unwrap();
        let a = app(d.path());
        let n = on_new_question(&a, &q("q-00000001", Priority::P1)).unwrap();
        assert_eq!(n.title, "p1 question: title q-00000001");
        assert_eq!(n.link, "https://console.example.com/t/q-00000001");
        assert!(matches!(n.priority, NPriority::Urgent));
        assert!(on_new_question(&a, &q("q-00000003", Priority::P3)).is_none());
        assert!(p2_batch(&a).is_none(), "p3 waits for nobody");
        assert!(on_new_question(&a, &q("q-00000002", Priority::P2)).is_none());
        let one = p2_batch(&a).unwrap();
        assert_eq!(one.link, "https://console.example.com/t/q-00000002");
        assert!(p2_batch(&a).is_none(), "the batch empties");
        on_new_question(&a, &q("q-00000004", Priority::P2));
        on_new_question(&a, &q("q-00000005", Priority::P2));
        let two = p2_batch(&a).unwrap();
        assert_eq!(
            two.title,
            "2 p2 questions: title q-00000004; title q-00000005"
        );
        assert_eq!(two.link, "https://console.example.com/questions");
        assert!(!two.title.contains("never sent"));
    }

    #[test]
    fn ticks_align() {
        let now = Utc.with_ymd_and_hms(2026, 11, 27, 3, 15, 0).unwrap();
        let t = next_tick(now, Duration::from_secs(7200));
        assert_eq!(t, Utc.with_ymd_and_hms(2026, 11, 27, 4, 0, 0).unwrap());
        let n = tick_note(&Seat::new("claude").unwrap(), t);
        assert_eq!(
            (n.kind, n.sender.as_str(), n.recipient.as_str()),
            (Kind::Tick, "console", "claude")
        );
        assert_eq!(n.created_at, "20261127T040000Z");
        assert_eq!(
            n.msg_id(),
            tick_note(&Seat::new("claude").unwrap(), t).msg_id()
        );
        n.validate().unwrap();
    }
}
