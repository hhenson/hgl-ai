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
    VIEWER_INBOX, VIEWER_STATE_KEYS,
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
    /// The console's page in a browser, over WebSocket (docs/owner.md §3): reads and
    /// watches [`VIEWER_BUCKETS`] and nothing else. Its subject permissions are read-only,
    /// but replies are not enforced until hgl-ai#141: with one NATS account, JetStream
    /// replies let any role write any subject.
    Viewer,
    /// Slack Q&A: owner actions, its own feed and delivery state.
    Slack,
    /// Everything: provisioning the stream and buckets (`hgl-ai init`, `server install`).
    Admin,
}

impl Role {
    pub const ALL: [Role; 7] = [
        Role::Seat,
        Role::Coord,
        Role::Builder,
        Role::Console,
        Role::Viewer,
        Role::Slack,
        Role::Admin,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Seat => "seat",
            Role::Coord => "coord",
            Role::Builder => "builder",
            Role::Console => "console",
            Role::Viewer => "viewer",
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
                format!(
                    "unknown hub role {s:?}: seat, coord, builder, console, viewer, slack or admin"
                )
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

/// A role's publish permissions, allow and deny. `None`: no limits (admin). What a user
/// may subscribe to is its own: [`subscribe_allow`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permissions {
    pub publish_allow: Vec<String>,
    pub publish_deny: Vec<String>,
}

/// What the user with public key `nkey` may subscribe to: its own inbox prefix
/// ([`hgl_ai_proto::user_inbox`]; the viewer's is [`VIEWER_INBOX`], which the console's
/// page uses), never `_INBOX.>`, where every client's JetStream and KV replies would be
/// (the console's sessions, threads, notes; hgl-ai#111).
pub fn subscribe_allow(role: Role, nkey: &str) -> Vec<String> {
    let inbox = if role == Role::Viewer {
        VIEWER_INBOX.to_string()
    } else {
        hgl_ai_proto::user_inbox(nkey)
    };
    vec![format!("{inbox}.>")]
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

/// The buckets the viewer reads: the telemetry (docs/owner.md §3).
pub const VIEWER_BUCKETS: [&str; 6] =
    [KV_HOSTS, KV_TREES, KV_STATE, KV_METRICS, KV_BUILD, KV_USAGE];

/// The viewer's publishes: what reading and watching [`VIEWER_BUCKETS`] needs, on those
/// buckets' streams only, and of `HGL_STATE` only [`VIEWER_STATE_KEYS`]: bucket status,
/// direct gets, the ordered push consumer a watch makes and its flow control. A consumer
/// only in the filtered create form, `CONSUMER.CREATE.<stream>.<name>.<filter>`, whose
/// filter the server holds the body to (one filter, the subject's); never the bare or
/// durable form. What subjects cannot stop (a `durable_name` in the body, any
/// `deliver_subject`, an `inactive_threshold`) `doctor` reports (docs/design.md §9). Subscribes
/// to [`VIEWER_INBOX`] only ([`subscribe_allow`]): no other client's replies.
fn viewer() -> Permissions {
    let mut allow = vec!["$JS.API.INFO".to_string(), "$JS.FC.>".to_string()];
    let mut deny = Vec::new();
    for b in VIEWER_BUCKETS {
        let s = kv(b);
        allow.extend([
            format!("$JS.API.STREAM.INFO.{s}"),
            format!("$JS.API.CONSUMER.INFO.{s}.*"),
        ]);
        if b == KV_STATE {
            for k in VIEWER_STATE_KEYS {
                allow.extend([
                    format!("$JS.API.DIRECT.GET.{s}.$KV.{b}.{k}"),
                    format!("$JS.API.CONSUMER.CREATE.{s}.*.$KV.{b}.{k}"),
                ]);
            }
        } else {
            allow.extend([
                format!("$JS.API.DIRECT.GET.{s}"),
                format!("$JS.API.DIRECT.GET.{s}.>"),
                format!("$JS.API.STREAM.MSG.GET.{s}"),
                format!("$JS.API.CONSUMER.CREATE.{s}.*.$KV.{b}.>"),
            ]);
        }
        deny.extend([
            format!("$JS.API.CONSUMER.CREATE.{s}"),
            format!("$JS.API.CONSUMER.CREATE.{s}.*"),
            format!("$JS.API.CONSUMER.DURABLE.CREATE.{s}.>"),
        ]);
    }
    Permissions {
        publish_allow: allow,
        publish_deny: deny,
    }
}

pub fn permissions(role: Role) -> Option<Permissions> {
    match role {
        Role::Admin => return None,
        Role::Viewer => return Some(viewer()),
        _ => {}
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
    // `doctor` lists the consumers on the telemetry streams (durables the viewer made).
    allow.extend(VIEWER_BUCKETS.map(|b| format!("$JS.API.CONSUMER.LIST.{}", kv(b))));
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
        Role::Viewer | Role::Slack | Role::Admin => unreachable!(),
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
    })
}

fn conf_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("\"{s}\"")).collect();
    format!("[{}]", quoted.join(", "))
}

