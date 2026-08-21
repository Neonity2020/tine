//! Admitted local semantic mutation and one-shot external reconciliation
//! through one sealed authoritative and derived-state drain.
//!
//! The managed runtime owns this crate-private coordinator and invokes it for
//! admitted local edits, provider ingress, recovery, and derived-state drains.

#![allow(dead_code)] // mixed production coordinator and fault-injection surface

use std::fmt;
use std::sync::Arc;
#[cfg(test)]
use std::time::{Duration, Instant};

use crate::model::{HandoffSafeGuard, PublishedHandoffLatch};
use crate::Graph;

use super::enrollment::{EnrollmentError, VerifiedLocalCompositionError};
use super::hot_engine::{EngineError, LocalAuthorCapture, ReconciliationNeeded};
use super::import::{plan_affected_import, plan_clean_affected_import};
use super::local_active::{
    CleanRuntimeSession, LocalRuntimeAdmission, RuntimePromotionError, RuntimeRevocation,
    WorkspaceAuthorityBoundary, WorkspaceAuthorityRefusal,
};
use super::{
    AcceptedBatchEvent, AuthorBatch, BatchDisposition, BatchId, BatchInspection, BatchOrigin,
    ContentDigest, CrdtPeerId, ImportId, ImportPlan, ImportPlanStatus, LineageDigest, ObjectStore,
    OperationTransaction, PageId, PreparedBatch, ProjectionEndpointBinding, ProjectionError,
    ProjectionReceiptStore, ProjectionWork, ProjectionWorkStatus, RebuildSource, SessionId,
    ShardedHotEngine, SqliteFrontier, TailOverlay, TailReservation,
};

const CRDT_PEER_PROBE_BUDGET: u64 = 8;
const RESUME_OPERATION_BUDGET: usize = 256;

#[cfg(test)]
thread_local! {
    static TEST_RESUME_OPERATION_BUDGET: std::cell::Cell<Option<usize>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
struct TestResumeOperationBudgetGuard(Option<usize>);

#[cfg(test)]
impl Drop for TestResumeOperationBudgetGuard {
    fn drop(&mut self) {
        TEST_RESUME_OPERATION_BUDGET.set(self.0);
    }
}

#[cfg(test)]
fn test_resume_operation_budget(value: usize) -> TestResumeOperationBudgetGuard {
    let prior = TEST_RESUME_OPERATION_BUDGET.replace(Some(value));
    TestResumeOperationBudgetGuard(prior)
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TrustedLocalPreparationStageTimings {
    pub(crate) session_parts: Duration,
    pub(crate) bindings: Duration,
    pub(crate) draft: Duration,
    pub(crate) capture: Duration,
    pub(crate) finalize: Duration,
}

#[cfg(test)]
thread_local! {
    static LAST_TRUSTED_LOCAL_PREPARATION_STAGE_TIMINGS:
        std::cell::Cell<TrustedLocalPreparationStageTimings> =
        std::cell::Cell::new(TrustedLocalPreparationStageTimings {
            session_parts: Duration::ZERO,
            bindings: Duration::ZERO,
            draft: Duration::ZERO,
            capture: Duration::ZERO,
            finalize: Duration::ZERO,
        });
}

#[cfg(test)]
fn reset_trusted_local_preparation_stage_timings() {
    LAST_TRUSTED_LOCAL_PREPARATION_STAGE_TIMINGS
        .set(TrustedLocalPreparationStageTimings::default());
}

#[cfg(test)]
fn note_trusted_local_preparation_stage(
    update: impl FnOnce(&mut TrustedLocalPreparationStageTimings),
) {
    LAST_TRUSTED_LOCAL_PREPARATION_STAGE_TIMINGS.with(|timings| {
        let mut current = timings.get();
        update(&mut current);
        timings.set(current);
    });
}

#[cfg(test)]
pub(crate) fn last_trusted_local_preparation_stage_timings() -> TrustedLocalPreparationStageTimings
{
    LAST_TRUSTED_LOCAL_PREPARATION_STAGE_TIMINGS.get()
}

struct ResumeBudget {
    remaining: usize,
}

impl ResumeBudget {
    fn new() -> Self {
        Self {
            remaining: Self::budget(),
        }
    }

    /// The per-slice operation budget. The former value of 16 made fixed
    /// reauthentication/reopen overhead dominate: a 300-file import needed 168
    /// ~277 ms slices, and one legitimate 20,000-block page needed thousands.
    /// 256 retains bounded yields while amortizing that fixed work; the large
    /// page regression completes within 64 actor turns instead of remaining in
    /// Recovering until an unrelated memory bound eventually fired (#311).
    /// `TINE_RESUME_BUDGET` remains available for measured trade-off probes.
    fn budget() -> usize {
        #[cfg(test)]
        if let Some(value) = TEST_RESUME_OPERATION_BUDGET.get() {
            return value;
        }
        std::env::var("TINE_RESUME_BUDGET")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(RESUME_OPERATION_BUDGET)
    }

    /// Charge `count` units to the phase that actually performed the work.
    ///
    /// The phase is supplied by the call site rather than assumed, so an
    /// exhaustion failure always names the drain that overran and phase
    /// assertions in regressions stay meaningful.
    fn consume(
        &mut self,
        count: usize,
        phase: OperationalPhase,
    ) -> Result<(), OperationalCoordinatorError> {
        if count > self.remaining {
            return Err(OperationalCoordinatorError::new(
                phase,
                "coordinator resume operation budget was exceeded",
            ));
        }
        // The budget is charged per phase, so this is the only place that knows
        // which phase consumes an import's work. ~36 s of a 300-file import is
        // irreducible work (F42) and it has never been attributed to a phase.
        if super::phase_trace_enabled() {
            eprintln!("PHASE CHARGE {phase:?} {count}");
        }
        self.remaining -= count;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationalPhase {
    Bindings,
    Planning,
    Draft,
    Capture,
    Finalize,
    TailReservation,
    Publication,
    ArchiveStage,
    TailAdmission,
    SqliteDrain,
    ProjectionDrain,
}

/// Stable post-publication evidence that retrying the exact immutable batch
/// cannot turn into progress.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RetainedBlockReason {
    Rejected(super::EngineError),
    Quarantined,
    PublishedAuthentication,
    StableBinding,
    GuardedProjectionConflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OperationalCoordinatorError {
    phase: OperationalPhase,
    detail: String,
    revocation: Option<RuntimeRevocation>,
    retained_block: Option<RetainedBlockReason>,
    /// This "failure" is a bounded slice asking to be resumed, not something
    /// that went wrong. It must not be charged against the transient-failure
    /// retry budget: retrying cannot continue a slice, so a legitimate
    /// multi-slice import would exhaust three attempts and wedge permanently.
    continuation_required: bool,
}

impl OperationalCoordinatorError {
    fn new(phase: OperationalPhase, detail: impl Into<String>) -> Self {
        Self {
            phase,
            detail: detail.into(),
            revocation: None,
            retained_block: None,
            continuation_required: false,
        }
    }

    /// A bounded slice completed its portion and must be resumed. Distinct from
    /// a failure so the caller can continue instead of retrying.
    fn continuation_required(phase: OperationalPhase, detail: impl Into<String>) -> Self {
        Self {
            phase,
            detail: detail.into(),
            revocation: None,
            retained_block: None,
            continuation_required: true,
        }
    }

    fn revoked(phase: OperationalPhase, refusal: WorkspaceAuthorityRefusal) -> Self {
        Self {
            phase,
            detail: refusal.to_string(),
            revocation: refusal.revocation().cloned(),
            retained_block: None,
            continuation_required: false,
        }
    }

    fn retained_block(
        phase: OperationalPhase,
        detail: impl Into<String>,
        reason: RetainedBlockReason,
    ) -> Self {
        Self {
            phase,
            detail: detail.into(),
            revocation: None,
            retained_block: Some(reason),
            continuation_required: false,
        }
    }

    pub(crate) const fn phase(&self) -> OperationalPhase {
        self.phase
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    /// Terminal workspace-authority loss, if this failure observed one.
    ///
    /// This is diagnosis only. The live admission remains the sole authority,
    /// and the runtime's own latch independently refuses every later boundary.
    pub(crate) const fn revocation(&self) -> Option<&RuntimeRevocation> {
        self.revocation.as_ref()
    }

    pub(crate) const fn retained_block_reason(&self) -> Option<&RetainedBlockReason> {
        self.retained_block.as_ref()
    }

    /// True when this is a resume request from a bounded slice rather than a
    /// failure. See `continuation_required`.
    pub(crate) const fn is_continuation_required(&self) -> bool {
        self.continuation_required
    }
}

impl fmt::Display for OperationalCoordinatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.phase, self.detail)
    }
}

impl std::error::Error for OperationalCoordinatorError {}

/// Re-derive archive-rooted workspace authority immediately before one
/// authority-changing boundary, and report a refusal as that exact phase.
///
/// The five side-effect call sites below are the coordinator's complete set of
/// boundaries that take, change, or externalize authority: immutable
/// publication, accepted-history archive staging, tail admission, the SQLite
/// advance, and each manifested Markdown projection step.
/// Every one of them is already an [`OperationalPhase`], so a lost workspace
/// stays diagnosable by phase rather than collapsing into one generic error.
///
/// The proof is one held-handle stat plus one no-follow resolution of the lease
/// pathname — a few per external reconciliation, and none on the keystroke
/// path. A failure latches the promoted runtime's terminal revocation, so the
/// journey cannot continue at any later boundary and no later admission,
/// window, or coordinator run can start either.
fn reprove_workspace_authority(
    admission: &LocalRuntimeAdmission<'_>,
    boundary: WorkspaceAuthorityBoundary,
    phase: OperationalPhase,
) -> Result<(), OperationalCoordinatorError> {
    admission
        .reprove_workspace_authority(boundary)
        .map_err(|refusal| OperationalCoordinatorError::revoked(phase, refusal))
}

fn authorize_coordinator(
    admission: &LocalRuntimeAdmission<'_>,
    graph: &Graph,
    engine: &ShardedHotEngine,
) -> Result<(), OperationalCoordinatorError> {
    // Preserve a typed terminal outcome when the runtime was revoked before
    // this call. `authorize` still performs the complete enrolled binding proof
    // immediately afterwards.
    reprove_workspace_authority(
        admission,
        WorkspaceAuthorityBoundary::WindowAuthorization,
        OperationalPhase::Bindings,
    )?;
    admission
        .authorize(graph, engine)
        .map_err(classify_authorization_failure)
}

fn classify_authorization_failure(error: RuntimePromotionError) -> OperationalCoordinatorError {
    match error {
        RuntimePromotionError::WorkspaceAuthorityRevoked(refusal) => {
            OperationalCoordinatorError::revoked(OperationalPhase::Bindings, refusal)
        }
        RuntimePromotionError::WorkspaceAuthorityCheckUnavailable(refusal) => {
            OperationalCoordinatorError::new(OperationalPhase::Bindings, refusal.to_string())
        }
        RuntimePromotionError::Enrollment(VerifiedLocalCompositionError::Enrollment(
            EnrollmentError::Io(detail),
        )) => OperationalCoordinatorError::new(
            OperationalPhase::Bindings,
            EnrollmentError::Io(detail).to_string(),
        ),
        RuntimePromotionError::Store(super::StoreError::Io(error)) => {
            OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
        }
        stable => OperationalCoordinatorError::retained_block(
            OperationalPhase::Bindings,
            stable.to_string(),
            RetainedBlockReason::StableBinding,
        ),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OperationalCompletion {
    batch_id: BatchId,
    import_id: ImportId,
}

impl OperationalCompletion {
    pub(crate) const fn batch_id(self) -> BatchId {
        self.batch_id
    }

    pub(crate) const fn import_id(self) -> ImportId {
        self.import_id
    }
}

pub(crate) enum OperationalCoordinatorState {
    Blocked(ImportPlan),
    Noop,
    Complete(OperationalCompletion),
    FailedClosed(ExternalPublishedContinuation),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LocalMutationCompletion {
    batch_id: BatchId,
}

impl LocalMutationCompletion {
    pub(crate) const fn batch_id(self) -> BatchId {
        self.batch_id
    }
}

/// Typed recovery work retained by an admitted local semantic mutation.
pub(crate) enum LocalMutationRecovery {
    /// Exact graph bytes changed before the local draft could be sealed. The
    /// caller must reconcile these engine-derived paths and redraft.
    ReconciliationRequired(ReconciliationNeeded),
    /// Immutable publication may have happened. This exact continuation must
    /// be retried; redrafting would create a second mutation writer.
    Published(LocalPublishedContinuation),
}

impl LocalMutationRecovery {
    pub(crate) fn reconciliation_paths(&self) -> Option<&[super::ManagedPath]> {
        match self {
            Self::ReconciliationRequired(reconciliation) => Some(reconciliation.paths()),
            Self::Published(_) => None,
        }
    }

    pub(crate) fn published(&self) -> Option<&LocalPublishedContinuation> {
        match self {
            Self::ReconciliationRequired(_) => None,
            Self::Published(continuation) => Some(continuation),
        }
    }

    pub(crate) fn into_published(self) -> Option<LocalPublishedContinuation> {
        match self {
            Self::ReconciliationRequired(_) => None,
            Self::Published(continuation) => Some(continuation),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LocalMutationBlockReason {
    Prepublication,
    Retained(RetainedBlockReason),
}

/// A prepublication refusal or a stable post-publication block. The latter
/// retains the exact affine continuation and its immutable evidence.
pub(crate) struct BlockedLocalMutation {
    failure: OperationalCoordinatorError,
    reason: LocalMutationBlockReason,
    continuation: Option<LocalPublishedContinuation>,
}

impl BlockedLocalMutation {
    pub(crate) fn failure(&self) -> &OperationalCoordinatorError {
        &self.failure
    }

    pub(crate) fn reason(&self) -> &LocalMutationBlockReason {
        &self.reason
    }

    pub(crate) fn continuation(&self) -> Option<&LocalPublishedContinuation> {
        self.continuation.as_ref()
    }

    pub(crate) fn into_continuation(self) -> Option<LocalPublishedContinuation> {
        self.continuation
    }
}

/// Terminal authority loss, with the post-publication continuation retained
/// when recovery still owes derived-state drains.
pub(crate) struct RevokedLocalMutation {
    failure: OperationalCoordinatorError,
    continuation: Option<LocalPublishedContinuation>,
}

impl RevokedLocalMutation {
    pub(crate) fn failure(&self) -> &OperationalCoordinatorError {
        &self.failure
    }

    pub(crate) fn continuation(&self) -> Option<&LocalPublishedContinuation> {
        self.continuation.as_ref()
    }

    pub(crate) fn into_continuation(self) -> Option<LocalPublishedContinuation> {
        self.continuation
    }
}

/// Facade-ready result of one already-translated local semantic mutation.
///
/// The variants deliberately match the runtime states a later actor/Tauri
/// adapter needs. None carries a `LocalActive` permit or runtime admission.
pub(crate) enum LocalMutationCoordinatorState {
    Active(LocalMutationCompletion),
    Recovering(LocalMutationRecovery),
    Blocked(BlockedLocalMutation),
    Revoked(RevokedLocalMutation),
}

pub(crate) struct CorrelatedGuardedProjectionConflict {
    batch_id: BatchId,
    paths: Vec<super::ManagedPath>,
}

impl CorrelatedGuardedProjectionConflict {
    pub(crate) const fn batch_id(&self) -> BatchId {
        self.batch_id
    }

    pub(crate) fn paths(&self) -> &[super::ManagedPath] {
        &self.paths
    }
}

pub(crate) enum CorrelatedPublishedLocalResume {
    NoCommit,
    GuardedConflict(CorrelatedGuardedProjectionConflict),
    State(LocalMutationCoordinatorState),
}

fn durable_exact_batch_evidence(
    engine: &ShardedHotEngine,
    database: &SqliteFrontier,
    batch_id: BatchId,
) -> Result<bool, OperationalCoordinatorError> {
    let accepted = match engine.accepted_batch_evidence(batch_id) {
        Ok(_) => true,
        Err(EngineError::MissingDependency(missing)) if missing == batch_id => false,
        Err(error) => {
            return Err(OperationalCoordinatorError::retained_block(
                OperationalPhase::ArchiveStage,
                error.to_string(),
                RetainedBlockReason::PublishedAuthentication,
            ))
        }
    };
    let sqlite = database.contains_batch(batch_id).map_err(|error| {
        OperationalCoordinatorError::retained_block(
            OperationalPhase::SqliteDrain,
            error.to_string(),
            RetainedBlockReason::PublishedAuthentication,
        )
    })?;
    let index = engine.projection_work_index().map_err(|error| {
        OperationalCoordinatorError::retained_block(
            OperationalPhase::ProjectionDrain,
            error.to_string(),
            RetainedBlockReason::PublishedAuthentication,
        )
    })?;
    let projection = match index.accepted_batch_work_statuses(batch_id) {
        Ok(_) => true,
        Err(super::projection_work_index::ProjectionWorkError::AcceptedWitnessMissing) => false,
        Err(error) => {
            return Err(OperationalCoordinatorError::retained_block(
                OperationalPhase::ProjectionDrain,
                error.to_string(),
                RetainedBlockReason::PublishedAuthentication,
            ))
        }
    };
    Ok(accepted || sqlite || projection)
}

impl LocalMutationCoordinatorState {
    fn blocked(error: OperationalCoordinatorError) -> Self {
        if error.revocation().is_some() {
            Self::Revoked(RevokedLocalMutation {
                failure: error,
                continuation: None,
            })
        } else {
            Self::Blocked(BlockedLocalMutation {
                failure: error,
                reason: LocalMutationBlockReason::Prepublication,
                continuation: None,
            })
        }
    }

    fn from_failed(continuation: LocalPublishedContinuation) -> Self {
        if continuation.failure().revocation().is_some() {
            let failure = continuation.failure().clone();
            Self::Revoked(RevokedLocalMutation {
                failure,
                continuation: Some(continuation),
            })
        } else if let Some(reason) = continuation.failure().retained_block_reason().cloned() {
            let failure = continuation.failure().clone();
            Self::Blocked(BlockedLocalMutation {
                failure,
                reason: LocalMutationBlockReason::Retained(reason),
                continuation: Some(continuation),
            })
        } else {
            Self::Recovering(LocalMutationRecovery::Published(continuation))
        }
    }
}

/// Post-manifest retry state. It owns the original graph handoff guard and the
/// exact immutable publication identity; retry never redrafts or republishes.
struct PublishedContinuationCore {
    guard: PublishedHandoffLatch,
    endpoint: ProjectionEndpointBinding,
    archive: Arc<ObjectStore>,
    batch_id: BatchId,
    origin: BatchOrigin,
    manifest_digest: ContentDigest,
    retained_bytes: usize,
    reservation: Option<TailReservation>,
    identity: Option<super::sqlite::PreparedSqliteIdentityTransition>,
    provider_ingress: bool,
    failure: OperationalCoordinatorError,
}

impl PublishedContinuationCore {
    pub(crate) const fn batch_id(&self) -> BatchId {
        self.batch_id
    }

    pub(crate) const fn phase(&self) -> OperationalPhase {
        self.failure.phase()
    }

    pub(crate) fn failure(&self) -> &OperationalCoordinatorError {
        &self.failure
    }

    fn authorize(
        &mut self,
        admission: &LocalRuntimeAdmission<'_>,
        graph: &Graph,
        engine: &ShardedHotEngine,
    ) -> bool {
        if let Err(error) = authorize_coordinator(admission, graph, engine) {
            self.failure = error;
            false
        } else {
            true
        }
    }

    /// Timing wrapper. ~44s of a 50s import is inside `drain_one` but outside
    /// `stage_archive_batch_bounded` (F44); this bisects whether it is inside
    /// `resume` at all, or in `drain_one`'s other work (notably the
    /// reconciliation scan). Instrumenting here covers every call site at once.
    fn resume(
        &mut self,
        admission: &LocalRuntimeAdmission<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        engine: &mut ShardedHotEngine,
        database: &mut SqliteFrontier,
        tail: &mut TailOverlay,
    ) -> Result<BatchId, OperationalCoordinatorError> {
        if !super::phase_trace_enabled() {
            return self.resume_inner(admission, graph, receipts, engine, database, tail);
        }
        let started = std::time::Instant::now();
        let outcome = self.resume_inner(admission, graph, receipts, engine, database, tail);
        eprintln!(
            "PHASE TIME coordinator.resume {:.1}ms ok={}",
            started.elapsed().as_secs_f64() * 1000.0,
            outcome.is_ok(),
        );
        outcome
    }

    fn resume_inner(
        &mut self,
        admission: &LocalRuntimeAdmission<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        engine: &mut ShardedHotEngine,
        database: &mut SqliteFrontier,
        tail: &mut TailOverlay,
    ) -> Result<BatchId, OperationalCoordinatorError> {
        let mut budget = ResumeBudget::new();
        verify_bindings(graph, receipts, engine, self.endpoint, Some(&self.archive))?;
        self.guard
            .verify_binding(graph, engine.workspace_id(), self.endpoint)
            .map_err(|error| {
                OperationalCoordinatorError::retained_block(
                    OperationalPhase::Bindings,
                    error.to_string(),
                    RetainedBlockReason::StableBinding,
                )
            })?;
        authenticate_published(
            &self.archive,
            self.batch_id,
            self.origin,
            self.manifest_digest,
            self.retained_bytes,
        )?;

        // Reserve enough of the one-resume budget to authenticate/admit every
        // event accepted by this staging slice plus the exact published event
        // if it was accepted on an earlier slice.
        //
        // No unit is reserved for the projection drain. Projection work cannot
        // run ahead of SQLite catch-up, so reserving one would only move an
        // honest continuation from the projection phase to the SQLite phase
        // while pushing total work for the journey above the 16-unit target.
        let already_accepted = self.provider_ingress
            && engine
                .accepted_batch_is_active(self.batch_id)
                .map_err(|error| {
                    OperationalCoordinatorError::new(
                        OperationalPhase::ArchiveStage,
                        error.to_string(),
                    )
                })?;
        let (mut events, stage_has_more) = if already_accepted {
            (Vec::new(), false)
        } else {
            let stage_limit = budget.remaining.saturating_sub(1) / 2;
            reprove_workspace_authority(
                admission,
                WorkspaceAuthorityBoundary::ArchiveStage,
                OperationalPhase::ArchiveStage,
            )?;
            // ArchiveStage charges 77.5% of the slice budget (F43), but ops are
            // not milliseconds, so time the call itself before concluding it
            // dominates wall clock too.
            let stage_started = super::phase_trace_enabled().then(std::time::Instant::now);
            let stage = engine
                .stage_archive_batch_bounded(self.batch_id, stage_limit)
                .map_err(|error| {
                    OperationalCoordinatorError::new(
                        OperationalPhase::ArchiveStage,
                        error.to_string(),
                    )
                })?;
            if let Some(started) = stage_started {
                eprintln!(
                    "PHASE TIME ArchiveStage.stage_archive_batch_bounded {:.1}ms limit={stage_limit}",
                    started.elapsed().as_secs_f64() * 1000.0,
                );
            }
            budget.consume(stage.work(), OperationalPhase::ArchiveStage)?;
            fault(OperationalFaultPoint::AfterStage)?;
            require_accepted_stage_disposition(self.batch_id, &stage.outcome().disposition())?;
            let events = stage
                .outcome()
                .newly_accepted()
                .iter()
                .map(|accepted| {
                    AcceptedBatchEvent::from_accepted(engine, &self.archive, accepted.batch_id)
                        .map_err(|error| {
                            OperationalCoordinatorError::new(
                                OperationalPhase::TailAdmission,
                                error.to_string(),
                            )
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            (events, stage.has_more())
        };
        if self.reservation.is_some()
            && !events.iter().any(|event| event.batch_id() == self.batch_id)
        {
            events.push(
                AcceptedBatchEvent::from_accepted(engine, &self.archive, self.batch_id).map_err(
                    |error| {
                        OperationalCoordinatorError::new(
                            OperationalPhase::TailAdmission,
                            error.to_string(),
                        )
                    },
                )?,
            );
        }
        events.sort_unstable_by_key(AcceptedBatchEvent::acceptance_sequence);
        fault(OperationalFaultPoint::BeforeTailAdmission)?;
        reprove_workspace_authority(
            admission,
            WorkspaceAuthorityBoundary::TailAdmission,
            OperationalPhase::TailAdmission,
        )?;
        for event in events {
            budget.consume(1, OperationalPhase::TailAdmission)?;
            if event.batch_id() == self.batch_id {
                let event =
                    match self.identity.clone() {
                        Some(identity) => event
                            .with_prepared_identity_transition(identity)
                            .map_err(|error| {
                                OperationalCoordinatorError::new(
                                    OperationalPhase::TailAdmission,
                                    error.to_string(),
                                )
                            })?,
                        None => event,
                    };
                if event.retained_bytes() != self.retained_bytes {
                    return Err(OperationalCoordinatorError::retained_block(
                        OperationalPhase::TailAdmission,
                        "published accepted event retained bytes differ from the reserved prepared batch",
                        RetainedBlockReason::PublishedAuthentication,
                    ));
                }
                if let Some(reservation) = self.reservation {
                    tail.enqueue_reserved(reservation, database, engine, event)
                        .map_err(|error| {
                            OperationalCoordinatorError::new(
                                OperationalPhase::TailAdmission,
                                error.to_string(),
                            )
                        })?;
                    self.reservation = None;
                    self.identity = None;
                    continue;
                }
                tail.try_enqueue(database, engine, &event)
                    .map_err(|error| {
                        OperationalCoordinatorError::new(
                            OperationalPhase::TailAdmission,
                            error.to_string(),
                        )
                    })?;
                self.identity = None;
                continue;
            }
            tail.try_enqueue(database, engine, &event)
                .map_err(|error| {
                    OperationalCoordinatorError::new(
                        OperationalPhase::TailAdmission,
                        error.to_string(),
                    )
                })?;
        }
        if self.reservation.is_some() {
            return Err(OperationalCoordinatorError::retained_block(
                OperationalPhase::TailAdmission,
                "published reservation survived tail admission of the published event",
                RetainedBlockReason::PublishedAuthentication,
            ));
        }
        if !self.provider_ingress && self.identity.is_some() {
            return Err(OperationalCoordinatorError::retained_block(
                OperationalPhase::TailAdmission,
                "published local SQLite identity preflight survived accepted tail admission",
                RetainedBlockReason::PublishedAuthentication,
            ));
        }
        fault(OperationalFaultPoint::AfterTailAdmission)?;
        if stage_has_more {
            return Err(OperationalCoordinatorError::continuation_required(
                OperationalPhase::ArchiveStage,
                "bounded staging slice has durable ready/fanout continuation",
            ));
        }

        reprove_workspace_authority(
            admission,
            WorkspaceAuthorityBoundary::SqliteDrain,
            OperationalPhase::SqliteDrain,
        )?;
        let source = RebuildSource::new(engine, &self.archive).map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::SqliteDrain, error.to_string())
        })?;
        let applied = tail
            .drain_ready(database, &source, budget.remaining)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::SqliteDrain, error.to_string())
            })?;
        budget.consume(applied, OperationalPhase::SqliteDrain)?;
        if applied > 0 {
            fault(OperationalFaultPoint::AfterSqliteApply)?;
        }
        let accepted_root = engine.accepted_frontier_root().map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::SqliteDrain, error.to_string())
        })?;
        if database.frontier_root().map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::SqliteDrain, error.to_string())
        })? != accepted_root
        {
            return Err(OperationalCoordinatorError::continuation_required(
                OperationalPhase::SqliteDrain,
                "SQLite bounded slice has durable accepted-sequence continuation",
            ));
        }

        // A guarded projection conflict is deliberately removed from the
        // ready-page index. Correlated cold recovery must authenticate this
        // exact accepted batch before the generic ready scan, or it would
        // mistake durable blocked ownership for successful completion.
        if self.provider_ingress {
            let blocked = correlated_blocked_work(engine, self.batch_id)?;
            if !blocked.is_empty() {
                return Err(OperationalCoordinatorError::retained_block(
                    OperationalPhase::ProjectionDrain,
                    "correlated published mutation has durable guarded projection conflict",
                    RetainedBlockReason::GuardedProjectionConflict,
                ));
            }
        }

        // ~39s of a 50s import is inside resume() but outside ArchiveStage
        // (F45). This loop is the largest remaining block; time it as a whole
        // rather than guessing which of its calls dominates.
        let projection_started = super::phase_trace_enabled().then(std::time::Instant::now);
        let mut projection_iterations = 0_u32;
        loop {
            projection_iterations += 1;
            let work = {
                let page = engine
                    .projection_work_index()
                    .map_err(|error| {
                        OperationalCoordinatorError::new(
                            OperationalPhase::ProjectionDrain,
                            error.to_string(),
                        )
                    })?
                    .ready_page(None, 1)
                    .map_err(|error| {
                        OperationalCoordinatorError::new(
                            OperationalPhase::ProjectionDrain,
                            error.to_string(),
                        )
                    })?;
                page.work().first().cloned()
            };
            let Some(work) = work else {
                if let Some(started) = projection_started {
                    eprintln!(
                        "PHASE TIME ProjectionDrain.loop {:.1}ms iterations={projection_iterations}",
                        started.elapsed().as_secs_f64() * 1000.0,
                    );
                }
                break;
            };
            if budget.remaining == 0 {
                // The loop normally leaves HERE, not via `break` -- budget
                // exhaustion is the common exit, clean drain the rare one. A
                // timer only on the break path caught 2 of 169 slices.
                if let Some(started) = projection_started {
                    eprintln!(
                        "PHASE TIME ProjectionDrain.loop {:.1}ms iterations={projection_iterations} exit=continuation",
                        started.elapsed().as_secs_f64() * 1000.0,
                    );
                }
                return Err(OperationalCoordinatorError::continuation_required(
                    OperationalPhase::ProjectionDrain,
                    "projection bounded slice has ready-work continuation",
                ));
            }
            reprove_workspace_authority(
                admission,
                WorkspaceAuthorityBoundary::ProjectionDrain,
                OperationalPhase::ProjectionDrain,
            )?;
            fault(OperationalFaultPoint::BeforeProjection)?;
            // ProjectionDrain is 58% of a managed import at ~1.45s per occurrence
            // over ~1 iteration each (F45), so a single work item costs about a
            // second. Split fetching the work from executing it.
            let execute_started = super::phase_trace_enabled().then(std::time::Instant::now);
            let executed = super::projection::execute_manifested_projection_work_under_handoff(
                graph,
                receipts,
                engine,
                &work,
                &self.guard,
            );
            if let Some(started) = execute_started {
                eprintln!(
                    "PHASE TIME ProjectionDrain.execute {:.1}ms",
                    started.elapsed().as_secs_f64() * 1000.0,
                );
            }
            executed.map_err(|error| match error {
                ProjectionError::GuardedConflict(error) => {
                    OperationalCoordinatorError::retained_block(
                        OperationalPhase::ProjectionDrain,
                        error.to_string(),
                        RetainedBlockReason::GuardedProjectionConflict,
                    )
                }
                error => OperationalCoordinatorError::new(
                    OperationalPhase::ProjectionDrain,
                    error.to_string(),
                ),
            })?;
            budget.consume(1, OperationalPhase::ProjectionDrain)?;
            fault(OperationalFaultPoint::AfterProjection)?;
        }

        let receiver_endpoint = engine
            .projection_endpoint_binding()
            .ok_or_else(|| {
                OperationalCoordinatorError::new(
                    OperationalPhase::ProjectionDrain,
                    "provider receiver has no enrolled projection endpoint",
                )
            })?
            .endpoint_id();
        let batch = match self.archive.inspect_batch(self.batch_id).map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::ProjectionDrain, error.to_string())
        })? {
            BatchInspection::Ready(batch) => batch,
            BatchInspection::Absent | BatchInspection::Staged { .. } => {
                return Err(OperationalCoordinatorError::new(
                    OperationalPhase::ProjectionDrain,
                    "provider ingress batch became partial before receiver projection",
                ));
            }
        };
        let projection = super::projection_manifest::validate_projection_object_set(
            batch.manifest(),
            batch.objects(),
        )
        .map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::ProjectionDrain, error.to_string())
        })?;
        for source in projection
            .intents()
            .iter()
            .filter(|source| source.source_endpoint_id() != receiver_endpoint)
        {
            reprove_workspace_authority(
                admission,
                WorkspaceAuthorityBoundary::ProjectionDrain,
                OperationalPhase::ProjectionDrain,
            )?;
            let consumed = super::projection::execute_receiver_local_projection_under_handoff(
                graph,
                receipts,
                engine,
                Some(database),
                source,
                &self.guard,
                budget.remaining > 0,
            )
            .map_err(|error| {
                OperationalCoordinatorError::new(
                    OperationalPhase::ProjectionDrain,
                    error.to_string(),
                )
            })?;
            let Some(consumed) = consumed else {
                return Err(OperationalCoordinatorError::continuation_required(
                    OperationalPhase::ProjectionDrain,
                    "bounded receiver-local provider projection has durable continuation",
                ));
            };
            if consumed {
                budget.consume(1, OperationalPhase::ProjectionDrain)?;
            }
        }
        Ok(self.batch_id)
    }
}

