//! What the installers need from the hub, behind a trait so tests run without one.

use std::future::Future;
use std::time::Duration;

use hgl_ai_bus::{Bus, BusError};
use hgl_ai_config::Config;
use hgl_ai_proto::build::KV_BUILD;
use hgl_ai_proto::{
    KV_CONSOLE, KV_ENDED, KV_HOSTS, KV_LOCKS, KV_METRICS, KV_SLACK, KV_STATE, KV_THREADS, KV_TREES,
    KV_USAGE, STREAM_NOTES, STREAM_OWNER, STREAM_USAGE,
};

pub trait Hub {
    /// Connect with the host configuration (client install).
    fn check(&self, cfg: &Config) -> impl Future<Output = Result<(), BusError>>;
    /// Connect and create the stream and buckets (server install).
    fn provision(&self, cfg: &Config) -> impl Future<Output = Result<(), BusError>>;
    /// Doctor: `Err` when unreachable, else the stream and buckets that are missing.
    fn health(&self, cfg: &Config) -> impl Future<Output = Result<Vec<String>, String>>;
    /// Slack startup status: `None` if unrecorded, `ok` on success, otherwise the error.
    fn slack_startup(&self, cfg: &Config) -> impl Future<Output = Result<Option<String>, String>>;
    /// Doctor: `Some(true)` when the hub lets this host's hub user subscribe to `_INBOX.>`,
    /// every client's replies (a `nats.conf` from before hgl-ai#111), `Some(false)` when it
    /// refuses. `None`: cannot tell (unreachable, no answer in time, the shared token, or a
    /// credential that is or may be the admin's).
    fn shared_inbox(&self, cfg: &Config) -> impl Future<Output = Option<bool>>;
    /// Doctor, on the hub's host: the consumers on the telemetry streams that outlive a
    /// page ([`lasting_consumers`]), as `<stream>/<name>`.
    fn durables(&self, cfg: &Config) -> impl Future<Output = Result<Vec<String>, String>>;
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

/// The hub's JetStream as `cfg`'s credential, plainly (no bus, which would create).
async fn jetstream_of(cfg: &Config) -> Result<async_nats::jetstream::Context, String> {
    let path = cfg.token_file().map_err(|e| e.to_string())?;
    let cred = hgl_ai_bus::read_credential(&path).map_err(|e| e.to_string())?;
    // A hub user may subscribe to its own inbox prefix only (hgl-ai#111).
    let inbox = cred.as_ref().and_then(hgl_ai_bus::Credential::inbox_prefix);
    let mut opts = match cred {
        Some(hgl_ai_bus::Credential::Token(t)) => async_nats::ConnectOptions::with_token(t),
        Some(hgl_ai_bus::Credential::Nkey { seed, .. }) => {
            async_nats::ConnectOptions::with_nkey(seed)
        }
        None => async_nats::ConnectOptions::new(),
    };
    if let Some(p) = inbox {
        opts = opts.custom_inbox_prefix(p);
    }
    let client = opts
        .connection_timeout(Duration::from_secs(5))
        .connect(cfg.hub.url.as_str())
        .await
        .map_err(|e| e.to_string())?;
    Ok(async_nats::jetstream::new(client))
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

    async fn slack_startup(&self, cfg: &Config) -> Result<Option<String>, String> {
        let bus = Bus::connect(cfg).await.map_err(|e| e.to_string())?;
        bus.state_get(hgl_ai_proto::SLACK_STARTUP_KEY)
            .await
            .map(|record| record.map(|entry| entry.value))
            .map_err(|e| e.to_string())
    }

    /// Read-only: the bus API has no existence check that does not create, so
    /// this looks the streams and buckets up directly.
    async fn health(&self, cfg: &Config) -> Result<Vec<String>, String> {
        let js = jetstream_of(cfg).await?;
        let mut missing = Vec::new();
        for s in [STREAM_NOTES, STREAM_OWNER, STREAM_USAGE] {
            if js.get_stream(s).await.is_err() {
                missing.push(s.to_string());
            }
        }
        for b in [
            KV_STATE, KV_HOSTS, KV_TREES, KV_LOCKS, KV_ENDED, KV_THREADS, KV_METRICS, KV_CONSOLE,
            KV_USAGE, KV_BUILD, KV_SLACK,
        ] {
            if js.get_key_value(b).await.is_err() {
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
}

#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;

    use super::*;

    /// Records calls; `reachable` decides every answer.
    pub struct FakeHub {
        pub reachable: bool,
        pub missing: Vec<String>,
        pub slack_error: Option<String>,
        pub calls: RefCell<Vec<&'static str>>,
        /// What `durables` finds.
        pub durables: Vec<String>,
        /// The credential file each call connected with.
        pub creds: RefCell<Vec<String>>,
        /// What [`Hub::shared_inbox`] answers when reachable.
        pub shared_inbox: Option<bool>,
    }

    impl FakeHub {
        pub fn up() -> Self {
            FakeHub {
                reachable: true,
                missing: Vec::new(),
                slack_error: Some("ok".into()),
                durables: Vec::new(),
                calls: RefCell::new(Vec::new()),
                creds: RefCell::new(Vec::new()),
                shared_inbox: None,
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
        async fn slack_startup(&self, cfg: &Config) -> Result<Option<String>, String> {
            self.answer("slack_startup", cfg)
                .map(|()| self.slack_error.clone())
                .map_err(|e| e.to_string())
        }
        async fn health(&self, cfg: &Config) -> Result<Vec<String>, String> {
            self.answer("health", cfg)
                .map(|()| self.missing.clone())
                .map_err(|e| e.to_string())
        }
        async fn shared_inbox(&self, _cfg: &Config) -> Option<bool> {
            self.shared_inbox.filter(|_| self.reachable)
        }
        async fn durables(&self, cfg: &Config) -> Result<Vec<String>, String> {
            self.answer("durables", cfg)
                .map(|()| self.durables.clone())
                .map_err(|e| e.to_string())
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

        let missing = BusHub.health(&cfg).await.unwrap();
        assert_eq!(
            missing,
            [
                STREAM_NOTES,
                STREAM_OWNER,
                STREAM_USAGE,
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
                KV_SLACK
            ]
        );

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
        for b in [
            KV_STATE, KV_HOSTS, KV_TREES, KV_LOCKS, KV_ENDED, KV_THREADS, KV_METRICS, KV_CONSOLE,
            KV_USAGE, KV_BUILD, KV_SLACK,
        ] {
            js.create_key_value(async_nats::jetstream::kv::Config {
                bucket: b.into(),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        assert!(BusHub.health(&cfg).await.unwrap().is_empty());

        std::fs::write(&tok_file, "wrong\n").unwrap();
        assert!(
            BusHub.health(&cfg).await.is_err(),
            "a wrong token is refused"
        );
    }
}
