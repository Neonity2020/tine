//! Restartable expansion of one authoritative managed-local journal record.
//!
//! The foreground journal record and its already-durable exact graph target
//! are authoritative on entry. This module never drafts, appends, allocates an
//! identity, or writes graph text. It only resumes the established immutable
//! archive, accepted-history, tail/SQLite, projection-receipt, authorship, and
//! provider derivatives of that exact record.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use tine_storage::{LocalJournalError, LocalJournalFrame};
use uuid::Uuid;

use crate::model::Graph;
use crate::oplog::batch::ObjectKind;
use crate::oplog::hot_engine::AcceptedFrontierRoot;
use crate::oplog::local_active::{LocalRuntimeAdmission, WorkspaceAuthorityBoundary};
use crate::oplog::object_store::{BatchInspection, StoreError};
use crate::oplog::{
    decode_managed_local_record, AcceptedBatchEvent, BatchDisposition, BatchId, ContentDigest,
    DeviceId, LineageDigest, ManagedLocalJournalPayloadKind, ManagedLocalRecord, ManagedPath,
    ManifestObjectRef, ManifestProjectionTarget, ObjectStore, PageId, ProjectionEndpointBinding,
    ProjectionReceiptStore, ProjectionTurn, ProjectionTurnError, ProjectionTurnPayloadKind,
    ProjectionWork, ProjectionWorkTarget, RebuildSource, SequenceDomain, ShardedHotEngine,
    SqliteFrontier, TailOverlay, TurnOrigin, TurnPage, TurnPrecondition, TurnTarget, WorkspaceId,
    PROJECTION_TURN_DERIVATION_SCHEME_V1, PROJECTION_TURN_SCHEMA_VERSION,
};

const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
const ENGINE_STAGE_WORK_PER_RESUME: usize = 8;
const SQLITE_BATCHES_PER_RESUME: usize = 1;

/// Exact point answer derived from the already decoded pending local journal.
/// The index is rebuildable acceleration state; the journal and hot-prefix
/// admission remain the authority checked by the drain.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManagedLocalSuccessorObservation {
    pub(crate) latest_sequence: Option<u64>,
    pub(crate) current_matches_target: bool,
}

pub(crate) trait ManagedLocalSuccessorIndex {
    fn observe_successor(
        &self,
        path: &ManagedPath,
        page_id: PageId,
        after_sequence: u64,
        current: &[u8],
    ) -> ManagedLocalSuccessorObservation;
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ManagedLocalDrainStageTimings {
    pub(crate) authenticate: std::time::Duration,
    pub(crate) archive_publication: std::time::Duration,
    pub(crate) engine_acceptance: std::time::Duration,
    pub(crate) tail_and_sqlite: std::time::Duration,
    pub(crate) projection_adoption: std::time::Duration,
    pub(crate) authorship_receipt: std::time::Duration,
    pub(crate) provider_publication: std::time::Duration,
    pub(crate) checkpoint: std::time::Duration,
    pub(crate) total: std::time::Duration,
}

#[cfg(test)]
thread_local! {
    static LAST_DRAIN_STAGE_TIMINGS: std::cell::Cell<ManagedLocalDrainStageTimings> =
        const { std::cell::Cell::new(ManagedLocalDrainStageTimings {
            authenticate: std::time::Duration::ZERO,
            archive_publication: std::time::Duration::ZERO,
            engine_acceptance: std::time::Duration::ZERO,
            tail_and_sqlite: std::time::Duration::ZERO,
            projection_adoption: std::time::Duration::ZERO,
            authorship_receipt: std::time::Duration::ZERO,
            provider_publication: std::time::Duration::ZERO,
            checkpoint: std::time::Duration::ZERO,
            total: std::time::Duration::ZERO,
        }) };
}

#[cfg(test)]
pub(crate) fn last_managed_local_drain_stage_timings() -> ManagedLocalDrainStageTimings {
    LAST_DRAIN_STAGE_TIMINGS.get()
}

#[cfg(test)]
fn reset_managed_local_drain_stage_timings() {
    LAST_DRAIN_STAGE_TIMINGS.set(ManagedLocalDrainStageTimings::default());
}

#[cfg(test)]
fn note_managed_local_drain_stage(update: impl FnOnce(&mut ManagedLocalDrainStageTimings)) {
    LAST_DRAIN_STAGE_TIMINGS.with(|timings| {
        let mut current = timings.get();
        update(&mut current);
        timings.set(current);
    });
}

#[cfg(test)]
thread_local! {
    static DRAIN_FAULT: std::cell::Cell<Option<ManagedLocalDrainFaultPoint>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedLocalDrainFaultPoint {
    BeforeArchivePublication,
    AfterArchivePublication,
    BeforeEngineAcceptance,
    AfterEngineAcceptance,
    BeforeTailAdmission,
    AfterTailAdmission,
    BeforeSqliteCommit,
    AfterSqliteCommit,
    BeforeProjectionAdoption,
    AfterProjectionAdoption,
    BeforeAuthorship,
    AfterAuthorship,
    BeforeProvider,
    AfterProvider,
}

#[cfg(test)]
pub(crate) fn fail_managed_local_drain_once_at(point: ManagedLocalDrainFaultPoint) {
    DRAIN_FAULT.with(|fault| fault.set(Some(point)));
}

#[cfg(test)]
fn fault(point: ManagedLocalDrainFaultPoint) -> bool {
    DRAIN_FAULT.with(|fault| {
        if fault.get() == Some(point) {
            fault.set(None);
            true
        } else {
            false
        }
    })
}

#[cfg(not(test))]
fn fault(_point: ()) -> bool {
    false
}

macro_rules! drain_fault {
    ($point:ident) => {{
        #[cfg(test)]
        {
            fault(ManagedLocalDrainFaultPoint::$point)
        }
        #[cfg(not(test))]
        {
            fault(())
        }
    }};
}

/// One bounded durable/rebuildable stage of the exact-record continuation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManagedLocalDrainStage {
    Authenticate,
    ArchivePublication,
    EngineAcceptance,
    TailAndSqlite,
    ProjectionAdoption,
    AuthorshipReceipt,
    ProviderPublication,
    Checkpoint,
}

/// Advisory live-retry token. Every member is rederived from durable evidence;
/// losing this value only forfeits an acceleration hint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedLocalDrainContinuation {
    stage: ManagedLocalDrainStage,
    device_id: Uuid,
    sequence: u64,
    payload_digest: ContentDigest,
    batch_id: BatchId,
    detail: Option<String>,
}

