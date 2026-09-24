//! The index's one decider (GH #543): what whole-graph work the index
//! needs, whether any is coming, and the registration of the owner that runs
//! it. Everything here is read and changed under `pending`.

use super::*;
use crate::query::IndexFailureClass;

/// What whole-graph work the index needs next; see [`index_need`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexNeed {
    /// The worker has not yet opened its stored image.
    SettingUp,
    /// The index could not be built or updated in [`INDEX_ATTEMPTS`]
    /// consecutive attempts and has stopped trying for this session (GH #594,
    /// index liveness L1). Only the user's retry or the next launch starts
    /// it again: nothing is coming, and readers answer `IndexFailed`.
    Failed,
    /// The worker is gone for good. Nothing enqueued is ever taken.
    Terminal,
    /// Another process holds the index database's lease (or a writer in
    /// this process has held it longer than a retired one would): the worker
    /// retries on a backoff, and nothing is coming meanwhile, so readers take
    /// their ordinary route. A retired predecessor in this process that is
    /// letting go reads as `SettingUp` (see `direct_projection_lease`).
    LeaseWait,
    /// A fresh snapshot is queued or being built.
    InHand,
    /// Only a complete parsed snapshot can make the index ready: there is no
    /// usable image, or a read found the image damaged. Walking the graph to
    /// validate the image first would only be read again.
    Fresh,
    /// The image may be good but has not been surveyed against the pages
    /// this session.
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
    } else if pending.failed.is_some() {
        IndexNeed::Failed
    } else if pending.full.is_some() || pending.building {
        IndexNeed::InHand
    } else if pending.rebuild {
        IndexNeed::Fresh
    } else if !shared.validated.load(Ordering::Acquire) {
        IndexNeed::Validate
    } else {
        IndexNeed::Nothing
    }
}

/// A failure the index met, as evidence for [`failure_owes_new_image`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexFailure {
    /// SQLite refused a read statement.
    StatementRefused,
    /// A read found rows that contradict each other (`InvalidSnapshot`).
    ContradictoryRows,
    /// A worker turn failed and rolled back.
    TurnFailed,
}

impl IndexFailure {
    /// The failure a worker turn that failed with `message` met. A constraint
    /// the turn's writes violated is rows contradicting what lowering writes
    /// -- orphans only a Tine defect leaves, which `quick_check` passes -- so
    /// it is `ContradictoryRows`; as `TurnFailed` it re-failed every retry and
    /// the index never recovered. tine-storage carries SQLite's error as text
    /// (`MaterializationError::Sqlite`), so this reads SQLite's constraint
    /// message; `a_constraint_violation_reads_as_contradictory_rows` pins it
    /// against rusqlite's real output.
    pub(crate) fn of_turn(message: &str) -> Self {
        if message.contains(" constraint failed") {
            Self::ContradictoryRows
        } else {
            Self::TurnFailed
        }
    }

    /// The failure a read that answered `reason` met.
    pub(crate) fn of_read(reason: crate::query::QueryUnavailableReason) -> Self {
        match reason {
            crate::query::QueryUnavailableReason::InvalidSnapshot => Self::ContradictoryRows,
            _ => Self::StatementRefused,
        }
    }
}

impl IndexFailure {
    /// The failure an index read that returned `error` met, by the rule
    /// query dispatch uses: a corrupt row is a contradiction, anything else a
    /// statement the image refused.
    pub(crate) fn of_materialization(error: &tine_storage::sqlite::MaterializationError) -> Self {
        match error {
            tine_storage::sqlite::MaterializationError::Corrupt(_) => Self::ContradictoryRows,
            _ => Self::StatementRefused,
        }
    }
}

/// K1's front door for the index's own readers (GH #543, audit R12-05): a
/// read that fails reports what it met to the one decider, which asks for a
/// new image when one is owed, and answers `None`. `.ok()` on a reader's SQL
/// result swallowed detected damage, so the image was never replaced and each
/// session parsed the graph instead. Guard: `index_readers_report_damage`.
pub(super) trait ReportDamage<T> {
    fn reported(self, projection: &DirectProjection) -> Option<T>;
}

