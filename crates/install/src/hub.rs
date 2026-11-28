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
    /// Doctor: `Some(true)` when the hub lets this host's hub user subscribe to `_INBOX.>`,
    /// every client's replies (a `nats.conf` from before hgl-ai#111). `None`: cannot tell
    /// (unreachable, the shared token, or a credential that is or may be the admin's).
    fn shared_inbox(&self, cfg: &Config) -> impl Future<Output = Option<bool>>;
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

    /// Read-only: the bus API has no existence check that does not create, so
    /// this looks the streams and buckets up directly.
    async fn health(&self, cfg: &Config) -> Result<Vec<String>, String> {
        let path = cfg.token_file().map_err(|e| e.to_string())?;
        let cred = hgl_ai_bus::read_credential(&path).map_err(|e| e.to_string())?;
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
        let js = async_nats::jetstream::new(client);
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

    /// Subscribes to `_INBOX.>` and flushes: an upgraded hub answers with a permissions
    /// violation before the flush's `PONG`; the wait after it covers the event's delivery.
    async fn shared_inbox(&self, cfg: &Config) -> Option<bool> {
        let path = cfg.token_file().ok()?;
        let cred = hgl_ai_bus::read_credential(&path).ok()??;
        let inbox = cred.inbox_prefix()?;
        let hgl_ai_bus::Credential::Nkey { seed, role, .. } = cred else {
            return None;
        };
        // The admin may subscribe to anything; a bare seed's role is unknown.
        if role.as_deref().is_none_or(|r| r == "admin") {
            return None;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let client = async_nats::ConnectOptions::with_nkey(seed)
            .custom_inbox_prefix(inbox)
            .connection_timeout(Duration::from_secs(5))
            .event_callback(move |e| {
                let tx = tx.clone();
                async move {
                    let s = e.to_string().to_ascii_lowercase();
                    if s.contains("permissions violation") && s.contains("\"_inbox.>\"") {
                        let _ = tx.send(());
                    }
                }
            })
            .connect(cfg.hub.url.as_str())
            .await
            .ok()?;
        let _sub = client.subscribe("_INBOX.>").await.ok()?;
        client.flush().await.ok()?;
        let refused = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .is_ok_and(|r| r.is_some());
        Some(!refused)
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
        async fn health(&self, cfg: &Config) -> Result<Vec<String>, String> {
            self.answer("health", cfg)
                .map(|()| self.missing.clone())
                .map_err(|e| e.to_string())
        }
        async fn shared_inbox(&self, _cfg: &Config) -> Option<bool> {
            self.shared_inbox.filter(|_| self.reachable)
        }
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
