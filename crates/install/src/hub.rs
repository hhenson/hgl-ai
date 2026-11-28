//! What the installers need from the hub, behind a trait so tests run without one.

use std::future::Future;
use std::time::Duration;

use hgl_ai_bus::{Bus, BusError};
use hgl_ai_config::Config;
use hgl_ai_proto::build::KV_BUILD;
use hgl_ai_proto::{
    HUB_API_PREFIX, HostRecord, KV_CANARY, KV_CONSOLE, KV_ENDED, KV_HOSTS, KV_LOCKS, KV_METRICS,
    KV_SLACK, KV_STATE, KV_THREADS, KV_TREES, KV_USAGE, STREAM_NOTES, STREAM_OWNER, STREAM_USAGE,
};

use crate::users::{KV_VERDICTS, Role};

pub trait Hub {
    /// Connect with the host configuration (client install).
    fn check(&self, cfg: &Config) -> impl Future<Output = Result<(), BusError>>;
    /// Connect and create the stream and buckets (server install).
    fn provision(&self, cfg: &Config) -> impl Future<Output = Result<(), BusError>>;
    /// Doctor: `Err` when unreachable, else the streams and buckets that are missing, of
    /// those the credential's role may look up by the access table.
    fn health(&self, cfg: &Config) -> impl Future<Output = Result<Vec<String>, String>>;
    /// Doctor: `Some(true)` when the hub lets this host's hub user subscribe to `_INBOX.>`,
    /// every client's replies (a `nats.conf` from before hgl-ai#111), `Some(false)` when it
    /// refuses. `None`: cannot tell (unreachable, no answer in time, the shared token, or a
    /// credential that is or may be the admin's).
    fn shared_inbox(&self, cfg: &Config) -> impl Future<Output = Option<bool>>;
    /// Doctor, on the hub's host: the consumers on the telemetry streams that outlive a
    /// page ([`lasting_consumers`]), as `<stream>/<name>`.
    fn durables(&self, cfg: &Config) -> impl Future<Output = Result<Vec<String>, String>>;
    /// Doctor (`hub boundary`, any host): write `HGL_CANARY`, which no role may, directly
    /// and by the reply route, then read it back: a breach detector, never proof of
    /// isolation. `Ok(None)`: the admin, who may write anything, is not tried.
    fn boundary(&self, cfg: &Config) -> impl Future<Output = Result<Option<Boundary>, String>>;
    /// Doctor, on the hub's host (`hub clients`): the live hosts' records.
    fn hosts(&self, cfg: &Config) -> impl Future<Output = Result<Vec<HostRecord>, String>>;
}

/// A consumer on the telemetry streams that lives on without its page: durable, or with an
/// inactive threshold above this.
pub const LASTING: Duration = Duration::from_secs(3600);

