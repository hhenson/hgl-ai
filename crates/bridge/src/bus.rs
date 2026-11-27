//! The bus as the bridge sees it: a small seam over `hgl_ai_bus`, so the seat loops run
//! against an in-memory fake in tests.

use std::future::Future;
use std::time::Duration;

use hgl_ai_bus::Result;
use hgl_ai_proto::{HostRecord, Note, Seat, TaskKey};
use hgl_ai_tree::Registry;

pub trait BusApi: Registry + Send + Sync + 'static {
    type Inbox: InboxApi<Delivery = Self::Delivery>;
    type Delivery: DeliveryApi;
    type Lease: LeaseApi;

    fn inbox(&self, seat: &Seat) -> impl Future<Output = Result<Self::Inbox>> + Send;
    fn send(&self, note: &Note) -> impl Future<Output = Result<()>> + Send;
    fn state_get(&self, key: &str) -> impl Future<Output = Result<Option<String>>> + Send;
    fn host_put(&self, rec: &HostRecord) -> impl Future<Output = Result<()>> + Send;
    fn host_del(&self, host: &str) -> impl Future<Output = Result<()>> + Send;
    /// The live host whose bridge serves `seat` (PROTO-6).
    fn bridge_host(&self, seat: &Seat) -> impl Future<Output = Result<Option<String>>> + Send;
    /// Take the lease `name` in `HGL_LOCKS` (create-if-absent; `BusError::Held` when held).
    fn lock_acquire(
        &self,
        name: &str,
        holder: &str,
    ) -> impl Future<Output = Result<Self::Lease>> + Send;
    /// Take over the lease `name` from `stale`, a holder known to be dead.
    fn lock_take_over(
        &self,
        name: &str,
        holder: &str,
        stale: &str,
    ) -> impl Future<Output = Result<Self::Lease>> + Send;
    /// The stream sequence of the note that ended `task` for `seat` (`HGL_ENDED`), if any.
    fn ended_get(
        &self,
        seat: &Seat,
        task: &TaskKey,
    ) -> impl Future<Output = Result<Option<u64>>> + Send;
    /// Record that `task` ended for `seat` at `seq`; a later end already recorded stands.
    fn ended_put(
        &self,
        seat: &Seat,
        task: &TaskKey,
        seq: u64,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// A held lease: refreshed at its revision (`BusError::LeaseLost` once it is not ours),
/// released when done.
pub trait LeaseApi: Send + Sync + 'static {
    fn refresh(&mut self) -> impl Future<Output = Result<()>> + Send;
    /// `false`: it was no longer ours, so nothing was deleted.
    fn release(self) -> impl Future<Output = Result<bool>> + Send;
}

pub trait InboxApi: Send + 'static {
    type Delivery: DeliveryApi;
    fn fetch(
        &mut self,
        max: usize,
        wait: Duration,
    ) -> impl Future<Output = Result<Vec<Self::Delivery>>> + Send;
}

pub trait DeliveryApi: Send + Sync + 'static {
    fn note(&self) -> &Note;
    fn seq(&self) -> u64;
    fn delivered(&self) -> u64;
    fn ack(&self) -> impl Future<Output = Result<()>> + Send;
    fn nak(&self, delay: Duration) -> impl Future<Output = Result<()>> + Send;
    /// Stop redelivery without acknowledging: a note that can never be handled.
    fn term(&self) -> impl Future<Output = Result<()>> + Send;
    fn in_progress(&self) -> impl Future<Output = Result<()>> + Send;
}

impl BusApi for hgl_ai_bus::Bus {
    type Inbox = hgl_ai_bus::Inbox;
    type Delivery = hgl_ai_bus::Delivery;
    type Lease = hgl_ai_bus::Lease;

    async fn inbox(&self, seat: &Seat) -> Result<Self::Inbox> {
        hgl_ai_bus::Bus::inbox(self, seat).await
    }
    async fn send(&self, note: &Note) -> Result<()> {
        hgl_ai_bus::Bus::send(self, note).await.map(|_| ())
    }
    async fn state_get(&self, key: &str) -> Result<Option<String>> {
        Ok(hgl_ai_bus::Bus::state_get(self, key)
            .await?
            .map(|e| e.value))
    }
    async fn host_put(&self, rec: &HostRecord) -> Result<()> {
        hgl_ai_bus::Bus::host_put(self, rec).await
    }
    async fn host_del(&self, host: &str) -> Result<()> {
        hgl_ai_bus::Bus::host_del(self, host).await
    }
    async fn bridge_host(&self, seat: &Seat) -> Result<Option<String>> {
        hgl_ai_bus::Bus::bridge_host(self, seat).await
    }
    async fn lock_acquire(&self, name: &str, holder: &str) -> Result<Self::Lease> {
        hgl_ai_bus::Bus::lock_acquire(self, name, holder).await
    }
    async fn lock_take_over(&self, name: &str, holder: &str, stale: &str) -> Result<Self::Lease> {
        hgl_ai_bus::Bus::lock_take_over(self, name, holder, stale).await
    }
    async fn ended_get(&self, seat: &Seat, task: &TaskKey) -> Result<Option<u64>> {
        Ok(hgl_ai_bus::Bus::ended_get(self, seat, task)
            .await?
            .map(|r| r.seq))
    }
    async fn ended_put(&self, seat: &Seat, task: &TaskKey, seq: u64) -> Result<()> {
        hgl_ai_bus::Bus::ended_put(self, seat, task, seq)
            .await
            .map(drop)
    }
}

impl LeaseApi for hgl_ai_bus::Lease {
    async fn refresh(&mut self) -> Result<()> {
        hgl_ai_bus::Lease::refresh(self).await
    }
    async fn release(self) -> Result<bool> {
        hgl_ai_bus::Lease::release(self).await
    }
}

impl InboxApi for hgl_ai_bus::Inbox {
    type Delivery = hgl_ai_bus::Delivery;
    async fn fetch(&mut self, max: usize, wait: Duration) -> Result<Vec<Self::Delivery>> {
        hgl_ai_bus::Inbox::fetch(self, max, wait).await
    }
}

impl DeliveryApi for hgl_ai_bus::Delivery {
    fn note(&self) -> &Note {
        &self.note
    }
    fn seq(&self) -> u64 {
        self.seq
    }
    fn delivered(&self) -> u64 {
        self.delivered
    }
    async fn ack(&self) -> Result<()> {
        hgl_ai_bus::Delivery::ack(self).await
    }
    async fn nak(&self, delay: Duration) -> Result<()> {
        hgl_ai_bus::Delivery::nak(self, delay).await
    }
    async fn term(&self) -> Result<()> {
        hgl_ai_bus::Delivery::term(self).await
    }
    async fn in_progress(&self) -> Result<()> {
        hgl_ai_bus::Delivery::in_progress(self).await
    }
}
