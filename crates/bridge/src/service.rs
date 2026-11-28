//! Host-level duties: heartbeat (§9), the seat leases' refresh, the `assign` gate (ROLE-18, ROLE-19), the daily
//! report of stale trees and disk use (PROTO-15), and the cleanup receipt's text.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use hgl_ai_proto::{HostRecord, Kind, Note, Seat, SeatRecord, TaskKey};
use hgl_ai_tree::stale::{self, human_age, human_bytes};
use hgl_ai_tree::{IsClosed, Removed};
use tokio::sync::watch;

use crate::bus::BusApi;
use crate::disk::GB;
use crate::{Bridge, Gate, down, lock};

impl<B: BusApi> Bridge<B> {
    /// Notes about the host come from the host label (a valid seat name, served by no one).
    fn host_seat(&self) -> Seat {
        Seat::new(self.cfg.host.clone()).unwrap_or_else(|_| Seat::broadcast())
    }

    fn free_gb(&self) -> Option<u64> {
        match self.deps.disk.free_bytes(self.trees.layout().root()) {
            Ok(b) => Some(b / GB),
            Err(e) => {
                tracing::warn!("free space of work_root: {e}");
                None
            }
        }
    }

    /// The gate for notes that make a new tree (ROLE-18, ROLE-19), telling the controller
    /// when it changes.
    pub(crate) async fn gate_check(&self) -> Gate {
        let mode = self.bus.state_get("mode").await.ok().flatten();
        let free = self.free_gb();
        let min = self.cfg.limits.min_free_gb;
        // Low disk first: it holds every new tree, drain only `assign` notes, so drain must
        // not hide it.
        let g = if free.is_some_and(|f| f < min) {
            Gate::LowDisk
        } else if mode.as_deref().map(str::trim) == Some("drain") {
            Gate::Drain
        } else {
            Gate::Open
        };
        let changed = {
            let mut last = lock(&self.gate);
            std::mem::replace(&mut *last, g) != g
        };
        if changed {
            let host = &self.cfg.host;
            let body = match g {
                Gate::Drain => format!(
                    "[bridge] {host}: mode is drain; not taking `assign` notes (held until it opens)."
                ),
                Gate::LowDisk => format!(
                    "[bridge] {host}: work_root has {} GB free, below min_free_gb {min}; making no new tree (`assign`, `review`, a moved task's tree; held until it opens).",
                    free.unwrap_or(0)
                ),
                Gate::Open => format!("[bridge] {host}: taking new work again."),
            };
            tracing::info!(?g, "assign gate changed");
            self.tell(&self.host_seat(), Kind::Note, None, body).await;
        }
        g
    }

    /// Write `HGL_HOSTS/<host>` with the seats served now (§9).
    pub(crate) async fn beat(&self) {
        let seats: Vec<SeatRecord> = {
            let served = lock(&self.served);
            served
                .values()
                .filter_map(|s| {
                    let r = self.cfg.resolve(&s.current().1).ok()?;
                    Some(SeatRecord {
                        seat: s.name().clone(),
                        role: s.cfg.role,
                        runtime: r.runtime,
                        family: r.family,
                        model: r.model,
                    })
                })
                .collect()
        };
        let rec = HostRecord {
            host: self.cfg.host.clone(),
            seats,
            memcap_gb: self.cfg.limits.memcap_gb,
            work_root_free_gb: self.free_gb().unwrap_or(0),
            updated_at: hgl_ai_proto::utc_now_stamp(),
        };
        if let Err(e) = self.bus.host_put(&rec).await {
            tracing::warn!("heartbeat: {e}");
        }
    }

    pub(crate) async fn heartbeat_loop(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        loop {
            self.beat().await;
            self.gate_check().await;
            tokio::select! {
                _ = tokio::time::sleep(self.opts.heartbeat_every) => {}
                _ = down(&mut shutdown) => return,
            }
        }
    }

    /// The seat leases' refresh, on its own: other hub calls that hang in a partition do
    /// not delay it past its deadline (PROTO-6).
    pub(crate) async fn lease_loop(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        loop {
            self.refresh_leases().await;
            tokio::select! {
                _ = tokio::time::sleep(self.lease_every()) => {}
                _ = down(&mut shutdown) => return,
            }
        }
    }