/// The consumers on the streams of the buckets the console's page reads
/// ([`crate::users::VIEWER_BUCKETS`]) that outlive a page ([`LASTING`]), as
/// `<stream>/<name>`. No part of the fleet makes one there, but the `viewer` user can:
/// subjects cannot stop a `durable_name` or an `inactive_threshold` in a consumer create's
/// body (docs/design.md §9).
pub async fn lasting_consumers(js: &async_nats::jetstream::Context) -> Result<Vec<String>, String> {
    use futures::StreamExt;
    let mut out = Vec::new();
    for b in crate::users::VIEWER_BUCKETS {
        let name = format!("KV_{b}");
        let stream = js
            .get_stream(&name)
            .await
            .map_err(|e| format!("{name}: {e}"))?;
        let mut list = stream.consumers();
        while let Some(c) = list.next().await {
            let c = c.map_err(|e| format!("listing the consumers of {name}: {e}"))?;
            if c.config.durable_name.is_some() || c.config.inactive_threshold > LASTING {
                out.push(format!("{name}/{}", c.name));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// A plain connection as `cfg`'s credential (no bus, whose permission warnings would
/// print in `doctor`), with the JetStream API where the hub serves it to this user.
struct Plain {
    client: async_nats::Client,
    js: async_nats::jetstream::Context,
    /// The hub answered at [`HUB_API_PREFIX`]: this user is in a role account (hgl-ai#141).
    accounts: bool,
    who: Who,
}

/// Who a credential is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Who {
    /// The shared token: every right.
    Token,
    /// An NKey user of a known role.
    Role(Role),
    /// An NKey whose role the credential does not say (a bare seed, an unknown name): any
    /// role, the admin too.
    Unknown,
}

/// Connect as `cfg`'s credential and find the JetStream API as the bus does: under
/// [`HUB_API_PREFIX`] when the hub answers there, else `$JS.API`. A user in one account
/// is refused the probe; the refusal, not the timeout, ends the wait.
async fn connect(cfg: &Config) -> Result<Plain, String> {
    use async_nats::jetstream;
    let path = cfg.token_file().map_err(|e| e.to_string())?;
    let cred = hgl_ai_bus::read_credential(&path).map_err(|e| e.to_string())?;
    // A hub user may subscribe to its own inbox prefix only (hgl-ai#111).
    let inbox = cred.as_ref().and_then(hgl_ai_bus::Credential::inbox_prefix);
    let who = match &cred {
        Some(hgl_ai_bus::Credential::Nkey { role, .. }) => role
            .as_deref()
            .and_then(|r| r.parse().ok())
            .map_or(Who::Unknown, Who::Role),
        _ => Who::Token,
    };
    let (tx, mut refused) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut opts = match cred {
        Some(hgl_ai_bus::Credential::Token(t)) => async_nats::ConnectOptions::with_token(t),
        Some(hgl_ai_bus::Credential::Nkey { seed, .. }) => {
            async_nats::ConnectOptions::with_nkey(seed)
        }
        None => async_nats::ConnectOptions::new(),
    }
    .event_callback(move |e| {
        let tx = tx.clone();
        async move {
            let s = e.to_string();
            if s.to_ascii_lowercase().contains("permissions violation") {
                let _ = tx.send(s);
            }
        }
    });
    if let Some(p) = inbox {
        opts = opts.custom_inbox_prefix(p);
    }
    let client = opts
        .connection_timeout(Duration::from_secs(5))
        .connect(cfg.hub.url.as_str())
        .await
        .map_err(|e| e.to_string())?;
    let imported = jetstream::with_prefix(client.clone(), HUB_API_PREFIX);
    let probe = format!("\"{HUB_API_PREFIX}.INFO\"");
    let accounts = tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(5), imported.query_account()) => {
            matches!(r, Ok(Ok(_)))
        }
        () = async {
            while let Some(m) = refused.recv().await {
                if m.contains(&probe) {
                    return;
                }
            }
            std::future::pending::<()>().await
        } => false,
    };
    let js = if accounts {
        imported
    } else {
        jetstream::new(client.clone())
    };
    Ok(Plain {
        client,
        js,
        accounts,
        who,
    })
}

/// The hub's JetStream as `cfg`'s credential.
async fn jetstream_of(cfg: &Config) -> Result<async_nats::jetstream::Context, String> {
    connect(cfg).await.map(|p| p.js)
}

/// Every stream `doctor` looks for, and every bucket.
const STREAMS: [&str; 3] = [STREAM_NOTES, STREAM_OWNER, STREAM_USAGE];
const BUCKETS: [&str; 13] = [
    KV_STATE,
    KV_HOSTS,
    KV_TREES,
    KV_LOCKS,
    KV_ENDED,
    KV_THREADS,
    KV_METRICS,
    KV_CONSOLE,
    KV_USAGE,
    KV_BUILD,
    KV_SLACK,
    KV_VERDICTS,
    KV_CANARY,
];

/// The streams and buckets `who` may look up, of [`STREAMS`] and [`BUCKETS`]: by the access
/// table for a role with an account, the viewer's own buckets, all for the admin and a
/// shared token, and for a credential of unknown role those every role may.
fn health_scope(who: Who) -> (Vec<&'static str>, Vec<&'static str>) {
    let reads = match who {
        Who::Token | Who::Role(Role::Admin) => return (STREAMS.to_vec(), BUCKETS.to_vec()),
        Who::Role(Role::Viewer) => return (Vec::new(), crate::users::VIEWER_BUCKETS.to_vec()),
        Who::Role(r) => crate::users::stream_reads(r),
        Who::Unknown => {
            let mut all = crate::users::ACCOUNT_ROLES.map(crate::users::stream_reads);
            let mut common = std::mem::take(&mut all[0]);
            common.retain(|s| all[1..].iter().all(|r| r.contains(s)));
            common
        }
    };
    (
        STREAMS.into_iter().filter(|s| reads.contains(*s)).collect(),
        BUCKETS
            .into_iter()
            .filter(|b| reads.contains(&format!("KV_{b}")))
            .collect(),
    )
}

/// What `hub boundary`'s live probe found (hgl-ai#146). It detects a breach and proves
/// nothing else: a write it does not see may still have been made, or dropped on its way
/// (r6 on #192). That the boundary holds is `hub accounts`'s audit of `nats.conf`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boundary {
    /// The hub served this user under [`HUB_API_PREFIX`]: a role account.
    pub accounts: bool,
    /// The routes by which a write reached `HGL_CANARY`: `directly`, `by the reply route`.
    pub wrote: Vec<&'static str>,
}

