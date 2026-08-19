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
//!   the configured count and byte limits. An observation batch is consumed
//!   lazily against those limits, so a watcher that reports a huge batch never
//!   materializes it here: the moment exact retention would exceed the bound,
//!   the batch collapses into one uncertain full-scan requirement and the rest
//!   of the batch stops being polled because a full scan already subsumes it.
//!   Every notify error, unknown path, or rescan request collapses the batch
//!   the same way. Work is never dropped; it is only ever made coarser.
//! * **Epochs.** One monotonic sequence counts intake, allocated with a checked
//!   add so an epoch is never reused. A drain is stamped with the sequence it
//!   actually covers, and an acknowledgement is accepted only for the exact
//!   in-flight work epoch. Anything enqueued during the drain lands past that
//!   epoch, so acknowledging the drain cannot claim it. If the sequence or the
//!   process-wide queue identity runs out, the queue closes and construction
//!   fails rather than aliasing unrelated work.
//! * **Quiesce.** [`WatcherQueueOwner::begin_quiesce`] bars intake and proves
//!   the queue empty, un-drained, and fully acknowledged under one lock. The
//!   returned guard defers intake; [`WatcherQuiesceGuard::commit_handoff`] then
//!   raises a stronger exclusive bar, waits for every already-started intake to
//!   land, re-proves the quiesce, and holds that bar across the caller's durable
//!   `Safe` commit. A failed proof, a failed commit, or a plain drop leaves the
//!   queue live with every epoch and every retained event intact.
//!
//! The queue has no filesystem authority, holds no [`crate::model::Graph`], and
//! writes no enrollment or projection state. Its only output is a
//! [`ReconciliationTrigger`]: a freshness signal a reconciliation consumer may
//! coalesce. Reconciliation semantics are not restated here.
//!
//! The threat model is the campaign's: honest single-user watcher/process races
//! and crashes, not a hostile peer. A crash loses only in-memory intake, and the
//! durable handoff stays `Unsafe` in exactly that case, so the next process
//! full-scans.

use std::collections::BTreeSet;
use std::fmt;
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;

use super::hot_engine::ProjectionStorageBinding;
use super::ManagedPath;

/// A freshness signal. It does not grant filesystem, graph, or write
/// authority; a consumer must recapture every selected path.
///
/// Only the two shapes this queue can produce exist. It was carried over from
/// the deleted `oplog::reconciliation_scan`, whose remaining variants were
/// minted exclusively by that module's scheduler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReconciliationTrigger {
    WatcherPaths(BTreeSet<ManagedPath>),
    WatcherUncertain,
}

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

impl WatcherQueueLimits {
    /// Production bounds for the promoted runtime's sole external-feed queue.
    pub(crate) const fn exact_external_feed() -> Self {
        Self {
            maximum_paths: 256,
            maximum_path_bytes: 64 * 1024,
            maximum_uncertain_reasons: 8,
        }
    }
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
    /// The intake sequence ran out, so this work could not be given a fresh
    /// epoch. It is retained as a full scan and the queue is closed.
    SequenceExhausted,
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

    /// Whether both epochs were minted by the same process-local queue.
    ///
    /// The identity stays opaque; a borrower gains no construction, drain,
    /// settlement, or quiesce authority from this comparison.
    pub(crate) const fn same_queue(self, other: Self) -> bool {
        self.queue_id == other.queue_id
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

/// A rejected intake never changes queue state, and never consumes a single
/// observation from the batch it was given.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WatcherEnqueueError {
    /// The presented binding is not this queue's enrolled binding.
    ForeignBinding,
    /// This thread is running a durable handoff commit under its own intake
    /// bar. Intake from any other thread waits for that bar; re-entering from
    /// the committing thread would wait on itself, so it is refused instead.
    /// The caller keeps the unobserved work and may retry once the commit
    /// returns.
    HandoffBarred,
}

impl fmt::Display for WatcherEnqueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignBinding => {
                formatter.write_str("watcher intake presented a foreign projection binding")
            }
            Self::HandoffBarred => formatter
                .write_str("watcher intake re-entered the durable handoff commit that bars it"),
        }
    }
}

impl std::error::Error for WatcherEnqueueError {}

/// Construction refused rather than aliasing a live queue identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WatcherQueueIdentityError {
    /// This process has minted every queue identity it has. A further queue
    /// would have to reuse a live one, so construction fails closed.
    QueueIdExhausted,
}

impl fmt::Display for WatcherQueueIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueIdExhausted => {
                formatter.write_str("this process has exhausted its watcher queue identities")
            }
        }
    }
}