/// An index reader's error, as the failure it met (`None`: not a failure,
/// e.g. a cancelled job).
pub(super) trait ReadError {
    fn failure(&self) -> Option<IndexFailure>;
}

impl ReadError for tine_storage::sqlite::MaterializationError {
    fn failure(&self) -> Option<IndexFailure> {
        Some(IndexFailure::of_materialization(self))
    }
}

impl ReadError for crate::query::results::ResultReadError {
    fn failure(&self) -> Option<IndexFailure> {
        use crate::query::results::ResultReadError;
        match self {
            ResultReadError::Cancelled => None,
            ResultReadError::Corrupt(_) => Some(IndexFailure::ContradictoryRows),
            ResultReadError::Sql(error) => error.failure(),
            ResultReadError::StatisticsResourceLimit => Some(IndexFailure::StatementRefused),
        }
    }
}

impl<T, E: ReadError> ReportDamage<T> for Result<T, E> {
    fn reported(self, projection: &DirectProjection) -> Option<T> {
        match self {
            Ok(value) => Some(value),
            Err(error) => {
                if error
                    .failure()
                    .is_some_and(|failure| failure_owes_new_image(&projection.shared, failure))
                {
                    projection.request_rebuild();
                }
                None
            }
        }
    }
}

/// The one answer to "does this failure owe the index a new image?" (GH #543,
/// class K1). Only damage does; every other failure is answered in place.
///
/// - `StatementRefused`: only when the image is damaged. A statement SQLite
///   refuses on an intact image (an expression tree too deep, any hard limit
///   on an admitted input) fails the same way on a freshly built one, so
///   rebuilding for it rebuilt the whole index on every retry of that query
///   (audit R10-01). The check (schema plus `quick_check`) runs once per
///   ready generation.
/// - `ContradictoryRows`: the reader's own evidence of damage `quick_check`
///   cannot see -- rows only a Tine defect writes, such as a block whose
///   page row is missing. Ignoring it left task and reference queries failing for the
///   rest of the session (audit R11-06). Once per projection: a contradiction
///   on the rebuilt image is a lowering defect no rebuild fixes.
///   A turn whose writes violate a constraint met the same evidence
///   ([`IndexFailure::of_turn`]).
/// - `TurnFailed`: only when the image is damaged, checked afresh (the failed
///   turn may be what damaged it). On an intact image the turn's marks go
///   back to the queue and re-lower exactly the pages it may have half
///   written (audit R11-07).
pub(super) fn failure_owes_new_image(shared: &ProjectionShared, failure: IndexFailure) -> bool {
    #[cfg(test)]
    if shared.inject_image_damage.swap(false, Ordering::AcqRel) {
        return true;
    }
    let damaged = !image_is_intact(shared, failure != IndexFailure::TurnFailed);
    damaged
        || failure == IndexFailure::ContradictoryRows
            && !shared.contradiction_rebuilt.swap(true, Ordering::AcqRel)
}

/// Whether the stored image passes its schema check and `quick_check`;
/// `memoized`: trust an earlier pass at this ready generation.
fn image_is_intact(shared: &ProjectionShared, memoized: bool) -> bool {
    let generation = shared.ready_generation.load(Ordering::Acquire);
    let mut verified = shared.image_verified_intact_at.lock().unwrap();
    if memoized && *verified == Some(generation) {
        return true;
    }
    let intact = PhysicalGraphProjectionDatabase::open_read_only(&shared.path)
        .is_ok_and(|database| database.validate_schema().is_ok() && database.quick_check().is_ok());
    *verified = intact.then_some(generation);
    intact
}