/// What the probe read back: each try's route and whether its key is there. A write seen
/// is the verdict whatever else failed; with none, a read that failed means the probe did
/// not run.
fn breach(
    accounts: bool,
    reads: Vec<(&'static str, Result<bool, String>)>,
) -> Result<Boundary, String> {
    let mut wrote: Vec<&'static str> = Vec::new();
    let mut failed = Vec::new();
    for (route, read) in reads {
        match read {
            Ok(true) if !wrote.contains(&route) => wrote.push(route),
            Ok(_) => {}
            Err(e) => failed.push(e),
        }
    }
    if wrote.is_empty() && !failed.is_empty() {
        return Err(failed.join("; "));
    }
    Ok(Boundary { accounts, wrote })
}

/// The real hub, through `hgl-ai-bus`.
pub struct BusHub;

impl Hub for BusHub {
    async fn check(&self, cfg: &Config) -> Result<(), BusError> {
        Bus::connect(cfg).await.map(drop)
    }

    async fn provision(&self, cfg: &Config) -> Result<(), BusError> {
        Bus::connect(cfg).await?.provision().await
    }

    /// Read-only: the bus API has no existence check that does not create, so this looks
    /// the streams and buckets up directly, those the credential's role may look up only:
    /// a lookup the hub refuses is not a missing bucket (r1 on hgl-ai#152).
    async fn health(&self, cfg: &Config) -> Result<Vec<String>, String> {
        let c = connect(cfg).await?;
        let (streams, buckets) = health_scope(c.who);
        let mut missing = Vec::new();
        for s in streams {
            if c.js.get_stream(s).await.is_err() {
                missing.push(s.to_string());
            }
        }
        for b in buckets {
            if c.js.get_key_value(b).await.is_err() {
                missing.push(b.to_string());
            }
        }
        Ok(missing)
    }

    /// Subscribes to a canary, `_INBOX.hgl-ai-doctor.<n>`: outside this user's own prefix,
    /// and no other client's. Then asks a JetStream API lookup it may make with the canary
    /// as the reply subject. The reply arriving there proves `_INBOX.>` open; the
    /// permissions violation naming the canary proves it closed; neither within the wait
    /// is inconclusive (r6 on #119).
    async fn shared_inbox(&self, cfg: &Config) -> Option<bool> {
        let path = cfg.token_file().ok()?;
        let cred = hgl_ai_bus::read_credential(&path).ok()??;
        let inbox = cred.inbox_prefix()?;
        let hgl_ai_bus::Credential::Nkey { seed, role, .. } = cred else {
            return None;
        };
        // The admin may subscribe to anything; a bare seed's role is unknown.
        let role = role.filter(|r| r != "admin")?;
        // A lookup each role may make; the Slack adapter's is its own bucket's.
        let ask = if role == "slack" {
            format!("$JS.API.STREAM.INFO.KV_{KV_SLACK}")
        } else {
            "$JS.API.INFO".to_string()
        };
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let canary = format!("_INBOX.hgl-ai-doctor.{}{n}", std::process::id());
        let (tx, mut refused) = tokio::sync::mpsc::unbounded_channel::<()>();
        let quoted = format!("\"{}\"", canary.to_ascii_lowercase());
        let client = async_nats::ConnectOptions::with_nkey(seed)
            .custom_inbox_prefix(inbox)
            .connection_timeout(Duration::from_secs(5))
            .event_callback(move |e| {
                let (tx, quoted) = (tx.clone(), quoted.clone());
                async move {
                    let s = e.to_string().to_ascii_lowercase();
                    if s.contains("permissions violation") && s.contains(&quoted) {
                        let _ = tx.send(());
                    }
                }
            })
            .connect(cfg.hub.url.as_str())
            .await
            .ok()?;
        let mut sub = client.subscribe(canary.clone()).await.ok()?;
        client.flush().await.ok()?;
        client
            .publish_with_reply(ask, canary, "".into())
            .await
            .ok()?;
        client.flush().await.ok()?;
        use futures::StreamExt;
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::select! {
                Some(()) = refused.recv() => Some(false),
                Some(_) = sub.next() => Some(true),
                else => None,
            }
        })
        .await
        .ok()
        .flatten()
    }

    async fn durables(&self, cfg: &Config) -> Result<Vec<String>, String> {
        lasting_consumers(&jetstream_of(cfg).await?).await
    }

    /// Three writes at keys of this run's own, `doctor.<random>.<route>`, which no role may
    /// make, then each read back (for up to a second; the first seen ends the wait):
    /// - directly, on `$KV.HGL_CANARY.<k>` and on its name under the hub API prefix;
    /// - by the reply route: a lookup every role may make, `STREAM.MSG.GET` of a key that
    ///   is not there, with the key as its reply subject, which one account lets JetStream
    ///   answer onto (hgl-ai#141). The answer, "no message found", carries no hub metadata.
    async fn boundary(&self, cfg: &Config) -> Result<Option<Boundary>, String> {
        let c = connect(cfg).await?;
        match c.who {
            Who::Role(Role::Admin) => return Ok(None),
            // A bare admin seed would write as the admin may: a false breach. In a role
            // account it is no admin, who lives in `HUB`.
            Who::Unknown if !c.accounts => {
                return Err(
                    "the credential names no role (a bare seed), and in one account \
                            it may be the admin's"
                        .into(),
                );
            }
            _ => {}
        }
        let tag = crate::token::generate().map_err(|e| e.to_string())?[..24].to_string();
        let local = |k: &str, under: bool| {
            let s = format!("$KV.{KV_CANARY}.{k}");
            if under {
                format!("{HUB_API_PREFIX}.{s}")
            } else {
                s
            }
        };
        let api = if c.accounts {
            HUB_API_PREFIX
        } else {
            "$JS.API"
        };
        let mut tries: Vec<(&'static str, String, Result<(), String>)> = Vec::new();
        for under in [false, true] {
            let k = format!("doctor.{tag}.direct{}", if under { "-api" } else { "" });
            let sent = c
                .client
                .publish(local(&k, under), "hgl-ai doctor".into())
                .await
                .map_err(|e| e.to_string());
            tries.push(("directly", k, sent));
        }
        let k = format!("doctor.{tag}.reply");
        let body = format!(r#"{{"last_by_subj":"$KV.{KV_CANARY}.doctor.{tag}.none"}}"#);
        let sent = c
            .client
            .publish_with_reply(
                format!("{api}.STREAM.MSG.GET.KV_{KV_CANARY}"),
                local(&k, false),
                body.into(),
            )
            .await
            .map_err(|e| e.to_string());
        tries.push(("by the reply route", k, sent));
        let _ = c.client.flush().await;

        let canary =
            c.js.get_key_value(KV_CANARY)
                .await
                .map_err(|e| format!("no bucket {KV_CANARY}: {e}"));
        let read = |k: String| {
            let canary = canary.clone();
            async move {
                let store = canary?;
                store
                    .entry(k.as_str())
                    .await
                    .map(|e| e.is_some())
                    .map_err(|e| format!("reading {KV_CANARY}: {e}"))
            }
        };
        let mut reads = Vec::new();
        for round in 0..5 {
            reads.clear();
            for (route, k, sent) in &tries {
                let r = match sent {
                    Err(e) => Err(format!("publishing: {e}")),
                    Ok(()) => read(k.clone()).await,
                };
                reads.push((*route, r));
            }
            if round == 4 || reads.iter().any(|(_, r)| r == &Ok(true)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        breach(c.accounts, reads).map(Some)
    }

    async fn hosts(&self, cfg: &Config) -> Result<Vec<HostRecord>, String> {
        let bus = Bus::connect(cfg).await.map_err(|e| e.to_string())?;
        bus.hosts().await.map_err(|e| e.to_string())
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;

    use super::*;

    /// Records calls; `reachable` decides every answer.
    pub struct FakeHub {
        pub reachable: bool,
        pub missing: Vec<String>,
        pub calls: RefCell<Vec<&'static str>>,
        /// What `durables` finds.
        pub durables: Vec<String>,
        /// The credential file each call connected with.
        pub creds: RefCell<Vec<String>>,
        /// What [`Hub::shared_inbox`] answers when reachable.
        pub shared_inbox: Option<bool>,
        /// What [`Hub::boundary`] answers when reachable.
        pub boundary: Result<Option<Boundary>, String>,
        /// What [`Hub::hosts`] answers when reachable.
        pub hosts: Vec<HostRecord>,
    }

    impl FakeHub {
        pub fn up() -> Self {
            FakeHub {
                reachable: true,
                missing: Vec::new(),
                durables: Vec::new(),
                calls: RefCell::new(Vec::new()),
                creds: RefCell::new(Vec::new()),
                shared_inbox: None,
                boundary: Ok(Some(Boundary {
                    accounts: true,
                    wrote: Vec::new(),
                })),
                hosts: Vec::new(),
            }
        }
        pub fn down() -> Self {
            FakeHub {
                reachable: false,
                ..FakeHub::up()
            }
        }
        fn answer(&self, call: &'static str, cfg: &Config) -> Result<(), BusError> {
            self.calls.borrow_mut().push(call);
            self.creds.borrow_mut().push(cfg.hub.token_file.clone());
            if self.reachable {
                Ok(())
            } else {
                Err(BusError::Unreachable {
                    url: cfg.hub.url.clone(),
                    reason: "connection refused".into(),
                })
            }
        }
    }

    impl Hub for FakeHub {
        async fn check(&self, cfg: &Config) -> Result<(), BusError> {
            self.answer("check", cfg)
        }
        async fn provision(&self, cfg: &Config) -> Result<(), BusError> {
            self.answer("provision", cfg)
        }
        async fn health(&self, cfg: &Config) -> Result<Vec<String>, String> {
            self.answer("health", cfg)
                .map(|()| self.missing.clone())
                .map_err(|e| e.to_string())
        }
        async fn shared_inbox(&self, cfg: &Config) -> Option<bool> {
            self.answer("shared_inbox", cfg).ok()?;
            self.shared_inbox
        }
        async fn durables(&self, cfg: &Config) -> Result<Vec<String>, String> {
            self.answer("durables", cfg)
                .map(|()| self.durables.clone())
                .map_err(|e| e.to_string())
        }
        async fn boundary(&self, cfg: &Config) -> Result<Option<Boundary>, String> {
            self.answer("boundary", cfg).map_err(|e| e.to_string())?;
            self.boundary.clone()
        }
        async fn hosts(&self, cfg: &Config) -> Result<Vec<HostRecord>, String> {
            self.answer("hosts", cfg)
                .map(|()| self.hosts.clone())
                .map_err(|e| e.to_string())
        }
    }
}

/// `hub boundary`'s probe: a write seen wins; with none, a failed read is no result.
#[cfg(test)]
mod breaches {
    use super::*;

    #[test]
    fn a_write_seen_wins_and_a_failed_read_is_no_result() {
        let ok = |w: Vec<&'static str>| {
            Ok(Boundary {
                accounts: true,
                wrote: w,
            })
        };
        let none = vec![
            ("directly", Ok(false)),
            ("directly", Ok(false)),
            ("by the reply route", Ok(false)),
        ];
        assert_eq!(breach(true, none), ok(vec![]));
        let seen = vec![
            ("directly", Ok(true)),
            ("directly", Err("reading HGL_CANARY: timed out".into())),
            ("by the reply route", Err("publishing: closed".into())),
        ];
        assert_eq!(breach(true, seen), ok(vec!["directly"]));
        let failed = vec![
            ("directly", Ok(false)),
            ("by the reply route", Err("no bucket HGL_CANARY".into())),
        ];
        assert!(breach(true, failed).is_err());
    }
}

/// What `hub` looks up per credential (r1 on hgl-ai#152, r6 on #192).
#[cfg(test)]
mod scope {
    use super::*;

    #[test]
    fn a_credential_of_unknown_role_checks_what_every_role_may() {
        assert_eq!(
            health_scope(Who::Unknown),
            (vec![STREAM_OWNER], vec![KV_CANARY])
        );
        let (s, b) = health_scope(Who::Role(Role::Seat));
        assert!(s.contains(&STREAM_NOTES) && b.contains(&KV_VERDICTS));
        assert!(!b.contains(&KV_CONSOLE) && !b.contains(&KV_SLACK));
        for who in [Who::Token, Who::Role(Role::Admin)] {
            assert_eq!(health_scope(who), (STREAMS.to_vec(), BUCKETS.to_vec()));
        }
    }
}

/// [`BusHub::shared_inbox`] against a scripted broker: a late denial and a silent hub
/// (r6 on #119).
#[cfg(test)]
mod probe {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    /// A broker that answers `PING`s and, `deny` after the probe's request, refuses the
    /// canary subscription; `None`: says nothing else.
    async fn broker(deny: Option<Duration>) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("nats://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            let (sock, _) = l.accept().await.unwrap();
            let (r, mut w) = sock.into_split();
            w.write_all(
                b"INFO {\"server_id\":\"mock\",\"version\":\"2.10.0\",\"proto\":1,\
                  \"max_payload\":1048576,\"headers\":true,\"auth_required\":true,\
                  \"nonce\":\"bm9uY2U\"}\r\n",
            )
            .await
            .unwrap();
            let mut lines = BufReader::new(r).lines();
            let mut canary = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.starts_with("PING") {
                    let _ = w.write_all(b"PONG\r\n").await;
                } else if let Some(rest) = line.strip_prefix("SUB ") {
                    canary = rest.split(' ').next().unwrap().to_string();
                } else if line.starts_with("PUB ")
                    && let Some(d) = deny
                {
                    tokio::time::sleep(d).await;
                    let e = format!(
                        "-ERR 'Permissions Violation for Subscription to \"{canary}\"'\r\n"
                    );
                    let _ = w.write_all(e.as_bytes()).await;
                }
            }
        });
        url
    }

    async fn probe(url: String, role: &str) -> Option<bool> {
        let d = tempfile::tempdir().unwrap();
        let seed = nkeys::KeyPair::new_user().seed().unwrap();
        let f = d.path().join("hub.token");
        std::fs::write(&f, hgl_ai_bus::Credential::render_nkey("h", role, &seed)).unwrap();
        let mut cfg = Config::new("t");
        cfg.hub.url = url;
        cfg.hub.token_file = f.display().to_string();
        BusHub.shared_inbox(&cfg).await
    }

    #[tokio::test]
    async fn a_late_denial_is_closed() {
        let url = broker(Some(Duration::from_millis(1500))).await;
        assert_eq!(probe(url, "seat").await, Some(false));
    }

    #[tokio::test]
    async fn no_answer_is_inconclusive() {
        assert_eq!(probe(broker(None).await, "seat").await, None);
    }

    #[tokio::test]
    async fn the_admin_is_not_probed() {
        assert_eq!(probe(broker(None).await, "admin").await, None);
    }
}

