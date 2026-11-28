//! Hub users (OWN-5, docs/install.md): every client has its own NKey user with the
//! permissions of its role. The console and Slack publish owner words on separate subjects.
//!
//! The registry `<data_dir>/hub-users.toml` (mode 0600) holds names, roles and public
//! keys only; each credential (the NKey seed) is written once, to a file of mode 0600,
//! and copied to its host by hand. `nats.conf` lists the public keys with their roles'
//! permissions.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use hgl_ai_bus::Credential;
use hgl_ai_proto::build::KV_BUILD;
use hgl_ai_proto::{
    CONSOLE_PREFIX, KV_CONSOLE, KV_ENDED, KV_HOSTS, KV_LOCKS, KV_METRICS, KV_SLACK, KV_STATE,
    KV_THREADS, KV_TREES, KV_USAGE, OWNER_NOTES_PREFIX, OWNER_PREFIX, SLACK_FEED,
    SLACK_OWNER_FILTER, STREAM_NOTES, STREAM_OWNER, STREAM_USAGE, SUBJECT_PREFIX, USAGE_PREFIX,
};
use serde::{Deserialize, Serialize};

use crate::sys::System;

/// The prefix of the local build verdict keys in `HGL_STATE`.
const VERDICT_PREFIX: &str = "verdict.";
use crate::{Result, io_at, other, token};

/// What a hub user may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A client host: its bridge, agents and command line. Notes to seats, agent traffic
    /// for the owner, the shared buckets; never the owner's subjects.
    Seat,
    /// The coordinator service: a seat's rights and `HGL_METRICS`.
    Coord,
    /// A builder service (`builder run`, docs/build.md BUILD-15): a seat's rights and the
    /// local build verdicts (`HGL_STATE/verdict.*`), which no seat may write.
    Builder,
    /// The owner's console: the owner's subjects, `HGL_THREADS`, `tick` notes.
    Console,
    /// Slack Q&A: owner actions, its own feed and delivery state.
    Slack,
    /// Everything: provisioning the stream and buckets (`hgl-ai init`, `server install`).
    Admin,
}

impl Role {
    pub const ALL: [Role; 6] = [
        Role::Seat,
        Role::Coord,
        Role::Builder,
        Role::Console,
        Role::Slack,
        Role::Admin,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Seat => "seat",
            Role::Coord => "coord",
            Role::Builder => "builder",
            Role::Console => "console",
            Role::Slack => "slack",
            Role::Admin => "admin",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        Role::ALL
            .into_iter()
            .find(|r| r.as_str() == s)
            .ok_or_else(|| {
                format!("unknown hub role {s:?}: seat, coord, builder, console, slack or admin")
            })
    }
}

/// One registered user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub name: String,
    pub role: Role,
    /// The public NKey (`U...`).
    pub nkey: String,
    pub created_at: String,
}

/// `<data_dir>/hub-users.toml`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    #[serde(default, rename = "user")]
    pub users: Vec<User>,
}

pub fn registry_path(data_dir: &Path) -> PathBuf {
    data_dir.join("hub-users.toml")
}

/// Where `server` writes the credential of `name`: `<config_dir>/hub-users/<name>.cred`.
pub fn cred_path(config_dir: &Path, name: &str) -> PathBuf {
    config_dir.join("hub-users").join(format!("{name}.cred"))
}

/// User names: like host labels, `[a-z0-9][a-z0-9_-]{0,31}`.
pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && !name.starts_with(['-', '_']);
    if ok {
        Ok(())
    } else {
        Err(other(format!(
            "hub user name {name:?}: use [a-z0-9_-], at most 32, starting with a letter or digit"
        )))
    }
}

impl Registry {
    pub fn load(sys: &dyn System, data_dir: &Path) -> Result<Registry> {
        let p = registry_path(data_dir);
        if !sys.exists(&p) {
            return Ok(Registry::default());
        }
        let text = sys.read_to_string(&p).map_err(io_at(&p))?;
        toml::from_str(&text).map_err(|e| other(format!("{}: {e}", p.display())))
    }