fn correlated_blocked_work(
    engine: &ShardedHotEngine,
    batch_id: BatchId,
) -> Result<Vec<ProjectionWork>, OperationalCoordinatorError> {
    let index = engine.projection_work_index().map_err(|error| {
        OperationalCoordinatorError::new(OperationalPhase::ProjectionDrain, error.to_string())
    })?;
    let rows = match index.accepted_batch_work_statuses(batch_id) {
        Ok(rows) => rows,
        Err(super::projection_work_index::ProjectionWorkError::AcceptedWitnessMissing) => {
            return Ok(Vec::new())
        }
        Err(error) => {
            return Err(OperationalCoordinatorError::new(
                OperationalPhase::ProjectionDrain,
                error.to_string(),
            ))
        }
    };
    Ok(rows
        .into_iter()
        .filter_map(|(work, status)| (status == ProjectionWorkStatus::Blocked).then_some(work))
        .collect())
}

fn construct_archive_continuation(
    graph: &Graph,
    engine: &ShardedHotEngine,
    endpoint: ProjectionEndpointBinding,
    archive: Arc<ObjectStore>,
    batch_id: BatchId,
    expected: Option<(BatchOrigin, ContentDigest)>,
    reconstruct_handoff: bool,
) -> Result<PublishedContinuationCore, OperationalCoordinatorError> {
    let validated = match archive.inspect_batch(batch_id).map_err(|error| {
        OperationalCoordinatorError::new(OperationalPhase::Publication, error.to_string())
    })? {
        BatchInspection::Ready(validated) => validated,
        BatchInspection::Absent | BatchInspection::Staged { .. } => {
            return Err(OperationalCoordinatorError::new(
                OperationalPhase::Publication,
                "archive continuation batch is not complete",
            ));
        }
    };
    let manifest_bytes = validated.manifest().encode().map_err(|error| {
        OperationalCoordinatorError::new(OperationalPhase::Publication, error.to_string())
    })?;
    let manifest_digest = ContentDigest::of(&manifest_bytes);
    let origin = validated.manifest().origin();
    if expected.is_some_and(|expected| expected != (origin, manifest_digest)) {
        return Err(OperationalCoordinatorError::retained_block(
            OperationalPhase::Publication,
            "archive continuation manifest identity mismatch",
            RetainedBlockReason::PublishedAuthentication,
        ));
    }
    let retained_bytes =
        validated
            .objects()
            .iter()
            .try_fold(manifest_bytes.len(), |total, object| {
                object
                    .encode()
                    .map_err(|error| {
                        OperationalCoordinatorError::new(
                            OperationalPhase::Publication,
                            error.to_string(),
                        )
                    })
                    .and_then(|bytes| {
                        total.checked_add(bytes.len()).ok_or_else(|| {
                            OperationalCoordinatorError::new(
                                OperationalPhase::Publication,
                                "archive continuation retained-byte count overflowed",
                            )
                        })
                    })
            })?;
    let handoff = if reconstruct_handoff {
        graph.reconstruct_published_handoff_safe(engine.workspace_id(), endpoint)
    } else {
        graph.mint_handoff_safe(engine.workspace_id(), endpoint)
    }
    .map_err(|error| {
        OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
    })?;
    let guard = handoff.into_publisher_guard();
    guard
        .verify_binding(graph, engine.workspace_id(), endpoint)
        .map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
        })?;
    Ok(PublishedContinuationCore {
        guard: guard.into_published_latch(),
        endpoint,
        archive,
        batch_id,
        origin,
        manifest_digest,
        retained_bytes,
        reservation: None,
        identity: None,
        provider_ingress: true,
        failure: OperationalCoordinatorError::new(
            OperationalPhase::ArchiveStage,
            "archive continuation has not completed its first bounded slice",
        ),
    })
}

fn require_accepted_stage_disposition(
    batch_id: BatchId,
    disposition: &BatchDisposition,
) -> Result<(), OperationalCoordinatorError> {
    match disposition {
        BatchDisposition::Accepted { .. } | BatchDisposition::DuplicateAccepted { .. } => Ok(()),
        // Carry the counts the variant already holds. Without them this failure
        // reads identically whether one object is missing or ten thousand, and
        // whether it is waiting on a dependency batch or on object bytes -- which
        // are different bugs with different fixes. The retry loop above surfaces
        // only this string, so anything dropped here is unrecoverable downstream.
        // `{0, []}` is the compact continuation sentinel a bounded slice returns
        // when its work budget is spent (see hot_engine's
        // `compact_incomplete_staged_disposition`): nothing is missing, there is
        // simply more to do. Anything else is genuine incompleteness.
        BatchDisposition::IncompleteStaged {
            missing_objects: 0,
            missing_dependencies,
        } if missing_dependencies.is_empty() => {
            Err(OperationalCoordinatorError::continuation_required(
                OperationalPhase::ArchiveStage,
                format!("bounded staging slice for {batch_id} needs another resume"),
            ))
        }
        BatchDisposition::IncompleteStaged {
            missing_objects,
            missing_dependencies,
        } => Err(OperationalCoordinatorError::new(
            OperationalPhase::ArchiveStage,
            format!(
                "bounded staging slice for {batch_id} retains dependency/work continuation: \
                 {missing_objects} missing objects, {} missing dependencies{}",
                missing_dependencies.len(),
                match missing_dependencies.first() {
                    Some(first) => format!(" (first: {first})"),
                    None => String::new(),
                }
            ),
        )),
        BatchDisposition::Rejected { error } => Err(OperationalCoordinatorError::retained_block(
            OperationalPhase::ArchiveStage,
            format!("published mutation {batch_id} was rejected: {error}"),
            RetainedBlockReason::Rejected(error.clone()),
        )),
        BatchDisposition::Quarantined => Err(OperationalCoordinatorError::retained_block(
            OperationalPhase::ArchiveStage,
            format!("published mutation {batch_id} was quarantined"),
            RetainedBlockReason::Quarantined,
        )),
    }
}

