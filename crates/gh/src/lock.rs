//! The landing lock: one landing (or release watch) per repository at a time, on any host.
//! A lease on the hub, `HGL_LOCKS/lock.land.<repo>` (docs/protocol.md §3, §8): taken by
//! create-if-absent, refreshed every [`REFRESH`] while held, released when the work is done.
//! A holder that dies stops refreshing, and the lease expires after the bucket's `max_age`.

use std::time::{Duration, Instant};

use hgl_ai_bus::{Bus, BusError, Lease};
use hgl_ai_proto::land_lock_key;
use tokio::sync::oneshot;
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
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
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
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(keep(lease, refresh, stopped));
        Ok(LandLock {
            name: name.to_string(),
            stop: Some(stop),
            task: Some(task),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
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

/// Refresh `lease` every `every` until told to stop (or the guard is dropped), then
/// release it. A lost lease is reported and no longer refreshed.
async fn keep(mut lease: Lease, every: Duration, mut stopped: oneshot::Receiver<()>) {
    let mut lost = false;
    loop {
        tokio::select! {
            _ = &mut stopped => break,
            _ = tokio::time::sleep(every), if !lost => match lease.refresh().await {
                Ok(()) => {}
                Err(e @ BusError::LeaseLost { .. }) => {
                    eprintln!("warning: {e}; another landing may start");
                    lost = true;
                }
                Err(e) => eprintln!("warning: refreshing {}: {e}; trying again", lease.name()),
            },
        }
    }
    if lost {
        return;
    }
    let name = lease.name().to_string();
    match lease.release().await {
        Ok(true) => {}
        Ok(false) => eprintln!("warning: {name} was no longer ours at release"),
        Err(e) => eprintln!("warning: releasing {name}: {e}; it expires on its own"),
    }
}