impl std::error::Error for WatcherQueueIdentityError {}

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
    /// The intake sequence ran out, so an epoch no longer identifies one batch
    /// of work. Nothing may settle: an acknowledgement for an earlier drain at
    /// the final epoch would otherwise claim work enqueued after it.
    SequenceExhausted,
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
            Self::SequenceExhausted => formatter
                .write_str("the watcher intake sequence is exhausted, so nothing may settle"),
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
    /// The intake sequence ran out. The queue is closed and permanently
    /// requires a full scan, so it can never be proved quiesced again.
    SequenceExhausted,
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
            Self::SequenceExhausted => formatter
                .write_str("the watcher intake sequence is exhausted, so the queue is closed"),
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

/// Affine proof that one exact queue is barred, empty, and fully acknowledged
/// at one epoch.
///
/// Construction and use are closure-scoped inside
/// [`WatcherQuiesceGuard::commit_handoff`]. It is deliberately neither `Clone`
/// nor `Copy`, and the commit never returns it after releasing the intake bar,
/// so no sibling can replay yesterday's queue fact as deletion authority.
#[derive(Debug, Eq, PartialEq)]
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
    /// The intake sequence ran out. The queue is closed: it still retains every
    /// hint as a full scan, but nothing settles and no quiesce can be proved.
    pub(crate) sequence_exhausted: bool,
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

    /// Retain one exact path within the limits.
    ///
    /// Returns `false` when the path does not fit; the caller turns that into a
    /// full-scan requirement rather than dropping the path. Retaining into an
    /// already-uncertain set is a no-op that always "fits": a full scan covers
    /// every path, exact or not.
    fn retain_path(&mut self, path: ManagedPath, limits: WatcherQueueLimits) -> bool {
        if self.is_uncertain() || self.paths.contains(&path) {
            return true;
        }
        let path_bytes = path.as_str().len();
        if self.paths.len() >= limits.maximum_paths
            || path_bytes > limits.maximum_path_bytes.saturating_sub(self.path_bytes)
        {
            return false;
        }
        self.path_bytes = self.path_bytes.saturating_add(path_bytes);
        self.paths.insert(path);
        true
    }

    /// Merge another retained set into this one without losing work.
    fn absorb(&mut self, other: Self, limits: WatcherQueueLimits) {
        let Self {
            paths,
            path_bytes: _,
            uncertain_reasons,
            omitted_uncertain_reasons,
        } = other;
        let mut overflowed = false;
        for path in paths {
            overflowed |= !self.retain_path(path, limits);
        }
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

    /// Consume one watcher batch lazily under the limits.
    ///
    /// The batch is never materialized: each observation is retained or folds
    /// into the full-scan requirement as it arrives, and consumption stops the
    /// moment the batch has become uncertain, because a full scan already
    /// subsumes everything the rest of the batch could say. The returned flag
    /// reports whether the batch observed anything at all — an empty batch must
    /// not advance the intake epoch.
    fn stage(
        observations: impl IntoIterator<Item = WatcherObservation>,
        limits: WatcherQueueLimits,
    ) -> (Self, bool) {
        let mut staged = Self::default();
        let mut observed = false;
        for observation in observations {
            observed = true;
            match observation {
                WatcherObservation::ManagedPath(path) => {
                    if !staged.retain_path(path, limits) {
                        staged.require_full_scan(WatcherUncertainReason::PathOverflow, limits);
                    }
                }
                WatcherObservation::UnknownPath => {
                    staged.require_full_scan(WatcherUncertainReason::UnknownPath, limits);
                }
                WatcherObservation::NotifyError => {
                    staged.require_full_scan(WatcherUncertainReason::NotifyError, limits);
                }
                WatcherObservation::RescanRequired => {
                    staged.require_full_scan(WatcherUncertainReason::RescanRequired, limits);
                }
            }
            if staged.is_uncertain() {
                break;
            }
        }
        (staged, observed)
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
    /// The counter can no longer advance without reusing an epoch, so the queue
    /// is closed: intake is still retained, but as a permanent full-scan
    /// requirement that nothing may settle and no quiesce may claim.
    sequence_exhausted: bool,
    /// Highest work epoch a drain acknowledged.
    acknowledged: u64,
    pending: PendingWatcherWork,
    /// Intake retained aside while a quiesce guard holds the bar.
    deferred: PendingWatcherWork,
    in_flight: Option<InFlightDrain>,
    quiesce: Option<QuiesceState>,
    last_committed_quiesce: Option<u64>,
    /// Intake calls that have registered but not yet landed. A durable handoff
    /// waits for this to reach zero, so a batch that began before the bar is
    /// counted by the proof instead of slipping past it.
    active_intake: usize,
    /// The thread running a durable handoff commit under the exclusive intake
    /// bar. Intake from any other thread waits; that thread is refused.
    intake_bar: Option<ThreadId>,
}

static NEXT_WATCHER_QUEUE_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate one process-unique queue identity.
///
/// Checked, not wrapping: two live queues sharing an identity would let one
/// queue's acknowledgement settle the other's work, which is exactly the
/// aliasing the identity exists to prevent. Exhaustion fails construction
/// instead, and a refused allocation does not move the counter.
fn allocate_queue_id(counter: &AtomicU64) -> Result<u64, WatcherQueueIdentityError> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .map_err(|_| WatcherQueueIdentityError::QueueIdExhausted)
}

