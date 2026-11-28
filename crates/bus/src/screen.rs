//! Subject-scoped access to the screen's KV records and the decision log.

use async_nats::jetstream::{
    self,
    context::{Publish, PublishErrorKind},
    message::StreamMessage,
    stream::{DirectGetErrorKind, Stream},
};
use futures::TryStreamExt;
use hgl_ai_proto::{
    HUB_API_PREFIX, KEY_DECISIONS, KV_SCREEN, KV_STATE, SCREEN_LOG_KEYS, parse_stamp,
    screen::{ScreenLog, ScreenRule, check_rule_id},
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{APPEND_ATTEMPTS, Bus, BusError, Result, StateEntry, backoff, check_value_size, nats};

const RULE_IDS: &str = "rule._ids";
const DECISION_AT: &str = "Hgl-Screen-Decision-At";
const LEGACY_API: &str = "$JS.API";

pub struct ScreenRuleEntry {
    pub rule: ScreenRule,
    pub revision: u64,
    /// When the rule was written, if its decision-log entry still needs recording.
    pub decision_at: Option<String>,
}

struct ScopedKv<'a> {
    bus: &'a Bus,
    api: &'static str,
    bucket: &'static str,
    js: jetstream::Context,
}

impl Bus {
    async fn scoped_kv(&self, bucket: &'static str, key: &str) -> Result<ScopedKv<'_>> {
        let prefix = self
            .scoped_api
            .get_or_try_init(|| async {
                let subject = format!("{HUB_API_PREFIX}.DIRECT.GET.KV_{bucket}.$KV.{bucket}.{key}");
                // A missing key still gets a 404 from the imported stream. In the
                // single-account layout the import does not exist.
                let answer = self
                    .guarded(&subject, async {
                        match self.client.request(subject.clone(), "".into()).await {
                            Ok(_) => Ok(HUB_API_PREFIX),
                            Err(e) if e.kind() == async_nats::RequestErrorKind::NoResponders => {
                                Ok(LEGACY_API)
                            }
                            Err(e) => Err(nats("checking the hub API", e)),
                        }
                    })
                    .await;
                match answer {
                    Err(BusError::Denied { .. }) => Ok(LEGACY_API),
                    other => other,
                }
            })
            .await?;
        Ok(ScopedKv {
            bus: self,
            api: prefix,
            bucket,
            js: jetstream::with_prefix(self.client.clone(), prefix),
        })
    }

    async fn screen_kv(&self) -> Result<ScopedKv<'_>> {
        self.scoped_kv(KV_SCREEN, RULE_IDS).await
    }

    /// Verify the controller's log-read grant, even for a bare NKey with no role label.
    pub async fn screen_check_controller(&self) -> Result<()> {
        // Sequence zero is reserved, so an authorized request will normally get a 404.
        self.screen_kv().await?.get("log.0").await?;
        Ok(())
    }

    pub async fn screen_rule_get(&self, id: &str) -> Result<Option<ScreenRuleEntry>> {
        check_rule_id(id)?;
        self.screen_kv().await?.rule(id).await
    }

