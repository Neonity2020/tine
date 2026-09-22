//! The index's one decider (GH #543): what whole-graph work the index
//! needs, whether any is coming, and the registration of the owner that runs
//! it. Everything here is read and changed under `pending`.

use super::*;

/// What whole-graph work the index needs next; see [`index_need`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexNeed {
    /// The worker has not yet opened its stored image.
    SettingUp,
    /// The worker is gone for good. Nothing enqueued is ever taken.
    Terminal,
    /// Another writer holds the index database's lease. The worker takes it
    /// when that writer lets go (or on a backoff when it is another process);
    /// nothing is coming meanwhile, so readers take their ordinary route.
    LeaseWait,
    /// A full snapshot or a warm validation is queued or being applied.
    InHand,
    /// Only a complete parsed snapshot can make the index ready: there is no
    /// usable image, or a read found the image damaged. Walking the graph to
    /// validate the image first would only be read again.
    Fresh,
    /// The image may be good but has not been checked against the pages this
    /// session, or an unnamed deletion may have left it describing pages
    /// that are gone.
    Validate,
    /// Nothing whole-graph is owed: page updates keep the image current.
    Nothing,
}

/// The one answer to "what whole-graph work does the index need?".
///
/// Computed on read, under the `pending` lock, from state that only ever
/// changes under that lock, so it cannot disagree with its inputs or lag an
/// enqueue: a queued full snapshot reads as `InHand` the moment it is queued.
/// The order of the tests is the order of authority.
pub(super) fn index_need(shared: &ProjectionShared, pending: &PendingProjection) -> IndexNeed {
    if pending.stop || !shared.worker_available.load(Ordering::Acquire) {
        IndexNeed::Terminal
    } else if pending.lease_wait {
        IndexNeed::LeaseWait
    } else if !pending.set_up {
        IndexNeed::SettingUp
    } else if pending.full.is_some() || pending.warm.is_some() || pending.building {
        IndexNeed::InHand
    } else if pending.requires_full_rebuild || pending.rebuild {
        IndexNeed::Fresh
    } else if !shared.validated.load(Ordering::Acquire) || pending.stale {
        IndexNeed::Validate
    } else {
        IndexNeed::Nothing
    }
}

/// Whether a fresh build already owns this image's replacement: one is
/// running, or a rebuild is queued with the payload that carries it. A
/// read that failed on the current image owes nothing more then -- the
/// build replaces that image whole -- and a second request would queue a
/// second complete build behind it (GH #543, indexing audit IT-10).
pub(super) fn fresh_build_owns_image(
    shared: &ProjectionShared,
    pending: &PendingProjection,
) -> bool {
    shared.build_progress.snapshot().is_some() || (pending.rebuild && pending.full.is_some())
}

/// Whether the owner is waiting out a backoff after passes or turns that did
/// not make the index ready.
pub(super) fn backing_off(pending: &PendingProjection) -> bool {
    pending
        .retry_after
        .is_some_and(|retry_after| std::time::Instant::now() < retry_after)
}

/// Record a whole-graph pass or worker turn that did not make the index
/// ready. The first one is retried at once: a launch check that a rename
/// raced is ordinary, and while a backoff holds, nothing is coming, so every
/// reader falls back to parsing the graph itself. From the second on, the
/// next owner pass waits `1 s · 2^(n−2)`, at most 30 minutes, so a
/// deterministic failure (a page that always panics, a directory that stays
/// unreadable) costs a bounded number of whole-graph passes instead of one
/// after another. New facts do not cut the wait short: that would make every
/// save a rebuild trigger while the failure lasts.
pub(super) fn note_unsettled(pending: &mut PendingProjection) {
    pending.unsettled_passes = pending.unsettled_passes.saturating_add(1);
    if pending.unsettled_passes == 1 {
        projection_diag(|| "unsettled pass 1; retrying at once".to_owned());
        return;
    }
    let exponent = (pending.unsettled_passes - 2).min(11);
    let wait = std::time::Duration::from_secs(1u64 << exponent)
        .min(std::time::Duration::from_secs(30 * 60));
    pending.retry_after = Some(std::time::Instant::now() + wait);
    let passes = pending.unsettled_passes;
    projection_diag(|| format!("unsettled pass {passes}; next owner pass in {wait:?}"));
}

/// Whether whole-graph index work is coming: an owner is registered and the
/// index needs, or is being given, work it will take. Readers wait while this
/// holds; when it does not (no owner, a backoff, a gone worker) they take
/// their ordinary route. One predicate, read under `pending`, for every
/// consumer (GH #543).
pub(super) fn index_work_coming(shared: &ProjectionShared, pending: &PendingProjection) -> bool {
    if pending.owners == 0 {
        return false;
    }
    let need = index_need(shared, pending);
    match need {
        IndexNeed::Terminal | IndexNeed::LeaseWait => false,
        IndexNeed::SettingUp | IndexNeed::InHand => true,
        IndexNeed::Validate | IndexNeed::Fresh if !backing_off(pending) => true,
        _ => {
            pending.has_work()
                || shared.deltas_coming.load(Ordering::Acquire) > 0
                || shared.worker_busy.load(Ordering::Acquire)
        }
    }
}

