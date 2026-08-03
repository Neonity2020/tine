//! Restartable expansion of one authoritative managed-local journal record.
//!
//! The foreground journal record and its already-durable exact graph target
//! are authoritative on entry. This module never drafts, appends, allocates an
//! identity, or writes graph text. It only resumes the established immutable
//! archive, accepted-history, tail/SQLite, projection-receipt, authorship, and
//! provider derivatives of that exact record.

use serde::{Deserialize, Serialize};
use tine_storage::LocalJournalFrame;
use uuid::Uuid;

use crate::model::Graph;
use crate::oplog::batch::ObjectKind;
use crate::oplog::hot_engine::AcceptedFrontierRoot;
use crate::oplog::local_active::{
    LocalRuntimeAdmission, PromotedRuntimeSession, WorkspaceAuthorityBoundary,
};
use crate::oplog::object_store::{BatchInspection, StoreError};
use crate::oplog::projection_work_index::{
    ProjectionWork, ProjectionWorkStatus, ProjectionWorkTarget,
};
use crate::oplog::{
    decode_managed_local_record, AcceptedBatchEvent, BatchDisposition, BatchId, ContentDigest,
    DeviceId, LineageDigest, ManagedLocalJournalPayloadKind, ManagedLocalRecord, ManifestObjectRef,
    ManifestProjectionTarget, ObjectStore, ProjectionEndpointBinding, ProjectionReceiptStore,
    RebuildSource, ShardedHotEngine, SqliteFrontier, TailOverlay, WorkspaceId,
};