    pub(crate) async fn report_loop(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        loop {
            self.report().await;
            tokio::select! {
                _ = tokio::time::sleep(self.opts.report_every) => {}
                _ = down(&mut shutdown) => return,
            }
        }
    }

    /// Measure the trees, refresh their records, and post the stale ones and disk use.
    pub(crate) async fn report(&self) {
        let hold = self.bus.state_get("hold").await.ok().flatten();
        let trees = self.trees.clone();
        let closed: Arc<dyn IsClosed + Send + Sync> = if hold.is_some() {
            Arc::new(stale::Unknown)
        } else {
            self.deps.closed.clone()
        };
        let after = self.cfg.stale_after();
        let job = tokio::task::spawn_blocking(move || {
            let list = trees.list()?;
            let records: Vec<_> = list.iter().map(|t| t.record.clone()).collect();
            let stale = stale::stale(&records, chrono::Utc::now(), after, &*closed);
            let used = hgl_ai_tree::dir_size(trees.layout().root());
            Ok::<_, hgl_ai_tree::TreeError>((list, stale, used))
        });
        let (list, stale, used) = match job.await {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => return tracing::warn!("listing trees: {e}"),
            Err(e) => return tracing::warn!("listing trees: {e}"),
        };
        // A merge, not a put: a turn may have registered a fresher record since the
        // listing (hgl-ai#51).
        for t in list.iter().filter(|t| t.file.is_some()) {
            if let Err(e) = self.bus.tree_merge(&t.record).await {
                tracing::warn!(key = %t.record.key(), "registering: {e}");
            }
        }
        let host = &self.cfg.host;
        let mut body = format!(
            "[bridge] {host}: work_root uses {}, {} GB free (min {}); {} trees.\n",
            human_bytes(used),
            self.free_gb().map_or("?".into(), |f| f.to_string()),
            self.cfg.limits.min_free_gb,
            list.len()
        );
        if stale.is_empty() {
            body.push_str("Stale: none.\n");
        } else {
            let _ = writeln!(
                body,
                "Stale (older than {} or closed{}):",
                self.cfg.limits.stale_after,
                if hold.is_some() {
                    "; closed-ness not checked during the hold"
                } else {
                    ""
                }
            );
            for s in &stale {
                let _ = writeln!(
                    body,
                    "- {} age {} size {} {} {}",
                    s.record.key(),
                    s.age.map(human_age).unwrap_or_else(|| "?".into()),
                    human_bytes(s.record.size_bytes),
                    s.reason(),
                    s.record.path
                );
            }
        }
        self.tell(&self.host_seat(), Kind::Note, None, body).await;
    }
}

/// The `done` cleanup receipt (PROTO-14).
#[allow(clippy::too_many_arguments)]
pub(crate) fn cleanup_body(
    host: &str,
    seat: &Seat,
    note: &Note,
    task: &TaskKey,
    removed: &[Removed],
    stopped: bool,
    errors: &[String],
    root: &Path,
) -> String {
    let mut s = format!(
        "[bridge] Cleanup: {}#{} on {host}/{seat} after `{}`: ",
        task.repo, task.issue, note.kind
    );
    if removed.is_empty() {
        s.push_str("no tree removed");
    } else {
        let names: Vec<String> = removed
            .iter()
            .map(|r| {
                r.dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        let freed: u64 = removed.iter().map(|r| r.freed_bytes).sum();
        let _ = write!(
            s,
            "removed {}, freed {}",
            names.join(", "),
            human_bytes(freed)
        );
        let branches: Vec<&str> = removed.iter().filter_map(|r| r.branch.as_deref()).collect();
        if !branches.is_empty() {
            let _ = write!(s, ", deleted local branch {}", branches.join(", "));
        }
        let salvage: Vec<String> = removed
            .iter()
            .filter_map(|r| r.salvage.as_ref())
            .map(|p| p.strip_prefix(root).unwrap_or(p).display().to_string())
            .collect();
        if salvage.is_empty() {
            s.push_str("; salvage: none");
        } else {
            let _ = write!(s, "; salvage: {} (under work_root)", salvage.join(", "));
        }
    }
    if stopped {
        s.push_str("; stopped the running turn");
    }
    if !errors.is_empty() {
        let _ = write!(s, "; errors: {}", errors.join("; "));
    }
    s.push('.');
    s
}
