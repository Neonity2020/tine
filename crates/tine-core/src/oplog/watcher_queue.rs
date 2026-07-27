//! The core-owned graph-text watcher event queue, its drain epochs, and the
//! RAII quiesce guard that a later `Safe` handoff needs.
//!
//! [`super::local_active::SAFE_HANDOFF_MISSING_DEPENDENCY`] names the one
//! device-local drain `tine-core` could not observe: the watcher event queue was
//! owned by the Tauri watcher, so a production `LocalActive -> Safe` transition
//! had to fail closed. This module moves that queue into the core so the drain
//! becomes provable here. It deliberately stops one step short of the handoff:
//! nothing in `local_active`, enrollment, startup, or Tauri is wired to it yet.
//!
//! What the primitive owns, and nothing else:
//!
//! * **Binding.** A queue is created for exactly one enrolled
//!   [`ProjectionStorageBinding`] (endpoint identity plus receipt-store
//!   identity). Every intake, drain, and quiesce call presents the binding it
//!   believes it is talking to, and a substituted binding is refused before any
//!   state moves. Epochs additionally carry a process-unique queue identity, so
//!   a second queue's acknowledgement can never settle this queue's work.
//! * **Bounded intake.** Exact [`ManagedPath`] hints are retained while they fit
//!   the configured count and byte limits. Anything that does not fit — and
//!   every notify error, unknown path, or rescan request — collapses into one
//!   uncertain full-scan requirement. Work is never dropped; it is only ever
//!   made coarser.
//! * **Epochs.** One monotonic sequence counts intake. A drain is stamped with
//!   the sequence it actually covers, and an acknowledgement is accepted only
//!   for the exact in-flight work epoch. Anything enqueued during the drain
//!   lands past that epoch, so acknowledging the drain cannot claim it.
//! * **Quiesce.** [`WatcherQueueOwner::begin_quiesce`] bars intake and proves
//!   the queue empty, un-drained, and fully acknowledged under one lock. The
//!   returned guard holds the bar until the caller's durable `Safe` commit
//!   returns. A failed proof, a failed commit, or a plain drop leaves the queue
//!   live with every epoch and every retained event intact.
//!
//! The queue has no filesystem authority, holds no [`crate::model::Graph`], and
//! writes no enrollment or projection state. Its only output is a
//! [`ReconciliationTrigger`], which the existing
//! [`super::reconciliation_scan::ReconciliationScheduler`] already knows how to
//! coalesce; reconciliation semantics are not restated here.
//!
//! The threat model is the campaign's: honest single-user watcher/process races
//! and crashes, not a hostile peer. A crash loses only in-memory intake, and the
//! durable handoff stays `Unsafe` in exactly that case, so the next process
//! full-scans.

use std::collections::BTreeSet;
use std::fmt;
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::hot_engine::ProjectionStorageBinding;
use super::reconciliation_scan::ReconciliationTrigger;
use super::ManagedPath;

/// Bounded retention for one endpoint's watcher intake.
///
/// These bound retained queue state only. A watcher may report a larger batch;
/// any path that cannot be retained turns the queue into a full-scan
/// requirement instead of being silently forgotten.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WatcherQueueLimits {
    pub(crate) maximum_paths: usize,
    pub(crate) maximum_path_bytes: usize,
    pub(crate) maximum_uncertain_reasons: usize,
}

impl Default for WatcherQueueLimits {
    fn default() -> Self {
        // Deliberately the same shape and magnitude as the watcher fields of
        // `ReconciliationSchedulerLimits`, which consumes this queue's output.
        Self {
            maximum_paths: 1024,
            maximum_path_bytes: 128 * 1024,
            maximum_uncertain_reasons: 8,
        }
    }
}

/// One thing a platform watcher observed.
///
/// Only an already-validated exact managed path can carry path detail. Every
/// other observation is deliberately detail-free: an unbounded raw path string
/// is exactly the kind of state this queue must not retain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WatcherObservation {
    /// The watcher resolved one exact enrolled managed path.
    ManagedPath(ManagedPath),
    /// The watcher saw a path it could not resolve to an exact enrolled
    /// managed path.
    UnknownPath,
    /// The platform watcher reported an error.
    NotifyError,
    /// The platform watcher dropped events or asked for a rescan.
    RescanRequired,
}

/// Why retained intake collapsed into a full-scan requirement.
///
/// This is bounded diagnostic detail. Its absence never means work was lost;
/// the requirement itself is always retained.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum WatcherUncertainReason {
    UnknownPath,
    NotifyError,
    RescanRequired,
    PathOverflow,
}

/// A monotonic point in one queue's intake sequence.
///
/// `queue_id` is process-unique, so an epoch minted by another queue — however
/// its sequence compares — can never settle work here. Sequence `0` means
/// nothing has ever been enqueued.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct WatcherEpoch {
    queue_id: u64,
    sequence: u64,
}

impl WatcherEpoch {
    pub(crate) const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// What one intake call retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WatcherIntake {
    /// The intake epoch after this call. Unchanged when nothing was observed.
    pub(crate) epoch: WatcherEpoch,
    /// The affected queue now requires a full scan rather than exact paths.
    pub(crate) retained_uncertain: bool,
    /// Intake was barred by a live quiesce guard. The work is retained aside
    /// and becomes drainable when the guard is released; the quiesce proof is
    /// invalidated.
    pub(crate) deferred_by_quiesce: bool,
}

/// A rejected intake never changes queue state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WatcherEnqueueError {
    /// The presented binding is not this queue's enrolled binding.
    ForeignBinding,
}

impl fmt::Display for WatcherEnqueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignBinding => {
                formatter.write_str("watcher intake presented a foreign projection binding")
            }
        }
    }
}

impl std::error::Error for WatcherEnqueueError {}

/// A rejected drain never removes retained work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WatcherDrainError {
    ForeignBinding,
    /// One drain is already outstanding at this epoch.
    DrainInFlight(WatcherEpoch),
    /// A quiesce guard is holding intake barred.
    Quiescing,
}

impl fmt::Display for WatcherDrainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignBinding => {
                formatter.write_str("watcher drain presented a foreign projection binding")
            }
            Self::DrainInFlight(epoch) => write!(
                formatter,
                "watcher drain epoch {} is still in flight",
                epoch.sequence
            ),
            Self::Quiescing => formatter.write_str("watcher intake is barred by a quiesce guard"),
        }
    }
}

impl std::error::Error for WatcherDrainError {}

/// A rejected settlement never advances the acknowledged epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WatcherSettlementError {
    NoDrainInFlight,
    /// The presented epoch is not the exact work epoch in flight — it is stale,
    /// ahead, or from another queue.
    StaleOrForeignEpoch {
        in_flight: WatcherEpoch,
    },
}

impl fmt::Display for WatcherSettlementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDrainInFlight => formatter.write_str("no watcher drain is in flight"),
            Self::StaleOrForeignEpoch { in_flight } => write!(
                formatter,
                "watcher settlement is not for the in-flight work epoch {}",
                in_flight.sequence
            ),
        }
    }
}

impl std::error::Error for WatcherSettlementError {}