const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
const ENGINE_STAGE_WORK_PER_RESUME: usize = 8;
const SQLITE_BATCHES_PER_RESUME: usize = 1;

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

    pub(crate) const fn commitment(&self) -> ContentDigest {
        self.commitment
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

fn exact_work(
    record: &ManagedLocalRecord,
    endpoint: ProjectionEndpointBinding,
) -> Result<ProjectionWork, String> {
    let batch = record.prepared_batch();
    let intent = record.projection().intent();
    let descriptor = batch
        .manifest()
        .required_objects()
        .iter()
        .find(|descriptor| {
            descriptor.kind() == ObjectKind::ProjectionIntent
                && descriptor.document_id() == intent.descriptor_document_id()
        })
        .ok_or_else(|| "journal projection descriptor is absent".to_owned())?;
    let target = intent
        .target()
        .description()
        .map_or(ProjectionWorkTarget::Absent, ProjectionWorkTarget::Present);
    Ok(ProjectionWork::new(
        batch.manifest().workspace_id(),
        endpoint.endpoint_id(),
        endpoint.graph_resource_id(),
        batch.manifest().batch_id(),
        intent.page_id(),
        intent.path().clone(),
        intent.portable_path_index_root(),
        ManifestObjectRef::from_descriptor(descriptor),
        intent.post_frontier().clone(),
        target,
    ))
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

/// Production facade for one promoted runtime window.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_managed_local_journal_drain(
    session: &mut PromotedRuntimeSession<'_>,
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    frame: &LocalJournalFrame<ManagedLocalJournalPayloadKind>,
    checkpoint: &ManagedLocalDrainCheckpoint,
    continuation: Option<&ManagedLocalDrainContinuation>,
    publisher: &mut impl ManagedLocalDerivativePublisher,
) -> ManagedLocalDrainOutcome {
    let (admission, engine, database, tail) = match session.parts() {
        Ok(parts) => parts,
        Err(error) => return recovery(ManagedLocalDrainStage::Authenticate, error.to_string()),
    };
    resume_managed_local_journal_drain_with_parts(
        &admission,
        graph,
        receipts,
        engine,
        database,
        tail,
        frame,
        checkpoint,
        continuation,
        publisher,
    )
}

/// The same boundary with explicit promoted parts, retained for deterministic
/// semantic/failure tests. Production callers use the session facade above.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_managed_local_journal_drain_with_parts(
    admission: &LocalRuntimeAdmission<'_>,
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    engine: &mut ShardedHotEngine,
    database: &mut SqliteFrontier,
    tail: &mut TailOverlay,
    frame: &LocalJournalFrame<ManagedLocalJournalPayloadKind>,
    checkpoint: &ManagedLocalDrainCheckpoint,
    continuation: Option<&ManagedLocalDrainContinuation>,
    publisher: &mut impl ManagedLocalDerivativePublisher,
) -> ManagedLocalDrainOutcome {
    let record = match decode_managed_local_record(frame) {
        Ok(record) => record,
        Err(error) => return conflict(ManagedLocalDrainStage::Authenticate, error.to_string()),
    };
    let mut work_done = ManagedLocalDrainWork {
        records: 1,
        graph_target_point_reads: 1,
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
        || endpoint.endpoint_id() != record.projection().intent().source_endpoint_id()
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
    let intent = record.projection().intent();
    let exact_target = match intent.target() {
        ManifestProjectionTarget::Present { bytes, .. } => bytes.as_slice(),
        ManifestProjectionTarget::Absent => {
            return conflict(
                ManagedLocalDrainStage::Authenticate,
                "managed-local drain does not accept deletion targets",
            )
        }
    };
    match graph.read_projection_input(intent.path()) {
        Ok(Some(current)) if current == exact_target => {}
        Ok(_) => {
            return conflict(
                ManagedLocalDrainStage::Authenticate,
                "graph target is not the exact journal-authorized bytes",
            )
        }
        Err(error) => return recovery(ManagedLocalDrainStage::Authenticate, error.to_string()),
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
            if let Err(error) = archive.publish_prepared(record.prepared_batch()) {
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
        let staged =
            match engine.stage_archive_batch_bounded(batch_id, ENGINE_STAGE_WORK_PER_RESUME) {
                Ok(staged) => staged,
                Err(error) => {
                    return recovery(ManagedLocalDrainStage::EngineAcceptance, error.to_string())
                }
            };
        work_done.engine_stage_work = staged.work();
        match staged.outcome().disposition() {
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
        if staged.has_more() {
            return pending(ManagedLocalDrainStage::EngineAcceptance, frame, &record);
        }
        if drain_fault!(AfterEngineAcceptance) {
            return pending(ManagedLocalDrainStage::TailAndSqlite, frame, &record);
        }
    }

    if drain_fault!(BeforeTailAdmission) {
        return pending(ManagedLocalDrainStage::TailAndSqlite, frame, &record);
    }
    if let Err(error) =
        admission.reprove_workspace_authority(WorkspaceAuthorityBoundary::TailAdmission)
    {
        return recovery(ManagedLocalDrainStage::TailAndSqlite, error.to_string());
    }
    let event = match AcceptedBatchEvent::from_accepted(engine, &archive, batch_id) {
        Ok(event) => event,
        Err(error) => return recovery(ManagedLocalDrainStage::TailAndSqlite, error.to_string()),
    };
    work_done.accepted_events = 1;
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
        Err(error) => return recovery(ManagedLocalDrainStage::TailAndSqlite, error.to_string()),
    };
    if drain_fault!(BeforeSqliteCommit) {
        return pending(ManagedLocalDrainStage::TailAndSqlite, frame, &record);
    }
    work_done.sqlite_batches = match tail.drain_ready(database, &source, SQLITE_BATCHES_PER_RESUME)
    {
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
    let accepted_frontier = match engine.accepted_frontier_root() {
        Ok(frontier) => frontier,
        Err(error) => return recovery(ManagedLocalDrainStage::TailAndSqlite, error.to_string()),
    };
    match database.frontier_root() {
        Ok(frontier) if frontier.same_accepted_authority(&accepted_frontier) => {}
        Ok(_) => return pending(ManagedLocalDrainStage::TailAndSqlite, frame, &record),
        Err(error) => return recovery(ManagedLocalDrainStage::TailAndSqlite, error.to_string()),
    }
    if drain_fault!(AfterSqliteCommit) {
        return pending(ManagedLocalDrainStage::ProjectionAdoption, frame, &record);
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
    let expected_work = match exact_work(&record, endpoint) {
        Ok(work) => work,
        Err(error) => return conflict(ManagedLocalDrainStage::ProjectionAdoption, error),
    };
    let work_index = match engine.projection_work_index() {
        Ok(index) => index,
        Err(error) => {
            return recovery(
                ManagedLocalDrainStage::ProjectionAdoption,
                error.to_string(),
            )
        }
    };
    // Read the accepted point row rather than enumerating ready work. The row
    // was prepared directly from this archived manifested intent and the
    // executor below reauthenticates that binding against both authorities.
    let work = match work_index.get(expected_work.work_id()) {
        Ok(Some(work)) => work,
        Ok(None) => {
            return recovery(
                ManagedLocalDrainStage::ProjectionAdoption,
                "journal projection work point row is absent",
            )
        }
        Err(error) => {
            return recovery(
                ManagedLocalDrainStage::ProjectionAdoption,
                error.to_string(),
            )
        }
    };
    work_done.projection_work_point_reads = 1;
    match work_index.status(work.work_id()) {
        Ok(Some(ProjectionWorkStatus::Ready | ProjectionWorkStatus::Completed)) => {}
        Ok(Some(ProjectionWorkStatus::Blocked | ProjectionWorkStatus::Superseded { .. })) => {
            return conflict(
                ManagedLocalDrainStage::ProjectionAdoption,
                "journal projection work has divergent terminal state",
            )
        }
        Ok(_) => {
            return recovery(
                ManagedLocalDrainStage::ProjectionAdoption,
                "journal projection work is absent or not accepted",
            )
        }
        Err(error) => {
            return recovery(
                ManagedLocalDrainStage::ProjectionAdoption,
                error.to_string(),
            )
        }
    }
    if let Err(error) =
        crate::oplog::projection::execute_manifested_projection_work(graph, receipts, engine, &work)
    {
        let current = graph.read_projection_input(intent.path());
        return if matches!(current, Ok(Some(bytes)) if bytes != exact_target) {
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
    if drain_fault!(AfterProjectionAdoption) {
        return pending(ManagedLocalDrainStage::AuthorshipReceipt, frame, &record);
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

    let next_checkpoint = match checkpoint_advance(checkpoint, frame, batch_id, &accepted_frontier)
    {
        Ok(checkpoint) => checkpoint,
        Err(error) => return recovery(ManagedLocalDrainStage::Checkpoint, error),
    };
    ManagedLocalDrainOutcome::Complete(ManagedLocalDrainCompletion {
        batch_id,
        sequence: frame.sequence(),
        checkpoint: next_checkpoint,
        reclaimable_through_after_checkpoint: frame.sequence(),
        work: work_done,
    })
}