    pub fn save(&self, sys: &dyn System, data_dir: &Path) -> Result<()> {
        let p = registry_path(data_dir);
        let body = toml::to_string_pretty(self)
            .map_err(|e| other(format!("serialising the hub users: {e}")))?;
        let text =
            format!("# hgl-ai hub users, written by `hgl-ai server`; public keys only.\n{body}");
        sys.write(&p, text.as_bytes(), token::MODE)
            .map_err(io_at(&p))
    }

    pub fn get(&self, name: &str) -> Option<&User> {
        self.users.iter().find(|u| u.name == name)
    }

    /// Add `name` with a fresh key, or give it a fresh key (`rotate`). Returns the
    /// credential text, to be written once and never printed.
    pub fn issue(&mut self, name: &str, role: Role, rotate: bool) -> Result<String> {
        let kp = nkeys::KeyPair::new_user();
        let seed = kp
            .seed()
            .map_err(|e| other(format!("making an NKey: {e}")))?;
        self.register(name, role, &kp.public_key(), rotate)?;
        Ok(Credential::render_nkey(name, role.as_str(), &seed))
    }

    /// Add `name` by a public NKey whose seed stays on its host (a host that made its own
    /// key pair; docs/fleet.md FLEET-10), or replace its key (`rotate`). No secret is
    /// written here.
    pub fn register(&mut self, name: &str, role: Role, nkey: &str, rotate: bool) -> Result<()> {
        check_name(name)?;
        check_public_key(nkey)?;
        if let Some(u) = self.get(name)
            && !rotate
        {
            return Err(other(format!(
                "hub user {name} exists (role {}); --rotate gives it a new key and voids the old credential",
                u.role
            )));
        }
        if let Some(u) = self.users.iter().find(|u| u.nkey == nkey && u.name != name) {
            return Err(other(format!("that key is already hub user {}", u.name)));
        }
        let user = User {
            name: name.to_string(),
            role,
            nkey: nkey.to_string(),
            created_at: hgl_ai_proto::utc_now_stamp(),
        };
        self.users.retain(|u| u.name != name);
        self.users.push(user);
        self.users.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.users.len();
        self.users.retain(|u| u.name != name);
        self.users.len() != before
    }
}

/// A user's public NKey: `U...`, checksum valid.
pub fn check_public_key(k: &str) -> Result<()> {
    if k.starts_with('U') && nkeys::KeyPair::from_public_key(k).is_ok() {
        Ok(())
    } else {
        Err(other(format!("{k:?} is not a user's public NKey (U...)")))
    }
}

/// A short fingerprint of a public key, for comparing it by eye: the first 16 hex
/// digits of its SHA-256, in groups of four.
pub fn fingerprint(nkey: &str) -> String {
    use sha2::Digest as _;
    let h = hex::encode(sha2::Sha256::digest(nkey.as_bytes()));
    h.as_bytes()[..16]
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("-")
}

/// A role's permissions: publish allow and deny, subscribe allow. `None`: no limits (admin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permissions {
    pub publish_allow: Vec<String>,
    pub publish_deny: Vec<String>,
    pub subscribe_allow: Vec<String>,
}

fn kv(bucket: &str) -> String {
    format!("KV_{bucket}")
}

/// Create consumers on `stream`: by name, durable, ephemeral.
fn consumers_on(stream: &str) -> [String; 3] {
    [
        format!("$JS.API.CONSUMER.CREATE.{stream}"),
        format!("$JS.API.CONSUMER.CREATE.{stream}.>"),
        format!("$JS.API.CONSUMER.DURABLE.CREATE.{stream}.*"),
    ]
}

/// Pull from and acknowledge consumers of `stream`.
fn pull_from(stream: &str) -> [String; 2] {
    [
        format!("$JS.API.CONSUMER.MSG.NEXT.{stream}.*"),
        format!("$JS.ACK.{stream}.>"),
    ]
}