/// Why the queue could not be proved quiesced.
///
/// Every variant leaves the queue live and every retained event intact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WatcherQuiesceError {
    ForeignBinding,
    /// Another quiesce guard is already live for this queue.
    AlreadyQuiescing,
    /// Retained intake still needs reconciliation.
    WorkPending {
        requires_full_scan: bool,
    },
    /// A drain is outstanding, so its work is neither queued nor proved.
    DrainInFlight(WatcherEpoch),
    /// The latest intake epoch has not been acknowledged by a drain.
    UnacknowledgedEpoch {
        latest_enqueue: WatcherEpoch,
        acknowledged: WatcherEpoch,
    },
    /// Intake arrived after the bar went up, so this quiesce is not true.
    ArrivedDuringQuiesce {
        latest_enqueue: WatcherEpoch,
    },
    /// The guard is no longer the queue's live quiesce.
    GuardReleased,
}

impl fmt::Display for WatcherQuiesceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignBinding => {
                formatter.write_str("watcher quiesce presented a foreign projection binding")
            }
            Self::AlreadyQuiescing => {
                formatter.write_str("watcher intake is already barred by a live quiesce guard")
            }
            Self::WorkPending { requires_full_scan } => write!(
                formatter,
                "watcher queue retains unreconciled work (full scan required: {requires_full_scan})"
            ),
            Self::DrainInFlight(epoch) => write!(
                formatter,
                "watcher drain epoch {} is still in flight",
                epoch.sequence
            ),
            Self::UnacknowledgedEpoch {
                latest_enqueue,
                acknowledged,
            } => write!(
                formatter,
                "watcher intake epoch {} is unacknowledged (acknowledged: {})",
                latest_enqueue.sequence, acknowledged.sequence
            ),
            Self::ArrivedDuringQuiesce { latest_enqueue } => write!(
                formatter,
                "watcher intake epoch {} arrived while the quiesce bar was held",
                latest_enqueue.sequence
            ),
            Self::GuardReleased => {
                formatter.write_str("the watcher quiesce guard is no longer live")
            }
        }
    }
}

impl std::error::Error for WatcherQuiesceError {}

/// Either the quiesce stopped being true, or the caller's durable commit failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WatcherHandoffError<E> {
    Quiesce(WatcherQuiesceError),
    Commit(E),
}

impl<E: fmt::Display> fmt::Display for WatcherHandoffError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Quiesce(error) => error.fmt(formatter),
            Self::Commit(error) => error.fmt(formatter),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for WatcherHandoffError<E> {}

/// One epoch of retained work, handed to a reconciliation caller.
///
/// Owning this value does not settle anything: the queue keeps the work until
/// [`WatcherQueueOwner::acknowledge_drain`] accepts this exact epoch, so a
/// dropped or crashed drain fails closed instead of losing hints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WatcherDrain {
    epoch: WatcherEpoch,
    trigger: ReconciliationTrigger,
    uncertain_reasons: BTreeSet<WatcherUncertainReason>,
    omitted_uncertain_reasons: bool,
}

impl WatcherDrain {
    /// The exact work epoch this drain covers, and the only epoch that may
    /// acknowledge it.
    pub(crate) const fn epoch(&self) -> WatcherEpoch {
        self.epoch
    }

    /// The freshness signal to hand to an existing reconciliation scheduler or
    /// session. It grants no filesystem, graph, or write authority.
    pub(crate) const fn trigger(&self) -> &ReconciliationTrigger {
        &self.trigger
    }

    pub(crate) const fn requires_full_scan(&self) -> bool {
        matches!(self.trigger, ReconciliationTrigger::WatcherUncertain)
    }

    pub(crate) const fn uncertain_reasons(&self) -> &BTreeSet<WatcherUncertainReason> {
        &self.uncertain_reasons
    }

    /// The diagnostic bound was reached. The full-scan requirement itself is
    /// still complete.
    pub(crate) const fn omitted_uncertain_reasons(&self) -> bool {
        self.omitted_uncertain_reasons
    }
}

/// Proof that one exact queue was barred, empty, and fully acknowledged at one
/// epoch, and that the caller's durable handoff commit then succeeded.
///
/// It is not a capability: it authorizes nothing on its own and stays valid only
/// while [`WatcherQueueOwner::quiesce_proof_is_current`] agrees.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WatcherQuiescedProof {
    binding: ProjectionStorageBinding,
    epoch: WatcherEpoch,
}

impl WatcherQuiescedProof {
    pub(crate) const fn binding(&self) -> ProjectionStorageBinding {
        self.binding
    }

    pub(crate) const fn epoch(&self) -> WatcherEpoch {
        self.epoch
    }
}

/// A compact, allocation-free view of queue state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WatcherQueueStatus {
    pub(crate) latest_enqueue: WatcherEpoch,
    pub(crate) acknowledged: WatcherEpoch,
    pub(crate) drain_in_flight: Option<WatcherEpoch>,
    pub(crate) last_committed_quiesce: Option<WatcherEpoch>,
    pub(crate) pending: bool,
    pub(crate) pending_requires_full_scan: bool,
    pub(crate) deferred: bool,
    pub(crate) quiescing: bool,
}

/// Bounded retained intake. Uncertainty subsumes exact paths: once a full scan
/// is required, retaining individual paths would only cost memory.
#[derive(Debug, Default)]
struct PendingWatcherWork {
    paths: BTreeSet<ManagedPath>,
    path_bytes: usize,
    uncertain_reasons: BTreeSet<WatcherUncertainReason>,
    omitted_uncertain_reasons: bool,
}

impl PendingWatcherWork {
    fn is_uncertain(&self) -> bool {
        !self.uncertain_reasons.is_empty() || self.omitted_uncertain_reasons
    }

    fn is_empty(&self) -> bool {
        !self.is_uncertain() && self.paths.is_empty()
    }

    /// Collapse into one full-scan requirement, retaining the reason while the
    /// diagnostic bound allows it.
    fn require_full_scan(&mut self, reason: WatcherUncertainReason, limits: WatcherQueueLimits) {
        self.paths.clear();
        self.path_bytes = 0;
        if self.uncertain_reasons.contains(&reason) {
            return;
        }
        if self.uncertain_reasons.len() < limits.maximum_uncertain_reasons {
            self.uncertain_reasons.insert(reason);
        } else {
            self.omitted_uncertain_reasons = true;
        }
    }

    /// Retain exact paths within the limits. Returns whether anything
    /// overflowed; the caller converts that into a full-scan requirement.
    fn retain_paths(&mut self, paths: BTreeSet<ManagedPath>, limits: WatcherQueueLimits) -> bool {
        if self.is_uncertain() {
            // A full scan already covers every path, exact or not.
            return false;
        }
        let mut overflowed = false;
        for path in paths {
            if self.paths.contains(&path) {
                continue;
            }
            let path_bytes = path.as_str().len();
            if self.paths.len() >= limits.maximum_paths
                || path_bytes > limits.maximum_path_bytes.saturating_sub(self.path_bytes)
            {
                overflowed = true;
                continue;
            }
            self.path_bytes = self.path_bytes.saturating_add(path_bytes);
            self.paths.insert(path);
        }
        overflowed
    }

