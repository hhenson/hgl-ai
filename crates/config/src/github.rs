//! Settings of the GitHub services and gates (`crates/gh`), under `[github]`.
//! Every key has a default, so a configuration file names only what it changes.
//! The defaults reproduce the rules of the HGL workspace the services were written for.

use std::collections::BTreeMap;

use hgl_ai_proto::Seat;
use serde::{Deserialize, Serialize};

/// The reviewer pool for one repository: `[github.review_pools.<repo>]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPool {
    pub seats: Vec<Seat>,
    /// The controller handles all review routing for this repository.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub manual: bool,
}

impl ReviewPool {
    pub fn automatic(&self, repo: &str) -> bool {
        repo != "hgl_spec" && !self.manual && !self.seats.is_empty()
    }
}

/// Gate settings of one repository: `[github.gate.<repo>]`. A repository not listed has
/// the defaults ([`RepoGate::default`]): a review receipt is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoGate {
    /// A merge needs a clean review receipt (docs/protocol.md PROTO-19) for the exact head,
    /// from a model family other than the author's (docs/roles.md ROLE-11). Default `true`.
    #[serde(default = "yes", skip_serializing_if = "is_yes")]
    pub review_receipt: bool,
    /// Path prefixes added to `github.global_paths` for this repository.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub global_paths: Vec<String>,
    /// Names of the local build verdicts a merge needs for the head on the present `main`
    /// under the current recipe, each a platform (`mac` for macOS): state keys
    /// `verdict.<repo>.<pr>.<name>.<pair>`, value `<head> <main> pass|fail ...`
    /// (docs/build.md BUILD-15).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verdicts: Vec<String>,
    /// The checks a merge needs (docs/roles.md ROLE-20): check-run names or commit-status
    /// contexts, each present at the head and passed. Every other check present must pass
    /// as well; with none listed and none present, a mergeable pull request merges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    /// Regular expressions over changed paths. A pull request that changes only such paths
    /// does not wait for the `relax_optional` checks once every `relax_required` check passed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relax_paths: Vec<String>,
    /// Check names that must have succeeded for the relaxation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relax_required: Vec<String>,
    /// Regular expressions over check names that may still run but must not have failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relax_optional: Vec<String>,
}

fn yes() -> bool {
    true
}

fn is_yes(v: &bool) -> bool {
    *v
}

impl Default for RepoGate {
    fn default() -> Self {
        RepoGate {
            review_receipt: true,
            global_paths: Vec::new(),
            verdicts: Vec::new(),
            checks: Vec::new(),
            relax_paths: Vec::new(),
            relax_required: Vec::new(),
            relax_optional: Vec::new(),
        }
    }
}

/// One submodule pin of the wrapper repository: `[[github.pins]]`. Give `repo` (follow that
/// repository's `main`) or `from` and `submodule` (the commit that submodule path holds in the
/// commit another pin is raised to).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    /// Submodule path in the wrapper.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The `path` of another pin that has a `repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Submodule path inside `from`'s repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submodule: Option<String>,
}

/// `[github.release]`: what `gh release watch` waits for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseWatch {
    pub repo: String,
    /// Workflow run event to watch.
    pub event: String,
    /// Workflow names that must each have a run on the commit before it counts as finished.
    pub workflows: Vec<String>,
    /// Workflow names whose jobs are listed.
    pub list_jobs: Vec<String>,
    pub timeout: String,
}

impl Default for ReleaseWatch {
    fn default() -> Self {
        ReleaseWatch {
            repo: "hgl-compiler".into(),
            event: "push".into(),
            workflows: vec!["CI".into(), "Release bundles".into()],
            list_jobs: vec!["Release bundles".into()],
            timeout: "110m".into(),
        }
    }
}

/// `[github.tap]`: the Homebrew tap and the bottle flow of `gh tap release`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tap {
    pub repo: String,
    /// Formula that gets the bottle stanza (`Formula/<formula>.rb`).
    pub formula: String,
    /// Repository whose release holds the bottle and which runs the bottle workflow.
    pub source_repo: String,
    pub bottle_workflow: String,
    pub bottle_tag: String,
    /// The formula text the stanza is placed after; it must occur exactly once.
    pub anchor: String,
}

impl Default for Tap {
    fn default() -> Self {
        Tap {
            repo: "homebrew-hgraph".into(),
            formula: "hgl".into(),
            source_repo: "hgl-compiler".into(),
            bottle_workflow: "homebrew-bottle.yml".into(),
            bottle_tag: "arm64_tahoe".into(),
            anchor: "  license \"MIT\"\n\n".into(),
        }
    }
}