impl ManagedLocalDrainContinuation {
    pub(crate) const fn stage(&self) -> ManagedLocalDrainStage {
        self.stage
    }
}

/// Canonical, checkpointable accepted derivative prefix for one device.
///
/// Persistence is owned by the later runtime lifecycle lane. A returned next
/// checkpoint is safe to publish atomically; no journal frame is reclaimable
/// merely because this in-memory value exists.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedLocalDrainCheckpoint {
    schema_version: u32,
    device_id: Uuid,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    next_sequence: u64,
    commitment: ContentDigest,
    accepted_frontier_digest: ContentDigest,
}

impl ManagedLocalDrainCheckpoint {
    pub(crate) fn initial(
        device_id: Uuid,
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
    ) -> Self {
        Self {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            device_id,
            workspace_id,
            lineage_digest,
            next_sequence: 0,
            commitment: ContentDigest::of(b"tine/managed-local-drain-prefix/empty/v1\0"),
            accepted_frontier_digest: ContentDigest::of(
                b"tine/managed-local-drain-frontier/empty/v1\0",
            ),
        }
    }

    pub(crate) const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    pub(crate) const fn device_id(&self) -> Uuid {
        self.device_id
    }

    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) const fn lineage_digest(&self) -> LineageDigest {
        self.lineage_digest
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, String> {
        postcard::to_allocvec(self).map_err(|error| error.to_string())
    }

    pub(crate) fn decode(
        bytes: &[u8],
        device_id: Uuid,
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
    ) -> Result<Self, String> {
        let checkpoint: Self = postcard::from_bytes(bytes).map_err(|error| error.to_string())?;
        if checkpoint.schema_version != CHECKPOINT_SCHEMA_VERSION
            || checkpoint.device_id != device_id
            || checkpoint.workspace_id != workspace_id
            || checkpoint.lineage_digest != lineage_digest
            || postcard::to_allocvec(&checkpoint).map_err(|error| error.to_string())? != bytes
        {
            return Err("managed-local drain checkpoint binding is invalid".into());
        }
        Ok(checkpoint)
    }
}

/// Exact accepted evidence supplied to the existing authorship/provider owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedLocalDerivativeAuthority {
    pub(crate) device_id: DeviceId,
    pub(crate) sequence: u64,
    pub(crate) batch_id: BatchId,
    pub(crate) manifest_digest: ContentDigest,
    pub(crate) accepted_frontier_digest: ContentDigest,
}

/// Result of idempotently authenticating/adopting one runtime-owned derivative.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManagedLocalPublicationState {
    Complete,
    Pending(String),
    Blocked(String),
    Conflict(String),
    RecoveryRequired(String),
}

/// Adapter implemented by the runtime owner of local-authorship receipts and
/// provider publication. Implementations must treat an exact pre-existing
/// state as `Complete` and must return `Conflict` without overwriting a
/// divergent authenticated winner.
pub(crate) trait ManagedLocalDerivativePublisher {
    fn ensure_local_authorship(
        &mut self,
        authority: &ManagedLocalDerivativeAuthority,
    ) -> ManagedLocalPublicationState;