/// What an index owner does next; see [`DirectProjection::wait_owner_step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnerStep {
    Cancelled,
    /// The worker is gone for good.
    Terminal,
    /// Nothing is coming any more: the launch completion owed is due.
    Settle,
    /// Run a whole-graph pass: `Validate` or `Fresh`.
    Pass(IndexNeed),
}

impl DirectProjection {
    /// Register an index owner. Taken before the graph is published, so a
    /// reader reaching it sees index work coming and waits for it instead of
    /// parsing the whole graph itself (GH #543).
    pub(crate) fn register_owner(&self) -> IndexOwnerRegistration {
        self.shared.pending.lock().unwrap().owners += 1;
        self.shared.changed.notify_all();
        IndexOwnerRegistration(Arc::clone(&self.shared))
    }

    /// Whether an index owner is registered: whole-graph index work is then
    /// its to start, and a reader or a failed query only reports the need.
    pub(crate) fn owner_registered(&self) -> bool {
        self.shared.pending.lock().unwrap().owners > 0
    }

    /// See [`index_work_coming`].
    pub(crate) fn coming(&self) -> bool {
        index_work_coming(&self.shared, &self.shared.pending.lock().unwrap())
    }

    /// The owner loop's wait: returns when there is a pass to run and no
    /// backoff holds it, when the launch completion is due (`settle_owed` and
    /// nothing coming), when the worker is gone, or when `cancelled`. Every
    /// input it reads changes under `pending` and notifies `changed`; the
    /// timeout only bounds how late it notices `cancelled` and a backoff's
    /// end.
    pub(crate) fn wait_owner_step(
        &self,
        settle_owed: bool,
        cancelled: &impl Fn() -> bool,
    ) -> OwnerStep {
        let mut pending = self.shared.pending.lock().unwrap();
        loop {
            if cancelled() {
                return OwnerStep::Cancelled;
            }
            let need = index_need(&self.shared, &pending);
            if need == IndexNeed::Terminal {
                return OwnerStep::Terminal;
            }
            if settle_owed && !index_work_coming(&self.shared, &pending) {
                return OwnerStep::Settle;
            }
            let backing_off = backing_off(&pending);
            if matches!(need, IndexNeed::Validate | IndexNeed::Fresh) && !backing_off {
                return OwnerStep::Pass(need);
            }
            let mut wait = std::time::Duration::from_millis(500);
            if backing_off {
                if let Some(retry_after) = pending.retry_after {
                    wait =
                        wait.min(retry_after.saturating_duration_since(std::time::Instant::now()));
                }
            }
            pending = self
                .shared
                .changed
                .wait_timeout(pending, wait.max(std::time::Duration::from_millis(1)))
                .unwrap()
                .0;
        }
    }

    /// The need right now, for an owner re-checking it under its permit.
    pub(crate) fn index_need_now(&self) -> (IndexNeed, bool) {
        let pending = self.shared.pending.lock().unwrap();
        (index_need(&self.shared, &pending), backing_off(&pending))
    }

    /// Record an owner pass that ended without the worker taking a payload
    /// that could make the index ready; see [`note_unsettled`].
    pub(crate) fn note_unsettled_pass(&self) {
        note_unsettled(&mut self.shared.pending.lock().unwrap());
        self.shared.changed.notify_all();
    }

    /// Whether the owner is waiting out a backoff; the progress bar is not
    /// shown for it.
    pub(crate) fn backing_off(&self) -> bool {
        backing_off(&self.shared.pending.lock().unwrap())
    }

    /// Ask for the image to be replaced whole. A no-op when a fresh build
    /// already owns its replacement (IT-10): the rule is checked under the
    /// same lock as the request, so two failed reads cannot both see "no
    /// build yet" and queue two.
    pub(crate) fn request_rebuild(&self) {
        let mut pending = self.shared.pending.lock().unwrap();
        if fresh_build_owns_image(&self.shared, &pending) {
            return;
        }
        pending.rebuild = true;
        self.shared.ready.store(false, Ordering::Release);
        drop(pending);
        self.shared.changed.notify_all();
    }

    /// What whole-graph work the index needs next, once the worker has
    /// opened its stored image (see [`index_need`]). Waits while it is
    /// still opening it; `SettingUp` is returned only when `cancelled`.
    pub(crate) fn wait_index_need(&self, cancelled: &impl Fn() -> bool) -> IndexNeed {
        let mut pending = self.shared.pending.lock().unwrap();
        loop {
            let need = index_need(&self.shared, &pending);
            if need != IndexNeed::SettingUp || cancelled() {
                return need;
            }
            pending = self
                .shared
                .changed
                .wait_timeout(pending, std::time::Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }
}

/// A registered index owner; see `DirectProjection::register_owner`.
pub(crate) struct IndexOwnerRegistration(Arc<ProjectionShared>);

impl Drop for IndexOwnerRegistration {
    fn drop(&mut self) {
        self.0.pending.lock().unwrap().owners -= 1;
        self.0.changed.notify_all();
    }
}
