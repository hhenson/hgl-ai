//! `nats-server`: find or install it, and write its configuration.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::env::Os;
use crate::sys::{Cmd, System};
use crate::{Result, io_at, other, run_ok};

/// Release downloads; `<base>/v<version>/<asset>`.
pub const RELEASE_BASE: &str = "https://github.com/nats-io/nats-server/releases/download";
/// Largest message the hub accepts. Notes and state values are small; this
/// leaves room for long review evidence without nearing the 8 MB ceiling.
pub const MAX_PAYLOAD: &str = "4MB";

/// Quote a value for `nats.conf`.
fn conf_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

/// How clients authenticate: one shared token (the single-token mode of older installs),
/// a hub user per client (OWN-5) in one account, or the same users in one account per
/// role (hgl-ai#141; not installed yet).
#[derive(Debug, Clone, Copy)]
pub enum Auth<'a> {
    Token(&'a str),
    Users(&'a crate::users::Registry),
    Accounts(&'a crate::users::Registry),
}

/// The hub's WebSocket listener, for the console's page (docs/owner.md §3): loopback,
/// plain (the TLS proxy in front of the console serves it at `<console url>/nats`), and
/// only for pages of the console's origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Websocket {
    pub port: u16,
    pub origins: Vec<String>,
}

/// The listener `cfg` asks for: a websocket port, a console, and hub users (a shared
/// token would give the page every right).
pub fn websocket_of(cfg: &hgl_ai_config::Config) -> Option<Websocket> {
    let srv = cfg.server.as_ref()?;
    let console = cfg.console.as_ref()?;
    let port = srv.websocket_port?;
    (srv.auth == hgl_ai_config::HubAuth::Users).then(|| Websocket {
        port,
        origins: vec![console.url.trim_end_matches('/').to_string()],
    })
}

/// `<data_dir>/nats.conf`: listen address, JetStream store, authorization.
pub fn render_conf(listen: &str, port: u16, store_dir: &Path, auth: &Auth) -> String {
    render_conf_ws(listen, port, store_dir, auth, None)
}

/// [`render_conf`] with the WebSocket listener, if any.
pub fn render_conf_ws(
    listen: &str,
    port: u16,
    store_dir: &Path,
    auth: &Auth,
    ws: Option<&Websocket>,
) -> String {
    let authorization = match auth {
        Auth::Token(t) => format!("authorization {{\n  token: {}\n}}\n", conf_str(t)),
        Auth::Users(reg) => crate::users::render_authorization(reg),
        Auth::Accounts(reg) => crate::users::render_accounts(reg),
    };
    format!(
        "# hgl-ai hub, written by `hgl-ai server install`; reinstalling rewrites it.\n\
         host: {host}\n\
         port: {port}\n\
         max_payload: {MAX_PAYLOAD}\n\
         pid_file: {pid}\n\
         \n\
         jetstream {{\n  store_dir: {store}\n}}\n\
         \n\
         {websocket}{authorization}",
        websocket = ws.map(render_websocket).unwrap_or_default(),
        host = conf_str(listen),
        pid = conf_str(&pid_path(store_dir).display().to_string()),
        store = conf_str(&store_dir.display().to_string()),
    )
}

fn render_websocket(w: &Websocket) -> String {
    let origins: Vec<String> = w.origins.iter().map(|o| conf_str(o)).collect();
    format!(
        "# The console's page, through the TLS proxy at <console url>/nats.\n\
         websocket {{\n  host: \"127.0.0.1\"\n  port: {}\n  no_tls: true\n  \
         allowed_origins: [{}]\n}}\n\n",
        w.port,
        origins.join(", ")
    )
}

/// `<store_dir>/nats.pid`: where the hub writes its process id, for a reload.
pub fn pid_path(store_dir: &Path) -> std::path::PathBuf {
    store_dir.join("nats.pid")
}

/// The release archive for this OS and architecture.
pub fn release_asset(version: &str, os: Os, arch: &str) -> Result<String> {
    let os_s = match os {
        Os::Linux => "linux",
        Os::MacOs => "darwin",
    };
    let arch_s = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => {
            return Err(other(format!(
                "no nats-server release for architecture {a}"
            )));
        }
    };
    Ok(format!("nats-server-v{version}-{os_s}-{arch_s}.tar.gz"))
}