    fn ensure_provider_publication(
        &mut self,
        authority: &ManagedLocalDerivativeAuthority,
    ) -> ManagedLocalPublicationState;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedLocalDrainCompletion {
    pub(crate) batch_id: BatchId,
    pub(crate) sequence: u64,
    /// Canonical proof the lifecycle lane may atomically checkpoint.
    pub(crate) checkpoint: ManagedLocalDrainCheckpoint,
    /// Inclusive journal sequence covered by `checkpoint` after persistence.
    pub(crate) reclaimable_through_after_checkpoint: u64,
    pub(crate) work: ManagedLocalDrainWork,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManagedLocalDrainWork {
    pub(crate) records: usize,
    pub(crate) graph_target_point_reads: usize,
    pub(crate) archive_objects: usize,
    pub(crate) engine_stage_work: usize,
    pub(crate) accepted_events: usize,
    pub(crate) sqlite_batches: usize,
    pub(crate) projection_work_point_reads: usize,
    pub(crate) authorship_attempts: usize,
    pub(crate) provider_attempts: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedLocalDrainBlock {
    pub(crate) stage: ManagedLocalDrainStage,
    pub(crate) missing_dependencies: Vec<BatchId>,
    pub(crate) detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedLocalDrainFailure {
    pub(crate) stage: ManagedLocalDrainStage,
    pub(crate) detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManagedLocalDrainOutcome {
    Complete(ManagedLocalDrainCompletion),
    Pending(ManagedLocalDrainContinuation),
    Blocked(ManagedLocalDrainBlock),
    Conflict(ManagedLocalDrainFailure),
    RecoveryRequired(ManagedLocalDrainFailure),
}

fn pending(
    stage: ManagedLocalDrainStage,
    frame: &LocalJournalFrame<ManagedLocalJournalPayloadKind>,
    record: &ManagedLocalRecord,
) -> ManagedLocalDrainOutcome {
    ManagedLocalDrainOutcome::Pending(ManagedLocalDrainContinuation {
        stage,
        device_id: frame.device_id(),
        sequence: frame.sequence(),
        payload_digest: frame.payload_digest(),
        batch_id: record.prepared_batch().manifest().batch_id(),
        detail: None,
    })
}

fn pending_with_detail(
    stage: ManagedLocalDrainStage,
    frame: &LocalJournalFrame<ManagedLocalJournalPayloadKind>,
    record: &ManagedLocalRecord,
    detail: impl Into<String>,
) -> ManagedLocalDrainOutcome {
    let ManagedLocalDrainOutcome::Pending(mut continuation) = pending(stage, frame, record) else {
        unreachable!()
    };
    continuation.detail = Some(detail.into());
    ManagedLocalDrainOutcome::Pending(continuation)
}

fn conflict(stage: ManagedLocalDrainStage, detail: impl Into<String>) -> ManagedLocalDrainOutcome {
    ManagedLocalDrainOutcome::Conflict(ManagedLocalDrainFailure {
        stage,
        detail: detail.into(),
    })
}

fn recovery(stage: ManagedLocalDrainStage, detail: impl Into<String>) -> ManagedLocalDrainOutcome {
    ManagedLocalDrainOutcome::RecoveryRequired(ManagedLocalDrainFailure {
        stage,
        detail: detail.into(),
    })
}

fn continuation_matches(
    continuation: &ManagedLocalDrainContinuation,
    frame: &LocalJournalFrame<ManagedLocalJournalPayloadKind>,
    record: &ManagedLocalRecord,
) -> bool {
    continuation.device_id == frame.device_id()
        && continuation.sequence == frame.sequence()
        && continuation.payload_digest == frame.payload_digest()
        && continuation.batch_id == record.prepared_batch().manifest().batch_id()
}

fn exact_archive_batch(record: &ManagedLocalRecord, archive: &ObjectStore) -> Result<bool, String> {
    let expected = record.prepared_batch();
    match archive
        .inspect_batch(expected.manifest().batch_id())
        .map_err(|error| error.to_string())?
    {
        BatchInspection::Absent | BatchInspection::Staged { .. } => Ok(false),
        BatchInspection::Ready(actual) => {
            let actual_manifest = actual
                .manifest()
                .encode()
                .map_err(|error| error.to_string())?;
            let expected_manifest = expected
                .manifest()
                .encode()
                .map_err(|error| error.to_string())?;
            let actual_objects = actual
                .objects()
                .iter()
                .map(|object| object.encode().map_err(|error| error.to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            let expected_objects = expected
                .objects()
                .iter()
                .map(|object| object.encode().map_err(|error| error.to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            if actual_manifest != expected_manifest || actual_objects != expected_objects {
                return Err("ready archive batch diverges from the exact journal objects".into());
            }
            Ok(true)
        }
    }
}

fn publication_conflict(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::ObjectCollision(_)
            | StoreError::BatchCollision(_)
            | StoreError::ObjectPathMismatch(_)
            | StoreError::ManifestPathMismatch { .. }
            | StoreError::WorkspaceMismatch { .. }
            | StoreError::LineageMismatch { .. }
            | StoreError::LineageClaimCollision(_)
            | StoreError::ImmutableCollision(_)
    )
}

fn checkpoint_advance(
    checkpoint: &ManagedLocalDrainCheckpoint,
    frame: &LocalJournalFrame<ManagedLocalJournalPayloadKind>,
    batch_id: BatchId,
    frontier: &AcceptedFrontierRoot,
) -> Result<ManagedLocalDrainCheckpoint, String> {
    let next_sequence = checkpoint
        .next_sequence
        .checked_add(1)
        .ok_or_else(|| "managed-local drain sequence overflow".to_owned())?;
    let accepted_frontier_digest = frontier.state_digest();
    let bytes = postcard::to_allocvec(&(
        b"tine/managed-local-drain-prefix/v1\0".as_slice(),
        checkpoint.commitment,
        checkpoint.device_id,
        frame.sequence(),
        frame.payload_digest(),
        batch_id,
        accepted_frontier_digest,
    ))
    .map_err(|error| error.to_string())?;
    Ok(ManagedLocalDrainCheckpoint {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        device_id: checkpoint.device_id,
        workspace_id: checkpoint.workspace_id,
        lineage_digest: checkpoint.lineage_digest,
        next_sequence,
        commitment: ContentDigest::of(&bytes),
        accepted_frontier_digest,
    })
}

/// Resume one foreground-journal record through the clean runtime's direct
/// SQLite frontier. The journal is the durable foreground boundary; this
/// continuation performs only rebuildable archive, SQLite, Markdown receipt,
/// and publication derivatives.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_clean_managed_local_journal_drain(
    admission: &LocalRuntimeAdmission<'_>,
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    engine: &mut ShardedHotEngine,
    database: &mut SqliteFrontier,
    frame: &LocalJournalFrame<ManagedLocalJournalPayloadKind>,
    superseding_projections: Option<&dyn ManagedLocalSuccessorIndex>,
    checkpoint: &ManagedLocalDrainCheckpoint,
    continuation: Option<&ManagedLocalDrainContinuation>,
    publisher: &mut impl ManagedLocalDerivativePublisher,
) -> ManagedLocalDrainOutcome {
    resume_managed_local_journal_drain_with_parts_and_superseding_projection(
        admission,
        graph,
        receipts,
        engine,
        database,
        ManagedLocalSqliteMode::Direct,
        frame,
        superseding_projections,
        checkpoint,
        continuation,
        publisher,
    )
}

enum ManagedLocalSqliteMode<'a> {
    Tail(&'a mut TailOverlay),
    Direct,
}

#[allow(clippy::too_many_arguments)]
fn resume_managed_local_journal_drain_with_parts_and_superseding_projection(
    admission: &LocalRuntimeAdmission<'_>,
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    engine: &mut ShardedHotEngine,
    database: &mut SqliteFrontier,
    sqlite_mode: ManagedLocalSqliteMode<'_>,
    frame: &LocalJournalFrame<ManagedLocalJournalPayloadKind>,
    superseding_projections: Option<&dyn ManagedLocalSuccessorIndex>,
    checkpoint: &ManagedLocalDrainCheckpoint,
    continuation: Option<&ManagedLocalDrainContinuation>,
    publisher: &mut impl ManagedLocalDerivativePublisher,
) -> ManagedLocalDrainOutcome {
    #[cfg(test)]
    reset_managed_local_drain_stage_timings();
    #[cfg(test)]
    let drain_started = std::time::Instant::now();
    #[cfg(test)]
    let mut stage_started = std::time::Instant::now();
    let record = match decode_managed_local_record(frame) {
        Ok(record) => record,
        Err(error) => return conflict(ManagedLocalDrainStage::Authenticate, error.to_string()),
    };
    let mut work_done = ManagedLocalDrainWork {
        records: 1,
        graph_target_point_reads: record.projections().len(),
        archive_objects: record.prepared_batch().objects().len(),
        ..ManagedLocalDrainWork::default()
    };
    let manifest = record.prepared_batch().manifest();
    if checkpoint.schema_version != CHECKPOINT_SCHEMA_VERSION
        || checkpoint.device_id != frame.device_id()
        || checkpoint.workspace_id != manifest.workspace_id()
        || checkpoint.lineage_digest != manifest.lineage_digest()
        || engine.workspace_id() != manifest.workspace_id()
        || engine.lineage_digest() != manifest.lineage_digest()
    {
        return conflict(
            ManagedLocalDrainStage::Authenticate,
            "journal/checkpoint/runtime binding mismatch",
        );
    }
    if frame.sequence() < checkpoint.next_sequence {
        return conflict(
            ManagedLocalDrainStage::Authenticate,
            "duplicate journal sequence is already checkpointed",
        );
    }
    if frame.sequence() > checkpoint.next_sequence {
        return ManagedLocalDrainOutcome::Blocked(ManagedLocalDrainBlock {
            stage: ManagedLocalDrainStage::Authenticate,
            missing_dependencies: Vec::new(),
            detail: format!(
                "journal sequence gap: expected {}, found {}",
                checkpoint.next_sequence,
                frame.sequence()
            ),
        });
    }
    if engine.managed_local_prefix_state().next_sequence <= frame.sequence() {
        return recovery(
            ManagedLocalDrainStage::Authenticate,
            "journal record is not present in the reconstructed committed hot prefix",
        );
    }
    if continuation.is_some_and(|token| !continuation_matches(token, frame, &record)) {
        return conflict(
            ManagedLocalDrainStage::Authenticate,
            "continuation belongs to another journal record",
        );
    }
    if let Err(error) = admission.authorize(graph, engine) {
        return recovery(ManagedLocalDrainStage::Authenticate, error.to_string());
    }
    let endpoint = match engine.projection_endpoint_binding() {
        Some(endpoint) => endpoint,
        None => {
            return recovery(
                ManagedLocalDrainStage::Authenticate,
                "missing endpoint binding",
            )
        }
    };
    if endpoint.device_id().as_uuid() != frame.device_id()
        || record
            .projections()
            .iter()
            .any(|projection| endpoint.endpoint_id() != projection.intent().source_endpoint_id())
        || receipts.workspace_id() != manifest.workspace_id()
        || receipts.endpoint_binding() != Some(endpoint)
        || !matches!(
            graph.canonical_resource_id(),
            Ok(resource) if resource == endpoint.graph_resource_id()
        )
    {
        return conflict(
            ManagedLocalDrainStage::Authenticate,
            "journal projection authority differs from the enrolled graph/receipt endpoint",
        );
    }
    let mut projection_superseded = BTreeSet::new();
    for projection in record.projections() {
        let intent = projection.intent();
        let exact_target = intent.target().bytes();
        let completed_creation =
            if exact_target.is_some() && projection.precondition_base().is_none() {
                let intent_id = match projection
                    .completion_intent()
                    .and_then(|intent| intent.id())
                {
                    Ok(intent_id) => intent_id,
                    Err(error) => {
                        return conflict(ManagedLocalDrainStage::Authenticate, error.to_string())
                    }
                };
                match engine.local_projection_completed(intent_id) {
                    Ok(completed) => completed,
                    Err(error) => {
                        return recovery(ManagedLocalDrainStage::Authenticate, error.to_string())
                    }
                }
            } else {
                false
            };
        let superseded = match graph.read_projection_input(intent.path()) {
            Ok(Some(current)) => {
                let current_matches_target = exact_target.is_some_and(|target| current == target);
                let current_matches_base = projection
                    .precondition_base()
                    .is_some_and(|base| current == base.bytes());
                let successor = superseding_projections
                    .map(|index| {
                        index.observe_successor(
                            intent.path(),
                            intent.page_id(),
                            frame.sequence(),
                            &current,
                        )
                    })
                    .unwrap_or_default();
                if successor.latest_sequence.is_some_and(|sequence| {
                    sequence >= engine.managed_local_prefix_state().next_sequence
                }) {
                    return conflict(
                        ManagedLocalDrainStage::Authenticate,
                        "superseding journal index extends beyond the committed local prefix",
                    );
                }
                if successor.latest_sequence.is_some()
                    && (successor.current_matches_target
                        || current_matches_target
                        || current_matches_base)
                {
                    true
                } else if current_matches_target || current_matches_base {
                    false
                } else {
                    return conflict(
                        ManagedLocalDrainStage::Authenticate,
                        "graph target is neither the journal base, target, nor an authoritative successor",
                    );
                }
            }
            Ok(None) if exact_target.is_none() => false,
            Ok(None) if graph.has_interrupted_publication_claimant(intent.path()) => {
                return conflict(
                    ManagedLocalDrainStage::Authenticate,
                    "an unresolved publication artifact still claims the absent journal target",
                )
            }
            // A creation-shaped foreground projection has no captured base.
            // Only the exact intent's completed local-half entry can prove it
            // ran before the file disappeared; otherwise this is its first
            // projection and must create as before.
            Ok(None) if projection.precondition_base().is_none() => completed_creation,
            // W4 has already reconciled publication artifacts before any
            // journal replay. A claimant-free absence is therefore a real
            // external deletion, not a torn W1 window. Accept this older batch
            // first, suppress its obsolete projection, and let the ordinary
            // clean watcher/full-scan import the deletion as the next batch.
            Ok(None) => true,
            Err(error) => return recovery(ManagedLocalDrainStage::Authenticate, error.to_string()),
        };
        if superseded {
            projection_superseded.insert(intent.path().clone());
        }
    }

    let archive = match engine
        .archive_store()
        .ok_or_else(|| "engine has no archive".to_owned())
        .and_then(|archive| {
            archive
                .duplicate_retained_capability()
                .map_err(|error| error.to_string())
        }) {
        Ok(archive) => archive,
        Err(error) => return recovery(ManagedLocalDrainStage::Authenticate, error),
    };
    #[cfg(test)]
    {
        note_managed_local_drain_stage(|timings| timings.authenticate = stage_started.elapsed());
        stage_started = std::time::Instant::now();
    }

    let direct_clean_runtime = matches!(&sqlite_mode, ManagedLocalSqliteMode::Direct);
    match exact_archive_batch(&record, &archive) {
        Ok(true) => {}
        Ok(false) => {
            if drain_fault!(BeforeArchivePublication) {
                return pending(ManagedLocalDrainStage::ArchivePublication, frame, &record);
            }
            if let Err(error) =
                admission.reprove_workspace_authority(WorkspaceAuthorityBoundary::Publication)
            {
                return recovery(
                    ManagedLocalDrainStage::ArchivePublication,
                    error.to_string(),
                );
            }
            if let Err(error) = archive.publish_turn_covered_prepared(record.prepared_batch()) {
                if publication_conflict(&error) {
                    return conflict(
                        ManagedLocalDrainStage::ArchivePublication,
                        error.to_string(),
                    );
                }
                return pending(ManagedLocalDrainStage::ArchivePublication, frame, &record);
            }
            match exact_archive_batch(&record, &archive) {
                Ok(true) => {}
                Ok(false) => {
                    return pending(ManagedLocalDrainStage::ArchivePublication, frame, &record)
                }
                Err(error) => return conflict(ManagedLocalDrainStage::ArchivePublication, error),
            }
            if drain_fault!(AfterArchivePublication) {
                return pending(ManagedLocalDrainStage::EngineAcceptance, frame, &record);
            }
        }
        Err(error) => return conflict(ManagedLocalDrainStage::ArchivePublication, error),
    }
    #[cfg(test)]
    {
        note_managed_local_drain_stage(|timings| {
            timings.archive_publication = stage_started.elapsed()
        });
        stage_started = std::time::Instant::now();
    }

    let batch_id = manifest.batch_id();
    let accepted = match engine.accepted_batch_is_active(batch_id) {
        Ok(accepted) => accepted,
        Err(error) => return recovery(ManagedLocalDrainStage::EngineAcceptance, error.to_string()),
    };
    if !accepted {
        if drain_fault!(BeforeEngineAcceptance) {
            return pending(ManagedLocalDrainStage::EngineAcceptance, frame, &record);
        }
        if let Err(error) =
            admission.reprove_workspace_authority(WorkspaceAuthorityBoundary::ArchiveStage)
        {
            return recovery(ManagedLocalDrainStage::EngineAcceptance, error.to_string());
        }
        let (disposition, has_more, stage_work) = if direct_clean_runtime {
            let claim_source = match database.materialized_read() {
                Ok(source) => source,
                Err(error) => {
                    return recovery(ManagedLocalDrainStage::EngineAcceptance, error.to_string())
                }
            };
            match engine.accept_clean_prepared_below_managed_local_overlay(
                record.prepared_batch(),
                &claim_source,
            ) {
                Ok(staged) => (staged.disposition().clone(), false, 1),
                Err(error) => {
                    return recovery(ManagedLocalDrainStage::EngineAcceptance, error.to_string())
                }
            }
        } else {
            match engine.stage_archive_batch_bounded_below_managed_local_overlay(
                batch_id,
                ENGINE_STAGE_WORK_PER_RESUME,
            ) {
                Ok(staged) => (
                    staged.outcome().disposition().clone(),
                    staged.has_more(),
                    staged.work(),
                ),
                Err(error) => {
                    return recovery(ManagedLocalDrainStage::EngineAcceptance, error.to_string())
                }
            }
        };
        work_done.engine_stage_work = stage_work;
        match disposition {
            BatchDisposition::Accepted { .. } | BatchDisposition::DuplicateAccepted { .. } => {}
            BatchDisposition::IncompleteStaged {
                missing_dependencies,
                ..
            } if !missing_dependencies.is_empty() => {
                return ManagedLocalDrainOutcome::Blocked(ManagedLocalDrainBlock {
                    stage: ManagedLocalDrainStage::EngineAcceptance,
                    missing_dependencies,
                    detail: "accepted-history dependencies are not available".into(),
                })
            }
            BatchDisposition::IncompleteStaged { .. } => {
                return pending(ManagedLocalDrainStage::EngineAcceptance, frame, &record)
            }
            BatchDisposition::Rejected { error } => {
                return conflict(ManagedLocalDrainStage::EngineAcceptance, error.to_string())
            }
            BatchDisposition::Quarantined => {
                return recovery(
                    ManagedLocalDrainStage::EngineAcceptance,
                    "accepted-history staging quarantined the journal batch",
                )
            }
        }
        if has_more {
            return pending(ManagedLocalDrainStage::EngineAcceptance, frame, &record);
        }
        if drain_fault!(AfterEngineAcceptance) {
            return pending(ManagedLocalDrainStage::TailAndSqlite, frame, &record);
        }
    }
    #[cfg(test)]
    {
        note_managed_local_drain_stage(|timings| {
            timings.engine_acceptance = stage_started.elapsed()
        });
        stage_started = std::time::Instant::now();
    }

    if drain_fault!(BeforeTailAdmission) {
        return pending(ManagedLocalDrainStage::TailAndSqlite, frame, &record);
    }
    if let Err(error) =
        admission.reprove_workspace_authority(WorkspaceAuthorityBoundary::TailAdmission)
    {
        return recovery(ManagedLocalDrainStage::TailAndSqlite, error.to_string());
    }
    let event_started = super::phase_trace_enabled().then(std::time::Instant::now);
    let event = match AcceptedBatchEvent::from_accepted(engine, &archive, batch_id) {
        Ok(event) => event,
        Err(error) => return recovery(ManagedLocalDrainStage::TailAndSqlite, error.to_string()),
    };
    if let Some(started) = event_started {
        eprintln!(
            "PHASE TIME ManagedLocal.accepted_event {:.3}ms",
            started.elapsed().as_secs_f64() * 1_000.0,
        );
    }
    work_done.accepted_events = 1;
    match sqlite_mode {
        ManagedLocalSqliteMode::Tail(tail) => {
            if let Err(error) = tail.try_enqueue(database, engine, &event) {
                return pending_with_detail(
                    ManagedLocalDrainStage::TailAndSqlite,
                    frame,
                    &record,
                    error.to_string(),
                );
            }
            if drain_fault!(AfterTailAdmission) {
                return pending(ManagedLocalDrainStage::TailAndSqlite, frame, &record);
            }
            if let Err(error) =
                admission.reprove_workspace_authority(WorkspaceAuthorityBoundary::SqliteDrain)
            {
                return recovery(ManagedLocalDrainStage::TailAndSqlite, error.to_string());
            }
            let source = match RebuildSource::new(engine, &archive) {
                Ok(source) => source,
                Err(error) => {
                    return recovery(ManagedLocalDrainStage::TailAndSqlite, error.to_string())
                }
            };
            if drain_fault!(BeforeSqliteCommit) {
                return pending(ManagedLocalDrainStage::TailAndSqlite, frame, &record);
            }
            work_done.sqlite_batches =
                match tail.drain_ready(database, &source, SQLITE_BATCHES_PER_RESUME) {
                    Ok(applied) => applied,
                    Err(error) => {
                        return pending_with_detail(
                            ManagedLocalDrainStage::TailAndSqlite,
                            frame,
                            &record,
                            error.to_string(),
                        )
                    }
                };
        }
        ManagedLocalSqliteMode::Direct => {
            if drain_fault!(AfterTailAdmission) {
                return pending(ManagedLocalDrainStage::TailAndSqlite, frame, &record);
            }
            if let Err(error) =
                admission.reprove_workspace_authority(WorkspaceAuthorityBoundary::SqliteDrain)
            {
                return recovery(ManagedLocalDrainStage::TailAndSqlite, error.to_string());
            }
            if drain_fault!(BeforeSqliteCommit) {
                return pending(ManagedLocalDrainStage::TailAndSqlite, frame, &record);
            }
            let frontier_started = super::phase_trace_enabled().then(std::time::Instant::now);
            let applied = match database.frontier_root() {
                Ok(frontier) => frontier,
                Err(error) => {
                    return recovery(ManagedLocalDrainStage::TailAndSqlite, error.to_string())
                }
            };
            if let Some(started) = frontier_started {
                eprintln!(
                    "PHASE TIME ManagedLocal.sqlite_frontier_before {:.3}ms",
                    started.elapsed().as_secs_f64() * 1_000.0,
                );
            }
            if applied.same_accepted_authority(event.prior_frontier_root()) {
                let apply_started = super::phase_trace_enabled().then(std::time::Instant::now);
                if let Err(error) = database.apply_engine_owned_accepted(&event, engine) {
                    return pending_with_detail(
                        ManagedLocalDrainStage::TailAndSqlite,
                        frame,
                        &record,
                        error.to_string(),
                    );
                }
                if let Some(started) = apply_started {
                    eprintln!(
                        "PHASE TIME ManagedLocal.sqlite_apply {:.3}ms",
                        started.elapsed().as_secs_f64() * 1_000.0,
                    );
                }
                work_done.sqlite_batches = 1;
            } else if !applied.same_accepted_authority(event.post_frontier_root()) {
                return recovery(
                    ManagedLocalDrainStage::TailAndSqlite,
                    "clean SQLite frontier is neither before nor after the journal batch",
                );
            }
        }
    }
    let verify_started = super::phase_trace_enabled().then(std::time::Instant::now);
    let accepted_frontier = match engine.accepted_frontier_root() {
        Ok(frontier) => frontier,
        Err(error) => return recovery(ManagedLocalDrainStage::TailAndSqlite, error.to_string()),
    };
    match database.frontier_root() {
        Ok(frontier) if frontier.same_accepted_authority(&accepted_frontier) => {}
        Ok(_) => return pending(ManagedLocalDrainStage::TailAndSqlite, frame, &record),
        Err(error) => return recovery(ManagedLocalDrainStage::TailAndSqlite, error.to_string()),
    }
    if let Some(started) = verify_started {
        eprintln!(
            "PHASE TIME ManagedLocal.sqlite_frontier_verify {:.3}ms",
            started.elapsed().as_secs_f64() * 1_000.0,
        );
    }
    if drain_fault!(AfterSqliteCommit) {
        return pending(ManagedLocalDrainStage::ProjectionAdoption, frame, &record);
    }
    #[cfg(test)]
    {
        note_managed_local_drain_stage(|timings| timings.tail_and_sqlite = stage_started.elapsed());
        stage_started = std::time::Instant::now();
    }

    if drain_fault!(BeforeProjectionAdoption) {
        return pending(ManagedLocalDrainStage::ProjectionAdoption, frame, &record);
    }
    if let Err(error) =
        admission.reprove_workspace_authority(WorkspaceAuthorityBoundary::ProjectionDrain)
    {
        return recovery(
            ManagedLocalDrainStage::ProjectionAdoption,
            error.to_string(),
        );
    }
    let mut turn = match projection_turn_from_managed_local_record(
        frame.device_id(),
        checkpoint.lineage_digest(),
        &record,
    ) {
        Ok(turn) => turn,
        Err(error) => {
            return conflict(
                ManagedLocalDrainStage::ProjectionAdoption,
                error.to_string(),
            )
        }
    };
    // A later authenticated foreground frame for this path is itself queued
    // and will replay as its own turn. The older record must not validate its
    // stale receipt precondition against the newer editor publication.
    turn.pages
        .retain(|page| !projection_superseded.contains(&page.path));
    if let Err(error) = crate::oplog::projection::replay_projection_turn(
        graph, receipts, engine, database, &turn, None,
    ) {
        let conflicts = record.projections().iter().any(|projection| {
            let intent = projection.intent();
            let exact_target = intent.target().bytes();
            matches!(
                graph.read_projection_input(intent.path()),
                Ok(Some(bytes))
                    if exact_target.is_none_or(|target| bytes != target)
                        && projection
                            .precondition_base()
                            .is_none_or(|base| bytes != base.bytes())
            )
        });
        return if conflicts {
            conflict(
                ManagedLocalDrainStage::ProjectionAdoption,
                error.to_string(),
            )
        } else {
            pending_with_detail(
                ManagedLocalDrainStage::ProjectionAdoption,
                frame,
                &record,
                error.to_string(),
            )
        };
    }
    work_done.projection_work_point_reads = record.projections().len();
    if drain_fault!(AfterProjectionAdoption) {
        return pending(ManagedLocalDrainStage::AuthorshipReceipt, frame, &record);
    }
    #[cfg(test)]
    {
        note_managed_local_drain_stage(|timings| {
            timings.projection_adoption = stage_started.elapsed()
        });
        stage_started = std::time::Instant::now();
    }

    let manifest_digest = match manifest.encode() {
        Ok(bytes) => ContentDigest::of(&bytes),
        Err(error) => return conflict(ManagedLocalDrainStage::Authenticate, error.to_string()),
    };
    let authority = ManagedLocalDerivativeAuthority {
        device_id: manifest.author_device_id(),
        sequence: frame.sequence(),
        batch_id,
        manifest_digest,
        accepted_frontier_digest: accepted_frontier.state_digest(),
    };
    if drain_fault!(BeforeAuthorship) {
        return pending(ManagedLocalDrainStage::AuthorshipReceipt, frame, &record);
    }
    work_done.authorship_attempts = 1;
    match publisher.ensure_local_authorship(&authority) {
        ManagedLocalPublicationState::Complete => {}
        ManagedLocalPublicationState::Pending(_) => {
            return pending(ManagedLocalDrainStage::AuthorshipReceipt, frame, &record)
        }
        ManagedLocalPublicationState::Blocked(detail) => {
            return ManagedLocalDrainOutcome::Blocked(ManagedLocalDrainBlock {
                stage: ManagedLocalDrainStage::AuthorshipReceipt,
                missing_dependencies: Vec::new(),
                detail,
            })
        }
        ManagedLocalPublicationState::Conflict(detail) => {
            return conflict(ManagedLocalDrainStage::AuthorshipReceipt, detail)
        }
        ManagedLocalPublicationState::RecoveryRequired(detail) => {
            return recovery(ManagedLocalDrainStage::AuthorshipReceipt, detail)
        }
    }
    if drain_fault!(AfterAuthorship) {
        return pending(ManagedLocalDrainStage::ProviderPublication, frame, &record);
    }
    #[cfg(test)]
    {
        note_managed_local_drain_stage(|timings| {
            timings.authorship_receipt = stage_started.elapsed()
        });
        stage_started = std::time::Instant::now();
    }
    if drain_fault!(BeforeProvider) {
        return pending(ManagedLocalDrainStage::ProviderPublication, frame, &record);
    }
    work_done.provider_attempts = 1;
    match publisher.ensure_provider_publication(&authority) {
        ManagedLocalPublicationState::Complete => {}
        ManagedLocalPublicationState::Pending(_) => {
            return pending(ManagedLocalDrainStage::ProviderPublication, frame, &record)
        }
        ManagedLocalPublicationState::Blocked(detail) => {
            return ManagedLocalDrainOutcome::Blocked(ManagedLocalDrainBlock {
                stage: ManagedLocalDrainStage::ProviderPublication,
                missing_dependencies: Vec::new(),
                detail,
            })
        }
        ManagedLocalPublicationState::Conflict(detail) => {
            return conflict(ManagedLocalDrainStage::ProviderPublication, detail)
        }
        ManagedLocalPublicationState::RecoveryRequired(detail) => {
            return recovery(ManagedLocalDrainStage::ProviderPublication, detail)
        }
    }
    if drain_fault!(AfterProvider) {
        return pending(ManagedLocalDrainStage::Checkpoint, frame, &record);
    }
    #[cfg(test)]
    {
        note_managed_local_drain_stage(|timings| {
            timings.provider_publication = stage_started.elapsed()
        });
        stage_started = std::time::Instant::now();
    }

    let next_checkpoint = match checkpoint_advance(checkpoint, frame, batch_id, &accepted_frontier)
    {
        Ok(checkpoint) => checkpoint,
        Err(error) => return recovery(ManagedLocalDrainStage::Checkpoint, error),
    };
    let outcome = ManagedLocalDrainOutcome::Complete(ManagedLocalDrainCompletion {
        batch_id,
        sequence: frame.sequence(),
        checkpoint: next_checkpoint,
        reclaimable_through_after_checkpoint: frame.sequence(),
        work: work_done,
    });
    #[cfg(test)]
    note_managed_local_drain_stage(|timings| {
        timings.checkpoint = stage_started.elapsed();
        timings.total = drain_started.elapsed();
    });
    outcome
}

// ---------------------------------------------------------------------------
// Torn versus corrupt: the WAL rule (journal-universal durability design §3.4).
//
// The discriminator is NOT invented here. `LocalJournalSegmentV2` maintains a
// durable frontier: append file-flushes the frame and durably publishes the
// successor frontier before returning, open validates every byte inside the
// committed frontier and truncates only bytes beyond it. So:
//
// * bytes BEYOND the durable frontier -- the append never returned. By the
//   turn-before-mutation invariant, no graph mutation for those bytes can have
//   started. The segment truncates them; nothing is owed; there is no residue
//   probe because none is needed.
// * any invalid frame AT OR BELOW the frontier, tail or interior -- a disk or
//   media error damaged an authoritative record whose effects may exist.
//   Refuse activation, retain the segment bytes as evidence, report the
//   component. Never truncate, never skip.
//
// The first case never reaches this module: the segment handles it silently and
// reports it as `discarded_tail_bytes`. The second arrives as an open error,
// and mapping every open error to one code would be wrong -- `open_selected`
// also reports an honest concurrent instance and an unsafe filesystem, whose
// in-scope scenarios are different and whose refusals already exist. The
// mapping below is therefore per-variant, and each arm names its scenario.
// ---------------------------------------------------------------------------

/// A local-journal segment that could not be opened, classified by the in-scope
/// failure it actually names (the refusal-scenario rule).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LocalJournalOpenRefusal {
    /// `MS-REF-DISK-CORRUPT`. In-scope scenario: a disk/media error damaged an
    /// authoritative record inside the durable frontier, whose graph effects
    /// may already exist. Retain the segment bytes as evidence; never truncate.
    DiskCorrupt(String),
    /// In-scope scenario: an honest second Tine instance holds the segment.
    /// The existing concurrent-instance refusal applies; nothing is corrupt.
    ConcurrentInstance(String),
    /// In-scope scenario: the journal namespace contains an entry this
    /// platform cannot safely open (symlink, non-regular file, unsafe name).
    UnsafeFilesystem(String),
    /// In-scope scenario: a transient I/O or capability failure. Retryable;
    /// asserts nothing about the record's integrity.
    Unavailable(String),
}

impl LocalJournalOpenRefusal {
    /// The contract refusal code this classification maps to, or `None` where
    /// the existing (non-corruption) refusal owns the message.
    #[cfg(test)]
    pub(crate) const fn refusal_code(&self) -> Option<&'static str> {
        match self {
            Self::DiskCorrupt(_) => Some("MS-REF-DISK-CORRUPT"),
            Self::ConcurrentInstance(_) | Self::UnsafeFilesystem(_) | Self::Unavailable(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) const fn retains_evidence(&self) -> bool {
        matches!(self, Self::DiskCorrupt(_))
    }

    #[cfg(test)]
    pub(crate) fn detail(&self) -> &str {
        match self {
            Self::DiskCorrupt(detail)
            | Self::ConcurrentInstance(detail)
            | Self::UnsafeFilesystem(detail)
            | Self::Unavailable(detail) => detail,
        }
    }
}

impl std::fmt::Display for LocalJournalOpenRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DiskCorrupt(detail) => write!(
                formatter,
                "MS-REF-DISK-CORRUPT: an authoritative journal record inside the durable \
                 frontier is damaged and its bytes are retained as evidence: {detail}"
            ),
            Self::ConcurrentInstance(detail) => write!(
                formatter,
                "another Tine instance already holds this local journal segment: {detail}"
            ),
            Self::UnsafeFilesystem(detail) => write!(
                formatter,
                "the local journal namespace contains an entry that cannot be safely opened: \
                 {detail}"
            ),
            Self::Unavailable(detail) => {
                write!(formatter, "the local journal is unavailable: {detail}")
            }
        }
    }
}

/// Per-variant mapping of a `LocalJournalSegmentV2::open_selected` failure.
///
/// This deliberately does not collapse to "any open error is corruption": a
/// check with the wrong scenario is a future availability bug, not hardening.
pub(crate) fn classify_local_journal_open_error(
    error: &LocalJournalError,
) -> LocalJournalOpenRefusal {
    let detail = error.to_string();
    match error {
        // Every one of these is reported by the committed-prefix scan, i.e.
        // strictly at or below the durable frontier. Bytes beyond the frontier
        // are truncated by the segment and never surface as an error at all.
        LocalJournalError::CorruptSegment { .. }
        | LocalJournalError::SegmentDeviceMismatch { .. }
        | LocalJournalError::SegmentSequenceGap { .. }
        | LocalJournalError::ChecksumMismatch
        | LocalJournalError::PayloadDigestMismatch
        | LocalJournalError::InvalidFrameMagic
        | LocalJournalError::NonCanonicalFrameHeader
        | LocalJournalError::TruncatedFrame
        | LocalJournalError::FrameLengthMismatch { .. }
        | LocalJournalError::LengthOverflow
        | LocalJournalError::FrameTooLarge(_)
        | LocalJournalError::FrameHeaderTooLarge(_)
        | LocalJournalError::SegmentTooLarge(_)
        | LocalJournalError::UnknownFrameSchemaVersion { .. }
        | LocalJournalError::AmbiguousLegacySuffix { .. }
        | LocalJournalError::Decode(_)
        | LocalJournalError::SequenceExhausted => LocalJournalOpenRefusal::DiskCorrupt(detail),
        LocalJournalError::SegmentAlreadyOpen(_) | LocalJournalError::PreparedArtifactExists(_) => {
            LocalJournalOpenRefusal::ConcurrentInstance(detail)
        }
        LocalJournalError::UnsafeSegmentName(_)
        | LocalJournalError::UnsupportedDurableReplacement => {
            LocalJournalOpenRefusal::UnsafeFilesystem(detail)
        }
        LocalJournalError::Io(_)
        | LocalJournalError::Encode(_)
        | LocalJournalError::SegmentPoisoned => LocalJournalOpenRefusal::Unavailable(detail),
    }
}

/// Decode a projection-turn frame.
///
/// One of the two producers of the single [`ProjectionTurn`] view; the sibling
/// below reads a managed-local frame. Recovery therefore has one record shape
/// to reason about regardless of which physical segment carried it.
pub(crate) fn decode_projection_turn_frame(
    frame: &LocalJournalFrame<ProjectionTurnPayloadKind>,
) -> Result<ProjectionTurn, ProjectionTurnError> {
    if frame.payload_kind() != ProjectionTurnPayloadKind::TurnV1 {
        return Err(ProjectionTurnError::CorruptPayload(
            "unknown projection turn payload kind".into(),
        ));
    }
    ProjectionTurn::decode(frame.payload(), frame.device_id(), frame.sequence())
}

/// Project one already-authenticated managed-local frame into the same
/// [`ProjectionTurn`] view.
///
/// For A1/A2 this is a re-shape of material the frame already carries
/// (`ManagedLocalProjection::precondition_base`/`render_base`), not a size
/// increase, and it is why the foreground journal needs no second record kind.
/// `lineage_digest` is a binding input for the same reason the drain
/// checkpoint takes one: the frame does not carry it.
#[cfg(test)]
pub(crate) fn projection_turn_from_managed_local_frame(
    frame: &LocalJournalFrame<ManagedLocalJournalPayloadKind>,
    lineage_digest: LineageDigest,
) -> Result<ProjectionTurn, ProjectionTurnError> {
    let record = decode_managed_local_record(frame)
        .map_err(|error| ProjectionTurnError::CorruptPayload(error.to_string()))?;
    projection_turn_from_managed_local_record(frame.device_id(), lineage_digest, &record)
}

pub(crate) fn projection_turn_from_managed_local_record(
    device_id: Uuid,
    lineage_digest: LineageDigest,
    record: &ManagedLocalRecord,
) -> Result<ProjectionTurn, ProjectionTurnError> {
    let first = record.projections().first().ok_or_else(|| {
        ProjectionTurnError::CorruptPayload(
            "a managed-local record retains at least one projection".into(),
        )
    })?;
    let workspace_id = first.intent().workspace_id();
    let endpoint_id = first.intent().source_endpoint_id();
    let mut pages = Vec::with_capacity(record.projections().len());
    for projection in record.projections() {
        let intent = projection.intent();
        if intent.workspace_id() != workspace_id || intent.source_endpoint_id() != endpoint_id {
            return Err(ProjectionTurnError::CorruptPayload(
                "one managed-local record spans two workspaces or endpoints".into(),
            ));
        }
        let precondition = match projection.precondition_base() {
            None => TurnPrecondition::Absent,
            Some(base) => TurnPrecondition::Base {
                description: base.description(),
                bytes: Some(base.bytes().to_vec()),
                annotations: base.annotations().to_vec(),
            },
        };
        let target = match intent.target() {
            ManifestProjectionTarget::Absent => TurnTarget::Absent,
            ManifestProjectionTarget::Present {
                description,
                bytes,
                annotations,
            } => TurnTarget::Present {
                description: *description,
                bytes: Some(bytes.clone()),
                annotations: annotations.clone(),
            },
        };
        pages.push(TurnPage {
            page_id: intent.page_id(),
            path: intent.path().clone(),
            precondition,
            target,
            frontier: intent.post_frontier().clone(),
            claim_evidence: intent.claim_evidence().to_vec(),
        });
    }
    let turn = ProjectionTurn {
        schema_version: PROJECTION_TURN_SCHEMA_VERSION,
        derivation_scheme: PROJECTION_TURN_DERIVATION_SCHEME_V1,
        workspace_id,
        lineage_digest,
        device_id,
        endpoint_id,
        sequence: record.sequence(),
        domain: SequenceDomain::ManagedLocal,
        origin: TurnOrigin::LocalBatch {
            batch_id: record.prepared_batch().manifest().batch_id(),
        },
        pages,
    };
    // Encoding revalidates every structural invariant of the record.
    turn.encode()?;
    Ok(turn)
}