/// Whether the stored image answers for `pending.latest_generation`: nothing
/// whole-graph is owed and no page update is queued. The one test readiness
/// is published under (`publish_if_current`), so readiness is never claimed over an image the
/// decider still owes a pass. A turn that ignored a stale mark or a latched
/// fresh build published readiness beside a `Validate`/`Fresh` need; the
/// owner's pass then found the image "ready", did nothing, and ran again at
/// once, millions of times a second (GH #543, audit R7-02).
pub(super) fn image_is_current(shared: &ProjectionShared, pending: &PendingProjection) -> bool {
    index_need(shared, pending) == IndexNeed::Nothing && !pending.has_work()
}

/// Whether a fresh build already owns this image's replacement: one is
/// running (`fresh_build_running`, the worker's own answer, never the
/// progress counter: audit R12-01), or a rebuild is queued with the payload
/// that carries it. A
/// read that failed on the current image owes nothing more then -- the
/// build replaces that image whole -- and a second request would queue a
/// second complete build behind it (GH #543, indexing audit IT-10).
pub(super) fn fresh_build_owns_image(
    shared: &ProjectionShared,
    pending: &PendingProjection,
) -> bool {
    shared.fresh_build_running.load(Ordering::Acquire)
        || (pending.rebuild && pending.full.is_some())
}

/// Whether the owner is waiting out a backoff after passes or turns that did
/// not make the index ready.
pub(super) fn backing_off(pending: &PendingProjection) -> bool {
    pending
        .retry_after
        .is_some_and(|retry_after| std::time::Instant::now() < retry_after)
}

/// How many consecutive whole-graph passes or worker turns may end without
/// making the index ready before it stops trying for the session (GH #594,
/// index liveness L1). The first failure is retried at once and the second
/// after a second; a third leaves the index `Failed`.
pub(crate) const INDEX_ATTEMPTS: u32 = 3;

/// Record a whole-graph pass or worker turn that did not make the index
/// ready, and why. The first one is retried at once: a launch check that a
/// rename raced is ordinary. The second waits a second, so a failure that
/// repeats does not run whole-graph passes back to back. The
/// [`INDEX_ATTEMPTS`]th leaves the index `Failed` with this class, for good
/// this session: retrying on a backoff forever reported "recovering" to every
/// reader while nothing ever recovered, and each retry parsed the whole graph
/// again (GH #594). New facts do not cut the wait short: that would make every
/// save a rebuild trigger while the failure lasts.
pub(super) fn note_unsettled(pending: &mut PendingProjection, class: IndexFailureClass) {
    pending.unsettled_passes = pending.unsettled_passes.saturating_add(1);
    pending.last_failure = Some(class);
    let passes = pending.unsettled_passes;
    report_index_failure(IndexFailureEvent {
        class,
        attempt: passes,
        terminal: passes >= INDEX_ATTEMPTS,
    });
    if passes >= INDEX_ATTEMPTS {
        pending.failed = Some(class);
        pending.retry_after = None;
        projection_diag(|| format!("unsettled pass {passes} ({}); index failed", class.as_str()));
        return;
    }
    if passes == 1 {
        projection_diag(|| format!("unsettled pass 1 ({}); retrying at once", class.as_str()));
        return;
    }
    let wait = std::time::Duration::from_secs(1);
    pending.retry_after = Some(std::time::Instant::now() + wait);
    projection_diag(|| format!("unsettled pass {passes}; next owner pass in {wait:?}"));
}

/// A failure no further attempt can get past (the worker cannot set up its
/// image): the index is `Failed` at once. The user's Retry reopens the graph,
/// which starts a new worker, exactly as the next launch would.
pub(super) fn note_failed(pending: &mut PendingProjection, class: IndexFailureClass) {
    pending.last_failure = Some(class);
    pending.failed = Some(class);
    pending.retry_after = None;
    report_index_failure(IndexFailureEvent {
        class,
        attempt: pending.unsettled_passes.saturating_add(1),
        terminal: true,
    });
}

/// One failed attempt, for whoever records them (the app's flight recorder).
/// Fixed codes only: no path, message or graph content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexFailureEvent {
    pub class: IndexFailureClass,
    /// 1-based, consecutive since the index was last ready; 0 when the
    /// background integrity check found the stored image damaged.
    pub attempt: u32,
    /// This attempt left the index `Failed`.
    pub terminal: bool,
}