/// Affine external-reconciliation continuation. Only this type exposes an
/// import identity and the external retry API.
pub(crate) struct ExternalPublishedContinuation {
    import_id: ImportId,
    core: PublishedContinuationCore,
}

/// Compatibility name for the existing external reconciliation session.
/// Local mutation continuations are a distinct type and cannot enter it.
pub(crate) type FailedClosedOperationalCoordinator = ExternalPublishedContinuation;

impl ExternalPublishedContinuation {
    pub(crate) const fn batch_id(&self) -> BatchId {
        self.core.batch_id()
    }

    pub(crate) const fn import_id(&self) -> ImportId {
        self.import_id
    }

    pub(crate) const fn phase(&self) -> OperationalPhase {
        self.core.phase()
    }

    pub(crate) fn failure(&self) -> &OperationalCoordinatorError {
        self.core.failure()
    }

    #[cfg(test)]
    const fn retained_bytes(&self) -> usize {
        self.core.retained_bytes
    }

    pub(crate) fn retry(
        mut self,
        admission: &LocalRuntimeAdmission<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        engine: &mut ShardedHotEngine,
        database: &mut SqliteFrontier,
        tail: &mut TailOverlay,
    ) -> OperationalCoordinatorState {
        if !self.core.authorize(admission, graph, engine) {
            return OperationalCoordinatorState::FailedClosed(self);
        }
        match self
            .core
            .resume(admission, graph, receipts, engine, database, tail)
        {
            Ok(batch_id) => {
                self.core.guard.complete();
                OperationalCoordinatorState::Complete(OperationalCompletion {
                    batch_id,
                    import_id: self.import_id,
                })
            }
            Err(error) => {
                self.core.failure = error;
                OperationalCoordinatorState::FailedClosed(self)
            }
        }
    }
}

/// Affine admitted-local continuation. It has no import accessor and exposes
/// only the local retry API.
pub(crate) struct LocalPublishedContinuation {
    core: PublishedContinuationCore,
}

impl LocalPublishedContinuation {
    pub(crate) const fn batch_id(&self) -> BatchId {
        self.core.batch_id()
    }

    pub(crate) const fn phase(&self) -> OperationalPhase {
        self.core.phase()
    }

    pub(crate) fn failure(&self) -> &OperationalCoordinatorError {
        self.core.failure()
    }

    pub(crate) fn retry(
        mut self,
        admission: &LocalRuntimeAdmission<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        engine: &mut ShardedHotEngine,
        database: &mut SqliteFrontier,
        tail: &mut TailOverlay,
    ) -> LocalMutationCoordinatorState {
        if !self.core.authorize(admission, graph, engine) {
            return LocalMutationCoordinatorState::from_failed(self);
        }
        match self
            .core
            .resume(admission, graph, receipts, engine, database, tail)
        {
            Ok(batch_id) => {
                self.core.guard.complete();
                LocalMutationCoordinatorState::Active(LocalMutationCompletion { batch_id })
            }
            Err(error) => {
                self.core.failure = error;
                LocalMutationCoordinatorState::from_failed(self)
            }
        }
    }
}

pub(crate) struct OperationalCoordinator;

/// Result after the clean runtime's only irreversible boundary. A pending
/// continuation means the manifest is already the durable operation commit;
/// only disposable SQLite and/or exact Markdown projection remains.
pub(crate) enum CleanLocalMutationState {
    Complete(BatchId),
    DurablePending(CleanPublishedContinuation),
}

pub(crate) enum CleanExternalMutationState {
    Noop,
    Complete(BatchId),
    DurablePending(CleanPublishedContinuation),
}

pub(crate) struct CleanPublishedContinuation {
    guard: PublishedHandoffLatch,
    batch_id: BatchId,
    identity: super::sqlite::PreparedSqliteIdentityTransition,
    failure: OperationalCoordinatorError,
}

impl CleanPublishedContinuation {
    pub(crate) const fn batch_id(&self) -> BatchId {
        self.batch_id
    }

    pub(crate) fn failure(&self) -> &OperationalCoordinatorError {
        &self.failure
    }
}

pub(crate) enum ProviderArchiveIngress {
    Complete,
    Pending(ProviderArchiveContinuation),
}

pub(crate) struct ProviderArchiveContinuation {
    core: PublishedContinuationCore,
}

impl ProviderArchiveContinuation {
    pub(crate) const fn batch_id(&self) -> BatchId {
        self.core.batch_id()
    }

    pub(crate) fn failure(&self) -> &OperationalCoordinatorError {
        self.core.failure()
    }
}

fn execute_clean_local_inner(
    session: &mut CleanRuntimeSession<'_>,
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    transaction: &OperationTransaction,
    batch_id: Option<BatchId>,
    persist_fingerprint: Option<&mut dyn FnMut(ContentDigest) -> Result<(), String>>,
) -> Result<CleanLocalMutationState, OperationalCoordinatorError> {
    let (admission, engine, database) = session.parts().map_err(|refusal| {
        OperationalCoordinatorError::revoked(OperationalPhase::Bindings, refusal)
    })?;
    let claim_source = database.materialized_read().map_err(|error| {
        OperationalCoordinatorError::new(
            OperationalPhase::Draft,
            format!("clean SQLite identity candidates are unavailable: {error}"),
        )
    })?;
    let mut prepared = match prepare_local_inner(
        &admission,
        graph,
        receipts,
        engine,
        LocalDraftSource::Promoted { batch_id },
        LocalPreparationBinding::TrustedLocal,
        transaction,
        None,
        Some(&claim_source),
    )? {
        PreparedLocalMutationState::Prepared(prepared) => prepared,
        PreparedLocalMutationState::ReconciliationRequired(_) => {
            return Err(OperationalCoordinatorError::new(
                OperationalPhase::Planning,
                "clean local mutation requires external reconciliation before publication",
            ));
        }
    };
    drop(claim_source);
    prepared.preflight_identity(database, engine)?;
    if let Some(persist_fingerprint) = persist_fingerprint {
        let manifest_bytes = prepared.prepared.manifest().encode().map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::Finalize, error.to_string())
        })?;
        persist_fingerprint(ContentDigest::of(&manifest_bytes)).map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::Publication, error)
        })?;
    }
    let PreparedLocalMutation {
        endpoint: _,
        archive: _,
        guard,
        prepared,
        batch_id,
        identity,
    } = prepared;
    let identity = identity.expect("clean local publication requires identity preflight");
    reprove_workspace_authority(
        &admission,
        WorkspaceAuthorityBoundary::Publication,
        OperationalPhase::Publication,
    )?;
    let archive = engine.archive_store_capability().ok_or_else(|| {
        OperationalCoordinatorError::new(
            OperationalPhase::ArchiveStage,
            "clean runtime has no retained operation archive",
        )
    })?;
    let published = guard.into_published_latch();
    let commit_claim_source = database.materialized_read().map_err(|error| {
        OperationalCoordinatorError::new(
            OperationalPhase::Publication,
            format!("clean SQLite identity candidates are unavailable: {error}"),
        )
    })?;
    let outcome = match engine.commit_clean_prepared(&prepared, &commit_claim_source) {
        Ok(outcome) => outcome,
        Err(error) => {
            let failure =
                OperationalCoordinatorError::new(OperationalPhase::Publication, error.to_string());
            if matches!(archive.inspect_batch(batch_id), Ok(BatchInspection::Absent)) {
                published.cancel_prepublication();
                return Err(failure);
            }
            return Ok(CleanLocalMutationState::DurablePending(
                CleanPublishedContinuation {
                    guard: published,
                    batch_id,
                    identity,
                    failure,
                },
            ));
        }
    };
    drop(commit_claim_source);
    if !matches!(outcome.disposition(), BatchDisposition::Accepted { .. }) {
        return Err(OperationalCoordinatorError::new(
            OperationalPhase::ArchiveStage,
            "clean manifest commit did not leave one accepted operation",
        ));
    }
    let mut continuation = CleanPublishedContinuation {
        guard: published,
        batch_id,
        identity,
        failure: OperationalCoordinatorError::new(
            OperationalPhase::SqliteDrain,
            "durable clean operation is awaiting derived-state application",
        ),
    };
    if let Err(error) = fault(OperationalFaultPoint::AfterManifest) {
        continuation.failure = error;
        return Ok(CleanLocalMutationState::DurablePending(continuation));
    }
    match resume_clean_published(&admission, graph, receipts, engine, database, &continuation) {
        Ok(()) => {
            continuation.guard.complete();
            Ok(CleanLocalMutationState::Complete(batch_id))
        }
        Err(error) => {
            continuation.failure = error;
            Ok(CleanLocalMutationState::DurablePending(continuation))
        }
    }
}

impl OperationalCoordinator {
    /// Execute one local semantic operation through the clean
    /// baseline-plus-manifest runtime. Validation, exact graph capture and the
    /// SQLite identity transition are completed at frontier F before the
    /// manifest commit. After that commit, SQLite and Markdown projection are
    /// recoverable derived work and are retained in an affine continuation on
    /// failure.
    pub(crate) fn execute_clean_local(
        session: &mut CleanRuntimeSession<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        transaction: &OperationTransaction,
    ) -> Result<CleanLocalMutationState, OperationalCoordinatorError> {
        execute_clean_local_inner(session, graph, receipts, transaction, None, None)
    }

    /// Execute one clean local mutation with a stable application-owned batch
    /// identity. The immutable episode record is published before the manifest
    /// commit, so a crash can distinguish a retry of the same move from an
    /// unrelated batch collision without creating a second semantic edit.
    pub(crate) fn execute_clean_local_correlated(
        session: &mut CleanRuntimeSession<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        batch_id: BatchId,
        transaction: &OperationTransaction,
        mut persist_fingerprint: impl FnMut(ContentDigest) -> Result<(), String>,
    ) -> Result<CleanLocalMutationState, OperationalCoordinatorError> {
        execute_clean_local_inner(
            session,
            graph,
            receipts,
            transaction,
            Some(batch_id),
            Some(&mut persist_fingerprint),
        )
    }

    pub(crate) fn retry_clean_local(
        session: &mut CleanRuntimeSession<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        mut continuation: CleanPublishedContinuation,
    ) -> CleanLocalMutationState {
        let (admission, engine, database) = match session.parts() {
            Ok(parts) => parts,
            Err(refusal) => {
                continuation.failure =
                    OperationalCoordinatorError::revoked(OperationalPhase::Bindings, refusal);
                return CleanLocalMutationState::DurablePending(continuation);
            }
        };
        match resume_clean_published(&admission, graph, receipts, engine, database, &continuation) {
            Ok(()) => {
                let batch_id = continuation.batch_id;
                continuation.guard.complete();
                CleanLocalMutationState::Complete(batch_id)
            }
            Err(error) => {
                continuation.failure = error;
                CleanLocalMutationState::DurablePending(continuation)
            }
        }
    }

    /// Validate and commit one provider-delivered clean batch without first
    /// placing its manifest in the accepted archive. Provider bytes are only
    /// transport evidence; the manifest becomes authority at the same
    /// commit-last boundary used by local and external authoring.
    pub(crate) fn ingest_clean_prepared(
        session: &mut CleanRuntimeSession<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        prepared: &PreparedBatch,
    ) -> Result<CleanLocalMutationState, OperationalCoordinatorError> {
        let (admission, engine, database) = session.parts().map_err(|refusal| {
            OperationalCoordinatorError::revoked(OperationalPhase::Bindings, refusal)
        })?;
        authorize_coordinator(&admission, graph, engine)?;
        let endpoint = engine.projection_endpoint_binding().ok_or_else(|| {
            OperationalCoordinatorError::new(
                OperationalPhase::Bindings,
                "clean engine has no projection endpoint",
            )
        })?;
        verify_projection_bindings(graph, receipts, engine, endpoint)?;
        let handoff = graph
            .mint_handoff_safe(engine.workspace_id(), endpoint)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
            })?;
        handoff
            .verify_binding(graph, engine.workspace_id(), endpoint)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
            })?;
        let identity = database
            .preflight_prepared_identity_transition(engine, prepared)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Finalize, error.to_string())
            })?;
        reprove_workspace_authority(
            &admission,
            WorkspaceAuthorityBoundary::Publication,
            OperationalPhase::Publication,
        )?;
        let archive = engine.archive_store_capability().ok_or_else(|| {
            OperationalCoordinatorError::new(
                OperationalPhase::ArchiveStage,
                "clean runtime has no retained operation archive",
            )
        })?;
        let published = handoff.into_publisher_guard().into_published_latch();
        let batch_id = prepared.manifest().batch_id();
        let commit_claim_source = database.materialized_read().map_err(|error| {
            OperationalCoordinatorError::new(
                OperationalPhase::Publication,
                format!("clean SQLite identity candidates are unavailable: {error}"),
            )
        })?;
        let outcome = match engine.commit_clean_prepared(&prepared, &commit_claim_source) {
            Ok(outcome) => outcome,
            Err(error) => {
                let failure = OperationalCoordinatorError::new(
                    OperationalPhase::Publication,
                    error.to_string(),
                );
                if matches!(archive.inspect_batch(batch_id), Ok(BatchInspection::Absent)) {
                    published.cancel_prepublication();
                    return Err(failure);
                }
                return Ok(CleanLocalMutationState::DurablePending(
                    CleanPublishedContinuation {
                        guard: published,
                        batch_id,
                        identity,
                        failure,
                    },
                ));
            }
        };
        drop(commit_claim_source);
        if !matches!(outcome.disposition(), BatchDisposition::Accepted { .. }) {
            return Err(OperationalCoordinatorError::new(
                OperationalPhase::ArchiveStage,
                "clean provider manifest did not leave one accepted operation",
            ));
        }
        let mut continuation = CleanPublishedContinuation {
            guard: published,
            batch_id,
            identity,
            failure: OperationalCoordinatorError::new(
                OperationalPhase::SqliteDrain,
                "durable clean provider operation is awaiting derived-state application",
            ),
        };
        #[cfg(test)]
        if let Err(error) = fault(OperationalFaultPoint::AfterManifest) {
            continuation.failure = error;
            return Ok(CleanLocalMutationState::DurablePending(continuation));
        }
        match resume_clean_published(&admission, graph, receipts, engine, database, &continuation) {
            Ok(()) => {
                continuation.guard.complete();
                Ok(CleanLocalMutationState::Complete(batch_id))
            }
            Err(error) => {
                continuation.failure = error;
                Ok(CleanLocalMutationState::DurablePending(continuation))
            }
        }
    }

    /// Reconcile exact externally changed graph paths through the clean
    /// baseline-plus-manifest authority.  Planning reuses the established
    /// structural matcher, but its predecessor evidence is reconstructed from
    /// the lazy-genesis baseline and accepted manifests rather than the
    /// persistent Patricia work index.
    pub(crate) fn execute_clean_external(
        session: &mut CleanRuntimeSession<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        requested_paths: &[&str],
    ) -> Result<CleanExternalMutationState, OperationalCoordinatorError> {
        let (admission, engine, database) = session.parts().map_err(|refusal| {
            OperationalCoordinatorError::revoked(OperationalPhase::Bindings, refusal)
        })?;
        authorize_coordinator(&admission, graph, engine)?;
        let endpoint = engine.projection_endpoint_binding().ok_or_else(|| {
            OperationalCoordinatorError::new(
                OperationalPhase::Bindings,
                "clean engine has no projection endpoint",
            )
        })?;
        verify_projection_bindings(graph, receipts, engine, endpoint)?;
        let archive = engine.archive_store_capability().ok_or_else(|| {
            OperationalCoordinatorError::new(
                OperationalPhase::ArchiveStage,
                "clean runtime has no retained operation archive",
            )
        })?;
        let handoff = graph
            .mint_handoff_safe(engine.workspace_id(), endpoint)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
            })?;
        handoff
            .verify_binding(graph, engine.workspace_id(), endpoint)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
            })?;
        fault(OperationalFaultPoint::AfterHandoff)?;
        let plan = plan_clean_affected_import(graph, engine, database, requested_paths);
        fault(OperationalFaultPoint::AfterPlan)?;
        match plan.status() {
            ImportPlanStatus::Noop => {
                handoff.cancel();
                return Ok(CleanExternalMutationState::Noop);
            }
            ImportPlanStatus::Blocked => {
                handoff.cancel();
                let detail = plan
                    .blocks()
                    .first()
                    .map(|blocked| blocked.detail.clone())
                    .unwrap_or_else(|| "clean external reconciliation was blocked".into());
                return Err(OperationalCoordinatorError::new(
                    OperationalPhase::Planning,
                    detail,
                ));
            }
            ImportPlanStatus::Reconcile => {}
        }
        let guard = handoff.into_publisher_guard();
        guard
            .verify_binding(graph, engine.workspace_id(), endpoint)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
            })?;
        let material = plan.into_execution_material().map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::Planning, error.to_string())
        })?;
        let import_id = material.import_id();
        let claim_source = database.materialized_read().map_err(|error| {
            OperationalCoordinatorError::new(
                OperationalPhase::Draft,
                format!("clean SQLite identity candidates are unavailable: {error}"),
            )
        })?;
        let (author, draft) = draft_with_bounded_peer_candidates(
            engine,
            endpoint,
            &material,
            Some(&claim_source),
            |attempt| {
                CrdtPeerId::external_import_candidate(engine.workspace_id(), import_id, attempt)
            },
        )?;
        drop(claim_source);
        fault(OperationalFaultPoint::AfterDraft)?;
        let captured = engine
            .capture_external_author_transaction(draft, graph, receipts, endpoint)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Capture, error.to_string())
            })?;
        fault(OperationalFaultPoint::AfterCapture)?;
        let prepared = engine
            .finalize_captured_author_transaction(captured, receipts)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Finalize, error.to_string())
            })?;
        if prepared.manifest().batch_id() != author.batch_id
            || prepared.manifest().origin() != (BatchOrigin::ExternalReconciliation { import_id })
        {
            return Err(OperationalCoordinatorError::new(
                OperationalPhase::Finalize,
                "clean external batch lost its import identity",
            ));
        }
        fault(OperationalFaultPoint::AfterFinalize)?;
        let identity = database
            .preflight_prepared_identity_transition(engine, &prepared)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Finalize, error.to_string())
            })?;
        reprove_workspace_authority(
            &admission,
            WorkspaceAuthorityBoundary::Publication,
            OperationalPhase::Publication,
        )?;
        let published = guard.into_published_latch();
        let batch_id = author.batch_id;
        let commit_claim_source = database.materialized_read().map_err(|error| {
            OperationalCoordinatorError::new(
                OperationalPhase::Publication,
                format!("clean SQLite identity candidates are unavailable: {error}"),
            )
        })?;
        let outcome = match engine.commit_clean_prepared(&prepared, &commit_claim_source) {
            Ok(outcome) => outcome,
            Err(error) => {
                let failure = OperationalCoordinatorError::new(
                    OperationalPhase::Publication,
                    error.to_string(),
                );
                if matches!(archive.inspect_batch(batch_id), Ok(BatchInspection::Absent)) {
                    published.cancel_prepublication();
                    return Err(failure);
                }
                return Ok(CleanExternalMutationState::DurablePending(
                    CleanPublishedContinuation {
                        guard: published,
                        batch_id,
                        identity,
                        failure,
                    },
                ));
            }
        };
        drop(commit_claim_source);
        if !matches!(outcome.disposition(), BatchDisposition::Accepted { .. }) {
            return Err(OperationalCoordinatorError::new(
                OperationalPhase::ArchiveStage,
                "clean external manifest did not leave one accepted operation",
            ));
        }
        let mut continuation = CleanPublishedContinuation {
            guard: published,
            batch_id,
            identity,
            failure: OperationalCoordinatorError::new(
                OperationalPhase::SqliteDrain,
                "durable clean external operation is awaiting derived-state application",
            ),
        };
        if let Err(error) = fault(OperationalFaultPoint::AfterManifest) {
            continuation.failure = error;
            return Ok(CleanExternalMutationState::DurablePending(continuation));
        }
        match resume_clean_published(&admission, graph, receipts, engine, database, &continuation) {
            Ok(()) => {
                continuation.guard.complete();
                Ok(CleanExternalMutationState::Complete(batch_id))
            }
            Err(error) => {
                continuation.failure = error;
                Ok(CleanExternalMutationState::DurablePending(continuation))
            }
        }
    }

    /// Execute one bounded external reconciliation.
    ///
    /// `admission` is the new-architecture write gate: it is derived only from a
    /// live [`LocalActiveAuthority`](super::local_active::LocalActiveAuthority)
    /// permit, and it revalidates the enrolled graph/endpoint/device binding
    /// against this exact live graph and engine before any authoritative,
    /// projection, or SQLite work is admitted.
    pub(crate) fn execute(
        admission: &LocalRuntimeAdmission<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        engine: &mut ShardedHotEngine,
        database: &mut SqliteFrontier,
        tail: &mut TailOverlay,
        requested_paths: &[&str],
    ) -> Result<OperationalCoordinatorState, OperationalCoordinatorError> {
        authorize_coordinator(admission, graph, engine)?;
        let endpoint = engine.projection_endpoint_binding().ok_or_else(|| {
            OperationalCoordinatorError::new(
                OperationalPhase::Bindings,
                "engine has no enrolled projection endpoint",
            )
        })?;
        let archive = verify_bindings(graph, receipts, engine, endpoint, None)?;
        let handoff = graph
            .mint_handoff_safe(engine.workspace_id(), endpoint)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
            })?;
        handoff
            .verify_binding(graph, engine.workspace_id(), endpoint)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
            })?;
        fault(OperationalFaultPoint::AfterHandoff)?;

        let plan = plan_affected_import(graph, receipts, engine, requested_paths);
        fault(OperationalFaultPoint::AfterPlan)?;
        match plan.status() {
            ImportPlanStatus::Blocked => {
                handoff.cancel();
                return Ok(OperationalCoordinatorState::Blocked(plan));
            }
            ImportPlanStatus::Noop => {
                if let Some(formatting) = plan.into_formatting_material() {
                    let guard = handoff.into_publisher_guard();
                    for page in formatting.pages() {
                        if let Err(error) = super::projection::adopt_existing_projection_formatting(
                            graph,
                            receipts,
                            engine,
                            &guard,
                            page.page_id(),
                            page.bytes(),
                            page.annotations(),
                        ) {
                            return Err(OperationalCoordinatorError::new(
                                OperationalPhase::Planning,
                                format!(
                                    "formatting-only baseline adoption for {} failed: {error}",
                                    page.path()
                                ),
                            ));
                        }
                    }
                    drop(guard);
                    return Ok(OperationalCoordinatorState::Noop);
                }
                handoff.cancel();
                return Ok(OperationalCoordinatorState::Noop);
            }
            ImportPlanStatus::Reconcile => {}
        }

        let guard = handoff.into_publisher_guard();
        guard
            .verify_binding(graph, engine.workspace_id(), endpoint)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
            })?;
        let material = plan.into_execution_material().map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::Planning, error.to_string())
        })?;
        let import_id = material.import_id();
        if material.origin() != (BatchOrigin::ExternalReconciliation { import_id }) {
            return Err(OperationalCoordinatorError::new(
                OperationalPhase::Planning,
                "sealed execution material lost its external-import identity",
            ));
        }
        let (author, draft) =
            draft_with_bounded_peer_candidates(engine, endpoint, &material, None, |attempt| {
                CrdtPeerId::external_import_candidate(engine.workspace_id(), import_id, attempt)
            })?;
        fault(OperationalFaultPoint::AfterDraft)?;
        let captured = engine
            .capture_external_author_transaction(draft, graph, receipts, endpoint)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Capture, error.to_string())
            })?;
        fault(OperationalFaultPoint::AfterCapture)?;
        let prepared = engine
            .finalize_captured_author_transaction(captured, receipts)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Finalize, error.to_string())
            })?;
        if prepared.manifest().batch_id() != author.batch_id
            || prepared.manifest().origin() != (BatchOrigin::ExternalReconciliation { import_id })
        {
            return Err(OperationalCoordinatorError::new(
                OperationalPhase::Finalize,
                "finalized batch lost its exact external-import identity",
            ));
        }
        fault(OperationalFaultPoint::AfterFinalize)?;
        let identity = database
            .preflight_prepared_identity_transition(engine, &prepared)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Finalize, error.to_string())
            })?;
        let origin = BatchOrigin::ExternalReconciliation { import_id };
        match publish_and_drain(
            admission,
            graph,
            receipts,
            engine,
            database,
            tail,
            endpoint,
            archive,
            guard,
            prepared,
            author.batch_id,
            origin,
            identity,
        )? {
            PublishedPipelineState::Complete(batch_id) => Ok(
                OperationalCoordinatorState::Complete(OperationalCompletion {
                    batch_id,
                    import_id,
                }),
            ),
            PublishedPipelineState::FailedClosed(continuation) => Ok(
                OperationalCoordinatorState::FailedClosed(ExternalPublishedContinuation {
                    import_id,
                    core: continuation,
                }),
            ),
        }
    }

    /// Raw-author escape hatch for deterministic pre-enrollment fixtures.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn execute_local_with_author(
        admission: &LocalRuntimeAdmission<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        engine: &mut ShardedHotEngine,
        database: &mut SqliteFrontier,
        tail: &mut TailOverlay,
        author: AuthorBatch,
        transaction: &OperationTransaction,
    ) -> LocalMutationCoordinatorState {
        match execute_local_inner(
            admission,
            graph,
            receipts,
            engine,
            database,
            tail,
            LocalDraftSource::Raw(author),
            transaction,
        ) {
            Ok(state) => state,
            Err(error) => LocalMutationCoordinatorState::blocked(error),
        }
    }

    #[cfg(test)]
    pub(super) fn prepare_trusted_local_with_author(
        admission: &LocalRuntimeAdmission<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        engine: &mut ShardedHotEngine,
        author: AuthorBatch,
        transaction: &OperationTransaction,
    ) -> Result<PreparedLocalMutationState, OperationalCoordinatorError> {
        prepare_local_inner(
            admission,
            graph,
            receipts,
            engine,
            LocalDraftSource::Raw(author),
            LocalPreparationBinding::TrustedLocal,
            transaction,
            None,
            None,
        )
    }
}