/// The `authorization { ... }` block of `nats.conf` for these users: a publish block per
/// role (`SEAT_PUBLISH`), and per user that and its own inbox to subscribe to.
pub fn render_authorization(reg: &Registry) -> String {
    let mut s = String::from("authorization {\n");
    let used: std::collections::BTreeSet<Role> = reg.users.iter().map(|u| u.role).collect();
    let mut vars = BTreeMap::new();
    for role in used {
        let Some(p) = permissions(role) else { continue };
        let var = format!("{}_PUBLISH", role.as_str().to_ascii_uppercase());
        let mut publish = format!("allow = {}", conf_list(&p.publish_allow));
        if !p.publish_deny.is_empty() {
            publish.push_str(&format!(", deny = {}", conf_list(&p.publish_deny)));
        }
        s.push_str(&format!("  {var} = {{ {publish} }}\n"));
        vars.insert(role, var);
    }
    s.push_str("  users = [\n");
    for u in &reg.users {
        match vars.get(&u.role) {
            Some(v) => s.push_str(&format!(
                "    {{ nkey: \"{}\", permissions: {{ publish: ${v}, subscribe: {{ allow: {} }} }} }}  # {} ({})\n",
                u.nkey,
                conf_list(&subscribe_allow(u.role, &u.nkey)),
                u.name,
                u.role
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

// ---- One account per role (hgl-ai#141) ----------------------------------------------
//
// The broker checks a publish on its subject only, and JetStream writes its replies
// inside its account unchecked, so no permission inside one account is a boundary
// between roles. On this layout `HUB` holds every stream and bucket and only the admin
// user; each other role has an account without JetStream whose only way into `HUB` is
// the services `HUB` exports to it: a reply to a role's request goes back to the role's
// account, never into `HUB`. The exports come from one table, [`access_table`].

/// The account that holds the streams and buckets, and the admin user.
pub const HUB_ACCOUNT: &str = "HUB";

/// The local build verdicts' own bucket (hgl-ai#145): an export cannot be "`HGL_STATE`
/// but `verdict.*`".
pub const KV_VERDICTS: &str = "HGL_VERDICTS";

/// The console's durable `HGL_OWNER` feed (`hgl_ai_owner::FEED`).
pub const CONSOLE_FEED: &str = "console";

/// The roles with an account of their own: the access table's columns.
pub const ACCOUNT_ROLES: [Role; 5] = [
    Role::Seat,
    Role::Coord,
    Role::Builder,
    Role::Console,
    Role::Slack,
];

impl Role {
    /// The role's account on the account layout.
    pub fn account(self) -> &'static str {
        match self {
            Role::Seat => "SEAT",
            Role::Coord => "COORD",
            Role::Builder => "BUILDER",
            Role::Console => "CONSOLE",
            Role::Slack => "SLACK",
            // Not rendered yet: the viewer's account and imports are hgl-ai#148.
            Role::Viewer => "VIEWER",
            Role::Admin => HUB_ACCOUNT,
        }
    }
}

/// What a role may do with a [`Target`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    No,
    R,
    W,
    RW,
}

impl Access {
    pub fn reads(self) -> bool {
        matches!(self, Access::R | Access::RW)
    }
    pub fn writes(self) -> bool {
        matches!(self, Access::W | Access::RW)
    }
}

/// A thing in `HUB` a row of the access table is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `$JS.API.INFO`: the hub's JetStream use and limits.
    AccountInfo,
    /// The keys of `bucket` matching `keys` (`>`: all). Read: get by key (a direct get
    /// by subject), and, for all keys, the stream's state, by sequence and through pull
    /// consumers. Write: put, create, update, delete.
    Keys { bucket: String, keys: String },
    /// A stream read through any pull consumer, with acknowledgement, and by sequence.
    Stream(String),
    /// A stream read through the one durable consumer `consumer` only.
    Feed { stream: String, consumer: String },
    /// A stream read by sequence only, through no consumer.
    BySequence(String),
    /// Publishing on `subject` (with wildcards), into whichever stream holds it.
    Subjects(String),
}

/// One row: a target and each role's access to it, in [`ACCOUNT_ROLES`] order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub target: Target,
    pub access: [Access; 5],
}

impl Row {
    pub fn of(&self, role: Role) -> Access {
        ACCOUNT_ROLES
            .iter()
            .position(|r| *r == role)
            .map_or(Access::No, |i| self.access[i])
    }
}

/// The access map of hgl-ai#141: role → reads and writes. Stream create, update, delete
/// and purge, and consumer deletion, are the admin's, who is in `HUB` itself.
pub fn access_table() -> Vec<Row> {
    use Access::{No, R, RW, W};
    let keys = |b: &str, k: &str| Target::Keys {
        bucket: b.into(),
        keys: k.into(),
    };
    let all = |b: &str| keys(b, ">");
    let subj = |s: String| Target::Subjects(s);
    let row = |target: Target, access: [Access; 5]| Row { target, access };
    // Columns: seat, coord, builder, console, slack.
    vec![
        row(Target::AccountInfo, [R, R, R, R, R]),
        // Notes: seats, the coordinator and builders send them; the console its `tick`.
        row(subj(format!("{SUBJECT_PREFIX}.*")), [W, W, W, W, No]),
        row(subj(format!("{OWNER_NOTES_PREFIX}.*")), [No, No, No, W, No]),
        row(Target::Stream(STREAM_NOTES.into()), [R, R, R, R, No]),
        // Agents to the owner; the owner's thread events (Slack only its own).
        row(subj(format!("{CONSOLE_PREFIX}.>")), [W, W, W, No, No]),
        row(
            subj(format!("{OWNER_PREFIX}.thread.*")),
            [No, No, No, W, No],
        ),
        row(subj(SLACK_OWNER_FILTER.into()), [No, No, No, No, W]),
        row(Target::BySequence(STREAM_OWNER.into()), [R, R, R, R, R]),
        row(
            Target::Feed {
                stream: STREAM_OWNER.into(),
                consumer: CONSOLE_FEED.into(),
            },
            [No, No, No, R, No],
        ),
        row(
            Target::Feed {
                stream: STREAM_OWNER.into(),
                consumer: SLACK_FEED.into(),
            },
            [No, No, No, No, R],
        ),
        row(all(KV_THREADS), [R, R, R, RW, No]),
        row(all(KV_STATE), [RW, RW, RW, R, No]),
        row(keys(KV_STATE, "decisions.log"), [No, No, No, W, No]),
        // The local build verdicts the merge gate reads (BUILD-15).
        row(all(KV_VERDICTS), [R, R, RW, R, No]),
        row(all(KV_HOSTS), [RW, RW, RW, R, No]),
        row(all(KV_TREES), [RW, RW, RW, R, No]),
        row(all(KV_LOCKS), [RW, RW, RW, R, No]),
        row(all(KV_ENDED), [RW, RW, RW, R, No]),
        // Usage records and their history (PROTO-23, PROTO-24).
        row(all(KV_USAGE), [RW, RW, RW, R, No]),
        row(subj(format!("{USAGE_PREFIX}.>")), [W, W, W, No, No]),
        row(Target::Stream(STREAM_USAGE.into()), [R, R, R, R, No]),
        // Build runs; the recipes every builder runs are the admin's (BUILD-1).
        row(all(KV_BUILD), [R, R, R, R, No]),
        row(keys(KV_BUILD, "run.>"), [W, W, W, No, No]),
        row(all(KV_METRICS), [R, RW, R, R, No]),
        row(all(KV_CONSOLE), [No, No, No, RW, No]),
        // The console reads the permalink index; delivery state is the adapter's.
        row(keys(KV_SLACK, "links.*.*"), [No, No, No, R, No]),
        row(all(KV_SLACK), [No, No, No, No, RW]),
    ]
}

/// A service `HUB` exports: requests on `subject`, answered by one message or by several
/// (`stream`: a pull's batch, a direct get's).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Service {
    pub subject: String,
    pub stream: bool,
}