struct WatcherQueueShared {
    queue_id: u64,
    binding: ProjectionStorageBinding,
    limits: WatcherQueueLimits,
    state: Mutex<WatcherQueueState>,
    /// Signals that `active_intake` dropped or that the intake bar lifted.
    /// Both waiters use `notify_all`, so neither predicate can miss a wakeup.
    intake_settled: Condvar,
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

    fn wait<'a>(
        &self,
        state: MutexGuard<'a, WatcherQueueState>,
    ) -> MutexGuard<'a, WatcherQueueState> {
        self.intake_settled
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner)
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
        // A closed queue permanently owes a full scan, whatever the counters
        // say, so it can never be proved quiesced.
        if state.sequence_exhausted {
            return Err(WatcherQuiesceError::SequenceExhausted);
        }
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
    ///
    /// The ergonomic constructor. It panics only where
    /// [`Self::try_new`] fails: after this process has minted `u64::MAX - 1`
    /// watcher queues, which no run of Tine reaches. Failing closed is the
    /// point — the alternative is two live queues sharing one identity.
    pub(crate) fn new(binding: ProjectionStorageBinding, limits: WatcherQueueLimits) -> Self {
        Self::try_new(binding, limits).expect("watcher queue identities are not exhausted")
    }

    /// Create the queue, refusing rather than aliasing a live queue identity.
    pub(crate) fn try_new(
        binding: ProjectionStorageBinding,
        limits: WatcherQueueLimits,
    ) -> Result<Self, WatcherQueueIdentityError> {
        Self::try_new_in(binding, limits, &NEXT_WATCHER_QUEUE_ID)
    }

    /// [`Self::try_new`] against an explicit identity allocator, so exhaustion
    /// is provable without disturbing the process-wide counter.
    fn try_new_in(
        binding: ProjectionStorageBinding,
        limits: WatcherQueueLimits,
        counter: &AtomicU64,
    ) -> Result<Self, WatcherQueueIdentityError> {
        Ok(Self {
            shared: Arc::new(WatcherQueueShared {
                queue_id: allocate_queue_id(counter)?,
                binding,
                limits,
                state: Mutex::new(WatcherQueueState {
                    sequence: 0,
                    sequence_exhausted: false,
                    acknowledged: 0,
                    pending: PendingWatcherWork::default(),
                    deferred: PendingWatcherWork::default(),
                    in_flight: None,
                    quiesce: None,
                    last_committed_quiesce: None,
                    active_intake: 0,
                    intake_bar: None,
                }),
                intake_settled: Condvar::new(),
            }),
        })
    }

    /// Start the queue at an arbitrary point in its intake sequence, so the
    /// exhaustion boundary is reachable in a test without 2^64 enqueues.
    #[cfg(test)]
    fn new_at_sequence(
        binding: ProjectionStorageBinding,
        limits: WatcherQueueLimits,
        sequence: u64,
    ) -> Self {
        let owner = Self::new(binding, limits);
        {
            let mut state = owner.shared.lock();
            state.sequence = sequence;
            state.acknowledged = sequence;
        }
        owner
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
            sequence_exhausted: state.sequence_exhausted,
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
    ///
    /// Once the intake sequence is exhausted an epoch stops identifying one
    /// batch of work, so nothing settles at all: an acknowledgement replayed
    /// for an earlier drain at the final epoch would otherwise claim work
    /// enqueued after it. The work stays owed and drainable, and the caller
    /// returns it with [`Self::abandon_drain`].
    pub(crate) fn acknowledge_drain(
        &self,
        epoch: WatcherEpoch,
    ) -> Result<(), WatcherSettlementError> {
        let mut state = self.shared.lock();
        let in_flight = self.in_flight_epoch(&state)?;
        if epoch != in_flight {
            return Err(WatcherSettlementError::StaleOrForeignEpoch { in_flight });
        }
        if state.sequence_exhausted {
            return Err(WatcherSettlementError::SequenceExhausted);
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
    ) -> Result<WatcherQuiesceGuard, WatcherQuiesceError> {
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
            shared: Arc::clone(&self.shared),
            epoch: self.shared.epoch(sequence),
            committed: false,
        })
    }

    /// Whether a committed proof still describes the queue.
    ///
    /// Any intake, drain, or later quiesce since the commit makes it stale. This
    /// is the check a `Safe` record's revalidation pass would run.
    pub(crate) fn committed_quiesce_is_current(
        &self,
        binding: ProjectionStorageBinding,
        epoch: WatcherEpoch,
    ) -> bool {
        if binding != self.shared.binding || epoch.queue_id != self.shared.queue_id {
            return false;
        }
        let state = self.shared.lock();
        !state.sequence_exhausted
            && state.last_committed_quiesce == Some(epoch.sequence)
            && state.sequence == epoch.sequence
            && state.acknowledged == epoch.sequence
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
    /// any overflow — collapses the batch into a full-scan requirement, after
    /// which the rest of the batch is subsumed and is not consumed. Nothing is
    /// ever dropped. A batch that observes nothing does not advance the epoch.
    ///
    /// The batch is registered as in-progress intake *before* the first
    /// observation is consumed, so a durable handoff cannot commit past a batch
    /// that has already started. While such a commit holds the intake bar this
    /// call waits for it; the committing thread itself is refused with
    /// [`WatcherEnqueueError::HandoffBarred`] rather than waiting on its own
    /// bar, and neither outcome consumes an observation.
    pub(crate) fn enqueue(
        &self,
        binding: ProjectionStorageBinding,
        observations: impl IntoIterator<Item = WatcherObservation>,
    ) -> Result<WatcherIntake, WatcherEnqueueError> {
        if binding != self.shared.binding {
            return Err(WatcherEnqueueError::ForeignBinding);
        }
        // Registration happens first and outlives the consumption below, even
        // if the watcher's iterator panics.
        let _active = ActiveIntake::register(&self.shared)?;

        // Consumed without the state lock: the batch is arbitrary caller code,
        // and it is bounded here rather than materialized.
        let limits = self.shared.limits;
        let (staged, observed) = PendingWatcherWork::stage(observations, limits);

        let mut state = self.shared.lock();
        let quiescing = state.quiesce.is_some();
        if !observed {
            return Ok(WatcherIntake {
                epoch: self.shared.epoch(state.sequence),
                retained_uncertain: state.pending.is_uncertain(),
                deferred_by_quiesce: quiescing,
            });
        }
        // Checked, not saturating: reusing the final epoch would let a drain's
        // acknowledgement settle work it never covered. The queue closes
        // instead, and this batch is still retained — as a full scan, because a
        // closed queue can no longer distinguish epochs of work.
        match state.sequence.checked_add(1) {
            Some(next) => state.sequence = next,
            None => state.sequence_exhausted = true,
        }
        let exhausted = state.sequence_exhausted;
        let epoch = self.shared.epoch(state.sequence);
        let retained = if quiescing {
            &mut state.deferred
        } else {
            &mut state.pending
        };
        retained.absorb(staged, limits);
        if exhausted {
            retained.require_full_scan(WatcherUncertainReason::SequenceExhausted, limits);
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

/// One in-progress intake call, registered before its batch is consumed.
///
/// A durable handoff commit waits for every registration to clear before it
/// re-proves the quiesce, so an intake that had begun is either counted by that
/// proof — invalidating it — or has not begun at all. The registration is
/// released on drop, so a panicking watcher iterator cannot strand a handoff.
struct ActiveIntake<'a> {
    shared: &'a WatcherQueueShared,
}

impl<'a> ActiveIntake<'a> {
    fn register(shared: &'a WatcherQueueShared) -> Result<Self, WatcherEnqueueError> {
        let mut state = shared.lock();
        loop {
            match state.intake_bar {
                // Re-entrant intake from the thread running the durable commit
                // would wait on its own bar forever. Refuse it: the queue is
                // untouched and the caller still holds its unobserved work.
                Some(holder) if holder == std::thread::current().id() => {
                    return Err(WatcherEnqueueError::HandoffBarred);
                }
                // Another thread's durable commit is in flight. Wait: this
                // batch must land strictly after it, never inside it.
                Some(_) => state = shared.wait(state),
                None => break,
            }
        }
        state.active_intake += 1;
        Ok(Self { shared })
    }
}

impl Drop for ActiveIntake<'_> {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.active_intake -= 1;
        drop(state);
        self.shared.intake_settled.notify_all();
    }
}