type IndexFailureObserver = Box<dyn Fn(IndexFailureEvent) + Send + Sync>;
static INDEX_FAILURE_OBSERVER: std::sync::OnceLock<IndexFailureObserver> =
    std::sync::OnceLock::new();

/// Install the process's recorder of index failures (index liveness L5). The
/// release app has no console, so a failure printed to stderr alone was
/// invisible in every field report (GH #594). The first install wins.
pub fn set_index_failure_observer(observer: impl Fn(IndexFailureEvent) + Send + Sync + 'static) {
    let _ = INDEX_FAILURE_OBSERVER.set(Box::new(observer));
}

pub(super) fn report_index_failure(event: IndexFailureEvent) {
    #[cfg(test)]
    record_index_failure_for_test(event);
    if let Some(observer) = INDEX_FAILURE_OBSERVER.get() {
        observer(event);
    }
}

#[cfg(test)]
static TEST_INDEX_FAILURE_LOG: Mutex<Vec<IndexFailureEvent>> = Mutex::new(Vec::new());

#[cfg(test)]
fn record_index_failure_for_test(event: IndexFailureEvent) {
    TEST_INDEX_FAILURE_LOG.lock().unwrap().push(event);
}

/// Every failure reported in this test process so far (tests run in
/// parallel: filter by class or count increases, never assert equality).
#[cfg(test)]
pub(crate) fn index_failures_reported_for_test() -> Vec<IndexFailureEvent> {
    TEST_INDEX_FAILURE_LOG.lock().unwrap().clone()
}

/// Whether the index serves the stored image to `LaunchStored` reads: the image
/// the last session left, opened under this facts version and configuration
/// (`stored_servable`), before the launch check has validated it, with no
/// replacement owed or under way and no edit of this session waiting to be
/// applied (read-your-writes). The ONE answer to that question (launch design
/// D2); it is not readiness, which [`image_is_current`] alone claims.
pub(super) fn serving_stored(shared: &ProjectionShared, pending: &PendingProjection) -> bool {
    pending.stored_servable
        && pending.set_up
        && !shared.validated.load(Ordering::Acquire)
        && !pending.stop
        && !pending.lease_wait
        && pending.failed.is_none()
        && !pending.rebuild
        && pending.full.is_none()
        && !pending.building
        && pending.marks.is_empty()
        && pending.in_flight.is_empty()
        && shared.worker_available.load(Ordering::Acquire)
        && !shared.worker_failed.load(Ordering::Acquire)
        && shared.deltas_coming.load(Ordering::Acquire) == 0
        && shared.repairs_in_flight.load(Ordering::Acquire) == 0
}

/// The index's state apart from readiness at a particular generation: the ONE
/// answer both the progress a reader is told ([`DirectProjection::progress_at`])
/// and "is work coming" ([`index_work_coming`]) are read from (GH #594, index
/// liveness L2). They used to be two computations, and during a backoff one
/// said "recovering, wait" while the other said "nothing is coming".
pub(super) fn index_state(shared: &ProjectionShared, pending: &PendingProjection) -> IndexState {
    use crate::query::QueryReadinessReason as Reason;
    if pending.stop || pending.lease_wait {
        return IndexState::Stopped;
    }
    // Before the worker's availability: a worker that could not set up its
    // image exits `Failed`, and that is what readers are told.
    if let Some(class) = pending.failed {
        return IndexState::Failed(class);
    }
    if !shared.worker_available.load(Ordering::Acquire) {
        return IndexState::Stopped;
    }
    // With an owner registered, whole-graph work the index needs is the
    // owner's to run; a query reports it and never starts it (GH #543).
    let owned = pending.owners > 0;
    let need = index_need(shared, pending);
    if pending.full.is_some() || (pending.rebuild && owned) {
        return IndexState::Working(Reason::Recovering);
    }
    if shared.repairs_in_flight.load(Ordering::Acquire) > 0 {
        return IndexState::Working(Reason::Recovering);
    }
    let owner_pass_coming = owned
        && matches!(
            need,
            IndexNeed::SettingUp | IndexNeed::Validate | IndexNeed::Fresh
        );
    if owner_pass_coming && !backing_off(pending) {
        return IndexState::Working(Reason::Indexing);
    }
    // An owed registry capture is queued work too, taken when no rebuild is
    // owed (`worker_can_take`). One waiting out a failed turn's backoff was
    // reported as "nothing coming", so readers parsed the whole graph beside
    // an index that was about to answer (GH #594 L2).
    if !pending.marks.is_empty()
        || !pending.in_flight.is_empty()
        || (pending.registry_owed.is_some() && !pending.rebuild)
        || shared.deltas_coming.load(Ordering::Acquire) > 0
    {
        return IndexState::Working(Reason::PendingEdits);
    }
    if owner_pass_coming {
        // Backing off: the owner retries when the backoff ends, at most a
        // second away (see `note_unsettled`).
        return IndexState::Working(Reason::Recovering);
    }
    if shared.worker_failed.load(Ordering::Acquire) {
        // The queue is empty and the last turn failed: nothing is coming.
        return IndexState::Idle;
    }
    if shared.worker_busy.load(Ordering::Acquire) {
        return IndexState::Working(Reason::Busy);
    }
    IndexState::Idle
}

