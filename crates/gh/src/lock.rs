//! The landing lock: one landing (or release watch) per repository at a time, on any host.
//! A lease on the hub, `HGL_LOCKS/lock.land.<repo>` (docs/protocol.md §3, §8): taken by
//! create-if-absent, refreshed every [`REFRESH`] while held, released when the work is done.
//! A holder that dies stops refreshing, and the lease expires after the bucket's `max_age`.
//! Before each step the lock protects (the merge, `landed.log`, `main`) the holder confirms
//! the lease is still its own ([`LandLock::confirm`]) and stops when it is not.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hgl_ai_bus::{Bus, BusError, Lease};
use hgl_ai_proto::land_lock_key;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use crate::{Error, Result, Settings};

/// How often a held lease is written again; well within the bucket's `max_age`.
pub const REFRESH: Duration = Duration::from_secs(60);
/// Between attempts while another holder has it.
const POLL: Duration = Duration::from_secs(2);

/// A held landing lock. Call [`LandLock::release`] when done; dropping it releases the
/// lease in the background, which a process about to exit may not finish.
pub struct LandLock {
    name: String,
    shared: Arc<Shared>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

/// The lease, shared by the holder's checks and the background refresher.
struct Shared {
    lease: Mutex<Option<Lease>>,
    /// A refresh found the lease no longer ours.
    lost: AtomicBool,
}

impl Shared {
    /// Refresh at our revision; `Ok(false)` when the lease is no longer ours.
    async fn refresh(&self) -> Result<bool> {
        if self.lost.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let mut lease = self.lease.lock().await;
        let Some(l) = lease.as_mut() else {
            return Ok(false);
        };
        match l.refresh().await {
            Ok(()) => Ok(true),
            Err(BusError::LeaseLost { .. }) => {
                self.lost.store(true, Ordering::SeqCst);
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }
}

/// `<host> pid <pid> (<what>)`: who holds a lock, as a refused acquirer is told.
pub fn holder(s: &Settings, what: &str) -> String {
    format!("{} pid {} ({what})", s.host, std::process::id())
}

impl LandLock {
    /// Take `lock.land.<repo>`, waiting at most `wait` while another holder has it.
    pub async fn acquire(bus: &Bus, repo: &str, holder: &str, wait: Duration) -> Result<LandLock> {
        Self::acquire_every(bus, &land_lock_key(repo), holder, wait, REFRESH).await
    }

    /// Take a named hub lease with the same renewal and ownership checks.
    pub async fn acquire_named(
        bus: &Bus,
        name: &str,
        holder: &str,
        wait: Duration,
    ) -> Result<LandLock> {
        Self::acquire_every(bus, name, holder, wait, REFRESH).await
    }

    pub(crate) async fn acquire_every(
        bus: &Bus,
        name: &str,
        holder: &str,
        wait: Duration,
        refresh: Duration,
    ) -> Result<LandLock> {
        let start = Instant::now();
        let mut told = false;
        let lease = loop {
            match bus.lock_acquire(name, holder).await {
                Ok(l) => break l,
                Err(e @ BusError::Held { .. }) => {
                    if start.elapsed() + POLL > wait {
                        return Err(Error::Locked(format!("{e} (waited {wait:?})")));
                    }
                    if !told {
                        eprintln!("{e}; waiting up to {wait:?}");
                        told = true;
                    }
                    tokio::time::sleep(POLL).await;
                }
                Err(e) => return Err(e.into()),
            }
        };
        let shared = Arc::new(Shared {
            lease: Mutex::new(Some(lease)),
            lost: AtomicBool::new(false),
        });
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(keep(shared.clone(), refresh, stopped));
        Ok(LandLock {
            name: name.to_string(),
            shared,
            stop: Some(stop),
            task: Some(task),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Confirm, by a compare-and-set refresh on the hub, that the lease is still ours;
    /// called right before each step the lock protects. Fails with [`Error::LeaseLost`],
    /// saying the step was not taken, when it expired or another holder took it.
    pub async fn confirm(&self, step: &str) -> Result<()> {
        if self.shared.refresh().await? {
            return Ok(());
        }
        Err(Error::LeaseLost(format!(
            "the landing lock {} is no longer ours (it expired or another holder took it); stopped before {step}",
            self.name
        )))
    }

    /// Stop refreshing and delete the lease if it is still ours; returns once the hub has
    /// answered.
    pub async fn release(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for LandLock {
    fn drop(&mut self) {
        // Dropping the sender wakes the refresher, which then releases the lease.
        self.stop.take();
    }
}

/// Refresh the lease every `every` until told to stop (or the guard is dropped), then
/// release it. A lost lease is reported and no longer refreshed; the holder's next
/// [`LandLock::confirm`] stops the landing.
async fn keep(shared: Arc<Shared>, every: Duration, mut stopped: oneshot::Receiver<()>) {
    let mut lost = false;
    loop {
        tokio::select! {
            _ = &mut stopped => break,
            _ = tokio::time::sleep(every), if !lost => match shared.refresh().await {
                Ok(true) => {}
                Ok(false) => {
                    eprintln!("warning: the landing lease is no longer ours; the landing stops before its next step");
                    lost = true;
                }
                Err(e) => eprintln!("warning: refreshing the landing lease: {e}; trying again"),
            },
        }
    }
    let Some(lease) = shared.lease.lock().await.take() else {
        return;
    };
    if lost || shared.lost.load(Ordering::SeqCst) {
        return;
    }
    let name = lease.name().to_string();
    match lease.release().await {
        Ok(true) => {}
        Ok(false) => eprintln!("warning: {name} was no longer ours at release"),
        Err(e) => eprintln!("warning: releasing {name}: {e}; it expires on its own"),
    }
}
