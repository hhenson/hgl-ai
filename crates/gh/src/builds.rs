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
use hgl_ai_proto::Seat;
use hgl_ai_proto::build::{
    BUILD_VERSION, BuildJob, BuildStatus, VerdictValue, VerdictWord, new_build_id, reply_seat,
    verdict_key, verdict_platform,
};

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

/// A gate build asked for by this landing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Asked {
    head: String,
    main: String,
    id: String,
    builder: String,
}

/// The gate builds one `gh land` or `gh merge-if-clean` has asked for, so each pair of head
/// and `main` is asked once per verdict.
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

    /// Send a gate build of `head` merged onto `main` to a builder of the verdict's
    /// platform. `Err` is why none was sent.
    async fn ask(
        &self,
        bus: &Bus,
        repo: &str,
        pr: u64,
        name: &str,
        head: &str,
        main: &str,
    ) -> Result<std::result::Result<Asked, String>> {
        let Some(platform) = verdict_platform(name) else {
            return Ok(Err(format!(
                "{name} is not a platform's name; no builder can give it"
            )));
        };
        let builders = bus.builders().await?;
        let Some(b) = request::choose(&builders, platform, None) else {
            return Ok(Err(format!("no {platform} builder is registered")));
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
        };
        self.asked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(verdict_key(repo, pr, name), a.clone());
        Ok(Ok(a))
    }
}

fn store_err(e: hgl_ai_build::store::StoreError) -> Error {
    match e {
        hgl_ai_build::store::StoreError::Bus(b) => Error::Bus(b),
        other => Error::Invalid(other.to_string()),
    }
}

/// The verdict `name` of `repo#pr` for `head` on `main`, from the value on the hub. With
/// `builds` (the gate acts), a verdict neither judged nor being built is asked for.
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
    let raw = bus.state_get(&key).await?.map(|e| e.value);
    let value = raw.as_deref().and_then(VerdictValue::parse);
    let mine = value.as_ref().filter(|v| v.is_for(head, main));
    if let Some(v) = mine {
        let raw = raw.as_deref().unwrap_or_default().trim().to_string();
        match v.word {
            VerdictWord::Pass => return Ok(VerdictState::Pass(raw)),
            VerdictWord::Fail => return Ok(VerdictState::Fail(raw)),
            VerdictWord::Running => {
                return Ok(VerdictState::Building(format!(
                    "building {} on {}",
                    v.id.as_deref().unwrap_or("?"),
                    v.builder.as_deref().unwrap_or("?")
                )));
            }
            VerdictWord::NotRun => {}
        }
    }
    // What the value says instead, for a line.
    let other = value.as_ref().map(|v| match v.word {
        VerdictWord::NotRun if v.head == head => format!(
            "not run: {}",
            v.reason.as_deref().unwrap_or("no reason given")
        ),
        _ if v.head != head => format!("for head {}", short(&v.head)),
        _ => format!("for main {}", short(&v.main)),
    });
    let Some(builds) = builds else {
        return Ok(VerdictState::Missing(other));
    };
    if let Some(a) = builds.asked(&key)
        && a.head == head
        && a.main == main
    {
        // Ours: its verdict, else its run.
        if let Some(v) = &value
            && v.id.as_deref() == Some(a.id.as_str())
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
        if let Some(s) = run.filter(|s| s.status.is_final()) {
            // The builder writes the verdict before the final run, so read it again: none
            // now means it could not.
            let again = bus.state_get(&key).await?.map(|e| e.value);
            if let Some(raw) = again.as_deref()
                && let Some(v) = VerdictValue::parse(raw).filter(|v| v.is_for(head, main))
            {
                match v.word {
                    VerdictWord::Pass => return Ok(VerdictState::Pass(raw.trim().into())),
                    VerdictWord::Fail => return Ok(VerdictState::Fail(raw.trim().into())),
                    _ => {}
                }
            }
            let why = match s.status {
                BuildStatus::NotRun | BuildStatus::Stopped => {
                    s.reason.unwrap_or_else(|| "no reason given".into())
                }
                st => format!("it ended {st} but wrote no verdict"),
            };
            return Ok(VerdictState::NotRun(format!(
                "{} on {}: {why}",
                a.id, a.builder
            )));
        }
        return Ok(VerdictState::Building(format!(
            "asked {} of {}",
            a.id, a.builder
        )));
    }
    Ok(match builds.ask(bus, repo, pr, name, head, main).await? {
        Ok(a) => VerdictState::Building(format!("asked {} of {}", a.id, a.builder)),
        Err(why) => VerdictState::Missing(Some(why)),
    })
}