pub fn permissions(role: Role) -> Option<Permissions> {
    if role == Role::Admin {
        return None;
    }
    let slack_bucket = kv(KV_SLACK);
    if role == Role::Slack {
        return Some(Permissions {
            publish_allow: vec![
                SLACK_OWNER_FILTER.into(),
                format!("$KV.{KV_SLACK}.>"),
                format!("$JS.API.STREAM.INFO.{slack_bucket}"),
                format!("$JS.API.DIRECT.GET.{slack_bucket}.>"),
                format!("$JS.API.STREAM.MSG.GET.{slack_bucket}"),
                format!("$JS.API.STREAM.INFO.{STREAM_OWNER}"),
                format!("$JS.API.STREAM.MSG.GET.{STREAM_OWNER}"),
                format!("$JS.API.CONSUMER.CREATE.{STREAM_OWNER}.{SLACK_FEED}"),
                format!("$JS.API.CONSUMER.INFO.{STREAM_OWNER}.{SLACK_FEED}"),
                format!("$JS.API.CONSUMER.MSG.NEXT.{STREAM_OWNER}.{SLACK_FEED}"),
                format!("$JS.ACK.{STREAM_OWNER}.{SLACK_FEED}.>"),
            ],
            publish_deny: Vec::new(),
            subscribe_allow: vec!["_INBOX.>".into()],
        });
    }
    // Reads everyone needs: stream and bucket lookups, direct gets, consumer state,
    // flow control of the ordered consumers that list a bucket's keys.
    let mut allow: Vec<String> = [
        "$JS.API.INFO",
        "$JS.API.STREAM.INFO.*",
        "$JS.API.STREAM.NAMES",
        "$JS.API.STREAM.MSG.GET.*",
        "$JS.API.DIRECT.GET.>",
        "$JS.API.CONSUMER.INFO.*.*",
        "$JS.FC.>",
    ]
    .map(String::from)
    .to_vec();
    let buckets = [
        KV_STATE, KV_HOSTS, KV_TREES, KV_LOCKS, KV_ENDED, KV_THREADS, KV_METRICS, KV_USAGE,
        KV_BUILD,
    ];
    // The usage history (PROTO-24) is read by every role through short-lived consumers.
    let mut streams: Vec<String> = vec![STREAM_NOTES.to_string(), STREAM_USAGE.to_string()];
    streams.extend(buckets.iter().map(|b| kv(b)));
    if role == Role::Console {
        streams.push(STREAM_OWNER.to_string());
        streams.push(kv(KV_CONSOLE));
    }
    for s in &streams {
        allow.extend(consumers_on(s));
    }
    allow.extend(pull_from(STREAM_NOTES));
    allow.push(format!("$JS.API.CONSUMER.MSG.NEXT.{STREAM_USAGE}.*"));
    // The owner's subjects; denied to every client but the console.
    let owner = vec![
        format!("{OWNER_PREFIX}.>"),
        format!("{OWNER_NOTES_PREFIX}.>"),
        format!("$KV.{KV_THREADS}.>"),
    ];
    let mut deny = Vec::new();
    match role {
        Role::Seat | Role::Coord | Role::Builder => {
            allow.push(format!("{SUBJECT_PREFIX}.*"));
            allow.push(format!("{CONSOLE_PREFIX}.>"));
            for b in [KV_STATE, KV_HOSTS, KV_TREES, KV_LOCKS, KV_ENDED, KV_USAGE] {
                allow.push(format!("$KV.{b}.>"));
            }
            // Usage records and their history (PROTO-23).
            allow.push(format!("{USAGE_PREFIX}.>"));
            // `HGL_BUILD`: the run summaries builders and `build run` keep. The recipes,
            // which every builder runs, only the admin user writes: seats share their
            // host's credential, so no seat may (docs/build.md BUILD-1).
            allow.push(format!("$KV.{KV_BUILD}.run.>"));
            if role == Role::Coord {
                allow.push(format!("$KV.{KV_METRICS}.>"));
            }
            // The local build verdicts the merge gate reads: only a builder's user writes
            // them, from the builds it ran (BUILD-15); a seat's is refused (hgl-ai#16).
            let verdicts = format!("$KV.{KV_STATE}.{VERDICT_PREFIX}>");
            // Read `HGL_OWNER` (`status list`) through short-lived consumers, which take no
            // acknowledgement; the console's durable feed stays the console's.
            allow.push(format!("$JS.API.CONSUMER.CREATE.{STREAM_OWNER}"));
            allow.push(format!("$JS.API.CONSUMER.MSG.NEXT.{STREAM_OWNER}.*"));
            deny.extend(owner);
            if role != Role::Builder {
                deny.push(verdicts);
            }
            deny.extend(
                [
                    "$JS.API.STREAM.CREATE.>",
                    "$JS.API.STREAM.UPDATE.>",
                    "$JS.API.STREAM.DELETE.>",
                    "$JS.API.STREAM.PURGE.>",
                    "$JS.API.STREAM.MSG.DELETE.>",
                    "$JS.API.CONSUMER.DELETE.>",
                ]
                .map(String::from),
            );
            deny.extend([
                format!("$JS.API.CONSUMER.CREATE.{STREAM_OWNER}.>"),
                format!("$JS.API.CONSUMER.DURABLE.CREATE.{STREAM_OWNER}.*"),
                format!("$JS.API.CONSUMER.MSG.NEXT.{STREAM_OWNER}.console"),
                format!("$JS.API.CONSUMER.MSG.NEXT.{STREAM_OWNER}.{SLACK_FEED}"),
                format!("$JS.ACK.{STREAM_OWNER}.>"),
            ]);
            // The console's own state (sessions) is not for agents to read.
            let c = kv(KV_CONSOLE);
            deny.extend([
                format!("$KV.{KV_CONSOLE}.>"),
                format!("$JS.API.DIRECT.GET.{c}"),
                format!("$JS.API.DIRECT.GET.{c}.>"),
                format!("$JS.API.STREAM.MSG.GET.{c}"),
                format!("$JS.API.CONSUMER.CREATE.{c}"),
                format!("$JS.API.CONSUMER.CREATE.{c}.>"),
            ]);
        }
        Role::Console => {
            // `tick` notes to the controller, the owner's notes and thread events.
            allow.push(format!("{SUBJECT_PREFIX}.*"));
            allow.extend([
                format!("{OWNER_PREFIX}.thread.*"),
                format!("{OWNER_NOTES_PREFIX}.>"),
                format!("$KV.{KV_THREADS}.>"),
            ]);
            allow.extend(pull_from(STREAM_OWNER));
            allow.push(format!("$KV.{KV_STATE}.decisions.log"));
            allow.push(format!("$KV.{KV_CONSOLE}.>"));
        }
        Role::Slack | Role::Admin => unreachable!(),
    }
    deny.push(format!("$JS.API.DIRECT.GET.{slack_bucket}"));
    deny.push(format!("$JS.API.STREAM.MSG.GET.{slack_bucket}"));
    // The console can read the link index (links.<scope>.<id>), while the
    // four-token delivery and reverse mapping keys stay with the adapter.
    let private_keys = if role == Role::Console {
        "*.*.*.*"
    } else {
        ">"
    };
    deny.push(format!(
        "$JS.API.DIRECT.GET.{slack_bucket}.$KV.{KV_SLACK}.{private_keys}"
    ));
    if role != Role::Console {
        deny.push(format!("$JS.API.DIRECT.GET.{slack_bucket}.>"));
    }
    Some(Permissions {
        publish_allow: allow,
        publish_deny: deny,
        subscribe_allow: vec!["_INBOX.>".into()],
    })
}