    /// Merge another retained set into this one without losing work.
    fn absorb(&mut self, other: Self, limits: WatcherQueueLimits) {
        let Self {
            paths,
            path_bytes: _,
            uncertain_reasons,
            omitted_uncertain_reasons,
        } = other;
        let overflowed = self.retain_paths(paths, limits);
        for reason in uncertain_reasons {
            self.require_full_scan(reason, limits);
        }
        if omitted_uncertain_reasons {
            self.omitted_uncertain_reasons = true;
            self.paths.clear();
            self.path_bytes = 0;
        }
        if overflowed {
            self.require_full_scan(WatcherUncertainReason::PathOverflow, limits);
        }
    }

    fn trigger(&self) -> Option<ReconciliationTrigger> {
        if self.is_uncertain() {
            return Some(ReconciliationTrigger::WatcherUncertain);
        }
        if self.paths.is_empty() {
            return None;
        }
        Some(ReconciliationTrigger::WatcherPaths(self.paths.clone()))
    }
}

struct InFlightDrain {
    sequence: u64,
    work: PendingWatcherWork,
}

struct QuiesceState {
    sequence: u64,
    invalidated: bool,
}

struct WatcherQueueState {
    /// Monotonic intake counter. One non-empty intake call advances it once.
    sequence: u64,
    /// Highest work epoch a drain acknowledged.
    acknowledged: u64,
    pending: PendingWatcherWork,
    /// Intake retained aside while a quiesce guard holds the bar.
    deferred: PendingWatcherWork,
    in_flight: Option<InFlightDrain>,
    quiesce: Option<QuiesceState>,
    last_committed_quiesce: Option<u64>,
}

static NEXT_WATCHER_QUEUE_ID: AtomicU64 = AtomicU64::new(1);

struct WatcherQueueShared {
    queue_id: u64,
    binding: ProjectionStorageBinding,
    limits: WatcherQueueLimits,
    state: Mutex<WatcherQueueState>,
}

impl WatcherQueueShared {
    /// Recover a poisoned lock rather than stranding retained watcher work.
    ///
    /// Every state transition below is total and leaves no torn invariant: a
    /// panic can only happen between whole transitions, so the state a poisoned
    /// guard exposes is one the queue could legitimately be in. Refusing the
    /// lock instead would silently strip the graph of its freshness hints, which
    /// is the failure this queue exists to prevent.
    fn lock(&self) -> MutexGuard<'_, WatcherQueueState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    const fn epoch(&self, sequence: u64) -> WatcherEpoch {
        WatcherEpoch {
            queue_id: self.queue_id,
            sequence,
        }
    }

    /// Prove the queue holds no unreconciled work at `expected`.
    fn prove_quiesced(
        &self,
        state: &WatcherQueueState,
        expected: u64,
    ) -> Result<(), WatcherQuiesceError> {
        // Checked first because it is the strictly more informative diagnosis:
        // a barred arrival also advances the intake epoch, so it would
        // otherwise be reported as a plain unacknowledged epoch.
        if state.sequence != expected
            || !state.deferred.is_empty()
            || state
                .quiesce
                .as_ref()
                .is_some_and(|quiesce| quiesce.invalidated || quiesce.sequence != expected)
        {
            return Err(WatcherQuiesceError::ArrivedDuringQuiesce {
                latest_enqueue: self.epoch(state.sequence),
            });
        }
        if let Some(drain) = &state.in_flight {
            return Err(WatcherQuiesceError::DrainInFlight(
                self.epoch(drain.sequence),
            ));
        }
        // The epoch counters are the primary gate: they are monotonic and no
        // path can decrement them, so they cannot be fooled by a bookkeeping
        // slip in the retained sets.
        if state.acknowledged != state.sequence {
            return Err(WatcherQuiesceError::UnacknowledgedEpoch {
                latest_enqueue: self.epoch(state.sequence),
                acknowledged: self.epoch(state.acknowledged),
            });
        }
        // Retained work always keeps the counters apart, so reaching this means
        // the retained sets and the epochs disagree. Kept as a backstop: the
        // conservative answer to that disagreement is to refuse the quiesce.
        if !state.pending.is_empty() {
            return Err(WatcherQuiesceError::WorkPending {
                requires_full_scan: state.pending.is_uncertain(),
            });
        }
        Ok(())
    }
}

/// The single owner of one endpoint's watcher queue.
///
/// Deliberately not `Clone`: draining, settling, and quiescing are the runtime's
/// responsibility, and duplicating them would let two reconcilers acknowledge
/// each other's epochs. Watchers get [`WatcherHandle`] instead.
pub(crate) struct WatcherQueueOwner {
    shared: Arc<WatcherQueueShared>,
}

impl fmt::Debug for WatcherQueueOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WatcherQueueOwner")
            .field("queue_id", &self.shared.queue_id)
            .finish_non_exhaustive()
    }
}

impl WatcherQueueOwner {
    /// Create the queue for exactly one enrolled endpoint/storage binding.
    pub(crate) fn new(binding: ProjectionStorageBinding, limits: WatcherQueueLimits) -> Self {
        Self {
            shared: Arc::new(WatcherQueueShared {
                queue_id: NEXT_WATCHER_QUEUE_ID.fetch_add(1, Ordering::Relaxed),
                binding,
                limits,
                state: Mutex::new(WatcherQueueState {
                    sequence: 0,
                    acknowledged: 0,
                    pending: PendingWatcherWork::default(),
                    deferred: PendingWatcherWork::default(),
                    in_flight: None,
                    quiesce: None,
                    last_committed_quiesce: None,
                }),
            }),
        }
    }

    pub(crate) fn binding(&self) -> ProjectionStorageBinding {
        self.shared.binding
    }

    pub(crate) fn limits(&self) -> WatcherQueueLimits {
        self.shared.limits
    }

