//! The bus as the bridge sees it: a small seam over `hgl_ai_bus`, so the seat loops run
//! against an in-memory fake in tests.

use std::future::Future;
use std::time::Duration;

use hgl_ai_bus::Result;
use hgl_ai_proto::{HostRecord, Note, Seat};
use hgl_ai_tree::Registry;

pub trait BusApi: Registry + Send + Sync + 'static {
    type Inbox: InboxApi<Delivery = Self::Delivery>;
    type Delivery: DeliveryApi;

    fn inbox(&self, seat: &Seat) -> impl Future<Output = Result<Self::Inbox>> + Send;
    fn send(&self, note: &Note) -> impl Future<Output = Result<()>> + Send;
    fn state_get(&self, key: &str) -> impl Future<Output = Result<Option<String>>> + Send;
    fn host_put(&self, rec: &HostRecord) -> impl Future<Output = Result<()>> + Send;
    fn host_del(&self, host: &str) -> impl Future<Output = Result<()>> + Send;
    /// The live host whose bridge serves `seat` (PROTO-6).
    fn bridge_host(&self, seat: &Seat) -> impl Future<Output = Result<Option<String>>> + Send;
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
    fn in_progress(&self) -> impl Future<Output = Result<()>> + Send;
}

impl BusApi for hgl_ai_bus::Bus {
    type Inbox = hgl_ai_bus::Inbox;
    type Delivery = hgl_ai_bus::Delivery;

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
    async fn in_progress(&self) -> Result<()> {
        hgl_ai_bus::Delivery::in_progress(self).await
    }
}
