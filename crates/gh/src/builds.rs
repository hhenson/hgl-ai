//! Gate builds (docs/build.md BUILD-15): where a repository names local build verdicts,
//! `gh land` and `gh merge-if-clean` ask a builder of each verdict's platform to build the
//! head merged onto the `main` the gate saw, and read the verdict the builder writes
//! (`HGL_STATE/verdict.<repo>.<pr>.<name>`, docs/protocol.md §8). Only `pass` or `fail` for
//! this head on this `main` decides; a verdict for another `main` (it moved) is asked
//! again. The requester writes no verdict: the builder's hub user alone may.

use std::collections::BTreeMap;
use std::sync::Mutex;

use hgl_ai_build::request;
use hgl_ai_bus::Bus;
use hgl_ai_proto::build::{
    BUILD_VERSION, BuildJob, BuildStatus, BuilderRecord, VerdictValue, VerdictWord, new_build_id,
    reply_seat, verdict_key, verdict_platform,
};
use hgl_ai_proto::{Seat, utc_now_stamp};

use crate::{Error, Result, short};

/// One local build verdict as the gate reads it, for this head on this `main`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictState {
    /// The value.
    Pass(String),
    /// The value.
    Fail(String),
    /// A builder is building it, or has it queued: what and where.
    Building(String),
    /// The build this gate asked for did not judge the code: why.
    NotRun(String),
    /// None for this pair, and none asked: what there is instead, if anything.
    Missing(Option<String>),
}

/// How many times one landing asks for one verdict of one head on one `main` (the first,
/// and again for a builder that went away or a verdict that was not written).
pub const MAX_ASKS: u32 = 3;

/// A gate build asked for by this landing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Asked {
    head: String,
    main: String,
    id: String,
    builder: String,
    /// Asks for this pair so far.
    asks: u32,
}

/// The gate builds one `gh land`, `gh merge-if-clean` or `build gate` has asked for, so
/// each pair of head and `main` is asked once per verdict, and again only when its build is
/// lost (at most [`MAX_ASKS`] in all).
#[derive(Debug)]
pub struct GateBuilds {
    /// The seat the jobs come from.
    pub sender: Seat,
    /// Who asked, in the summaries.
    pub requester: String,
    asked: Mutex<BTreeMap<String, Asked>>,
}

impl GateBuilds {
    pub fn new(sender: Seat, requester: impl Into<String>) -> GateBuilds {
        GateBuilds {
            sender,
            requester: requester.into(),
            asked: Mutex::new(BTreeMap::new()),
        }
    }

    fn asked(&self, key: &str) -> Option<Asked> {
        self.asked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    /// Send a gate build of `head` merged onto `main` to a live builder of the verdict's
    /// platform, other than `not` (one that went away). `Err` is why none was sent.
    #[allow(clippy::too_many_arguments)]
    async fn ask(
        &self,
        bus: &Bus,
        builders: &[BuilderRecord],
        repo: &str,
        pr: u64,
        name: &str,
        head: &str,
        main: &str,
        not: Option<&str>,
    ) -> Result<std::result::Result<Asked, String>> {
        let Some(platform) = verdict_platform(name) else {
            return Ok(Err(format!(
                "{name} is not a platform's name; no builder can give it"
            )));
        };
        let key = verdict_key(repo, pr, name);
        let asks = self
            .asked(&key)
            .filter(|a| a.head == head && a.main == main)
            .map_or(0, |a| a.asks);
        let live: Vec<BuilderRecord> = builders
            .iter()
            .filter(|b| Some(b.label.as_str()) != not)
            .cloned()
            .collect();
        let Some(b) = request::choose(&live, platform, None) else {
            return Ok(Err(format!("no live {platform} builder is registered")));
        };
        let id = new_build_id();
        let job = BuildJob {
            v: BUILD_VERSION,
            id: id.clone(),
            repo: repo.to_string(),
            head: head.to_string(),
            // The commit the gate compares with, not the branch: the verdict binds what
            // the gate will check, and a `main` that moves since is asked again.
            base: Some(main.to_string()),
            issue: pr,
            steps: None,
            recipe_rev: None,
            keep_going: false,
            verdict: Some(name.to_string()),
            reply_to: reply_seat(&id)?,
            notify: None,
            requester: self.requester.clone(),
        };
        request::send(bus, self.sender.clone(), &b, &job)
            .await
            .map_err(store_err)?;
        let a = Asked {
            head: head.to_string(),
            main: main.to_string(),
            id,
            builder: b.label.clone(),
            asks: asks + 1,
        };
        self.asked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, a.clone());
        Ok(Ok(a))
    }
}

