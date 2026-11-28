//! Best-effort usage collection outside the seat loops, with one worker per runtime.

use std::collections::BTreeMap;
use std::sync::Arc;

use hgl_ai_proto::{Family, QuotaRefusal, Runtime, Seat};
use hgl_ai_usage::{Collected, fingerprint};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use crate::{Bridge, BusApi};

pub(crate) struct RefusedTurn {
    pub seat: Seat,
    pub family: Family,
    pub model: String,
    pub at: String,
    pub id: String,
}

/// Each message requests collection after a turn, with the refusal if there was one.
pub(crate) type UsageQueues = BTreeMap<Runtime, mpsc::UnboundedSender<Option<RefusedTurn>>>;

impl<B: BusApi> Bridge<B> {
    pub(crate) fn start_usage(self: &Arc<Self>) -> (Arc<UsageQueues>, JoinSet<()>) {
        let mut tasks = JoinSet::new();
        let queues = Runtime::ALL
            .iter()
            .map(|&runtime| {
                let (tx, rx) = mpsc::unbounded_channel();
                tasks.spawn(self.clone().usage_loop(runtime, rx));
                (runtime, tx)
            })
            .collect();
        (Arc::new(queues), tasks)
    }

    async fn usage_loop(
        self: Arc<Self>,
        runtime: Runtime,
        mut turns: mpsc::UnboundedReceiver<Option<RefusedTurn>>,
    ) {
        let sources = &self.opts.usage_sources;
        let every = self.opts.usage_every;
        let mut timer = interval_at(Instant::now() + every, every);
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = timer.tick(), if !turns.is_closed() => {}
                turn = turns.recv() => match turn {
                    Some(Some(refusal)) => self.publish_refusal(runtime, refusal).await,
                    Some(None) => {}
                    None => return,
                },
            }
            for outcome in hgl_ai_usage::collect(sources, &[runtime], &self.cfg.host).await {
                match outcome {
                    Collected::Record(record) => {
                        if let Err(e) = self.bus.usage_put(*record).await {
                            tracing::warn!(%runtime, "publishing a usage observation: {e}");
                        }
                    }
                    Collected::Skipped { why, .. } => {
                        tracing::debug!(%runtime, "skipping usage collection: {why}");
                    }
                }
            }
        }
    }

    async fn publish_refusal(&self, runtime: Runtime, turn: RefusedTurn) {
        let account = match fingerprint(&self.opts.usage_sources, runtime).await {
            Ok(account) => account,
            Err(e) => {
                tracing::warn!(%runtime, seat = %turn.seat, "resolving the account of a quota refusal: {e}");
                return;
            }
        };
        let event = QuotaRefusal {
            runtime,
            account,
            host: self.cfg.host.clone(),
            seat: turn.seat,
            family: turn.family,
            model: turn.model,
            at: turn.at,
        };
        if let Err(e) = self.bus.quota_refusal(&event, &turn.id).await {
            tracing::warn!(%runtime, seat = %event.seat, "publishing a quota refusal: {e}");
        }
    }
}