    /// Mint a cloneable intake handle. This is the only value a platform
    /// watcher — later, the narrow runtime facade Tauri sees — ever needs.
    pub(crate) fn handle(&self) -> WatcherHandle {
        WatcherHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    pub(crate) fn status(&self) -> WatcherQueueStatus {
        let state = self.shared.lock();
        WatcherQueueStatus {
            latest_enqueue: self.shared.epoch(state.sequence),
            acknowledged: self.shared.epoch(state.acknowledged),
            drain_in_flight: state
                .in_flight
                .as_ref()
                .map(|drain| self.shared.epoch(drain.sequence)),
            last_committed_quiesce: state
                .last_committed_quiesce
                .map(|sequence| self.shared.epoch(sequence)),
            pending: !state.pending.is_empty(),
            pending_requires_full_scan: state.pending.is_uncertain(),
            deferred: !state.deferred.is_empty(),
            quiescing: state.quiesce.is_some(),
        }
    }

    /// Take every retained hint as one epoch of work.
    ///
    /// Single flight: a second drain is refused while one is outstanding, so an
    /// acknowledgement is never ambiguous. `Ok(None)` means the queue is clean.
    pub(crate) fn begin_drain(
        &self,
        binding: ProjectionStorageBinding,
    ) -> Result<Option<WatcherDrain>, WatcherDrainError> {
        if binding != self.shared.binding {
            return Err(WatcherDrainError::ForeignBinding);
        }
        let mut state = self.shared.lock();
        if state.quiesce.is_some() {
            return Err(WatcherDrainError::Quiescing);
        }
        if let Some(drain) = &state.in_flight {
            return Err(WatcherDrainError::DrainInFlight(
                self.shared.epoch(drain.sequence),
            ));
        }
        if state.pending.is_empty() {
            return Ok(None);
        }
        let work = mem::take(&mut state.pending);
        let sequence = state.sequence;
        let drain = WatcherDrain {
            epoch: self.shared.epoch(sequence),
            trigger: work
                .trigger()
                .expect("non-empty retained watcher work always yields a trigger"),
            uncertain_reasons: work.uncertain_reasons.clone(),
            omitted_uncertain_reasons: work.omitted_uncertain_reasons,
        };
        state.in_flight = Some(InFlightDrain { sequence, work });
        Ok(Some(drain))
    }

    /// Settle the in-flight drain for the exact epoch that was reconciled.
    ///
    /// The epoch must be the one this queue handed out. A stale, ahead, or
    /// foreign epoch is refused and nothing moves — in particular, work that
    /// arrived during the drain keeps the queue unacknowledged.
    pub(crate) fn acknowledge_drain(
        &self,
        epoch: WatcherEpoch,
    ) -> Result<(), WatcherSettlementError> {
        let mut state = self.shared.lock();
        let in_flight = self.in_flight_epoch(&state)?;
        if epoch != in_flight {
            return Err(WatcherSettlementError::StaleOrForeignEpoch { in_flight });
        }
        state.in_flight = None;
        state.acknowledged = state.acknowledged.max(epoch.sequence);
        Ok(())
    }

    /// Return an unreconciled drain's work to the queue.
    ///
    /// The acknowledged epoch does not move, so the returned work is still owed
    /// a drain.
    pub(crate) fn abandon_drain(&self, epoch: WatcherEpoch) -> Result<(), WatcherSettlementError> {
        let mut state = self.shared.lock();
        let in_flight = self.in_flight_epoch(&state)?;
        if epoch != in_flight {
            return Err(WatcherSettlementError::StaleOrForeignEpoch { in_flight });
        }
        let drain = state
            .in_flight
            .take()
            .expect("the in-flight drain was just observed");
        let limits = self.shared.limits;
        if state.quiesce.is_some() {
            state.deferred.absorb(drain.work, limits);
        } else {
            state.pending.absorb(drain.work, limits);
        }
        Ok(())
    }

    fn in_flight_epoch(
        &self,
        state: &WatcherQueueState,
    ) -> Result<WatcherEpoch, WatcherSettlementError> {
        state
            .in_flight
            .as_ref()
            .map(|drain| self.shared.epoch(drain.sequence))
            .ok_or(WatcherSettlementError::NoDrainInFlight)
    }

    /// Bar intake and prove the queue quiesced, atomically.
    ///
    /// The bar goes up and the proof runs under one lock, so no arrival can slip
    /// between them. A failed proof takes the bar back down before returning:
    /// the queue stays live and keeps every retained event.
    pub(crate) fn begin_quiesce(
        &self,
        binding: ProjectionStorageBinding,
    ) -> Result<WatcherQuiesceGuard<'_>, WatcherQuiesceError> {
        if binding != self.shared.binding {
            return Err(WatcherQuiesceError::ForeignBinding);
        }
        let mut state = self.shared.lock();
        if state.quiesce.is_some() {
            return Err(WatcherQuiesceError::AlreadyQuiescing);
        }
        let sequence = state.sequence;
        state.quiesce = Some(QuiesceState {
            sequence,
            invalidated: false,
        });
        if let Err(error) = self.shared.prove_quiesced(&state, sequence) {
            state.quiesce = None;
            return Err(error);
        }
        drop(state);
        Ok(WatcherQuiesceGuard {
            shared: &self.shared,
            epoch: self.shared.epoch(sequence),
            committed: false,
        })
    }

    /// Whether a committed proof still describes the queue.
    ///
    /// Any intake, drain, or later quiesce since the commit makes it stale. This
    /// is the check a `Safe` record's revalidation pass would run.
    pub(crate) fn quiesce_proof_is_current(&self, proof: &WatcherQuiescedProof) -> bool {
        if proof.binding != self.shared.binding || proof.epoch.queue_id != self.shared.queue_id {
            return false;
        }
        let state = self.shared.lock();
        state.last_committed_quiesce == Some(proof.epoch.sequence)
            && state.sequence == proof.epoch.sequence
            && state.acknowledged == proof.epoch.sequence
            && state.in_flight.is_none()
            && state.pending.is_empty()
            && state.deferred.is_empty()
    }
}

/// The cloneable intake side of one endpoint's watcher queue.
///
/// Intake is the only capability it carries: a handle cannot drain, settle,
/// quiesce, or observe another handle's epochs.
#[derive(Clone)]
pub(crate) struct WatcherHandle {
    shared: Arc<WatcherQueueShared>,
}

impl fmt::Debug for WatcherHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WatcherHandle")
            .field("queue_id", &self.shared.queue_id)
            .finish_non_exhaustive()
    }
}

impl WatcherHandle {
    pub(crate) fn binding(&self) -> ProjectionStorageBinding {
        self.shared.binding
    }

    /// Retain one batch of watcher observations.
    ///
    /// Exact managed paths are coalesced while they fit; everything else — and
    /// any overflow — collapses the batch into a full-scan requirement. Nothing
    /// is ever dropped. A batch that observes nothing does not advance the
    /// epoch.
    pub(crate) fn enqueue(
        &self,
        binding: ProjectionStorageBinding,
        observations: impl IntoIterator<Item = WatcherObservation>,
    ) -> Result<WatcherIntake, WatcherEnqueueError> {
        if binding != self.shared.binding {
            return Err(WatcherEnqueueError::ForeignBinding);
        }
        let mut paths = BTreeSet::new();
        let mut reasons = BTreeSet::new();
        let mut observed = false;
        for observation in observations {
            observed = true;
            match observation {
                WatcherObservation::ManagedPath(path) => {
                    paths.insert(path);
                }
                WatcherObservation::UnknownPath => {
                    reasons.insert(WatcherUncertainReason::UnknownPath);
                }
                WatcherObservation::NotifyError => {
                    reasons.insert(WatcherUncertainReason::NotifyError);
                }
                WatcherObservation::RescanRequired => {
                    reasons.insert(WatcherUncertainReason::RescanRequired);
                }
            }
        }

        let limits = self.shared.limits;
        let mut state = self.shared.lock();
        let quiescing = state.quiesce.is_some();
        if !observed {
            return Ok(WatcherIntake {
                epoch: self.shared.epoch(state.sequence),
                retained_uncertain: state.pending.is_uncertain(),
                deferred_by_quiesce: quiescing,
            });
        }
        state.sequence = state.sequence.saturating_add(1);
        let epoch = self.shared.epoch(state.sequence);
        let retained = if quiescing {
            &mut state.deferred
        } else {
            &mut state.pending
        };
        let overflowed = retained.retain_paths(paths, limits);
        for reason in reasons {
            retained.require_full_scan(reason, limits);
        }
        if overflowed {
            retained.require_full_scan(WatcherUncertainReason::PathOverflow, limits);
        }
        let retained_uncertain = retained.is_uncertain();
        if let Some(quiesce) = &mut state.quiesce {
            quiesce.invalidated = true;
        }
        Ok(WatcherIntake {
            epoch,
            retained_uncertain,
            deferred_by_quiesce: quiescing,
        })
    }
}