/// A live bar on watcher intake, proving the queue quiesced at one exact epoch.
///
/// The bar is held for the guard's whole lifetime, so the caller may commit its
/// durable `Safe` record while holding it and know that no watcher event became
/// drainable in between. Dropping the guard — committed or not — reopens intake
/// and releases everything that arrived while it was held.
pub(crate) struct WatcherQuiesceGuard {
    shared: Arc<WatcherQueueShared>,
    epoch: WatcherEpoch,
    committed: bool,
}

impl fmt::Debug for WatcherQuiesceGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WatcherQuiesceGuard")
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl WatcherQuiesceGuard {
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

    /// Run the caller's durable handoff commit as one atomic barrier.
    ///
    /// The exclusive intake bar goes up first and stays up until the durable
    /// record has been written *and* recorded here. Under that bar the call
    /// waits for every intake that had already begun, then re-proves the
    /// quiesce, so the sequence is exactly:
    ///
    /// 1. bar intake;
    /// 2. drain the intake that began before the bar — it lands, and a landed
    ///    batch invalidates the proof;
    /// 3. re-prove the quiesce, and stop here if anything landed;
    /// 4. run the caller's durable commit, with intake still barred;
    /// 5. record the committed proof and release deferred work, still barred;
    /// 6. lift the bar.
    ///
    /// So an enqueue either lands before step 3 and defeats the commit, or is
    /// held until step 6 and lands after it. New work and a successful `Safe`
    /// commit cannot both win. A failed proof or a failed commit leaves the
    /// queue live with every epoch and retained event intact, and records no
    /// proof.
    pub(crate) fn commit_handoff<T, E>(
        mut self,
        commit: impl FnOnce(&WatcherQuiescedProof) -> Result<T, E>,
    ) -> Result<T, WatcherHandoffError<E>> {
        let shared = Arc::clone(&self.shared);
        let bar = IntakeBar::acquire(&shared, self.epoch.sequence)
            .map_err(WatcherHandoffError::Quiesce)?;
        let proof = WatcherQuiescedProof {
            binding: shared.binding,
            epoch: self.epoch,
        };
        let committed = match commit(&proof) {
            Ok(committed) => committed,
            Err(error) => {
                // The guard releases the queue inside the barrier either way,
                // so a failed commit cannot interleave with intake either. It
                // simply records nothing.
                drop(self);
                drop(bar);
                return Err(WatcherHandoffError::Commit(error));
            }
        };
        // Only now is the durable record real, so only now may the queue record
        // a committed quiesce. Dropping the guard before the bar keeps that
        // record, and the release of any deferred work, inside the barrier.
        self.committed = true;
        drop(self);
        drop(bar);
        Ok(committed)
    }
}

