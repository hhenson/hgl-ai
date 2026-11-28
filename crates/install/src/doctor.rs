//! `hgl-ai doctor`: one line per check, with the fix. It changes nothing (but for
//! a probe directory it removes again) and never spends a model turn.

use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::time::Duration;

use hgl_ai_config::slack::{APP_TOKEN_FILE, BOT_TOKEN_FILE};
use hgl_ai_config::{CODEX_PROFILE, Config, ServerUser, codex_profile};
use hgl_ai_proto::Runtime;
use serde::Serialize;

use crate::hub::Hub;
use crate::service::{self, Service, State};
use crate::skill::{self, Status};
use crate::sys::{Cmd, System};
use crate::token;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Check {
    fn new(
        name: impl Into<String>,
        level: Level,
        detail: impl Into<String>,
        fix: Option<String>,
    ) -> Self {
        Check {
            name: name.into(),
            level,
            detail: detail.into(),
            fix,
        }
    }
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Check::new(name, Level::Ok, detail, None)
    }
    fn warn(name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Check::new(name, Level::Warn, detail, Some(fix.into()))
    }
    fn fail(name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Check::new(name, Level::Fail, detail, Some(fix.into()))
    }
    pub fn line(&self) -> String {
        let fix = self
            .fix
            .as_ref()
            .map(|f| format!("; fix: {f}"))
            .unwrap_or_default();
        format!(
            "{:<4} {}: {}{fix}",
            self.level.as_str(),
            self.name,
            self.detail
        )
    }
}

pub fn any_fail(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.level == Level::Fail)
}

const PROBE: Duration = Duration::from_secs(20);

/// Signed in, per the CLI's own status command; `None` when it cannot tell.
pub fn signed_in(sys: &dyn System, rt: Runtime, bin: &Path) -> Option<bool> {
    let b = bin.display().to_string();
    let cmd = match rt {
        Runtime::Claude => Cmd::new(b).args(["auth", "status", "--json"]),
        Runtime::Codex => Cmd::new(b).args(["login", "status"]),
        // No status command: listing models needs a session but no model turn.
        Runtime::Agy => Cmd::new(b).arg("models"),
    };
    let o = sys.query(&cmd.timeout(PROBE)).ok()?;
    if o.timed_out || o.code.is_none() {
        return None;
    }
    match rt {
        Runtime::Claude => serde_json::from_str::<serde_json::Value>(&o.stdout)
            .ok()
            .and_then(|v| v.get("loggedIn").and_then(serde_json::Value::as_bool))
            .or(if o.ok() { None } else { Some(false) }),
        Runtime::Codex => Some(o.ok()),
        Runtime::Agy => Some(
            o.ok()
                && o.stdout
                    .lines()
                    .any(|l| !l.trim().is_empty() && !l.starts_with("Fetching")),
        ),
    }
}

fn sign_in_fix(rt: Runtime) -> &'static str {
    match rt {
        Runtime::Claude => "`claude auth login`",
        Runtime::Codex => "`codex login`",
        Runtime::Agy => "run `agy` once and sign in",
    }
}

/// A session id no rollout has: `exec resume` loads the configuration, then fails
/// without a model turn.
const NO_SESSION: &str = "00000000-0000-0000-0000-000000000000";

/// The Codex CLI applies the permission profile a turn runs under (hgl-ai#59).
/// `codex profile`: `codex sandbox -P hgl_task` with the turn's overrides, a probe
/// directory under `work_root` writable, writes there; a CLI without profiles ignores the
/// overrides and runs read-only. `codex config`: the configuration (`config.toml` and the
/// overrides) loads under `--strict-config`, which every turn passes.
fn codex_checks(sys: &dyn System, bin: &Path, work_root: &Path, used: bool) -> Vec<Check> {
    let bad = |name: &str, detail: String, fix: &str| {
        if used {
            Check::fail(name, format!("{detail}; a seat uses it"), fix)
        } else {
            Check::warn(name, detail, fix)
        }
    };
    let unknown = |name: &str, detail: String| Check::new(name, Level::Warn, detail, None);
    let b = bin.display().to_string();
    let dir = work_root.join(format!(".doctor-codex-{}", std::process::id()));
    let profile = codex_profile(std::slice::from_ref(&dir));
    let overrides: Vec<String> = profile
        .iter()
        .flat_map(|c| ["-c".to_string(), c.clone()])
        .collect();
    let mut out = Vec::new();

    let name = "codex profile";
    let probe = dir.join("probe");
    out.push(match sys.mkdir(&dir) {
        Err(e) => unknown(name, format!("no probe directory {}: {e}", dir.display())),
        Ok(()) => {
            let cmd = Cmd::new(b.clone())
                .args(["sandbox", "-P", CODEX_PROFILE, "-C"])
                .arg(dir.display().to_string())
                .args(overrides.iter().cloned())
                .args(["--", "sh", "-c", ": > \"$1\"", "sh"])
                .arg(probe.display().to_string())
                .timeout(PROBE);
            let o = sys.query(&cmd);
            let wrote = sys.exists(&probe);
            let _ = sys.remove_dir_all(&dir);
            match o {
                Err(e) => unknown(name, format!("`codex sandbox`: {e}")),
                Ok(o) if o.timed_out => unknown(name, "`codex sandbox` timed out".into()),
                Ok(o) if o.ok() && wrote => Check::ok(
                    name,
                    format!("`codex sandbox -P {CODEX_PROFILE}` writes the task directory"),
                ),
                Ok(o) => bad(
                    name,
                    format!(
                        "`codex sandbox -P {CODEX_PROFILE}` did not write ({}: {}); \
                         turns would run read-only, without network",
                        o.status_text(),
                        o.detail()
                    ),
                    "upgrade codex to one with permission profiles (0.172.0 has them)",
                ),
            }
        }
    });

    let name = "codex config";
    let cmd = Cmd::new(b)
        .args(["exec", "resume", "--strict-config"])
        .args(overrides)
        .args(["--skip-git-repo-check", NO_SESSION, "-"])
        .timeout(PROBE);
    out.push(match sys.query(&cmd) {
        Err(e) => unknown(name, format!("`codex exec resume`: {e}")),
        Ok(o) if o.timed_out => unknown(name, "`codex exec resume` timed out".into()),
        Ok(o) => {
            let text = format!("{}\n{}", o.stderr, o.stdout);
            if text.contains("unknown configuration field")
                || text.contains("Error loading config")
                || text.contains("--strict-config")
            {
                let line = text
                    .lines()
                    .find(|l| l.contains("unknown configuration field") || l.contains("strict"))
                    .map_or_else(|| o.detail(), |l| l.trim().to_string());
                bad(
                    name,
                    format!("rejected under `--strict-config`, so every turn fails: {line}"),
                    "remove the field from codex's `config.toml`, or upgrade codex",
                )
            } else if text.contains("No prompt provided") {
                Check::ok(name, "loads under `--strict-config`")
            } else {
                unknown(name, format!("could not tell: {}", o.detail()))
            }
        }
    });
    out
}

fn seat_uses(cfg: Option<&Config>, rt: Runtime) -> bool {
    cfg.is_some_and(|c| {
        c.seats
            .iter()
            .any(|s| s.binding.runtime == rt || s.fallback.iter().any(|b| b.runtime == rt))
    })
}

/// Available space at `p` in decimal GB (10^9 bytes, the unit of `min_free_gb` and of the
/// bridge's own check), from `df -Pk`.
pub fn free_gb(sys: &dyn System, p: &Path) -> Option<u64> {
    let o = sys
        .query(&Cmd::new("df").args(["-Pk"]).arg(p.display().to_string()))
        .ok()?;
    if !o.ok() {
        return None;
    }
    parse_df(&o.stdout)
}

fn parse_df(out: &str) -> Option<u64> {
    let line = out.lines().nth(1)?;
    let f: Vec<&str> = line.split_whitespace().collect();
    let cap = f.iter().position(|w| w.ends_with('%'))?;
    let kb: u64 = f.get(cap.checked_sub(1)?)?.parse().ok()?;
    Some(kb.saturating_mul(1024) / 1_000_000_000)
}

/// Where this user's agent CLIs are: the caller's `PATH`, then the one the bridge's unit
/// records (what turns run with), then `~/.local/bin`. A plain `sudo -u NAME` resets
/// `PATH`, so the caller's alone can miss them.
fn client_path(sys: &dyn System) -> String {
    let env = sys.env();
    let mut dirs: Vec<String> = Vec::new();
    let local = env.home.join(".local/bin").display().to_string();
    let recorded = service::recorded_path(sys, Service::Bridge).unwrap_or_default();
    for d in [env.path_var.as_str(), recorded.as_str(), local.as_str()]
        .iter()
        .flat_map(|p| p.split(':'))
    {
        if !d.is_empty() && !dirs.iter().any(|x| x == d) {
            dirs.push(d.to_string());
        }
    }
    dirs.join(":")
}

/// A secret file: present, and readable by its owner only.
fn secret_check(sys: &dyn System, name: &str, shown: &str) -> Check {
    let p = sys.env().expand(shown);
    match sys.mode(&p) {
        Err(_) => Check::fail(
            name,
            format!("no {shown}"),
            format!("write it, then `chmod 600 {shown}`"),
        ),
        Ok(m) if !token::mode_ok(m) => Check::fail(
            name,
            format!("{shown} has mode {m:o}"),
            format!("`chmod 600 {shown}`"),
        ),
        Ok(_) => Check::ok(name, shown.to_string()),
    }
}

