//! Gate builds (docs/build.md BUILD-15): where a repository names local build verdicts,
//! `gh land`, `gh merge-if-clean` and `build gate` ask a builder of each verdict's platform
//! to build the head merged onto the `main` the gate saw, under the current recipe, and
//! read the verdict the builder writes in the key of exactly that (head, `main`, recipe
//! revision): `HGL_STATE/verdict.<repo>.<pr>.<name>.<pair>` (docs/protocol.md §8). A build
//! of anything else writes another key, so it can never displace the one the gate reads; a
//! `main` that moved or a recipe that changed is another key, asked for again. The
//! requester writes no verdict: the builder's hub user alone may.

use std::collections::BTreeMap;
use std::sync::Mutex;

use hgl_ai_build::request;
use hgl_ai_bus::Bus;
use hgl_ai_proto::build::{
    BUILD_VERSION, BuildJob, BuildStatus, BuilderRecord, VerdictValue, VerdictWord, new_build_id,
    reply_seat, verdict_key, verdict_platform, verdict_prefix,
};
use hgl_ai_proto::{Seat, utc_now_stamp};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// One local build verdict as the gate reads it, for this head on this `main` under the
/// current recipe.
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
    /// None, and none asked: why, if known.
    Missing(Option<String>),
}

/// How many times one command asks for one verdict of one head on one `main` (the first,
/// and again for a build that was lost).
pub const MAX_ASKS: u32 = 3;

/// The gate build this command asked for last, for one verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Asked {
    /// The pull request, in a durable ask's value (its key alone may be ambiguous).
    #[serde(default)]
    repo: String,
    #[serde(default)]
    pr: u64,
    head: String,
    main: String,
    recipe: u64,
    id: String,
    builder: String,
    /// Asks for this head, `main` and recipe so far.
    asks: u32,
    /// A durable ask's delivery state: the job and its note's `created_at`, kept before the
    /// send, so a requester that stopped before it knew the send went out sends the same
    /// message again (the hub drops a copy) rather than wait for a job never published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    job: Option<BuildJob>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    at: String,
    /// The job note is published. An ask kept without it (an older value) counts as sent.
    #[serde(default = "yes")]
    sent: bool,
}

fn yes() -> bool {
    true
}

/// The gate builds one `gh land`, `gh merge-if-clean` or `build gate` has asked for. While
/// the build it asked for may still come, it waits for that one and asks nothing more; it
/// asks again only when that build is lost (at most [`MAX_ASKS`] in all).
#[derive(Debug)]
pub struct GateBuilds {
    /// The seat the jobs come from.
    pub sender: Seat,
    /// Who asked, in the summaries.
    pub requester: String,
    asked: Mutex<BTreeMap<String, Asked>>,
    /// The `HGL_STATE` key prefix each ask is also written under before it is sent, so a
    /// restarted requester waits for the build it asked for rather than ask again (the
    /// coordinator, a long-lived service); `None`: this command's memory only.
    durable: Option<String>,
}

/// What one verdict needs: the gate's pull request, name, head, `main` and the recipe
/// revision on the hub.
struct Want<'a> {
    repo: &'a str,
    pr: u64,
    name: &'a str,
    head: &'a str,
    main: &'a str,
    recipe: u64,
}

impl Want<'_> {
    /// This command's slot for the verdict, whatever head and `main`.
    fn slot(&self) -> String {
        format!("{}{}", verdict_prefix(self.repo, self.pr), self.name)
    }
    fn key(&self) -> String {
        verdict_key(
            self.repo,
            self.pr,
            self.name,
            self.head,
            self.main,
            self.recipe,
        )
    }
}

impl GateBuilds {
    pub fn new(sender: Seat, requester: impl Into<String>) -> GateBuilds {
        GateBuilds {
            sender,
            requester: requester.into(),
            asked: Mutex::new(BTreeMap::new()),
            durable: None,
        }
    }

    /// [`GateBuilds::new`] whose asks are kept on the hub under `prefix` too:
    /// `<prefix><repo>.<pr>.<name>`, JSON of the build asked for last.
    pub fn durable(sender: Seat, requester: impl Into<String>, prefix: &str) -> GateBuilds {
        GateBuilds {
            durable: Some(prefix.to_string()),
            ..GateBuilds::new(sender, requester)
        }
    }

