//! Usage probes and quota refusals run independently of each other and of seat turns.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hgl_ai_proto::{Family, QuotaRefusal, Runtime, Seat};
use hgl_ai_usage::{ClaudeAccount, claude_account, collect_claude, collect_runtime, fingerprint};
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until};

use crate::{Bridge, BusApi, down, lock};

pub(crate) struct RefusedTurn {
    pub seat: Seat,
    pub family: Family,
    pub model: String,
    pub at: String,
    pub id: String,
}

pub(crate) type UsageQueues = BTreeMap<Runtime, RuntimeUsage>;

pub(crate) struct RuntimeUsage {
    collection: Arc<CollectionRequests>,
    refusals: mpsc::UnboundedSender<RefusedTurn>,
}

impl RuntimeUsage {
    pub(crate) fn after_turn(&self, runtime: Runtime, refusal: Option<RefusedTurn>) {
        if runtime != Runtime::Claude || refusal.is_none() {
            self.collection.request(CollectRequest::FinishedTurn);
        }
        if let Some(refusal) = refusal
            && self.refusals.send(refusal).is_err()
        {
            tracing::warn!(%runtime, "bridge quota-refusal worker stopped");
        }
    }
}

enum CollectRequest {
    FinishedTurn,
    Claude(Result<ClaudeAccount, String>),
}

#[derive(Default)]
struct CollectionRequests {
    pending: Mutex<Option<CollectRequest>>,
    wake: Notify,
}

impl CollectionRequests {
    fn request(&self, request: CollectRequest) {
        let mut pending = lock(&self.pending);
        // Keep a resolved login for the coalesced probe when an ordinary turn also ends.
        if !matches!(
            (&*pending, &request),
            (
                Some(CollectRequest::Claude(Ok(_))),
                CollectRequest::FinishedTurn
            )
        ) {
            *pending = Some(request);
        }
        self.wake.notify_one();
    }

    async fn run<F: Future<Output = ()>>(
        &self,
        every: Duration,
        minimum: Duration,
        mut collect: impl FnMut(Option<CollectRequest>) -> F,
    ) {
        let every = every.max(minimum);
        let mut earliest = Instant::now();
        let mut periodic = earliest + every;
        loop {
            let after_turn = tokio::select! {
                _ = sleep_until(periodic) => false,
                _ = self.wake.notified() => {
                    sleep_until(earliest).await;
                    true
                }
            };
            let request = lock(&self.pending).take();
            // A timer can consume the pending turn request before its wakeup is read.
            if after_turn && request.is_none() {
                continue;
            }
            collect(request).await;
            let finished = Instant::now();
            earliest = finished + minimum;
            periodic = finished + every;
        }
    }
}

pub(crate) struct RefusalTasks {
    close: watch::Sender<bool>,
    tasks: JoinSet<()>,
}

impl RefusalTasks {
    /// Called after the seats have finished, so the workers can drain all accepted events.
    pub(crate) async fn drain(mut self) {
        self.close.send_replace(true);
        while let Some(result) = self.tasks.join_next().await {
            if let Err(e) = result {
                tracing::warn!("bridge quota-refusal task failed: {e}");
            }
        }
    }
}

impl<B: BusApi> Bridge<B> {
    pub(crate) fn start_usage(
        self: &Arc<Self>,
        collectors: &mut JoinSet<()>,
        shutdown: watch::Receiver<bool>,
    ) -> (Arc<UsageQueues>, RefusalTasks) {
        let (close, closed) = watch::channel(false);
        let mut refusals = JoinSet::new();
        let queues = Runtime::ALL
            .iter()
            .map(|&runtime| {
                let collection = Arc::new(CollectionRequests::default());
                let requests = collection.clone();
                let bridge = self.clone();
                let mut shutdown = shutdown.clone();
                collectors.spawn(async move {
                    tokio::select! {
                        biased;
                        _ = down(&mut shutdown) => {}
                        _ = bridge.collect_loop(runtime, &requests) => {}
                    }
                });
                let (tx, rx) = mpsc::unbounded_channel();
                refusals.spawn(self.clone().refusal_loop(
                    runtime,
                    rx,
                    collection.clone(),
                    closed.clone(),
                ));
                (
                    runtime,
                    RuntimeUsage {
                        collection,
                        refusals: tx,
                    },
                )
            })
            .collect();
        (
            Arc::new(queues),
            RefusalTasks {
                close,
                tasks: refusals,
            },
        )
    }