/// This OS user's name, from `id -un`.
fn user_name(sys: &dyn System) -> Option<String> {
    sys.query(&Cmd::new("id").arg("-un"))
        .ok()
        .filter(|o| o.ok())
        .map(|o| o.stdout.trim().to_string())
        .filter(|n| !n.is_empty())
}

/// The server user of a hub host whose server side runs as its own OS user. The
/// installer's configuration records it (`[server_user]`); the client user's, from
/// `client install`, does not, so there it is the user the installed hub unit runs as,
/// with the `HOME` the unit gives it (`--server-home`, which need not be its account's).
fn server_user(sys: &dyn System, c: &Config, me: Option<&str>) -> Option<ServerUser> {
    let mut user = if let Some(user) = &c.server_user {
        user.clone()
    } else {
        if c.server.is_some() {
            return None;
        }
        let (name, home) =
            service::system_account(sys, Service::Hub).filter(|(n, _)| Some(n.as_str()) != me)?;
        let home = home.or_else(|| crate::system::home_of(sys, &name))?;
        ServerUser {
            name,
            home: home.display().to_string(),
            console_url: None,
            slack: false,
            client_user: None,
        }
    };
    user.slack = crate::system::slack_enabled(sys, &user);
    Some(user)
}

const SLACK_CRED: &str = "hub-users/slack.cred";
const SLACK_FILES: [&str; 3] = [SLACK_CRED, BOT_TOKEN_FILE, APP_TOKEN_FILE];

fn slack_server_checks(dir: &Path, uid: Option<u32>) -> Vec<Check> {
    SLACK_FILES
        .into_iter()
        .map(|file| {
            let name = format!("Slack {file}");
            let path = dir.join(file);
            match std::fs::symlink_metadata(&path) {
                Err(e) => Check::fail(
                    name,
                    e.to_string(),
                    "`hgl-ai server install --slack --keep-token`",
                ),
                Ok(meta) if !meta.is_file() => Check::fail(
                    name,
                    "expected a regular file",
                    format!(
                        "replace {} with a regular mode-0600 file owned by the server user",
                        path.display()
                    ),
                ),
                Ok(meta) if meta.mode() & 0o7777 != 0o600 => {
                    let mode = meta.mode() & 0o7777;
                    Check::fail(
                        name,
                        format!("mode {mode:o}, expected 600"),
                        format!("`chmod 600 {}`", path.display()),
                    )
                }
                Ok(meta) if Some(meta.uid()) != uid => Check::fail(
                    name,
                    "expected ownership by the server user",
                    format!("`sudo chown $(id -un) {}`", path.display()),
                ),
                Ok(_) => Check::ok(name, "owned by the server user, mode 600"),
            }
        })
        .collect()
}

/// The server side and the client side apart (docs/install.md): the server user's home
/// is its own (mode 0700), and owner-channel credentials are out of the client user's
/// reach (checked where the client runs: `client` false for an installer whose client
/// is another user); on the server user's side, its credentials are its own, mode 0600.
fn separation_checks(
    sys: &dyn System,
    c: &Config,
    server_user: Option<&ServerUser>,
    client: bool,
) -> Vec<Check> {
    let env = sys.env();
    let mut out = Vec::new();
    let me = sys
        .query(&Cmd::new("id").arg("-u"))
        .ok()
        .and_then(|o| o.stdout.trim().parse::<u32>().ok());
    if let Some(su) = server_user {
        let home = env.expand(&su.home);
        let stat = match env.os {
            crate::Os::Linux => Cmd::new("stat").args(["-c", "%U %a"]),
            crate::Os::MacOs => Cmd::new("stat").args(["-f", "%Su %Lp"]),
        }
        .arg(home.display().to_string());
        let got = sys
            .query(&stat)
            .ok()
            .filter(|o| o.ok())
            .map(|o| o.stdout.trim().to_string());
        let want = format!("{} 700", su.name);
        out.push(match got {
            Some(g) if g == want => Check::ok(
                "server user",
                format!("{} owns {} (mode 700)", su.name, su.home),
            ),
            Some(g) => Check::fail(
                "server user",
                format!("{}: owner and mode are {g}, not {want}", su.home),
                format!(
                    "`sudo chown -R {0} {1} && sudo chmod 700 {1}`",
                    su.name, su.home
                ),
            ),
            None => Check::fail(
                "server user",
                format!("no {}", su.home),
                "`hgl-ai server install`",
            ),
        });
        let cred = home.join(".config/hgl-ai/hub-users/console.cred");
        if client {
            out.push(if sys.can_read(&cred) {
                Check::fail(
                    "console credential",
                    format!(
                        "{} is readable by you, the client user: an agent could speak as the owner",
                        cred.display()
                    ),
                    format!(
                        "`sudo chmod 700 {}` and `sudo chmod 600 {}`",
                        su.home,
                        cred.display()
                    ),
                )
            } else {
                Check::ok("console credential", "not readable by the client user")
            });
            if su.slack {
                let dir = home.join(".config/hgl-ai");
                for file in SLACK_FILES {
                    let name = format!("Slack {file}");
                    let path = dir.join(file);
                    out.push(if sys.can_read(&path) {
                        Check::fail(
                            name,
                            format!("{} is readable by the client user", path.display()),
                            format!("`sudo chmod 700 {}`", su.home),
                        )
                    } else {
                        Check::ok(name, "not readable by the client user")
                    });
                }
            }
        }
    } else if let Some(srv) = &c.server {
        let cred = env.config_dir().join("hub-users/console.cred");
        if srv.units == hgl_ai_config::Units::User && sys.exists(&cred) {
            out.push(Check::warn(
                "console credential",
                "a same-user install: this user, and every agent it runs, can read the console's credential",
                "install the server side as its own user (`hgl-ai server install` without --same-user)",
            ));
        }
        if srv.units == hgl_ai_config::Units::System {
            // As the server user: its secrets are its own, mode 0600.
            let config_dir = env.config_dir();
            let slack_cred = config_dir.join(SLACK_CRED);
            if c.slack.is_some() {
                out.extend(slack_server_checks(&config_dir, me));
            }
            let mut files = vec![env.expand(&c.hub.token_file)];
            let dir = config_dir.join("hub-users");
            if let Ok(rd) = std::fs::read_dir(&dir) {
                files.extend(rd.flatten().map(|e| e.path()));
            }
            for f in files.iter().filter(|f| sys.exists(f)) {
                if c.slack.is_some() && *f == slack_cred {
                    continue;
                }
                let shown = env.contract(f);
                let mode = sys.mode(f).ok();
                let uid = sys.owner_uid(f).ok();
                out.push(match (mode, uid, me) {
                    (Some(m), _, _) if !crate::token::mode_ok(m) => Check::fail(
                        format!("secret {shown}"),
                        format!("mode {m:o}"),
                        format!("`chmod 600 {shown}`"),
                    ),
                    (_, Some(u), Some(me)) if u != me => Check::fail(
                        format!("secret {shown}"),
                        "owned by another user".to_string(),
                        format!("`sudo chown $(id -un) {shown}`"),
                    ),
                    _ => Check::ok(format!("secret {shown}"), "mode 600, own"),
                });
            }
        }
    }
    out
}

/// `[console]`: who can sign in, how, and the secrets beside the configuration.
fn console_checks(sys: &dyn System, c: &hgl_ai_config::Console) -> Vec<Check> {
    let mut out = Vec::new();
    let missing = c.missing_handles();
    let no_email = c.without_email();
    let members = c.members().unwrap_or_default();
    out.push(if c.users.is_empty() {
        Check::warn(
            "console users",
            "nobody may sign in",
            "add `{ email = \"<email>\", handle = \"<name>\" }` to [console] users",
        )
    } else if !no_email.is_empty() {
        let fixes: Vec<String> = members
            .iter()
            .filter(|m| m.email.is_none())
            .map(|m| {
                let mut f = format!(
                    "{{ email = \"<email>\", handle = \"{}\"",
                    m.handle.as_deref().unwrap_or("<name>")
                );
                for (k, v) in [
                    ("github", &m.github),
                    ("apple", &m.apple),
                    ("google", &m.google),
                ] {
                    if let Some(v) = v {
                        f.push_str(&format!(", {k} = \"{v}\""));
                    }
                }
                f.push_str(" }");
                f
            })
            .collect();
        Check::fail(
            "console users",
            format!(
                "no email for {}: a user is keyed by email",
                no_email.join(", ")
            ),
            format!("write {}", fixes.join(", ")),
        )
    } else if !missing.is_empty() {
        Check::warn(
            "console users",
            format!(
                "no handle for {}: their words are signed by email",
                missing.join(", ")
            ),
            "add `handle = \"<the name they sign with>\"` (OWN-10)",
        )
    } else {
        let names: Vec<String> = members
            .iter()
            .map(|m| {
                let h = m.handle.as_deref().unwrap_or("?");
                let note = if h.chars().count() > 12 {
                    "; a shorter handle reads better"
                } else {
                    ""
                };
                format!("{h} ({}){note}", m.key)
            })
            .collect();
        Check::ok("console users", names.join(", "))
    });
    if c.github.is_none() && c.apple.is_none() && c.google.is_none() {
        out.push(Check::warn(
            "console sign-in",
            "no GitHub, Apple or Google app: a first sign-in or a new device cannot enrol a passkey",
            "set [console.github], [console.apple] or [console.google] (docs/install.md)",
        ));
    }
    if let Some(g) = &c.google {
        out.push(secret_check(sys, "console google", &g.secret_file));
    }
    if let Some(g) = &c.github {
        out.push(secret_check(sys, "console github", &g.secret_file));
    }
    if let Some(a) = &c.apple {
        out.push(secret_check(sys, "console apple", &a.key_file));
    }
    match (&c.tls_cert, &c.tls_key) {
        (Some(cert), Some(key)) => {
            let p = sys.env().expand(cert);
            out.push(if sys.exists(&p) {
                Check::ok("console tls", format!("{cert} for {}", c.url))
            } else {
                Check::fail(
                    "console tls",
                    format!("no {cert}"),
                    "write the certificate for the console's name",
                )
            });
            out.push(secret_check(sys, "console tls key", key));
        }
        _ => out.push(Check::ok(
            "console tls",
            format!(
                "plain HTTP on {} behind a TLS proxy for {}",
                c.listen, c.url
            ),
        )),
    }
    if c.notify.is_empty() {
        out.push(Check::warn(
            "console notify",
            "no channel: p1 questions show only in the console",
            "add [[console.notify]] (ntfy, slack, email)",
        ));
    }
    for n in &c.notify {
        match n {
            hgl_ai_config::Notify::Ntfy {
                token_file: Some(t),
                ..
            } => out.push(secret_check(sys, "console notify ntfy", t)),
            hgl_ai_config::Notify::Slack { webhook_file } => {
                out.push(secret_check(sys, "console notify slack", webhook_file))
            }
            hgl_ai_config::Notify::Email {
                password_file: Some(p),
                ..
            } => out.push(secret_check(sys, "console notify email", p)),
            _ => {}
        }
    }
    out
}