/// Check `data` against the line for `asset` in a `SHA256SUMS` file.
pub fn verify_sha256(sums: &str, asset: &str, data: &[u8]) -> Result<()> {
    let want = sums
        .lines()
        .filter_map(|l| l.split_once(char::is_whitespace))
        .find(|(_, name)| name.trim().trim_start_matches('*') == asset)
        .map(|(sum, _)| sum.trim().to_ascii_lowercase())
        .ok_or_else(|| other(format!("SHA256SUMS has no entry for {asset}")))?;
    let got = hex::encode(Sha256::digest(data));
    if got == want {
        Ok(())
    } else {
        Err(other(format!(
            "checksum mismatch for {asset}: expected {want}, got {got}"
        )))
    }
}

/// Where `nats-server` came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NatsSource {
    Path,
    Brew,
    Release,
}

/// Use `nats-server` from `PATH`; else `brew install nats-server`; else the
/// pinned release, checksum verified, into `<data_dir>/bin`.
pub fn ensure_server(
    sys: &dyn System,
    data_dir: &Path,
    version: &str,
    base: &str,
) -> Result<(PathBuf, NatsSource)> {
    ensure_server_with(sys, data_dir, version, base, true)
}

/// [`ensure_server`], with Homebrew only when `brew` (not for a server user).
pub fn ensure_server_with(
    sys: &dyn System,
    data_dir: &Path,
    version: &str,
    base: &str,
    brew: bool,
) -> Result<(PathBuf, NatsSource)> {
    if let Some(p) = sys.which("nats-server") {
        return Ok((p, NatsSource::Path));
    }
    if let Some(brew) = sys.which("brew").filter(|_| brew)
        && !pinned_release(sys, data_dir, version)
    {
        let brew = brew.display().to_string();
        run_ok(sys, &Cmd::new(&brew).args(["install", "nats-server"]))?;
        if let Some(p) = sys.which("nats-server") {
            return Ok((p, NatsSource::Brew));
        }
        let prefix = sys
            .query(&Cmd::new(&brew).arg("--prefix"))
            .ok()
            .filter(|o| o.ok())
            .map(|o| PathBuf::from(o.stdout.trim()))
            .ok_or_else(|| other("`brew --prefix` failed after installing nats-server"))?;
        return Ok((prefix.join("bin/nats-server"), NatsSource::Brew));
    }
    Ok((
        ensure_release(sys, data_dir, version, base)?,
        NatsSource::Release,
    ))
}

/// The `nats-server` a system install chose (`chosen`): the pinned release when it is
/// `<data_dir>/bin/nats-server`, else that program as it is.
pub fn ensure_chosen(
    sys: &dyn System,
    data_dir: &Path,
    version: &str,
    base: &str,
    chosen: &Path,
) -> Result<(PathBuf, NatsSource)> {
    if chosen == release_path(data_dir) {
        Ok((
            ensure_release(sys, data_dir, version, base)?,
            NatsSource::Release,
        ))
    } else {
        Ok((chosen.to_path_buf(), NatsSource::Path))
    }
}

/// Where the pinned release goes.
pub fn release_path(data_dir: &Path) -> PathBuf {
    data_dir.join("bin/nats-server")
}

fn pinned_release(sys: &dyn System, data_dir: &Path, version: &str) -> bool {
    let ours = release_path(data_dir);
    sys.exists(&ours)
        && sys
            .query(
                &Cmd::new(ours.display().to_string())
                    .arg("--version")
                    .timeout(std::time::Duration::from_secs(10)),
            )
            .is_ok_and(|o| {
                o.ok()
                    && o.stdout
                        .split_whitespace()
                        .any(|w| w == format!("v{version}"))
            })
}

/// The pinned release in `<data_dir>/bin`, downloaded when it is missing or another
/// version.
fn ensure_release(sys: &dyn System, data_dir: &Path, version: &str, base: &str) -> Result<PathBuf> {
    if !pinned_release(sys, data_dir, version) {
        install_release(sys, data_dir, version, base)?;
    }
    Ok(release_path(data_dir))
}