macro_rules! default_fns {
    ($($name:ident, $is:ident: $t:ty = $e:expr;)+) => {
        $(
            pub fn $name() -> $t { $e }
            pub fn $is(v: &$t) -> bool { *v == $name() }
        )+
    };
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

default_fns! {
    review_bot, is_review_bot: String = "chatgpt-codex-connector".into();
    controller, is_controller: String = "controller".into();
    coord_seat, is_coord_seat: String = "coord".into();
    coord_interval, is_coord_interval: String = "5m".into();
    hold_below, is_hold_below: u64 = 100;
    global_paths, is_global_paths: Vec<String> = strings(&[".github/", "Cargo.toml", "Cargo.lock"]);
    hygiene_files, is_hygiene_files: Vec<String> = strings(&[
        r"(^|/)__pycache__/", r"\.pyc$", r"(^|/)target/", r"\.keep/", r"\.orig$", r"\.rej$", r"(^|/)\.venv", r"\.log$",
    ]);
    // Written so that the patterns do not match their own text.
    hygiene_lines, is_hygiene_lines: Vec<String> = strings(&["/home/[a-z]", "/tmp/claude[-]", "/Users/[A-Za-z]"]);
    gates, is_gates: BTreeMap<String, RepoGate> = default_gates();
    pins, is_pins: Vec<Pin> = default_pins();
    pin_title, is_pin_title: String = "Pin the compiler at {sha8}".into();
    pin_branch, is_pin_branch: String = "pins-compiler-{sha8}".into();
    release, is_release: ReleaseWatch = ReleaseWatch::default();
    tap, is_tap: Tap = Tap::default();
}

fn default_gates() -> BTreeMap<String, RepoGate> {
    let compiler = RepoGate {
        review_receipt: true,
        global_paths: strings(&["spec/hgl_spec", "spec/hgl_core"]),
        verdicts: strings(&["linux", "mac", "windows"]),
        // The `relax_optional` tests are left out, or the relaxation could never apply;
        // they are judged whenever they run.
        checks: strings(&[
            "lint",
            "test (ubuntu-latest)",
            "cpp (plain, runtime and core)",
            "cpp (plain, sdk)",
            "cpp (sanitize, runtime and core)",
            "cpp (sanitize, sdk)",
        ]),
        relax_paths: strings(&[
            r"^runtime/cpp/",
            r"^crates/hgl_cpp/",
            r"^crates/hgl_cli/src/cpp",
            r"^crates/hgl_cli/tests/cpp",
            r"^docs/",
            r"^scripts/provision-hgraph-sdk\.sh$",
        ]),
        relax_required: strings(&[
            "lint",
            "test (ubuntu-latest)",
            "cpp (plain, runtime and core)",
            "cpp (plain, sdk)",
            "cpp (sanitize, runtime and core)",
            "cpp (sanitize, sdk)",
        ]),
        relax_optional: strings(&[r"^test \((macos|windows)-latest\)$"]),
    };
    let checks = |c: &[&str]| RepoGate {
        checks: strings(c),
        ..RepoGate::default()
    };
    BTreeMap::from([
        ("hgl-compiler".to_string(), compiler),
        (
            "hgl-ai".to_string(),
            checks(&[
                "lint",
                "lint (macos-latest)",
                "test (ubuntu-latest)",
                "test (macos-latest)",
            ]),
        ),
        (
            "hgl-project".to_string(),
            checks(&["docs/readthedocs.org:hgl-project"]),
        ),
        ("homebrew-hgraph".to_string(), checks(&["linux-formula"])),
    ])
}

fn default_pins() -> Vec<Pin> {
    let follow = |path: &str| Pin {
        path: path.into(),
        repo: Some(path.into()),
        from: None,
        submodule: None,
    };
    let via = |path: &str, sub: &str| Pin {
        path: path.into(),
        repo: None,
        from: Some("hgl-compiler".into()),
        submodule: Some(sub.into()),
    };
    vec![
        follow("hgl-compiler"),
        via("hgl_spec", "spec/hgl_spec"),
        via("hgl_core", "spec/hgl_core"),
    ]
}

/// Check the GitHub service settings that the schema alone cannot.
pub fn validate_pins(pins: &[Pin]) -> Result<(), String> {
    for p in pins {
        match (&p.repo, &p.from, &p.submodule) {
            (Some(_), None, None) => {}
            (None, Some(f), Some(_)) => {
                let src = pins.iter().find(|q| &q.path == f);
                if !src.is_some_and(|q| q.repo.is_some()) {
                    return Err(format!(
                        "pin {}: `from = {f:?}` must name a pin that has a `repo`",
                        p.path
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "pin {}: give `repo`, or `from` and `submodule`",
                    p.path
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_checks_of_the_fleet() {
        let g: RepoGate = toml::from_str("checks = [\"lint\"]").unwrap();
        assert_eq!(g.checks, ["lint"]);
        assert!(RepoGate::default().checks.is_empty());
        let all = gates();
        let of = |r: &str| all.get(r).map(|g| g.checks.clone()).unwrap_or_default();
        assert_eq!(
            of("hgl-ai"),
            [
                "lint",
                "lint (macos-latest)",
                "test (ubuntu-latest)",
                "test (macos-latest)"
            ]
        );
        assert_eq!(of("hgl-project"), ["docs/readthedocs.org:hgl-project"]);
        assert_eq!(of("homebrew-hgraph"), ["linux-formula"]);
        assert!(of("hgl_spec").is_empty() && of("hgl_core").is_empty());
        let compiler = of("hgl-compiler");
        assert_eq!(
            compiler,
            [
                "lint",
                "test (ubuntu-latest)",
                "cpp (plain, runtime and core)",
                "cpp (plain, sdk)",
                "cpp (sanitize, runtime and core)",
                "cpp (sanitize, sdk)",
            ]
        );
        assert_eq!(all["hgl-compiler"].relax_required, compiler);
        // The relaxation's optional checks are not required, or it could never apply.
        for o in ["test (macos-latest)", "test (windows-latest)"] {
            assert!(!compiler.iter().any(|c| c == o), "{o}");
        }
    }

    #[test]
    fn a_review_receipt_is_required_unless_turned_off() {
        assert!(RepoGate::default().review_receipt);
        assert!(gates().values().all(|g| g.review_receipt));
        let g: RepoGate = toml::from_str("verdicts = [\"linux\"]").unwrap();
        assert!(
            g.review_receipt,
            "an entry that does not name it keeps the default"
        );
        let off: RepoGate = toml::from_str("review_receipt = false").unwrap();
        assert!(!off.review_receipt);
        assert_eq!(
            toml::to_string(&off).unwrap().trim(),
            "review_receipt = false"
        );
        assert!(toml::to_string(&g).unwrap().contains("verdicts"));
        assert!(!toml::to_string(&g).unwrap().contains("review_receipt"));
    }
}