fn api(s: &str) -> Service {
    Service {
        subject: format!("$JS.API.{s}"),
        stream: false,
    }
}

fn api_stream(s: &str) -> Service {
    Service {
        stream: true,
        ..api(s)
    }
}

fn plain(s: String) -> Service {
    Service {
        subject: s,
        stream: false,
    }
}

impl Target {
    /// The services `access` to this target takes.
    pub fn services(&self, access: Access) -> Vec<Service> {
        let mut out = Vec::new();
        if access.reads() {
            match self {
                Target::AccountInfo => out.push(api("INFO")),
                Target::Keys { bucket, keys } => {
                    let s = kv(bucket);
                    out.push(api_stream(&format!("DIRECT.GET.{s}.$KV.{bucket}.{keys}")));
                    // The stream's state (counts, and key names by subject filter) only
                    // with every key: some keys are read by subject alone.
                    if keys == ">" {
                        out.push(api(&format!("STREAM.INFO.{s}")));
                        out.push(api_stream(&format!("DIRECT.GET.{s}")));
                        out.push(api(&format!("STREAM.MSG.GET.{s}")));
                        out.extend(consumers(&s));
                    }
                }
                Target::Stream(s) => {
                    out.push(api(&format!("STREAM.INFO.{s}")));
                    out.push(api(&format!("STREAM.MSG.GET.{s}")));
                    out.extend(consumers(s));
                    out.push(api(&format!("CONSUMER.DURABLE.CREATE.{s}.*")));
                    out.push(plain(format!("$JS.ACK.{s}.>")));
                }
                Target::Feed { stream, consumer } => {
                    let (s, c) = (stream, consumer);
                    out.push(api(&format!("STREAM.INFO.{s}")));
                    out.push(api(&format!("CONSUMER.CREATE.{s}.{c}")));
                    out.push(api(&format!("CONSUMER.CREATE.{s}.{c}.>")));
                    out.push(api(&format!("CONSUMER.DURABLE.CREATE.{s}.{c}")));
                    out.push(api(&format!("CONSUMER.INFO.{s}.{c}")));
                    out.push(api_stream(&format!("CONSUMER.MSG.NEXT.{s}.{c}")));
                    out.push(plain(format!("$JS.ACK.{s}.{c}.>")));
                }
                Target::BySequence(s) => {
                    out.push(api(&format!("STREAM.INFO.{s}")));
                    out.push(api(&format!("STREAM.MSG.GET.{s}")));
                }
                Target::Subjects(_) => {}
            }
        }
        if access.writes() {
            match self {
                Target::Keys { bucket, keys } => out.push(plain(format!("$KV.{bucket}.{keys}"))),
                Target::Subjects(s) => out.push(plain(s.clone())),
                _ => {}
            }
        }
        out
    }
}

