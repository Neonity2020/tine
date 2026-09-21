//! Read-only observers of the projection worker's queue: how far a fresh build
//! has got, and whether queued page deltas have drained. Neither changes what
//! the worker does.

use std::sync::atomic::Ordering;

use super::DirectProjection;

impl DirectProjection {
    /// The running fresh build's page count, for the progress bar only.
    pub(crate) fn build_progress(&self) -> Option<crate::indexing_progress::IndexingProgress> {
        self.shared.build_progress.snapshot()
    }

    /// Wait, at most `limit`, until no page delta is queued, so a warm whose
    /// read already covers those publications can be offered to the queue
    /// without reading the graph again (GH #543). False when deltas remain
    /// (a turn that owes a full inventory keeps them) or the worker is gone
    /// or failed.
    pub(crate) fn wait_for_queued_deltas(&self, limit: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + limit;
        let mut pending = self.shared.pending.lock().unwrap();
        loop {
            if !self.shared.worker_available.load(Ordering::Acquire)
                || self.shared.worker_failed.load(Ordering::Acquire)
            {
                return false;
            }
            if pending.deltas.is_empty() {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            pending = self
                .shared
                .changed
                .wait_timeout(pending, deadline - now)
                .unwrap()
                .0;
        }
    }
}