/// The exclusive bar that makes a durable handoff atomic.
///
/// Stronger than the quiesce guard's deferral: while it is held, intake from
/// any other thread waits instead of being retained aside, so nothing at all
/// changes between the re-proof and the durable write. It is released on drop,
/// including when the caller's commit panics.
struct IntakeBar<'a> {
    shared: &'a Arc<WatcherQueueShared>,
}

impl<'a> IntakeBar<'a> {
    /// Raise the bar, wait for intake that began before it, and re-prove the
    /// quiesce — all without ever releasing the lock between the wait and the
    /// proof, so no batch can slip between them.
    fn acquire(
        shared: &'a Arc<WatcherQueueShared>,
        sequence: u64,
    ) -> Result<Self, WatcherQuiesceError> {
        let mut state = shared.lock();
        match &state.quiesce {
            Some(quiesce) if quiesce.sequence == sequence => {}
            _ => return Err(WatcherQuiesceError::GuardReleased),
        }
        debug_assert!(
            state.intake_bar.is_none(),
            "one live quiesce guard means at most one intake bar"
        );
        state.intake_bar = Some(std::thread::current().id());
        while state.active_intake > 0 {
            state = shared.wait(state);
        }
        if let Err(error) = shared.prove_quiesced(&state, sequence) {
            state.intake_bar = None;
            drop(state);
            shared.intake_settled.notify_all();
            return Err(error);
        }
        drop(state);
        Ok(Self { shared })
    }
}

impl Drop for IntakeBar<'_> {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.intake_bar = None;
        drop(state);
        self.shared.intake_settled.notify_all();
    }
}