fn conf_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("\"{s}\"")).collect();
    format!("[{}]", quoted.join(", "))
}

/// The `authorization { ... }` block of `nats.conf` for these users.
pub fn render_authorization(reg: &Registry) -> String {
    let mut s = String::from("authorization {\n");
    let used: std::collections::BTreeSet<Role> = reg.users.iter().map(|u| u.role).collect();
    let mut vars = BTreeMap::new();
    for role in used {
        let Some(p) = permissions(role) else { continue };
        let var = role.as_str().to_ascii_uppercase();
        let mut publish = format!("allow = {}", conf_list(&p.publish_allow));
        if !p.publish_deny.is_empty() {
            publish.push_str(&format!(", deny = {}", conf_list(&p.publish_deny)));
        }
        s.push_str(&format!(
            "  {var} = {{\n    publish = {{ {publish} }}\n    subscribe = {{ allow = {} }}\n  }}\n",
            conf_list(&p.subscribe_allow)
        ));
        vars.insert(role, var);
    }
    s.push_str("  users = [\n");
    for u in &reg.users {
        match vars.get(&u.role) {
            Some(v) => s.push_str(&format!(
                "    {{ nkey: \"{}\", permissions: ${v} }}  # {} ({})\n",
                u.nkey, u.name, u.role
            )),
            None => s.push_str(&format!(
                "    {{ nkey: \"{}\" }}  # {} ({})\n",
                u.nkey, u.name, u.role
            )),
        }
    }
    s.push_str("  ]\n}\n");
    s
}