enum LocalDraftSource {
    Promoted {
        batch_id: Option<BatchId>,
    },
    #[cfg(test)]
    Raw(AuthorBatch),
}

#[derive(Clone, Copy)]
enum LocalPreparationBinding {
    SlowPipeline,
    TrustedLocal,
}

pub(crate) enum PreparedLocalMutationState {
    Prepared(PreparedLocalMutation),
    ReconciliationRequired(ReconciliationNeeded),
}

/// Finalized local-author transaction plus the exact graph handoff retained
/// from draft capture. Construction stays in the established local
/// coordinator; the trusted-local commit consumes it and deliberately releases
/// the old pipeline handoff before entering the journal-authorized path guard.
pub(crate) struct PreparedLocalMutation {
    endpoint: ProjectionEndpointBinding,
    archive: Option<Arc<ObjectStore>>,
    guard: HandoffSafeGuard,
    prepared: PreparedBatch,
    batch_id: BatchId,
    identity: Option<super::sqlite::PreparedSqliteIdentityTransition>,
}

impl PreparedLocalMutation {
    pub(crate) const fn batch_id(&self) -> BatchId {
        self.batch_id
    }

    pub(crate) const fn prepared_batch(&self) -> &PreparedBatch {
        &self.prepared
    }

    fn preflight_identity(
        &mut self,
        database: &SqliteFrontier,
        engine: &ShardedHotEngine,
    ) -> Result<(), OperationalCoordinatorError> {
        if self.identity.is_some() {
            return Err(OperationalCoordinatorError::new(
                OperationalPhase::Finalize,
                "prepared local mutation was identity-preflighted twice",
            ));
        }
        self.identity = Some(
            database
                .preflight_prepared_identity_transition(engine, &self.prepared)
                .map_err(|error| {
                    OperationalCoordinatorError::new(OperationalPhase::Finalize, error.to_string())
                })?,
        );
        Ok(())
    }

    pub(super) fn into_trusted_batch(self) -> PreparedBatch {
        let Self {
            endpoint: _,
            archive: _,
            guard,
            prepared,
            batch_id: _,
            identity: _,
        } = self;
        drop(guard);
        prepared
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_local_inner(
    admission: &LocalRuntimeAdmission<'_>,
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    engine: &mut ShardedHotEngine,
    source: LocalDraftSource,
    binding: LocalPreparationBinding,
    transaction: &OperationTransaction,
    prepared_editor_projection: Option<super::projection::PreparedEditorProjection>,
    claim_source: Option<&dyn super::hot_engine::ProjectionClaimSource>,
) -> Result<PreparedLocalMutationState, OperationalCoordinatorError> {
    #[cfg(test)]
    let bindings_started = Instant::now();
    authorize_coordinator(admission, graph, engine)?;
    let endpoint = engine.projection_endpoint_binding().ok_or_else(|| {
        OperationalCoordinatorError::new(
            OperationalPhase::Bindings,
            "engine has no enrolled projection endpoint",
        )
    })?;
    let archive = match binding {
        LocalPreparationBinding::SlowPipeline => {
            Some(verify_bindings(graph, receipts, engine, endpoint, None)?)
        }
        LocalPreparationBinding::TrustedLocal => {
            verify_projection_bindings(graph, receipts, engine, endpoint)?;
            None
        }
    };
    let handoff = graph
        .mint_handoff_safe(engine.workspace_id(), endpoint)
        .map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
        })?;
    handoff
        .verify_binding(graph, engine.workspace_id(), endpoint)
        .map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
        })?;
    fault(OperationalFaultPoint::AfterHandoff)?;
    let guard = handoff.into_publisher_guard();
    guard
        .verify_binding(graph, engine.workspace_id(), endpoint)
        .map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
        })?;
    #[cfg(test)]
    note_trusted_local_preparation_stage(|timings| {
        timings.bindings = bindings_started.elapsed();
    });

    #[cfg(test)]
    let draft_started = Instant::now();
    let (batch_id, author_device_id, author_session_id, draft) = match source {
        LocalDraftSource::Promoted { batch_id } => {
            let authority = admission
                .mint_local_author_authority(graph, engine, endpoint)
                .map_err(classify_authorization_failure)?;
            let author_device_id = authority.device_id();
            let author_session_id = authority.session_id();
            let (batch_id, draft) = match batch_id {
                Some(batch_id) => engine.draft_admitted_local_author_transaction_with_batch_id(
                    &authority,
                    batch_id,
                    transaction,
                    prepared_editor_projection,
                    claim_source,
                ),
                None => engine.draft_admitted_local_author_transaction(
                    &authority,
                    transaction,
                    prepared_editor_projection,
                    claim_source,
                ),
            }
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Draft, error.to_string())
            })?;
            (batch_id, author_device_id, author_session_id, draft)
        }
        #[cfg(test)]
        LocalDraftSource::Raw(author) => {
            if author.author_device_id != endpoint.device_id() {
                return Err(OperationalCoordinatorError::new(
                    OperationalPhase::Bindings,
                    "local author device does not match the admitted projection endpoint",
                ));
            }
            let draft = engine
                .draft_author_transaction(author, BatchOrigin::LocalMutation, transaction)
                .map_err(|error| {
                    OperationalCoordinatorError::new(OperationalPhase::Draft, error.to_string())
                })?;
            (
                author.batch_id,
                author.author_device_id,
                author.author_session_id,
                draft,
            )
        }
    };
    fault(OperationalFaultPoint::AfterDraft)?;
    #[cfg(test)]
    note_trusted_local_preparation_stage(|timings| {
        timings.draft = draft_started.elapsed();
    });
    #[cfg(test)]
    let capture_started = Instant::now();
    let captured = match engine
        .capture_local_author_transaction(draft, graph, receipts, endpoint)
        .map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::Capture, error.to_string())
        })? {
        LocalAuthorCapture::Captured(captured) => captured,
        LocalAuthorCapture::ReconciliationNeeded(reconciliation) => {
            drop(guard);
            return Ok(PreparedLocalMutationState::ReconciliationRequired(
                reconciliation,
            ));
        }
    };
    fault(OperationalFaultPoint::AfterCapture)?;
    #[cfg(test)]
    note_trusted_local_preparation_stage(|timings| {
        timings.capture = capture_started.elapsed();
    });
    #[cfg(test)]
    let finalize_started = Instant::now();
    let prepared = engine
        .finalize_captured_author_transaction(captured, receipts)
        .map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::Finalize, error.to_string())
        })?;
    let manifest = prepared.manifest();
    if manifest.batch_id() != batch_id
        || manifest.author_device_id() != author_device_id
        || manifest.author_session_id() != author_session_id
        || manifest.origin() != BatchOrigin::LocalMutation
    {
        return Err(OperationalCoordinatorError::new(
            OperationalPhase::Finalize,
            "finalized batch lost its exact local-author identity",
        ));
    }
    fault(OperationalFaultPoint::AfterFinalize)?;
    #[cfg(test)]
    note_trusted_local_preparation_stage(|timings| {
        timings.finalize = finalize_started.elapsed();
    });
    Ok(PreparedLocalMutationState::Prepared(
        PreparedLocalMutation {
            endpoint,
            archive,
            guard,
            prepared,
            batch_id,
            identity: None,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
fn execute_local_inner(
    admission: &LocalRuntimeAdmission<'_>,
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    engine: &mut ShardedHotEngine,
    database: &mut SqliteFrontier,
    tail: &mut TailOverlay,
    source: LocalDraftSource,
    transaction: &OperationTransaction,
) -> Result<LocalMutationCoordinatorState, OperationalCoordinatorError> {
    let mut prepared = match prepare_local_inner(
        admission,
        graph,
        receipts,
        engine,
        source,
        LocalPreparationBinding::SlowPipeline,
        transaction,
        None,
        None,
    )? {
        PreparedLocalMutationState::Prepared(prepared) => prepared,
        PreparedLocalMutationState::ReconciliationRequired(reconciliation) => {
            return Ok(LocalMutationCoordinatorState::Recovering(
                LocalMutationRecovery::ReconciliationRequired(reconciliation),
            ));
        }
    };
    prepared.preflight_identity(database, engine)?;
    publish_prepared_local(admission, graph, receipts, engine, database, tail, prepared)
}

fn resume_clean_published(
    admission: &LocalRuntimeAdmission<'_>,
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    engine: &mut ShardedHotEngine,
    database: &mut SqliteFrontier,
    continuation: &CleanPublishedContinuation,
) -> Result<(), OperationalCoordinatorError> {
    authorize_coordinator(admission, graph, engine)?;
    let event = AcceptedBatchEvent::from_accepted(
        engine,
        engine.archive_store().ok_or_else(|| {
            OperationalCoordinatorError::retained_block(
                OperationalPhase::ArchiveStage,
                "clean committed operation has no retained archive",
                RetainedBlockReason::PublishedAuthentication,
            )
        })?,
        continuation.batch_id,
    )
    .map_err(|error| {
        OperationalCoordinatorError::retained_block(
            OperationalPhase::ArchiveStage,
            error.to_string(),
            RetainedBlockReason::PublishedAuthentication,
        )
    })?
    .with_prepared_identity_transition(continuation.identity.clone())
    .map_err(|error| {
        OperationalCoordinatorError::retained_block(
            OperationalPhase::SqliteDrain,
            error.to_string(),
            RetainedBlockReason::PublishedAuthentication,
        )
    })?;
    let applied = database.frontier_root().map_err(|error| {
        OperationalCoordinatorError::new(OperationalPhase::SqliteDrain, error.to_string())
    })?;
    if applied.same_accepted_authority(event.prior_frontier_root()) {
        reprove_workspace_authority(
            admission,
            WorkspaceAuthorityBoundary::SqliteDrain,
            OperationalPhase::SqliteDrain,
        )?;
        database
            .apply_engine_owned_accepted(&event, engine)
            .map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::SqliteDrain, error.to_string())
            })?;
        fault(OperationalFaultPoint::AfterSqliteApply)?;
    } else if !applied.same_accepted_authority(event.post_frontier_root()) {
        return Err(OperationalCoordinatorError::new(
            OperationalPhase::SqliteDrain,
            "clean SQLite frontier is neither before nor after the durable operation",
        ));
    }

    for work in engine
        .clean_projection_work_for_batch(continuation.batch_id)
        .map_err(|error| {
            OperationalCoordinatorError::retained_block(
                OperationalPhase::ProjectionDrain,
                error.to_string(),
                RetainedBlockReason::PublishedAuthentication,
            )
        })?
    {
        reprove_workspace_authority(
            admission,
            WorkspaceAuthorityBoundary::ProjectionDrain,
            OperationalPhase::ProjectionDrain,
        )?;
        fault(OperationalFaultPoint::BeforeProjection)?;
        super::projection::execute_clean_manifested_projection_work_under_handoff(
            graph,
            receipts,
            database,
            engine,
            &work,
            &continuation.guard,
        )
        .map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::ProjectionDrain, error.to_string())
        })?;
        fault(OperationalFaultPoint::AfterProjection)?;
    }

    // Projection intents authored by another endpoint describe accepted
    // semantic state, not bytes that this receiver may copy verbatim.  Once
    // SQLite has advanced to the accepted frontier, render each foreign
    // intent again against this endpoint's exact local base and record the
    // result in its private receipt store.  Keeping this in the resumable
    // published continuation makes provider ingress crash-safe: a retry may
    // observe the manifest and SQLite transition already complete, but it
    // must still finish the receiver-local Markdown projection.
    let receiver_endpoint = engine
        .projection_endpoint_binding()
        .ok_or_else(|| {
            OperationalCoordinatorError::new(
                OperationalPhase::ProjectionDrain,
                "clean provider receiver has no enrolled projection endpoint",
            )
        })?
        .endpoint_id();
    let batch = match engine
        .archive_store()
        .ok_or_else(|| {
            OperationalCoordinatorError::retained_block(
                OperationalPhase::ArchiveStage,
                "clean committed operation has no retained archive",
                RetainedBlockReason::PublishedAuthentication,
            )
        })?
        .inspect_batch(continuation.batch_id)
        .map_err(|error| {
            OperationalCoordinatorError::retained_block(
                OperationalPhase::ProjectionDrain,
                error.to_string(),
                RetainedBlockReason::PublishedAuthentication,
            )
        })? {
        BatchInspection::Ready(batch) => batch,
        BatchInspection::Absent | BatchInspection::Staged { .. } => {
            return Err(OperationalCoordinatorError::retained_block(
                OperationalPhase::ProjectionDrain,
                "clean accepted batch became partial before receiver-local projection",
                RetainedBlockReason::PublishedAuthentication,
            ));
        }
    };
    let projection = super::projection_manifest::validate_projection_object_set(
        batch.manifest(),
        batch.objects(),
    )
    .map_err(|error| {
        OperationalCoordinatorError::retained_block(
            OperationalPhase::ProjectionDrain,
            error.to_string(),
            RetainedBlockReason::PublishedAuthentication,
        )
    })?;
    for source in projection
        .intents()
        .iter()
        .filter(|source| source.source_endpoint_id() != receiver_endpoint)
    {
        reprove_workspace_authority(
            admission,
            WorkspaceAuthorityBoundary::ProjectionDrain,
            OperationalPhase::ProjectionDrain,
        )?;
        let completed = super::projection::execute_receiver_local_projection_under_handoff(
            graph,
            receipts,
            engine,
            Some(database),
            source,
            &continuation.guard,
            true,
        )
        .map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::ProjectionDrain, error.to_string())
        })?;
        if completed.is_none() {
            // Name the artifact and the operation. This detail is what
            // `clean_shutdown` reports when it refuses `Safe`, so an
            // unapplied delivered deletion is never silent.
            let operation = if matches!(source.target(), super::ManifestProjectionTarget::Absent) {
                "deletion"
            } else {
                "projection"
            };
            return Err(OperationalCoordinatorError::continuation_required(
                OperationalPhase::ProjectionDrain,
                format!(
                    "clean receiver-local {operation} of {:?} requires a continuation",
                    source.path().as_str()
                ),
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn publish_prepared_local(
    admission: &LocalRuntimeAdmission<'_>,
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    engine: &mut ShardedHotEngine,
    database: &mut SqliteFrontier,
    tail: &mut TailOverlay,
    prepared: PreparedLocalMutation,
) -> Result<LocalMutationCoordinatorState, OperationalCoordinatorError> {
    let PreparedLocalMutation {
        endpoint,
        archive,
        guard,
        prepared,
        batch_id,
        identity,
    } = prepared;
    let archive = archive.expect("slow local preparation retains its verified archive");
    let identity = identity.expect("slow local publication requires SQLite identity preflight");

    match publish_and_drain(
        admission,
        graph,
        receipts,
        engine,
        database,
        tail,
        endpoint,
        archive,
        guard,
        prepared,
        batch_id,
        BatchOrigin::LocalMutation,
        identity,
    )? {
        PublishedPipelineState::Complete(batch_id) => Ok(LocalMutationCoordinatorState::Active(
            LocalMutationCompletion { batch_id },
        )),
        PublishedPipelineState::FailedClosed(continuation) => {
            Ok(LocalMutationCoordinatorState::from_failed(
                LocalPublishedContinuation { core: continuation },
            ))
        }
    }
}

enum PublishedPipelineState {
    Complete(BatchId),
    FailedClosed(PublishedContinuationCore),
}

/// The sole terminal commit pipeline for both local semantic mutations and
/// external reconciliation. Callers may differ before finalization; every
/// durable or derived-state side effect converges here.
#[allow(clippy::too_many_arguments)]
fn publish_and_drain(
    admission: &LocalRuntimeAdmission<'_>,
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    engine: &mut ShardedHotEngine,
    database: &mut SqliteFrontier,
    tail: &mut TailOverlay,
    endpoint: ProjectionEndpointBinding,
    archive: Arc<ObjectStore>,
    guard: HandoffSafeGuard,
    prepared: PreparedBatch,
    batch_id: BatchId,
    origin: BatchOrigin,
    identity: super::sqlite::PreparedSqliteIdentityTransition,
) -> Result<PublishedPipelineState, OperationalCoordinatorError> {
    if prepared.manifest().batch_id() != batch_id || prepared.manifest().origin() != origin {
        return Err(OperationalCoordinatorError::new(
            OperationalPhase::Finalize,
            "prepared batch does not match the sealed terminal-pipeline identity",
        ));
    }
    #[cfg(test)]
    TERMINAL_PIPELINE_ORIGINS.with(|origins| origins.borrow_mut().push(origin));
    let retained_bytes = prepared.retained_bytes().map_err(|error| {
        OperationalCoordinatorError::new(OperationalPhase::TailReservation, error.to_string())
    })?;
    let reservation = tail
        .reserve_bound_mutation(database, engine, retained_bytes)
        .map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::TailReservation, error.to_string())
        })?;
    if let Err(failure) = fault(OperationalFaultPoint::AfterReservation) {
        tail.cancel_reservation(reservation).map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::TailReservation, error.to_string())
        })?;
        return Err(failure);
    }
    let manifest_bytes = match prepared.manifest().encode() {
        Ok(bytes) => bytes,
        Err(error) => {
            tail.cancel_reservation(reservation).map_err(|cancel| {
                OperationalCoordinatorError::new(
                    OperationalPhase::TailReservation,
                    format!("{error}; reservation cancellation failed: {cancel}"),
                )
            })?;
            return Err(OperationalCoordinatorError::new(
                OperationalPhase::Publication,
                error.to_string(),
            ));
        }
    };
    let manifest_digest = ContentDigest::of(&manifest_bytes);

    // Publication is the first irreversible step. The reservation is still
    // cancellable and the publisher guard has not yet been consumed.
    if let Err(failure) = reprove_workspace_authority(
        admission,
        WorkspaceAuthorityBoundary::Publication,
        OperationalPhase::Publication,
    ) {
        drop(guard);
        tail.cancel_reservation(reservation).map_err(|cancel| {
            OperationalCoordinatorError::new(
                OperationalPhase::TailReservation,
                format!("{failure}; reservation cancellation failed: {cancel}"),
            )
        })?;
        return Err(failure);
    }
    let published_latch = guard.into_published_latch();

    if let Err(error) = archive.publish_prepared(&prepared) {
        let publication = archive.inspect_batch(batch_id);
        if matches!(publication, Ok(BatchInspection::Absent)) {
            published_latch.cancel_prepublication();
            tail.cancel_reservation(reservation).map_err(|cancel| {
                OperationalCoordinatorError::new(
                    OperationalPhase::TailReservation,
                    format!("{error}; reservation cancellation failed: {cancel}"),
                )
            })?;
            return Err(OperationalCoordinatorError::new(
                OperationalPhase::Publication,
                error.to_string(),
            ));
        }
        return Ok(PublishedPipelineState::FailedClosed(
            PublishedContinuationCore {
                guard: published_latch,
                endpoint,
                archive,
                batch_id,
                origin,
                manifest_digest,
                retained_bytes,
                reservation: Some(reservation),
                identity: Some(identity),
                provider_ingress: false,
                failure: OperationalCoordinatorError::new(
                    OperationalPhase::Publication,
                    error.to_string(),
                ),
            },
        ));
    }
    let boundary = fault(OperationalFaultPoint::AfterManifest);
    let mut coordinator = PublishedContinuationCore {
        guard: published_latch,
        endpoint,
        archive,
        batch_id,
        origin,
        manifest_digest,
        retained_bytes,
        reservation: Some(reservation),
        identity: Some(identity),
        provider_ingress: false,
        failure: boundary.clone().err().unwrap_or_else(|| {
            OperationalCoordinatorError::new(
                OperationalPhase::ArchiveStage,
                "published mutation is awaiting derived-state drains",
            )
        }),
    };
    if let Err(failure) = boundary {
        coordinator.failure = failure;
        return Ok(PublishedPipelineState::FailedClosed(coordinator));
    }
    match coordinator.resume(admission, graph, receipts, engine, database, tail) {
        Ok(batch_id) => {
            coordinator.guard.complete();
            Ok(PublishedPipelineState::Complete(batch_id))
        }
        Err(failure) => {
            coordinator.failure = failure;
            Ok(PublishedPipelineState::FailedClosed(coordinator))
        }
    }
}

