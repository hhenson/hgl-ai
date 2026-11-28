//! Subscription usage (PROTO-23, PROTO-24): `HGL_USAGE` keeps the newest record per login,
//! compared on `observed_at`; each record that wins its key also goes to `hgl.usage.<key>`
//! on `HGL_USAGE_LOG`, the history.

use std::collections::BTreeMap;

use async_nats::jetstream::context::{Publish, PublishErrorKind};
use async_nats::jetstream::kv;
use async_nats::jetstream::message::StreamMessage;
use async_nats::jetstream::stream::{LastRawMessageErrorKind, Stream};
use futures::StreamExt;
use hgl_ai_proto::{
    KV_USAGE, QuotaRefusal, STREAM_USAGE, UsageLabel, UsageRecord, check_account, check_label,
    parse_stamp, usage_label_key, utc_now_stamp,
};

/// Quota events link back to the last observation on the same subject, or 0 if none.
const OBSERVATION_SEQUENCE: &str = "Hgl-Usage-Observation-Sequence";

use crate::{
    APPEND_ATTEMPTS, Bus, BusError, MAX_VALUE_BYTES, Result, backoff, live_entry, missing, nats,
    publish_error, put_json,
};

/// What [`Bus::usage_put`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsagePut {
    /// The record is the newest: written at this revision and sent to the history.
    Written { revision: u64, history_seq: u64 },
    /// The hub holds an observation at least as new, from `host` at `observed_at`; nothing
    /// was written.
    Stale { host: String, observed_at: String },
}