/// Write a credential (mode 0600, its directory 0700).
pub fn write_cred(sys: &dyn System, path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        sys.mkdir(dir).map_err(io_at(dir))?;
        if !sys.dry_run() {
            sys.chmod(dir, 0o700).map_err(io_at(dir))?;
        }
    }
    sys.write(path, text.as_bytes(), token::MODE)
        .map_err(io_at(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Os;
    use crate::sys::fake::Fake;

    #[test]
    fn roles_and_names() {
        for r in Role::ALL {
            assert_eq!(r.as_str().parse::<Role>().unwrap(), r);
        }
        assert!("owner".parse::<Role>().is_err());
        check_name("build-1").unwrap();
        for bad in ["", "Build", "-x", "a b", &"x".repeat(33)] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn seats_never_publish_the_owners_subjects() {
        let seat = permissions(Role::Seat).unwrap();
        for s in [
            "hgl.owner.>",
            "hgl.notes.owner.>",
            "$KV.HGL_THREADS.>",
            "$JS.API.STREAM.UPDATE.>",
        ] {
            assert!(seat.publish_deny.iter().any(|d| d == s), "{s}");
            assert!(!seat.publish_allow.iter().any(|a| a == s), "{s}");
        }
        assert!(seat.publish_allow.iter().any(|a| a == "hgl.notes.*"));
        // Status reports by a short-lived consumer; never the console's feed or an ack.
        assert!(
            seat.publish_allow
                .iter()
                .any(|a| a == "$JS.API.CONSUMER.CREATE.HGL_OWNER")
        );
        for d in [
            "$JS.API.CONSUMER.CREATE.HGL_OWNER.>",
            "$JS.API.CONSUMER.DURABLE.CREATE.HGL_OWNER.*",
            "$JS.API.CONSUMER.MSG.NEXT.HGL_OWNER.console",
            "$JS.ACK.HGL_OWNER.>",
        ] {
            assert!(seat.publish_deny.iter().any(|x| x == d), "{d}");
        }
        // Builders and `build run` keep runs; a build job is a note; recipes are the
        // admin's.
        assert!(
            seat.publish_allow
                .iter()
                .any(|a| a == "$KV.HGL_BUILD.run.>")
        );
        assert!(
            !seat
                .publish_allow
                .iter()
                .any(|a| a.starts_with("$KV.HGL_BUILD.recipe"))
        );
        assert!(
            seat.publish_allow
                .iter()
                .any(|a| a == "$JS.API.CONSUMER.CREATE.KV_HGL_BUILD.>")
        );
        let coord = permissions(Role::Coord).unwrap();
        assert!(coord.publish_allow.iter().any(|a| a == "$KV.HGL_METRICS.>"));
        assert!(!seat.publish_allow.iter().any(|a| a == "$KV.HGL_METRICS.>"));
        for s in ["$KV.HGL_USAGE.>", "hgl.usage.>"] {
            assert!(seat.publish_allow.iter().any(|a| a == s), "{s}");
        }
        // The local build verdicts: a builder's user only (BUILD-15).
        let builder = permissions(Role::Builder).unwrap();
        for p in [&seat, &coord] {
            assert!(
                p.publish_deny
                    .iter()
                    .any(|d| d == "$KV.HGL_STATE.verdict.>")
            );
        }
        assert!(!builder.publish_deny.iter().any(|d| d.contains("verdict")));
        assert!(builder.publish_deny.iter().any(|d| d == "hgl.owner.>"));
        assert!(builder.publish_allow.iter().any(|a| a == "$KV.HGL_STATE.>"));
        let console = permissions(Role::Console).unwrap();
        for s in [
            "hgl.owner.thread.*",
            "hgl.notes.owner.>",
            "$KV.HGL_THREADS.>",
        ] {
            assert!(console.publish_allow.iter().any(|a| a == s), "{s}");
        }
        assert!(
            !console
                .publish_allow
                .iter()
                .any(|a| a == SLACK_OWNER_FILTER)
        );
        assert!(permissions(Role::Admin).is_none());
    }

    #[test]
    fn registry_issue_rotate_remove() {
        let d = tempfile::tempdir().unwrap();
        let f = Fake::new(d.path(), Os::Linux);
        let data = f.env.home.join(".local/share/hgl-ai");
        let mut reg = Registry::load(&f, &data).unwrap();
        let cred = reg.issue("build-1", Role::Seat, false).unwrap();
        let c = hgl_ai_bus::parse_credential(&cred).unwrap().unwrap();
        assert_eq!(c.describe(), "hub user build-1 (seat)");
        let key = reg.get("build-1").unwrap().nkey.clone();
        assert!(key.starts_with('U'));
        assert!(reg.issue("build-1", Role::Seat, false).is_err());
        reg.issue("build-1", Role::Seat, true).unwrap();
        assert_ne!(reg.get("build-1").unwrap().nkey, key, "rotated");
        reg.issue("console", Role::Console, false).unwrap();
        reg.save(&f, &data).unwrap();
        assert_eq!(f.mode(&registry_path(&data)).unwrap(), 0o600);
        let back = Registry::load(&f, &data).unwrap();
        assert_eq!(back, reg);
        let text = std::fs::read_to_string(registry_path(&data)).unwrap();
        assert!(!text.contains("seed"), "no seeds in the registry");
        assert!(reg.remove("build-1") && !reg.remove("build-1"));

        // A host's own key: only the public half is registered.
        let kp = nkeys::KeyPair::new_user();
        reg.register("host-2", Role::Seat, &kp.public_key(), false)
            .unwrap();
        assert_eq!(reg.get("host-2").unwrap().nkey, kp.public_key());
        assert!(
            reg.register("host-3", Role::Seat, &kp.public_key(), false)
                .is_err(),
            "one key, one user"
        );
        assert!(
            reg.register("host-4", Role::Seat, "UNOTAKEY", false)
                .is_err()
        );
        assert!(
            reg.register("host-4", Role::Seat, &kp.seed().unwrap(), false)
                .is_err(),
            "never a seed"
        );
        assert_eq!(fingerprint(&kp.public_key()).len(), 19);
    }

    /// The rendered block is a valid `nats-server` configuration (when one is installed).
    #[test]
    fn authorization_parses_with_nats_server() {
        let mut reg = Registry::default();
        for (n, r) in [
            ("hub", Role::Seat),
            ("hub-coord", Role::Coord),
            ("console", Role::Console),
            ("hub-admin", Role::Admin),
        ] {
            reg.issue(n, r, false).unwrap();
        }
        let block = render_authorization(&reg);
        assert!(
            block.contains("permissions: $SEAT }  # hub (seat)"),
            "{block}"
        );
        assert!(block.contains("# hub-admin (admin)"));
        let Some(bin) =
            crate::sys::which_in(&std::env::var("PATH").unwrap_or_default(), "nats-server")
        else {
            return;
        };
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("nats.conf");
        let conf = crate::nats::render_conf(
            "127.0.0.1",
            4222,
            &d.path().join("store"),
            &crate::nats::Auth::Users(&reg),
        );
        std::fs::write(&p, conf).unwrap();
        let o = std::process::Command::new(bin)
            .arg("-t")
            .arg("-c")
            .arg(&p)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
}