    /// A durable ask's key for `w`, valid or `Err` why not.
    fn ask_key(&self, w: &Want<'_>) -> Option<std::result::Result<String, String>> {
        let p = self.durable.as_ref()?;
        let k = format!("{p}{}.{}.{}", w.repo, w.pr, w.name);
        Some(
            hgl_ai_proto::check_state_key(&k)
                .map(|_| k.clone())
                .map_err(|_| format!("{k} is not a state key; the ask cannot be kept")),
        )
    }

    /// The durable ask for `w`'s slot, into memory when memory has none (a restart).
    async fn recall(&self, bus: &Bus, w: &Want<'_>) -> Result<()> {
        let Some(Ok(k)) = self.ask_key(w) else {
            return Ok(());
        };
        if self.slot_held(w) {
            return Ok(());
        }
        let Some(e) = bus.state_get(&k).await? else {
            return Ok(());
        };
        match serde_json::from_str::<Asked>(&e.value) {
            Ok(a) if a.repo == w.repo && a.pr == w.pr => {
                self.asked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(w.slot())
                    .or_insert(a);
            }
            Ok(_) => {}
            Err(err) => tracing::warn!("{k}: unreadable gate build ask ({err})"),
        }
        Ok(())
    }

    fn slot_held(&self, w: &Want<'_>) -> bool {
        self.asked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&w.slot())
    }

    /// Forget every ask for `repo#pr`, in memory and on the hub: it landed or closed.
    pub async fn forget(&self, bus: &Bus, repo: &str, pr: u64) -> Result<()> {
        let slots = verdict_prefix(repo, pr);
        self.asked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, _| !k.starts_with(&slots));
        let Some(p) = &self.durable else {
            return Ok(());
        };
        for k in bus.state_keys(&format!("{p}{repo}.{pr}.")).await? {
            let ours = bus
                .state_get(&k)
                .await?
                .and_then(|e| serde_json::from_str::<Asked>(&e.value).ok())
                .is_some_and(|a| a.repo == repo && a.pr == pr);
            if ours {
                bus.state_del(&k).await?;
            }
        }
        Ok(())
    }

    /// The build asked for `w`'s exact head, `main` and recipe, if any.
    fn asked(&self, w: &Want<'_>) -> Option<Asked> {
        self.asked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&w.slot())
            .filter(|a| a.head == w.head && a.main == w.main && a.recipe == w.recipe)
            .cloned()
    }

    /// Send a gate build of `w` to a live builder of its platform, other than `not` (one
    /// that went away). `Err` is why none was sent.
    async fn ask(
        &self,
        bus: &Bus,
        builders: &[BuilderRecord],
        w: &Want<'_>,
        not: Option<&str>,
    ) -> Result<std::result::Result<Asked, String>> {
        let Some(platform) = verdict_platform(w.name) else {
            return Ok(Err(format!(
                "{} is not a platform's name; no builder can give it",
                w.name
            )));
        };
        let asks = self.asked(w).map_or(0, |a| a.asks);
        let live: Vec<BuilderRecord> = builders
            .iter()
            .filter(|b| Some(b.label.as_str()) != not)
            .cloned()
            .collect();
        let Some(b) = request::choose(&live, platform, None) else {
            return Ok(Err(format!("no live {platform} builder is registered")));
        };
        let key = match self.ask_key(w) {
            Some(Err(why)) => return Ok(Err(why)),
            Some(Ok(k)) => Some(k),
            None => None,
        };
        let id = new_build_id();
        let job = BuildJob {
            v: BUILD_VERSION,
            id: id.clone(),
            repo: w.repo.to_string(),
            head: w.head.to_string(),
            // The commit the gate compares with, not the branch: the verdict binds what
            // the gate will check, and a `main` that moves since is asked again.
            base: Some(w.main.to_string()),
            issue: w.pr,
            steps: None,
            recipe_rev: None,
            keep_going: false,
            verdict: Some(w.name.to_string()),
            reply_to: reply_seat(&id)?,
            notify: None,
            requester: self.requester.clone(),
        };
        let a = Asked {
            repo: w.repo.to_string(),
            pr: w.pr,
            head: w.head.to_string(),
            main: w.main.to_string(),
            recipe: w.recipe,
            id,
            builder: b.label.clone(),
            asks: asks + 1,
            job: key.is_some().then(|| job.clone()),
            at: if key.is_some() {
                utc_now_stamp()
            } else {
                String::new()
            },
            sent: key.is_none(),
        };
        let Some(k) = key else {
            request::send(bus, self.sender.clone(), &b, &job)
                .await
                .map_err(store_err)?;
            self.remember(w, &a);
            return Ok(Ok(a));
        };
        // Kept, unsent, before it is sent: a restart at any point after finds it and either
        // waits for the job or sends the same message again ([`GateBuilds::replay`]).
        self.keep(bus, &k, &a).await?;
        self.remember(w, &a);
        Ok(Ok(self.deliver(bus, &k, w, a, &b).await?))
    }

    fn remember(&self, w: &Want<'_>, a: &Asked) {
        self.asked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(w.slot(), a.clone());
    }

    async fn keep(&self, bus: &Bus, key: &str, a: &Asked) -> Result<()> {
        let text = serde_json::to_string(a).expect("an ask encodes");
        crate::store::put_latest(bus, key, &text).await?;
        Ok(())
    }

    /// Publish a durable ask's job with its kept `created_at`, then keep it sent. A failed
    /// send leaves it unsent, for the next read to send again.
    async fn deliver(
        &self,
        bus: &Bus,
        key: &str,
        w: &Want<'_>,
        mut a: Asked,
        b: &BuilderRecord,
    ) -> Result<Asked> {
        let Some(job) = a.job.clone() else {
            return Ok(a);
        };
        request::send_at(bus, self.sender.clone(), b, &job, Some(&a.at))
            .await
            .map_err(store_err)?;
        a.sent = true;
        self.keep(bus, key, &a).await?;
        self.remember(w, &a);
        Ok(a)
    }

    /// A durable ask whose send is not known to have gone out: its builder has the job
    /// (its summary is past `queued`), or the same message is sent again while that
    /// builder takes jobs. Otherwise it stays unsent and is lost like any other.
    async fn replay(
        &self,
        bus: &Bus,
        builders: &[BuilderRecord],
        w: &Want<'_>,
        mut a: Asked,
    ) -> Result<Asked> {
        let Some(Ok(key)) = self.ask_key(w) else {
            return Ok(a);
        };
        let run = hgl_ai_build::store::run_get(bus, &a.id)
            .await
            .map_err(store_err)?;
        if run.is_some_and(|s| s.status != BuildStatus::Queued) {
            a.sent = true;
            self.keep(bus, &key, &a).await?;
            self.remember(w, &a);
            return Ok(a);
        }
        let Some(b) = builders
            .iter()
            .find(|b| b.label == a.builder && b.state == "active" && b.reachable)
        else {
            return Ok(a);
        };
        tracing::info!("{}: sending {} to {} again", key, a.id, a.builder);
        self.deliver(bus, &key, w, a, b).await
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

/// The value in `key`: its text and what it says.
async fn read(bus: &Bus, key: &str) -> Result<(String, Option<VerdictValue>)> {
    let raw = bus
        .state_get(key)
        .await?
        .map(|e| e.value.trim().to_string())
        .unwrap_or_default();
    let v = VerdictValue::parse(&raw);
    Ok((raw, v))
}

/// `pass` or `fail` for the pair: the gate's answer.
fn judged(raw: &str, v: &Option<VerdictValue>, w: &Want<'_>) -> Option<VerdictState> {
    let v = v.as_ref().filter(|v| v.is_for(w.head, w.main))?;
    match v.word {
        VerdictWord::Pass => Some(VerdictState::Pass(raw.to_string())),
        VerdictWord::Fail => Some(VerdictState::Fail(raw.to_string())),
        _ => None,
    }
}

/// The verdict `name` of `repo#pr` for `head` on `main` under the hub's current recipe.
/// With `builds` (the gate acts), one neither judged nor being built is asked for; a build
/// this command asked for is waited for while it may come, and asked again (on another live
/// builder) when it is lost: its builder gone, its `running` value past its bound, or an end
/// without its verdict.
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
    // The gate needs the current recipe's verdict: an update asks again (BUILD-15).
    let Some(recipe) = hgl_ai_build::store::recipe_get(bus, repo, None)
        .await
        .map_err(store_err)?
    else {
        return Ok(VerdictState::Missing(Some(format!(
            "no recipe for {repo} on the hub"
        ))));
    };
    let w = Want {
        repo,
        pr,
        name,
        head,
        main,
        recipe: recipe.revision,
    };
    let key = w.key();
    let (raw, value) = read(bus, &key).await?;
    if let Some(j) = judged(&raw, &value, &w) {
        return Ok(j);
    }
    let running = value
        .as_ref()
        .filter(|v| v.is_for(head, main) && v.word == VerdictWord::Running);
    let Some(builds) = builds else {
        return Ok(match (running, &value) {
            (Some(v), _) => VerdictState::Building(format!(
                "building {} on {}",
                v.id.as_deref().unwrap_or("?"),
                v.builder.as_deref().unwrap_or("?")
            )),
            (None, Some(v)) if v.word == VerdictWord::NotRun => {
                VerdictState::Missing(Some(format!(
                    "not run: {}",
                    v.reason.as_deref().unwrap_or("no reason given")
                )))
            }
            _ => VerdictState::Missing(None),
        });
    };
    let builders = bus.builders().await?;
    let now = utc_now_stamp();
    builds.recall(bus, &w).await?;
    let mut asked = builds.asked(&w);
    if let Some(a) = asked.clone().filter(|a| !a.sent) {
        asked = Some(builds.replay(bus, &builders, &w, a).await?);
    }
    // Why the build to wait for is lost, and the builder that lost it; `None` with nothing
    // to wait for.
    let lost: Option<(String, Option<String>)> = if let Some(a) = &asked {
        // The build this command asked for decides; another build's value in the key (an
        // older one's `running`) is not waited on.
        let ours = value
            .as_ref()
            .filter(|v| v.id.as_deref() == Some(a.id.as_str()));
        if let Some(v) = ours
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
                let (raw, again) = read(bus, &key).await?;
                if let Some(j) = judged(&raw, &again, &w) {
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
                    st => Some((
                        format!("{} on {} ended {st} without its verdict", a.id, a.builder),
                        None,
                    )),
                }
            }
            Some(s) if s.status == BuildStatus::Running => {
                if ours.is_some_and(|v| v.expired(&now)) {
                    Some((
                        format!("{} on {} ran past its bound", a.id, a.builder),
                        None,
                    ))
                } else if !alive(&builders, &a.builder) {
                    Some((
                        format!("builder {} of {} is gone", a.builder, a.id),
                        Some(a.builder.clone()),
                    ))
                } else {
                    return Ok(VerdictState::Building(format!(
                        "building {} on {}",
                        a.id, a.builder
                    )));
                }
            }
            // Kept before the send and not sent, with its builder not taking jobs, or no
            // summary at all: lost.
            _ if !a.sent => Some((format!("{} was not sent to {}", a.id, a.builder), None)),
            None if builds.durable.is_some() => {
                Some((format!("{} was not sent to {}", a.id, a.builder), None))
            }
            // Queued (or its summary not kept): waiting while its builder takes jobs.
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
    } else if let Some(v) = running {
        // Another command's build: waited for while it may come.
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
        } else if run
            .as_ref()
            .is_some_and(|s| s.status == BuildStatus::Queued)
            && !taking(&builders, &label)
        {
            // Stopped and queued again, with its builder now draining or unreachable.
            Some((
                format!("{id} is queued again with builder {label}, which takes no jobs"),
                Some(label),
            ))
        } else if run.as_ref().is_some_and(|s| s.status.is_final()) {
            let (raw, again) = read(bus, &key).await?;
            if let Some(j) = judged(&raw, &again, &w) {
                return Ok(j);
            }
            Some((format!("{id} on {label} ended without its verdict"), None))
        } else {
            return Ok(VerdictState::Building(format!("building {id} on {label}")));
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
        match builds.ask(bus, &builders, &w, not.as_deref()).await? {
            Ok(a) => VerdictState::Building(match why {
                Some(why) => format!("{why}; asked {} of {}", a.id, a.builder),
                None => format!("asked {} of {}", a.id, a.builder),
            }),
            Err(e) => match why {
                Some(why) => VerdictState::Missing(Some(format!("{why}; {e}"))),
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
