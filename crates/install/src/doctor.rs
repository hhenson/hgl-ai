//! `hgl-ai doctor`: one line per check, with the fix. It changes nothing and
//! never spends a model turn.

use std::path::Path;
use std::time::Duration;

use hgl_ai_config::Config;
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

    if let Some(c) = &cfg {
        let tp = env.expand(&c.hub.token_file);
        let shown = &c.hub.token_file;
        out.push(match token::read(sys, &tp) {
            Ok(None) => Check::fail(
                "token",
                format!("no {shown}"),
                format!("copy the hub token there, then {reinstall}"),
            ),
            Err(e) => Check::fail(
                "token",
                e.to_string(),
                format!("copy the hub token to {shown}"),
            ),
            Ok(Some(_)) => match sys.mode(&tp) {
                Ok(m) if token::mode_ok(m) => Check::ok("token", format!("{shown} (mode {m:o})")),
                Ok(m) => Check::fail(
                    "token",
                    format!("{shown} has mode {m:o}"),
                    format!("`chmod 600 {shown}`"),
                ),
                Err(e) => Check::fail("token", e.to_string(), format!("`chmod 600 {shown}`")),
            },
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

    let clis = skill::installed_clis(sys);
    for rt in Runtime::ALL.iter().copied() {
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
        let mut svcs = vec![(Service::Bridge, reinstall.clone())];
        if c.server.is_some() {
            svcs.push((Service::Hub, "`hgl-ai server install`".into()));
            svcs.push((Service::Coord, "`hgl-ai server install`".into()));
        }
        for (s, install) in svcs {
            let name = format!("service {}", s.short());
            out.push(match service::state(sys, s) {
                State::Running => Check::ok(name, "running"),
                State::Stopped => Check::fail(
                    name,
                    "installed, not running",
                    format!("`{}`", service::start_hint(env, s)),
                ),
                State::NotInstalled if s == Service::Coord => {
                    Check::new(name, Level::Warn, "not installed on this hub host", None)
                }
                State::NotInstalled => Check::fail(name, "not installed", install),
            });
        }
        if let Ok(Some(on)) = service::linger(sys, false) {
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
        out.push(if !sys.exists(&wr) {
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
    }

    let tool = |name: &str| sys.which(name);
    out.push(match tool("tmux") {
        Some(p) => Check::ok("tmux", p.display().to_string()),
        None => Check::warn(
            "tmux",
            "not on PATH; `hgl-ai agent start` needs it",
            "install tmux",
        ),
    });
    out.push(match tool("git") {
        Some(p) => Check::ok("git", p.display().to_string()),
        None => Check::fail("git", "not on PATH", "install git"),
    });
    out.push(match tool("gh") {
        None => Check::fail("gh", "not on PATH", "install gh"),
        Some(p) => match sys.query(
            &Cmd::new(p.display().to_string())
                .args(["auth", "status"])
                .timeout(PROBE),
        ) {
            Ok(o) if o.ok() => Check::ok("gh", format!("{} (authenticated)", p.display())),
            Ok(o) if o.timed_out => {
                Check::new("gh", Level::Warn, "`gh auth status` timed out", None)
            }
            _ => Check::fail("gh", "`gh auth status` failed", "`gh auth login`"),
        },
    });
    out
}

#[cfg(test)]
mod tests {
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
        crate::sys::write_atomic(&home.join(".config/hgl-ai/hub.token"), b"t\n", 0o600).unwrap();
        std::fs::create_dir_all(home.join("hgl-ai/work")).unwrap();
        for rt in [Runtime::Claude, Runtime::Codex] {
            skill::install(&f, rt).unwrap();
        }
        std::fs::create_dir_all(home.join(".config/systemd/user")).unwrap();
        std::fs::write(home.join(".config/systemd/user/hgl-ai-bridge.service"), "").unwrap();
        f.on(|c| {
            let a: Vec<&str> = c.args.iter().map(String::as_str).collect();
            match (c.program.as_str(), a.as_slice()) {
                (_, ["auth", "status", "--json"]) => Output::success(r#"{"loggedIn": true}"#),
                ("id", _) => Output::success("1000\n"),
                ("loginctl", _) => Output::success("yes\n"),
                ("systemctl", [_, "is-active", _]) => Output::success("active\n"),
                ("df", _) => Output::success(DF),
                _ => Output::success(""),
            }
        });
        f
    }

    fn levels(cs: &[Check]) -> Vec<(String, Level)> {
        cs.iter().map(|c| (c.name.clone(), c.level)).collect()
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
        assert!(any_fail(&cs));
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