impl Drop for WatcherQuiesceGuard {
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
    use std::sync::mpsc;
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
    fn a_committed_quiesce_epoch_is_diagnostic_only_and_later_intake_stales_it() {
        let (owner, binding) = queue(0x1080);
        let handle = owner.handle();
        handle.enqueue(binding, [observe("One")]).unwrap();
        reconcile(&owner, binding);

        let guard = owner.begin_quiesce(binding).unwrap();
        let mut committed_while_barred = None;
        let (record, proof_binding, proof_epoch) = guard
            .commit_handoff(|proof| -> Result<&'static str, ()> {
                // The durable `Safe` write would happen here. Intake is still
                // barred at this exact point.
                committed_while_barred = Some(owner.status().quiescing);
                assert_eq!(proof.epoch().sequence(), 1);
                Ok("safe")
            })
            .map(|record| {
                (
                    record,
                    binding,
                    owner.status().last_committed_quiesce.unwrap(),
                )
            })
            .unwrap();
        assert_eq!(record, "safe");
        assert_eq!(committed_while_barred, Some(true));
        assert_eq!(proof_binding, binding);
        assert!(!owner.status().quiescing, "the bar lifts after the commit");
        assert_eq!(owner.status().last_committed_quiesce, Some(proof_epoch));
        assert!(owner.committed_quiesce_is_current(proof_binding, proof_epoch));

        handle.enqueue(binding, [observe("Two")]).unwrap();
        assert!(
            !owner.committed_quiesce_is_current(proof_binding, proof_epoch),
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
        let epoch = owner
            .begin_quiesce(binding)
            .unwrap()
            .commit_handoff(|proof| -> Result<WatcherEpoch, ()> { Ok(proof.epoch()) })
            .unwrap();
        assert!(owner.committed_quiesce_is_current(binding, epoch));

        let (other_owner, other_binding) = queue(0x1091);
        let other_epoch = other_owner
            .begin_quiesce(other_binding)
            .unwrap()
            .commit_handoff(|proof| -> Result<WatcherEpoch, ()> { Ok(proof.epoch()) })
            .unwrap();
        assert_eq!(other_epoch.sequence(), epoch.sequence());
        assert!(
            !owner.committed_quiesce_is_current(other_binding, other_epoch),
            "a proof minted by another queue must never be current here"
        );
        assert!(!other_owner.committed_quiesce_is_current(binding, epoch));
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

    /// Blocker 1a. An intake that has begun consuming its batch must be part of
    /// the handoff barrier, not something the barrier can commit past.
    #[test]
    fn an_intake_that_began_before_the_quiesce_defeats_the_durable_commit() {
        let (owner, binding) = queue(0x10d0);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        std::thread::scope(|scope| {
            let handle = owner.handle();
            let watcher = scope.spawn(move || {
                // A lazy watcher batch: the first poll announces that intake
                // has begun, then blocks until the test releases it.
                let observations = std::iter::once_with(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    observe("One")
                });
                handle.enqueue(binding, observations).unwrap()
            });

            // Intake has begun and has not landed, so the queue still looks
            // clean and the quiesce proof is granted at epoch 0.
            started_rx.recv().unwrap();
            assert_eq!(owner.status().latest_enqueue.sequence(), 0);
            let guard = owner.begin_quiesce(binding).unwrap();
            assert_eq!(guard.quiesced_epoch().sequence(), 0);

            // Release the batch. The commit must wait for it and then refuse:
            // a `Safe` record at epoch 0 would deny a watcher event that the
            // queue had already accepted responsibility for.
            release_tx.send(()).unwrap();
            let outcome = guard.commit_handoff(|_| -> Result<(), &str> {
                panic!("an intake that began before the bar must never reach the durable commit")
            });
            assert!(
                matches!(
                    outcome,
                    Err(WatcherHandoffError::Quiesce(
                        WatcherQuiesceError::ArrivedDuringQuiesce { .. }
                    ))
                ),
                "expected the barrier to observe the in-progress intake, got {outcome:?}"
            );
            assert_eq!(watcher.join().unwrap().epoch.sequence(), 1);
        });

        // Nothing was lost, and nothing was committed.
        let status = owner.status();
        assert_eq!(status.latest_enqueue.sequence(), 1);
        assert!(status.last_committed_quiesce.is_none());
        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(drained_paths(&drain), vec!["pages/One.md"]);
    }

