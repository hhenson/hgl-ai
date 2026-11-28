//! `hgl-ai client install|uninstall` (docs/install.md, Clients).

use std::path::PathBuf;

use hgl_ai_bus::BusError;
use hgl_ai_config::Config;
use hgl_ai_proto::Runtime;

use crate::hub::Hub;
use crate::service::{self, Service, Spec};
use crate::skill::{self, Status};
use crate::sys::System;
use crate::{Result, done, io_at, load_config, other, save_config, token};

#[derive(Debug, Clone)]
pub struct ClientOpts {
    /// `nats://host:port` of the hub.
    pub hub: String,
    /// The hub token as copied to this host; it is copied beside `config.toml`.
    pub token_file: String,
    pub host: String,
    pub work_root: Option<String>,
    /// Run `loginctl enable-linger` (Linux).
    pub linger: bool,
    /// Install the builder service (`--no-builder` clears it).
    pub builder: bool,
    /// A separate OS user runs the builder.
    pub builder_user: Option<String>,
}

/// What `install` could not finish.
#[derive(Debug)]
pub struct ClientOutcome {
    /// Set when the hub did not answer; the configuration and token were kept
    /// and nothing after the check was installed.
    pub hub_error: Option<BusError>,
}

fn check_url(u: &str) -> Result<()> {
    let ok = ["nats://", "tls://", "ws://", "wss://"]
        .iter()
        .any(|s| u.strip_prefix(s).is_some_and(|rest| !rest.is_empty()));
    if ok {
        Ok(())
    } else {
        Err(other(format!(
            "hub URL {u:?}: expected nats://<host>:<port>"
        )))
    }
}

fn absolute(p: PathBuf) -> PathBuf {
    std::path::absolute(&p).unwrap_or(p)
}