/// See [`index_state`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IndexState {
    /// The worker is gone or waiting for another process's writer lease.
    Stopped,
    /// See [`IndexNeed::Failed`].
    Failed(IndexFailureClass),
    /// Queued or running work will change the image; the reason is why.
    Working(crate::query::QueryReadinessReason),
    /// Nothing is queued or running. The image is ready if it is current;
    /// otherwise only a repair can make it so.
    Idle,
}

/// Whether whole-graph index work is coming: an owner is registered and the
/// index is [`IndexState::Working`]. Readers wait (bounded) while this holds;
/// when it does not (no owner, a failed, stopped or idle index) they take
/// their ordinary route. Read under `pending`, from the one state function
/// progress is read from (GH #543, GH #594 L2).
pub(super) fn index_work_coming(shared: &ProjectionShared, pending: &PendingProjection) -> bool {
    pending.owners > 0 && matches!(index_state(shared, pending), IndexState::Working(_))
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

    /// Wait, at most `limit`, for the index's state to change while work is
    /// coming. A reader whose answer waits on that work sleeps here instead
    /// of asking again at once: the readiness wait returns at once while the
    /// image is ready, so a read that looped on `coming()` alone spun a core
    /// for as long as an update was announced (GH #543, audit R13-07). Every
    /// input of `coming()` notifies `changed`; `limit` bounds a wakeup lost
    /// to an announcement dropped between the check and the wait.
    pub(crate) fn wait_while_coming(&self, limit: std::time::Duration) {
        let pending = self.shared.pending.lock().unwrap();
        if index_work_coming(&self.shared, &pending) {
            drop(self.shared.changed.wait_timeout(pending, limit).unwrap());
        }
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
    pub(crate) fn note_unsettled_pass(&self, class: IndexFailureClass) {
        note_unsettled(&mut self.shared.pending.lock().unwrap(), class);
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
        request_rebuild(&self.shared);
    }

    /// Whether `failure` owes the index a new image; see
    /// [`failure_owes_new_image`].
    pub(crate) fn failure_owes_new_image(&self, failure: IndexFailure) -> bool {
        failure_owes_new_image(&self.shared, failure)
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

/// Owe the index a fresh image: the stored one is damaged. Nothing when a
/// fresh build already owns its replacement (IT-10); checked under the same
/// lock as the request, so two reports cannot both queue a build.
pub(super) fn request_rebuild(shared: &ProjectionShared) {
    let mut pending = shared.pending.lock().unwrap();
    if fresh_build_owns_image(shared, &pending) {
        return;
    }
    pending.rebuild = true;
    shared.ready.store(false, Ordering::Release);
    drop(pending);
    shared.changed.notify_all();
}