    /// Blocker 1b. The forced order is revalidate, then an attempted enqueue,
    /// then the durable callback — the exact window a non-atomic barrier loses.
    #[test]
    fn an_intake_attempted_inside_the_durable_commit_is_barred_and_observes_nothing() {
        let (owner, binding) = queue(0x10d5);
        let handle = owner.handle();
        handle.enqueue(binding, [observe("One")]).unwrap();
        reconcile(&owner, binding);

        let guard = owner.begin_quiesce(binding).unwrap();
        let (record, proof_epoch) = guard
            .commit_handoff(|proof| -> Result<&'static str, ()> {
                // The barrier has already re-proved the quiesce; the durable
                // write is happening now.
                assert_eq!(proof.epoch().sequence(), 1);
                let barred = handle.enqueue(
                    binding,
                    std::iter::once_with(|| -> WatcherObservation {
                        panic!("a barred intake must not consume a single observation")
                    }),
                );
                assert_eq!(barred.unwrap_err(), WatcherEnqueueError::HandoffBarred);
                let status = owner.status();
                assert_eq!(status.latest_enqueue.sequence(), 1);
                assert!(!status.deferred, "a barred intake retains nothing");
                Ok("safe")
            })
            .map(|record| (record, owner.status().last_committed_quiesce.unwrap()))
            .unwrap();