fn draft_with_bounded_peer_candidates(
    engine: &ShardedHotEngine,
    endpoint: ProjectionEndpointBinding,
    material: &super::import::ImportExecutionMaterial,
    claim_source: Option<&dyn super::hot_engine::ProjectionClaimSource>,
    mut candidate_at: impl FnMut(u64) -> CrdtPeerId,
) -> Result<(AuthorBatch, super::AuthorTransactionDraft), OperationalCoordinatorError> {
    for attempt in 0..CRDT_PEER_PROBE_BUDGET {
        let crdt_peer_id = candidate_at(attempt);
        if crdt_peer_id.as_u64() == 0 {
            continue;
        }
        let author = AuthorBatch {
            batch_id: material.batch_id(),
            author_device_id: endpoint.device_id(),
            author_session_id: SessionId::for_external_import_author(
                engine.workspace_id(),
                material.import_id(),
            ),
            crdt_peer_id,
        };
        let drafted = match claim_source {
            Some(source) => {
                engine.draft_clean_external_import_transaction(author, material.clone(), source)
            }
            None => engine.draft_external_import_transaction(author, material.clone()),
        };
        match drafted {
            Ok(draft) => return Ok((author, draft)),
            Err(super::EngineError::CrdtPeerCollision(collision)) if collision == crdt_peer_id => {}
            Err(error) => {
                return Err(OperationalCoordinatorError::new(
                    OperationalPhase::Draft,
                    error.to_string(),
                ));
            }
        }
    }
    Err(OperationalCoordinatorError::new(
        OperationalPhase::Draft,
        format!(
            "no collision-free nonzero CRDT peer in the bounded {CRDT_PEER_PROBE_BUDGET}-candidate probe"
        ),
    ))
}

fn verify_bindings(
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    engine: &ShardedHotEngine,
    endpoint: ProjectionEndpointBinding,
    expected_archive: Option<&Arc<ObjectStore>>,
) -> Result<Arc<ObjectStore>, OperationalCoordinatorError> {
    verify_projection_bindings(graph, receipts, engine, endpoint)?;
    let (archive, index) = engine.enrolled_projection_runtime().map_err(|error| {
        OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
    })?;
    if index.endpoint_id() != endpoint.endpoint_id()
        || index.graph_resource_id() != endpoint.graph_resource_id()
        || index.receipt_store_id() != receipts.store_id()
    {
        return Err(OperationalCoordinatorError::retained_block(
            OperationalPhase::Bindings,
            "enrolled archive/projection runtime binding changed",
            RetainedBlockReason::StableBinding,
        ));
    }
    // The retained continuation authenticates the archive by stable workspace
    // and no-follow resource identity rather than by `Arc` pointer identity, so
    // a same-process engine reconstruction over the exact same enrolled archive
    // can resume. A substituted or copied archive directory still fails.
    if let Some(expected) = expected_archive {
        let identity = |store: &ObjectStore| {
            store.canonical_archive_identity().map_err(|error| {
                OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
            })
        };
        if expected.workspace_id() != archive.workspace_id()
            || identity(expected)? != identity(&archive)?
        {
            return Err(OperationalCoordinatorError::retained_block(
                OperationalPhase::Bindings,
                "enrolled archive resource identity changed",
                RetainedBlockReason::StableBinding,
            ));
        }
    }
    Ok(archive)
}

fn verify_projection_bindings(
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    engine: &ShardedHotEngine,
    endpoint: ProjectionEndpointBinding,
) -> Result<(), OperationalCoordinatorError> {
    let graph_resource = graph.canonical_resource_id().map_err(|error| {
        OperationalCoordinatorError::new(OperationalPhase::Bindings, error.to_string())
    })?;
    if endpoint.graph_resource_id() != graph_resource
        || receipts.workspace_id() != engine.workspace_id()
        || receipts.endpoint_binding() != Some(endpoint)
        || engine.projection_receipt_store_id() != Some(receipts.store_id())
    {
        return Err(OperationalCoordinatorError::retained_block(
            OperationalPhase::Bindings,
            "graph, engine endpoint, or receipt namespace binding mismatch",
            RetainedBlockReason::StableBinding,
        ));
    }
    Ok(())
}