/// A live bar on watcher intake, proving the queue quiesced at one exact epoch.
///
/// The bar is held for the guard's whole lifetime, so the caller may commit its
/// durable `Safe` record while holding it and know that no watcher event became
/// drainable in between. Dropping the guard — committed or not — reopens intake
/// and releases everything that arrived while it was held.
pub(crate) struct WatcherQuiesceGuard<'a> {
    shared: &'a Arc<WatcherQueueShared>,
    epoch: WatcherEpoch,
    committed: bool,
}

impl fmt::Debug for WatcherQuiesceGuard<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WatcherQuiesceGuard")
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl WatcherQuiesceGuard<'_> {
    pub(crate) const fn quiesced_epoch(&self) -> WatcherEpoch {
        self.epoch
    }

    pub(crate) fn binding(&self) -> ProjectionStorageBinding {
        self.shared.binding
    }

    /// Re-prove the quiesce. It can only fail because intake arrived after the
    /// bar went up, which is exactly the race a `Safe` record must not lose.
    pub(crate) fn revalidate(&self) -> Result<(), WatcherQuiesceError> {
        let state = self.shared.lock();
        match &state.quiesce {
            Some(quiesce) if quiesce.sequence == self.epoch.sequence => {}
            _ => return Err(WatcherQuiesceError::GuardReleased),
        }
        self.shared.prove_quiesced(&state, self.epoch.sequence)
    }

    /// Run the caller's durable handoff commit while intake stays barred.
    ///
    /// The quiesce is re-proved first, so a commit never runs against a queue
    /// that stopped being quiesced. Intake reopens only after `commit` returns,
    /// and a failed commit leaves the queue live with every epoch and retained
    /// event intact — no proof is recorded.
    pub(crate) fn commit_handoff<T, E>(
        mut self,
        commit: impl FnOnce(&WatcherQuiescedProof) -> Result<T, E>,
    ) -> Result<(T, WatcherQuiescedProof), WatcherHandoffError<E>> {
        self.revalidate().map_err(WatcherHandoffError::Quiesce)?;
        let proof = WatcherQuiescedProof {
            binding: self.shared.binding,
            epoch: self.epoch,
        };
        let committed = commit(&proof).map_err(WatcherHandoffError::Commit)?;
        // Only now is the durable record real, so only now may the queue record
        // a committed quiesce. The bar drops with `self` on return.
        self.committed = true;
        Ok((committed, proof))
    }
}

