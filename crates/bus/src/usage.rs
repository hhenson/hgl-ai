//! Subscription usage (PROTO-23, PROTO-24): `HGL_USAGE` keeps the newest record per login,
//! compared on `observed_at`; each record that wins its key also goes to `hgl.usage.<key>`
//! on `HGL_USAGE_LOG`, the history.

use std::collections::BTreeMap;

use async_nats::jetstream::context::Publish;
use async_nats::jetstream::kv;
use async_nats::jetstream::stream::LastRawMessageErrorKind;
use futures::StreamExt;
use hgl_ai_proto::{
    KV_USAGE, STREAM_USAGE, UsageLabel, UsageRecord, check_account, check_label, parse_stamp,
    usage_label_key, utc_now_stamp,
};

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

    /// Whether the history's last record for `rec`'s key is this observation. Read from
    /// the stream, not left to its deduplication window, so an idle login collected days
    /// later is not appended again.
    async fn usage_in_history(&self, rec: &UsageRecord) -> Result<bool> {
        let stream = self
            .js
            .get_stream(STREAM_USAGE)
            .await
            .map_err(|e| missing(&format!("stream {STREAM_USAGE}"), e))?;
        match stream.get_last_raw_message_by_subject(&rec.subject()).await {
            Ok(m) => Ok(serde_json::from_slice::<UsageRecord>(&m.payload)
                .is_ok_and(|h| h.observed_at == rec.observed_at)),
            Err(e) if e.kind() == LastRawMessageErrorKind::NoMessageFound => Ok(false),
            Err(e) => Err(nats(&format!("reading the history of {}", rec.key()), e)),
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
        let ack = self
            .guarded(&subject, async {
                self.js
                    .send_publish(subject.clone(), publish)
                    .await
                    .map_err(|e| publish_error(&subject, e))?
                    .await
                    .map_err(|e| publish_error(&subject, e))
            })
            .await?;
        Ok(ack.sequence)
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

fn conflict(key: &str, expected: u64) -> BusError {
    BusError::Conflict {
        key: key.to_string(),
        expected,
    }
}