fn authenticate_published(
    archive: &ObjectStore,
    batch_id: BatchId,
    origin: BatchOrigin,
    manifest_digest: ContentDigest,
    retained_bytes: usize,
) -> Result<(), OperationalCoordinatorError> {
    // This runs once per coordinator tick and asks only manifest questions:
    // the batch id, the origin, the manifest digest, and the total retained
    // byte count. Every one of those is answerable from the manifest and its
    // descriptors, so reading the batch -- which reads, SHA-256s and decodes
    // every object -- was the single largest source of the O(n^2) import:
    // measured at 91 of 107 `inspect_batch` calls and 27,458 of 30,259 object
    // reads on a 100-file import.
    let manifest = archive
        .read_manifest(batch_id)
        .map_err(|error| match error {
            super::StoreError::Io(error) => {
                OperationalCoordinatorError::new(OperationalPhase::Publication, error.to_string())
            }
            stable => OperationalCoordinatorError::retained_block(
                OperationalPhase::Publication,
                stable.to_string(),
                RetainedBlockReason::PublishedAuthentication,
            ),
        })?
        .ok_or_else(|| {
            OperationalCoordinatorError::retained_block(
                OperationalPhase::Publication,
                "published mutation is not a complete immutable batch",
                RetainedBlockReason::PublishedAuthentication,
            )
        })?;
    let encoded = manifest.encode().map_err(|error| {
        OperationalCoordinatorError::retained_block(
            OperationalPhase::Publication,
            error.to_string(),
            RetainedBlockReason::PublishedAuthentication,
        )
    })?;
    if manifest.batch_id() != batch_id
        || manifest.origin() != origin
        || ContentDigest::of(&encoded) != manifest_digest
    {
        return Err(OperationalCoordinatorError::retained_block(
            OperationalPhase::Publication,
            "durable manifest differs from the failed-closed publication identity",
            RetainedBlockReason::PublishedAuthentication,
        ));
    }
    // Identical arithmetic to the old per-object `encode().len()` fold: an
    // object file is written, and read back, at exactly its descriptor's
    // `encoded_byte_length`.
    let actual =
        manifest
            .required_objects()
            .iter()
            .try_fold(encoded.len(), |total, descriptor| {
                usize::try_from(descriptor.encoded_byte_length())
                    .ok()
                    .and_then(|length| total.checked_add(length))
                    .ok_or_else(|| {
                        OperationalCoordinatorError::retained_block(
                            OperationalPhase::Publication,
                            "durable retained-byte count overflowed",
                            RetainedBlockReason::PublishedAuthentication,
                        )
                    })
            })?;
    if actual != retained_bytes {
        return Err(OperationalCoordinatorError::retained_block(
            OperationalPhase::Publication,
            "durable batch bytes differ from the reserved prepared-byte count",
            RetainedBlockReason::PublishedAuthentication,
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationalFaultPoint {
    AfterHandoff,
    AfterPlan,
    AfterDraft,
    AfterCapture,
    AfterFinalize,
    AfterReservation,
    AfterManifest,
    AfterStage,
    BeforeTailAdmission,
    AfterTailAdmission,
    AfterSqliteApply,
    BeforeProjection,
    AfterProjection,
}

thread_local! {
    static OPERATIONAL_FAULT: std::cell::Cell<Option<OperationalFaultPoint>> =
        const { std::cell::Cell::new(None) };
    #[cfg(test)]
    static OPERATIONAL_REPEATED_FAULT:
        std::cell::Cell<Option<(OperationalFaultPoint, u8)>> =
        const { std::cell::Cell::new(None) };
    static OPERATIONAL_ACTION: std::cell::RefCell<
        Option<(OperationalFaultPoint, Box<dyn FnOnce()>)>,
    > = std::cell::RefCell::new(None);
    #[cfg(test)]
    static TERMINAL_PIPELINE_ORIGINS: std::cell::RefCell<Vec<BatchOrigin>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn reset_terminal_pipeline_origins() {
    TERMINAL_PIPELINE_ORIGINS.with(|origins| origins.borrow_mut().clear());
}

#[cfg(test)]
fn terminal_pipeline_origins() -> Vec<BatchOrigin> {
    TERMINAL_PIPELINE_ORIGINS.with(|origins| origins.borrow().clone())
}

pub(crate) fn fail_once_at(point: OperationalFaultPoint) {
    OPERATIONAL_FAULT.set(Some(point));
}

#[cfg(test)]
pub(crate) fn fail_repeatedly_at(point: OperationalFaultPoint, failures: u8) {
    assert!(failures > 0, "a repeated operational fault needs work");
    OPERATIONAL_REPEATED_FAULT.set(Some((point, failures)));
}

pub(crate) fn act_once_at(point: OperationalFaultPoint, action: impl FnOnce() + 'static) {
    OPERATIONAL_ACTION.with(|slot| {
        *slot.borrow_mut() = Some((point, Box::new(action)));
    });
}

fn fault(point: OperationalFaultPoint) -> Result<(), OperationalCoordinatorError> {
    OPERATIONAL_ACTION.with(|slot| {
        let matches = slot
            .borrow()
            .as_ref()
            .is_some_and(|(scheduled, _)| *scheduled == point);
        if matches {
            let (_, action) = slot.borrow_mut().take().expect("checked action exists");
            action();
        }
    });
    #[cfg(test)]
    if let Some((scheduled, failures)) = OPERATIONAL_REPEATED_FAULT.get() {
        if scheduled == point {
            OPERATIONAL_REPEATED_FAULT.set(
                failures
                    .checked_sub(1)
                    .filter(|remaining| *remaining > 0)
                    .map(|remaining| (scheduled, remaining)),
            );
            return Err(operational_fault_error(point));
        }
    }
    if OPERATIONAL_FAULT.get() == Some(point) {
        OPERATIONAL_FAULT.set(None);
        return Err(operational_fault_error(point));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn fail_next_clean_after_manifest_for_harness() {
    fail_once_at(OperationalFaultPoint::AfterManifest);
}

fn operational_fault_error(point: OperationalFaultPoint) -> OperationalCoordinatorError {
    OperationalCoordinatorError::new(
        match point {
            OperationalFaultPoint::AfterHandoff => OperationalPhase::Bindings,
            OperationalFaultPoint::AfterPlan => OperationalPhase::Planning,
            OperationalFaultPoint::AfterDraft => OperationalPhase::Draft,
            OperationalFaultPoint::AfterCapture => OperationalPhase::Capture,
            OperationalFaultPoint::AfterFinalize => OperationalPhase::Finalize,
            OperationalFaultPoint::AfterReservation => OperationalPhase::TailReservation,
            OperationalFaultPoint::AfterManifest => OperationalPhase::Publication,
            OperationalFaultPoint::AfterStage => OperationalPhase::ArchiveStage,
            OperationalFaultPoint::BeforeTailAdmission => OperationalPhase::TailAdmission,
            OperationalFaultPoint::AfterTailAdmission => OperationalPhase::TailAdmission,
            OperationalFaultPoint::AfterSqliteApply => OperationalPhase::SqliteDrain,
            OperationalFaultPoint::BeforeProjection | OperationalFaultPoint::AfterProjection => {
                OperationalPhase::ProjectionDrain
            }
        },
        "deterministic operational fault",
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use uuid::Uuid;

    use super::*;
    use crate::model::{projection_graph_test_counters, reset_projection_graph_test_counters};
    use crate::oplog::import::{
        commit_clean_activation, open_clean_activation, prepare_clean_activation,
    };
    use crate::oplog::local_active::CleanLocalRuntime;
    use crate::oplog::object_store::{
        fail_next_engine_history_head_swap, fail_next_publish_after_objects,
    };
    use crate::oplog::projection::fail_next_formatting_adoption_after_intent_for_harness;
    use crate::oplog::sqlite::{LeasedWorkspaceProjection, WorkspaceRuntimeLease};
    use crate::oplog::{
        recover_incomplete_projections, write_projection_exact, AnnotatedProjectionBase,
        ApplicationRuntimeRoot, BlockId, BlockLocation, DeviceId, DocumentId, LineageDigest,
        LogicalPageName, ManagedPath, ManagedTextKind, ManifestProjectionPrecondition,
        ManifestedProjectionIntent, ObjectKind, OperationTransaction, PageId, ProjectionClaim,
        ProjectionEndpointId, SemanticOperation, TAIL_MAX_BYTES,
    };

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "tine-operational-coordinator-{label}-{}",
                Uuid::new_v4()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The baseline-plus-manifest composition used by the clean coordinator
    /// corpus. Source files are the genesis and operation manifests are the
    /// only durable semantic history; no enrolled history, projection-work
    /// index, scratch store, or persistent tail participates.
    struct CleanCoordinatorFixture {
        _root: TestRoot,
        graph_root: PathBuf,
        archive_root: PathBuf,
        enrollment_root: PathBuf,
        database_path: PathBuf,
        graph: Graph,
        receipts: ProjectionReceiptStore,
        archive: ObjectStore,
        runtime: CleanLocalRuntime,
        lineage: LineageDigest,
        catalog: DocumentId,
        page_id: PageId,
        home_document_id: DocumentId,
        block_id: BlockId,
        path: String,
    }

    impl CleanCoordinatorFixture {
        fn new(label: &str) -> Self {
            Self::new_at(
                label,
                "pages/deep/projects/a.md",
                None,
                ManagedTextKind::Page,
            )
        }

        fn configured(label: &str) -> Self {
            Self::new_at(
                label,
                "content/pages/deep/projects/a.md",
                Some(
                    "{:pages-directory \"content/pages\"\n\
                      :journals-directory \"content/journals\"}\n",
                ),
                ManagedTextKind::Page,
            )
        }

        fn formatting_only(label: &str) -> Self {
            Self::new_at(
                label,
                "pages/Coordinator Page.md",
                None,
                ManagedTextKind::Page,
            )
        }

        fn new_at(label: &str, path: &str, config: Option<&str>, _kind: ManagedTextKind) -> Self {
            let root = TestRoot::new(&format!("{label}-clean"));
            let graph_root = root.path().join("graph");
            fs::create_dir_all(&graph_root).unwrap();
            if let Some(config) = config {
                fs::create_dir_all(graph_root.join("logseq")).unwrap();
                fs::write(graph_root.join("logseq/config.edn"), config).unwrap();
            }
            let target = graph_root.join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(&target, clean_coordinator_source(path, "root", "child")).unwrap();
            let graph = Graph::open(&graph_root);

            let workspace = super::super::WorkspaceId::from_uuid(Uuid::from_u128(1));
            let lineage = LineageDigest::of(label.as_bytes());
            let catalog = DocumentId::from_uuid(Uuid::from_u128(4));
            let database_path = root.path().join("clean-projection.sqlite");
            let archive_parent = root.path().join("clean-archive");
            let enrollment_root = root.path().join("clean-enrollment");
            fs::create_dir(&archive_parent).unwrap();
            let capture_root = root.path().join("clean-capture");
            fs::create_dir(&capture_root).unwrap();
            let capture = graph
                .capture_inactive_bootstrap_sources(&capture_root)
                .unwrap();
            let preparation = prepare_clean_activation(
                &graph,
                capture,
                workspace,
                lineage,
                catalog,
                &root.path().join("clean-preparation"),
                &database_path,
                &super::super::ReferenceCatalogPolicyV1::default(),
            )
            .unwrap();
            let page_id = preparation
                .candidates()
                .baseline()
                .page_ids()
                .find(|page_id| {
                    preparation
                        .candidates()
                        .baseline()
                        .page(*page_id)
                        .unwrap()
                        .is_some_and(|page| page.path.as_str() == path)
                })
                .unwrap_or_else(|| panic!("clean baseline has no {path}"));
            let committed = commit_clean_activation(
                &graph,
                preparation,
                &archive_parent.join(crate::oplog::lazy_genesis::LAZY_GENESIS_BASELINE_DIRECTORY),
                &enrollment_root,
            )
            .unwrap();
            let (baseline, physical, baseline_frontier, _) = committed.into_parts();
            drop(physical);
            drop(baseline);
            let reopened = open_clean_activation(
                &enrollment_root,
                &archive_parent.join(crate::oplog::lazy_genesis::LAZY_GENESIS_BASELINE_DIRECTORY),
                &database_path,
                catalog,
                super::super::ReferenceCatalogPolicyV1::default(),
            )
            .unwrap()
            .expect("published clean coordinator activation reopens");
            let (mut engine, projection, _) = reopened.into_parts();
            let archive_root = archive_parent.join("operations");
            engine
                .attach_clean_archive_store(ObjectStore::open(&archive_root, workspace).unwrap())
                .unwrap();
            let archive = ObjectStore::open(&archive_root, workspace).unwrap();
            let lease = WorkspaceRuntimeLease::acquire(&archive, workspace).unwrap();
            let projection = LeasedWorkspaceProjection::adopt_clean_genesis(
                lease,
                &database_path,
                ProjectionClaim::current(workspace, lineage),
                &baseline_frontier,
                &archive,
                &engine,
                projection,
            )
            .map_err(|(_, error)| error)
            .unwrap();
            let endpoint = ProjectionEndpointBinding::enroll_graph(
                &graph,
                ProjectionEndpointId::from_uuid(Uuid::from_u128(2)),
                DeviceId::from_uuid(Uuid::from_u128(3)),
            )
            .unwrap();
            let receipts = ProjectionReceiptStore::open_for_endpoint(
                &root.path().join("clean-receipts"),
                workspace,
                endpoint,
            )
            .unwrap();
            engine
                .attach_clean_projection_endpoint(&graph, &receipts)
                .unwrap();
            let runtime = CleanLocalRuntime::from_open_parts(
                SessionId::from_uuid(Uuid::from_u128(7)),
                endpoint,
                engine,
                projection,
            )
            .unwrap();
            let page = runtime.engine().materialize_page(page_id).unwrap();
            let home_document_id = page.home_document_id;
            let block_id = page.blocks[0].block_id;
            Self {
                _root: root,
                graph_root,
                archive_root,
                enrollment_root,
                database_path,
                graph,
                receipts,
                archive,
                runtime,
                lineage,
                catalog,
                page_id,
                home_document_id,
                block_id,
                path: path.into(),
            }
        }

        fn overwrite(&self, bytes: &[u8]) {
            fs::write(self.graph_root.join(&self.path), bytes).unwrap();
        }

        fn engine(&self) -> &ShardedHotEngine {
            self.runtime.engine()
        }

        fn database(&self) -> &SqliteFrontier {
            self.runtime.database()
        }

        fn local_edit_transaction(&self, content: &str) -> OperationTransaction {
            OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: self.block_id,
                    home_document_id: self.home_document_id,
                },
                content: content.into(),
            }])
            .unwrap()
        }

        fn local_author(&self, seed: u128) -> AuthorBatch {
            AuthorBatch {
                batch_id: BatchId::from_uuid(Uuid::from_u128(seed)),
                author_device_id: self.runtime.endpoint().device_id(),
                author_session_id: SessionId::from_uuid(Uuid::from_u128(seed + 1)),
                crdt_peer_id: CrdtPeerId::from_u64((seed as u64).saturating_add(10_001)),
            }
        }

        fn execute_local(
            &mut self,
            transaction: &OperationTransaction,
        ) -> Result<CleanLocalMutationState, OperationalCoordinatorError> {
            let mut session = self.runtime.admit_clean_mutation(&self.graph).unwrap();
            OperationalCoordinator::execute_clean_local(
                &mut session,
                &self.graph,
                &self.receipts,
                transaction,
            )
        }

        fn execute_local_correlated(
            &mut self,
            batch_id: BatchId,
            transaction: &OperationTransaction,
        ) -> Result<CleanLocalMutationState, OperationalCoordinatorError> {
            let mut session = self.runtime.admit_clean_mutation(&self.graph).unwrap();
            OperationalCoordinator::execute_clean_local_correlated(
                &mut session,
                &self.graph,
                &self.receipts,
                batch_id,
                transaction,
                |_| Ok(()),
            )
        }

        fn local_edit(
            &mut self,
            content: &str,
        ) -> Result<CleanLocalMutationState, OperationalCoordinatorError> {
            let transaction = self.local_edit_transaction(content);
            self.execute_local(&transaction)
        }

        fn execute_external(
            &mut self,
            paths: &[&str],
        ) -> Result<CleanExternalMutationState, OperationalCoordinatorError> {
            let mut session = self.runtime.admit_clean_mutation(&self.graph).unwrap();
            OperationalCoordinator::execute_clean_external(
                &mut session,
                &self.graph,
                &self.receipts,
                paths,
            )
        }

        fn retry_clean(
            &mut self,
            continuation: CleanPublishedContinuation,
        ) -> CleanLocalMutationState {
            let mut session = self
                .runtime
                .admit_clean_derived_recovery(&self.graph)
                .unwrap();
            OperationalCoordinator::retry_clean_local(
                &mut session,
                &self.graph,
                &self.receipts,
                continuation,
            )
        }

        fn assert_clean_drained(&self) {
            assert_eq!(
                self.database().frontier_root().unwrap(),
                self.engine().accepted_frontier_root().unwrap()
            );
            let source = RebuildSource::new(self.engine(), &self.archive).unwrap();
            let tail = TailOverlay::from_durable(self.database(), &source).unwrap();
            assert_eq!(tail.status().unapplied_batches, 0);
            assert_eq!(tail.status().retained_bytes, 0);
        }

        fn restart_clean_runtime(self) -> Self {
            let Self {
                _root,
                graph_root,
                archive_root,
                enrollment_root,
                database_path,
                graph,
                receipts,
                archive,
                runtime,
                lineage,
                catalog,
                page_id,
                home_document_id: _,
                block_id: _,
                path,
            } = self;
            let endpoint = receipts.endpoint_binding().unwrap();
            let workspace = runtime.engine().workspace_id();
            drop(runtime);
            drop(archive);
            let archive_parent = archive_root.parent().unwrap();
            let reopened = open_clean_activation(
                &enrollment_root,
                &archive_parent.join(crate::oplog::lazy_genesis::LAZY_GENESIS_BASELINE_DIRECTORY),
                &database_path,
                catalog,
                super::super::ReferenceCatalogPolicyV1::default(),
            )
            .unwrap()
            .expect("published clean coordinator activation reopens after restart");
            let (mut engine, baseline_projection, _) = reopened.into_parts();
            engine
                .attach_clean_archive_store(ObjectStore::open(&archive_root, workspace).unwrap())
                .unwrap();
            let baseline_root = engine.accepted_frontier_root().unwrap();
            let baseline_claim_source = super::super::sqlite::clean_genesis_materialized_read(
                &baseline_projection,
                &baseline_root,
            )
            .unwrap();
            let replayed = engine
                .replay_clean_committed_tail(&baseline_claim_source)
                .unwrap();
            drop(baseline_claim_source);
            let archive = ObjectStore::open(&archive_root, workspace).unwrap();
            let lease = WorkspaceRuntimeLease::acquire(&archive, workspace).unwrap();
            let projection = if replayed == 0 {
                let expected = engine.accepted_frontier_root().unwrap();
                LeasedWorkspaceProjection::adopt_clean_genesis(
                    lease,
                    &database_path,
                    ProjectionClaim::current(workspace, lineage),
                    &expected,
                    &archive,
                    &engine,
                    baseline_projection,
                )
                .map_err(|(_, error)| error)
                .unwrap()
            } else {
                drop(baseline_projection);
                let application_runtime = ApplicationRuntimeRoot::open_for_test(
                    &_root.path().join("clean-application-runtime"),
                )
                .unwrap();
                let source = RebuildSource::new(&engine, &archive).unwrap();
                LeasedWorkspaceProjection::open_under(lease, |slot| {
                    let opened = SqliteFrontier::open_or_rebuild_with_applier_slot(
                        &database_path,
                        &application_runtime,
                        ProjectionClaim::current(workspace, lineage),
                        source,
                        slot,
                    )?;
                    Ok::<_, super::super::SqliteProjectionError>((opened, ()))
                })
                .map(|(projection, ())| projection)
                .map_err(|(_, error)| error)
                .unwrap()
            };
            engine
                .attach_clean_projection_endpoint(&graph, &receipts)
                .unwrap();
            let runtime = CleanLocalRuntime::from_open_parts(
                SessionId::from_uuid(Uuid::from_u128(7)),
                endpoint,
                engine,
                projection,
            )
            .unwrap();
            let page = runtime.engine().materialize_page(page_id).unwrap();
            let home_document_id = page.home_document_id;
            let block_id = page.blocks[0].block_id;
            Self {
                _root,
                graph_root,
                archive_root,
                enrollment_root,
                database_path,
                graph,
                receipts,
                archive,
                runtime,
                lineage,
                catalog,
                page_id,
                home_document_id,
                block_id,
                path,
            }
        }
    }

    fn clean_coordinator_source(path: &str, root: &str, child: &str) -> Vec<u8> {
        if path.ends_with(".org") {
            format!("* {root}\n** {child}\n").into_bytes()
        } else {
            format!("- {root}\n  - {child}\n").into_bytes()
        }
    }

    fn expect_clean_local_complete(state: CleanLocalMutationState) -> BatchId {
        match state {
            CleanLocalMutationState::Complete(batch_id) => batch_id,
            CleanLocalMutationState::DurablePending(pending) => {
                panic!(
                    "unexpected durable clean local continuation: {}",
                    pending.failure()
                )
            }
        }
    }

    fn expect_clean_local_pending(state: CleanLocalMutationState) -> CleanPublishedContinuation {
        match state {
            CleanLocalMutationState::DurablePending(pending) => pending,
            CleanLocalMutationState::Complete(_) => {
                panic!("unexpected completed clean local mutation")
            }
        }
    }

    fn expect_clean_external_complete(state: CleanExternalMutationState) -> BatchId {
        match state {
            CleanExternalMutationState::Complete(batch_id) => batch_id,
            CleanExternalMutationState::Noop => panic!("unexpected clean external no-op"),
            CleanExternalMutationState::DurablePending(pending) => panic!(
                "unexpected durable clean external continuation: {}",
                pending.failure()
            ),
        }
    }

    fn expect_clean_external_pending(
        state: CleanExternalMutationState,
    ) -> CleanPublishedContinuation {
        match state {
            CleanExternalMutationState::DurablePending(pending) => pending,
            CleanExternalMutationState::Noop => panic!("unexpected clean external no-op"),
            CleanExternalMutationState::Complete(_) => {
                panic!("unexpected completed clean external mutation")
            }
        }
    }

    fn settle_clean_local(
        fixture: &mut CleanCoordinatorFixture,
        mut state: CleanLocalMutationState,
    ) -> BatchId {
        for _ in 0..8 {
            match state {
                CleanLocalMutationState::Complete(batch_id) => return batch_id,
                CleanLocalMutationState::DurablePending(pending) => {
                    state = fixture.retry_clean(pending);
                }
            }
        }
        panic!("clean local mutation did not settle within the bounded turn budget")
    }

    #[test]
    fn fresh_nested_layout_reconcile_drains_history_sqlite_and_projection_clean() {
        let mut fixture = CleanCoordinatorFixture::configured("nested-success");
        let path = fixture.path.clone();
        fixture.overwrite(b"- root edited\n  - child edited\n");
        let batch_id = expect_clean_external_complete(fixture.execute_external(&[&path]).unwrap());
        let batch = match fixture.archive.inspect_batch(batch_id).unwrap() {
            BatchInspection::Ready(batch) => batch,
            other => panic!("clean external batch did not become Ready: {other:?}"),
        };
        let BatchOrigin::ExternalReconciliation { import_id } = batch.manifest().origin() else {
            panic!("clean external batch lost its external origin")
        };
        assert_eq!(batch_id, import_id.batch_id());
        fixture.assert_clean_drained();
        assert_eq!(
            fs::read(fixture.graph_root.join(path)).unwrap(),
            b"- root edited\n  - child edited\n"
        );
    }

    #[test]
    fn admitted_local_semantic_mutation_commits_history_sqlite_and_projection_once_clean() {
        let mut fixture = CleanCoordinatorFixture::configured("local-success");
        let path = fixture.path.clone();
        let manifests_before = fixture.archive.committed_manifests().unwrap().len();
        let accepted_before = fixture.engine().accepted_batch_count().unwrap();
        let sqlite_before = fixture.database().applied_batch_count().unwrap();
        let releases_before = fixture.graph.handoff_release_count();
        reset_projection_graph_test_counters();

        let batch_id =
            expect_clean_local_complete(fixture.local_edit("local semantic edit").unwrap());

        assert_eq!(
            fixture.archive.committed_manifests().unwrap().len(),
            manifests_before + 1
        );
        assert_eq!(
            fixture.engine().accepted_batch_count().unwrap(),
            accepted_before + 1
        );
        assert_eq!(
            fixture.database().applied_batch_count().unwrap(),
            sqlite_before + 1
        );
        assert!(fixture.database().contains_batch(batch_id).unwrap());
        let batch = match fixture.archive.inspect_batch(batch_id).unwrap() {
            BatchInspection::Ready(batch) => batch,
            other => panic!("clean local batch did not become Ready: {other:?}"),
        };
        assert_eq!(batch.manifest().origin(), BatchOrigin::LocalMutation);
        assert_eq!(projection_graph_test_counters().write_calls, 1);
        assert_eq!(fixture.graph.handoff_release_count(), releases_before + 1);
        assert_eq!(
            fs::read(fixture.graph_root.join(path)).unwrap(),
            clean_coordinator_source("pages/a.md", "local semantic edit", "child")
        );
        fixture.assert_clean_drained();
    }

    #[test]
    fn local_exact_path_drift_requests_reconciliation_without_publication_clean() {
        let mut fixture = CleanCoordinatorFixture::new("local-reconcile-first");
        let path = fixture.path.clone();
        fixture.overwrite(b"- externally moved local base\n  - child\n");
        let immutable_before = snapshot_immutable_publication(&fixture.archive_root);
        let frontier_before = fixture.engine().accepted_frontier_root().unwrap();
        let sqlite_before = fixture.database().frontier_root().unwrap();
        reset_projection_graph_test_counters();

        let error = match fixture.local_edit("must not overwrite external bytes") {
            Err(error) => error,
            Ok(_) => panic!("exact clean local path drift must request reconciliation"),
        };
        assert_eq!(error.phase(), OperationalPhase::Planning);
        assert!(error.detail().contains("requires external reconciliation"));
        assert_eq!(
            snapshot_immutable_publication(&fixture.archive_root),
            immutable_before
        );
        assert_eq!(
            fixture.engine().accepted_frontier_root().unwrap(),
            frontier_before
        );
        assert_eq!(fixture.database().frontier_root().unwrap(), sqlite_before);
        assert_eq!(projection_graph_test_counters().write_calls, 0);
        assert_eq!(
            fs::read(fixture.graph_root.join(path)).unwrap(),
            b"- externally moved local base\n  - child\n"
        );
        fixture.graph.probe_managed_text_writer().unwrap();
    }

    #[test]
    fn stale_local_binding_is_typed_blocked_before_any_writer_side_effect_clean() {
        let mut fixture = CleanCoordinatorFixture::new("local-stale-binding");
        let foreign_root = TestRoot::new("local-stale-binding-clean-foreign");
        let foreign_graph_root = foreign_root.path().join("graph");
        fs::create_dir_all(&foreign_graph_root).unwrap();
        let foreign_graph = Graph::open(&foreign_graph_root);
        let foreign_endpoint = ProjectionEndpointBinding::enroll_graph(
            &foreign_graph,
            ProjectionEndpointId::from_uuid(Uuid::from_u128(44_100)),
            DeviceId::from_uuid(Uuid::from_u128(44_101)),
        )
        .unwrap();
        let foreign_receipts = ProjectionReceiptStore::open_for_endpoint(
            &foreign_root.path().join("receipts"),
            fixture.engine().workspace_id(),
            foreign_endpoint,
        )
        .unwrap();
        let immutable_before = snapshot_immutable_publication(&fixture.archive_root);
        let frontier_before = fixture.engine().accepted_frontier_root().unwrap();
        let sqlite_before = fixture.database().frontier_root().unwrap();
        let transaction = fixture.local_edit_transaction("blocked stale binding");
        let mut session = fixture
            .runtime
            .admit_clean_mutation(&fixture.graph)
            .unwrap();

        let error = match OperationalCoordinator::execute_clean_local(
            &mut session,
            &fixture.graph,
            &foreign_receipts,
            &transaction,
        ) {
            Err(error) => error,
            Ok(_) => panic!("a stale clean local binding must be typed before publication"),
        };
        assert_eq!(error.phase(), OperationalPhase::Bindings);
        assert_eq!(
            snapshot_immutable_publication(&fixture.archive_root),
            immutable_before
        );
        assert_eq!(
            fixture.engine().accepted_frontier_root().unwrap(),
            frontier_before
        );
        assert_eq!(fixture.database().frontier_root().unwrap(), sqlite_before);
        fixture.graph.probe_managed_text_writer().unwrap();
    }

    #[test]
    fn local_and_external_mutations_enter_the_identical_terminal_pipeline_clean() {
        let mut local = CleanCoordinatorFixture::new("shared-terminal-local");
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let local_pending =
            expect_clean_local_pending(local.local_edit("shared terminal local").unwrap());
        let local_batch = local_pending.batch_id();
        let local_manifest = match local.archive.inspect_batch(local_batch).unwrap() {
            BatchInspection::Ready(batch) => batch,
            other => panic!("clean local publication is not Ready: {other:?}"),
        };
        assert_eq!(
            local_manifest.manifest().origin(),
            BatchOrigin::LocalMutation
        );
        assert_eq!(
            expect_clean_local_complete(local.retry_clean(local_pending)),
            local_batch
        );
        local.assert_clean_drained();

        let mut external = CleanCoordinatorFixture::new("shared-terminal-external");
        let path = external.path.clone();
        external.overwrite(b"- shared terminal external\n  - child\n");
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let external_pending =
            expect_clean_external_pending(external.execute_external(&[&path]).unwrap());
        let external_batch = external_pending.batch_id();
        let external_manifest = match external.archive.inspect_batch(external_batch).unwrap() {
            BatchInspection::Ready(batch) => batch,
            other => panic!("clean external publication is not Ready: {other:?}"),
        };
        assert!(matches!(
            external_manifest.manifest().origin(),
            BatchOrigin::ExternalReconciliation { .. }
        ));
        assert_eq!(
            expect_clean_local_complete(external.retry_clean(external_pending)),
            external_batch
        );
        external.assert_clean_drained();
    }

    #[test]
    fn local_late_failure_retries_exact_publication_without_a_second_writer_clean() {
        for (index, point) in [
            OperationalFaultPoint::AfterManifest,
            OperationalFaultPoint::AfterSqliteApply,
            OperationalFaultPoint::BeforeProjection,
            OperationalFaultPoint::AfterProjection,
        ]
        .into_iter()
        .enumerate()
        {
            let mut fixture = CleanCoordinatorFixture::new(&format!("local-late-{point:?}"));
            let path = fixture.path.clone();
            let manifests_before = fixture.archive.committed_manifests().unwrap().len();
            let releases_before = fixture.graph.handoff_release_count();
            reset_projection_graph_test_counters();
            fail_once_at(OperationalFaultPoint::AfterManifest);
            let mut pending =
                expect_clean_local_pending(fixture.local_edit("late local edit").unwrap());
            let batch_id = pending.batch_id();
            assert_eq!(
                fixture.archive.committed_manifests().unwrap().len(),
                manifests_before + 1
            );
            assert!(fixture.graph.probe_managed_text_writer().is_err());

            if point != OperationalFaultPoint::AfterManifest {
                fail_once_at(point);
                pending = expect_clean_local_pending(fixture.retry_clean(pending));
                assert_eq!(pending.batch_id(), batch_id);
                assert_eq!(fixture.graph.handoff_release_count(), releases_before);
                assert!(fixture.graph.probe_managed_text_writer().is_err());
            }
            let completion = expect_clean_local_complete(fixture.retry_clean(pending));
            assert_eq!(
                completion, batch_id,
                "case {index} changed clean publication"
            );
            assert_eq!(
                fixture.archive.committed_manifests().unwrap().len(),
                manifests_before + 1,
                "late clean retry republished the local mutation"
            );
            assert!(projection_graph_test_counters().write_calls <= 1);
            assert_eq!(
                fs::read(fixture.graph_root.join(&path)).unwrap(),
                clean_coordinator_source(&path, "root", "late local edit")
            );
            fixture.graph.probe_managed_text_writer().unwrap();
            fixture.assert_clean_drained();
        }
    }

    #[test]
    fn local_semantic_paths_accept_nested_nonstandard_utf8_markdown_and_org_clean() {
        for (index, path) in [
            "content/pages/研究/über topic.md",
            "content/pages/研究/über topic.org",
        ]
        .into_iter()
        .enumerate()
        {
            let mut fixture = CleanCoordinatorFixture::new_at(
                &format!("local-utf-{index}"),
                path,
                Some(
                    "{:pages-directory \"content/pages\"\n\
                      :journals-directory \"content/journals\"}\n",
                ),
                ManagedTextKind::Page,
            );
            expect_clean_local_complete(fixture.local_edit("utf local edit").unwrap());
            let expected = if path.ends_with(".org") {
                clean_coordinator_source(path, "utf local edit", "child")
            } else {
                clean_coordinator_source(path, "root", "utf local edit")
            };
            assert_eq!(fs::read(fixture.graph_root.join(path)).unwrap(), expected);
            fixture.assert_clean_drained();
        }
    }

    #[test]
    fn in_place_catalog_derivation_publishes_the_same_markdown_and_org_source_clean() {
        for (index, path) in [
            "content/pages/研究/über topic.md",
            "content/pages/研究/über topic.org",
        ]
        .into_iter()
        .enumerate()
        {
            let mut fixture = CleanCoordinatorFixture::new_at(
                &format!("local-derivation-{index}"),
                path,
                Some(
                    "{:pages-directory \"content/pages\"\n\
                      :journals-directory \"content/journals\"}\n",
                ),
                ManagedTextKind::Page,
            );

            let edit_author = fixture.local_author(44_000 + index as u128 * 100);
            let edit = fixture.local_edit_transaction("TODO utf derivation edit [[Other Page]]");
            let observed = fixture.engine().assert_draft_matches_previous_derivation(
                edit_author,
                BatchOrigin::LocalMutation,
                &edit,
            );
            assert_eq!(observed.refused, None);
            assert_eq!(observed.optimized_catalog_copies, 0);
            assert!(observed.oracle_catalog_copies >= 1);
            let state = fixture.execute_local(&edit).unwrap();
            settle_clean_local(&mut fixture, state);
            let expected = if path.ends_with(".org") {
                clean_coordinator_source(path, "TODO utf derivation edit [[Other Page]]", "child")
            } else {
                clean_coordinator_source(path, "root", "TODO utf derivation edit [[Other Page]]")
            };
            assert_eq!(fs::read(fixture.graph_root.join(path)).unwrap(), expected);
            fixture.assert_clean_drained();

            let insert_author = fixture.local_author(44_050 + index as u128 * 100);
            let insert = OperationTransaction::new(vec![SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id: BlockId::from_uuid(Uuid::from_u128(44_900 + index as u128)),
                    home_document_id: fixture.home_document_id,
                },
                page_id: fixture.page_id,
                parent: None,
                order: "b".into(),
                content: "DONE appended tail".into(),
            }])
            .unwrap();
            let observed = fixture.engine().assert_draft_matches_previous_derivation(
                insert_author,
                BatchOrigin::LocalMutation,
                &insert,
            );
            assert_eq!(observed.refused, None);
            assert_eq!(observed.optimized_catalog_copies, 0);
            assert!(observed.oracle_catalog_copies >= 1);
            let state = fixture.execute_local(&insert).unwrap();
            settle_clean_local(&mut fixture, state);
            fixture.assert_clean_drained();

            let settle_author = fixture.local_author(44_070 + index as u128 * 100);
            let settle = fixture.local_edit_transaction("TODO settled after deferral");
            let observed = fixture.engine().assert_draft_matches_previous_derivation(
                settle_author,
                BatchOrigin::LocalMutation,
                &settle,
            );
            assert_eq!(observed.refused, None);
            assert_eq!(observed.optimized_catalog_copies, 0);
            fail_once_at(OperationalFaultPoint::AfterManifest);
            let pending = expect_clean_local_pending(fixture.execute_local(&settle).unwrap());
            let batch_id = pending.batch_id();
            fail_once_at(OperationalFaultPoint::BeforeProjection);
            let pending = expect_clean_local_pending(fixture.retry_clean(pending));
            assert_eq!(pending.batch_id(), batch_id);
            assert_eq!(
                expect_clean_local_complete(fixture.retry_clean(pending)),
                batch_id
            );
            fixture.assert_clean_drained();

            let restarted = fixture.restart_clean_runtime();
            restarted.assert_clean_drained();
            assert_eq!(
                fs::read(restarted.graph_root.join(path)).unwrap(),
                fs::read(restarted.graph_root.join(&restarted.path)).unwrap()
            );
        }
    }

    #[test]
    fn local_continuation_drop_stays_closed_and_completion_releases_once_clean() {
        let mut dropped = CleanCoordinatorFixture::new("drop-local-published");
        let releases = dropped.graph.handoff_release_count();
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let continuation =
            expect_clean_local_pending(dropped.local_edit("drop local continuation").unwrap());
        drop(continuation);
        assert_eq!(dropped.graph.handoff_release_count(), releases);
        assert!(dropped.graph.probe_managed_text_writer().is_err());

        let mut completed = CleanCoordinatorFixture::new("complete-local-once");
        let releases = completed.graph.handoff_release_count();
        expect_clean_local_complete(completed.local_edit("complete local once").unwrap());
        assert_eq!(completed.graph.handoff_release_count(), releases + 1);
        completed.graph.probe_managed_text_writer().unwrap();
        completed.graph.probe_managed_text_writer().unwrap();
        assert_eq!(completed.graph.handoff_release_count(), releases + 1);
    }

    #[test]
    fn retained_terminal_dispositions_are_blocked_while_progress_is_recovering_clean() {
        let mut fixture = CleanCoordinatorFixture::new("retained-clean-published");
        let releases = fixture.graph.handoff_release_count();
        let manifests_before = fixture.archive.committed_manifests().unwrap().len();
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let mut pending =
            expect_clean_local_pending(fixture.local_edit("retained clean core").unwrap());
        let batch_id = pending.batch_id();
        let published = snapshot_immutable_publication(&fixture.archive_root);
        fail_repeatedly_at(OperationalFaultPoint::BeforeProjection, 2);

        for _ in 0..2 {
            pending = expect_clean_local_pending(fixture.retry_clean(pending));
            assert_eq!(pending.batch_id(), batch_id);
            assert_eq!(pending.failure().phase(), OperationalPhase::ProjectionDrain);
            assert_eq!(
                snapshot_immutable_publication(&fixture.archive_root),
                published
            );
            assert_eq!(fixture.graph.handoff_release_count(), releases);
            assert!(fixture.graph.probe_managed_text_writer().is_err());
        }
        assert_eq!(
            fixture.archive.committed_manifests().unwrap().len(),
            manifests_before + 1
        );
        assert_eq!(
            expect_clean_local_complete(fixture.retry_clean(pending)),
            batch_id
        );
        assert_eq!(fixture.graph.handoff_release_count(), releases + 1);
        fixture.graph.probe_managed_text_writer().unwrap();
        fixture.assert_clean_drained();
    }

    #[test]
    fn rejected_published_local_batch_retains_typed_blocked_evidence_clean() {
        let mut fixture = CleanCoordinatorFixture::new("published-rejected-blocked");
        let sqlite_before = fixture.database().frontier_root().unwrap();
        let releases = fixture.graph.handoff_release_count();
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let pending =
            expect_clean_local_pending(fixture.local_edit("durable history rejection").unwrap());
        let batch_id = pending.batch_id();
        let manifest = fixture
            .archive_root
            .join(crate::oplog::sync_layout::ARCHIVE_BATCHES_DIR)
            .join(format!("{batch_id}.manifest"));
        let held = manifest.with_extension("manifest.held");
        fs::rename(&manifest, &held).unwrap();

        let pending = expect_clean_local_pending(fixture.retry_clean(pending));
        assert_eq!(pending.batch_id(), batch_id);
        assert_eq!(pending.failure().phase(), OperationalPhase::ArchiveStage);
        assert_eq!(
            pending.failure().retained_block_reason(),
            Some(&RetainedBlockReason::PublishedAuthentication)
        );
        assert_eq!(fixture.database().frontier_root().unwrap(), sqlite_before);
        assert_eq!(fixture.graph.handoff_release_count(), releases);
        assert!(fixture.graph.probe_managed_text_writer().is_err());

        fs::rename(held, manifest).unwrap();
        assert_eq!(
            expect_clean_local_complete(fixture.retry_clean(pending)),
            batch_id
        );
        assert_eq!(fixture.graph.handoff_release_count(), releases + 1);
        fixture.assert_clean_drained();
    }

    #[test]
    fn stable_postpublication_binding_failure_retains_typed_blocked_continuation_clean() {
        let mut fixture = CleanCoordinatorFixture::new("local-stable-postpublication-binding");
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let pending = expect_clean_local_pending(fixture.local_edit("stable binding").unwrap());
        let batch_id = pending.batch_id();
        let foreign_root = TestRoot::new("local-stable-postpublication-binding-clean-foreign");
        let foreign_graph_root = foreign_root.path().join("graph");
        fs::create_dir_all(&foreign_graph_root).unwrap();
        let foreign_graph = Graph::open(&foreign_graph_root);
        let mut session = fixture
            .runtime
            .admit_clean_derived_recovery(&fixture.graph)
            .unwrap();

        let pending = expect_clean_local_pending(OperationalCoordinator::retry_clean_local(
            &mut session,
            &foreign_graph,
            &fixture.receipts,
            pending,
        ));
        assert_eq!(pending.batch_id(), batch_id);
        assert_eq!(pending.failure().phase(), OperationalPhase::Bindings);
        assert!(fixture.graph.probe_managed_text_writer().is_err());
        assert_eq!(
            expect_clean_local_complete(fixture.retry_clean(pending)),
            batch_id
        );
        fixture.assert_clean_drained();
    }

    #[test]
    fn blocked_and_noop_cancel_without_durable_or_derived_mutation_clean() {
        let mut fixture = CleanCoordinatorFixture::new("blocked-noop");
        let accepted = fixture.engine().accepted_frontier_root().unwrap();
        let sqlite = fixture.database().frontier_root().unwrap();
        let graph = fs::read(fixture.graph_root.join(&fixture.path)).unwrap();
        let archive = snapshot_tree(&fixture.archive_root);
        let receipts = snapshot_tree(fixture.receipts.root_path());

        let blocked = fixture.execute_external(&["../escape.md"]);
        assert!(matches!(
            blocked,
            Err(OperationalCoordinatorError {
                phase: OperationalPhase::Planning,
                ..
            })
        ));
        let path = fixture.path.clone();
        assert!(matches!(
            fixture.execute_external(&[&path]).unwrap(),
            CleanExternalMutationState::Noop
        ));
        assert_eq!(fixture.engine().accepted_frontier_root().unwrap(), accepted);
        assert_eq!(fixture.database().frontier_root().unwrap(), sqlite);
        assert_eq!(
            fs::read(fixture.graph_root.join(&fixture.path)).unwrap(),
            graph
        );
        assert_eq!(snapshot_tree(&fixture.archive_root), archive);
        assert_eq!(snapshot_tree(fixture.receipts.root_path()), receipts);
        fixture.graph.probe_managed_text_writer().unwrap();
    }

    #[test]
    fn every_pre_manifest_boundary_releases_and_allows_fresh_retry_clean() {
        for (index, point) in [
            OperationalFaultPoint::AfterHandoff,
            OperationalFaultPoint::AfterDraft,
            OperationalFaultPoint::AfterCapture,
            OperationalFaultPoint::AfterFinalize,
        ]
        .into_iter()
        .enumerate()
        {
            let mut fixture = CleanCoordinatorFixture::new(&format!("pre-manifest-local-{index}"));
            let accepted = fixture.engine().accepted_frontier_root().unwrap();
            let sqlite = fixture.database().frontier_root().unwrap();
            let archive = snapshot_immutable_publication(&fixture.archive_root);
            fail_once_at(point);
            assert!(
                fixture.local_edit("changed local").is_err(),
                "{point:?} was not reached"
            );
            assert_eq!(fixture.engine().accepted_frontier_root().unwrap(), accepted);
            assert_eq!(fixture.database().frontier_root().unwrap(), sqlite);
            assert_eq!(
                snapshot_immutable_publication(&fixture.archive_root),
                archive
            );
            fixture.graph.probe_managed_text_writer().unwrap();
            expect_clean_local_complete(fixture.local_edit("changed local").unwrap());
            fixture.assert_clean_drained();
        }

        for (index, point) in [
            OperationalFaultPoint::AfterHandoff,
            OperationalFaultPoint::AfterPlan,
            OperationalFaultPoint::AfterDraft,
            OperationalFaultPoint::AfterCapture,
            OperationalFaultPoint::AfterFinalize,
        ]
        .into_iter()
        .enumerate()
        {
            let mut fixture =
                CleanCoordinatorFixture::new(&format!("pre-manifest-external-{index}"));
            let path = fixture.path.clone();
            fixture.overwrite(b"- changed external\n  - still nested\n");
            let accepted = fixture.engine().accepted_frontier_root().unwrap();
            let sqlite = fixture.database().frontier_root().unwrap();
            let archive = snapshot_immutable_publication(&fixture.archive_root);
            fail_once_at(point);
            assert!(
                fixture.execute_external(&[&path]).is_err(),
                "{point:?} was not reached"
            );
            assert_eq!(fixture.engine().accepted_frontier_root().unwrap(), accepted);
            assert_eq!(fixture.database().frontier_root().unwrap(), sqlite);
            assert_eq!(
                snapshot_immutable_publication(&fixture.archive_root),
                archive
            );
            fixture.graph.probe_managed_text_writer().unwrap();
            expect_clean_external_complete(fixture.execute_external(&[&path]).unwrap());
            fixture.assert_clean_drained();
        }
    }

    #[test]
    fn exact_reservation_precedes_manifest_and_object_only_cut_has_no_semantic_effect_clean() {
        let mut fixture = CleanCoordinatorFixture::new("objects-only");
        let sqlite = fixture.database().frontier_root().unwrap();
        let manifests = fixture.archive.committed_manifests().unwrap();
        let before = snapshot_tree(&fixture.archive_root);
        fail_next_publish_after_objects();
        let error = match fixture.local_edit("objects only") {
            Err(error) => error,
            Ok(_) => panic!("object-only clean publication cut unexpectedly committed"),
        };
        assert_eq!(error.phase(), OperationalPhase::Publication);
        assert_eq!(fixture.database().frontier_root().unwrap(), sqlite);
        assert_eq!(fixture.archive.committed_manifests().unwrap(), manifests);
        assert_ne!(snapshot_tree(&fixture.archive_root), before);
        assert!(
            fixture.engine().materialize_page(fixture.page_id).is_err(),
            "the speculative failed-commit engine must remain poisoned and unobservable"
        );
        fixture.graph.probe_managed_text_writer().unwrap();
    }

    #[test]
    fn every_post_manifest_failure_retains_guard_and_retries_idempotently_clean() {
        for (index, point) in [
            OperationalFaultPoint::AfterManifest,
            OperationalFaultPoint::AfterSqliteApply,
            OperationalFaultPoint::BeforeProjection,
            OperationalFaultPoint::AfterProjection,
        ]
        .into_iter()
        .enumerate()
        {
            let mut fixture = CleanCoordinatorFixture::new(&format!("post-manifest-{index}"));
            let path = fixture.path.clone();
            fixture.overwrite(b"- durable edit\n  - nested durable edit\n");
            let releases = fixture.graph.handoff_release_count();
            fail_once_at(OperationalFaultPoint::AfterManifest);
            let mut pending =
                expect_clean_external_pending(fixture.execute_external(&[&path]).unwrap());
            let batch_id = pending.batch_id();
            assert!(fixture.graph.probe_managed_text_writer().is_err());
            if point != OperationalFaultPoint::AfterManifest {
                fail_once_at(point);
                pending = expect_clean_local_pending(fixture.retry_clean(pending));
                assert_eq!(pending.batch_id(), batch_id);
                assert_eq!(fixture.graph.handoff_release_count(), releases);
                assert!(fixture.graph.probe_managed_text_writer().is_err());
            }
            assert_eq!(
                expect_clean_local_complete(fixture.retry_clean(pending)),
                batch_id
            );
            assert_eq!(fixture.graph.handoff_release_count(), releases + 1);
            fixture.graph.probe_managed_text_writer().unwrap();
            fixture.assert_clean_drained();
            assert_eq!(
                fs::read(fixture.graph_root.join(path)).unwrap(),
                b"- durable edit\n  - nested durable edit\n"
            );
        }
    }

    #[test]
    fn sqlite_budget_boundary_retains_handoff_and_resumes_without_republication_clean() {
        let mut fixture = CleanCoordinatorFixture::new("bounded-clean-resume");
        let releases = fixture.graph.handoff_release_count();
        let manifests_before = fixture.archive.committed_manifests().unwrap().len();
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let mut pending =
            expect_clean_local_pending(fixture.local_edit("immutable clean retry").unwrap());
        let batch_id = pending.batch_id();
        let published = snapshot_immutable_publication(&fixture.archive_root);
        let published_count = fixture.archive.committed_manifests().unwrap().len();
        assert_eq!(published_count, manifests_before + 1);
        fail_repeatedly_at(OperationalFaultPoint::BeforeProjection, 3);

        for _ in 0..3 {
            assert_eq!(
                snapshot_immutable_publication(&fixture.archive_root),
                published
            );
            assert_eq!(
                fixture.archive.committed_manifests().unwrap().len(),
                published_count
            );
            assert_eq!(fixture.graph.handoff_release_count(), releases);
            assert!(fixture.graph.probe_managed_text_writer().is_err());
            pending = expect_clean_local_pending(fixture.retry_clean(pending));
            assert_eq!(pending.batch_id(), batch_id);
            assert_eq!(pending.failure().phase(), OperationalPhase::ProjectionDrain);
        }

        assert_eq!(
            expect_clean_local_complete(fixture.retry_clean(pending)),
            batch_id
        );
        assert_eq!(fixture.graph.handoff_release_count(), releases + 1);
        assert_eq!(
            snapshot_immutable_publication(&fixture.archive_root),
            published
        );
        assert_eq!(
            fixture.archive.committed_manifests().unwrap().len(),
            published_count
        );
        fixture.graph.probe_managed_text_writer().unwrap();
        fixture.graph.probe_managed_text_writer().unwrap();
        assert_eq!(fixture.graph.handoff_release_count(), releases + 1);
        fixture.assert_clean_drained();
    }

    #[test]
    fn published_continuation_survives_same_process_engine_reconstruction_clean() {
        let mut fixture = CleanCoordinatorFixture::new("engine-reconstruction");
        let path = fixture.path.clone();
        fixture.overwrite(b"- reconstructed continuation\n  - nested\n");
        let releases = fixture.graph.handoff_release_count();
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let pending = expect_clean_external_pending(fixture.execute_external(&[&path]).unwrap());
        let batch_id = pending.batch_id();
        assert!(fixture.graph.probe_managed_text_writer().is_err());

        fixture = fixture.restart_clean_runtime();
        assert_eq!(fixture.graph.handoff_release_count(), releases);
        assert_eq!(
            expect_clean_local_complete(fixture.retry_clean(pending)),
            batch_id
        );
        assert_eq!(fixture.graph.handoff_release_count(), releases + 1);
        fixture.graph.probe_managed_text_writer().unwrap();
        fixture.assert_clean_drained();
        assert_eq!(
            fs::read(fixture.graph_root.join(path)).unwrap(),
            b"- reconstructed continuation\n  - nested\n"
        );
    }

    #[test]
    fn dropping_published_continuation_stays_closed_and_completion_releases_once_clean() {
        let mut dropped = CleanCoordinatorFixture::new("drop-published");
        let path = dropped.path.clone();
        dropped.overwrite(b"- durable dropped continuation\n");
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let pending = expect_clean_external_pending(dropped.execute_external(&[&path]).unwrap());
        let releases = dropped.graph.handoff_release_count();
        drop(pending);
        assert_eq!(dropped.graph.handoff_release_count(), releases);
        assert!(dropped.graph.probe_managed_text_writer().is_err());

        let mut completed = CleanCoordinatorFixture::new("complete-once");
        let path = completed.path.clone();
        completed.overwrite(b"- successful explicit completion\n");
        let releases = completed.graph.handoff_release_count();
        expect_clean_external_complete(completed.execute_external(&[&path]).unwrap());
        assert_eq!(completed.graph.handoff_release_count(), releases + 1);
        completed.graph.probe_managed_text_writer().unwrap();
        completed.graph.probe_managed_text_writer().unwrap();
        assert_eq!(completed.graph.handoff_release_count(), releases + 1);
    }

    #[test]
    fn manifested_preconditions_are_exact_fresh_external_observations_clean() {
        let mut edit = CleanCoordinatorFixture::new("observed-precondition-edit");
        let path = edit.path.clone();
        let prior = fs::read(edit.graph_root.join(&path)).unwrap();
        let observed = b"- externally changed bytes\n  - current annotation source\n".to_vec();
        edit.overwrite(&observed);
        let batch_id = expect_clean_external_complete(edit.execute_external(&[&path]).unwrap());
        let batch = match edit.archive.inspect_batch(batch_id).unwrap() {
            BatchInspection::Ready(batch) => batch,
            other => panic!("completed batch is not immutable Ready: {other:?}"),
        };
        let intent = batch
            .objects()
            .iter()
            .find(|object| object.kind() == ObjectKind::ProjectionIntent)
            .map(|object| ManifestedProjectionIntent::decode(object.payload()).unwrap())
            .expect("edit carries manifested projection intent");
        let ManifestProjectionPrecondition::Present { base } = intent.precondition() else {
            panic!("fresh present edit must manifest Present");
        };
        let manifested_base = batch
            .objects()
            .iter()
            .find(|object| {
                object.kind() == ObjectKind::AnnotatedBaseBlob
                    && object.document_id() == base.document_id()
            })
            .map(|object| AnnotatedProjectionBase::decode(object.payload()).unwrap())
            .expect("manifested observed base exists");
        assert_eq!(manifested_base.bytes(), observed);
        assert_ne!(manifested_base.bytes(), prior);

        let mut absent = CleanCoordinatorFixture::new("observed-precondition-absent");
        let path = absent.path.clone();
        fs::remove_file(absent.graph_root.join(&path)).unwrap();
        let batch_id = expect_clean_external_complete(absent.execute_external(&[&path]).unwrap());
        let batch = match absent.archive.inspect_batch(batch_id).unwrap() {
            BatchInspection::Ready(batch) => batch,
            other => panic!("completed batch is not immutable Ready: {other:?}"),
        };
        let intent = batch
            .objects()
            .iter()
            .find(|object| object.kind() == ObjectKind::ProjectionIntent)
            .map(|object| ManifestedProjectionIntent::decode(object.payload()).unwrap())
            .expect("delete carries manifested projection intent");
        assert!(matches!(
            intent.precondition(),
            ManifestProjectionPrecondition::Absent
        ));
    }

    #[test]
    fn crdt_peer_probe_is_bounded_for_zero_collision_and_exhaustion_clean() {
        let fixture = CleanCoordinatorFixture::new("peer-probe");
        let path = fixture.path.clone();
        fixture.overwrite(b"- peer probe edit\n");
        let plan = plan_clean_affected_import(
            &fixture.graph,
            fixture.engine(),
            fixture.database(),
            &[&path],
        );
        let material = plan.into_execution_material().unwrap();
        let endpoint = fixture.engine().projection_endpoint_binding().unwrap();
        let claim_source = fixture.database().materialized_read().unwrap();
        let genesis_peer = 0x5449_4e45_4745_4e31;
        let candidates = [0, genesis_peer, genesis_peer + 1];
        let (author, _) = draft_with_bounded_peer_candidates(
            fixture.engine(),
            endpoint,
            &material,
            Some(&claim_source),
            |attempt| CrdtPeerId::from_u64(candidates[usize::try_from(attempt).unwrap().min(2)]),
        )
        .unwrap();
        assert_eq!(author.crdt_peer_id, CrdtPeerId::from_u64(genesis_peer + 1));

        let exhausted = match draft_with_bounded_peer_candidates(
            fixture.engine(),
            endpoint,
            &material,
            Some(&claim_source),
            |_| CrdtPeerId::from_u64(genesis_peer),
        ) {
            Err(error) => error,
            Ok(_) => panic!("colliding bounded peer probe unexpectedly succeeded"),
        };
        assert_eq!(exhausted.phase(), OperationalPhase::Draft);
        assert!(exhausted.detail().contains("bounded 8-candidate probe"));
        fixture.graph.probe_managed_text_writer().unwrap();
    }

    #[test]
    fn delete_and_rename_project_exact_old_removal_and_new_render_base_clean() {
        let mut deletion = CleanCoordinatorFixture::new("delete");
        let delete_path = deletion.path.clone();
        fs::remove_file(deletion.graph_root.join(&delete_path)).unwrap();
        expect_clean_external_complete(deletion.execute_external(&[&delete_path]).unwrap());
        deletion.assert_clean_drained();
        assert!(!deletion.graph_root.join(delete_path).exists());

        let mut rename = CleanCoordinatorFixture::new("rename");
        let old = rename.path.clone();
        let new = "pages/elsewhere/deeper/renamed.md";
        fs::create_dir_all(rename.graph_root.join(new).parent().unwrap()).unwrap();
        fs::rename(rename.graph_root.join(&old), rename.graph_root.join(new)).unwrap();
        let batch_id =
            expect_clean_external_complete(rename.execute_external(&[&old, new]).unwrap());
        rename.assert_clean_drained();
        assert!(!rename.graph_root.join(&old).exists());
        assert_eq!(
            fs::read(rename.graph_root.join(new)).unwrap(),
            b"- root\n  - child\n"
        );
        let batch = match rename.archive.inspect_batch(batch_id).unwrap() {
            BatchInspection::Ready(batch) => batch,
            other => panic!("completed rename batch is not Ready: {other:?}"),
        };
        let intents = batch
            .objects()
            .iter()
            .filter(|object| object.kind() == ObjectKind::ProjectionIntent)
            .map(|object| ManifestedProjectionIntent::decode(object.payload()).unwrap())
            .collect::<Vec<_>>();
        let old_intent = intents
            .iter()
            .find(|intent| intent.path().as_str() == old)
            .expect("rename carries old-path removal");
        assert!(matches!(
            old_intent.precondition(),
            ManifestProjectionPrecondition::Absent
        ));
        let new_intent = intents
            .iter()
            .find(|intent| intent.path().as_str() == new)
            .expect("rename carries new-path projection");
        let ManifestProjectionPrecondition::Present { base } = new_intent.precondition() else {
            panic!("fresh rename destination is present");
        };
        let base = batch
            .objects()
            .iter()
            .find(|object| {
                object.kind() == ObjectKind::AnnotatedBaseBlob
                    && object.document_id() == base.document_id()
            })
            .map(|object| AnnotatedProjectionBase::decode(object.payload()).unwrap())
            .expect("rename destination observed base exists");
        assert_eq!(base.bytes(), b"- root\n  - child\n");
    }

    #[test]
    fn binding_mismatch_rejects_before_handoff_or_publication_clean() {
        let mut fixture = CleanCoordinatorFixture::new("binding");
        let foreign_root = TestRoot::new("foreign-receipts-clean");
        let foreign_graph_root = foreign_root.path().join("graph");
        fs::create_dir_all(&foreign_graph_root).unwrap();
        let foreign_graph = Graph::open(&foreign_graph_root);
        let foreign_endpoint = ProjectionEndpointBinding::enroll_graph(
            &foreign_graph,
            ProjectionEndpointId::from_uuid(Uuid::from_u128(900)),
            DeviceId::from_uuid(Uuid::from_u128(901)),
        )
        .unwrap();
        let foreign = ProjectionReceiptStore::open_for_endpoint(
            &foreign_root.path().join("receipts"),
            fixture.engine().workspace_id(),
            foreign_endpoint,
        )
        .unwrap();
        let releases = fixture.graph.handoff_release_count();
        let transaction = fixture.local_edit_transaction("rejected binding");
        let mut session = fixture
            .runtime
            .admit_clean_mutation(&fixture.graph)
            .unwrap();
        assert!(matches!(
            OperationalCoordinator::execute_clean_local(
                &mut session,
                &fixture.graph,
                &foreign,
                &transaction,
            ),
            Err(OperationalCoordinatorError {
                phase: OperationalPhase::Bindings,
                ..
            })
        ));
        assert_eq!(fixture.graph.handoff_release_count(), releases);
        fixture.graph.probe_managed_text_writer().unwrap();
        assert!(fixture.archive.committed_manifests().unwrap().is_empty());
    }

    #[test]
    fn post_manifest_retry_rejects_rebound_graph_and_keeps_original_guard_clean() {
        let mut fixture = CleanCoordinatorFixture::new("retry-binding");
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let pending =
            expect_clean_local_pending(fixture.local_edit("durable retry binding").unwrap());
        let batch_id = pending.batch_id();

        let foreign_root = TestRoot::new("retry-binding-clean-foreign");
        let foreign_graph_root = foreign_root.path().join("graph");
        fs::create_dir_all(&foreign_graph_root).unwrap();
        let foreign_graph = Graph::open(&foreign_graph_root);
        let mut session = fixture
            .runtime
            .admit_clean_derived_recovery(&fixture.graph)
            .unwrap();
        let pending = expect_clean_local_pending(OperationalCoordinator::retry_clean_local(
            &mut session,
            &foreign_graph,
            &fixture.receipts,
            pending,
        ));
        assert_eq!(pending.batch_id(), batch_id);
        assert_eq!(pending.failure().phase(), OperationalPhase::Bindings);
        assert!(fixture.graph.probe_managed_text_writer().is_err());
        assert_eq!(
            expect_clean_local_complete(fixture.retry_clean(pending)),
            batch_id
        );
        fixture.graph.probe_managed_text_writer().unwrap();
        fixture.assert_clean_drained();
    }

    #[test]
    fn reordered_batch_ids_drain_by_authenticated_acceptance_sequence_clean() {
        let mut fixture = CleanCoordinatorFixture::new("acceptance-sequence");
        let first_id = BatchId::from_uuid(Uuid::from_u128(u128::MAX - 1));
        let second_id = BatchId::from_uuid(Uuid::from_u128(20));
        let first = fixture.local_edit_transaction("first accepted");
        assert_eq!(
            expect_clean_local_complete(
                fixture.execute_local_correlated(first_id, &first).unwrap()
            ),
            first_id
        );
        let second = fixture.local_edit_transaction("second accepted");
        assert_eq!(
            expect_clean_local_complete(
                fixture
                    .execute_local_correlated(second_id, &second)
                    .unwrap()
            ),
            second_id
        );
        assert!(first_id > second_id);
        let first_event =
            AcceptedBatchEvent::from_accepted(fixture.engine(), &fixture.archive, first_id)
                .unwrap();
        let second_event =
            AcceptedBatchEvent::from_accepted(fixture.engine(), &fixture.archive, second_id)
                .unwrap();
        assert!(first_event.acceptance_sequence() < second_event.acceptance_sequence());
        assert_eq!(
            fixture
                .engine()
                .accepted_batch_id_at(first_event.acceptance_sequence())
                .unwrap(),
            Some(first_id)
        );
        assert_eq!(
            fixture
                .engine()
                .accepted_batch_id_at(second_event.acceptance_sequence())
                .unwrap(),
            Some(second_id)
        );
        fixture.assert_clean_drained();
    }

    fn hex(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push(DIGITS[(byte >> 4) as usize] as char);
            encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
        }
        encoded
    }

    /// Byte-for-byte image of the immutable publication surface.
    ///
    /// Every object and batch manifest file is compared by exact bytes, so an
    /// extra object, a rewritten manifest, or leftover temporary residue under
    /// either directory is detected. The archive's top-level entry names are
    /// included so a stray sibling namespace is detected too. Derived
    /// namespaces the resume is expected to advance (durable engine history,
    /// the projection work index, run-local scratch) are deliberately not
    /// byte-compared.
    fn snapshot_immutable_publication(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut image = Vec::new();
        let mut names = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort_unstable();
        image.push((
            PathBuf::from("<archive-entry-names>"),
            format!("{names:?}").into_bytes(),
        ));
        for immutable in ["objects", "batches"] {
            let directory = root.join(immutable);
            assert!(
                directory.is_dir(),
                "{immutable} is not an archive directory"
            );
            image.extend(
                snapshot_tree(&directory)
                    .into_iter()
                    .map(|(path, bytes)| (PathBuf::from(immutable).join(path), bytes)),
            );
        }
        image
    }

    fn snapshot_tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(base: &Path, current: &Path, output: &mut Vec<(PathBuf, Vec<u8>)>) {
            let mut entries = fs::read_dir(current)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect::<Vec<_>>();
            entries.sort_unstable_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    walk(base, &path, output);
                } else {
                    output.push((
                        path.strip_prefix(base).unwrap().to_path_buf(),
                        fs::read(path).unwrap(),
                    ));
                }
            }
        }
        let mut output = Vec::new();
        walk(root, root, &mut output);
        output
    }
}