/// Pull consumers on stream `s`: create (ephemeral, or by name with filters), state, pull,
/// and the list `doctor` reads for consumers that outlive a page (`telemetry consumers`,
/// hgl-ai#105). Not on `HGL_SLACK`, whose consumers' names are the adapter's.
fn consumers(s: &str) -> Vec<Service> {
    let mut out = vec![
        api(&format!("CONSUMER.CREATE.{s}")),
        api(&format!("CONSUMER.CREATE.{s}.>")),
        api(&format!("CONSUMER.INFO.{s}.*")),
        api_stream(&format!("CONSUMER.MSG.NEXT.{s}.*")),
    ];
    if s != kv(KV_SLACK) {
        out.push(api(&format!("CONSUMER.LIST.{s}")));
    }
    out
}

/// The subject a role's account imports `hub_subject` as: the JetStream API and bucket
/// writes under [`hgl_ai_proto::HUB_API_PREFIX`], everything else by its own name.
pub fn local_subject(hub_subject: &str) -> String {
    let p = hgl_ai_proto::HUB_API_PREFIX;
    if let Some(rest) = hub_subject.strip_prefix("$JS.API.") {
        format!("{p}.{rest}")
    } else if hub_subject.starts_with("$KV.") {
        format!("{p}.{hub_subject}")
    } else {
        hub_subject.to_string()
    }
}

/// The services `role` may use, from the access table, each once.
pub fn role_services(role: Role) -> Vec<Service> {
    let mut out: std::collections::BTreeSet<Service> = Default::default();
    for row in access_table() {
        out.extend(row.target.services(row.of(role)));
    }
    out.into_iter().collect()
}

/// What `HUB` exports to the role accounts: each service once, with the accounts it is
/// for. Every service names at least one account (an export naming none would be open to
/// every account).
pub fn exports() -> BTreeMap<Service, Vec<Role>> {
    let mut out: BTreeMap<Service, Vec<Role>> = BTreeMap::new();
    for role in ACCOUNT_ROLES {
        for s in role_services(role) {
            out.entry(s).or_default().push(role);
        }
    }
    out
}