fn store_err(e: hgl_ai_build::store::StoreError) -> Error {
    match e {
        hgl_ai_build::store::StoreError::Bus(b) => Error::Bus(b),
        other => Error::Invalid(other.to_string()),
    }
}

/// Builder `label` serves: its record is on the hub (it expires two minutes after its last
/// heartbeat) and it is not retired.
fn alive(builders: &[BuilderRecord], label: &str) -> bool {
    builders
        .iter()
        .any(|b| b.label == label && b.state != "retired")
}

/// Takes queued jobs: active and reachable.
fn taking(builders: &[BuilderRecord], label: &str) -> bool {
    builders
        .iter()
        .any(|b| b.label == label && b.state == "active" && b.reachable)
}

/// The verdict `name` of `repo#pr` for `head` on `main`, from the value on the hub. With
/// `builds` (the gate acts), a verdict neither judged nor being built is asked for, and
/// asked again (on another live builder) when its build is lost: a builder gone, a
/// `running` value past its bound, or a build that ended without its verdict.
#[allow(clippy::too_many_arguments)]
pub async fn verdict_state(
    bus: Option<&Bus>,
    builds: Option<&GateBuilds>,
    repo: &str,
    pr: u64,
    name: &str,
    head: &str,
    main: &str,
) -> Result<VerdictState> {
    let Some(bus) = bus else {
        return Ok(VerdictState::Missing(None));
    };
    let key = verdict_key(repo, pr, name);
    let read = |raw: Option<String>| {
        let v = raw.as_deref().and_then(VerdictValue::parse);
        (raw.map(|r| r.trim().to_string()), v)
    };
    let (raw, value) = read(bus.state_get(&key).await?.map(|e| e.value));
    let judged = |raw: &Option<String>, v: &Option<VerdictValue>| match v {
        Some(v) if v.is_for(head, main) && v.word == VerdictWord::Pass => {
            Some(VerdictState::Pass(raw.clone().unwrap_or_default()))
        }
        Some(v) if v.is_for(head, main) && v.word == VerdictWord::Fail => {
            Some(VerdictState::Fail(raw.clone().unwrap_or_default()))
        }
        _ => None,
    };
    if let Some(j) = judged(&raw, &value) {
        return Ok(j);
    }
    // What the value says instead, for a line.
    let other = value.as_ref().map(|v| match v.word {
        VerdictWord::NotRun if v.head == head => format!(
            "not run: {}",
            v.reason.as_deref().unwrap_or("no reason given")
        ),
        VerdictWord::Running if v.is_for(head, main) => format!(
            "building {} on {}",
            v.id.as_deref().unwrap_or("?"),
            v.builder.as_deref().unwrap_or("?")
        ),
        _ if v.head != head => format!("for head {}", short(&v.head)),
        _ => format!("for main {}", short(&v.main)),
    });
    let Some(builds) = builds else {
        let running = value
            .as_ref()
            .filter(|v| v.is_for(head, main) && v.word == VerdictWord::Running);
        return Ok(match running {
            Some(_) => VerdictState::Building(other.unwrap_or_default()),
            None => VerdictState::Missing(other),
        });
    };
    let builders = bus.builders().await?;
    let now = utc_now_stamp();
    let asked = builds
        .asked(&key)
        .filter(|a| a.head == head && a.main == main);
    // Why the build of this pair is lost, and the builder that lost it; `None` while it
    // may still come.
    let lost: Option<(String, Option<String>)> = if let Some(v) = value
        .as_ref()
        .filter(|v| v.is_for(head, main) && v.word == VerdictWord::Running)
    {
        let id = v.id.clone().unwrap_or_default();
        let label = v.builder.clone().unwrap_or_default();
        let run = hgl_ai_build::store::run_get(bus, &id)
            .await
            .map_err(store_err)?;
        if v.expired(&now) {
            // Alive but past its bound: any live builder may take it again, this one too.
            Some((format!("{id} on {label} ran past its bound"), None))
        } else if !alive(&builders, &label) {
            Some((format!("builder {label} of {id} is gone"), Some(label)))
        } else if run.as_ref().is_some_and(|s| s.status.is_final()) {
            // It may have just written it: the verdict goes before the final run.
            let (raw, again) = read(bus.state_get(&key).await?.map(|e| e.value));
            if let Some(j) = judged(&raw, &again) {
                return Ok(j);
            }
            Some((format!("{id} on {label} ended without its verdict"), None))
        } else {
            return Ok(VerdictState::Building(format!("building {id} on {label}")));
        }
    } else if let Some(a) = &asked {
        // Ours, not started or ended: its verdict, else its run.
        let theirs = value.as_ref().and_then(|v| v.id.clone());
        if let Some(v) = &value
            && theirs.as_deref() == Some(a.id.as_str())
            && v.word == VerdictWord::NotRun
        {
            return Ok(VerdictState::NotRun(format!(
                "{} on {}: {}",
                a.id,
                a.builder,
                v.reason.as_deref().unwrap_or("no reason given")
            )));
        }
        let run = hgl_ai_build::store::run_get(bus, &a.id)
            .await
            .map_err(store_err)?;
        match run {
            Some(s) if s.status.is_final() => {
                // The builder writes the verdict before the final run, so read it again.
                let (raw, again) = read(bus.state_get(&key).await?.map(|e| e.value));
                if let Some(j) = judged(&raw, &again) {
                    return Ok(j);
                }
                match s.status {
                    BuildStatus::NotRun | BuildStatus::Stopped => {
                        return Ok(VerdictState::NotRun(format!(
                            "{} on {}: {}",
                            a.id,
                            a.builder,
                            s.reason.unwrap_or_else(|| "no reason given".into())
                        )));
                    }
                    // A judged build whose verdict is not there: not written, or replaced
                    // by a build asked later for another pair. Ask again.
                    st => Some((
                        format!("{} on {} ended {st} without its verdict", a.id, a.builder),
                        None,
                    )),
                }
            }
            // Started (or not yet): wait while its builder serves.
            Some(s) if s.status == BuildStatus::Running && alive(&builders, &a.builder) => {
                return Ok(VerdictState::Building(format!(
                    "asked {} of {}",
                    a.id, a.builder
                )));
            }
            _ if taking(&builders, &a.builder) => {
                return Ok(VerdictState::Building(format!(
                    "asked {} of {}",
                    a.id, a.builder
                )));
            }
            _ => Some((
                format!("builder {} of {} is not taking jobs", a.builder, a.id),
                Some(a.builder.clone()),
            )),
        }
    } else {
        None
    };
    let (why, not) = match lost {
        Some((why, not)) => (Some(why), not),
        None => (None, None),
    };
    if let Some(a) = &asked
        && a.asks >= MAX_ASKS
    {
        return Ok(VerdictState::NotRun(format!(
            "asked {} times; {}",
            a.asks,
            why.unwrap_or_default()
        )));
    }
    Ok(
        match builds
            .ask(bus, &builders, repo, pr, name, head, main, not.as_deref())
            .await?
        {
            Ok(a) => VerdictState::Building(match why {
                Some(w) => format!("{w}; asked {} of {}", a.id, a.builder),
                None => format!("asked {} of {}", a.id, a.builder),
            }),
            Err(e) => match why {
                Some(w) => VerdictState::Missing(Some(format!("{w}; {e}"))),
                None => VerdictState::Missing(Some(e)),
            },
        },
    )
}

/// `hgl-ai build gate`: every local build verdict `repo#pr` needs, for its head on the
/// present `main`, asking for those neither judged nor being built (as `gh land` does).
/// Returns the head, `main` and each verdict.
pub async fn gate<G: crate::exec::Gh>(
    gh: &G,
    s: &crate::Settings,
    bus: &Bus,
    builds: &GateBuilds,
    repo: &str,
    pr: u64,
) -> Result<(String, String, Vec<(String, VerdictState)>)> {
    let names = s.gate_rules(repo).verdicts;
    if names.is_empty() {
        return Err(Error::Invalid(format!(
            "{repo} names no local build verdicts ([github.gate.{repo}] verdicts)"
        )));
    }
    let p = crate::gate::fetch_pr(gh, s, repo, pr).await?;
    if p.merged || p.state != "open" {
        return Err(Error::Invalid(format!("{repo} #{pr} is not open")));
    }
    let main = crate::gate::main_head(gh, s, repo).await?;
    let mut out = Vec::new();
    for name in names {
        let st = verdict_state(Some(bus), Some(builds), repo, pr, &name, &p.head, &main).await?;
        out.push((name, st));
    }
    Ok((p.head, main, out))
}