    /// All current rule records, including expired and retired ones, in ID order.
    /// The screen can enumerate them using just its rule-subject direct gets.
    pub async fn screen_rules(&self) -> Result<Vec<ScreenRule>> {
        let kv = self.screen_kv().await?;
        let (_, ids) = kv.rule_ids().await?;
        let mut rules = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(entry) = kv.rule(&id).await? {
                rules.push(entry.rule);
            }
        }
        Ok(rules)
    }

    /// Write one revision of a rule and its pending decision (if any).
    /// Create reserves the ID first, so an interrupted create can be retried
    /// and a successfully written rule is always discoverable.
    pub async fn screen_rule_update(
        &self,
        rule: &ScreenRule,
        revision: u64,
        decision_at: Option<&str>,
    ) -> Result<u64> {
        rule.validate()?;
        if decision_at.is_some_and(|stamp| parse_stamp(stamp).is_none()) {
            return Err(nats("screen rule decision", "invalid timestamp"));
        }
        let key = format!("rule.{}", rule.id);
        let payload = encode(&key, rule)?;
        let kv = self.screen_kv().await?;
        if revision == 0 {
            kv.reserve_rule_id(&rule.id).await?;
        }
        let mut publish = Publish::build().payload(payload.into());
        if let Some(stamp) = decision_at {
            publish = publish.header(DECISION_AT, stamp);
        }
        kv.update(&key, publish, revision).await
    }

    /// Record one result for an original note sequence; the first result wins
    /// if the screen processes the note again.
    pub async fn screen_log_put(&self, entry: &ScreenLog) -> Result<()> {
        entry.validate()?;
        let key = format!("log.{}", entry.seq);
        let payload = encode(&key, entry)?;
        let kv = self.screen_kv().await?;
        match kv
            .update(&key, Publish::build().payload(payload.into()), 0)
            .await
        {
            Ok(_) | Err(BusError::Conflict { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Read screen results strictly after the original HGL_NOTES sequence.
    pub async fn screen_log(&self, since: u64) -> Result<Vec<ScreenLog>> {
        let kv = self.screen_kv().await?;
        let stream = kv.stream().await?;
        let prefix = format!("$KV.{KV_SCREEN}.log.");
        let info = format!("{}.STREAM.INFO.KV_{KV_SCREEN}", kv.api);
        let mut keys = self
            .guarded(&info, async {
                let mut subjects = stream
                    .info_with_subjects(format!("$KV.{KV_SCREEN}.{SCREEN_LOG_KEYS}"))
                    .await
                    .map_err(|e| nats("listing screen log", e))?;
                let mut keys = Vec::new();
                while let Some((subject, _)) = subjects
                    .try_next()
                    .await
                    .map_err(|e| nats("listing screen log", e))?
                {
                    if let Some(seq) = subject
                        .strip_prefix(&prefix)
                        .and_then(|s| s.parse::<u64>().ok())
                        && seq > since
                    {
                        keys.push(seq);
                    }
                }
                Ok(keys)
            })
            .await?;
        keys.sort_unstable();
        keys.dedup();
        let mut entries = Vec::with_capacity(keys.len());
        for seq in keys {
            let key = format!("log.{seq}");
            if let Some(m) = kv.get(&key).await?.filter(is_put) {
                let entry: ScreenLog = decode(&key, &m)?;
                entry.validate()?;
                if entry.seq != seq {
                    return Err(nats(&key, "record sequence differs from key"));
                }
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// The decision log can be read by a role with access to that key alone.
    pub async fn decision_get(&self) -> Result<Option<StateEntry>> {
        let kv = self.scoped_kv(KV_STATE, KEY_DECISIONS).await?;
        kv.get(KEY_DECISIONS)
            .await?
            .filter(is_put)
            .map(|m| {
                let value = std::str::from_utf8(&m.payload)
                    .map_err(|e| nats("reading decisions.log", e))?;
                Ok(StateEntry {
                    value: value.to_owned(),
                    revision: m.sequence,
                    created: m.time.unix_timestamp(),
                })
            })
            .transpose()
    }

    /// Compare-and-set for the exact decision-log key.
    pub async fn decision_update(&self, value: &str, revision: u64) -> Result<u64> {
        check_value_size(KEY_DECISIONS, value.len())?;
        let kv = self.scoped_kv(KV_STATE, KEY_DECISIONS).await?;
        let expected = if revision == 0 {
            match kv.get(KEY_DECISIONS).await? {
                Some(m) if is_put(&m) => {
                    return Err(BusError::Conflict {
                        key: KEY_DECISIONS.into(),
                        expected: 0,
                    });
                }
                Some(m) => m.sequence,
                None => 0,
            }
        } else {
            revision
        };
        kv.update(
            KEY_DECISIONS,
            Publish::build().payload(value.as_bytes().to_vec().into()),
            expected,
        )
        .await
    }
}

impl ScopedKv<'_> {
    async fn stream(&self) -> Result<Stream<()>> {
        self.js
            .get_stream_no_info(format!("KV_{}", self.bucket))
            .await
            .map_err(|e| nats("opening bucket", e))
    }

    async fn get(&self, key: &str) -> Result<Option<StreamMessage>> {
        let subject = format!("$KV.{}.{key}", self.bucket);
        let api = format!("{}.DIRECT.GET.KV_{}.{subject}", self.api, self.bucket);
        let stream = self.stream().await?;
        self.bus
            .guarded(&api, async {
                match stream.direct_get_last_for_subject(&subject).await {
                    Ok(m) => {
                        check_value_size(key, m.payload.len())?;
                        Ok(Some(m))
                    }
                    Err(e) if e.kind() == DirectGetErrorKind::NotFound => Ok(None),
                    Err(e) => Err(nats(&format!("reading {} {key}", self.bucket), e)),
                }
            })
            .await
    }

    async fn update(&self, key: &str, publish: Publish, revision: u64) -> Result<u64> {
        let kv_subject = format!("$KV.{}.{key}", self.bucket);
        let subject = if self.api == LEGACY_API {
            kv_subject
        } else {
            format!("{}.{kv_subject}", self.api)
        };
        self.bus
            .guarded(&subject, async {
                let ack = self
                    .js
                    .send_publish(
                        subject.clone(),
                        publish.expected_last_subject_sequence(revision),
                    )
                    .await
                    .map_err(|e| nats(&format!("writing {} {key}", self.bucket), e))?;
                match ack.await {
                    Ok(ack) => Ok(ack.sequence),
                    Err(e) if e.kind() == PublishErrorKind::WrongLastSequence => {
                        Err(BusError::Conflict {
                            key: key.into(),
                            expected: revision,
                        })
                    }
                    Err(e) => Err(nats(&format!("writing {} {key}", self.bucket), e)),
                }
            })
            .await
    }

    async fn rule(&self, id: &str) -> Result<Option<ScreenRuleEntry>> {
        let key = format!("rule.{id}");
        self.get(&key)
            .await?
            .filter(is_put)
            .map(|m| {
                let rule: ScreenRule = decode(&key, &m)?;
                rule.validate()?;
                if rule.id != id {
                    return Err(nats(&key, "record ID differs from key"));
                }
                let decision_at = m.headers.get(DECISION_AT).map(|v| v.as_str().to_owned());
                Ok(ScreenRuleEntry {
                    rule,
                    revision: m.sequence,
                    decision_at,
                })
            })
            .transpose()
    }

    async fn rule_ids(&self) -> Result<(u64, Vec<String>)> {
        match self.get(RULE_IDS).await? {
            Some(m) if is_put(&m) => {
                let ids: Vec<String> = decode(RULE_IDS, &m)?;
                for id in &ids {
                    check_rule_id(id)?;
                }
                Ok((m.sequence, ids))
            }
            Some(m) => Ok((m.sequence, Vec::new())),
            None => Ok((0, Vec::new())),
        }
    }

    async fn reserve_rule_id(&self, id: &str) -> Result<()> {
        for attempt in 0..APPEND_ATTEMPTS {
            let (revision, mut ids) = self.rule_ids().await?;
            match ids.binary_search_by(|stored| stored.as_str().cmp(id)) {
                Ok(_) => return Ok(()),
                Err(at) => ids.insert(at, id.to_owned()),
            }
            let payload = encode(RULE_IDS, &ids)?;
            match self
                .update(RULE_IDS, Publish::build().payload(payload.into()), revision)
                .await
            {
                Err(BusError::Conflict { .. }) => tokio::time::sleep(backoff(attempt)).await,
                other => return other.map(|_| ()),
            }
        }
        Err(nats(
            "reserving a screen rule ID",
            "the index kept changing",
        ))
    }
}

fn is_put(m: &StreamMessage) -> bool {
    !m.headers
        .get("KV-Operation")
        .is_some_and(|op| matches!(op.as_str(), "DEL" | "PURGE"))
}

fn encode(key: &str, value: &impl Serialize) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(value).map_err(|e| nats(key, e))?;
    check_value_size(key, payload.len())?;
    Ok(payload)
}

fn decode<T: DeserializeOwned>(key: &str, m: &StreamMessage) -> Result<T> {
    serde_json::from_slice(&m.payload).map_err(|e| nats(key, e))
}