/// Run every check. Checks that need the configuration are skipped when it
/// does not load.
pub async fn run(sys: &dyn System, hub: &impl Hub) -> Vec<Check> {
    let env = sys.env();
    let mut out = Vec::new();
    let cfg = match crate::load_config(sys, env) {
        Ok(Some(c)) => {
            out.push(Check::ok(
                "config",
                format!(
                    "{} (host {}, {} seats)",
                    env.config_path.display(),
                    c.host,
                    c.seats.len()
                ),
            ));
            Some(c)
        }
        Ok(None) => {
            out.push(Check::fail(
                "config",
                format!("no {}", env.config_path.display()),
                "`hgl-ai client install`",
            ));
            None
        }
        Err(e) => {
            out.push(Check::fail(
                "config",
                e.to_string(),
                format!("edit {}", env.config_path.display()),
            ));
            None
        }
    };
    let reinstall = cfg.as_ref().map_or_else(
        || "`hgl-ai client install`".to_string(),
        |c| {
            format!(
                "`hgl-ai client install --hub {} --token-file {} --host {}`",
                c.hub.url, c.hub.token_file, c.host
            )
        },
    );
    let me = user_name(sys);
    let su = cfg
        .as_ref()
        .and_then(|c| server_user(sys, c, me.as_deref()));
    // The installer of a hub host whose client runs as another user (`--client-user`):
    // the client's checks are that user's, against its home.
    let client_user = cfg
        .as_ref()
        .and_then(|c| c.server_user.as_ref())
        .and_then(|s| s.client_user.clone())
        .filter(|u| Some(u.as_str()) != me.as_deref());
    if let Some(u) = &client_user {
        // A service account may have no login shell: `env HOME=`, not `sudo -i` (nor
        // `-H`, which sudo-rs lacks).
        let home = crate::system::home_of(sys, u)
            .map_or_else(|| "<its home>".to_string(), |h| h.display().to_string());
        out.push(Check::ok(
            "client user",
            format!(
                "{u} runs this host's client (bridge, agents); its checks: \
                 `sudo -u {u} env HOME={home} hgl-ai doctor`"
            ),
        ));
    }

    if let Some(c) = cfg.as_ref().filter(|_| client_user.is_none()) {
        let tp = env.expand(&c.hub.token_file);
        let shown = &c.hub.token_file;
        let cred = token::read_credential(sys, &tp);
        let what = match &cred {
            Ok(Some(c)) => c.describe(),
            _ => String::new(),
        };
        out.push(match &cred {
            Ok(None) => Check::fail(
                "token",
                format!("no {shown}"),
                format!("copy the hub credential there, then {reinstall}"),
            ),
            Err(e) => Check::fail(
                "token",
                e.to_string(),
                format!("copy the hub credential to {shown}"),
            ),
            Ok(Some(_)) => match sys.mode(&tp) {
                Ok(m) if token::mode_ok(m) => {
                    Check::ok("token", format!("{shown} ({what}, mode {m:o})"))
                }
                Ok(m) => Check::fail(
                    "token",
                    format!("{shown} has mode {m:o}"),
                    format!("`chmod 600 {shown}`"),
                ),
                Err(e) => Check::fail("token", e.to_string(), format!("`chmod 600 {shown}`")),
            },
        });
        let token_mode = c
            .server
            .as_ref()
            .is_some_and(|s| s.auth == hgl_ai_config::HubAuth::Token)
            || matches!(&cred, Ok(Some(c)) if c.is_token());
        out.push(if token_mode {
            Check::warn(
                "hub auth",
                "single shared token: any client can publish as the owner (OWN-5)",
                "give each client its own hub user: docs/install.md, Hub users",
            )
        } else {
            let n = c.server.as_ref().map(|_| {
                crate::users::Registry::load(sys, &env.expand(&c.paths.data_dir))
                    .map(|r| r.users.len())
                    .unwrap_or(0)
            });
            Check::ok(
                "hub auth",
                match n {
                    Some(n) => format!("hub users ({n} registered)"),
                    None => "hub user".to_string(),
                },
            )
        });
        out.push(match hub.health(c).await {
            Err(e) => Check::fail(
                "hub",
                format!("unreachable at {}: {e}", c.hub.url),
                "check the link to the hub and the token",
            ),
            Ok(m) if !m.is_empty() => Check::fail(
                "hub",
                format!("{} missing {}", c.hub.url, m.join(", ")),
                "`hgl-ai init`",
            ),
            Ok(_) => Check::ok("hub", format!("{} (stream and buckets present)", c.hub.url)),
        });
    }

    // The server user (a system install) runs no agents: no CLIs, skills, work_root
    // or tmux to check there.
    let server_side = cfg.as_ref().is_some_and(|c| {
        c.server
            .as_ref()
            .is_some_and(|s| s.units == hgl_ai_config::Units::System)
    });
    // A configuration with no seats has no agent-side services or tools.
    let no_seats = cfg.as_ref().is_some_and(|c| c.seats.is_empty());
    let builder_only = no_seats
        && sys
            .read_to_string(&env.config_path)
            .ok()
            .and_then(|text| {
                text.parse::<toml::Value>()
                    .ok()
                    .map(|v| v.get("builder").is_some())
            })
            .unwrap_or(false);
    let no_client = server_side || client_user.is_some() || no_seats;
    let path = client_path(sys);
    let clis: Vec<(Runtime, std::path::PathBuf)> = if no_client {
        Vec::new()
    } else {
        Runtime::ALL
            .iter()
            .filter_map(|&rt| sys.which_on(&path, rt.as_str()).map(|p| (rt, p)))
            .collect()
    };
    for rt in Runtime::ALL.iter().copied().filter(|_| !no_client) {
        let name = format!("cli {rt}");
        let used = seat_uses(cfg.as_ref(), rt);
        let Some((_, bin)) = clis.iter().find(|(r, _)| *r == rt) else {
            out.push(if used {
                Check::fail(name, "not on PATH; a seat uses it", format!("install {rt}"))
            } else {
                Check::new(name, Level::Warn, "not on PATH", None)
            });
            continue;
        };
        out.push(match signed_in(sys, rt, bin) {
            Some(true) => Check::ok(name, format!("{} (signed in)", bin.display())),
            Some(false) if used => {
                Check::fail(name, "not signed in; a seat uses it", sign_in_fix(rt))
            }
            Some(false) => Check::warn(name, "not signed in", sign_in_fix(rt)),
            None => Check::new(
                name,
                Level::Warn,
                format!("{} (sign-in unknown)", bin.display()),
                None,
            ),
        });
        let sp = env.contract(&skill::skill_path(env, rt));
        out.push(match skill::status(sys, rt) {
            Status::Current => Check::ok(format!("skill {rt}"), sp),
            Status::Stale => Check::warn(
                format!("skill {rt}"),
                format!("{sp} is outdated"),
                reinstall.clone(),
            ),
            Status::Missing => {
                Check::fail(format!("skill {rt}"), format!("no {sp}"), reinstall.clone())
            }
        });
    }

    if let Some(c) = &cfg {
        let system_units = c
            .server
            .as_ref()
            .is_some_and(|s| s.units == hgl_ai_config::Units::System);
        let slack =
            (system_units && c.slack.is_some()) || su.as_ref().is_some_and(|user| user.slack);
        // The server user (a system install) has no client of its own, nor has an
        // installer whose client is another user.
        let mut svcs = Vec::new();
        if !system_units && client_user.is_none() {
            if !no_seats {
                svcs.push((Service::Bridge, reinstall.clone()));
            }
            svcs.push((Service::Builder, reinstall.clone()));
        }
        if c.server.is_some() || su.is_some() {
            svcs.push((Service::Hub, "`hgl-ai server install`".into()));
            svcs.push((Service::Coord, "`hgl-ai server install`".into()));
            svcs.push((
                Service::Console,
                "`hgl-ai server install --console-url https://<stable name>`".into(),
            ));
        }
        if slack {
            svcs.push((Service::Slack, "`hgl-ai server install --slack`".into()));
        }
        // The client's own services (bridge, builder) are never the server user's.
        let client = |s: Service| matches!(s, Service::Bridge | Service::Builder);
        let system = |s: Service| system_units || (!client(s) && su.is_some());
        for (s, install) in svcs {
            let name = format!("service {}", s.short());
            if s == Service::Builder
                && let Some(user) = c
                    .builder
                    .user
                    .as_deref()
                    .filter(|u| Some(*u) != me.as_deref())
            {
                let system_state = service::system_state(sys, s);
                if system_state == State::NotInstalled {
                    let uid = sys
                        .query(&Cmd::new("id").args(["-u", user]))
                        .ok()
                        .filter(|o| o.ok())
                        .map(|o| o.stdout.trim().to_string());
                    let result = uid.and_then(|uid| {
                        let cmd = match env.os {
                            crate::Os::Linux => Cmd::new("systemctl").args([
                                "--user",
                                "--machine",
                                &format!("{user}@"),
                                "show",
                                "-p",
                                "LoadState,ActiveState",
                                &s.unit(),
                            ]),
                            crate::Os::MacOs => Cmd::new("launchctl")
                                .args(["print", &format!("gui/{uid}/{}", s.label())]),
                        };
                        sys.query(&cmd).ok().filter(|o| o.ok())
                    });
                    out.push(match result {
                        Some(o)
                            if o.stdout.lines().any(|l| {
                                matches!(l.trim(), "ActiveState=active" | "state = running")
                            }) =>
                        {
                            Check::ok(name, format!("running (user {user})"))
                        }
                        Some(_) => Check::fail(
                            name,
                            format!("not running (user {user})"),
                            format!("check the builder service as {user}"),
                        ),
                        None => Check::new(
                            name,
                            Level::Warn,
                            format!("cannot check user {user}'s builder service"),
                            None,
                        ),
                    });
                    continue;
                }
            }
            let local = if system(s) {
                State::NotInstalled
            } else {
                service::state(sys, s)
            };
            let use_system = system(s)
                || (s == Service::Builder
                    && local == State::NotInstalled
                    && service::system_state(sys, s) != State::NotInstalled);
            let st = if use_system {
                service::system_state(sys, s)
            } else {
                local
            };
            let hint = if use_system {
                let start = match env.os {
                    crate::Os::Linux => format!("`sudo systemctl start {}`", s.unit()),
                    crate::Os::MacOs => format!("`sudo launchctl kickstart system/{}`", s.label()),
                };
                format!("{start}; logs: {}", service::system_log_hint(env.os, s))
            } else {
                format!("`{}`", service::start_hint(env, s))
            };
            out.push(match st {
                State::Running => Check::ok(
                    name,
                    if use_system {
                        match (s, service::system_account(sys, s)) {
                            (Service::Builder, Some((user, _))) => {
                                format!("running (system, user {user})")
                            }
                            _ => "running (system)".to_string(),
                        }
                    } else {
                        "running".to_string()
                    },
                ),
                State::Stopped => Check::fail(name, "installed, not running", hint),
                State::NotInstalled if s == Service::Coord => {
                    Check::new(name, Level::Warn, "not installed on this hub host", None)
                }
                State::NotInstalled if s == Service::Console => Check::warn(
                    name,
                    "not installed on this hub host; the owner has no console",
                    install,
                ),
                State::NotInstalled if s == Service::Builder => Check::new(
                    name,
                    Level::Warn,
                    "not installed (`client install --no-builder`?)",
                    None,
                ),
                State::NotInstalled => Check::fail(name, "not installed", install),
            });
            if use_system
                && st != State::NotInstalled
                && let Some(why) = service::system_log_unsafe(sys, s)
            {
                out.push(Check::fail(
                    format!("service {} log", s.short()),
                    format!("{why}: root opens it in the server user's home (hgl-ai#63)"),
                    crate::system::LOG_UPGRADE,
                ));
            }
        }
        if let Some(con) = &c.console {
            out.extend(console_checks(sys, con));
        }
        out.extend(separation_checks(
            sys,
            c,
            su.as_ref(),
            client_user.is_none(),
        ));
        // Lingering keeps the client's user services up: none here on the server side,
        // nor for an installer whose client is another user.
        if !no_client && let Ok(Some(on)) = service::linger(sys, false) {
            out.push(if on {
                Check::ok("lingering", "on")
            } else {
                Check::warn(
                    "lingering",
                    "off; services stop at logout",
                    "`loginctl enable-linger`",
                )
            });
        }

        let wr = env.expand(&c.paths.work_root);
        let shown = &c.paths.work_root;
        out.push(if server_side {
            Check::ok("work_root", "none on the server side")
        } else if let Some(u) = &client_user {
            Check::ok("work_root", format!("none here: the client runs as {u}"))
        } else if !sys.exists(&wr) {
            Check::fail(
                "work_root",
                format!("no {shown}"),
                format!("`mkdir -p {shown}`"),
            )
        } else {
            match free_gb(sys, &wr) {
                Some(g) if g < c.limits.min_free_gb => Check::fail(
                    "work_root",
                    format!(
                        "{shown}: {g} GB free, below min_free_gb {}",
                        c.limits.min_free_gb
                    ),
                    "free space, or `hgl-ai tree list --stale`",
                ),
                Some(g) => Check::ok("work_root", format!("{shown}: {g} GB free")),
                None => Check::new(
                    "work_root",
                    Level::Warn,
                    format!("{shown}: free space unknown"),
                    None,
                ),
            }
        });
        if !no_client
            && sys.exists(&wr)
            && let Some((_, bin)) = clis.iter().find(|(r, _)| *r == Runtime::Codex)
        {
            out.extend(codex_checks(
                sys,
                bin,
                &wr,
                seat_uses(Some(c), Runtime::Codex),
            ));
        }
    }

    let tool = |name: &str| sys.which(name);
    if !no_client {
        out.push(match tool("tmux") {
            Some(p) => Check::ok("tmux", p.display().to_string()),
            None => Check::warn(
                "tmux",
                "not on PATH; `hgl-ai agent start` needs it",
                "install tmux",
            ),
        });
    }
    if builder_only
        && let Some(credential) = cfg.as_ref().and_then(|c| c.builder.credential.as_deref())
    {
        let path = env.expand(credential);
        let uid = sys
            .query(&Cmd::new("id").arg("-u"))
            .ok()
            .filter(|o| o.ok())
            .and_then(|o| o.stdout.trim().parse::<u32>().ok());
        out.push(match (sys.mode(&path), sys.owner_uid(&path), uid) {
            (Ok(mode), Ok(owner), Some(me)) if mode == 0o600 && owner == me => Check::ok(
                "builder credential",
                format!("{credential} (mode 600, own)"),
            ),
            _ => Check::fail(
                "builder credential",
                format!("{credential} must be owned by this user with mode 600"),
                format!("check ownership and `chmod 600 {credential}`"),
            ),
        });
    }
    // `gh` with the token the coordinator uses (`[github] token_file`), when there is one.
    let token_file = cfg.as_ref().and_then(|c| c.github.token_file.clone());
    let mut gh_token = None;
    if let Some(tf) = &token_file {
        let p = env.expand(tf);
        out.push(match sys.mode(&p) {
            Err(_) => Check::fail(
                "github token",
                format!("no {tf}"),
                "write a fine-grained read-only token there, mode 0600 (docs/install.md)",
            ),
            Ok(m) if !token::mode_ok(m) => Check::fail(
                "github token",
                format!("{tf} has mode {m:o}"),
                format!("`chmod 600 {tf}`"),
            ),
            Ok(_) => match sys.read_to_string(&p) {
                Ok(t) if !t.trim().is_empty() => {
                    gh_token = Some(t.trim().to_string());
                    Check::ok("github token", format!("{tf} (mode 600)"))
                }
                _ => Check::fail(
                    "github token",
                    format!("{tf} is empty"),
                    "write the token there",
                ),
            },
        });
    }
    out.push(match tool("git") {
        Some(p) => Check::ok("git", p.display().to_string()),
        None => Check::fail("git", "not on PATH", "install git"),
    });
    // `gh` is the client's (and, with a token, the coordinator's).
    if builder_only || (client_user.is_some() && token_file.is_none()) {
        return out;
    }
    out.push(match tool("gh") {
        None => Check::fail("gh", "not on PATH", "install gh"),
        Some(p) => match sys.query(&{
            let c = Cmd::new(p.display().to_string())
                .args(["auth", "status"])
                .timeout(PROBE);
            match &gh_token {
                Some(t) => c.env("GH_TOKEN", t.clone()),
                None => c,
            }
        }) {
            Ok(o) if o.ok() => Check::ok("gh", format!("{} (authenticated)", p.display())),
            Ok(o) if o.timed_out => {
                Check::new("gh", Level::Warn, "`gh auth status` timed out", None)
            }
            _ if token_file.is_some() => Check::fail(
                "gh",
                "`gh auth status` failed with the token in [github] token_file",
                "renew the token",
            ),
            _ => Check::fail("gh", "`gh auth status` failed", "`gh auth login`"),
        },
    });
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::hub::fake::FakeHub;
    use crate::sys::fake::Fake;
    use crate::{Os, Output};

    const DF: &str = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/x 976762584 100 524288000 47% /home\n";

    #[test]
    fn df_parsing() {
        // 524288000 KiB is 536.9 GB: the bridge's unit, so the two agree at the threshold.
        assert_eq!(parse_df(DF), Some(536));
        assert_eq!(
            parse_df("Filesystem\nmy disk 10 1 2097152 1% /a b\n"),
            Some(2)
        );
        // 200.0 to 214.7 GB free: both say at least 200 (Carbon's finding 18).
        assert_eq!(
            parse_df("Filesystem\n/dev/x 1 1 195312500 1% /w\n"),
            Some(200)
        );
        assert_eq!(parse_df("garbage"), None);
    }

    #[test]
    fn lines() {
        assert_eq!(
            Check::ok("git", "/usr/bin/git").line(),
            "ok   git: /usr/bin/git"
        );
        assert_eq!(
            Check::fail("gh", "not on PATH", "install gh").line(),
            "FAIL gh: not on PATH; fix: install gh"
        );
    }

    /// A host where everything is in order, then broken one piece at a time.
    fn healthy(d: &Path) -> Fake {
        let f = Fake::new(d, Os::Linux);
        for t in ["claude", "codex", "tmux", "git", "gh"] {
            f.tool(t);
        }
        let home = f.env.home.clone();
        let mut cfg = Config::new("build-1");
        cfg.seats = toml::from_str::<Config>(
            "host = \"x\"\n[[seat]]\nname = \"w1\"\nrole = \"implementor\"\nruntime = \"codex\"\ntier = \"frontier\"\n",
        )
        .unwrap()
        .seats;
        crate::save_config(&f, &f.env, &cfg).unwrap();
        let cred = hgl_ai_bus::Credential::render_nkey(
            "build-1",
            "seat",
            &nkeys::KeyPair::new_user().seed().unwrap(),
        );
        crate::sys::write_atomic(
            &home.join(".config/hgl-ai/hub.token"),
            cred.as_bytes(),
            0o600,
        )
        .unwrap();
        std::fs::create_dir_all(home.join("hgl-ai/work")).unwrap();
        for rt in [Runtime::Claude, Runtime::Codex] {
            skill::install(&f, rt).unwrap();
        }
        std::fs::create_dir_all(home.join(".config/systemd/user")).unwrap();
        std::fs::write(home.join(".config/systemd/user/hgl-ai-bridge.service"), "").unwrap();
        std::fs::write(home.join(".config/systemd/user/hgl-ai-builder.service"), "").unwrap();
        f.on(|c| {
            let a: Vec<&str> = c.args.iter().map(String::as_str).collect();
            match (c.program.as_str(), a.as_slice()) {
                (_, ["auth", "status", "--json"]) => Output::success(r#"{"loggedIn": true}"#),
                (_, ["sandbox", ..] | ["exec", "resume", ..]) => codex_ok(&a),
                ("id", _) => Output::success("1000\n"),
                ("loginctl", _) => Output::success("yes\n"),
                ("systemctl", [_, "is-active", _]) => Output::success("active\n"),
                ("df", _) => Output::success(DF),
                _ => Output::success(""),
            }
        });
        f
    }

    /// codex 0.172.0 for the two probes: `exec resume` with no prompt, and `sandbox`.
    fn codex_ok(a: &[&str]) -> Output {
        if a[0] == "exec" {
            return Output::failure(1, "No prompt provided via stdin.");
        }
        codex_sandbox(a)
    }

    /// `codex sandbox` (0.172.0): applies `-P` from the overrides; the probe writes its
    /// last argument when the profile makes its directory writable.
    fn codex_sandbox(a: &[&str]) -> Output {
        let dir = a[a.iter().position(|x| *x == "-C").unwrap() + 1];
        let profile = codex_profile(&[PathBuf::from(dir)]);
        let applied = profile.iter().all(|p| a.contains(&p.as_str()))
            && a.windows(2).any(|w| w == ["-P", CODEX_PROFILE]);
        if !applied {
            return Output::failure(
                1,
                "Error: default_permissions requires a `[permissions]` table",
            );
        }
        let probe = a.last().unwrap();
        assert!(probe.starts_with(dir), "{probe} outside {dir}");
        std::fs::write(probe, "").unwrap();
        Output::success("")
    }

    fn levels(cs: &[Check]) -> Vec<(String, Level)> {
        cs.iter().map(|c| (c.name.clone(), c.level)).collect()
    }

    #[tokio::test]
    async fn builder_only_skips_agent_checks() {
        let d = tempfile::tempdir().unwrap();
        let f = healthy(d.path());
        let mut cfg = Config::new("build-1");
        cfg.builder.credential = Some("~/builder.cred".into());
        crate::save_config(&f, &f.env, &cfg).unwrap();
        let uid = std::fs::metadata(&f.env.home).unwrap().uid();
        f.on(
            move |c| match (c.program.as_str(), c.args.first().map(String::as_str)) {
                ("id", Some("-u")) => Output::success(format!("{uid}\n")),
                ("systemctl", _) => Output::success("active\n"),
                ("df", _) => Output::success(DF),
                _ => Output::success(""),
            },
        );
        std::fs::write(f.env.home.join("builder.cred"), "credential").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            f.env.home.join("builder.cred"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let cs = run(&f, &FakeHub::up()).await;
        for name in ["service bridge", "cli codex", "skill codex", "tmux", "gh"] {
            assert!(!cs.iter().any(|c| c.name == name), "unexpected {name}");
        }
        assert!(
            cs.iter()
                .any(|c| c.name == "builder credential" && c.level == Level::Ok)
        );
        assert!(
            cs.iter()
                .any(|c| c.name == "service builder" && c.level == Level::Ok)
        );
    }

    #[tokio::test]
    async fn builder_other_user_service_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let f = healthy(d.path());
        let mut cfg = crate::load_config(&f, &f.env).unwrap().unwrap();
        cfg.builder.user = Some("builder".into());
        crate::save_config(&f, &f.env, &cfg).unwrap();
        f.on(
            |c| match (c.program.as_str(), c.args.first().map(String::as_str)) {
                ("id", Some("-un")) => Output::success("agents\n"),
                ("id", _) => Output::success("1001\n"),
                ("systemctl", Some("--user")) => {
                    Output::success("LoadState=loaded\nActiveState=active\n")
                }
                ("systemctl", _) => Output::success("LoadState=not-found\nActiveState=inactive\n"),
                _ => Output::success(""),
            },
        );
        let cs = run(&f, &FakeHub::up()).await;
        assert!(cs.iter().any(|c| c.name == "service builder"
            && c.level == Level::Ok
            && c.detail.contains("user builder")));
    }

    #[tokio::test]
    async fn builder_system_service_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let f = healthy(d.path());
        std::fs::remove_file(
            f.env
                .home
                .join(".config/systemd/user/hgl-ai-builder.service"),
        )
        .unwrap();
        f.on(|c| {
            if c.program == "systemctl" && c.args.iter().any(|a| a == "LoadState,ActiveState") {
                Output::success("LoadState=loaded\nActiveState=active\n")
            } else {
                Output::success("")
            }
        });
        let cs = run(&f, &FakeHub::up()).await;
        assert!(cs.iter().any(|c| c.name == "service builder"
            && c.level == Level::Ok
            && c.detail.contains("system")));
    }

    #[tokio::test]
    async fn all_good() {
        let d = tempfile::tempdir().unwrap();
        let f = healthy(d.path());
        let cs = run(&f, &FakeHub::up()).await;
        let bad: Vec<_> = cs
            .iter()
            .filter(|c| c.level != Level::Ok && c.name != "cli agy")
            .collect();
        assert!(bad.is_empty(), "{bad:?}");
        assert!(!any_fail(&cs));
        assert_eq!(
            levels(&cs).iter().find(|(n, _)| n == "cli agy").unwrap().1,
            Level::Warn
        );
        assert!(
            cs.iter()
                .any(|c| c.name == "work_root" && c.detail.contains("536 GB free"))
        );
        assert!(f.runs().is_empty(), "doctor changes nothing");
        let work = f.env.home.join("hgl-ai/work");
        assert!(
            std::fs::read_dir(&work).unwrap().next().is_none(),
            "the codex probe directory is removed"
        );
        for n in ["codex profile", "codex config"] {
            assert!(cs.iter().any(|c| c.name == n), "no check {n}");
        }
        let auth = cs.iter().find(|c| c.name == "hub auth").unwrap();
        assert_eq!(auth.detail, "hub user");
        assert!(
            cs.iter()
                .any(|c| c.name == "token" && c.detail.contains("hub user build-1 (seat)"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn console_configuration() {
        let d = tempfile::tempdir().unwrap();
        let f = healthy(d.path());
        let mut cfg = crate::load_config(&f, &f.env).unwrap().unwrap();
        let secret = f.env.home.join(".config/hgl-ai/console/github.secret");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, "s\n").unwrap();
        std::fs::set_permissions(&secret, std::os::unix::fs::PermissionsExt::from_mode(0o644))
            .unwrap();
        cfg.console = Some(hgl_ai_config::Console {
            url: "https://hub.example.com".into(),
            github: Some(hgl_ai_config::GithubOAuth::new("Iv1.example")),
            ..hgl_ai_config::Console::default()
        });
        crate::save_config(&f, &f.env, &cfg).unwrap();
        let cs = run(&f, &FakeHub::up()).await;
        let get = |n: &str| {
            cs.iter()
                .find(|c| c.name == n)
                .unwrap_or_else(|| panic!("{n}"))
        };
        assert_eq!(get("console users").level, Level::Warn, "nobody");
        let mut with_old = cfg.clone();
        with_old.console.as_mut().unwrap().users = vec![
            hgl_ai_config::User::Plain("hhenson".into()),
            hgl_ai_config::User::person("o@example.com", "This Dude"),
        ];
        crate::save_config(&f, &f.env, &with_old).unwrap();
        let cs2 = run(&f, &FakeHub::up()).await;
        let users = cs2.iter().find(|c| c.name == "console users").unwrap();
        assert_eq!(users.level, Level::Fail, "an earlier form: no email");
        assert!(
            users.detail.contains("no email for github:hhenson"),
            "{users:?}"
        );
        assert!(
            users
                .fix
                .as_deref()
                .unwrap()
                .contains("{ email = \"<email>\", handle = \"<name>\", github = \"hhenson\" }"),
            "{users:?}"
        );
        with_old.console.as_mut().unwrap().users =
            vec![hgl_ai_config::User::Person(hgl_ai_config::Person {
                email: "o@example.com".into(),
                handle: None,
                github: None,
                apple: None,
                google: None,
            })];
        crate::save_config(&f, &f.env, &with_old).unwrap();
        let cs3 = run(&f, &FakeHub::up()).await;
        let users = cs3.iter().find(|c| c.name == "console users").unwrap();
        assert_eq!(users.level, Level::Warn, "no handle");
        with_old.console.as_mut().unwrap().users = vec![hgl_ai_config::User::person(
            "o@example.com",
            "Somebody Rather Long",
        )];
        crate::save_config(&f, &f.env, &with_old).unwrap();
        let cs4 = run(&f, &FakeHub::up()).await;
        let users = cs4.iter().find(|c| c.name == "console users").unwrap();
        assert_eq!(users.level, Level::Ok, "a long handle is only noted");
        assert!(users.detail.contains("shorter handle reads better"));
        crate::save_config(&f, &f.env, &cfg).unwrap();
        assert_eq!(get("console github").level, Level::Fail);
        assert!(
            get("console github")
                .fix
                .as_deref()
                .unwrap()
                .contains("chmod 600")
        );
        assert_eq!(get("console tls").level, Level::Ok);
        assert_eq!(get("console notify").level, Level::Warn);
    }

    /// The client side of a hub host whose server side runs as `hgl-ai`.
    #[tokio::test]
    async fn server_user_and_the_console_credential() {
        let d = tempfile::tempdir().unwrap();
        let f = healthy(d.path());
        let mut cfg = crate::load_config(&f, &f.env).unwrap().unwrap();
        cfg.server_user = Some(hgl_ai_config::ServerUser {
            name: "hgl-ai".into(),
            home: "/var/lib/hgl-ai".into(),
            console_url: None,
            slack: false,
            client_user: None,
        });
        crate::save_config(&f, &f.env, &cfg).unwrap();
        let stat = std::rc::Rc::new(std::cell::RefCell::new("hgl-ai 700".to_string()));
        let st = stat.clone();
        f.on(move |c| {
            let a: Vec<&str> = c.args.iter().map(String::as_str).collect();
            match (c.program.as_str(), a.as_slice()) {
                (_, ["auth", "status", "--json"]) => Output::success(r#"{"loggedIn": true}"#),
                (_, ["sandbox", ..] | ["exec", "resume", ..]) => codex_ok(&a),
                ("id", _) => Output::success("1000\n"),
                ("loginctl", _) => Output::success("yes\n"),
                ("systemctl", [_, "is-active", _]) => Output::success("active\n"),
                (
                    "systemctl",
                    [
                        "show",
                        "-p",
                        "LoadState,User,Environment",
                        "hgl-ai-slack.service",
                    ],
                ) => Output::success("LoadState=loaded\nUser=hgl-ai\n"),
                ("systemctl", ["show", ..]) => {
                    Output::success("LoadState=loaded\nActiveState=active\n")
                }
                ("stat", _) => Output::success(format!("{}\n", st.borrow())),
                ("df", _) => Output::success(DF),
                _ => Output::success(""),
            }
        });
        let cs = run(&f, &FakeHub::up()).await;
        let get = |cs: &[Check], n: &str| cs.iter().find(|c| c.name == n).cloned().unwrap();
        assert_eq!(get(&cs, "server user").level, Level::Ok);
        assert_eq!(get(&cs, "console credential").level, Level::Ok);
        assert_eq!(get(&cs, "service hub").detail, "running (system)");
        assert_eq!(get(&cs, "service slack").detail, "running (system)");
        for file in SLACK_FILES {
            assert_eq!(get(&cs, &format!("Slack {file}")).level, Level::Ok);
        }
        assert!(
            !any_fail(&cs),
            "{:?}",
            cs.iter()
                .filter(|c| c.level == Level::Fail)
                .collect::<Vec<_>>()
        );

        for file in SLACK_FILES {
            f.readable
                .borrow_mut()
                .insert(Path::new("/var/lib/hgl-ai/.config/hgl-ai").join(file));
        }
        let cs = run(&f, &FakeHub::up()).await;
        for file in SLACK_FILES {
            let check = get(&cs, &format!("Slack {file}"));
            assert_eq!(check.level, Level::Fail, "{check:?}");
            assert!(check.detail.contains("readable by the client user"));
        }
        assert!(!f.cmds.borrow().iter().any(|(_, cmd)| cmd.program == "sudo"));

        // A unit from before hgl-ai#63: root appends to a file in the server user's home.
        let st = stat.clone();
        f.on(move |c| {
            let a: Vec<&str> = c.args.iter().map(String::as_str).collect();
            match (c.program.as_str(), a.as_slice()) {
                (_, ["auth", "status", "--json"]) => Output::success(r#"{"loggedIn": true}"#),
                (_, ["sandbox", ..] | ["exec", "resume", ..]) => codex_ok(&a),
                ("id", _) => Output::success("1000\n"),
                ("loginctl", _) => Output::success("yes\n"),
                ("systemctl", ["show", "-p", "StandardOutput,StandardError", u])
                    if *u == "hgl-ai-hub.service" =>
                {
                    Output::success("StandardOutput=append\nStandardError=append\n")
                }
                ("systemctl", ["show", ..]) => {
                    Output::success("LoadState=loaded\nActiveState=active\n")
                }
                ("stat", _) => Output::success(format!("{}\n", st.borrow())),
                ("df", _) => Output::success(DF),
                _ => Output::success(""),
            }
        });
        let cs = run(&f, &FakeHub::up()).await;
        let log = get(&cs, "service hub log");
        assert_eq!(log.level, Level::Fail);
        assert!(log.detail.contains("StandardOutput=append"), "{log:?}");
        assert!(log.fix.unwrap().contains("rerun `hgl-ai server install`"));
        assert!(!cs.iter().any(|c| c.name == "service coord log"));

        // The console credential within the client user's reach, or a home open to others.
        f.readable
            .borrow_mut()
            .insert("/var/lib/hgl-ai/.config/hgl-ai/hub-users/console.cred".into());
        *stat.borrow_mut() = "hgl-ai 755".into();
        let cs = run(&f, &FakeHub::up()).await;
        assert_eq!(get(&cs, "console credential").level, Level::Fail);
        let su = get(&cs, "server user");
        assert_eq!(su.level, Level::Fail);
        assert!(su.fix.unwrap().contains("sudo chmod 700 /var/lib/hgl-ai"));
    }

    #[test]
    fn slack_server_secrets_must_be_regular_private_files() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        let dir = f.env.config_dir();
        let cred = dir.join(SLACK_CRED);
        crate::sys::write_atomic(&cred, b"test", 0o600).unwrap();
        std::os::unix::fs::symlink(&cred, dir.join(BOT_TOKEN_FILE)).unwrap();
        std::fs::create_dir(dir.join(APP_TOKEN_FILE)).unwrap();
        let uid = std::fs::metadata(&cred).unwrap().uid();
        f.on(move |cmd| {
            assert_eq!(cmd.program, "id");
            assert_eq!(cmd.args, ["-u"]);
            Output::success(format!("{uid}\n"))
        });
        let mut cfg = Config::new("hub");
        cfg.server = Some(hgl_ai_config::Server {
            units: hgl_ai_config::Units::System,
            ..Default::default()
        });
        cfg.slack = Some(hgl_ai_config::slack::Slack {
            team: "TTEST".into(),
            channel: "CTEST".into(),
            user: "UTEST".into(),
            handle: "Owner".into(),
        });
        let checks = separation_checks(&f, &cfg, None, false);
        let slack = |file| {
            checks
                .iter()
                .find(|c| c.name == format!("Slack {file}"))
                .unwrap()
        };
        assert_eq!(slack(SLACK_CRED).level, Level::Ok);
        for file in [BOT_TOKEN_FILE, APP_TOKEN_FILE] {
            assert_eq!(slack(file).level, Level::Fail);
            assert_eq!(slack(file).detail, "expected a regular file");
        }
        assert_eq!(checks.len(), 3, "Slack credentials get one check each");
    }

    /// The installer of a hub host whose client runs as `agents` (`--client-user`): no
    /// client checks against the installer's home, and a pointer to `agents`' doctor.
    #[tokio::test]
    async fn installer_with_a_separate_client_user() {
        let d = tempfile::tempdir().unwrap();
        let f = healthy(d.path());
        let mut cfg = crate::load_config(&f, &f.env).unwrap().unwrap();
        cfg.seats.clear();
        cfg.server_user = Some(hgl_ai_config::ServerUser {
            name: "hgl-ai".into(),
            home: "/var/lib/hgl-ai".into(),
            console_url: None,
            slack: false,
            client_user: Some("agents".into()),
        });
        crate::save_config(&f, &f.env, &cfg).unwrap();
        // The installer has no client credential, skills or work_root of its own.
        std::fs::remove_file(f.env.home.join(".config/hgl-ai/hub.token")).unwrap();
        std::fs::remove_dir_all(f.env.home.join("hgl-ai")).unwrap();
        for rt in [Runtime::Claude, Runtime::Codex] {
            std::fs::remove_file(skill::skill_path(&f.env, rt)).unwrap();
        }
        let whoami = std::rc::Rc::new(std::cell::RefCell::new("owner".to_string()));
        let who = whoami.clone();
        f.on(move |c| {
            let a: Vec<&str> = c.args.iter().map(String::as_str).collect();
            match (c.program.as_str(), a.as_slice()) {
                (_, ["auth", "status", "--json"]) => Output::success(r#"{"loggedIn": true}"#),
                (_, ["sandbox", ..] | ["exec", "resume", ..]) => codex_ok(&a),
                ("id", ["-un"]) => Output::success(format!("{}\n", who.borrow())),
                ("id", _) => Output::success("1000\n"),
                ("loginctl", _) => Output::success("no\n"),
                ("systemctl", [_, "is-active", _]) => Output::failure(3, "inactive"),
                ("systemctl", ["show", ..]) => {
                    Output::success("LoadState=loaded\nActiveState=active\nUser=hgl-ai\n")
                }
                ("getent", ["passwd", "agents"]) => {
                    Output::success("agents:x:1001:1001::/home/agents:/usr/sbin/nologin\n")
                }
                ("stat", _) => Output::success("hgl-ai 700\n"),
                ("df", _) => Output::success(DF),
                _ => Output::success(""),
            }
        });
        let cs = run(&f, &FakeHub::down()).await;
        let names: Vec<&str> = cs.iter().map(|c| c.name.as_str()).collect();
        let cu = cs.iter().find(|c| c.name == "client user").unwrap();
        assert_eq!(cu.level, Level::Ok);
        assert!(
            cu.detail
                .contains("`sudo -u agents env HOME=/home/agents hgl-ai doctor`"),
            "{cu:?}"
        );
        for n in [
            "token",
            "hub",
            "hub auth",
            "cli claude",
            "cli codex",
            "skill claude",
            "service bridge",
            "service builder",
            "codex profile",
            "tmux",
            "lingering",
            "gh",
            "console credential",
        ] {
            assert!(
                !names.contains(&n),
                "{n} checked for the installer: {names:?}"
            );
        }
        assert!(
            !cs.iter().any(|c| c.line().contains("hub.token")),
            "no advice to copy the client credential here: {cs:?}"
        );
        let wr = cs.iter().find(|c| c.name == "work_root").unwrap();
        assert_eq!(
            (wr.level, wr.detail.as_str()),
            (Level::Ok, "none here: the client runs as agents")
        );
        assert_eq!(
            cs.iter().find(|c| c.name == "server user").unwrap().level,
            Level::Ok
        );
        let hub = cs.iter().find(|c| c.name == "service hub").unwrap();
        assert_eq!(hub.detail, "running (system)");
        assert!(!any_fail(&cs), "{cs:?}");

        // The same configuration, read as `agents` itself: the client is here.
        *whoami.borrow_mut() = "agents".into();
        let cs = run(&f, &FakeHub::down()).await;
        assert!(!cs.iter().any(|c| c.name == "client user"));
        assert_eq!(
            cs.iter().find(|c| c.name == "token").unwrap().level,
            Level::Fail
        );
        assert!(cs.iter().any(|c| c.name == "console credential"));
    }

    /// The client user of a hub host: its configuration (`client install`) names no
    /// server user, so doctor finds it from the installed hub unit and checks the
    /// separation.
    #[tokio::test]
    async fn client_user_finds_the_server_user() {
        let d = tempfile::tempdir().unwrap();
        let f = healthy(d.path());
        let respond = |f: &Fake, unit: &'static str| {
            f.on(move |c| {
                let a: Vec<&str> = c.args.iter().map(String::as_str).collect();
                match (c.program.as_str(), a.as_slice()) {
                    (_, ["auth", "status", "--json"]) => Output::success(r#"{"loggedIn": true}"#),
                    (_, ["sandbox", ..] | ["exec", "resume", ..]) => codex_ok(&a),
                    ("id", ["-un"]) => Output::success("agents\n"),
                    ("id", _) => Output::success("1001\n"),
                    ("loginctl", _) => Output::success("yes\n"),
                    ("systemctl", [_, "is-active", _]) => Output::success("active\n"),
                    ("systemctl", ["show", "-p", "LoadState,User,Environment", u])
                        if *u == "hgl-ai-hub.service" =>
                    {
                        Output::success(unit)
                    }
                    ("systemctl", ["show", ..]) => {
                        Output::success("LoadState=loaded\nActiveState=active\n")
                    }
                    ("getent", ["passwd", "hgl-ai"]) => {
                        Output::success("hgl-ai:x:998:998::/var/lib/hgl-ai:/usr/sbin/nologin\n")
                    }
                    ("stat", [.., h]) if h.ends_with("/hgl-ai") => Output::success("hgl-ai 700\n"),
                    ("stat", _) => Output::success("root 755\n"),
                    ("df", _) => Output::success(DF),
                    _ => Output::success(""),
                }
            })
        };
        respond(&f, "LoadState=loaded\nUser=hgl-ai\n");
        let cs = run(&f, &FakeHub::up()).await;
        let get = |cs: &[Check], n: &str| {
            cs.iter()
                .find(|c| c.name == n)
                .cloned()
                .unwrap_or_else(|| panic!("no check {n}: {cs:?}"))
        };
        let su = get(&cs, "server user");
        assert_eq!(su.level, Level::Ok);
        assert_eq!(su.detail, "hgl-ai owns /var/lib/hgl-ai (mode 700)");
        assert_eq!(get(&cs, "console credential").level, Level::Ok);
        assert_eq!(get(&cs, "service hub").detail, "running (system)");
        assert_eq!(get(&cs, "service bridge").detail, "running");
        assert!(!cs.iter().any(|c| c.name == "client user"));
        assert!(!any_fail(&cs), "{cs:?}");

        f.readable
            .borrow_mut()
            .insert("/var/lib/hgl-ai/.config/hgl-ai/hub-users/console.cred".into());
        let cs = run(&f, &FakeHub::up()).await;
        assert_eq!(get(&cs, "console credential").level, Level::Fail);

        // The unit's `HOME` (`--server-home`) wins over the account's home: an existing
        // server user whose passwd home is another directory.
        respond(
            &f,
            "LoadState=loaded\nEnvironment=HOME=/srv/hgl-ai PATH=/usr/bin\nUser=hgl-ai\n",
        );
        let cs = run(&f, &FakeHub::up()).await;
        assert_eq!(
            get(&cs, "server user").detail,
            "hgl-ai owns /srv/hgl-ai (mode 700)"
        );
        assert_eq!(
            get(&cs, "console credential").level,
            Level::Ok,
            "the passwd home's credential is not the server's"
        );
        f.readable
            .borrow_mut()
            .insert("/srv/hgl-ai/.config/hgl-ai/hub-users/console.cred".into());
        let cs = run(&f, &FakeHub::up()).await;
        let cred = get(&cs, "console credential");
        assert_eq!(cred.level, Level::Fail);
        assert!(cred.detail.starts_with("/srv/hgl-ai/"), "{cred:?}");

        // No hub unit (a client-only host), or one that runs as this user: nothing to
        // separate.
        for unit in [
            "LoadState=not-found\nUser=\n",
            "LoadState=loaded\nUser=agents\n",
        ] {
            respond(&f, unit);
            let cs = run(&f, &FakeHub::up()).await;
            for n in ["server user", "console credential", "service hub"] {
                assert!(!cs.iter().any(|c| c.name == n), "{n} for {unit:?}");
            }
        }
    }

    /// The client user's doctor under a plain `sudo -u NAME env HOME=...`: no
    /// `XDG_RUNTIME_DIR` and a reset `PATH`. The bridge is still found running, and the
    /// CLIs on the bridge unit's `PATH` or in `~/.local/bin`.
    #[tokio::test]
    async fn under_a_plain_sudo() {
        let d = tempfile::tempdir().unwrap();
        let mut f = healthy(d.path());
        let home = f.env.home.clone();
        f.env.path_var = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into();
        let run_user = d.path().join("run/user");
        std::fs::create_dir_all(run_user.join("1000")).unwrap();
        f.env.runtime_root = Some(run_user);
        f.tool_at("claude", &home.join(".local/bin"));
        f.tool_at("codex", Path::new("/opt/codex/bin"));
        std::fs::write(
            home.join(".config/systemd/user/hgl-ai-bridge.service"),
            "[Service]\nEnvironment=\"PATH=/opt/codex/bin:/usr/bin:/bin\"\n",
        )
        .unwrap();
        f.on(|c| {
            let a: Vec<&str> = c.args.iter().map(String::as_str).collect();
            let session = c.env.iter().any(|(k, _)| k == "XDG_RUNTIME_DIR");
            match (c.program.as_str(), a.as_slice()) {
                (_, ["auth", "status", "--json"]) => Output::success(r#"{"loggedIn": true}"#),
                (_, ["sandbox", ..] | ["exec", "resume", ..]) => codex_ok(&a),
                ("id", _) => Output::success("1000\n"),
                ("loginctl", _) => Output::success("yes\n"),
                ("systemctl", ["--user", "is-active", _]) if session => Output::success("active\n"),
                ("systemctl", ["--user", ..]) => {
                    Output::failure(1, "Failed to connect to bus: No medium found")
                }
                ("df", _) => Output::success(DF),
                _ => Output::success(""),
            }
        });
        let cs = run(&f, &FakeHub::up()).await;
        let get = |n: &str| cs.iter().find(|c| c.name == n).unwrap();
        assert_eq!(get("service bridge").detail, "running");
        let claude = get("cli claude");
        assert_eq!(claude.level, Level::Ok, "{claude:?}");
        assert!(claude.detail.contains(".local/bin/claude"), "{claude:?}");
        assert_eq!(get("cli codex").detail, "/opt/codex/bin/codex (signed in)");
        assert!(!any_fail(&cs), "{cs:?}");

        // Without the runtime directory, the bridge cannot be told from a stopped one.
        f.env.runtime_root = Some(d.path().join("elsewhere"));
        let cs = run(&f, &FakeHub::up()).await;
        let bridge = cs.iter().find(|c| c.name == "service bridge").unwrap();
        assert_eq!(bridge.level, Level::Fail);
    }

    /// The server user's own side: its secrets are mode 0600 and its own.
    #[cfg(unix)]
    #[tokio::test]
    async fn server_side_secrets() {
        let d = tempfile::tempdir().unwrap();
        let f = healthy(d.path());
        let mut cfg = crate::load_config(&f, &f.env).unwrap().unwrap();
        cfg.seats.clear();
        cfg.server = Some(hgl_ai_config::Server {
            units: hgl_ai_config::Units::System,
            ..hgl_ai_config::Server::default()
        });
        crate::save_config(&f, &f.env, &cfg).unwrap();
        let dir = f.env.home.join(".config/hgl-ai/hub-users");
        std::fs::create_dir_all(&dir).unwrap();
        crate::sys::write_atomic(&dir.join("console.cred"), b"x", 0o644).unwrap();
        let cs = run(&f, &FakeHub::up()).await;
        let bad = cs
            .iter()
            .find(|c| c.name.ends_with("hub-users/console.cred"))
            .unwrap();
        assert_eq!(bad.level, Level::Fail);
        assert!(bad.fix.as_deref().unwrap().contains("chmod 600"));
        assert!(
            !cs.iter().any(|c| c.name == "service bridge"),
            "the server user runs no bridge"
        );
        for n in ["skill claude", "cli codex", "tmux", "lingering"] {
            assert!(!cs.iter().any(|c| c.name == n), "{n} on the server side");
        }
        assert_eq!(
            cs.iter().find(|c| c.name == "work_root").unwrap().level,
            Level::Ok
        );

        // The coordinator's GitHub token: mode 0600, and passed to `gh auth status`.
        let mut cfg = crate::load_config(&f, &f.env).unwrap().unwrap();
        cfg.github.token_file = Some("~/.config/hgl-ai/github.token".into());
        crate::save_config(&f, &f.env, &cfg).unwrap();
        let tok = f.env.home.join(".config/hgl-ai/github.token");
        crate::sys::write_atomic(&tok, b"github_pat_example\n", 0o644).unwrap();
        let cs = run(&f, &FakeHub::up()).await;
        assert_eq!(
            cs.iter().find(|c| c.name == "github token").unwrap().level,
            Level::Fail
        );
        crate::sys::write_atomic(&tok, b"github_pat_example\n", 0o600).unwrap();
        run(&f, &FakeHub::up()).await;
        let with_token = f.cmds.borrow().iter().any(|(_, c)| {
            c.args == ["auth", "status"]
                && c.env
                    .iter()
                    .any(|(k, v)| k == "GH_TOKEN" && v == "github_pat_example")
        });
        assert!(with_token, "gh auth status ran with the token");
        assert!(
            !cs.iter()
                .any(|c| c.name == "console credential" && c.level == Level::Warn)
        );
    }

    #[tokio::test]
    async fn warns_on_a_shared_token() {
        let d = tempfile::tempdir().unwrap();
        let f = healthy(d.path());
        crate::sys::write_atomic(
            &f.env.home.join(".config/hgl-ai/hub.token"),
            b"c0ffee\n",
            0o600,
        )
        .unwrap();
        let cs = run(&f, &FakeHub::up()).await;
        let auth = cs.iter().find(|c| c.name == "hub auth").unwrap();
        assert_eq!(auth.level, Level::Warn);
        assert!(auth.detail.contains("publish as the owner"), "{auth:?}");
        assert!(!any_fail(&cs));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_each_problem() {
        let d = tempfile::tempdir().unwrap();
        let f = healthy(d.path());
        let home = f.env.home.clone();
        std::fs::set_permissions(
            home.join(".config/hgl-ai/hub.token"),
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .unwrap();
        std::fs::write(skill::skill_path(&f.env, Runtime::Claude), "old").unwrap();
        std::fs::remove_file(skill::skill_path(&f.env, Runtime::Codex)).unwrap();
        f.tools.borrow_mut().remove("gh");
        f.on(|c| {
            let a: Vec<&str> = c.args.iter().map(String::as_str).collect();
            match (c.program.as_str(), a.as_slice()) {
                (_, ["auth", "status", "--json"]) => Output {
                    code: Some(1),
                    stdout: r#"{"loggedIn": false}"#.into(),
                    ..Output::default()
                },
                (_, ["login", "status"]) => Output::failure(1, "Not logged in"),
                // A CLI without permission profiles or `--strict-config`.
                (_, ["sandbox", ..]) => Output::failure(2, "error: unexpected argument '-P' found"),
                (_, ["exec", "resume", ..]) => {
                    Output::failure(2, "error: unexpected argument '--strict-config' found")
                }
                ("id", _) => Output::success("1000\n"),
                ("loginctl", _) => Output::success("no\n"),
                ("systemctl", [_, "is-active", _]) => Output::failure(3, "inactive"),
                ("df", _) => Output::success(DF.replace("524288000", "10485760")),
                _ => Output::success(""),
            }
        });
        let mut hub = FakeHub::up();
        hub.missing = vec!["HGL_TREES".into()];
        let cs = run(&f, &hub).await;
        let get = |n: &str| {
            cs.iter()
                .find(|c| c.name == n)
                .unwrap_or_else(|| panic!("no check {n}"))
        };
        assert_eq!(get("token").level, Level::Fail);
        assert!(get("token").fix.as_deref().unwrap().contains("chmod 600"));
        assert_eq!(get("hub").level, Level::Fail);
        assert_eq!(get("hub").fix.as_deref(), Some("`hgl-ai init`"));
        assert_eq!(
            get("cli claude").level,
            Level::Warn,
            "not signed in, no seat uses it"
        );
        assert_eq!(
            get("cli codex").level,
            Level::Fail,
            "not signed in, seat w1 uses it"
        );
        assert_eq!(get("skill claude").level, Level::Warn);
        assert_eq!(get("skill codex").level, Level::Fail);
        assert!(
            get("skill codex")
                .fix
                .as_deref()
                .unwrap()
                .contains("--host build-1")
        );
        assert_eq!(get("service bridge").level, Level::Fail);
        assert_eq!(get("lingering").level, Level::Warn);
        assert_eq!(get("work_root").level, Level::Fail, "10 GB is below 200");
        assert_eq!(get("gh").level, Level::Fail);
        assert_eq!(get("codex profile").level, Level::Fail);
        assert!(get("codex profile").detail.contains("read-only"));
        assert_eq!(get("codex config").level, Level::Fail);
        assert!(get("codex config").detail.contains("'--strict-config'"));
        assert!(any_fail(&cs));
    }

    /// The CLI runs the sandbox but ignores the profile (read-only), and `config.toml`
    /// has a field this CLI does not know: both fail for a seat's runtime, warn otherwise.
    #[test]
    fn codex_profile_ignored_or_config_rejected() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        let work = f.env.home.join("work");
        std::fs::create_dir_all(&work).unwrap();
        f.on(|c| match c.args.first().map(String::as_str) {
            Some("sandbox") => Output::success(""),
            _ => Output::failure(
                1,
                "Error loading config.toml:\n/h/.codex/config.toml:2:1: unknown configuration \
                 field `not_a_field`\n  |\n2 | not_a_field = 1",
            ),
        });
        let cs = codex_checks(&f, Path::new("/usr/bin/codex"), &work, true);
        assert_eq!(
            levels(&cs),
            [
                ("codex profile".to_string(), Level::Fail),
                ("codex config".to_string(), Level::Fail)
            ]
        );
        assert!(
            cs[0].detail.contains("did not write (exit 0"),
            "{}",
            cs[0].detail
        );
        assert!(cs[1].detail.contains("`not_a_field`"), "{}", cs[1].detail);
        assert!(std::fs::read_dir(&work).unwrap().next().is_none());
        let q = f.cmds.borrow();
        let strict = &q.iter().find(|(_, c)| c.args[0] == "exec").unwrap().1.args;
        assert!(
            strict.contains(&"--strict-config".to_string()),
            "{strict:?}"
        );
        assert_eq!(strict.last().map(String::as_str), Some("-"));
        drop(q);
        let cs = codex_checks(&f, Path::new("/usr/bin/codex"), &work, false);
        assert!(cs.iter().all(|c| c.level == Level::Warn), "{cs:?}");
    }

    #[tokio::test]
    async fn no_config_still_checks_tools() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        f.tool("git");
        let cs = run(&f, &FakeHub::down()).await;
        assert_eq!(cs[0].name, "config");
        assert_eq!(cs[0].level, Level::Fail);
        assert!(cs.iter().all(|c| c.name != "hub" && c.name != "token"));
        assert!(cs.iter().any(|c| c.name == "git" && c.level == Level::Ok));
        let j = serde_json::to_value(&cs).unwrap();
        assert_eq!(j[0]["level"], "fail");
    }

    #[tokio::test]
    async fn unreachable_hub() {
        let d = tempfile::tempdir().unwrap();
        let f = healthy(d.path());
        let cs = run(&f, &FakeHub::down()).await;
        let h = cs.iter().find(|c| c.name == "hub").unwrap();
        assert_eq!(h.level, Level::Fail);
        assert!(h.detail.contains("unreachable"));
    }
}
