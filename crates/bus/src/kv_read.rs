//! Finite KV queries through ephemeral pull consumers.

use async_nats::jetstream::{
    self,
    consumer::{AckPolicy, DeliverPolicy, pull},
    kv,
};
use futures::StreamExt;

use crate::{Bus, CONNECT_TIMEOUT, READ_BATCH, READER_IDLE, Result, live_entry, nats};

pub(crate) async fn keys(
    bus: &Bus,
    store: &kv::Store,
    keep: impl Fn(&str) -> bool,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let read = read(bus, store, ">", DeliverPolicy::LastPerSubject, true, |m| {
        let key = m
            .subject
            .strip_prefix(&store.prefix)
            .ok_or_else(|| nats("listing keys", format!("unexpected subject {}", m.subject)))?;
        if is_put(m.headers.as_ref()) && keep(key) {
            out.push(key.to_string());
        }
        Ok(())
    })
    .await;
    match read {
        Err(e) if bus.legacy_kv_pull_denied(&e) => {
            out = legacy_keys(store, keep).await?;
        }
        other => other?,
    }
    out.sort();
    out.dedup();
    Ok(out)
}

// Deployed single-account configurations only permit push consumers on KV streams.
// The existing stream-info and get grants suffice to list keys during rollout.
async fn legacy_keys(store: &kv::Store, keep: impl Fn(&str) -> bool) -> Result<Vec<String>> {
    let mut subjects = store
        .stream
        .info_with_subjects(format!("{}>", store.prefix))
        .await
        .map_err(|e| nats("listing keys", e))?;
    let mut out = Vec::new();
    while let Some(item) = subjects.next().await {
        let (subject, _) = item.map_err(|e| nats("listing keys", e))?;
        let key = subject
            .strip_prefix(&store.prefix)
            .ok_or_else(|| nats("listing keys", format!("unexpected subject {subject}")))?;
        if keep(key) && live_entry(store, key).await?.is_some() {
            out.push(key.to_string());
        }
    }
    Ok(out)
}

pub(crate) async fn read(
    bus: &Bus,
    store: &kv::Store,
    filter: &str,
    deliver_policy: DeliverPolicy,
    headers_only: bool,
    mut visit: impl FnMut(jetstream::Message) -> Result<()>,
) -> Result<()> {
    let what = format!("reading {} ({filter})", store.name);
    let consumer = store
        .stream
        .create_consumer(pull::Config {
            description: Some("hgl-ai KV reader".into()),
            filter_subject: format!("{}{filter}", store.prefix),
            deliver_policy,
            ack_policy: AckPolicy::None,
            headers_only,
            inactive_threshold: READER_IDLE,
            memory_storage: true,
            ..Default::default()
        })
        .await
        .map_err(|e| nats(&what, e))?;
    // Bound the read to the pending count at creation even if writers keep publishing.
    // The reader expires on its own, including when the role has no delete permission.
    let mut remaining = consumer.cached_info().num_pending;
    let subject = bus.api_subject(&format!(
        "CONSUMER.MSG.NEXT.{}.{}",
        store.stream_name,
        consumer.cached_info().name
    ));
    bus.guarded(&subject, async {
        while remaining > 0 {
            let count = remaining.min(READ_BATCH as u64);
            let received = tokio::time::timeout(CONNECT_TIMEOUT, async {
                let mut batch = consumer
                    .fetch()
                    .max_messages(count as usize)
                    .messages()
                    .await
                    .map_err(|e| nats(&what, e))?;
                let mut received = 0;
                while let Some(message) = batch.next().await {
                    visit(message.map_err(|e| nats(&what, e))?)?;
                    received += 1;
                }
                Ok::<_, crate::BusError>(received)
            })
            .await
            .map_err(|e| nats(&what, e))??;
            if received < count {
                break;
            }
            remaining -= received;
        }
        Ok(())
    })
    .await
}

pub(crate) fn is_put(headers: Option<&async_nats::HeaderMap>) -> bool {
    let Some(headers) = headers else { return true };
    if let Some(op) = headers.get("KV-Operation") {
        return !matches!(op.as_str(), "DEL" | "PURGE");
    }
    !headers
        .get("Nats-Marker-Reason")
        .is_some_and(|reason| matches!(reason.as_str(), "MaxAge" | "Purge" | "Remove"))
}