/// `nats.conf` for one account per role (hgl-ai#141), in place of the `authorization`
/// block: `HUB` with the admin users, JetStream and its service exports, and one account
/// per role with its imports. Every account, export and import is there whether or not
/// the role has users yet, so the first user of a role needs only the reload that adds
/// it (a reload that added the account left its imports unanswered: r1 on hgl-ai#152).
/// Each user publishes only on what its account imports, and subscribes to its own inbox
/// only (hgl-ai#111). Viewer users are not rendered: their account is hgl-ai#148.
pub fn render_accounts(reg: &Registry) -> String {
    let used: std::collections::BTreeSet<Role> = reg.users.iter().map(|u| u.role).collect();
    let user_line = |u: &User, perms: Option<&str>| match perms {
        Some(v) => format!(
            "      {{ nkey: \"{}\", permissions: {{ publish: ${v}, subscribe: {{ allow: {} }} }} }}  # {} ({})\n",
            u.nkey,
            conf_list(&subscribe_allow(u.role, &u.nkey)),
            u.name,
            u.role
        ),
        None => format!(
            "      {{ nkey: \"{}\" }}  # {} ({})\n",
            u.nkey, u.name, u.role
        ),
    };
    let mut s = String::new();
    for role in ACCOUNT_ROLES.iter().filter(|r| used.contains(r)) {
        let local: Vec<String> = role_services(*role)
            .iter()
            .map(|x| local_subject(&x.subject))
            .collect();
        s.push_str(&format!(
            "{}_PUBLISH = {{ allow = {} }}\n",
            role.account(),
            conf_list(&local)
        ));
    }
    s.push_str(&format!(
        "accounts {{\n  {HUB_ACCOUNT} {{\n    jetstream: enabled\n    users: [\n"
    ));
    for u in reg.users.iter().filter(|u| u.role == Role::Admin) {
        s.push_str(&user_line(u, None));
    }
    s.push_str("    ]\n    exports: [\n");
    for (svc, roles) in exports() {
        let accounts: Vec<&str> = roles.iter().map(|r| r.account()).collect();
        s.push_str(&format!(
            "      {{ service: \"{}\", accounts: [{}]{} }}\n",
            svc.subject,
            accounts.join(", "),
            if svc.stream {
                ", response_type: stream"
            } else {
                ""
            }
        ));
    }
    s.push_str("    ]\n  }\n");
    for role in ACCOUNT_ROLES.iter() {
        let var = format!("{}_PUBLISH", role.account());
        s.push_str(&format!("  {} {{\n    users: [\n", role.account()));
        for u in reg.users.iter().filter(|u| u.role == *role) {
            s.push_str(&user_line(u, Some(&var)));
        }
        s.push_str("    ]\n    imports: [\n");
        for svc in role_services(*role) {
            let to = local_subject(&svc.subject);
            let to = if to == svc.subject {
                String::new()
            } else {
                format!(", to: \"{to}\"")
            };
            s.push_str(&format!(
                "      {{ service: {{ account: {HUB_ACCOUNT}, subject: \"{}\" }}{to} }}\n",
                svc.subject
            ));
        }
        s.push_str("    ]\n  }\n");
    }
    s.push_str("}\n");
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
    fn the_viewer_reads_the_page_keys_through_its_own_inbox() {
        let p = permissions(Role::Viewer).unwrap();
        assert_eq!(subscribe_allow(Role::Viewer, "UX"), ["_INBOX_VIEWER.>"]);
        assert_eq!(subscribe_allow(Role::Seat, "UX"), ["_INBOX.UX.>"]);
        let allow = |s: &str| p.publish_allow.iter().any(|a| a == s);
        // `HGL_STATE`: the page's keys by subject, never a request naming one in its body.
        assert!(allow(
            "$JS.API.DIRECT.GET.KV_HGL_STATE.$KV.HGL_STATE.landed.log"
        ));
        assert!(allow(
            "$JS.API.CONSUMER.CREATE.KV_HGL_STATE.*.$KV.HGL_STATE.prs"
        ));
        assert!(!allow("$JS.API.DIRECT.GET.KV_HGL_STATE"));
        assert!(!allow("$JS.API.STREAM.MSG.GET.KV_HGL_STATE"));
        assert!(
            p.publish_allow
                .iter()
                .filter(|a| a.contains("$KV.HGL_STATE."))
                .all(|a| VIEWER_STATE_KEYS
                    .iter()
                    .any(|k| a.ends_with(&format!(".{k}")))),
            "{:?}",
            p.publish_allow
        );
        // Consumers only in the filtered form; the bare and durable forms denied outright.
        assert!(
            p.publish_allow
                .iter()
                .filter(|a| a.contains("CONSUMER.CREATE") || a.contains("DURABLE"))
                .all(|a| a.contains(".*.$KV.")),
            "{:?}",
            p.publish_allow
        );
        for b in VIEWER_BUCKETS {
            for d in [
                format!("$JS.API.CONSUMER.CREATE.KV_{b}"),
                format!("$JS.API.CONSUMER.CREATE.KV_{b}.*"),
                format!("$JS.API.CONSUMER.DURABLE.CREATE.KV_{b}.>"),
            ] {
                assert!(p.publish_deny.contains(&d), "{d}");
            }
        }
    }

    /// The page (`crates/console/web`) uses the same inbox prefix and `HGL_STATE` keys.
    #[test]
    fn the_page_matches_the_viewer() {
        let hub = include_str!("../../console/web/src/hub.ts");
        assert!(
            hub.contains(&format!("INBOX_PREFIX = \"{VIEWER_INBOX}\"")),
            "hub.ts"
        );
        let tables = include_str!("../../console/web/src/tables.ts");
        let keys: Vec<String> = VIEWER_STATE_KEYS
            .iter()
            .map(|k| format!("\"{k}\""))
            .collect();
        assert!(
            tables.contains(&format!("STATE_KEYS = [{}] as const", keys.join(", "))),
            "tables.ts"
        );
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
        let hub = &reg.get("hub").unwrap().nkey;
        assert!(
            block.contains(&format!(
                "permissions: {{ publish: $SEAT_PUBLISH, subscribe: {{ allow: [\"_INBOX.{hub}.>\"] }} }} }}  # hub (seat)"
            )),
            "{block}"
        );
        // Every user its own inbox; none `_INBOX.>` (hgl-ai#111).
        assert!(!block.contains("\"_INBOX.>\""), "{block}");
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

    fn every_role() -> Registry {
        let mut reg = Registry::default();
        for (n, r) in [
            ("hub", Role::Seat),
            ("lab2", Role::Seat),
            ("hub-coord", Role::Coord),
            ("lab1-builder", Role::Builder),
            ("console", Role::Console),
            ("hub-slack", Role::Slack),
            ("hub-admin", Role::Admin),
        ] {
            reg.issue(n, r, false).unwrap();
        }
        reg
    }

    /// The exports (subject, accounts, rest of the line) and each account's imports
    /// (subject, local subject) of a rendered account layout, read back from its text.
    #[allow(clippy::type_complexity)]
    fn read_back(
        conf: &str,
    ) -> (
        Vec<(String, Vec<String>, String)>,
        BTreeMap<String, Vec<(String, String)>>,
    ) {
        let quoted = |l: &str, key: &str| {
            let rest = l.split_once(&format!("{key} \"")).map(|x| x.1)?;
            Some(rest.split_once('"').unwrap().0.to_string())
        };
        let mut exports = Vec::new();
        let mut imports: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        let mut account = String::new();
        for l in conf.lines() {
            let t = l.trim();
            if l.starts_with("  ") && !l.starts_with("   ") && t.ends_with('{') {
                account = t.trim_end_matches('{').trim().to_string();
            } else if t.contains("{ stream:") || t.contains("stream: {") {
                panic!("a stream export or import: {l}");
            } else if t.starts_with("{ service: \"") {
                let subject = quoted(t, "service:").unwrap();
                let (_, rest) = t.split_once("accounts: [").expect(t);
                let (accs, tail) = rest.split_once(']').unwrap();
                let accs: Vec<String> = accs.split(", ").map(String::from).collect();
                exports.push((subject, accs, tail.to_string()));
            } else if t.starts_with("{ service: { account: ") {
                assert!(t.starts_with(&format!("{{ service: {{ account: {HUB_ACCOUNT},")));
                let subject = quoted(t, "subject:").unwrap();
                let to = quoted(t, "to:").unwrap_or_else(|| subject.clone());
                imports
                    .entry(account.clone())
                    .or_default()
                    .push((subject, to));
            }
        }
        (exports, imports)
    }

    /// hgl-ai#141: `HUB` exports services only, each to the accounts whose role's row has
    /// it, and every import is exactly one of those exports.
    #[test]
    fn accounts_export_services_from_the_table_only() {
        let reg = every_role();
        let conf = render_accounts(&reg);
        let (exports, imports) = read_back(&conf);
        assert!(!exports.is_empty());
        let by_account: BTreeMap<&str, Role> =
            ACCOUNT_ROLES.iter().map(|r| (r.account(), *r)).collect();
        let mut seen = std::collections::BTreeSet::new();
        for (subject, accs, tail) in &exports {
            assert!(seen.insert(subject.clone()), "exported twice: {subject}");
            assert!(
                !accs.is_empty() && !accs[0].is_empty(),
                "open export {subject}"
            );
            for a in accs {
                let role = by_account[a.as_str()];
                let svc = role_services(role);
                let s = svc.iter().find(|s| &s.subject == subject);
                let s = s.unwrap_or_else(|| panic!("{subject} to {a}: not in its row"));
                assert_eq!(
                    tail.contains("response_type: stream"),
                    s.stream,
                    "{subject}"
                );
            }
            // Never an admin's operation, never all of the API.
            for admin in [
                ".STREAM.CREATE",
                ".STREAM.UPDATE",
                ".STREAM.DELETE",
                ".STREAM.PURGE",
                ".STREAM.MSG.DELETE",
                ".CONSUMER.DELETE",
            ] {
                assert!(!subject.contains(admin), "{subject}");
            }
            assert!(
                !["$JS.API.>", "$JS.>", ">", "$KV.>", "hgl.>"].contains(&subject.as_str()),
                "{subject}"
            );
        }
        assert_eq!(imports.len(), ACCOUNT_ROLES.len(), "{conf}");
        for (account, list) in &imports {
            let role = by_account[account.as_str()];
            let want: Vec<(String, String)> = role_services(role)
                .into_iter()
                .map(|s| (local_subject(&s.subject), s.subject))
                .map(|(l, s)| (s, l))
                .collect();
            assert_eq!(list, &want, "{account}");
            for (subject, local) in list {
                let e = exports.iter().find(|e| &e.0 == subject).unwrap();
                assert!(e.1.contains(account), "{account} imports {subject}");
                // The API and bucket writes under the prefix; data and acks as they are.
                if subject.starts_with("$JS.API.") || subject.starts_with("$KV.") {
                    assert!(local.starts_with("JS.HUB.API."), "{local}");
                } else {
                    assert_eq!(local, subject);
                    assert!(subject.starts_with("hgl.") || subject.starts_with("$JS.ACK."));
                }
            }
            // A user publishes on its account's imports and nothing else.
            let var = format!("{account}_PUBLISH = {{ allow = ");
            let line = conf.lines().find(|l| l.starts_with(&var)).unwrap();
            let locals: Vec<String> = list.iter().map(|x| x.1.clone()).collect();
            assert_eq!(line, format!("{var}{} }}", conf_list(&locals)));
        }
        // The admin alone in `HUB`, every other user in its role's account, on its own
        // inbox (hgl-ai#111).
        let hub = conf.split("\n  SEAT {").next().unwrap();
        let admin = &reg.get("hub-admin").unwrap().nkey;
        assert!(hub.contains(&format!("{{ nkey: \"{admin}\" }}  # hub-admin (admin)")));
        assert_eq!(hub.matches("nkey:").count(), 1, "{hub}");
        for u in reg.users.iter().filter(|u| u.role != Role::Admin) {
            assert!(conf.contains(&format!(
                "{{ nkey: \"{k}\", permissions: {{ publish: ${a}_PUBLISH, subscribe: {{ allow: [\"_INBOX.{k}.>\"] }} }} }}  # {n} ({r})",
                k = u.nkey,
                a = u.role.account(),
                n = u.name,
                r = u.role,
            )));
        }
        assert!(!conf.contains("\"_INBOX.>\""));
        assert!(!conf.contains("authorization"));
    }

    /// The rows of hgl-ai#141 that are boundaries, as services.
    #[test]
    fn the_table_keeps_the_boundaries() {
        for row in access_table() {
            for a in row.access {
                match row.target {
                    Target::Subjects(_) => assert!(!a.reads(), "{row:?}"),
                    Target::Keys { .. } => {}
                    _ => assert!(!a.writes(), "{row:?}"),
                }
            }
        }
        let has = |r: Role, s: &str| role_services(r).iter().any(|x| x.subject == s);
        let writes = |r: Role| -> Vec<String> {
            role_services(r)
                .into_iter()
                .map(|s| s.subject)
                .filter(|s| !s.starts_with("$JS."))
                .collect()
        };
        assert_eq!(
            writes(Role::Seat),
            [
                "$KV.HGL_BUILD.run.>",
                "$KV.HGL_ENDED.>",
                "$KV.HGL_HOSTS.>",
                "$KV.HGL_LOCKS.>",
                "$KV.HGL_STATE.>",
                "$KV.HGL_TREES.>",
                "$KV.HGL_USAGE.>",
                "hgl.console.>",
                "hgl.notes.*",
                "hgl.usage.>",
            ]
        );
        let mut coord = writes(Role::Seat);
        coord.insert(4, "$KV.HGL_METRICS.>".into());
        assert_eq!(writes(Role::Coord), coord);
        let mut builder = writes(Role::Seat);
        builder.insert(7, "$KV.HGL_VERDICTS.>".into());
        assert_eq!(writes(Role::Builder), builder);
        assert_eq!(
            writes(Role::Console),
            [
                "$KV.HGL_CONSOLE.>",
                "$KV.HGL_STATE.decisions.log",
                "$KV.HGL_THREADS.>",
                "hgl.notes.*",
                "hgl.notes.owner.*",
                "hgl.owner.thread.*",
            ]
        );
        assert_eq!(writes(Role::Slack), ["$KV.HGL_SLACK.>", SLACK_OWNER_FILTER]);
        // Seats read `HGL_OWNER` by sequence: no consumer, no feed, no ack.
        for r in [Role::Seat, Role::Coord, Role::Builder] {
            assert!(has(r, "$JS.API.STREAM.MSG.GET.HGL_OWNER"));
            assert!(
                !role_services(r)
                    .iter()
                    .any(
                        |s| s.subject.contains("HGL_OWNER") && s.subject.contains("CONSUMER")
                            || s.subject.starts_with("$JS.ACK.HGL_OWNER")
                    ),
                "{r}"
            );
            assert!(
                !role_services(r)
                    .iter()
                    .any(|s| s.subject.contains("HGL_CONSOLE") || s.subject.contains("HGL_SLACK"))
            );
        }
        // Each feed is its owner's.
        assert!(has(
            Role::Console,
            "$JS.API.CONSUMER.MSG.NEXT.HGL_OWNER.console"
        ));
        assert!(!has(
            Role::Console,
            "$JS.API.CONSUMER.MSG.NEXT.HGL_OWNER.slack"
        ));
        assert!(has(
            Role::Slack,
            "$JS.API.CONSUMER.MSG.NEXT.HGL_OWNER.slack"
        ));
        assert!(!has(
            Role::Slack,
            "$JS.API.CONSUMER.MSG.NEXT.HGL_OWNER.console"
        ));
        // The console reads the link index by key, never the adapter's delivery state.
        assert!(has(
            Role::Console,
            "$JS.API.DIRECT.GET.KV_HGL_SLACK.$KV.HGL_SLACK.links.*.*"
        ));
        // Not even the stream's state: its counts and, by subject filter, its key names.
        let slack: Vec<_> = role_services(Role::Console)
            .into_iter()
            .filter(|s| s.subject.contains("HGL_SLACK"))
            .map(|s| s.subject)
            .collect();
        assert_eq!(
            slack,
            ["$JS.API.DIRECT.GET.KV_HGL_SLACK.$KV.HGL_SLACK.links.*.*"]
        );
        assert_eq!(CONSOLE_FEED, hgl_ai_owner::FEED);
        // Consumer lists only where consumer state is read, never of `HGL_OWNER`,
        // `HGL_SLACK`, or, but for the console, `HGL_CONSOLE`; every role that reads the
        // telemetry lists its consumers, as `doctor` does (hgl-ai#105).
        for r in ACCOUNT_ROLES {
            let svcs = role_services(r);
            for s in svcs.iter().filter(|s| s.subject.contains("CONSUMER.LIST.")) {
                let stream = s.subject.rsplit('.').next().unwrap();
                assert!(
                    has(r, &format!("$JS.API.CONSUMER.INFO.{stream}.*")),
                    "{r}: {s:?}"
                );
                assert!(
                    stream != "HGL_OWNER" && stream != "KV_HGL_SLACK",
                    "{r}: {s:?}"
                );
                assert!(
                    stream != "KV_HGL_CONSOLE" || r == Role::Console,
                    "{r}: {s:?}"
                );
            }
            if r != Role::Slack {
                for b in VIEWER_BUCKETS {
                    assert!(has(r, &format!("$JS.API.CONSUMER.LIST.KV_{b}")), "{r}: {b}");
                }
            }
        }
    }

    /// Every role has its account, imports and exports, with users or without.
    #[test]
    fn accounts_of_every_role_with_or_without_users() {
        let full = render_accounts(&every_role());
        let mut reg = Registry::default();
        reg.issue("hub", Role::Seat, false).unwrap();
        let conf = render_accounts(&reg);
        // The same accounts, exports and imports: the first user of a role is one reload.
        assert_eq!(read_back(&conf), read_back(&full));
        let (_, imports) = read_back(&conf);
        let mut want: Vec<&str> = ACCOUNT_ROLES.iter().map(|r| r.account()).collect();
        want.sort();
        assert_eq!(imports.keys().collect::<Vec<_>>(), want);
        assert!(conf.contains("  HUB {\n    jetstream: enabled\n    users: [\n    ]\n"));
        assert!(conf.contains("  COORD {\n    users: [\n    ]\n    imports: [\n"));
        // A publish block only for the roles in use (`nats-server -t` warns of the others).
        assert!(conf.contains("SEAT_PUBLISH = {"));
        for r in [Role::Coord, Role::Builder, Role::Console, Role::Slack] {
            assert!(!conf.contains(&format!("{}_PUBLISH", r.account())), "{r}");
        }
    }

    /// The account layout is a valid `nats-server` configuration (when one is installed).
    #[test]
    fn accounts_parse_with_nats_server() {
        let Some(bin) =
            crate::sys::which_in(&std::env::var("PATH").unwrap_or_default(), "nats-server")
        else {
            return;
        };
        let mut one = Registry::default();
        one.issue("hub", Role::Seat, false).unwrap();
        // Every role, one role, and no user yet: each without a warning.
        for reg in [every_role(), one, Registry::default()] {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("nats.conf");
            let conf = crate::nats::render_conf(
                "127.0.0.1",
                4222,
                &d.path().join("store"),
                &crate::nats::Auth::Accounts(&reg),
            );
            std::fs::write(&p, &conf).unwrap();
            let o = std::process::Command::new(&bin)
                .arg("-t")
                .arg("-c")
                .arg(&p)
                .output()
                .unwrap();
            let err = String::from_utf8_lossy(&o.stderr);
            assert!(o.status.success(), "{err}\n{conf}");
            assert!(!err.contains("[WRN]") && !err.contains("[ERR]"), "{err}");
        }
    }
}
