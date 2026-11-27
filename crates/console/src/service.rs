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

/// How long `run` waits for the hub before serving, to take back kept sign-ins.
const RESTORE_WAIT: Duration = Duration::from_secs(10);
/// A failed fold is tried again after this long.
pub const FOLD_RETRY: Duration = Duration::from_secs(2);
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
    let pending = app.pending_p2.lock().expect("pending").clone();
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

/// Notify the owner of a new question by its priority; a `p1` no channel took is kept,
/// on the hub too, and retried with backoff ([`p1_retry`]).
pub async fn tell_new_question(app: &App, t: &Thread) {
    let Some(n) = on_new_question(app, t) else {
        return;
    };
    if !notify(app, n).await {
        let mut u = app.unsent_p1.lock().expect("unsent");
        if !u.iter().any(|(id, _)| id == &t.id) {
            u.push((t.id.clone(), t.title.clone()));
        }
        app.auth.persist(crate::auth::Persist::P1(u.clone()));
    }
}

/// Send the kept `p1` notifications again; those of questions no longer open are dropped.
/// Returns how many are still undelivered.
pub async fn p1_retry(app: &App, bus: Option<&Bus>) -> usize {
    let kept = app.unsent_p1.lock().expect("unsent").clone();
    let mut left = Vec::new();
    for (id, title) in &kept {
        if let Some(b) = bus
            && let Ok(t) = owner::show(b, id).await
            && !t.as_ref().is_some_and(Thread::is_open)
        {
            continue;
        }
        let n = Notification {
            title: format!("p1 question: {title}"),
            priority: NPriority::Urgent,
            link: app.thread_link(id),
        };
        if !notify(app, n).await {
            left.push((id.clone(), title.clone()));
        }
    }
    let mut u = app.unsent_p1.lock().expect("unsent");
    // Keep any added while this round ran.
    u.retain(|x| !kept.contains(x) || left.contains(x));
    if *u != kept {
        app.auth.persist(crate::auth::Persist::P1(u.clone()));
    }
    u.len()
}

/// The first wait before an undelivered `p1` is tried again; it doubles up to the max.
pub const P1_RETRY_MIN: Duration = Duration::from_secs(30);
pub const P1_RETRY_MAX: Duration = Duration::from_secs(30 * 60);

/// The next wait: doubled after a failed round, back to the minimum once all went.
pub fn p1_backoff(prev: Duration, left: usize) -> Duration {
    if left == 0 {
        P1_RETRY_MIN
    } else {
        (prev * 2).min(P1_RETRY_MAX)
    }
}

async fn p1_loop(app: Arc<App>, bus: Arc<Bus>) {
    let mut wait = P1_RETRY_MIN;
    loop {
        tokio::time::sleep(wait).await;
        if app.unsent_p1.lock().expect("unsent").is_empty() {
            wait = P1_RETRY_MIN;
            continue;
        }
        let left = p1_retry(&app, Some(&bus)).await;
        wait = p1_backoff(wait, left);
    }
}

/// What the console shows while urgent notifications are undelivered.
pub fn undelivered(app: &App) -> Option<String> {
    let u = app.unsent_p1.lock().expect("unsent");
    if u.is_empty() {
        return None;
    }
    let titles: Vec<&str> = u.iter().map(|(_, t)| t.as_str()).collect();
    Some(format!(
        "Not delivered yet (no channel took it; retrying): p1 {}",
        titles.join("; ")
    ))
}

/// The questions of a sent batch leave it (and the hub's copy).
pub fn p2_sent(app: &App, sent: &[String]) {
    let mut p = app.pending_p2.lock().expect("pending");
    p.retain(|(id, _)| !sent.contains(id));
    app.auth.persist(crate::auth::Persist::P2(p.clone()));
}

/// Drop from the batch the questions no longer open (answered or withdrawn meanwhile).
pub async fn p2_prune(app: &App, bus: &Bus) {
    let ids: Vec<String> = app
        .pending_p2
        .lock()
        .expect("pending")
        .iter()
        .map(|(i, _)| i.clone())
        .collect();
    let mut gone = Vec::new();
    for id in ids {
        if let Ok(t) = owner::show(bus, &id).await
            && !t.as_ref().is_some_and(Thread::is_open)
        {
            gone.push(id);
        }
    }
    if !gone.is_empty() {
        p2_sent(app, &gone);
    }
}