/// Against a real `nats-server` from `PATH` (skipped without one), started
/// with the rendered `nats.conf` on a free port inside a temporary directory.
#[cfg(test)]
mod live {
    use std::net::TcpListener;
    use std::process::{Child, Command, Stdio};

    use super::*;

    struct Server(Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    async fn health_with_rendered_conf() {
        let Some(bin) =
            crate::sys::which_in(&std::env::var("PATH").unwrap_or_default(), "nats-server")
        else {
            return;
        };
        let d = tempfile::tempdir().unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let token = crate::token::generate().unwrap();
        let conf = d.path().join("nats.conf");
        std::fs::create_dir_all(d.path().join("store")).unwrap();
        std::fs::write(
            &conf,
            crate::nats::render_conf(
                "127.0.0.1",
                port,
                &d.path().join("store"),
                &crate::nats::Auth::Token(&token),
            ),
        )
        .unwrap();
        let _server = Server(
            Command::new(bin)
                .arg("-c")
                .arg(&conf)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let up = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
        });
        assert!(up, "nats-server did not start");

        let tok_file = d.path().join("hub.token");
        std::fs::write(&tok_file, format!("{token}\n")).unwrap();
        let mut cfg = Config::new("t");
        cfg.hub.url = format!("nats://127.0.0.1:{port}");
        cfg.hub.token_file = tok_file.display().to_string();

        // A shared token is every role: every stream and bucket.
        let missing = BusHub.health(&cfg).await.unwrap();
        assert_eq!(missing, [&STREAMS[..], &BUCKETS[..]].concat());

        let client = async_nats::ConnectOptions::with_token(token.clone())
            .connect(cfg.hub.url.as_str())
            .await
            .unwrap();
        let js = async_nats::jetstream::new(client);
        for (name, subject) in [
            (STREAM_NOTES, "hgl.notes.>"),
            (STREAM_OWNER, "hgl.owner.>"),
            (STREAM_USAGE, "hgl.usage.>"),
        ] {
            js.create_stream(async_nats::jetstream::stream::Config {
                name: name.into(),
                subjects: vec![subject.into()],
                ..Default::default()
            })
            .await
            .unwrap();
        }
        for b in BUCKETS {
            js.create_key_value(async_nats::jetstream::kv::Config {
                bucket: b.into(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        assert!(BusHub.health(&cfg).await.unwrap().is_empty());
        // A shared token writes anything, the canary too, by either route.
        assert_eq!(
            BusHub.boundary(&cfg).await.unwrap(),
            Some(Boundary {
                accounts: false,
                wrote: vec!["directly", "by the reply route"],
            })
        );

        std::fs::write(&tok_file, "wrong\n").unwrap();
        assert!(
            BusHub.health(&cfg).await.is_err(),
            "a wrong token is refused"
        );
    }
}