impl Bus {
    /// Write `HGL_USAGE/<runtime>.<account>` if `rec` is newer than what it holds
    /// (compare-and-set on `observed_at`, so of several hosts reporting one login the
    /// newest observation stays), then publish it to the history.
    pub async fn usage_put(&self, rec: &UsageRecord) -> Result<UsagePut> {
        rec.validate()?;
        let key = rec.key();
        let value = serde_json::to_vec(rec).map_err(|e| nats(&format!("encoding {key}"), e))?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(BusError::Nats(format!(
                "usage {key} would be {} bytes; the limit is {MAX_VALUE_BYTES}",
                value.len()
            )));
        }
        let new_at = parse_stamp(&rec.observed_at);
        let store = self.store(&self.usage, KV_USAGE).await?;
        let kv_subject = format!("$KV.{KV_USAGE}.{key}");
        let mut revision = None;
        for attempt in 0..APPEND_ATTEMPTS {
            let held = live_entry(store, &key).await?;
            let expected = match &held {
                None => 0,
                Some(e) => {
                    if let Ok(old) = serde_json::from_slice::<UsageRecord>(&e.value)
                        && let Some(old_at) = parse_stamp(&old.observed_at)
                        && Some(old_at) >= new_at
                    {
                        if Some(old_at) == new_at && !self.usage_in_history(&old).await? {
                            // The history missed it (a stop between the two writes):
                            // send the held record now.
                            self.usage_history(&old, e.value.to_vec()).await?;
                        }
                        return Ok(UsagePut::Stale {
                            host: old.host,
                            observed_at: old.observed_at,
                        });
                    }
                    e.revision
                }
            };
            let res = self
                .guarded(&kv_subject, async {
                    if expected == 0 {
                        // A deleted key counts as absent (`create` replaces a tombstone).
                        store
                            .create(&key, value.clone().into())
                            .await
                            .map_err(|e| match e.kind() {
                                kv::CreateErrorKind::AlreadyExists => conflict(&key, 0),
                                _ => nats(&format!("writing usage {key}"), e),
                            })
                    } else {
                        store
                            .update(&key, value.clone().into(), expected)
                            .await
                            .map_err(|e| match e.kind() {
                                kv::UpdateErrorKind::WrongLastRevision => conflict(&key, expected),
                                _ => nats(&format!("writing usage {key}"), e),
                            })
                    }
                })
                .await;
            match res {
                Ok(r) => {
                    revision = Some(r);
                    break;
                }
                Err(BusError::Conflict { .. }) => tokio::time::sleep(backoff(attempt)).await,
                Err(e) => return Err(e),
            }
        }
        let Some(revision) = revision else {
            return Err(conflict(&key, 0));
        };
        Ok(UsagePut::Written {
            revision,
            history_seq: self.usage_history(rec, value).await?,
        })
    }

    /// Publish a refusal without updating the latest usage record in KV.
    pub async fn usage_quota_refusal(&self, event: &QuotaRefusal, turn_id: &str) -> Result<u64> {
        event.validate()?;
        let subject = event.subject();
        let msg_id = format!("quota_refusal.{turn_id}.{}", event.at);
        let payload = serde_json::to_vec(event)
            .map_err(|e| nats(&format!("encoding quota refusal for {subject}"), e))?;
        let stream = self.usage_stream().await?;
        for attempt in 0..APPEND_ATTEMPTS {
            let latest = usage_latest(&stream, &subject).await?;
            let expected = latest.as_ref().map_or(0, |m| m.sequence);
            let observation = latest.as_ref().map_or(0, observation_sequence);
            // Check the subject head at publish so a racing observation is never hidden
            // behind an event pointing to an older observation.
            let publish = Publish::build()
                .payload(payload.clone().into())
                .message_id(&msg_id)
                .header(OBSERVATION_SEQUENCE, observation.to_string())
                .expected_last_subject_sequence(expected);
            match self.publish_usage(&subject, publish, Some(expected)).await {
                Err(BusError::Conflict { .. }) => tokio::time::sleep(backoff(attempt)).await,
                r => return r,
            }
        }
        Err(conflict(&subject, 0))
    }

    async fn usage_stream(&self) -> Result<Stream> {
        self.js
            .get_stream(STREAM_USAGE)
            .await
            .map_err(|e| missing(&format!("stream {STREAM_USAGE}"), e))
    }

    /// Whether the latest retained observation for the key is this one, even when quota
    /// events follow it or the deduplication window has elapsed.
    async fn usage_in_history(&self, rec: &UsageRecord) -> Result<bool> {
        let stream = self.usage_stream().await?;
        let subject = rec.subject();
        let Some(latest) = usage_latest(&stream, &subject).await? else {
            return Ok(false);
        };
        let seq = observation_sequence(&latest);
        let observed = match seq {
            0 => return Ok(false),
            n if n == latest.sequence => latest,
            n => match stream.get_raw_message(n).await {
                Ok(m) => m,
                Err(e) if e.kind() == LastRawMessageErrorKind::NoMessageFound => return Ok(false),
                Err(e) => return Err(nats(&format!("reading the history of {subject}"), e)),
            },
        };
        Ok(observed.subject.as_str() == subject
            && serde_json::from_slice::<UsageRecord>(&observed.payload)
                .is_ok_and(|h| h.observed_at == rec.observed_at))
    }

    async fn publish_usage(
        &self,
        subject: &str,
        publish: Publish,
        expected: Option<u64>,
    ) -> Result<u64> {
        self.guarded(subject, async {
            let ack = self
                .js
                .send_publish(subject.to_string(), publish)
                .await
                .map_err(|e| publish_error(subject, e))?
                .await
                .map_err(|e| match expected {
                    Some(seq) if e.kind() == PublishErrorKind::WrongLastSequence => {
                        conflict(subject, seq)
                    }
                    _ => publish_error(subject, e),
                })?;
            Ok(ack.sequence)
        })
        .await
    }

    /// The label in force for one account.
    pub async fn usage_account_label(&self, account: &str) -> Result<Option<String>> {
        check_account(account)?;
        let store = self.store(&self.usage, KV_USAGE).await?;
        let key = usage_label_key(account);
        match live_entry(store, &key).await? {
            Some(e) => serde_json::from_slice::<UsageLabel>(&e.value)
                .map(|l| Some(l.label))
                .map_err(|e| nats(&format!("reading usage {key}"), e)),
            None => Ok(None),
        }
    }

    /// Publish `value` (the encoded `rec`) to the history, message id
    /// `<key>.<observed_at>`; returns its stream sequence.
    async fn usage_history(&self, rec: &UsageRecord, value: Vec<u8>) -> Result<u64> {
        let subject = rec.subject();
        let publish = Publish::build().payload(value.into()).message_id(format!(
            "{}.{}",
            rec.key(),
            rec.observed_at
        ));
        self.publish_usage(&subject, publish, None).await
    }

    /// Every usage record, ordered by key, with the owner's labels in force (a label set
    /// or removed after a record was written shows at once).
    pub async fn usage(&self) -> Result<Vec<UsageRecord>> {
        let (records, labels) = self.usage_all().await?;
        Ok(records
            .into_iter()
            .map(|mut r| {
                // The label in force, not the one the record was written with.
                r.label = labels.get(&r.account).cloned();
                r
            })
            .collect())
    }

    /// Every label, by account fingerprint.
    pub async fn usage_labels(&self) -> Result<BTreeMap<String, String>> {
        Ok(self.usage_all().await?.1)
    }

    /// Label an account (`hgl-ai usage label`); `None` removes the label.
    pub async fn usage_label(&self, account: &str, label: Option<&str>) -> Result<()> {
        check_account(account)?;
        let store = self.store(&self.usage, KV_USAGE).await?;
        let key = usage_label_key(account);
        let subject = format!("$KV.{KV_USAGE}.{key}");
        match label {
            Some(l) => {
                check_label(l)?;
                let rec = UsageLabel {
                    label: l.to_string(),
                    set_at: utc_now_stamp(),
                };
                self.guarded(&subject, put_json(store, &key, &rec)).await
            }
            None => {
                self.guarded(&subject, async {
                    store
                        .purge(&key)
                        .await
                        .map_err(|e| nats(&format!("deleting {key}"), e))
                })
                .await
            }
        }
    }

    async fn usage_all(&self) -> Result<(Vec<UsageRecord>, BTreeMap<String, String>)> {
        let store = self.store(&self.usage, KV_USAGE).await?;
        let mut keys = store.keys().await.map_err(|e| nats("listing keys", e))?;
        let mut names = Vec::new();
        while let Some(k) = keys.next().await {
            names.push(k.map_err(|e| nats("listing keys", e))?);
        }
        names.sort();
        names.dedup();
        let mut records = Vec::new();
        let mut labels = BTreeMap::new();
        for key in names {
            let Some(e) = live_entry(store, &key).await? else {
                continue;
            };
            if let Some(account) = key.strip_prefix("label.") {
                match serde_json::from_slice::<UsageLabel>(&e.value) {
                    Ok(l) => {
                        labels.insert(account.to_string(), l.label);
                    }
                    Err(err) => tracing::warn!(key, "skipping an unreadable label: {err}"),
                }
                continue;
            }
            match serde_json::from_slice::<UsageRecord>(&e.value) {
                Ok(r) => records.push(r),
                Err(err) => tracing::warn!(key, "skipping an unreadable usage record: {err}"),
            }
        }
        Ok((records, labels))
    }
}

async fn usage_latest(stream: &Stream, subject: &str) -> Result<Option<StreamMessage>> {
    match stream.get_last_raw_message_by_subject(subject).await {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == LastRawMessageErrorKind::NoMessageFound => Ok(None),
        Err(e) => Err(nats(&format!("reading the history of {subject}"), e)),
    }
}

fn observation_sequence(m: &StreamMessage) -> u64 {
    m.headers
        .get(OBSERVATION_SEQUENCE)
        .map_or(m.sequence, |h| h.as_str().parse().unwrap_or(0))
}

fn conflict(key: &str, expected: u64) -> BusError {
    BusError::Conflict {
        key: key.to_string(),
        expected,
    }
}