    async fn collect_loop(&self, runtime: Runtime, requests: &CollectionRequests) {
        let sources = &self.opts.usage_sources;
        let collect = |request| async move {
            let observed = match request {
                Some(CollectRequest::Claude(Ok(login))) => {
                    collect_claude(sources, &self.cfg.host, login).await
                }
                Some(CollectRequest::Claude(Err(e))) => Err(e),
                _ => collect_runtime(sources, runtime, &self.cfg.host).await,
            };
            match observed {
                Ok(record) => {
                    if let Err(e) = self.bus.usage_put(record).await {
                        tracing::warn!(%runtime, "publishing a usage observation: {e}");
                    }
                }
                Err(why) => tracing::debug!(%runtime, "skipping usage collection: {why}"),
            }
        };
        requests
            .run(self.opts.usage_every, self.opts.usage_min_every, collect)
            .await;
    }

    async fn refusal_loop(
        self: Arc<Self>,
        runtime: Runtime,
        mut turns: mpsc::UnboundedReceiver<RefusedTurn>,
        collection: Arc<CollectionRequests>,
        mut closed: watch::Receiver<bool>,
    ) {
        loop {
            let turn = tokio::select! {
                biased;
                _ = down(&mut closed), if !turns.is_closed() => {
                    turns.close();
                    continue;
                }
                turn = turns.recv() => turn,
            };
            let Some(turn) = turn else { return };
            let source = &self.opts.usage_sources;
            let account = if runtime == Runtime::Claude {
                let login = claude_account(source).await;
                let account = login
                    .as_ref()
                    .map(|login| login.account.clone())
                    .map_err(Clone::clone);
                collection.request(CollectRequest::Claude(login));
                account
            } else {
                fingerprint(source, runtime).await
            };
            match account {
                Ok(account) => self.publish_refusal(runtime, turn, account).await,
                Err(e) => {
                    tracing::warn!(%runtime, seat = %turn.seat, "resolving the account of a quota refusal: {e}")
                }
            }
        }
    }

    async fn publish_refusal(&self, runtime: Runtime, turn: RefusedTurn, account: String) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_turns_coalesces_and_collection_moves_the_periodic_deadline() {
        let requests = Arc::new(CollectionRequests::default());
        let (started, mut probes) = mpsc::unbounded_channel();
        let periodic = Duration::from_secs(15 * 60);
        let minimum = Duration::from_secs(60);
        let worker = tokio::spawn({
            let requests = requests.clone();
            async move {
                requests
                    .run(periodic, minimum, |_| async {
                        let (release, end) = oneshot::channel();
                        started.send((Instant::now(), release)).unwrap();
                        end.await.unwrap();
                    })
                    .await
            }
        });
        let start = Instant::now();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(10 * 60)).await;
        requests.request(CollectRequest::FinishedTurn);
        let (first, release) = probes.recv().await.unwrap();
        assert_eq!(first - start, Duration::from_secs(10 * 60));

        for _ in 0..16 {
            requests.request(CollectRequest::FinishedTurn);
        }
        tokio::time::advance(Duration::from_secs(30)).await;
        let first_ended = Instant::now();
        release.send(()).unwrap();
        tokio::task::yield_now().await;
        for _ in 0..16 {
            requests.request(CollectRequest::FinishedTurn);
        }
        let (second, release) = probes.recv().await.unwrap();
        assert_eq!(second - first_ended, minimum);
        release.send(()).unwrap();

        let (third, _release) = probes.recv().await.unwrap();
        assert_eq!(
            third - second,
            periodic,
            "the next probe is the reset timer"
        );
        worker.abort();
    }
}
