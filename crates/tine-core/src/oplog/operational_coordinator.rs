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
use super::import::{plan_affected_import_with_bootstrap, plan_clean_affected_import};
use super::local_active::{
    CleanRuntimeSession, LocalRuntimeAdmission, PromotedRuntimeSession, RuntimePromotionError,
    RuntimeRevocation, WorkspaceAuthorityBoundary, WorkspaceAuthorityRefusal,
};
#[cfg(test)]
use super::plan_affected_import;
use super::shadow_projection::BootstrapProjectionAuthority;
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
        None,
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
            .capture_external_author_transaction(draft, graph, receipts, endpoint, None)
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

    /// Admit one immutable provider-delivered archive batch through the same
    /// promoted authority, accepted-history, SQLite, and graph-projection
    /// boundaries used by authored work.
    ///
    /// Provider transport can stage bytes, but it cannot authorize them. This
    /// method is the production bridge from exact retained bytes to the
    /// one-actor runtime. Its affine continuation retains the graph handoff
    /// across every post-manifest retry.
    pub(crate) fn ingest_archive_batch(
        session: &mut PromotedRuntimeSession<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        batch_id: BatchId,
    ) -> Result<ProviderArchiveIngress, OperationalCoordinatorError> {
        let (admission, engine, database, tail) = session.parts().map_err(|refusal| {
            OperationalCoordinatorError::revoked(OperationalPhase::Bindings, refusal)
        })?;
        authorize_coordinator(&admission, graph, engine)?;
        let endpoint = engine.projection_endpoint_binding().ok_or_else(|| {
            OperationalCoordinatorError::new(
                OperationalPhase::Bindings,
                "engine has no enrolled projection endpoint",
            )
        })?;
        let archive = verify_bindings(graph, receipts, engine, endpoint, None)?;
        let mut core = construct_archive_continuation(
            graph, engine, endpoint, archive, batch_id, None, false,
        )?;
        match core.resume(&admission, graph, receipts, engine, database, tail) {
            Ok(_) => {
                core.guard.complete();
                Ok(ProviderArchiveIngress::Complete)
            }
            Err(error) => {
                core.failure = error;
                Ok(ProviderArchiveIngress::Pending(
                    ProviderArchiveContinuation { core },
                ))
            }
        }
    }

    /// Reconstruct one lost process-local continuation from an exact durable
    /// application-move episode and its immutable local-mutation manifest.
    /// This never enumerates ready/orphan batches and never publishes bytes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resume_correlated_published_local(
        session: &mut PromotedRuntimeSession<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        batch_id: BatchId,
        manifest_fingerprint: ContentDigest,
        lineage_digest: LineageDigest,
        source_page_id: PageId,
        destination_page_id: PageId,
    ) -> Result<CorrelatedPublishedLocalResume, OperationalCoordinatorError> {
        let (admission, engine, database, tail) = session.parts().map_err(|refusal| {
            OperationalCoordinatorError::revoked(OperationalPhase::Bindings, refusal)
        })?;
        authorize_coordinator(&admission, graph, engine)?;
        let endpoint = engine.projection_endpoint_binding().ok_or_else(|| {
            OperationalCoordinatorError::new(
                OperationalPhase::Bindings,
                "engine has no enrolled projection endpoint",
            )
        })?;
        let archive = verify_bindings(graph, receipts, engine, endpoint, None)?;
        let inspection = archive.inspect_batch(batch_id).map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::Publication, error.to_string())
        })?;
        let validated = match inspection {
            BatchInspection::Absent | BatchInspection::Staged { .. } => {
                if durable_exact_batch_evidence(engine, database, batch_id)? {
                    return Err(OperationalCoordinatorError::retained_block(
                        OperationalPhase::Publication,
                        "correlated application-move archive is incomplete despite exact durable batch evidence",
                        RetainedBlockReason::PublishedAuthentication,
                    ));
                }
                return Ok(CorrelatedPublishedLocalResume::NoCommit);
            }
            BatchInspection::Ready(validated) => validated,
        };
        let manifest = validated.manifest();
        let manifest_bytes = manifest.encode().map_err(|error| {
            OperationalCoordinatorError::new(OperationalPhase::Publication, error.to_string())
        })?;
        if manifest.batch_id() != batch_id
            || manifest.workspace_id() != engine.workspace_id()
            || manifest.lineage_digest() != lineage_digest
            || manifest.origin() != BatchOrigin::LocalMutation
            || ContentDigest::of(&manifest_bytes) != manifest_fingerprint
        {
            return Err(OperationalCoordinatorError::retained_block(
                OperationalPhase::Publication,
                "correlated application-move manifest identity mismatch",
                RetainedBlockReason::PublishedAuthentication,
            ));
        }
        let projection = super::projection_manifest::validate_projection_object_set(
            manifest,
            validated.objects(),
        )
        .map_err(|error| {
            OperationalCoordinatorError::retained_block(
                OperationalPhase::Publication,
                error.to_string(),
                RetainedBlockReason::PublishedAuthentication,
            )
        })?;
        let mut affected = projection
            .intents()
            .iter()
            .filter(|intent| intent.source_endpoint_id() == endpoint.endpoint_id())
            .map(|intent| intent.page_id())
            .collect::<Vec<_>>();
        affected.sort_unstable();
        affected.dedup();
        let mut expected = vec![source_page_id, destination_page_id];
        expected.sort_unstable();
        expected.dedup();
        if affected != expected || expected.len() != 2 {
            return Err(OperationalCoordinatorError::retained_block(
                OperationalPhase::Publication,
                "correlated application-move affected pages differ from its episode",
                RetainedBlockReason::PublishedAuthentication,
            ));
        }
        drop(validated);
        let preexisting_blocked = correlated_blocked_work(engine, batch_id)?;
        if !preexisting_blocked.is_empty() {
            engine
                .register_correlated_blocked_external_reconciliation(
                    batch_id,
                    manifest_fingerprint,
                    [source_page_id, destination_page_id],
                )
                .map_err(|error| {
                    OperationalCoordinatorError::retained_block(
                        OperationalPhase::ProjectionDrain,
                        error.to_string(),
                        RetainedBlockReason::PublishedAuthentication,
                    )
                })?;
            graph
                .reconstruct_and_consume_recovered_published_handoff(endpoint)
                .map_err(|error| {
                    OperationalCoordinatorError::retained_block(
                        OperationalPhase::Bindings,
                        error.to_string(),
                        RetainedBlockReason::StableBinding,
                    )
                })?;
            let mut paths = preexisting_blocked
                .into_iter()
                .map(|work| work.path().clone())
                .collect::<Vec<_>>();
            paths.sort();
            paths.dedup();
            return Ok(CorrelatedPublishedLocalResume::GuardedConflict(
                CorrelatedGuardedProjectionConflict { batch_id, paths },
            ));
        }
        let mut core = construct_archive_continuation(
            graph,
            engine,
            endpoint,
            archive,
            batch_id,
            Some((BatchOrigin::LocalMutation, manifest_fingerprint)),
            true,
        )?;
        let state = match core.resume(&admission, graph, receipts, engine, database, tail) {
            Ok(batch_id) => {
                core.guard.complete();
                LocalMutationCoordinatorState::Active(LocalMutationCompletion { batch_id })
            }
            Err(error)
                if error.retained_block_reason()
                    == Some(&RetainedBlockReason::GuardedProjectionConflict) =>
            {
                if let Err(registration) = engine
                    .register_correlated_blocked_external_reconciliation(
                        batch_id,
                        manifest_fingerprint,
                        [source_page_id, destination_page_id],
                    )
                {
                    core.failure = OperationalCoordinatorError::retained_block(
                        OperationalPhase::ProjectionDrain,
                        registration.to_string(),
                        RetainedBlockReason::PublishedAuthentication,
                    );
                    return Ok(CorrelatedPublishedLocalResume::State(
                        LocalMutationCoordinatorState::from_failed(LocalPublishedContinuation {
                            core,
                        }),
                    ));
                }
                let blocked = match correlated_blocked_work(engine, batch_id) {
                    Ok(blocked) => blocked,
                    Err(blocked_error) => {
                        core.failure = blocked_error;
                        return Ok(CorrelatedPublishedLocalResume::State(
                            LocalMutationCoordinatorState::from_failed(
                                LocalPublishedContinuation { core },
                            ),
                        ));
                    }
                };
                let mut paths = blocked
                    .into_iter()
                    .map(|work| work.path().clone())
                    .collect::<Vec<_>>();
                paths.sort();
                paths.dedup();
                // The exact feed cannot acquire graph-text write admission
                // until this reconstructed local latch is consumed.
                core.guard.complete();
                return Ok(CorrelatedPublishedLocalResume::GuardedConflict(
                    CorrelatedGuardedProjectionConflict { batch_id, paths },
                ));
            }
            Err(error) => {
                core.failure = error;
                LocalMutationCoordinatorState::from_failed(LocalPublishedContinuation { core })
            }
        };
        Ok(CorrelatedPublishedLocalResume::State(state))
    }

    /// Resume the exact process-local continuation before considering cold
    /// reconstruction. Its published latch is affine and intentionally does
    /// not release on `Drop`, so the actor must never discard this value merely
    /// because the durable episode is also available.
    pub(crate) fn resume_correlated_live_local(
        session: &mut PromotedRuntimeSession<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        continuation: LocalPublishedContinuation,
        batch_id: BatchId,
        manifest_fingerprint: ContentDigest,
        lineage_digest: LineageDigest,
        source_page_id: PageId,
        destination_page_id: PageId,
    ) -> Result<CorrelatedPublishedLocalResume, OperationalCoordinatorError> {
        let mut core = continuation.core;
        let (admission, engine, database, tail) = match session.parts() {
            Ok(parts) => parts,
            Err(refusal) => {
                core.failure =
                    OperationalCoordinatorError::revoked(OperationalPhase::Bindings, refusal);
                return Ok(CorrelatedPublishedLocalResume::State(
                    LocalMutationCoordinatorState::from_failed(LocalPublishedContinuation { core }),
                ));
            }
        };
        if let Err(error) = authorize_coordinator(&admission, graph, engine) {
            core.failure = error;
            return Ok(CorrelatedPublishedLocalResume::State(
                LocalMutationCoordinatorState::from_failed(LocalPublishedContinuation { core }),
            ));
        }
        let Some(endpoint) = engine.projection_endpoint_binding() else {
            core.failure = OperationalCoordinatorError::new(
                OperationalPhase::Bindings,
                "engine has no enrolled projection endpoint",
            );
            return Ok(CorrelatedPublishedLocalResume::State(
                LocalMutationCoordinatorState::from_failed(LocalPublishedContinuation { core }),
            ));
        };
        if core.batch_id != batch_id
            || core.origin != BatchOrigin::LocalMutation
            || core.manifest_digest != manifest_fingerprint
            || core.endpoint != endpoint
            || engine.lineage_digest() != lineage_digest
        {
            core.failure = OperationalCoordinatorError::retained_block(
                OperationalPhase::Publication,
                "live correlated application-move continuation identity mismatch",
                RetainedBlockReason::PublishedAuthentication,
            );
            return Ok(CorrelatedPublishedLocalResume::State(
                LocalMutationCoordinatorState::from_failed(LocalPublishedContinuation { core }),
            ));
        }
        let state = match core.resume(&admission, graph, receipts, engine, database, tail) {
            Ok(batch_id) => {
                core.guard.complete();
                LocalMutationCoordinatorState::Active(LocalMutationCompletion { batch_id })
            }
            Err(error)
                if error.retained_block_reason()
                    == Some(&RetainedBlockReason::GuardedProjectionConflict) =>
            {
                if let Err(registration) = engine
                    .register_correlated_blocked_external_reconciliation(
                        batch_id,
                        manifest_fingerprint,
                        [source_page_id, destination_page_id],
                    )
                {
                    core.failure = OperationalCoordinatorError::retained_block(
                        OperationalPhase::ProjectionDrain,
                        registration.to_string(),
                        RetainedBlockReason::PublishedAuthentication,
                    );
                    return Ok(CorrelatedPublishedLocalResume::State(
                        LocalMutationCoordinatorState::from_failed(LocalPublishedContinuation {
                            core,
                        }),
                    ));
                }
                let blocked = match correlated_blocked_work(engine, batch_id) {
                    Ok(blocked) => blocked,
                    Err(blocked_error) => {
                        core.failure = blocked_error;
                        return Ok(CorrelatedPublishedLocalResume::State(
                            LocalMutationCoordinatorState::from_failed(
                                LocalPublishedContinuation { core },
                            ),
                        ));
                    }
                };
                let mut paths = blocked
                    .into_iter()
                    .map(|work| work.path().clone())
                    .collect::<Vec<_>>();
                paths.sort();
                paths.dedup();
                core.guard.complete();
                return Ok(CorrelatedPublishedLocalResume::GuardedConflict(
                    CorrelatedGuardedProjectionConflict { batch_id, paths },
                ));
            }
            Err(error) => {
                core.failure = error;
                LocalMutationCoordinatorState::from_failed(LocalPublishedContinuation { core })
            }
        };
        Ok(CorrelatedPublishedLocalResume::State(state))
    }

    pub(crate) fn retry_archive_batch(
        session: &mut PromotedRuntimeSession<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        mut continuation: ProviderArchiveContinuation,
    ) -> ProviderArchiveIngress {
        let (admission, engine, database, tail) = match session.parts() {
            Ok(parts) => parts,
            Err(refusal) => {
                continuation.core.failure =
                    OperationalCoordinatorError::revoked(OperationalPhase::Bindings, refusal);
                return ProviderArchiveIngress::Pending(continuation);
            }
        };
        match continuation
            .core
            .resume(&admission, graph, receipts, engine, database, tail)
        {
            Ok(_) => {
                continuation.core.guard.complete();
                ProviderArchiveIngress::Complete
            }
            Err(error) => {
                continuation.core.failure = error;
                ProviderArchiveIngress::Pending(continuation)
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
        Self::execute_with_bootstrap(
            admission,
            graph,
            receipts,
            engine,
            database,
            tail,
            None,
            requested_paths,
        )
    }

    pub(crate) fn execute_with_bootstrap(
        admission: &LocalRuntimeAdmission<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        engine: &mut ShardedHotEngine,
        database: &mut SqliteFrontier,
        tail: &mut TailOverlay,
        bootstrap: Option<&BootstrapProjectionAuthority>,
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

        let plan = plan_affected_import_with_bootstrap(
            graph,
            receipts,
            engine,
            bootstrap,
            requested_paths,
        );
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
            .capture_external_author_transaction(draft, graph, receipts, endpoint, bootstrap)
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

    /// Authorize, draft, capture, and finalize one local transaction without
    /// entering the synchronous archive/SQLite pipeline. The returned sealed
    /// value is the only input accepted by the trusted-local commit boundary.
    pub(crate) fn prepare_trusted_local(
        session: &mut PromotedRuntimeSession<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        transaction: &OperationTransaction,
        prepared_editor_projection: Option<super::projection::PreparedEditorProjection>,
    ) -> Result<PreparedLocalMutationState, OperationalCoordinatorError> {
        #[cfg(test)]
        {
            reset_trusted_local_preparation_stage_timings();
            super::hot_engine::reset_local_mutation_detail_timings();
        }
        #[cfg(test)]
        let parts_started = Instant::now();
        let (admission, engine, database, _tail, bootstrap) =
            session.parts_with_bootstrap().map_err(|refusal| {
                OperationalCoordinatorError::revoked(OperationalPhase::Bindings, refusal)
            })?;
        #[cfg(test)]
        note_trusted_local_preparation_stage(|timings| {
            timings.session_parts = parts_started.elapsed();
        });
        let prepared = prepare_local_inner(
            &admission,
            graph,
            receipts,
            engine,
            Some(bootstrap),
            LocalDraftSource::Promoted { batch_id: None },
            LocalPreparationBinding::TrustedLocal,
            transaction,
            prepared_editor_projection,
            None,
        )?;
        match prepared {
            PreparedLocalMutationState::Prepared(mut prepared) => {
                prepared.preflight_identity(database, engine)?;
                Ok(PreparedLocalMutationState::Prepared(prepared))
            }
            reconciliation => Ok(reconciliation),
        }
    }

    /// Execute one already-translated semantic local mutation under the
    /// currently admitted `LocalActive` runtime.
    ///
    /// The caller supplies semantic operations only. The nonconstructible
    /// promoted runtime/session mints batch, device, session, and CRDT-peer
    /// identity inside this boundary.
    pub(crate) fn execute_local(
        session: &mut PromotedRuntimeSession<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        transaction: &OperationTransaction,
    ) -> LocalMutationCoordinatorState {
        let (admission, engine, database, tail, bootstrap) = match session.parts_with_bootstrap() {
            Ok(parts) => parts,
            Err(refusal) => {
                return LocalMutationCoordinatorState::blocked(
                    OperationalCoordinatorError::revoked(OperationalPhase::Bindings, refusal),
                );
            }
        };
        match execute_local_inner(
            &admission,
            graph,
            receipts,
            engine,
            database,
            tail,
            Some(bootstrap),
            LocalDraftSource::Promoted { batch_id: None },
            transaction,
        ) {
            Ok(state) => state,
            Err(error) => LocalMutationCoordinatorState::blocked(error),
        }
    }

    /// Execute one semantic transaction with a stable batch identity derived
    /// inside an actor-owned application protocol. Ordinary local mutations
    /// continue to mint random batch IDs through [`Self::execute_local`].
    pub(crate) fn execute_local_correlated(
        session: &mut PromotedRuntimeSession<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        batch_id: BatchId,
        transaction: &OperationTransaction,
        persist_fingerprint: impl FnOnce(ContentDigest) -> Result<(), String>,
    ) -> LocalMutationCoordinatorState {
        let (admission, engine, database, tail, bootstrap) = match session.parts_with_bootstrap() {
            Ok(parts) => parts,
            Err(refusal) => {
                return LocalMutationCoordinatorState::blocked(
                    OperationalCoordinatorError::revoked(OperationalPhase::Bindings, refusal),
                );
            }
        };
        let mut prepared = match prepare_local_inner(
            &admission,
            graph,
            receipts,
            engine,
            Some(bootstrap),
            LocalDraftSource::Promoted {
                batch_id: Some(batch_id),
            },
            LocalPreparationBinding::SlowPipeline,
            transaction,
            None,
            None,
        ) {
            Ok(PreparedLocalMutationState::Prepared(prepared)) => prepared,
            Ok(PreparedLocalMutationState::ReconciliationRequired(reconciliation)) => {
                return LocalMutationCoordinatorState::Recovering(
                    LocalMutationRecovery::ReconciliationRequired(reconciliation),
                );
            }
            Err(error) => return LocalMutationCoordinatorState::blocked(error),
        };
        if let Err(error) = prepared.preflight_identity(database, engine) {
            return LocalMutationCoordinatorState::blocked(error);
        }
        let manifest_bytes = match prepared.prepared.manifest().encode() {
            Ok(bytes) => bytes,
            Err(error) => {
                return LocalMutationCoordinatorState::blocked(OperationalCoordinatorError::new(
                    OperationalPhase::Finalize,
                    error.to_string(),
                ));
            }
        };
        if let Err(error) = persist_fingerprint(ContentDigest::of(&manifest_bytes)) {
            return LocalMutationCoordinatorState::blocked(OperationalCoordinatorError::new(
                OperationalPhase::Publication,
                error,
            ));
        }
        match publish_prepared_local(
            &admission, graph, receipts, engine, database, tail, prepared,
        ) {
            Ok(state) => state,
            Err(error) => LocalMutationCoordinatorState::blocked(error),
        }
    }

    /// Resume one exact published local mutation through the same promoted
    /// session boundary as initial execution.
    ///
    /// Keeping this split here prevents actor facades from acquiring direct
    /// access to the engine, SQLite applier, tail, or runtime admission.
    pub(crate) fn retry_local(
        session: &mut PromotedRuntimeSession<'_>,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        continuation: LocalPublishedContinuation,
    ) -> LocalMutationCoordinatorState {
        let (admission, engine, database, tail) = match session.parts() {
            Ok(parts) => parts,
            Err(refusal) => {
                let mut continuation = continuation;
                continuation.core.failure =
                    OperationalCoordinatorError::revoked(OperationalPhase::Bindings, refusal);
                return LocalMutationCoordinatorState::from_failed(continuation);
            }
        };
        continuation.retry(&admission, graph, receipts, engine, database, tail)
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
            None,
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
            None,
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
    bootstrap: Option<&BootstrapProjectionAuthority>,
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
        .capture_local_author_transaction(draft, graph, receipts, endpoint, bootstrap)
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
    bootstrap: Option<&BootstrapProjectionAuthority>,
    source: LocalDraftSource,
    transaction: &OperationTransaction,
) -> Result<LocalMutationCoordinatorState, OperationalCoordinatorError> {
    let mut prepared = match prepare_local_inner(
        admission,
        graph,
        receipts,
        engine,
        bootstrap,
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
    use crate::oplog::object_store::{
        fail_next_engine_history_head_swap, fail_next_publish_after_objects,
    };
    use crate::oplog::projection::fail_next_formatting_adoption_after_intent_for_harness;
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

    struct Fixture {
        _root: TestRoot,
        graph_root: PathBuf,
        archive_root: PathBuf,
        graph: Graph,
        receipts: ProjectionReceiptStore,
        archive: ObjectStore,
        engine: ShardedHotEngine,
        database: SqliteFrontier,
        tail: TailOverlay,
        lineage: LineageDigest,
        catalog: DocumentId,
        home_document_id: DocumentId,
        block_id: BlockId,
        intent: super::super::ProjectionIntent,
        path: String,
    }

    impl Fixture {
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
            Self::new_at_named(
                label,
                "pages/Coordinator Page.md",
                None,
                ManagedTextKind::Page,
                "Coordinator Page",
                true,
            )
        }

        fn new_at(label: &str, path: &str, config: Option<&str>, kind: ManagedTextKind) -> Self {
            Self::new_at_named(label, path, config, kind, "Coordinator Page", false)
        }

        fn new_at_named(
            label: &str,
            path: &str,
            config: Option<&str>,
            kind: ManagedTextKind,
            logical_name: &str,
            imported_orders: bool,
        ) -> Self {
            let root = TestRoot::new(label);
            let graph_root = root.path().join("graph");
            fs::create_dir_all(&graph_root).unwrap();
            if let Some(config) = config {
                fs::create_dir_all(graph_root.join("logseq")).unwrap();
                fs::write(graph_root.join("logseq/config.edn"), config).unwrap();
            }
            let graph = Graph::open(&graph_root);
            let workspace_id = super::super::WorkspaceId::from_uuid(Uuid::from_u128(1));
            let endpoint = ProjectionEndpointBinding::enroll_graph(
                &graph,
                ProjectionEndpointId::from_uuid(Uuid::from_u128(2)),
                DeviceId::from_uuid(Uuid::from_u128(3)),
            )
            .unwrap();
            let receipts = ProjectionReceiptStore::open_for_endpoint(
                &root.path().join("receipts"),
                workspace_id,
                endpoint,
            )
            .unwrap();
            let lineage = LineageDigest::of(label.as_bytes());
            let catalog = DocumentId::from_uuid(Uuid::from_u128(4));
            let page_id = PageId::from_uuid(Uuid::from_u128(5));
            let home = DocumentId::from_uuid(Uuid::from_u128(6));
            let block = BlockId::from_uuid(Uuid::from_u128(7));
            let managed_path = ManagedPath::parse(path).unwrap();
            let fixture_order = if imported_orders {
                super::super::import::imported_order(0)
            } else {
                "a".into()
            };
            let transaction = OperationTransaction::new(vec![
                SemanticOperation::CreatePage {
                    page_id,
                    home_document_id: home,
                    name: LogicalPageName::parse(logical_name).unwrap(),
                    path: managed_path,
                    kind,
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id: block,
                        home_document_id: home,
                    },
                    page_id,
                    parent: None,
                    order: fixture_order.clone(),
                    content: "root".into(),
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id: BlockId::from_uuid(Uuid::from_u128(8)),
                        home_document_id: home,
                    },
                    page_id,
                    parent: Some(block),
                    order: fixture_order,
                    content: "child".into(),
                },
            ])
            .unwrap();
            let author = ShardedHotEngine::new(workspace_id, lineage, catalog);
            let bootstrap = author
                .prepare_bootstrap_transaction(
                    AuthorBatch {
                        batch_id: BatchId::from_uuid(Uuid::from_u128(9)),
                        author_device_id: DeviceId::from_uuid(Uuid::from_u128(10)),
                        author_session_id: SessionId::from_uuid(Uuid::from_u128(11)),
                        crdt_peer_id: CrdtPeerId::from_u64(12),
                    },
                    &transaction,
                )
                .unwrap();
            let archive_root = root.path().join("archive");
            ObjectStore::open(&archive_root, workspace_id)
                .unwrap()
                .publish_bootstrap_prepared_for_test(&bootstrap)
                .unwrap();
            let mut engine = ShardedHotEngine::with_enrolled_projection(
                ObjectStore::open(&archive_root, workspace_id).unwrap(),
                lineage,
                catalog,
                &graph,
                &receipts,
            );
            engine
                .stage_archive_batch(bootstrap.manifest().batch_id())
                .unwrap();
            let intent = write_projection_exact(&graph, &receipts, &engine, page_id, None)
                .unwrap()
                .plan
                .intent()
                .clone();
            let archive = ObjectStore::open(&archive_root, workspace_id).unwrap();
            let runtime =
                ApplicationRuntimeRoot::open_for_test(&root.path().join("runtime")).unwrap();
            let database_path = root.path().join("sqlite/materialized.sqlite3");
            let source = RebuildSource::new(&engine, &archive).unwrap();
            let database = SqliteFrontier::open_or_rebuild(
                &database_path,
                &runtime,
                ProjectionClaim::current(workspace_id, lineage),
                source,
            )
            .unwrap()
            .database;
            let source = RebuildSource::new(&engine, &archive).unwrap();
            let tail = TailOverlay::from_durable(&database, &source).unwrap();
            assert_eq!(
                database.frontier_root().unwrap(),
                engine.accepted_frontier_root().unwrap()
            );
            Self {
                _root: root,
                graph_root,
                archive_root,
                graph,
                receipts,
                archive,
                engine,
                database,
                tail,
                lineage,
                catalog,
                home_document_id: home,
                block_id: block,
                intent,
                path: path.into(),
            }
        }

        fn overwrite(&self, bytes: &[u8]) {
            fs::write(self.graph_root.join(&self.path), bytes).unwrap();
        }

        fn execute(&mut self, paths: &[&str]) -> OperationalCoordinatorState {
            OperationalCoordinator::execute(
                &LocalRuntimeAdmission::unenrolled_pre_activation(),
                &self.graph,
                &self.receipts,
                &mut self.engine,
                &mut self.database,
                &mut self.tail,
                paths,
            )
            .unwrap()
        }

        fn local_author(&self, seed: u128) -> AuthorBatch {
            AuthorBatch {
                batch_id: BatchId::from_uuid(Uuid::from_u128(seed)),
                author_device_id: self
                    .engine
                    .projection_endpoint_binding()
                    .unwrap()
                    .device_id(),
                author_session_id: SessionId::from_uuid(Uuid::from_u128(seed + 1)),
                crdt_peer_id: CrdtPeerId::from_u64((seed as u64).saturating_add(10_001)),
            }
        }

        fn execute_local(
            &mut self,
            author: AuthorBatch,
            transaction: &OperationTransaction,
        ) -> LocalMutationCoordinatorState {
            OperationalCoordinator::execute_local_with_author(
                &LocalRuntimeAdmission::unenrolled_pre_activation(),
                &self.graph,
                &self.receipts,
                &mut self.engine,
                &mut self.database,
                &mut self.tail,
                author,
                transaction,
            )
        }

        fn local_edit(&mut self, seed: u128, content: &str) -> LocalMutationCoordinatorState {
            let transaction =
                OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: self.block_id,
                        home_document_id: self.home_document_id,
                    },
                    content: content.into(),
                }])
                .unwrap();
            let author = self.local_author(seed);
            self.execute_local(author, &transaction)
        }

        fn assert_drained(&self) {
            assert_eq!(
                self.database.frontier_root().unwrap(),
                self.engine.accepted_frontier_root().unwrap()
            );
            assert!(self
                .engine
                .projection_work_index()
                .unwrap()
                .ready_page(None, 1)
                .unwrap()
                .work()
                .is_empty());
            assert_eq!(self.tail.status().unapplied_batches, 0);
            assert_eq!(self.tail.status().retained_bytes, 0);
        }

        fn restart_projection_runtime(self) -> Self {
            let Self {
                _root,
                graph_root,
                archive_root,
                graph,
                receipts,
                archive,
                engine,
                database,
                tail,
                lineage,
                catalog,
                home_document_id,
                block_id,
                intent,
                path,
            } = self;
            let endpoint = receipts.endpoint_binding().unwrap();
            let receipt_root = receipts.root_path().to_path_buf();
            let workspace = engine.workspace_id();
            drop(tail);
            drop(database);
            drop(engine);
            drop(archive);
            drop(receipts);
            drop(graph);

            let graph = Graph::open(&graph_root);
            let receipts =
                ProjectionReceiptStore::open_for_endpoint(&receipt_root, workspace, endpoint)
                    .unwrap();
            let archive = ObjectStore::open(&archive_root, workspace).unwrap();
            let manifests = archive.committed_manifests().unwrap();
            let engine = ShardedHotEngine::open_enrolled_projection_resuming(
                ObjectStore::open(&archive_root, workspace).unwrap(),
                lineage,
                catalog,
                &graph,
                &receipts,
                None,
                &manifests,
                None,
            )
            .unwrap()
            .0;
            let runtime =
                ApplicationRuntimeRoot::open_for_test(&_root.path().join("runtime")).unwrap();
            let database_path = _root.path().join("sqlite/materialized.sqlite3");
            let source = RebuildSource::new(&engine, &archive).unwrap();
            let database = SqliteFrontier::open_or_rebuild(
                &database_path,
                &runtime,
                ProjectionClaim::current(workspace, lineage),
                source,
            )
            .unwrap()
            .database;
            let source = RebuildSource::new(&engine, &archive).unwrap();
            let tail = TailOverlay::from_durable(&database, &source).unwrap();
            Self {
                _root,
                graph_root,
                archive_root,
                graph,
                receipts,
                archive,
                engine,
                database,
                tail,
                lineage,
                catalog,
                home_document_id,
                block_id,
                intent,
                path,
            }
        }
    }

    fn expect_complete(state: OperationalCoordinatorState) -> OperationalCompletion {
        match state {
            OperationalCoordinatorState::Complete(completion) => completion,
            OperationalCoordinatorState::Blocked(plan) => {
                panic!("unexpected blocked plan: {:?}", plan.blocks())
            }
            OperationalCoordinatorState::Noop => panic!("unexpected no-op"),
            OperationalCoordinatorState::FailedClosed(failed) => {
                panic!("unexpected failed-closed state: {}", failed.failure())
            }
        }
    }

    fn expect_failed(state: OperationalCoordinatorState) -> ExternalPublishedContinuation {
        match state {
            OperationalCoordinatorState::FailedClosed(failed) => failed,
            OperationalCoordinatorState::Blocked(plan) => {
                panic!("unexpected blocked plan: {:?}", plan.blocks())
            }
            OperationalCoordinatorState::Noop => panic!("unexpected no-op"),
            OperationalCoordinatorState::Complete(_) => panic!("unexpected completion"),
        }
    }

    fn expect_local_active(state: LocalMutationCoordinatorState) -> LocalMutationCompletion {
        match state {
            LocalMutationCoordinatorState::Active(completion) => completion,
            LocalMutationCoordinatorState::Recovering(recovery) => match recovery {
                LocalMutationRecovery::ReconciliationRequired(reconciliation) => panic!(
                    "unexpected local reconciliation: {:?}",
                    reconciliation.paths()
                ),
                LocalMutationRecovery::Published(continuation) => {
                    panic!("unexpected local continuation: {}", continuation.failure())
                }
            },
            LocalMutationCoordinatorState::Blocked(blocked) => {
                panic!("unexpected blocked local mutation: {}", blocked.failure())
            }
            LocalMutationCoordinatorState::Revoked(revoked) => {
                panic!("unexpected revoked local mutation: {}", revoked.failure())
            }
        }
    }

    /// Settle a local mutation whose publication legitimately needs more than
    /// one bounded turn.
    fn settle_local(
        fixture: &mut Fixture,
        mut state: LocalMutationCoordinatorState,
    ) -> LocalMutationCompletion {
        for _ in 0..8 {
            match state {
                LocalMutationCoordinatorState::Active(completion) => return completion,
                LocalMutationCoordinatorState::Recovering(LocalMutationRecovery::Published(
                    continuation,
                )) => {
                    state = continuation.retry(
                        &LocalRuntimeAdmission::unenrolled_pre_activation(),
                        &fixture.graph,
                        &fixture.receipts,
                        &mut fixture.engine,
                        &mut fixture.database,
                        &mut fixture.tail,
                    );
                }
                other => return expect_local_active(other),
            }
        }
        panic!("local mutation did not settle within the bounded turn budget")
    }

    fn expect_local_published_recovery(
        state: LocalMutationCoordinatorState,
    ) -> LocalPublishedContinuation {
        match state {
            LocalMutationCoordinatorState::Recovering(LocalMutationRecovery::Published(
                continuation,
            )) => continuation,
            LocalMutationCoordinatorState::Recovering(
                LocalMutationRecovery::ReconciliationRequired(reconciliation),
            ) => panic!(
                "unexpected local reconciliation: {:?}",
                reconciliation.paths()
            ),
            LocalMutationCoordinatorState::Active(_) => {
                panic!("unexpected completed local mutation")
            }
            LocalMutationCoordinatorState::Blocked(blocked) => {
                panic!("unexpected blocked local mutation: {}", blocked.failure())
            }
            LocalMutationCoordinatorState::Revoked(revoked) => {
                panic!("unexpected revoked local mutation: {}", revoked.failure())
            }
        }
    }

    #[test]
    fn fresh_nested_layout_reconcile_drains_history_sqlite_and_projection() {
        let mut fixture = Fixture::configured("nested-success");
        let path = fixture.path.clone();
        fixture.overwrite(b"- root edited\n\t- child edited\n");
        let completion = expect_complete(fixture.execute(&[&path]));
        assert_eq!(completion.batch_id(), completion.import_id().batch_id());
        fixture.assert_drained();
        assert_eq!(
            fs::read(fixture.graph_root.join(path)).unwrap(),
            b"- root edited\n\t- child edited\n"
        );
    }

    #[test]
    fn admitted_local_semantic_mutation_commits_history_sqlite_and_projection_once() {
        let mut fixture = Fixture::configured("local-success");
        let path = fixture.path.clone();
        let manifests_before = fixture.archive.committed_manifests().unwrap().len();
        let accepted_before = fixture.engine.accepted_batch_count().unwrap();
        let sqlite_before = fixture.database.applied_batch_count().unwrap();
        let releases_before = fixture.graph.handoff_release_count();
        reset_projection_graph_test_counters();

        let completion = expect_local_active(fixture.local_edit(40_000, "local semantic edit"));

        assert_eq!(
            fixture.archive.committed_manifests().unwrap().len(),
            manifests_before + 1
        );
        assert_eq!(
            fixture.engine.accepted_batch_count().unwrap(),
            accepted_before + 1
        );
        assert_eq!(
            fixture.database.applied_batch_count().unwrap(),
            sqlite_before + 1,
            "the local accepted event must be applied to SQLite exactly once"
        );
        assert!(fixture
            .database
            .contains_batch(completion.batch_id())
            .unwrap());
        let batch = match fixture
            .archive
            .inspect_batch(completion.batch_id())
            .unwrap()
        {
            BatchInspection::Ready(batch) => batch,
            other => panic!("local mutation did not reach authenticated history: {other:?}"),
        };
        assert_eq!(batch.manifest().origin(), BatchOrigin::LocalMutation);
        assert_eq!(
            fixture.database.frontier_root().unwrap(),
            fixture.engine.accepted_frontier_root().unwrap()
        );
        assert_eq!(projection_graph_test_counters().write_calls, 1);
        assert_eq!(
            fixture.graph.handoff_release_count(),
            releases_before + 1,
            "successful completion releases the handoff exactly once"
        );
        assert_eq!(
            fs::read(fixture.graph_root.join(path)).unwrap(),
            b"- local semantic edit\n\t- child\n"
        );
        fixture.assert_drained();
    }

    #[test]
    fn local_exact_path_drift_requests_reconciliation_without_publication() {
        let mut fixture = Fixture::new("local-reconcile-first");
        let path = fixture.path.clone();
        fixture.overwrite(b"- externally moved local base\n\t- child\n");
        let immutable_before = snapshot_immutable_publication(&fixture.archive_root);
        let frontier_before = fixture.engine.accepted_frontier_root().unwrap();
        let sqlite_before = fixture.database.frontier_root().unwrap();
        reset_projection_graph_test_counters();

        let state = fixture.local_edit(40_100, "must not overwrite external bytes");
        let LocalMutationCoordinatorState::Recovering(
            LocalMutationRecovery::ReconciliationRequired(reconciliation),
        ) = state
        else {
            panic!("exact local path drift must request reconciliation");
        };
        assert_eq!(
            reconciliation.paths(),
            &[ManagedPath::parse(&path).unwrap()]
        );
        assert_eq!(
            snapshot_immutable_publication(&fixture.archive_root),
            immutable_before
        );
        assert_eq!(
            fixture.engine.accepted_frontier_root().unwrap(),
            frontier_before
        );
        assert_eq!(fixture.database.frontier_root().unwrap(), sqlite_before);
        assert_eq!(projection_graph_test_counters().write_calls, 0);
        assert_eq!(
            fs::read(fixture.graph_root.join(path)).unwrap(),
            b"- externally moved local base\n\t- child\n"
        );
        fixture.graph.probe_managed_text_writer().unwrap();
    }

    #[test]
    fn stale_local_binding_is_typed_blocked_before_any_writer_side_effect() {
        let mut fixture = Fixture::new("local-stale-binding");
        let foreign_root = TestRoot::new("local-stale-binding-foreign");
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
            fixture.engine.workspace_id(),
            foreign_endpoint,
        )
        .unwrap();
        let immutable_before = snapshot_immutable_publication(&fixture.archive_root);
        let frontier_before = fixture.engine.accepted_frontier_root().unwrap();
        let sqlite_before = fixture.database.frontier_root().unwrap();
        let transaction = OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
            block: BlockLocation {
                block_id: fixture.block_id,
                home_document_id: fixture.home_document_id,
            },
            content: "blocked stale binding".into(),
        }])
        .unwrap();
        let author = fixture.local_author(40_200);

        let state = OperationalCoordinator::execute_local_with_author(
            &LocalRuntimeAdmission::unenrolled_pre_activation(),
            &fixture.graph,
            &foreign_receipts,
            &mut fixture.engine,
            &mut fixture.database,
            &mut fixture.tail,
            author,
            &transaction,
        );
        let LocalMutationCoordinatorState::Blocked(blocked) = state else {
            panic!("a stale local runtime binding must return Blocked");
        };
        assert_eq!(blocked.failure().phase(), OperationalPhase::Bindings);
        assert_eq!(
            snapshot_immutable_publication(&fixture.archive_root),
            immutable_before
        );
        assert_eq!(
            fixture.engine.accepted_frontier_root().unwrap(),
            frontier_before
        );
        assert_eq!(fixture.database.frontier_root().unwrap(), sqlite_before);
        fixture.graph.probe_managed_text_writer().unwrap();
    }

    #[test]
    fn local_and_external_mutations_enter_the_identical_terminal_pipeline() {
        reset_terminal_pipeline_origins();
        let mut local = Fixture::new("shared-terminal-local");
        expect_local_active(local.local_edit(41_000, "shared terminal local"));

        let mut external = Fixture::new("shared-terminal-external");
        let path = external.path.clone();
        external.overwrite(b"- shared terminal external\n\t- child\n");
        expect_complete(external.execute(&[&path]));

        let origins = terminal_pipeline_origins();
        assert_eq!(origins.len(), 2);
        assert_eq!(origins[0], BatchOrigin::LocalMutation);
        assert!(matches!(
            origins[1],
            BatchOrigin::ExternalReconciliation { .. }
        ));
    }

    #[test]
    fn local_late_failure_retries_exact_publication_without_a_second_writer() {
        for (index, point) in [
            OperationalFaultPoint::AfterManifest,
            OperationalFaultPoint::AfterStage,
            OperationalFaultPoint::AfterTailAdmission,
            OperationalFaultPoint::AfterSqliteApply,
            OperationalFaultPoint::BeforeProjection,
            OperationalFaultPoint::AfterProjection,
        ]
        .into_iter()
        .enumerate()
        {
            let mut fixture = Fixture::new(&format!("local-late-{point:?}"));
            let path = fixture.path.clone();
            let manifests_before = fixture.archive.committed_manifests().unwrap().len();
            reset_projection_graph_test_counters();
            fail_once_at(point);
            let failed = expect_local_published_recovery(
                fixture.local_edit(42_000 + index as u128 * 10, "late local edit"),
            );
            let batch_id = failed.batch_id();
            assert_eq!(
                fixture.archive.committed_manifests().unwrap().len(),
                manifests_before + 1
            );
            assert!(fixture.graph.probe_managed_text_writer().is_err());

            let completion = expect_local_active(failed.retry(
                &LocalRuntimeAdmission::unenrolled_pre_activation(),
                &fixture.graph,
                &fixture.receipts,
                &mut fixture.engine,
                &mut fixture.database,
                &mut fixture.tail,
            ));
            assert_eq!(completion.batch_id(), batch_id);
            assert_eq!(
                fixture.archive.committed_manifests().unwrap().len(),
                manifests_before + 1,
                "late retry republished the local mutation"
            );
            assert!(projection_graph_test_counters().write_calls <= 1);
            assert_eq!(
                fs::read(fixture.graph_root.join(&path)).unwrap(),
                b"- late local edit\n\t- child\n"
            );
            fixture.graph.probe_managed_text_writer().unwrap();
            fixture.assert_drained();
        }
    }

    #[test]
    fn local_semantic_paths_accept_nested_nonstandard_utf8_markdown_and_org() {
        for (index, (path, kind, expected)) in [
            (
                "content/pages/研究/über topic.md",
                ManagedTextKind::Page,
                b"- utf local edit\n\t- child\n".as_slice(),
            ),
            (
                "content/pages/研究/über topic.org",
                ManagedTextKind::Page,
                b"* utf local edit\n** child\n".as_slice(),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let mut fixture = Fixture::new_at(
                &format!("local-utf-{index}"),
                path,
                Some(
                    "{:pages-directory \"content/pages\"\n\
                      :journals-directory \"content/journals\"}\n",
                ),
                kind,
            );
            expect_local_active(fixture.local_edit(43_000 + index as u128 * 10, "utf local edit"));
            assert_eq!(fs::read(fixture.graph_root.join(path)).unwrap(), expected);
            fixture.assert_drained();
        }
    }

    /// The draft derivation that reads the unchanged catalog in place must
    /// publish exactly what the previous whole-copy derivation published, in
    /// both managed source languages, including when publication only settles
    /// on the durable retry.
    #[test]
    fn in_place_catalog_derivation_publishes_the_same_markdown_and_org_source() {
        for (index, (path, kind, edited, inserted, settled)) in [
            (
                "content/pages/研究/über topic.md",
                ManagedTextKind::Page,
                b"- TODO utf derivation edit [[Other Page]]\n\t- child\n".as_slice(),
                b"- TODO utf derivation edit [[Other Page]]\n\t- child\n- DONE appended tail\n"
                    .as_slice(),
                b"- TODO settled after deferral\n\t- child\n- DONE appended tail\n".as_slice(),
            ),
            (
                "content/pages/研究/über topic.org",
                ManagedTextKind::Page,
                b"* TODO utf derivation edit [[Other Page]]\n** child\n".as_slice(),
                b"* TODO utf derivation edit [[Other Page]]\n** child\n* DONE appended tail\n"
                    .as_slice(),
                b"* TODO settled after deferral\n** child\n* DONE appended tail\n".as_slice(),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let mut fixture = Fixture::new_at(
                &format!("local-derivation-{index}"),
                path,
                Some(
                    "{:pages-directory \"content/pages\"\n\
                      :journals-directory \"content/journals\"}\n",
                ),
                kind,
            );

            let edit_author = fixture.local_author(44_000 + index as u128 * 100);
            let edit = OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: fixture.block_id,
                    home_document_id: fixture.home_document_id,
                },
                content: "TODO utf derivation edit [[Other Page]]".into(),
            }])
            .unwrap();
            let observed = fixture.engine.assert_draft_matches_previous_derivation(
                edit_author,
                BatchOrigin::LocalMutation,
                &edit,
            );
            assert_eq!(observed.refused, None);
            assert_eq!(
                observed.optimized_catalog_copies, 0,
                "a page-local content edit must read the catalog in place"
            );
            assert!(observed.oracle_catalog_copies >= 1);
            let state = fixture.execute_local(edit_author, &edit);
            settle_local(&mut fixture, state);
            assert_eq!(fs::read(fixture.graph_root.join(path)).unwrap(), edited);
            fixture.assert_drained();

            // Deferred, then durable: the same derivation must survive a
            // publication that only completes on the retry.
            let insert_author = fixture.local_author(44_050 + index as u128 * 100);
            let insert = OperationTransaction::new(vec![SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id: BlockId::from_uuid(Uuid::from_u128(44_900 + index as u128)),
                    home_document_id: fixture.home_document_id,
                },
                page_id: PageId::from_uuid(Uuid::from_u128(5)),
                parent: None,
                order: "b".into(),
                content: "DONE appended tail".into(),
            }])
            .unwrap();
            let observed = fixture.engine.assert_draft_matches_previous_derivation(
                insert_author,
                BatchOrigin::LocalMutation,
                &insert,
            );
            assert_eq!(observed.refused, None);
            assert_eq!(observed.optimized_catalog_copies, 0);
            assert!(observed.oracle_catalog_copies >= 1);

            let state = fixture.execute_local(insert_author, &insert);
            settle_local(&mut fixture, state);
            assert_eq!(fs::read(fixture.graph_root.join(path)).unwrap(), inserted);
            fixture.assert_drained();

            // Deferred, then durable: the same derivation must survive a
            // publication that only completes on the retry.
            let settle_author = fixture.local_author(44_070 + index as u128 * 100);
            let settle = OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: fixture.block_id,
                    home_document_id: fixture.home_document_id,
                },
                content: "TODO settled after deferral".into(),
            }])
            .unwrap();
            let observed = fixture.engine.assert_draft_matches_previous_derivation(
                settle_author,
                BatchOrigin::LocalMutation,
                &settle,
            );
            assert_eq!(observed.refused, None);
            assert_eq!(observed.optimized_catalog_copies, 0);
            fail_once_at(OperationalFaultPoint::BeforeProjection);
            let deferred =
                expect_local_published_recovery(fixture.execute_local(settle_author, &settle));
            let batch_id = deferred.batch_id();
            let state = deferred.retry(
                &LocalRuntimeAdmission::unenrolled_pre_activation(),
                &fixture.graph,
                &fixture.receipts,
                &mut fixture.engine,
                &mut fixture.database,
                &mut fixture.tail,
            );
            let completion = settle_local(&mut fixture, state);
            assert_eq!(completion.batch_id(), batch_id);
            assert_eq!(fs::read(fixture.graph_root.join(path)).unwrap(), settled);
            fixture.assert_drained();

            // Restart and replay must reproduce exactly the same source.
            let restarted = fixture.restart_projection_runtime();
            assert_eq!(fs::read(restarted.graph_root.join(path)).unwrap(), settled);
            restarted.assert_drained();
        }
    }

    #[test]
    fn production_local_api_owns_author_identity_and_raw_entry_is_test_only() {
        let source = include_str!("operational_coordinator.rs");
        let production = source
            .split_once("\n#[cfg(test)]\nmod tests")
            .expect("the coordinator test module must remain separated")
            .0;
        let signature = production
            .split_once("pub(crate) fn execute_local(")
            .expect("the production local entry must exist")
            .1
            .split_once(") -> LocalMutationCoordinatorState")
            .expect("the production local signature must close")
            .0;
        assert!(signature.contains("session: &mut PromotedRuntimeSession<'_>"));
        assert!(signature.contains("transaction: &OperationTransaction"));
        for forbidden in [
            "AuthorBatch",
            "BatchId",
            "DeviceId",
            "SessionId",
            "CrdtPeerId",
            "LocalRuntimeAdmission",
            "ShardedHotEngine",
            "SqliteFrontier",
            "TailOverlay",
        ] {
            assert!(
                !signature.contains(forbidden),
                "production local callers can still supply authoritative `{forbidden}`"
            );
        }
        assert!(
            production.contains(
                "#[cfg(test)]\n    #[allow(clippy::too_many_arguments)]\n    fn \
                 execute_local_with_author("
            ),
            "the raw-author coordinator entry must stay test-only"
        );

        let engine = include_str!("hot_engine.rs");
        let raw_engine = engine
            .split_once("pub fn draft_author_transaction(")
            .expect("the legacy raw fixture helper remains named")
            .1
            .split_once("self.draft_author_transaction_with_observation")
            .expect("the raw helper must still delegate to the shared draft core")
            .0;
        assert!(raw_engine.contains("#[cfg(not(test))]"));
        assert!(raw_engine.contains("self.promoted_lineage().is_some()"));
        assert!(raw_engine.contains("origin == BatchOrigin::LocalMutation"));
        assert!(
            raw_engine.contains("raw local author identity is unavailable"),
            "a production promoted engine must refuse the raw-author fixture helper"
        );
    }

    #[test]
    fn origin_specific_continuations_are_affine_nonserialized_and_panic_free() {
        let source = include_str!("operational_coordinator.rs");
        let production = source
            .split_once("\n#[cfg(test)]\nmod tests")
            .expect("the coordinator test module must remain separated")
            .0;
        assert!(production.contains("pub(crate) struct ExternalPublishedContinuation"));
        assert!(production.contains("pub(crate) struct LocalPublishedContinuation"));
        assert!(!production.contains("panic!("));

        for name in [
            "ExternalPublishedContinuation",
            "LocalPublishedContinuation",
        ] {
            let declaration = format!("pub(crate) struct {name}");
            let offset = production.find(&declaration).unwrap();
            let prefix = &production[offset.saturating_sub(120)..offset];
            assert!(
                !prefix.contains("#[derive("),
                "{name} must remain non-cloneable and non-serializable"
            );
        }
        let external = production
            .split_once("impl ExternalPublishedContinuation {")
            .unwrap()
            .1
            .split_once("/// Affine admitted-local continuation")
            .unwrap()
            .0;
        assert!(external.contains("pub(crate) const fn import_id"));
        assert!(external.contains("pub(crate) fn retry"));
        assert!(!external.contains("LocalMutationCoordinatorState"));

        let local = production
            .split_once("impl LocalPublishedContinuation {")
            .unwrap()
            .1
            .split_once("pub(crate) struct OperationalCoordinator")
            .unwrap()
            .0;
        assert!(local.contains("pub(crate) fn retry"));
        assert!(!local.contains("import_id"));
        assert!(!local.contains("OperationalCoordinatorState"));
    }

    #[test]
    fn local_continuation_drop_stays_closed_and_completion_releases_once() {
        let mut dropped = Fixture::new("drop-local-published");
        let releases = dropped.graph.handoff_release_count();
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let continuation =
            expect_local_published_recovery(dropped.local_edit(43_500, "drop local continuation"));
        drop(continuation);
        assert_eq!(dropped.graph.handoff_release_count(), releases);
        assert!(dropped.graph.probe_managed_text_writer().is_err());

        let mut completed = Fixture::new("complete-local-once");
        let releases = completed.graph.handoff_release_count();
        expect_local_active(completed.local_edit(43_510, "complete local once"));
        assert_eq!(completed.graph.handoff_release_count(), releases + 1);
        completed.graph.probe_managed_text_writer().unwrap();
        completed.graph.probe_managed_text_writer().unwrap();
        assert_eq!(completed.graph.handoff_release_count(), releases + 1);
    }

    #[test]
    fn retained_terminal_dispositions_are_blocked_while_progress_is_recovering() {
        let cases = [
            (
                "rejected",
                BatchDisposition::Rejected {
                    error: super::super::EngineError::AuthorDraftStale,
                },
                Some(RetainedBlockReason::Rejected(
                    super::super::EngineError::AuthorDraftStale,
                )),
            ),
            (
                "quarantined",
                BatchDisposition::Quarantined,
                Some(RetainedBlockReason::Quarantined),
            ),
            (
                "bounded",
                BatchDisposition::IncompleteStaged {
                    missing_objects: 0,
                    missing_dependencies: Vec::new(),
                },
                None,
            ),
        ];
        for (index, (label, disposition, expected_block)) in cases.into_iter().enumerate() {
            let mut fixture = Fixture::new(&format!("retained-{label}"));
            fail_once_at(OperationalFaultPoint::AfterManifest);
            let mut continuation = expect_local_published_recovery(
                fixture.local_edit(43_600 + index as u128 * 10, "retained classification"),
            );
            let manifests = fixture.archive.committed_manifests().unwrap().len();
            continuation.core.failure =
                require_accepted_stage_disposition(continuation.batch_id(), &disposition)
                    .expect_err("the synthetic final/progress disposition must retain work");
            let state = LocalMutationCoordinatorState::from_failed(continuation);
            match expected_block {
                Some(reason) => {
                    let LocalMutationCoordinatorState::Blocked(blocked) = state else {
                        panic!("{label} must be a retained typed Blocked outcome");
                    };
                    assert_eq!(
                        blocked.reason(),
                        &LocalMutationBlockReason::Retained(reason)
                    );
                    assert!(blocked.continuation().is_some());
                }
                None => {
                    let LocalMutationCoordinatorState::Recovering(
                        LocalMutationRecovery::Published(continuation),
                    ) = state
                    else {
                        panic!("bounded staging work must remain Recovering");
                    };
                    assert_eq!(continuation.phase(), OperationalPhase::ArchiveStage);
                }
            }
            assert_eq!(
                fixture.archive.committed_manifests().unwrap().len(),
                manifests,
                "classification must not redraft or republish"
            );
        }

        let mut fixture = Fixture::new("retained-authentication");
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let mut continuation =
            expect_local_published_recovery(fixture.local_edit(43_700, "stable auth"));
        continuation.core.failure = OperationalCoordinatorError::retained_block(
            OperationalPhase::Publication,
            "stable immutable authentication mismatch",
            RetainedBlockReason::PublishedAuthentication,
        );
        let LocalMutationCoordinatorState::Blocked(blocked) =
            LocalMutationCoordinatorState::from_failed(continuation)
        else {
            panic!("stable authentication failure must be retained Blocked");
        };
        assert_eq!(
            blocked.reason(),
            &LocalMutationBlockReason::Retained(RetainedBlockReason::PublishedAuthentication)
        );
        assert!(blocked.continuation().is_some());
    }

    #[test]
    fn rejected_published_local_batch_retains_typed_blocked_evidence() {
        let mut fixture = Fixture::new("published-rejected-blocked");
        let accepted_before = fixture.engine.accepted_frontier_root().unwrap();
        let sqlite_before = fixture.database.frontier_root().unwrap();
        let manifests_before = fixture.archive.committed_manifests().unwrap().len();
        act_once_at(OperationalFaultPoint::AfterManifest, || {
            fail_next_engine_history_head_swap();
        });
        let LocalMutationCoordinatorState::Blocked(blocked) =
            fixture.local_edit(43_800, "history head rejection")
        else {
            panic!("durable history rejection must return retained Blocked");
        };
        assert!(matches!(
            blocked.reason(),
            LocalMutationBlockReason::Retained(RetainedBlockReason::Rejected(_))
        ));
        let continuation = blocked
            .continuation()
            .expect("the rejected immutable batch retains its continuation/evidence");
        assert_eq!(continuation.phase(), OperationalPhase::ArchiveStage);
        assert_eq!(
            fixture.archive.committed_manifests().unwrap().len(),
            manifests_before + 1
        );
        assert_eq!(
            fixture.engine.accepted_frontier_root().unwrap(),
            accepted_before
        );
        assert_eq!(fixture.database.frontier_root().unwrap(), sqlite_before);
        assert!(fixture.graph.probe_managed_text_writer().is_err());
    }

    #[test]
    fn stable_postpublication_binding_failure_retains_typed_blocked_continuation() {
        let mut fixture = Fixture::new("local-stable-postpublication-binding");
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let continuation =
            expect_local_published_recovery(fixture.local_edit(43_900, "stable binding"));
        let foreign_root = TestRoot::new("local-stable-postpublication-binding-foreign");
        let foreign_graph_root = foreign_root.path().join("graph");
        fs::create_dir_all(&foreign_graph_root).unwrap();
        let foreign_graph = Graph::open(&foreign_graph_root);

        let LocalMutationCoordinatorState::Blocked(blocked) = continuation.retry(
            &LocalRuntimeAdmission::unenrolled_pre_activation(),
            &foreign_graph,
            &fixture.receipts,
            &mut fixture.engine,
            &mut fixture.database,
            &mut fixture.tail,
        ) else {
            panic!("stable rebound graph authentication must return retained Blocked");
        };
        assert_eq!(
            blocked.reason(),
            &LocalMutationBlockReason::Retained(RetainedBlockReason::StableBinding)
        );
        assert!(blocked.continuation().is_some());
        assert!(fixture.graph.probe_managed_text_writer().is_err());
    }

    #[test]
    fn blocked_and_noop_cancel_without_durable_or_derived_mutation() {
        let mut fixture = Fixture::new("blocked-noop");
        let accepted = fixture.engine.accepted_frontier_root().unwrap();
        let sqlite = fixture.database.frontier_root().unwrap();
        let tail = fixture.tail.status();
        let graph = fs::read(fixture.graph_root.join(&fixture.path)).unwrap();
        let archive = snapshot_tree(&fixture.archive_root);
        let receipts = snapshot_tree(fixture.receipts.root_path());

        assert!(matches!(
            fixture.execute(&["../escape.md"]),
            OperationalCoordinatorState::Blocked(_)
        ));
        let path = fixture.path.clone();
        assert!(matches!(
            fixture.execute(&[&path]),
            OperationalCoordinatorState::Noop
        ));
        assert_eq!(fixture.engine.accepted_frontier_root().unwrap(), accepted);
        assert_eq!(fixture.database.frontier_root().unwrap(), sqlite);
        assert_eq!(fixture.tail.status(), tail);
        assert_eq!(
            fs::read(fixture.graph_root.join(&fixture.path)).unwrap(),
            graph
        );
        assert_eq!(snapshot_tree(&fixture.archive_root), archive);
        assert_eq!(snapshot_tree(fixture.receipts.root_path()), receipts);
        fixture.graph.probe_managed_text_writer().unwrap();
    }

    #[test]
    fn formatting_only_noop_adopts_exact_bytes_without_a_semantic_batch() {
        let mut fixture = Fixture::formatting_only("formatting-only-noop");
        let path = fixture.path.clone();
        let formatted = b"- root\r\n\r\n\t- child\r\n";
        fixture.overwrite(formatted);
        let accepted = fixture.engine.accepted_frontier_root().unwrap();
        let sqlite = fixture.database.frontier_root().unwrap();
        let manifests = fixture.archive.committed_manifests().unwrap();
        let receipts_before = snapshot_tree(fixture.receipts.root_path());
        let plan =
            plan_affected_import(&fixture.graph, &fixture.receipts, &fixture.engine, &[&path]);
        assert_eq!(plan.status(), ImportPlanStatus::Noop, "{plan:?}");

        assert!(matches!(
            fixture.execute(&[&path]),
            OperationalCoordinatorState::Noop
        ));
        assert_eq!(fixture.engine.accepted_frontier_root().unwrap(), accepted);
        assert_eq!(fixture.database.frontier_root().unwrap(), sqlite);
        assert_eq!(fixture.archive.committed_manifests().unwrap(), manifests);
        assert_ne!(
            snapshot_tree(fixture.receipts.root_path()),
            receipts_before,
            "the endpoint-local exact baseline must advance"
        );
        assert_eq!(fs::read(fixture.graph_root.join(&path)).unwrap(), formatted);

        expect_local_active(fixture.local_edit(49_000, "root edited"));
        fixture.assert_drained();
        assert_eq!(
            fs::read(fixture.graph_root.join(&path)).unwrap(),
            b"- root edited\r\n\r\n\t- child\r\n",
            "the next real semantic edit must render from the adopted formatting baseline"
        );
    }

    #[test]
    fn formatting_receipt_survives_crash_reopen_late_callback_and_next_local_save() {
        let mut fixture = Fixture::formatting_only("formatting-receipt-reopen");
        let path = fixture.path.clone();
        let formatted = b"- root\r\n\r\n\t- child\r\n";
        fixture.overwrite(formatted);
        assert!(matches!(
            fixture.execute(&[&path]),
            OperationalCoordinatorState::Noop
        ));

        // Model a process loss after the durable completion/path row exists,
        // before its watcher callback. Reopen has no RAM predecessor to use.
        fixture = fixture.restart_projection_runtime();

        // The late callback must derive an authenticated no-op from retained
        // inventory and the durable completed-path receipt, not reconcile its
        // own exact projection as an external edit.
        assert!(matches!(
            fixture.execute(&[&path]),
            OperationalCoordinatorState::Noop
        ));
        expect_local_active(fixture.local_edit(49_050, "after unsafe reopen"));
        fixture.assert_drained();
        assert_eq!(
            fs::read(fixture.graph_root.join(&path)).unwrap(),
            b"- after unsafe reopen\r\n\r\n\t- child\r\n"
        );
    }

    #[test]
    fn formatting_only_intent_recovers_after_restart_before_the_next_real_edit() {
        let mut fixture = Fixture::formatting_only("formatting-only-restart");
        let path = fixture.path.clone();
        let formatted = b"- root\r\n\r\n\t- child\r\n";
        fixture.overwrite(formatted);
        let manifests = fixture.archive.committed_manifests().unwrap();
        fail_next_formatting_adoption_after_intent_for_harness();
        let failed = match OperationalCoordinator::execute(
            &LocalRuntimeAdmission::unenrolled_pre_activation(),
            &fixture.graph,
            &fixture.receipts,
            &mut fixture.engine,
            &mut fixture.database,
            &mut fixture.tail,
            &[&path],
        ) {
            Err(error) => error,
            Ok(_) => panic!("formatting adoption fault unexpectedly completed"),
        };
        assert_eq!(failed.phase(), OperationalPhase::Planning);
        assert_eq!(fixture.archive.committed_manifests().unwrap(), manifests);
        assert_eq!(fs::read(fixture.graph_root.join(&path)).unwrap(), formatted);

        fixture = fixture.restart_projection_runtime();
        let recovered =
            recover_incomplete_projections(&fixture.graph, &fixture.receipts, &fixture.engine)
                .unwrap();
        assert_eq!(recovered.len(), 1);
        assert!(matches!(
            fixture.execute(&[&path]),
            OperationalCoordinatorState::Noop
        ));
        expect_local_active(fixture.local_edit(49_100, "after restart"));
        assert_eq!(
            fs::read(fixture.graph_root.join(&path)).unwrap(),
            b"- after restart\r\n\r\n\t- child\r\n"
        );
    }

    #[test]
    fn every_pre_manifest_boundary_releases_and_allows_fresh_retry() {
        let cases = [
            OperationalFaultPoint::AfterHandoff,
            OperationalFaultPoint::AfterPlan,
            OperationalFaultPoint::AfterDraft,
            OperationalFaultPoint::AfterCapture,
            OperationalFaultPoint::AfterFinalize,
            OperationalFaultPoint::AfterReservation,
        ];
        for (index, point) in cases.into_iter().enumerate() {
            let mut fixture = Fixture::new(&format!("pre-manifest-{index}"));
            let path = fixture.path.clone();
            fixture.overwrite(b"- changed\n\t- still nested\n");
            let accepted = fixture.engine.accepted_frontier_root().unwrap();
            let sqlite = fixture.database.frontier_root().unwrap();
            fail_once_at(point);
            assert!(OperationalCoordinator::execute(
                &LocalRuntimeAdmission::unenrolled_pre_activation(),
                &fixture.graph,
                &fixture.receipts,
                &mut fixture.engine,
                &mut fixture.database,
                &mut fixture.tail,
                &[&path],
            )
            .is_err());
            assert_eq!(fixture.engine.accepted_frontier_root().unwrap(), accepted);
            assert_eq!(fixture.database.frontier_root().unwrap(), sqlite);
            assert_eq!(fixture.tail.status().unapplied_batches, 0);
            fixture.graph.probe_managed_text_writer().unwrap();
            expect_complete(fixture.execute(&[&path]));
            fixture.assert_drained();
        }
    }

    #[test]
    fn stale_observation_and_receipt_capture_reject_before_publication() {
        let mut observation = Fixture::new("stale-observation");
        let path = observation.path.clone();
        observation.overwrite(b"- first external edit\n");
        let target = observation.graph_root.join(&path);
        act_once_at(OperationalFaultPoint::AfterPlan, move || {
            fs::write(target, b"- replacement during draft\n").unwrap();
        });
        assert!(matches!(
            OperationalCoordinator::execute(
                &LocalRuntimeAdmission::unenrolled_pre_activation(),
                &observation.graph,
                &observation.receipts,
                &mut observation.engine,
                &mut observation.database,
                &mut observation.tail,
                &[&path],
            ),
            Err(OperationalCoordinatorError {
                phase: OperationalPhase::Capture,
                ..
            })
        ));
        observation.graph.probe_managed_text_writer().unwrap();
        expect_complete(observation.execute(&[&path]));
        observation.assert_drained();

        let mut receipt = Fixture::new("stale-receipt");
        let path = receipt.path.clone();
        receipt.overwrite(b"- receipt edit\n");
        let completion = receipt
            .receipts
            .root_path()
            .join("completions")
            .join(format!(
                "{}.completion",
                hex(receipt.intent.id().unwrap().as_bytes())
            ));
        let held = completion.with_extension("completion.held");
        let move_from = completion.clone();
        let move_to = held.clone();
        act_once_at(OperationalFaultPoint::AfterCapture, move || {
            fs::rename(move_from, move_to).unwrap();
        });
        assert!(matches!(
            OperationalCoordinator::execute(
                &LocalRuntimeAdmission::unenrolled_pre_activation(),
                &receipt.graph,
                &receipt.receipts,
                &mut receipt.engine,
                &mut receipt.database,
                &mut receipt.tail,
                &[&path],
            ),
            Err(OperationalCoordinatorError {
                phase: OperationalPhase::Finalize,
                ..
            })
        ));
        fs::rename(held, completion).unwrap();
        receipt.graph.probe_managed_text_writer().unwrap();
        expect_complete(receipt.execute(&[&path]));
        receipt.assert_drained();
    }

    #[test]
    fn exact_reservation_precedes_manifest_and_object_only_cut_has_no_semantic_effect() {
        let mut pressured = Fixture::new("reservation-first");
        let path = pressured.path.clone();
        pressured.overwrite(b"- pressure edit\n");
        let filler = pressured.tail.reserve_mutation(TAIL_MAX_BYTES).unwrap();
        let accepted = pressured.engine.accepted_frontier_root().unwrap();
        let result = OperationalCoordinator::execute(
            &LocalRuntimeAdmission::unenrolled_pre_activation(),
            &pressured.graph,
            &pressured.receipts,
            &mut pressured.engine,
            &mut pressured.database,
            &mut pressured.tail,
            &[&path],
        );
        assert!(matches!(
            result,
            Err(OperationalCoordinatorError {
                phase: OperationalPhase::TailReservation,
                ..
            })
        ));
        assert_eq!(pressured.engine.accepted_frontier_root().unwrap(), accepted);
        pressured.tail.cancel_reservation(filler).unwrap();
        pressured.graph.probe_managed_text_writer().unwrap();

        let mut objects = Fixture::new("objects-only");
        let path = objects.path.clone();
        objects.overwrite(b"- objects only\n");
        let accepted = objects.engine.accepted_frontier_root().unwrap();
        let sqlite = objects.database.frontier_root().unwrap();
        let before = snapshot_tree(&objects.archive_root);
        fail_next_publish_after_objects();
        assert!(matches!(
            OperationalCoordinator::execute(
                &LocalRuntimeAdmission::unenrolled_pre_activation(),
                &objects.graph,
                &objects.receipts,
                &mut objects.engine,
                &mut objects.database,
                &mut objects.tail,
                &[&path],
            ),
            Err(OperationalCoordinatorError {
                phase: OperationalPhase::Publication,
                ..
            })
        ));
        assert_eq!(objects.engine.accepted_frontier_root().unwrap(), accepted);
        assert_eq!(objects.database.frontier_root().unwrap(), sqlite);
        assert_ne!(snapshot_tree(&objects.archive_root), before);
        objects.graph.probe_managed_text_writer().unwrap();
        expect_complete(objects.execute(&[&path]));
        objects.assert_drained();
    }

    #[test]
    fn every_post_manifest_failure_retains_guard_and_retries_idempotently() {
        let cases = [
            OperationalFaultPoint::AfterManifest,
            OperationalFaultPoint::AfterStage,
            OperationalFaultPoint::AfterTailAdmission,
            OperationalFaultPoint::AfterSqliteApply,
            OperationalFaultPoint::BeforeProjection,
            OperationalFaultPoint::AfterProjection,
        ];
        for (index, point) in cases.into_iter().enumerate() {
            let mut fixture = Fixture::new(&format!("post-manifest-{index}"));
            let path = fixture.path.clone();
            fixture.overwrite(b"- durable edit\n\t- nested durable edit\n");
            fail_once_at(point);
            let failed = expect_failed(fixture.execute(&[&path]));
            assert_eq!(failed.batch_id(), failed.import_id().batch_id());
            assert!(fixture.graph.probe_managed_text_writer().is_err());
            if point == OperationalFaultPoint::AfterManifest {
                assert_eq!(
                    fixture.tail.status().retained_bytes,
                    failed.retained_bytes()
                );
            }
            if matches!(
                point,
                OperationalFaultPoint::BeforeProjection | OperationalFaultPoint::AfterProjection
            ) {
                assert_eq!(
                    fixture.database.frontier_root().unwrap(),
                    fixture.engine.accepted_frontier_root().unwrap(),
                    "projection faults are reachable only after exact SQLite catch-up"
                );
            }
            let completion = expect_complete(failed.retry(
                &LocalRuntimeAdmission::unenrolled_pre_activation(),
                &fixture.graph,
                &fixture.receipts,
                &mut fixture.engine,
                &mut fixture.database,
                &mut fixture.tail,
            ));
            assert_eq!(completion.batch_id(), failed_batch_id(&completion));
            fixture.graph.probe_managed_text_writer().unwrap();
            fixture.assert_drained();
            assert_eq!(
                fs::read(fixture.graph_root.join(path)).unwrap(),
                b"- durable edit\n\t- nested durable edit\n"
            );
        }
    }

    #[test]
    fn sqlite_budget_boundary_retains_handoff_and_resumes_without_republication() {
        let _budget = test_resume_operation_budget(16);
        const PREEXISTING: usize = 20;
        let mut fixture = Fixture::new("bounded-sqlite-resume");
        for index in 0..PREEXISTING {
            let transaction = if index == 0 {
                OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: fixture.block_id,
                        home_document_id: fixture.home_document_id,
                    },
                    content: "durable accepted tail base".into(),
                }])
                .unwrap()
            } else {
                OperationTransaction::new(vec![SemanticOperation::CreatePage {
                    page_id: PageId::from_uuid(Uuid::from_u128(64_000 + index as u128)),
                    home_document_id: DocumentId::from_uuid(Uuid::from_u128(
                        65_000 + index as u128,
                    )),
                    name: LogicalPageName::parse(&format!("Bounded Tail {index}")).unwrap(),
                    path: ManagedPath::parse(&format!("pages/bounded-tail-{index}.md")).unwrap(),
                    kind: ManagedTextKind::Page,
                }])
                .unwrap()
            };
            let prepared = fixture
                .engine
                .prepare_bootstrap_transaction(
                    AuthorBatch {
                        batch_id: BatchId::from_uuid(Uuid::from_u128(60_000 + index as u128)),
                        author_device_id: DeviceId::from_uuid(Uuid::from_u128(
                            61_000 + index as u128,
                        )),
                        author_session_id: SessionId::from_uuid(Uuid::from_u128(
                            62_000 + index as u128,
                        )),
                        crdt_peer_id: CrdtPeerId::from_u64(63_000 + index as u64),
                    },
                    &transaction,
                )
                .unwrap();
            let batch_id = prepared.manifest().batch_id();
            fixture
                .archive
                .publish_bootstrap_prepared_for_test(&prepared)
                .unwrap();
            assert!(matches!(
                fixture
                    .engine
                    .stage_archive_batch(batch_id)
                    .unwrap()
                    .disposition(),
                BatchDisposition::Accepted { .. }
            ));
            let event =
                AcceptedBatchEvent::from_accepted(&fixture.engine, &fixture.archive, batch_id)
                    .unwrap();
            fixture
                .tail
                .try_enqueue(&mut fixture.database, &fixture.engine, &event)
                .unwrap();
        }
        let current = fs::read(fixture.graph_root.join(&fixture.path)).unwrap();
        fixture.intent = write_projection_exact(
            &fixture.graph,
            &fixture.receipts,
            &fixture.engine,
            PageId::from_uuid(Uuid::from_u128(5)),
            Some(&current),
        )
        .unwrap()
        .plan
        .intent()
        .clone();
        assert_eq!(fixture.tail.status().unapplied_batches, PREEXISTING);

        let path = fixture.path.clone();
        fixture.overwrite(b"- bounded coordinator drain\n");
        let releases_before = fixture.graph.handoff_release_count();
        let mut failed = expect_failed(fixture.execute(&[&path]));
        let batch_id = failed.batch_id();
        assert_eq!(failed.batch_id(), failed.import_id().batch_id());
        assert!(fixture.graph.probe_managed_text_writer().is_err());
        // The immutable publication is complete and byte-frozen from here on.
        let published = snapshot_immutable_publication(&fixture.archive_root);
        let published_count = fixture.archive.committed_manifests().unwrap().len();
        assert_eq!(published_count, PREEXISTING + 2);

        // Exact per-resume accounting: phase, remaining backlog, retained
        // publication bytes, and handoff release count.
        let mut phases = vec![failed.phase()];
        let mut backlog = vec![fixture.tail.status().unapplied_batches];
        let completion = loop {
            // Nothing about the immutable publication, the retained latch, or
            // the release counter may move on a failed retry.
            assert_eq!(
                snapshot_immutable_publication(&fixture.archive_root),
                published,
                "a failed retry republished, mutated, or left residue in the immutable archive"
            );
            assert_eq!(
                fixture.archive.committed_manifests().unwrap().len(),
                published_count
            );
            assert_eq!(
                fixture.graph.handoff_release_count(),
                releases_before,
                "a failed retry released the retained managed-text handoff"
            );
            assert!(fixture.graph.probe_managed_text_writer().is_err());
            match failed.retry(
                &LocalRuntimeAdmission::unenrolled_pre_activation(),
                &fixture.graph,
                &fixture.receipts,
                &mut fixture.engine,
                &mut fixture.database,
                &mut fixture.tail,
            ) {
                OperationalCoordinatorState::Complete(completion) => break completion,
                OperationalCoordinatorState::FailedClosed(next) => {
                    failed = next;
                    phases.push(failed.phase());
                    backlog.push(fixture.tail.status().unapplied_batches);
                    assert!(phases.len() <= PREEXISTING + 4);
                }
                OperationalCoordinatorState::Blocked(_) | OperationalCoordinatorState::Noop => {
                    panic!("published bounded retry changed semantic state")
                }
            }
        };
        assert_eq!(completion.batch_id(), batch_id);
        // Weighted parent-clock and whole-batch prepayment may require several
        // ArchiveStage continuations before the published event can enter the
        // tail. Those retries consume only their own staging budget and retain
        // the latch. Once staging completes, the unchanged SQLite arithmetic
        // applies a strict bounded prefix and reports its durable remainder.
        assert_eq!(phases.last(), Some(&OperationalPhase::SqliteDrain));
        assert!(phases[..phases.len() - 1]
            .iter()
            .all(|phase| *phase == OperationalPhase::ArchiveStage));
        let sqlite_remainder = *backlog.last().unwrap();
        assert!(
            sqlite_remainder > 0 && sqlite_remainder < PREEXISTING + 1,
            "SQLite must apply a nonempty strict prefix under the remaining resume budget: {backlog:?}"
        );

        // Completion is the only release, and it happens exactly once.
        assert_eq!(fixture.graph.handoff_release_count(), releases_before + 1);
        assert_eq!(
            snapshot_immutable_publication(&fixture.archive_root),
            published,
            "completion republished or left residue in the immutable archive"
        );
        assert_eq!(
            fixture.engine.accepted_batch_count().unwrap(),
            u64::try_from(PREEXISTING + 2).unwrap()
        );
        fixture.graph.probe_managed_text_writer().unwrap();
        fixture.graph.probe_managed_text_writer().unwrap();
        assert_eq!(fixture.graph.handoff_release_count(), releases_before + 1);
        fixture.assert_drained();
    }

    #[test]
    fn retained_continuation_authenticates_the_archive_by_stable_resource_identity() {
        let fixture = Fixture::new("archive-identity");
        let workspace = fixture.engine.workspace_id();
        let endpoint = fixture.engine.projection_endpoint_binding().unwrap();

        // A separately opened handle to the exact enrolled archive resource is
        // accepted, so a same-process engine reconstruction does not have to
        // preserve `Arc` pointer identity to resume a published continuation.
        let reopened = Arc::new(ObjectStore::open(&fixture.archive_root, workspace).unwrap());
        verify_bindings(
            &fixture.graph,
            &fixture.receipts,
            &fixture.engine,
            endpoint,
            Some(&reopened),
        )
        .unwrap();

        // A different archive directory carrying the same workspace identity is
        // still rejected, so the relaxation does not weaken authentication.
        let foreign_root = TestRoot::new("archive-identity-foreign");
        let foreign =
            Arc::new(ObjectStore::open(&foreign_root.path().join("archive"), workspace).unwrap());
        let rejected = verify_bindings(
            &fixture.graph,
            &fixture.receipts,
            &fixture.engine,
            endpoint,
            Some(&foreign),
        )
        .expect_err("a substituted archive resource must not authenticate");
        assert_eq!(rejected.phase(), OperationalPhase::Bindings);
        assert!(rejected.detail().contains("archive resource identity"));
        fixture.graph.probe_managed_text_writer().unwrap();
    }

    #[test]
    fn published_continuation_survives_same_process_engine_reconstruction() {
        let mut fixture = Fixture::new("engine-reconstruction");
        let path = fixture.path.clone();
        fixture.overwrite(b"- reconstructed continuation\n\t- nested\n");
        let releases = fixture.graph.handoff_release_count();
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let failed = expect_failed(fixture.execute(&[&path]));
        assert!(fixture.graph.probe_managed_text_writer().is_err());

        // Discard every run-local derived engine structure. Only the retained
        // capabilities and their authenticated durable roots survive.
        fixture.engine.reconstruct_run_local_state().unwrap();
        assert_eq!(fixture.graph.handoff_release_count(), releases);

        let completion = expect_complete(failed.retry(
            &LocalRuntimeAdmission::unenrolled_pre_activation(),
            &fixture.graph,
            &fixture.receipts,
            &mut fixture.engine,
            &mut fixture.database,
            &mut fixture.tail,
        ));
        assert_eq!(completion.batch_id(), completion.import_id().batch_id());
        assert_eq!(fixture.graph.handoff_release_count(), releases + 1);
        fixture.graph.probe_managed_text_writer().unwrap();
        fixture.assert_drained();
        assert_eq!(
            fs::read(fixture.graph_root.join(path)).unwrap(),
            b"- reconstructed continuation\n\t- nested\n"
        );
    }

    #[test]
    fn dropping_published_continuation_stays_closed_and_completion_releases_once() {
        let mut dropped = Fixture::new("drop-published");
        let path = dropped.path.clone();
        dropped.overwrite(b"- durable dropped continuation\n");
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let failed = expect_failed(dropped.execute(&[&path]));
        let releases = dropped.graph.handoff_release_count();
        drop(failed);
        assert_eq!(dropped.graph.handoff_release_count(), releases);
        assert!(dropped.graph.probe_managed_text_writer().is_err());

        let mut completed = Fixture::new("complete-once");
        let path = completed.path.clone();
        completed.overwrite(b"- successful explicit completion\n");
        let releases = completed.graph.handoff_release_count();
        expect_complete(completed.execute(&[&path]));
        assert_eq!(completed.graph.handoff_release_count(), releases + 1);
        completed.graph.probe_managed_text_writer().unwrap();
        completed.graph.probe_managed_text_writer().unwrap();
        assert_eq!(completed.graph.handoff_release_count(), releases + 1);
    }

    #[test]
    fn manifested_preconditions_are_exact_fresh_external_observations() {
        let mut edit = Fixture::new("observed-precondition-edit");
        let path = edit.path.clone();
        let prior = fs::read(edit.graph_root.join(&path)).unwrap();
        let observed = b"- externally changed bytes\n\t- current annotation source\n".to_vec();
        edit.overwrite(&observed);
        let completion = expect_complete(edit.execute(&[&path]));
        let batch = match edit.archive.inspect_batch(completion.batch_id()).unwrap() {
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

        let mut absent = Fixture::new("observed-precondition-absent");
        let path = absent.path.clone();
        fs::remove_file(absent.graph_root.join(&path)).unwrap();
        let completion = expect_complete(absent.execute(&[&path]));
        let batch = match absent.archive.inspect_batch(completion.batch_id()).unwrap() {
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
    fn crdt_peer_probe_is_bounded_for_zero_collision_and_exhaustion() {
        let fixture = Fixture::new("peer-probe");
        let path = fixture.path.clone();
        fixture.overwrite(b"- peer probe edit\n");
        let plan =
            plan_affected_import(&fixture.graph, &fixture.receipts, &fixture.engine, &[&path]);
        let material = plan.into_execution_material().unwrap();
        let endpoint = fixture.engine.projection_endpoint_binding().unwrap();
        let candidates = [0, 12, 13];
        let (author, _) = draft_with_bounded_peer_candidates(
            &fixture.engine,
            endpoint,
            &material,
            None,
            |attempt| CrdtPeerId::from_u64(candidates[usize::try_from(attempt).unwrap().min(2)]),
        )
        .unwrap();
        assert_eq!(author.crdt_peer_id, CrdtPeerId::from_u64(13));

        let exhausted = match draft_with_bounded_peer_candidates(
            &fixture.engine,
            endpoint,
            &material,
            None,
            |_| CrdtPeerId::from_u64(12),
        ) {
            Err(error) => error,
            Ok(_) => panic!("colliding bounded peer probe unexpectedly succeeded"),
        };
        assert_eq!(exhausted.phase(), OperationalPhase::Draft);
        assert!(exhausted.detail().contains("bounded 8-candidate probe"));
        fixture.graph.probe_managed_text_writer().unwrap();
    }

    fn failed_batch_id(completion: &OperationalCompletion) -> BatchId {
        completion.import_id().batch_id()
    }

    #[test]
    fn delete_and_rename_project_exact_old_removal_and_new_render_base() {
        let mut deletion = Fixture::new("delete");
        let delete_path = deletion.path.clone();
        fs::remove_file(deletion.graph_root.join(&delete_path)).unwrap();
        expect_complete(deletion.execute(&[&delete_path]));
        deletion.assert_drained();
        assert!(!deletion.graph_root.join(delete_path).exists());

        let mut rename = Fixture::new("rename");
        let old = rename.path.clone();
        let new = "pages/elsewhere/deeper/renamed.md";
        fs::create_dir_all(rename.graph_root.join(new).parent().unwrap()).unwrap();
        fs::rename(rename.graph_root.join(&old), rename.graph_root.join(new)).unwrap();
        let completion = expect_complete(rename.execute(&[&old, new]));
        rename.assert_drained();
        assert!(!rename.graph_root.join(&old).exists());
        assert_eq!(
            fs::read(rename.graph_root.join(new)).unwrap(),
            b"- root\n\t- child\n"
        );
        let batch = match rename.archive.inspect_batch(completion.batch_id()).unwrap() {
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
        assert_eq!(base.bytes(), b"- root\n\t- child\n");
    }

    #[test]
    fn binding_mismatch_rejects_before_handoff_or_publication() {
        let mut fixture = Fixture::new("binding");
        let foreign_root = TestRoot::new("foreign-receipts");
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
            fixture.engine.workspace_id(),
            foreign_endpoint,
        )
        .unwrap();
        let path = fixture.path.clone();
        fixture.overwrite(b"- rejected binding\n");
        assert!(matches!(
            OperationalCoordinator::execute(
                &LocalRuntimeAdmission::unenrolled_pre_activation(),
                &fixture.graph,
                &foreign,
                &mut fixture.engine,
                &mut fixture.database,
                &mut fixture.tail,
                &[&path],
            ),
            Err(OperationalCoordinatorError {
                phase: OperationalPhase::Bindings,
                ..
            })
        ));
        fixture.graph.probe_managed_text_writer().unwrap();
    }

    #[test]
    fn post_manifest_retry_rejects_rebound_graph_and_keeps_original_guard() {
        let mut fixture = Fixture::new("retry-binding");
        let path = fixture.path.clone();
        fixture.overwrite(b"- durable retry binding\n");
        fail_once_at(OperationalFaultPoint::AfterManifest);
        let failed = expect_failed(fixture.execute(&[&path]));

        let foreign_root = TestRoot::new("retry-binding-foreign");
        let foreign_graph_root = foreign_root.path().join("graph");
        fs::create_dir_all(&foreign_graph_root).unwrap();
        let foreign_graph = Graph::open(&foreign_graph_root);
        let failed = expect_failed(failed.retry(
            &LocalRuntimeAdmission::unenrolled_pre_activation(),
            &foreign_graph,
            &fixture.receipts,
            &mut fixture.engine,
            &mut fixture.database,
            &mut fixture.tail,
        ));
        assert_eq!(failed.phase(), OperationalPhase::Bindings);
        assert!(fixture.graph.probe_managed_text_writer().is_err());
        expect_complete(failed.retry(
            &LocalRuntimeAdmission::unenrolled_pre_activation(),
            &fixture.graph,
            &fixture.receipts,
            &mut fixture.engine,
            &mut fixture.database,
            &mut fixture.tail,
        ));
        fixture.graph.probe_managed_text_writer().unwrap();
        fixture.assert_drained();
    }

    #[test]
    fn reordered_batch_ids_drain_by_authenticated_acceptance_sequence() {
        let mut fixture = Fixture::new("acceptance-sequence");
        let first = fixture
            .engine
            .prepare_bootstrap_transaction(
                AuthorBatch {
                    batch_id: BatchId::from_uuid(Uuid::from_u128(u128::MAX - 1)),
                    author_device_id: DeviceId::from_uuid(Uuid::from_u128(700)),
                    author_session_id: SessionId::from_uuid(Uuid::from_u128(701)),
                    crdt_peer_id: CrdtPeerId::from_u64(702),
                },
                &OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: fixture.block_id,
                        home_document_id: fixture.home_document_id,
                    },
                    content: "first accepted".into(),
                }])
                .unwrap(),
            )
            .unwrap();
        fixture
            .archive
            .publish_bootstrap_prepared_for_test(&first)
            .unwrap();
        fixture
            .engine
            .stage_archive_batch(first.manifest().batch_id())
            .unwrap();
        let second = fixture
            .engine
            .prepare_bootstrap_transaction(
                AuthorBatch {
                    batch_id: BatchId::from_uuid(Uuid::from_u128(20)),
                    author_device_id: DeviceId::from_uuid(Uuid::from_u128(703)),
                    author_session_id: SessionId::from_uuid(Uuid::from_u128(704)),
                    crdt_peer_id: CrdtPeerId::from_u64(705),
                },
                &OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: fixture.block_id,
                        home_document_id: fixture.home_document_id,
                    },
                    content: "second accepted".into(),
                }])
                .unwrap(),
            )
            .unwrap();
        fixture
            .archive
            .publish_bootstrap_prepared_for_test(&second)
            .unwrap();
        fixture
            .engine
            .stage_archive_batch(second.manifest().batch_id())
            .unwrap();
        assert!(first.manifest().batch_id() > second.manifest().batch_id());
        let first_event = AcceptedBatchEvent::from_accepted(
            &fixture.engine,
            &fixture.archive,
            first.manifest().batch_id(),
        )
        .unwrap();
        let second_event = AcceptedBatchEvent::from_accepted(
            &fixture.engine,
            &fixture.archive,
            second.manifest().batch_id(),
        )
        .unwrap();
        assert!(first_event.acceptance_sequence() < second_event.acceptance_sequence());
        fixture
            .tail
            .try_enqueue(&mut fixture.database, &fixture.engine, &second_event)
            .unwrap();
        fixture
            .tail
            .try_enqueue(&mut fixture.database, &fixture.engine, &first_event)
            .unwrap();
        let source = RebuildSource::new(&fixture.engine, &fixture.archive).unwrap();
        assert_eq!(
            fixture
                .tail
                .drain_ready(&mut fixture.database, &source, 64)
                .unwrap(),
            2
        );
        assert_eq!(
            fixture.database.frontier_root().unwrap(),
            fixture.engine.accepted_frontier_root().unwrap()
        );
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