/// Write or merge `config.toml`, copy the token, check the hub, install the
/// skill into each agent CLI found, and install and start the bridge service.
pub async fn install(sys: &dyn System, hub: &impl Hub, o: &ClientOpts) -> Result<ClientOutcome> {
    check_url(&o.hub)?;
    let env = sys.env();
    let existing = load_config(sys, env)?;
    let created = existing.is_none();
    let mut cfg = existing.unwrap_or_else(|| Config::new(o.host.clone()));
    cfg.host = o.host.clone();
    if let Some(user) = &o.builder_user {
        cfg.builder.user = Some(user.clone());
    }
    cfg.hub.url = o.hub.clone();
    let token_dst = env.client_token_path();
    cfg.hub.token_file = env.contract(&token_dst);
    if let Some(w) = &o.work_root {
        cfg.paths.work_root = env.contract(&absolute(env.expand(w)));
    }
    cfg.validate()?;

    let token_src = absolute(env.expand(&o.token_file));
    let cred = token::read_credential(sys, &token_src)?
        .ok_or_else(|| other(format!("no credential at {}", token_src.display())))?;
    if let Some(r @ ("console" | "builder")) = cred.role() {
        return Err(other(format!(
            "that is a {r} credential; a client takes a seat credential \
             (`hgl-ai server user add <label> --role seat`){}",
            if r == "builder" {
                ": every seat of the host would write the local build verdicts. A builder's \
                 goes in [builder] credential"
            } else {
                ""
            }
        )));
    }
    token::copy(sys, &token_src, &token_dst)?;
    sys.say(&format!(
        "credential: {} ({}, mode 600)",
        cfg.hub.token_file,
        cred.describe()
    ));
    if cred.is_token() {
        sys.say(
            "auth: a shared token; any client can publish as the owner. Ask the hub for a              hub user (`hgl-ai server user add <label> --role seat`, docs/install.md)",
        );
    }
    save_config(sys, env, &cfg)?;
    let verb = if created { "created" } else { "updated" };
    sys.say(&format!("config: {} ({verb})", env.config_path.display()));
    let work_root = env.expand(&cfg.paths.work_root);
    sys.mkdir(&work_root).map_err(io_at(&work_root))?;
    sys.say(&format!("work_root: {}", cfg.paths.work_root));

    if sys.dry_run() {
        sys.say(&format!("would check the hub at {}", cfg.hub.url));
    } else if let Err(e) = hub.check(&cfg).await {
        sys.say(&format!("hub: {e}"));
        sys.say(
            "kept the configuration and token; fix the link to the hub, then rerun this command",
        );
        return Ok(ClientOutcome { hub_error: Some(e) });
    } else {
        sys.say(&format!("hub: reachable at {}", cfg.hub.url));
    }

    let clis = skill::installed_clis(sys);
    if clis.is_empty() {
        sys.say("agent CLIs: none of claude, codex, agy on PATH; install one and rerun to add the skill");
    }
    for (rt, bin) in &clis {
        let before = skill::install(sys, *rt)?;
        let what = match before {
            Status::Missing => "installed",
            Status::Stale => "updated",
            Status::Current => "current",
        };
        sys.say(&format!(
            "skill: {} ({what}; {rt} at {})",
            env.contract(&skill::skill_path(env, *rt)),
            bin.display()
        ));
    }

    let data_dir = env.expand(&cfg.paths.data_dir);
    service::install(sys, &Spec::hgl_ai(Service::Bridge, sys, &data_dir))?;
    done(sys, "service: bridge started");
    if let Some(user) = &cfg.builder.user {
        sys.say(&format!("builder: install and start the service separately as {user}; remove any old client-user builder service"));
    }
    if o.builder && cfg.builder.user.is_none() {
        service::install(sys, &Spec::hgl_ai(Service::Builder, sys, &data_dir))?;
        done(sys, "service: builder started");
    }
    if let Some(l) = service::linger_line(service::linger(sys, o.linger)?) {
        sys.say(&l);
    }
    if cfg.seats.is_empty() {
        sys.say("next: add this host's seats with `hgl-ai seat add`, then run `hgl-ai doctor`");
    } else {
        sys.say("next: `hgl-ai doctor`");
    }
    Ok(ClientOutcome { hub_error: None })
}

