//! `[builder]`: the builder service of this host (docs/build.md §2, §2a). Host-local: a
//! remote's ssh alias and directory never go to the hub or a repository.

use std::collections::BTreeMap;

use hgl_ai_proto::build::{Platform, builder_inbox};
use serde::{Deserialize, Serialize};

/// Tools whose `--version` a builder reports in its record.
pub fn default_toolchains() -> Vec<String> {
    ["git", "cargo", "rustc", "cmake", "ninja", "c++"]
        .map(String::from)
        .to_vec()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Builder {
    /// Build this host's own platform (`false`: only drive the remotes).
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub local: bool,
    /// Builds at a time on this host.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub concurrency: u32,
    /// SDK name to its prefix on this host: `needs = ["sdk:hgraph"]` is met when it is
    /// set, and steps see it as `HGL_SDK_HGRAPH`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sdk: BTreeMap<String, String>,
    #[serde(
        default = "default_toolchains",
        skip_serializing_if = "is_default_toolchains"
    )]
    pub toolchains: Vec<String>,
    /// Hosts this one drives over ssh as a proxy builder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote: Vec<Remote>,
    /// OS user running a separate builder service, when different from the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The hub credential of this host's builder user (role `builder`), which alone may
    /// write the local build verdicts (docs/build.md BUILD-15); default: the host's own.
    /// Readable only by the OS user that runs `builder run`, never by the agents'.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

impl Default for Builder {
    fn default() -> Self {
        Builder {
            local: true,
            concurrency: 1,
            sdk: BTreeMap::new(),
            toolchains: default_toolchains(),
            remote: Vec::new(),
            credential: None,
            user: None,
        }
    }
}

impl Builder {
    pub fn is_default(&self) -> bool {
        *self == Builder::default()
    }
}

/// `[[builder.remote]]`: a host without hgl-ai, built over ssh by this one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    pub platform: Platform,
    /// An alias from the user's ssh configuration.
    pub ssh: String,
    /// The remote's directory for clones and job checkouts (`C:/hgl-ai/work`).
    pub work_dir: String,
    /// `powershell` (Windows) or `sh` (a POSIX remote).
    #[serde(default = "powershell")]
    pub shell: String,
    /// The remote's label (unique in the fleet); default `<host>-<platform>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub concurrency: u32,
    /// SDK prefixes on the remote, as `[builder.sdk]` for this host.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sdk: BTreeMap<String, String>,
}

impl Remote {
    pub fn label(&self, host: &str) -> String {
        self.label
            .clone()
            .unwrap_or_else(|| format!("{host}-{}", self.platform))
    }
}

fn yes() -> bool {
    true
}
fn is_yes(b: &bool) -> bool {
    *b
}
fn one() -> u32 {
    1
}
fn is_one(n: &u32) -> bool {
    *n == 1
}
fn powershell() -> String {
    "powershell".into()
}
fn is_default_toolchains(t: &[String]) -> bool {
    t == default_toolchains().as_slice()
}

fn label_ok(l: &str) -> bool {
    !l.is_empty()
        && !l.starts_with('-')
        && l.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Check `[builder]` for host `host`.
pub fn validate(b: &Builder, host: &str) -> Result<(), String> {
    if b.user.as_ref().is_some_and(|u| {
        u.is_empty()
            || u.starts_with('-')
            || !u
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }) {
        return Err("builder.user must be an OS user name".into());
    }
    if b.concurrency == 0 {
        return Err("builder.concurrency must be at least 1".into());
    }
    let mut labels = std::collections::BTreeSet::from([host.to_string()]);
    for r in &b.remote {
        let label = r.label(host);
        if !label_ok(&label) || builder_inbox(&label).is_err() {
            return Err(format!(
                "builder.remote label {label:?}: use [a-z0-9-], at most 24 (set `label`)"
            ));
        }
        if !labels.insert(label.clone()) {
            return Err(format!("builder.remote label {label} appears twice"));
        }
        if !matches!(r.shell.as_str(), "powershell" | "sh") {
            return Err(format!(
                "builder.remote {label}: shell {:?}: use powershell or sh",
                r.shell
            ));
        }
        if r.ssh.trim().is_empty() || r.ssh.starts_with('-') || r.ssh.contains(char::is_whitespace)
        {
            return Err(format!(
                "builder.remote {label}: ssh {:?} is not an alias",
                r.ssh
            ));
        }
        if r.work_dir.trim().is_empty() || r.work_dir.contains(['\'', '"', '`', '$', '\n']) {
            return Err(format!(
                "builder.remote {label}: work_dir {:?}: give a plain path",
                r.work_dir
            ));
        }
        if r.concurrency == 0 {
            return Err(format!(
                "builder.remote {label}: concurrency must be at least 1"
            ));
        }
    }
    for name in b
        .sdk
        .keys()
        .chain(b.remote.iter().flat_map(|r| r.sdk.keys()))
    {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!("builder sdk name {name:?}: use [A-Za-z0-9_-]"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes() {
        let b: Builder = toml::from_str(
            r#"
concurrency = 2
sdk = { hgraph = "~/sdk/hgraph" }
[[remote]]
platform = "windows"
ssh = "winbox"
work_dir = "C:/hgl-ai/work"
"#,
        )
        .unwrap();
        validate(&b, "lab1").unwrap();
        assert_eq!(b.remote[0].shell, "powershell");
        assert_eq!(b.remote[0].label("lab1"), "lab1-windows");
        assert!(b.local);
        let mut bad = b.clone();
        bad.remote[0].shell = "cmd".into();
        assert!(validate(&bad, "lab1").is_err());
        let mut bad = b.clone();
        bad.remote[0].label = Some("lab1".into());
        assert!(
            validate(&bad, "lab1").is_err(),
            "a remote's label is not the host's"
        );
        let mut bad = b.clone();
        bad.remote[0].ssh = "-oProxyCommand=x".into();
        assert!(validate(&bad, "lab1").is_err());
        assert!(validate(&b, &"h".repeat(20)).is_err(), "label too long");
        assert!(Builder::default().is_default());
    }
}