/// Send a notification; whether a channel took it (none configured counts as sent:
/// there is nowhere else to wait for).
async fn notify(app: &App, n: Notification) -> bool {
    let Some(nt) = &app.notifier else {
        tracing::info!("no notification channel: {}", n.title);
        return true;
    };
    match nt.send(&n).await {
        Ok(kind) => {
            tracing::info!(channel = kind, "notified: {}", n.title);
            true
        }
        Err(e) => {
            tracing::warn!("notifying the owner failed: {e}");
            false
        }
    }
}

/// Fold one batch of the owner feed; returns how many messages it handled.
pub async fn fold_batch(
    app: &App,
    bus: &Bus,
    feed: &mut hgl_ai_bus::Feed,
    wait: Duration,
) -> hgl_ai_owner::Result<usize> {
    let msgs: Vec<_> = feed.next(wait).await?.into_iter().collect();
    for (i, m) in msgs.iter().enumerate() {
        #[cfg(test)]
        let applied = if app.fold_probe.fails(m.seq) {
            Err(hgl_ai_owner::Error::Invalid("injected failure".into()))
        } else {
            owner::apply(bus, &app.controller, m).await
        };
        #[cfg(not(test))]
        let applied = owner::apply(bus, &app.controller, m).await;
        #[cfg(test)]
        app.fold_probe.saw(m.seq, applied.is_ok());
        match applied {
            Ok(Applied::Folded {
                thread,
                new_question,
            }) => {
                tracing::info!(thread = %thread.id, seq = m.seq, "folded");
                if new_question.is_some() {
                    tell_new_question(app, &thread).await;
                }
                m.ack().await?;
            }
            Ok(Applied::Status) => m.ack().await?,
            Ok(Applied::Skipped(why)) => {
                tracing::warn!(seq = m.seq, subject = %m.subject, "not folded: {why}");
                m.ack().await?;
            }
            Err(e) => {
                // Stop here: nothing after this event folds until it has. The feed has one
                // event in flight, so this is all of the batch; any other goes back as is.
                tracing::warn!(seq = m.seq, "folding failed, retrying: {e}");
                m.nak(FOLD_RETRY).await?;
                for rest in &msgs[i + 1..] {
                    rest.nak(Duration::ZERO).await?;
                }
                return Ok(i);
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

/// One hourly batch: the questions still open, sent; cleared only once a channel took
/// it, else kept for the next hour. Whether anything was sent.
pub async fn p2_round(app: &App, bus: Option<&Bus>) -> bool {
    if let Some(b) = bus {
        p2_prune(app, b).await;
    }
    let ids: Vec<String> = app
        .pending_p2
        .lock()
        .expect("pending")
        .iter()
        .map(|(i, _)| i.clone())
        .collect();
    let Some(n) = p2_batch(app) else {
        return false;
    };
    if notify(app, n).await {
        p2_sent(app, &ids);
        true
    } else {
        false
    }
}

async fn batch_loop(app: Arc<App>, bus: Arc<Bus>) {
    let mut every = tokio::time::interval(P2_BATCH);
    every.tick().await;
    loop {
        every.tick().await;
        p2_round(&app, Some(&bus)).await;
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
/// The key of the undelivered `p1` notifications in `HGL_CONSOLE`.
const P1_KEY: &str = "p1.unsent";

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
    match bus.console_get::<Vec<(String, String)>>(P1_KEY).await {
        Ok(Some(v)) => {
            let mut u = app.unsent_p1.lock().expect("unsent");
            for x in v {
                if !u.contains(&x) {
                    u.push(x);
                }
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("reading the undelivered p1 notifications: {e}"),
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tracing::info!("{} sessions kept on the hub", stored.len());
    app.auth.attach(stored, tx);
    let pending = app.pending_p2.lock().expect("pending").clone();
    if !pending.is_empty() {
        app.auth.persist(crate::auth::Persist::P2(pending));
    }
    let unsent = app.unsent_p1.lock().expect("unsent").clone();
    if !unsent.is_empty() {
        app.auth.persist(crate::auth::Persist::P1(unsent));
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
                Persist::P1(v) if v.is_empty() => b.console_del(P1_KEY).await,
                Persist::P1(v) => b.console_put(P1_KEY, v).await,
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
    if !app.restored.swap(true, std::sync::atomic::Ordering::SeqCst) {
        restore(&app, &bus).await;
    }
    tokio::spawn(fold_loop(app.clone(), bus.clone()));
    tokio::spawn(tick_loop(app.clone(), bus.clone()));
    tokio::spawn(batch_loop(app.clone(), bus.clone()));
    tokio::spawn(p1_loop(app.clone(), bus.clone()));
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
    // Sign-ins kept on the hub come back before the first request, when the hub answers
    // soon; else the background takes them back once it does.
    if let Ok(Ok(b)) = tokio::time::timeout(RESTORE_WAIT, Bus::connect(&app.cfg)).await {
        let b = Arc::new(b);
        app.set_hub(b.clone());
        restore(&app, &b).await;
        app.restored
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
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
pub(crate) mod tests {
    use super::*;

    /// One app's fold, for its own test: tests run at once, each on its own hub, and
    /// their sequences coincide, so nothing here is shared between apps.
    #[derive(Default)]
    pub(crate) struct FoldProbe {
        /// The fold of the event at this sequence fails once (a transient hub error).
        fail_once: std::sync::Mutex<Option<u64>>,
        /// Every event the fold was handed, in order, and whether it folded.
        seen: std::sync::Mutex<Vec<(u64, bool)>>,
    }

    impl FoldProbe {
        pub(crate) fn fail_once(&self, seq: u64) {
            *self.fail_once.lock().unwrap() = Some(seq);
        }
        pub(crate) fn fails(&self, seq: u64) -> bool {
            let mut f = self.fail_once.lock().unwrap();
            if *f == Some(seq) {
                *f = None;
                return true;
            }
            false
        }
        pub(crate) fn saw(&self, seq: u64, folded: bool) {
            self.seen.lock().unwrap().push((seq, folded));
        }
        pub(crate) fn seen(&self) -> Vec<(u64, bool)> {
            self.seen.lock().unwrap().clone()
        }
    }

    struct Server(std::process::Child, #[allow(dead_code)] tempfile::TempDir);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A token hub from `PATH` on a free port; `None` (skipped) without `nats-server`.
    fn hub() -> Option<(Server, String, String)> {
        let bin = std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join("nats-server"))
            .find(|p| p.is_file())?;
        let d = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let token = format!("t{}", std::process::id());
        let child = std::process::Command::new(bin)
            .args([
                "-js",
                "-a",
                "127.0.0.1",
                "-p",
                &port.to_string(),
                "--auth",
                &token,
                "-sd",
            ])
            .arg(d.path().join("store"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let s = Server(child, d);
        assert!((0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
        }));
        Some((s, format!("nats://127.0.0.1:{port}"), token))
    }

    /// A thread past the bucket's value limit: two 600 KiB replies fold, the record stays
    /// bounded, later events still fold, and the whole thread reads back from the stream.
    #[tokio::test]
    async fn a_long_thread_does_not_block_the_feed() {
        let Some((_srv, url, token)) = hub() else {
            eprintln!("skipped: nats-server is not on PATH");
            return;
        };
        let bus = Bus::connect_url(&url, Some(token)).await.unwrap();
        bus.provision().await.unwrap();
        let d = tempfile::tempdir().unwrap();
        let a = app(d.path());
        let claude = Seat::new("claude").unwrap();
        let id = owner::ask(
            &bus,
            &claude,
            owner::Ask {
                title: "Long".into(),
                priority: Priority::P3,
                issue: None,
                context: String::new(),
                question: "Q?".into(),
                options: vec![],
                suggested: None,
            },
        )
        .await
        .unwrap();
        // Two different replies: the same event twice is one (the stream drops a resend).
        let big = "y".repeat(600 << 10);
        owner::reply(&bus, &claude, &id, &big).await.unwrap();
        owner::reply(&bus, &claude, &id, &"z".repeat(600 << 10))
            .await
            .unwrap();
        let last = owner::reply(&bus, &claude, &id, "the end").await.unwrap();
        let filters = vec![format!("{CONSOLE_PREFIX}.>"), format!("{OWNER_PREFIX}.>")];
        let mut feed = bus.owner_feed(owner::FEED, filters).await.unwrap();
        let start = std::time::Instant::now();
        for _ in 0..20 {
            // The service's own wait: a fetch for more than one event would sit out the
            // whole of it each time.
            fold_batch(&a, &bus, &mut feed, Duration::from_secs(5))
                .await
                .unwrap();
            if owner::show(&bus, &id)
                .await
                .unwrap()
                .is_some_and(|t| t.seq >= last)
            {
                break;
            }
        }
        let t = owner::show(&bus, &id).await.unwrap().unwrap();
        assert_eq!(t.seq, last, "every event folded");
        assert!(t.cut && t.encoded_len() <= owner::model::RECORD_MAX);
        // Four events, one in flight at a time, without waiting out each fetch.
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "{:?}",
            start.elapsed()
        );
        let full = owner::show_full(&bus, &id).await.unwrap().unwrap();
        assert_eq!(full.messages.len(), 3);
        assert_eq!(full.messages[0].body, big, "the stream has every word");
        assert_eq!(full.messages[2].body, "the end");
    }

    /// Carbon's probe: the answer's fold fails once while the thread's next event waits.
    /// The next event is not folded first, the answer is not lost, and its ruling reaches
    /// `decisions.log`.
    #[tokio::test]
    async fn a_failed_fold_holds_the_threads_later_events() {
        let Some((_srv, url, token)) = hub() else {
            eprintln!("skipped: nats-server is not on PATH");
            return;
        };
        let bus = Bus::connect_url(&url, Some(token)).await.unwrap();
        bus.provision().await.unwrap();
        let d = tempfile::tempdir().unwrap();
        let a = app(d.path());
        let claude = Seat::new("claude").unwrap();
        let id = owner::ask(
            &bus,
            &claude,
            owner::Ask {
                title: "Pick".into(),
                priority: Priority::P3,
                issue: Some("alpha#7".into()),
                context: String::new(),
                question: "Which?".into(),
                options: vec![owner::QOption::parse("a:One").unwrap()],
                suggested: Some("a".into()),
            },
        )
        .await
        .unwrap();
        let answer = owner::owner_event(
            &bus,
            &id,
            "owner@example.com",
            Some("This Dude"),
            owner::Body::Answer {
                choice: owner::Choice::Accept,
                option: None,
                text: String::new(),
                note: String::new(),
            },
        )
        .await
        .unwrap();
        let later = owner::owner_event(
            &bus,
            &id,
            "owner@example.com",
            Some("This Dude"),
            owner::Body::OwnerMessage {
                body: "And thanks.".into(),
            },
        )
        .await
        .unwrap();
        a.fold_probe.fail_once(answer);
        let filters = vec![format!("{CONSOLE_PREFIX}.>"), format!("{OWNER_PREFIX}.>")];
        let mut feed = bus.owner_feed(owner::FEED, filters).await.unwrap();
        // Fetches go on while the answer waits out its retry: none hands over the message.
        for _ in 0..40 {
            fold_batch(&a, &bus, &mut feed, Duration::from_millis(500))
                .await
                .unwrap();
            let t = owner::show(&bus, &id).await.unwrap();
            if let Some(t) = &t {
                assert!(
                    t.messages.is_empty() || t.answer.is_some(),
                    "the message folded before the answer: {t:?}"
                );
            }
            if t.is_some_and(|t| t.seq >= later) {
                break;
            }
        }
        // What the fold was handed, in order: the ask, the answer (failed), the answer
        // again, then the message; never the message while the answer waited.
        let seen = a.fold_probe.seen();
        let (before, held) = seen.split_at(seen.len().saturating_sub(3));
        assert!(before.iter().all(|&(s, ok)| s < answer && ok), "{seen:?}");
        assert_eq!(
            held,
            [(answer, false), (answer, true), (later, true)],
            "{seen:?}"
        );
        let t = owner::show(&bus, &id).await.unwrap().unwrap();
        assert_eq!(t.state, owner::ThreadState::Answered, "{t:?}");
        assert_eq!(t.messages.last().unwrap().body, "And thanks.");
        let log = owner::decisions_tail(&bus, 5).await.unwrap();
        assert_eq!(log.iter().filter(|l| l.contains(&id)).count(), 1, "{log:?}");
        // The inbox got the answer and the message, once each.
        let mut inbox = bus.inbox(&claude).await.unwrap();
        let got = inbox.fetch(10, Duration::from_secs(2)).await.unwrap();
        let owner_notes: Vec<_> = got.iter().filter(|d| d.note.sender.is_owner()).collect();
        assert_eq!(
            owner_notes.len(),
            2,
            "{:?}",
            got.iter().map(|d| &d.note.body).collect::<Vec<_>>()
        );
        assert_eq!(owner_notes[0].note.kind, Kind::Answer, "in order");
    }

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
            older: 0,
            cut: false,
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

    /// A `p1` no channel took is kept, shown and retried with backoff until one takes it.
    #[tokio::test]
    async fn an_undelivered_p1_is_kept_and_retried() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let down = Arc::new(AtomicBool::new(true));
        let hits = Arc::new(AtomicUsize::new(0));
        let (d2, h2) = (down.clone(), hits.clone());
        let topic = axum::Router::new().route(
            "/t",
            axum::routing::post(move || {
                let (d, h) = (d2.clone(), h2.clone());
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    if d.load(Ordering::SeqCst) {
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR
                    } else {
                        axum::http::StatusCode::OK
                    }
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, topic).await });
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = hgl_ai_config::Config::new("hub");
        cfg.paths.data_dir = dir.path().display().to_string();
        cfg.console = Some(hgl_ai_config::Console {
            url: "https://console.example.com".into(),
            notify: vec![hgl_ai_config::Notify::Ntfy {
                url: format!("http://{addr}/t"),
                token_file: None,
            }],
            ..hgl_ai_config::Console::default()
        });
        let a = App::new(cfg).unwrap();
        tell_new_question(&a, &q("q-00000001", Priority::P1)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(a.unsent_p1.lock().unwrap().len(), 1, "kept");
        assert!(undelivered(&a).unwrap().contains("title q-00000001"));
        assert_eq!(p1_retry(&a, None).await, 1, "still down");
        assert_eq!(p1_backoff(P1_RETRY_MIN, 1), P1_RETRY_MIN * 2);
        assert_eq!(p1_backoff(P1_RETRY_MAX, 1), P1_RETRY_MAX);
        down.store(false, Ordering::SeqCst);
        assert_eq!(p1_retry(&a, None).await, 0, "delivered");
        assert!(undelivered(&a).is_none());
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        assert_eq!(p1_backoff(P1_RETRY_MAX, 0), P1_RETRY_MIN);
    }

    /// A batch that no channel took waits for the next hour; a sent one is cleared.
    #[tokio::test]
    async fn the_p2_batch_waits_until_a_channel_takes_it() {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = hgl_ai_config::Config::new("hub");
        cfg.paths.data_dir = d.path().display().to_string();
        cfg.console = Some(hgl_ai_config::Console {
            url: "https://console.example.com".into(),
            notify: vec![hgl_ai_config::Notify::Ntfy {
                url: "http://127.0.0.1:1/nobody".into(),
                token_file: None,
            }],
            ..hgl_ai_config::Console::default()
        });
        let down = App::new(cfg).unwrap();
        on_new_question(&down, &q("q-00000002", Priority::P2));
        assert!(!p2_round(&down, None).await, "the channel refused");
        assert!(p2_batch(&down).is_some(), "kept for the next hour");
        let up = app(d.path());
        on_new_question(&up, &q("q-00000003", Priority::P2));
        assert!(p2_round(&up, None).await);
        assert!(p2_batch(&up).is_none());
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
        assert!(p2_batch(&a).is_some(), "not cleared until sent");
        p2_sent(&a, &["q-00000002".into()]);
        assert!(p2_batch(&a).is_none(), "the batch empties once sent");
        on_new_question(&a, &q("q-00000004", Priority::P2));
        on_new_question(&a, &q("q-00000005", Priority::P2));
        let two = p2_batch(&a).unwrap();
        assert_eq!(
            two.title,
            "2 p2 questions: title q-00000004; title q-00000005"
        );
        p2_sent(&a, &["q-00000004".into(), "q-00000005".into()]);
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