impl Drop for WatcherQuiesceGuard<'_> {
    fn drop(&mut self) {
        let limits = self.shared.limits;
        let mut state = self.shared.lock();
        match &state.quiesce {
            Some(quiesce) if quiesce.sequence == self.epoch.sequence => {}
            // Another guard already owns the bar; leave its state alone.
            _ => return,
        }
        if self.committed {
            state.last_committed_quiesce = Some(self.epoch.sequence);
        }
        state.quiesce = None;
        let deferred = mem::take(&mut state.deferred);
        state.pending.absorb(deferred, limits);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::hot_engine::ProjectionEndpointBinding;
    use crate::oplog::identity::{DeviceId, ProjectionEndpointId};
    use crate::oplog::{CanonicalGraphResourceId, ProjectionReceiptStoreId};
    use std::sync::Barrier;
    use uuid::Uuid;

    fn test_binding(seed: u128) -> ProjectionStorageBinding {
        ProjectionStorageBinding {
            endpoint: ProjectionEndpointBinding {
                endpoint_id: ProjectionEndpointId::from_uuid(Uuid::from_u128(seed)),
                device_id: DeviceId::from_uuid(Uuid::from_u128(seed + 1)),
                graph_resource_id: CanonicalGraphResourceId::from_capability_identity(
                    b"tine/watcher-queue-test",
                    &seed.to_be_bytes(),
                ),
            },
            receipt_store_id: ProjectionReceiptStoreId::from_capability_identity(
                b"tine/watcher-queue-test",
                &(seed + 2).to_be_bytes(),
            ),
        }
    }

    fn path(name: &str) -> ManagedPath {
        ManagedPath::parse(format!("pages/{name}.md")).unwrap()
    }

    fn observe(name: &str) -> WatcherObservation {
        WatcherObservation::ManagedPath(path(name))
    }

    fn queue(seed: u128) -> (WatcherQueueOwner, ProjectionStorageBinding) {
        let binding = test_binding(seed);
        (
            WatcherQueueOwner::new(binding, WatcherQueueLimits::default()),
            binding,
        )
    }

    fn drained_paths(drain: &WatcherDrain) -> Vec<String> {
        match drain.trigger() {
            ReconciliationTrigger::WatcherPaths(paths) => {
                paths.iter().map(|path| path.as_str().to_owned()).collect()
            }
            other => panic!("expected exact watcher paths, got {other:?}"),
        }
    }

    /// Drain and acknowledge everything currently retained.
    fn reconcile(owner: &WatcherQueueOwner, binding: ProjectionStorageBinding) {
        if let Some(drain) = owner.begin_drain(binding).unwrap() {
            owner.acknowledge_drain(drain.epoch()).unwrap();
        }
    }

    #[test]
    fn fresh_queue_is_quiesced_and_binds_its_endpoint() {
        let (owner, binding) = queue(0x1000);
        let status = owner.status();
        assert_eq!(status.latest_enqueue.sequence(), 0);
        assert_eq!(status.acknowledged.sequence(), 0);
        assert!(!status.pending);
        assert!(owner.begin_drain(binding).unwrap().is_none());

        let guard = owner.begin_quiesce(binding).unwrap();
        assert_eq!(guard.quiesced_epoch().sequence(), 0);
        assert_eq!(guard.binding(), binding);
        assert_eq!(owner.handle().binding(), binding);
    }

    #[test]
    fn exact_paths_coalesce_and_drain_as_a_watcher_path_trigger() {
        let (owner, binding) = queue(0x1010);
        let handle = owner.handle();

        let first = handle
            .enqueue(binding, [observe("One"), observe("Two")])
            .unwrap();
        assert_eq!(first.epoch.sequence(), 1);
        assert!(!first.retained_uncertain);

        // Duplicate and overlapping batches coalesce into the same retained set
        // while still advancing the intake epoch.
        let second = handle
            .enqueue(binding, [observe("Two"), observe("Three")])
            .unwrap();
        assert_eq!(second.epoch.sequence(), 2);
        let third = handle.enqueue(binding, [observe("One")]).unwrap();
        assert_eq!(third.epoch.sequence(), 3);

        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(drain.epoch().sequence(), 3);
        assert!(!drain.requires_full_scan());
        assert_eq!(
            drained_paths(&drain),
            vec!["pages/One.md", "pages/Three.md", "pages/Two.md"]
        );
    }

    #[test]
    fn an_empty_batch_does_not_advance_the_epoch() {
        let (owner, binding) = queue(0x1015);
        let intake = owner.handle().enqueue(binding, []).unwrap();
        assert_eq!(intake.epoch.sequence(), 0);
        assert!(!owner.status().pending);
    }

    #[test]
    fn uncertain_observations_collapse_to_one_full_scan_requirement() {
        let (owner, binding) = queue(0x1020);
        let handle = owner.handle();

        handle.enqueue(binding, [observe("One")]).unwrap();
        let intake = handle
            .enqueue(binding, [WatcherObservation::NotifyError])
            .unwrap();
        assert!(intake.retained_uncertain);
        // Exact paths that arrive after the collapse are subsumed, not retained.
        handle.enqueue(binding, [observe("Two")]).unwrap();
        handle
            .enqueue(binding, [WatcherObservation::UnknownPath])
            .unwrap();
        handle
            .enqueue(binding, [WatcherObservation::RescanRequired])
            .unwrap();

        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert!(drain.requires_full_scan());
        assert_eq!(drain.trigger(), &ReconciliationTrigger::WatcherUncertain);
        assert_eq!(
            drain
                .uncertain_reasons()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![
                WatcherUncertainReason::UnknownPath,
                WatcherUncertainReason::NotifyError,
                WatcherUncertainReason::RescanRequired,
            ]
        );
        assert!(!drain.omitted_uncertain_reasons());
        assert_eq!(drain.epoch().sequence(), 5);
    }

    #[test]
    fn path_count_overflow_becomes_a_full_scan_instead_of_dropped_work() {
        let limits = WatcherQueueLimits {
            maximum_paths: 2,
            ..WatcherQueueLimits::default()
        };
        let binding = test_binding(0x1030);
        let owner = WatcherQueueOwner::new(binding, limits);
        let handle = owner.handle();

        let intake = handle
            .enqueue(binding, [observe("One"), observe("Two")])
            .unwrap();
        assert!(!intake.retained_uncertain);
        let intake = handle.enqueue(binding, [observe("Three")]).unwrap();
        assert!(intake.retained_uncertain);

        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert!(drain.requires_full_scan());
        assert_eq!(
            drain
                .uncertain_reasons()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![WatcherUncertainReason::PathOverflow]
        );
    }

    #[test]
    fn path_byte_overflow_becomes_a_full_scan_instead_of_dropped_work() {
        let limits = WatcherQueueLimits {
            maximum_path_bytes: path("One").as_str().len(),
            ..WatcherQueueLimits::default()
        };
        let binding = test_binding(0x1035);
        let owner = WatcherQueueOwner::new(binding, limits);

        let intake = owner
            .handle()
            .enqueue(binding, [observe("One"), observe("Two")])
            .unwrap();
        assert!(intake.retained_uncertain);
        assert!(owner
            .begin_drain(binding)
            .unwrap()
            .unwrap()
            .requires_full_scan());
    }

    #[test]
    fn the_uncertain_reason_bound_never_loses_the_requirement() {
        let limits = WatcherQueueLimits {
            maximum_uncertain_reasons: 1,
            ..WatcherQueueLimits::default()
        };
        let binding = test_binding(0x1038);
        let owner = WatcherQueueOwner::new(binding, limits);
        let handle = owner.handle();

        handle
            .enqueue(binding, [WatcherObservation::NotifyError])
            .unwrap();
        handle
            .enqueue(binding, [WatcherObservation::UnknownPath])
            .unwrap();

        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert!(drain.requires_full_scan());
        assert_eq!(drain.uncertain_reasons().len(), 1);
        assert!(drain.omitted_uncertain_reasons());
    }

    #[test]
    fn intake_during_a_drain_lands_past_the_drained_epoch() {
        let (owner, binding) = queue(0x1040);
        let handle = owner.handle();

        handle.enqueue(binding, [observe("One")]).unwrap();
        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(drain.epoch().sequence(), 1);

        // The watcher keeps running while reconciliation is in flight.
        let during = handle.enqueue(binding, [observe("Two")]).unwrap();
        assert_eq!(during.epoch.sequence(), 2);
        assert!(!during.deferred_by_quiesce);

        owner.acknowledge_drain(drain.epoch()).unwrap();
        let status = owner.status();
        assert_eq!(status.acknowledged.sequence(), 1);
        assert_eq!(status.latest_enqueue.sequence(), 2);
        assert!(status.pending);

        // Acknowledging the drained epoch must not claim the newer work.
        assert_eq!(
            owner.begin_quiesce(binding).unwrap_err(),
            WatcherQuiesceError::UnacknowledgedEpoch {
                latest_enqueue: during.epoch,
                acknowledged: drain.epoch(),
            }
        );

        let second = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(second.epoch().sequence(), 2);
        assert_eq!(drained_paths(&second), vec!["pages/Two.md"]);
        owner.acknowledge_drain(second.epoch()).unwrap();
        owner.begin_quiesce(binding).unwrap();
    }

    #[test]
    fn a_second_drain_is_refused_while_one_is_in_flight() {
        let (owner, binding) = queue(0x1045);
        owner.handle().enqueue(binding, [observe("One")]).unwrap();
        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(
            owner.begin_drain(binding).unwrap_err(),
            WatcherDrainError::DrainInFlight(drain.epoch())
        );
    }

    #[test]
    fn a_stale_or_foreign_acknowledgement_settles_nothing() {
        let (owner, binding) = queue(0x1050);
        let handle = owner.handle();

        handle.enqueue(binding, [observe("One")]).unwrap();
        let first = owner.begin_drain(binding).unwrap().unwrap();
        owner.acknowledge_drain(first.epoch()).unwrap();

        // Replaying a settled epoch finds no drain at all.
        assert_eq!(
            owner.acknowledge_drain(first.epoch()).unwrap_err(),
            WatcherSettlementError::NoDrainInFlight
        );

        handle.enqueue(binding, [observe("Two")]).unwrap();
        let second = owner.begin_drain(binding).unwrap().unwrap();
        assert_ne!(first.epoch(), second.epoch());
        // A stale epoch for a live drain is refused...
        assert_eq!(
            owner.acknowledge_drain(first.epoch()).unwrap_err(),
            WatcherSettlementError::StaleOrForeignEpoch {
                in_flight: second.epoch()
            }
        );

        // ...and so is a same-sequence epoch minted by a different queue.
        let (other_owner, other_binding) = queue(0x1051);
        other_owner
            .handle()
            .enqueue(other_binding, [observe("One")])
            .unwrap();
        other_owner
            .handle()
            .enqueue(other_binding, [observe("Two")])
            .unwrap();
        let foreign = other_owner.begin_drain(other_binding).unwrap().unwrap();
        assert_eq!(foreign.epoch().sequence(), second.epoch().sequence());
        assert_eq!(
            owner.acknowledge_drain(foreign.epoch()).unwrap_err(),
            WatcherSettlementError::StaleOrForeignEpoch {
                in_flight: second.epoch()
            }
        );

        // The rejected settlements left the real drain owed.
        assert_eq!(
            owner.status().drain_in_flight,
            Some(second.epoch()),
            "a refused acknowledgement must not settle the in-flight drain"
        );
        assert_eq!(owner.status().acknowledged.sequence(), 1);
        owner.acknowledge_drain(second.epoch()).unwrap();
        assert_eq!(owner.status().acknowledged.sequence(), 2);
    }

    #[test]
    fn an_abandoned_drain_returns_its_work_without_advancing_acknowledgement() {
        let (owner, binding) = queue(0x1055);
        owner
            .handle()
            .enqueue(binding, [observe("One"), observe("Two")])
            .unwrap();
        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert!(!owner.status().pending);

        owner.abandon_drain(drain.epoch()).unwrap();
        let status = owner.status();
        assert!(status.pending);
        assert!(status.drain_in_flight.is_none());
        assert_eq!(status.acknowledged.sequence(), 0);

        let retry = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(retry.epoch(), drain.epoch());
        assert_eq!(
            drained_paths(&retry),
            vec!["pages/One.md", "pages/Two.md"],
            "abandoning a drain must retain every hint"
        );
    }

    #[test]
    fn an_unsettled_drain_blocks_quiesce() {
        let (owner, binding) = queue(0x1058);
        owner.handle().enqueue(binding, [observe("One")]).unwrap();
        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(
            owner.begin_quiesce(binding).unwrap_err(),
            WatcherQuiesceError::DrainInFlight(drain.epoch())
        );
        assert_eq!(
            owner.begin_drain(binding).unwrap_err(),
            WatcherDrainError::DrainInFlight(drain.epoch()),
            "a failed quiesce must leave the queue live"
        );
    }

    #[test]
    fn an_unacknowledged_epoch_blocks_quiesce_and_keeps_the_queue_live() {
        let (owner, binding) = queue(0x1060);
        let handle = owner.handle();
        handle.enqueue(binding, [observe("One")]).unwrap();
        let drain = owner.begin_drain(binding).unwrap().unwrap();
        // Work arrives, is drained, but the drain is abandoned rather than
        // acknowledged: the queue must stay owed.
        owner.abandon_drain(drain.epoch()).unwrap();
        assert_eq!(
            owner.begin_quiesce(binding).unwrap_err(),
            WatcherQuiesceError::UnacknowledgedEpoch {
                latest_enqueue: drain.epoch(),
                acknowledged: owner.status().acknowledged,
            }
        );
        assert_eq!(owner.status().acknowledged.sequence(), 0);

        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(
            owner.begin_quiesce(binding).unwrap_err(),
            WatcherQuiesceError::DrainInFlight(drain.epoch())
        );
        owner.acknowledge_drain(drain.epoch()).unwrap();
        assert_eq!(owner.begin_quiesce(binding).unwrap().quiesced_epoch(), {
            let epoch = owner.status().latest_enqueue;
            assert_eq!(epoch.sequence(), 1);
            epoch
        });
    }

    #[test]
    fn a_second_quiesce_is_refused_while_a_guard_is_live() {
        let (owner, binding) = queue(0x1065);
        let guard = owner.begin_quiesce(binding).unwrap();
        assert_eq!(
            owner.begin_quiesce(binding).unwrap_err(),
            WatcherQuiesceError::AlreadyQuiescing
        );
        drop(guard);
        owner.begin_quiesce(binding).unwrap();
    }

    #[test]
    fn intake_during_quiesce_is_barred_retained_and_invalidates_the_proof() {
        let (owner, binding) = queue(0x1070);
        let handle = owner.handle();
        handle.enqueue(binding, [observe("One")]).unwrap();
        reconcile(&owner, binding);

        let guard = owner.begin_quiesce(binding).unwrap();
        assert_eq!(guard.quiesced_epoch().sequence(), 1);
        guard.revalidate().unwrap();

        let during = handle.enqueue(binding, [observe("Two")]).unwrap();
        assert!(during.deferred_by_quiesce);
        assert_eq!(during.epoch.sequence(), 2);

        let status = owner.status();
        assert!(status.quiescing);
        assert!(status.deferred, "barred intake must still be retained");
        assert!(
            !status.pending,
            "barred intake must not become drainable while the bar is held"
        );
        assert_eq!(
            owner.begin_drain(binding).unwrap_err(),
            WatcherDrainError::Quiescing
        );
        assert_eq!(
            guard.revalidate().unwrap_err(),
            WatcherQuiesceError::ArrivedDuringQuiesce {
                latest_enqueue: WatcherEpoch {
                    queue_id: guard.quiesced_epoch().queue_id,
                    sequence: 2,
                }
            }
        );

        // The commit closure must never run against an invalidated quiesce.
        let outcome = guard.commit_handoff(|_| -> Result<(), &str> {
            panic!("an invalidated quiesce must not reach the durable commit")
        });
        assert!(matches!(
            outcome,
            Err(WatcherHandoffError::Quiesce(
                WatcherQuiesceError::ArrivedDuringQuiesce { .. }
            ))
        ));

        // Releasing the guard reopens intake and releases the retained work.
        let status = owner.status();
        assert!(!status.quiescing);
        assert!(!status.deferred);
        assert!(status.pending);
        assert!(status.last_committed_quiesce.is_none());
        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(drain.epoch().sequence(), 2);
        assert_eq!(drained_paths(&drain), vec!["pages/Two.md"]);
    }

    #[test]
    fn dropping_a_guard_without_a_commit_reopens_intake_and_keeps_every_epoch() {
        let (owner, binding) = queue(0x1075);
        let handle = owner.handle();
        handle.enqueue(binding, [observe("One")]).unwrap();
        reconcile(&owner, binding);

        let guard = owner.begin_quiesce(binding).unwrap();
        handle
            .enqueue(binding, [observe("Two"), WatcherObservation::NotifyError])
            .unwrap();
        drop(guard);

        let status = owner.status();
        assert!(!status.quiescing);
        assert!(status.pending);
        assert!(
            status.pending_requires_full_scan,
            "released uncertainty must survive the guard"
        );
        assert_eq!(status.latest_enqueue.sequence(), 2);
        assert_eq!(status.acknowledged.sequence(), 1);
        assert!(status.last_committed_quiesce.is_none());

        // Retry: reconcile the released work, then quiesce again.
        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert!(drain.requires_full_scan());
        owner.acknowledge_drain(drain.epoch()).unwrap();
        let guard = owner.begin_quiesce(binding).unwrap();
        assert_eq!(guard.quiesced_epoch().sequence(), 2);
    }

    #[test]
    fn a_committed_handoff_records_a_proof_that_later_intake_invalidates() {
        let (owner, binding) = queue(0x1080);
        let handle = owner.handle();
        handle.enqueue(binding, [observe("One")]).unwrap();
        reconcile(&owner, binding);

        let guard = owner.begin_quiesce(binding).unwrap();
        let mut committed_while_barred = None;
        let (record, proof) = guard
            .commit_handoff(|proof| -> Result<&'static str, ()> {
                // The durable `Safe` write would happen here. Intake is still
                // barred at this exact point.
                committed_while_barred = Some(owner.status().quiescing);
                assert_eq!(proof.epoch().sequence(), 1);
                Ok("safe")
            })
            .unwrap();
        assert_eq!(record, "safe");
        assert_eq!(committed_while_barred, Some(true));
        assert_eq!(proof.binding(), binding);
        assert!(!owner.status().quiescing, "the bar lifts after the commit");
        assert_eq!(owner.status().last_committed_quiesce, Some(proof.epoch()));
        assert!(owner.quiesce_proof_is_current(&proof));

        handle.enqueue(binding, [observe("Two")]).unwrap();
        assert!(
            !owner.quiesce_proof_is_current(&proof),
            "intake after the commit must invalidate the proof"
        );
    }

    #[test]
    fn a_failed_handoff_commit_leaves_the_queue_live_and_records_no_proof() {
        let (owner, binding) = queue(0x1085);
        let handle = owner.handle();
        handle.enqueue(binding, [observe("One")]).unwrap();
        reconcile(&owner, binding);

        let guard = owner.begin_quiesce(binding).unwrap();
        let outcome = guard.commit_handoff(|_| -> Result<(), &'static str> { Err("disk full") });
        assert_eq!(
            outcome.unwrap_err(),
            WatcherHandoffError::Commit("disk full")
        );

        let status = owner.status();
        assert!(!status.quiescing);
        assert!(status.last_committed_quiesce.is_none());
        assert_eq!(status.latest_enqueue.sequence(), 1);
        assert_eq!(status.acknowledged.sequence(), 1);

        // The queue is fully live again: intake, drain, and retry all work.
        handle.enqueue(binding, [observe("Two")]).unwrap();
        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(drained_paths(&drain), vec!["pages/Two.md"]);
        owner.acknowledge_drain(drain.epoch()).unwrap();
        owner
            .begin_quiesce(binding)
            .unwrap()
            .commit_handoff(|_| -> Result<(), ()> { Ok(()) })
            .unwrap();
    }

    #[test]
    fn a_foreign_proof_is_never_current() {
        let (owner, binding) = queue(0x1090);
        let (_, proof) = owner
            .begin_quiesce(binding)
            .unwrap()
            .commit_handoff(|_| -> Result<(), ()> { Ok(()) })
            .unwrap();
        assert!(owner.quiesce_proof_is_current(&proof));

        let (other_owner, other_binding) = queue(0x1091);
        let (_, other_proof) = other_owner
            .begin_quiesce(other_binding)
            .unwrap()
            .commit_handoff(|_| -> Result<(), ()> { Ok(()) })
            .unwrap();
        assert_eq!(other_proof.epoch().sequence(), proof.epoch().sequence());
        assert!(
            !owner.quiesce_proof_is_current(&other_proof),
            "a proof minted by another queue must never be current here"
        );
        assert!(!other_owner.quiesce_proof_is_current(&proof));
    }

    #[test]
    fn two_handles_share_one_bounded_queue_and_one_epoch_sequence() {
        let (owner, binding) = queue(0x10a0);
        let watcher = owner.handle();
        let rescan = watcher.clone();

        assert_eq!(
            watcher.enqueue(binding, [observe("One")]).unwrap().epoch,
            owner.shared.epoch(1)
        );
        assert_eq!(
            rescan.enqueue(binding, [observe("Two")]).unwrap().epoch,
            owner.shared.epoch(2)
        );
        assert_eq!(
            watcher.enqueue(binding, [observe("Three")]).unwrap().epoch,
            owner.shared.epoch(3)
        );

        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(drain.epoch().sequence(), 3);
        assert_eq!(
            drained_paths(&drain),
            vec!["pages/One.md", "pages/Three.md", "pages/Two.md"],
            "both handles feed the same retained set"
        );
        owner.acknowledge_drain(drain.epoch()).unwrap();
        owner.begin_quiesce(binding).unwrap();
    }

    #[test]
    fn concurrent_handles_never_lose_an_epoch() {
        const HANDLES: usize = 4;
        const BATCHES: usize = 64;

        let (owner, binding) = queue(0x10a5);
        let barrier = Barrier::new(HANDLES);
        std::thread::scope(|scope| {
            for handle_index in 0..HANDLES {
                let handle = owner.handle();
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for batch in 0..BATCHES {
                        handle
                            .enqueue(binding, [observe(&format!("P{handle_index}x{batch}"))])
                            .unwrap();
                    }
                });
            }
        });

        // The interleaving is nondeterministic; the invariants are not. Every
        // batch advanced the sequence exactly once, and every path is retained.
        let status = owner.status();
        assert_eq!(status.latest_enqueue.sequence(), (HANDLES * BATCHES) as u64);
        assert!(status.pending);
        assert!(!status.pending_requires_full_scan);

        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(drained_paths(&drain).len(), HANDLES * BATCHES);
        owner.acknowledge_drain(drain.epoch()).unwrap();
        owner.begin_quiesce(binding).unwrap();
    }

    #[test]
    fn a_substituted_binding_is_refused_at_every_boundary() {
        let (owner, binding) = queue(0x10b0);
        let handle = owner.handle();

        // Same endpoint identity, different receipt store.
        let mut wrong_store = binding;
        wrong_store.receipt_store_id = ProjectionReceiptStoreId::from_capability_identity(
            b"tine/watcher-queue-test",
            b"substituted-store",
        );
        // Same receipt store, different endpoint device.
        let mut wrong_device = binding;
        wrong_device.endpoint.device_id = DeviceId::from_uuid(Uuid::from_u128(0xdead_beef));
        // Same endpoint and store, different graph resource.
        let mut wrong_graph = binding;
        wrong_graph.endpoint.graph_resource_id = CanonicalGraphResourceId::from_capability_identity(
            b"tine/watcher-queue-test",
            b"substituted-graph",
        );

        for substituted in [wrong_store, wrong_device, wrong_graph, test_binding(0x10b1)] {
            assert_eq!(
                handle.enqueue(substituted, [observe("One")]).unwrap_err(),
                WatcherEnqueueError::ForeignBinding
            );
            assert_eq!(
                owner.begin_drain(substituted).unwrap_err(),
                WatcherDrainError::ForeignBinding
            );
            assert_eq!(
                owner.begin_quiesce(substituted).unwrap_err(),
                WatcherQuiesceError::ForeignBinding
            );
        }

        // None of the refusals moved any state.
        let status = owner.status();
        assert_eq!(status.latest_enqueue.sequence(), 0);
        assert!(!status.pending);
        assert!(!status.quiescing);
        handle.enqueue(binding, [observe("One")]).unwrap();
        assert_eq!(owner.status().latest_enqueue.sequence(), 1);
    }

    #[test]
    fn a_barred_uncertain_arrival_survives_the_guard_as_a_full_scan() {
        let (owner, binding) = queue(0x10c0);
        let handle = owner.handle();
        let guard = owner.begin_quiesce(binding).unwrap();
        handle
            .enqueue(binding, [WatcherObservation::NotifyError])
            .unwrap();
        assert!(matches!(
            guard.revalidate(),
            Err(WatcherQuiesceError::ArrivedDuringQuiesce { .. })
        ));
        drop(guard);

        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert!(drain.requires_full_scan());
        assert_eq!(
            drain
                .uncertain_reasons()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![WatcherUncertainReason::NotifyError]
        );
    }

    #[test]
    fn released_deferred_work_merges_with_pending_work_under_the_limits() {
        let limits = WatcherQueueLimits {
            maximum_paths: 2,
            ..WatcherQueueLimits::default()
        };
        let binding = test_binding(0x10c5);
        let owner = WatcherQueueOwner::new(binding, limits);
        let handle = owner.handle();

        // Two retained paths, drained and acknowledged so a quiesce can start.
        handle
            .enqueue(binding, [observe("One"), observe("Two")])
            .unwrap();
        reconcile(&owner, binding);

        let guard = owner.begin_quiesce(binding).unwrap();
        handle
            .enqueue(
                binding,
                [observe("Three"), observe("Four"), observe("Five")],
            )
            .unwrap();
        drop(guard);

        // The deferred batch alone exceeded the limit, so the release is a full
        // scan rather than a silently truncated path set.
        let status = owner.status();
        assert!(status.pending_requires_full_scan);
        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert!(drain.requires_full_scan());
        assert_eq!(
            drain
                .uncertain_reasons()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![WatcherUncertainReason::PathOverflow]
        );
    }
}