/// Remove the bridge service and the skills; keep `config.toml` and the token
/// unless `purge` (and always on the hub host, whose server uses them).
pub fn uninstall(sys: &dyn System, purge: bool) -> Result<()> {
    let env = sys.env();
    service::uninstall(sys, Service::Bridge)?;
    sys.say("service: bridge removed");
    service::uninstall(sys, Service::Builder)?;
    sys.say("service: builder removed");
    for rt in Runtime::ALL {
        if skill::remove(sys, *rt)? {
            sys.say(&format!(
                "skill: removed {}",
                env.contract(&skill::skill_dir(env, *rt))
            ));
        }
    }
    let token = env.client_token_path();
    if !purge {
        sys.say(&format!(
            "kept {} and {} (--purge removes them)",
            env.config_path.display(),
            env.contract(&token)
        ));
        return Ok(());
    }
    let hub_host = matches!(load_config(sys, env), Ok(Some(c)) if c.server.is_some());
    if hub_host {
        sys.say("this host runs the hub: kept the configuration and token; run `hgl-ai server uninstall` first");
        return Ok(());
    }
    for p in [&env.config_path, &token] {
        sys.remove_file(p).map_err(io_at(p))?;
    }
    sys.say(&format!(
        "purged {} and {}",
        env.config_path.display(),
        env.contract(&token)
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::hub::fake::FakeHub;
    use crate::sys::fake::{Fake, files};
    use crate::{DryRun, Os, Output};

    fn fake(d: &Path) -> Fake {
        let f = Fake::new(d, Os::Linux);
        for t in ["claude", "codex", "agy"] {
            f.tool(t);
        }
        f.on(
            |c| match (c.program.as_str(), c.args.first().map(String::as_str)) {
                ("id", _) => Output::success("1000\n"),
                ("loginctl", Some("show-user")) => Output::success("no\n"),
                _ => Output::success(""),
            },
        );
        std::fs::write(f.env.home.join("hub.token"), "c0ffee\n").unwrap();
        f
    }

    fn opts() -> ClientOpts {
        ClientOpts {
            hub: "nats://hub.example:4222".into(),
            token_file: "~/hub.token".into(),
            host: "build-1".into(),
            work_root: None,
            linger: false,
            builder: true,
            builder_user: None,
        }
    }

    #[tokio::test]
    async fn separate_builder_user_is_recorded_without_installing_local_builder() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path());
        let o = ClientOpts {
            builder_user: Some("builder".into()),
            ..opts()
        };
        install(&f, &FakeHub::up(), &o).await.unwrap();
        assert_eq!(
            load_config(&f, &f.env)
                .unwrap()
                .unwrap()
                .builder
                .user
                .as_deref(),
            Some("builder")
        );
        assert!(!service::unit_path(&f.env, Service::Builder).exists());
        assert!(service::unit_path(&f.env, Service::Bridge).exists());
        let checks = crate::doctor::run(&f, &FakeHub::up()).await;
        assert!(!checks.iter().any(|c| c.name == "gh"));
    }

    #[tokio::test]
    async fn install_end_to_end() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path());
        let hub = FakeHub::up();
        let out = install(&f, &hub, &opts()).await.unwrap();
        assert!(out.hub_error.is_none());
        let home = &f.env.home;

        let cfg = load_config(&f, &f.env).unwrap().unwrap();
        assert_eq!(cfg.host, "build-1");
        assert_eq!(cfg.hub.url, "nats://hub.example:4222");
        assert_eq!(cfg.hub.token_file, "~/.config/hgl-ai/hub.token");
        let tok = home.join(".config/hgl-ai/hub.token");
        assert_eq!(std::fs::read_to_string(&tok).unwrap(), "c0ffee\n");
        assert_eq!(f.mode(&tok).unwrap(), 0o600);
        assert!(home.join("hgl-ai/work").is_dir());
        for p in [".claude/skills", ".codex/skills", ".gemini/config/skills"] {
            assert_eq!(
                std::fs::read_to_string(home.join(p).join("hgl-ai/SKILL.md")).unwrap(),
                skill::SKILL
            );
        }
        let unit = std::fs::read_to_string(home.join(".config/systemd/user/hgl-ai-bridge.service"))
            .unwrap();
        assert!(unit.contains("bridge run"));
        assert!(unit.contains(&format!(
            "Environment=\"HGL_AI_CONFIG={}\"",
            f.env.config_path.display()
        )));
        assert!(unit.contains("Environment=\"PATH="));
        assert_eq!(*hub.calls.borrow(), ["check"]);
        assert!(
            f.runs()
                .contains(&"systemctl --user restart hgl-ai-bridge.service".to_string())
        );
        let builder =
            std::fs::read_to_string(home.join(".config/systemd/user/hgl-ai-builder.service"))
                .unwrap();
        assert!(builder.contains("builder run"));
        let d2 = tempfile::tempdir().unwrap();
        let f2 = fake(d2.path());
        let o = ClientOpts {
            builder: false,
            ..opts()
        };
        install(&f2, &FakeHub::up(), &o).await.unwrap();
        assert!(
            !f2.env
                .home
                .join(".config/systemd/user/hgl-ai-builder.service")
                .exists(),
            "--no-builder"
        );
        let said = f.output();
        assert!(!said.contains("c0ffee"), "the token is never printed");
        assert!(said.contains("lingering: off"));
        assert!(said.contains("seat add"));
    }

    #[tokio::test]
    async fn merges_an_existing_config() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path());
        let mut cfg = Config::new("old");
        cfg.limits.memcap_gb = 128;
        cfg.builder.user = Some("builder".into());
        cfg.server = Some(hgl_ai_config::Server::default());
        crate::save_config(&f, &f.env, &cfg).unwrap();
        let o = ClientOpts {
            work_root: Some("~/w".into()),
            ..opts()
        };
        install(&f, &FakeHub::up(), &o).await.unwrap();
        let c = load_config(&f, &f.env).unwrap().unwrap();
        assert_eq!(
            (
                c.host.as_str(),
                c.limits.memcap_gb,
                c.paths.work_root.as_str()
            ),
            ("build-1", 128, "~/w")
        );
        assert!(c.server.is_some(), "the hub's section survives");
        assert_eq!(c.builder.user.as_deref(), Some("builder"));
        assert!(
            !f.env
                .home
                .join(".config/systemd/user/hgl-ai-builder.service")
                .exists()
        );
    }

    #[tokio::test]
    async fn unreachable_hub_keeps_files_and_stops() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path());
        let out = install(&f, &FakeHub::down(), &opts()).await.unwrap();
        assert!(matches!(out.hub_error, Some(BusError::Unreachable { .. })));
        assert!(f.env.config_path.exists());
        assert!(f.env.home.join(".config/hgl-ai/hub.token").exists());
        assert!(!f.env.home.join(".claude/skills").exists());
        assert!(f.runs().is_empty(), "no service without a hub");
        assert!(f.output().contains("kept the configuration and token"));
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path());
        let before = files(d.path());
        let hub = FakeHub::up();
        install(&DryRun::new(&f), &hub, &opts()).await.unwrap();
        assert_eq!(files(d.path()), before);
        assert!(f.runs().is_empty() && hub.calls.borrow().is_empty());
        let out = f.output();
        assert!(out.contains("would write") && out.contains(".claude/skills/hgl-ai/SKILL.md"));
        assert!(out.contains("would run: systemctl --user enable hgl-ai-bridge.service"));
    }

    #[tokio::test]
    async fn rejects_bad_input() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path());
        let hub = FakeHub::up();
        assert!(
            install(
                &f,
                &hub,
                &ClientOpts {
                    hub: "hub:4222".into(),
                    ..opts()
                }
            )
            .await
            .is_err()
        );
        assert!(
            install(
                &f,
                &hub,
                &ClientOpts {
                    host: "Build 1".into(),
                    ..opts()
                }
            )
            .await
            .is_err()
        );
        assert!(
            install(
                &f,
                &hub,
                &ClientOpts {
                    token_file: "~/none".into(),
                    ..opts()
                }
            )
            .await
            .is_err()
        );
        // The console's and a builder's credentials are no host's (hgl-ai#6).
        for role in ["console", "builder"] {
            let seed = nkeys::KeyPair::new_user().seed().unwrap();
            std::fs::write(
                f.env.home.join(format!("{role}.cred")),
                hgl_ai_bus::Credential::render_nkey("x", role, &seed),
            )
            .unwrap();
            let e = install(
                &f,
                &hub,
                &ClientOpts {
                    token_file: format!("~/{role}.cred"),
                    ..opts()
                },
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(e.contains(&format!("a {role} credential")), "{e}");
        }
        assert!(!f.env.config_path.exists());
    }

    #[tokio::test]
    async fn uninstall_keeps_config_unless_purged() {
        let d = tempfile::tempdir().unwrap();
        let f = fake(d.path());
        install(&f, &FakeHub::up(), &opts()).await.unwrap();
        uninstall(&f, false).unwrap();
        let home = &f.env.home;
        assert!(
            !home
                .join(".config/systemd/user/hgl-ai-bridge.service")
                .exists()
        );
        assert!(!home.join(".claude/skills/hgl-ai").exists());
        assert!(f.env.config_path.exists());
        uninstall(&f, true).unwrap();
        assert!(!f.env.config_path.exists());
        assert!(!home.join(".config/hgl-ai/hub.token").exists());
    }
}