/// Download, verify and unpack the pinned release into `<data_dir>/bin/nats-server`.
pub fn install_release(
    sys: &dyn System,
    data_dir: &Path,
    version: &str,
    base: &str,
) -> Result<PathBuf> {
    let env = sys.env();
    let asset = release_asset(version, env.os, &env.arch)?;
    let url = format!("{}/v{version}", base.trim_end_matches('/'));
    let work = data_dir.join("tmp/nats-release");
    let dest = data_dir.join("bin/nats-server");
    sys.remove_dir_all(&work).map_err(io_at(&work))?;
    sys.mkdir(&work).map_err(io_at(&work))?;
    let archive = work.join(&asset);
    let sums = work.join("SHA256SUMS");
    let curl = |file: &Path, name: &str| {
        Cmd::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(file.display().to_string())
            .arg(format!("{url}/{name}"))
    };
    let res = (|| {
        run_ok(sys, &curl(&archive, &asset))?;
        run_ok(sys, &curl(&sums, "SHA256SUMS"))?;
        if sys.dry_run() {
            sys.say(&format!(
                "would verify {asset} against SHA256SUMS and install {}",
                dest.display()
            ));
            return Ok(());
        }
        let data = sys.read(&archive).map_err(io_at(&archive))?;
        let sums_text = sys.read_to_string(&sums).map_err(io_at(&sums))?;
        verify_sha256(&sums_text, &asset, &data)?;
        run_ok(
            sys,
            &Cmd::new("tar")
                .args(["-xzf"])
                .arg(archive.display().to_string())
                .arg("-C")
                .arg(work.display().to_string()),
        )?;
        let unpacked = work
            .join(asset.trim_end_matches(".tar.gz"))
            .join("nats-server");
        let bin = sys.read(&unpacked).map_err(io_at(&unpacked))?;
        sys.write(&dest, &bin, 0o755).map_err(io_at(&dest))
    })();
    sys.remove_dir_all(&work).map_err(io_at(&work))?;
    res.map(|()| dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::RealSystem;
    use crate::sys::fake::{self, Fake};

    #[test]
    fn conf_golden() {
        let c = render_conf(
            "192.0.2.10",
            4222,
            Path::new("/srv/hgl-ai/nats"),
            &Auth::Token("abc123"),
        );
        assert_eq!(
            c,
            r#"# hgl-ai hub, written by `hgl-ai server install`; reinstalling rewrites it.
host: "192.0.2.10"
port: 4222
max_payload: 4MB
pid_file: "/srv/hgl-ai/nats/nats.pid"

jetstream {
  store_dir: "/srv/hgl-ai/nats"
}

authorization {
  token: "abc123"
}
"#
        );
        assert!(
            render_conf("::", 1, Path::new(r#"/a"b"#), &Auth::Token("t"))
                .contains(r#"store_dir: "/a\"b""#)
        );
        let ws = Websocket {
            port: 4223,
            origins: vec!["https://console.example.com".into()],
        };
        let c = render_conf_ws(
            "127.0.0.1",
            4222,
            Path::new("/s"),
            &Auth::Token("t"),
            Some(&ws),
        );
        assert!(
            c.contains(
                "websocket {\n  host: \"127.0.0.1\"\n  port: 4223\n  no_tls: true\n  \
                 allowed_origins: [\"https://console.example.com\"]\n}\n\nauthorization {"
            ),
            "{c}"
        );
    }

    #[test]
    fn websocket_needs_a_console_and_hub_users() {
        let mut cfg = hgl_ai_config::Config::new("hub");
        let mut srv = hgl_ai_config::Server {
            websocket_port: Some(4223),
            ..Default::default()
        };
        cfg.server = Some(srv.clone());
        assert_eq!(websocket_of(&cfg), None, "no console");
        cfg.console = Some(hgl_ai_config::Console {
            url: "https://c.example.com/".into(),
            ..Default::default()
        });
        assert_eq!(
            websocket_of(&cfg),
            Some(Websocket {
                port: 4223,
                origins: vec!["https://c.example.com".into()]
            })
        );
        srv.auth = hgl_ai_config::HubAuth::Token;
        cfg.server = Some(srv.clone());
        assert_eq!(websocket_of(&cfg), None, "a shared token");
        srv.auth = hgl_ai_config::HubAuth::Users;
        srv.websocket_port = None;
        cfg.server = Some(srv);
        assert_eq!(websocket_of(&cfg), None, "not asked for");
    }

    /// The rendered file is a valid `nats-server` configuration (when one is installed).
    #[test]
    fn conf_parses_with_nats_server() {
        let Some(bin) =
            crate::sys::which_in(&std::env::var("PATH").unwrap_or_default(), "nats-server")
        else {
            return;
        };
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("nats.conf");
        std::fs::write(
            &p,
            render_conf(
                "127.0.0.1",
                4222,
                &d.path().join("store"),
                &Auth::Token(&"ab".repeat(32)),
            ),
        )
        .unwrap();
        let o = std::process::Command::new(bin)
            .arg("-t")
            .arg("-c")
            .arg(&p)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }

    #[test]
    fn assets() {
        assert_eq!(
            release_asset("2.15.3", Os::Linux, "x86_64").unwrap(),
            "nats-server-v2.15.3-linux-amd64.tar.gz"
        );
        assert_eq!(
            release_asset("2.15.3", Os::MacOs, "aarch64").unwrap(),
            "nats-server-v2.15.3-darwin-arm64.tar.gz"
        );
        assert!(release_asset("2.15.3", Os::Linux, "riscv64").is_err());
    }

    #[test]
    fn checksums() {
        let data = b"archive bytes";
        let sum = hex::encode(Sha256::digest(data));
        let sums = format!("{}  other.tar.gz\n{sum}  a.tar.gz\n", "0".repeat(64));
        verify_sha256(&sums, "a.tar.gz", data).unwrap();
        verify_sha256(&format!("{sum} *a.tar.gz\n"), "a.tar.gz", data).unwrap();
        assert!(
            verify_sha256(&sums, "a.tar.gz", b"tampered")
                .unwrap_err()
                .to_string()
                .contains("mismatch")
        );
        assert!(
            verify_sha256(&sums, "b.tar.gz", data)
                .unwrap_err()
                .to_string()
                .contains("no entry")
        );
    }

    #[test]
    fn prefers_path_then_brew() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        let data = f.env.home.join(".local/share/hgl-ai");
        let p = f.tool("nats-server");
        assert_eq!(
            ensure_server(&f, &data, "2.15.3", RELEASE_BASE).unwrap(),
            (p, NatsSource::Path)
        );
        assert!(f.runs().is_empty());

        let f = Fake::new(d.path(), Os::MacOs);
        f.tool("brew");
        f.on(|c| {
            if c.args == ["--prefix"] {
                crate::Output::success("/opt/homebrew\n")
            } else {
                crate::Output::success("")
            }
        });
        let (p, src) = ensure_server(&f, &data, "2.15.3", RELEASE_BASE).unwrap();
        assert_eq!(
            (p, src),
            (
                PathBuf::from("/opt/homebrew/bin/nats-server"),
                NatsSource::Brew
            )
        );
        assert_eq!(f.runs(), ["/usr/bin/brew install nats-server"]);
    }

    /// A fake release directory served over `file://`, fetched with the real
    /// `curl` and unpacked with the real `tar`, all inside a temporary home.
    #[cfg(unix)]
    #[test]
    fn release_download_verifies_checksum() {
        let path = std::env::var("PATH").unwrap_or_default();
        if crate::sys::which_in(&path, "curl").is_none()
            || crate::sys::which_in(&path, "tar").is_none()
        {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let mut env = fake::env(&home, Os::Linux);
        env.arch = "x86_64".into();
        env.path_var = path;
        let asset = release_asset("9.9.9", Os::Linux, "x86_64").unwrap();
        let rel = d.path().join("release/v9.9.9");
        let stage = d.path().join("stage/nats-server-v9.9.9-linux-amd64");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::create_dir_all(&rel).unwrap();
        std::fs::write(stage.join("nats-server"), "#!/bin/sh\necho fake\n").unwrap();
        let ok = std::process::Command::new("tar")
            .arg("-czf")
            .arg(rel.join(&asset))
            .arg("-C")
            .arg(d.path().join("stage"))
            .arg("nats-server-v9.9.9-linux-amd64")
            .status()
            .unwrap();
        assert!(ok.success());
        let sum = hex::encode(Sha256::digest(std::fs::read(rel.join(&asset)).unwrap()));
        let base = format!("file://{}", d.path().join("release").display());
        let sys = RealSystem::new(env);
        let data = home.join(".local/share/hgl-ai");

        std::fs::write(
            rel.join("SHA256SUMS"),
            format!("{}  {asset}\n", "f".repeat(64)),
        )
        .unwrap();
        let err = install_release(&sys, &data, "9.9.9", &base).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        assert!(!data.join("bin/nats-server").exists());

        std::fs::write(rel.join("SHA256SUMS"), format!("{sum}  {asset}\n")).unwrap();
        let bin = install_release(&sys, &data, "9.9.9", &base).unwrap();
        assert_eq!(bin, data.join("bin/nats-server"));
        assert_eq!(
            std::fs::read_to_string(&bin).unwrap(),
            "#!/bin/sh\necho fake\n"
        );
        assert_eq!(crate::sys::file_mode(&bin).unwrap(), 0o755);
        assert!(
            !data.join("tmp/nats-release").exists(),
            "download directory removed"
        );
    }
}