        assert_eq!(record, "safe");
        assert!(owner.committed_quiesce_is_current(binding, proof_epoch));
        // The refused batch was never observed, so it is still the caller's to
        // retry — and retrying it correctly stales the committed proof.
        let retry = handle.enqueue(binding, [observe("Two")]).unwrap();
        assert_eq!(retry.epoch.sequence(), 2);
        assert!(!owner.committed_quiesce_is_current(binding, proof_epoch));
    }

    /// The bar must also be released. Another thread's intake waits for the
    /// durable commit and then lands, rather than being lost or stuck.
    #[test]
    fn another_threads_intake_lands_only_after_the_durable_commit() {
        let (owner, binding) = queue(0x10d8);
        owner.handle().enqueue(binding, [observe("One")]).unwrap();
        reconcile(&owner, binding);

        let guard = owner.begin_quiesce(binding).unwrap();
        let (start_tx, start_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let handle = owner.handle();
            let watcher = scope.spawn(move || {
                start_rx.recv().unwrap();
                handle.enqueue(binding, [observe("Two")]).unwrap()
            });

            let proof_epoch = guard
                .commit_handoff(|_| -> Result<(), ()> {
                    start_tx.send(()).unwrap();
                    // Whatever the watcher thread does from here, it cannot
                    // advance this queue while the bar is held.
                    assert_eq!(owner.status().latest_enqueue.sequence(), 1);
                    Ok(())
                })
                .map(|()| owner.status().last_committed_quiesce.unwrap())
                .unwrap();
            assert_eq!(proof_epoch.sequence(), 1);
            assert_eq!(watcher.join().unwrap().epoch.sequence(), 2);
        });

        let status = owner.status();
        assert_eq!(status.latest_enqueue.sequence(), 2);
        assert!(status.pending, "the barred intake landed after the commit");
        assert_eq!(
            status.last_committed_quiesce.map(|epoch| epoch.sequence()),
            Some(1)
        );
    }

    /// Blocker 2. At the end of the sequence the queue closes rather than
    /// reusing an epoch, and a replayed acknowledgement settles nothing.
    #[test]
    fn sequence_exhaustion_closes_the_queue_instead_of_reusing_an_epoch() {
        let binding = test_binding(0x10f0);
        let owner = WatcherQueueOwner::new_at_sequence(
            binding,
            WatcherQueueLimits::default(),
            u64::MAX - 1,
        );
        let handle = owner.handle();

        // The last epoch the sequence can mint behaves normally.
        let last = handle.enqueue(binding, [observe("One")]).unwrap();
        assert_eq!(last.epoch.sequence(), u64::MAX);
        assert!(!last.retained_uncertain);
        let settled = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(settled.epoch(), last.epoch);
        owner.acknowledge_drain(settled.epoch()).unwrap();
        drop(owner.begin_quiesce(binding).unwrap());

        // The next batch cannot be given a fresh epoch. It is retained as a
        // full scan and the queue closes.
        let overflow = handle.enqueue(binding, [observe("Two")]).unwrap();
        assert_eq!(
            overflow.epoch, last.epoch,
            "the sequence must not mint a fresh epoch it has already used"
        );
        assert!(overflow.retained_uncertain);
        let status = owner.status();
        assert!(status.sequence_exhausted);
        assert!(status.pending_requires_full_scan);

        // The later work is drainable at that same, now ambiguous, epoch — so
        // the acknowledgement replayed from the settled drain must not settle
        // it, and neither may a fresh one.
        let reopened = owner.begin_drain(binding).unwrap().unwrap();
        assert_eq!(reopened.epoch(), settled.epoch());
        assert!(reopened.requires_full_scan());
        assert_eq!(
            reopened
                .uncertain_reasons()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![WatcherUncertainReason::SequenceExhausted]
        );
        assert_eq!(
            owner.acknowledge_drain(settled.epoch()).unwrap_err(),
            WatcherSettlementError::SequenceExhausted
        );
        assert_eq!(
            owner.acknowledge_drain(reopened.epoch()).unwrap_err(),
            WatcherSettlementError::SequenceExhausted
        );
        assert_eq!(owner.status().drain_in_flight, Some(reopened.epoch()));
        assert_eq!(owner.status().acknowledged.sequence(), u64::MAX);

        // And no `Safe` handoff can ever be proved on a closed queue, whatever
        // the counters happen to say.
        assert_eq!(
            owner.begin_quiesce(binding).unwrap_err(),
            WatcherQuiesceError::SequenceExhausted
        );
        owner.abandon_drain(reopened.epoch()).unwrap();
        assert!(owner.status().pending_requires_full_scan);
        assert_eq!(
            owner.begin_quiesce(binding).unwrap_err(),
            WatcherQuiesceError::SequenceExhausted
        );
    }

    /// Blocker 2. A wrapped queue identity would let one queue's drain settle
    /// another's work, so the last identity is the last queue.
    #[test]
    fn queue_identity_exhaustion_refuses_construction_instead_of_aliasing() {
        let binding = test_binding(0x10f5);
        let limits = WatcherQueueLimits::default();
        let counter = AtomicU64::new(u64::MAX - 1);

        let last = WatcherQueueOwner::try_new_in(binding, limits, &counter).unwrap();
        assert_eq!(last.shared.queue_id, u64::MAX - 1);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);

        assert_eq!(
            WatcherQueueOwner::try_new_in(binding, limits, &counter).unwrap_err(),
            WatcherQueueIdentityError::QueueIdExhausted
        );
        assert_eq!(
            counter.load(Ordering::Relaxed),
            u64::MAX,
            "a refused construction must not move the allocator"
        );

        // The live queue owning that last identity is untouched.
        last.handle().enqueue(binding, [observe("One")]).unwrap();
        assert_eq!(last.status().latest_enqueue.sequence(), 1);
    }

    /// Blocker 3. Intake is bounded while it is consumed, not after.
    #[test]
    fn exact_path_intake_stops_consuming_once_the_bound_collapses_it() {
        let limits = WatcherQueueLimits {
            maximum_paths: 1,
            ..WatcherQueueLimits::default()
        };
        let binding = test_binding(0x10e0);
        let owner = WatcherQueueOwner::new(binding, limits);

        // An unbounded lazy batch. One path fits; the second distinct path
        // overflows and collapses the batch into a full scan, which subsumes
        // everything after it — so nothing after it may be polled.
        let mut polled = 0_usize;
        let observations = std::iter::from_fn(move || {
            polled += 1;
            Some(match polled {
                1 => observe("One"),
                2 => observe("Two"),
                _ => panic!("a subsumed watcher batch must not be consumed further"),
            })
        });

        let intake = owner.handle().enqueue(binding, observations).unwrap();
        assert_eq!(intake.epoch.sequence(), 1);
        assert!(intake.retained_uncertain);

        let drain = owner.begin_drain(binding).unwrap().unwrap();
        assert!(drain.requires_full_scan());
        assert_eq!(
            drain
                .uncertain_reasons()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![WatcherUncertainReason::PathOverflow],
            "the overflow must be retained as a full scan, not dropped"
        );
    }

    #[test]
    fn watcher_safe_proof_is_affine_closure_scoped_and_not_constructible_by_surface() {
        const SOURCE: &str = include_str!("watcher_queue.rs");
        let production = SOURCE
            .split_once("#[cfg(test)]\nmod tests")
            .expect("test module boundary must remain explicit")
            .0;
        let proof = production
            .split_once("pub(crate) struct WatcherQuiescedProof")
            .expect("proof declaration must remain present");
        let derive_prefix = proof
            .0
            .rsplit_once("#[derive(")
            .expect("proof derive must remain explicit")
            .1;
        assert!(!derive_prefix.contains("Clone"));
        assert!(!derive_prefix.contains("Copy"));
        assert!(!derive_prefix.contains("Default"));
        assert!(!derive_prefix.contains("Serialize"));
        assert!(!derive_prefix.contains("Deserialize"));
        for trait_name in ["Clone", "Copy", "Default"] {
            assert!(!production
                .contains(format!("impl {trait_name} for {}", "WatcherQuiescedProof").as_str()));
        }
        assert!(
            production.contains(") -> Result<T, WatcherHandoffError<E>>"),
            "commit_handoff must return only the closure result"
        );
        assert!(
            !production.contains(format!("Result<(T, {})", "WatcherQuiescedProof").as_str()),
            "commit_handoff must never return deletion authority after lifting the intake bar"
        );
    }
}
