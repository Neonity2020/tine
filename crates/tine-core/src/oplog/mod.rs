//! Production storage substrate for Tine's opt-in managed-storage runtime.
//!
//! Enrollment, activation, mutation, projection, reconciliation, SQLite
//! materialization, and shared-provider synchronization are composed by
//! `crate::sync_runtime`. Direct Files does not enter this module tree: the
//! application selects that mutually exclusive runtime before opening a graph.
//! Immutable operation/object bytes are authoritative; SQLite, scratch, and
//! projection-work state are disposable derived data.

pub mod batch;
#[allow(dead_code)] // mixed live/runtime and retained format-decoder surface
pub(crate) mod bootstrap_import;
pub(crate) mod causal_index;
pub(crate) mod content_patricia;
pub(crate) mod dependency_queue;
#[allow(dead_code)] // mixed live/runtime and diagnostic surface
pub(crate) mod discovery;
pub(crate) mod document_state;
#[allow(dead_code)] // mixed live/runtime and retained compatibility surface
pub(crate) mod enrollment;
// The only keyed enrollment compatibility code.  Current enrollment state is
// integrity-checked, while immutable v1/v5 history remains verifiable.
pub(crate) mod enrollment_legacy_hmac;
pub(crate) use enrollment_legacy_hmac as legacy_enrollment_verifier;
pub(crate) mod evidence_index;
#[allow(dead_code)] // mixed live/runtime and test instrumentation surface
pub(crate) mod exact_external_feed;
pub(crate) mod external_import;
pub mod hot_engine;
#[cfg(test)]
mod hot_engine_integration_tests;
pub mod identity;
pub mod import;
#[allow(dead_code)] // mixed live/runtime and recovery/test surface
pub(crate) mod local_active;
#[allow(dead_code)] // mixed live/runtime and recovery/test surface
pub(crate) mod local_journal_drain;
#[allow(dead_code)] // mixed live/runtime and retained format-decoder surface
pub(crate) mod local_journal_v2_anchor;
pub(crate) mod loro_store;
#[allow(dead_code)] // mixed live/runtime and recovery/test surface
pub(crate) mod migration_backup;
pub mod object_store;
pub(crate) mod operational_coordinator;
#[allow(dead_code)] // mixed live/runtime and test-construction surface
pub(crate) mod page_name_index;
pub(crate) mod portable_path_index;
pub mod projection;
pub mod projection_manifest;
pub mod projection_store;
pub mod projection_work_index;
pub mod receipt;
#[allow(dead_code)] // mixed live/runtime and diagnostic/test surface
pub(crate) mod reconciliation_baseline;
#[allow(dead_code)] // mixed live/runtime and test-composition surface
pub(crate) mod reconciliation_baseline_adapter;
#[allow(dead_code)] // mixed live/runtime and test-composition surface
pub(crate) mod reconciliation_import;
#[allow(dead_code)] // mixed live/runtime and test instrumentation surface
pub(crate) mod reconciliation_scan;
#[allow(dead_code)] // mixed live/runtime and test-composition surface
pub(crate) mod reconciliation_session;
pub mod reference_catalog;
#[allow(dead_code)] // retained recovery format plus test construction surface
pub(crate) mod resume_point;
pub(crate) mod scratch_store;
pub mod semantic;
#[allow(dead_code)] // mixed live/runtime and proof/test surface
pub(crate) mod shadow_projection;
pub mod simulator;
pub mod sqlite;
pub mod sqlite_materialization;
pub mod sync_layout;
#[allow(dead_code)] // mixed live/runtime and recovery/test surface
pub(crate) mod trusted_local_commit;
pub(crate) mod uuid_claim_index;
#[allow(dead_code)] // mixed live/runtime and test instrumentation surface
pub(crate) mod watcher_queue;
pub(crate) mod wire;

/// Diagnostic trace gates, resolved once per process.
///
/// `std::env::var_os` walks the whole environment on every call, and these gates
/// sit on per-document and per-batch paths. Resolving them once keeps the
/// instrumentation free when it is off, which is the only way it is acceptable
/// on a hot path at all. The trade is that toggling mid-process does nothing --
/// correct for a diagnostic, wrong for a feature flag, so do not reuse these for
/// behaviour.
pub(crate) fn phase_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("TINE_PHASE_TRACE").is_some())
}

/// Per-call-site `inspect_batch` attribution. Off, it costs one atomic load;
/// on, it formats a `file:line` key and takes a global lock per call.
pub(crate) fn inspect_site_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("TINE_INSPECT_SITES").is_some())
}

pub use crate::graph_text_scope::{
    GraphTextScopeBinding, GraphTextScopeBindingError, GRAPH_TEXT_SCOPE_BINDING_SCHEMA_VERSION,
    GRAPH_TEXT_SCOPE_VERSION,
};
pub use batch::{
    BatchCausalDot, BatchError, BatchOrigin, CausalPeerId, ContentDigest, LineageDigest,
    ObjectDescriptor, ObjectKind, OperationBatch, OperationObject, PreparedBatch,
    SemanticEffectDigest, ValidatedBatch, MANIFEST_ENCODING_VERSION, MAX_MANIFEST_BYTES,
    MAX_OBJECT_BYTES, OBJECT_ENVELOPE_SCHEMA_VERSION, OPERATION_SCHEMA_VERSION,
    OPLOG_PROTOCOL_VERSION,
};
pub(crate) use hot_engine::{
    append_managed_local_record, ManagedLocalAppendError, ManagedLocalAppendProof,
    ManagedLocalJournalAppend, ProjectionTombstoneAuthorization,
};
pub use hot_engine::{
    decode_managed_local_record, AcceptedBatch, AcceptedBatchEvidence, AuthorBatch,
    AuthorTransactionDraft, BatchDisposition, BlockContentRewrite, BlockLocation,
    CapabilityCapturedProjectionInput, CapabilityCapturedProjectionState, CurrentPageAtPath,
    EngineError, EngineInstrumentation, EngineStatus, FatalEvidenceHandle, ImmutableHomeClaim,
    ImmutableHomeConflict, ImmutableHomeEvidence, LogseqIdentityMutation, LogseqIdentityTrigger,
    LogseqUuidClaim, LogseqUuidResolution, ManagedLocalApplyOutcome,
    ManagedLocalJournalPayloadKind, ManagedLocalPrefixState, ManagedLocalProjection,
    ManagedLocalRecord, ManagedLocalRecordError, ManagedLocalWork, MaterializationStats,
    MaterializedBlock, MaterializedPage, OperationTransaction, PagePreambleRewrite, PageRename,
    PortablePathConflict, PortablePathConflictParticipant, PreparedManagedLocalRecord,
    ProjectionEndpointBinding, ProjectionPageState, ProjectionRequirement,
    ProjectionRequirementState, ProjectionWriteAuthorization, SemanticOperation, ShardedHotEngine,
    StageOutcome, WorkspaceStatus,
};
#[cfg(test)]
pub(crate) use hot_engine::{inject_managed_local_append_fault_for_test, ManagedLocalAppendFault};
pub use identity::{
    BatchId, BlockId, CanonicalArchiveResourceId, CanonicalGraphResourceId, CrdtPeerId, DeviceId,
    DocumentId, ImportId, LogseqUuid, PageId, ProjectionEndpointId, ProjectionReceiptStoreId,
    SessionId, WorkspaceId,
};
pub use import::{
    classify_conflict_copy, inventory_affected, inventory_initial_shadow, plan_affected_import,
    BlockImportMatch, BlockMatchBasis, ConflictClassificationError, ConflictCopyClass, ExactBytes,
    ImportBlock, ImportBlockReason, ImportInstrumentation, ImportMatches, ImportPlan,
    ImportPlanStatus, InventoryError, PageImportMatch, PageMatchBasis, RawInventory,
    RawObservation, RejectedRawId, RejectedRawIdReason, MAX_IMPORT_CATALOG_ENTRIES,
    MAX_IMPORT_DEPTH, MAX_IMPORT_FILES, MAX_IMPORT_LOCATOR_COMPONENTS, MAX_IMPORT_PARSED_NODES,
    MAX_IMPORT_RAW_BYTES,
};
pub(crate) use local_journal_v2_anchor::{
    classify_managed_local_anchor, managed_local_v2_anchor_name,
    parse_managed_local_v2_anchor_name, ManagedLocalAnchorEncoding, ManagedLocalGenerationAnchorV2,
    ManagedLocalJournal, ManagedLocalJournalProtocol, MANAGED_LOCAL_ANCHOR_V2_BYTES,
};
pub use object_store::{BatchInspection, ObjectStore, ObjectStoreStats, StoreError};
pub use page_name_index::{
    ExactLogicalPageNameBlobV1, ExactLogicalPageNameDigest, ExactLogicalPageNameRefV1,
    PageNameOwnershipOccupiedV1, PageNameOwnershipRecordV1, PageNameOwnershipReleasedV1,
    PageNameOwnershipRootV1, EXACT_LOGICAL_PAGE_NAME_BLOB_SCHEMA_VERSION,
    EXACT_LOGICAL_PAGE_NAME_REF_SCHEMA_VERSION, MAX_PAGE_NAME_POINT_BATCH,
    PAGE_NAME_CATALOG_FRONTIER_SCHEMA_VERSION, PAGE_NAME_OWNERSHIP_RECORD_SCHEMA_VERSION,
    PAGE_NAME_OWNERSHIP_ROOT_SCHEMA_VERSION, PAGE_NAME_OWNERSHIP_STORE_SCHEMA_VERSION,
};
pub use portable_path_index::{
    PortablePathIndexRoot, PortablePathOccupied, PortablePathRecord, PortablePathReleased,
};
pub use projection::{
    derive_receiver_local_projection, execute_manifested_projection_work, plan_projection,
    recover_incomplete_projections, write_projection_exact, PolicyGeneratedAnchor, ProjectionError,
    ProjectionPlan, ProjectionWrite,
};
pub use projection_manifest::{
    annotated_base_document_id, projection_intent_document_id, AnnotatedProjectionBase,
    ManifestObjectRef, ManifestProjectionPrecondition, ManifestProjectionTarget,
    ManifestedProjectionIntent, ProjectionManifestError, ValidatedProjectionObjects,
    ANNOTATED_BASE_SCHEMA_VERSION, MANIFESTED_PROJECTION_SCHEMA_VERSION, MAX_ANNOTATED_BASE_BYTES,
    MAX_MANIFESTED_PROJECTION_BYTES,
};
pub use projection_store::{
    LocalProjectionEvidenceRecord, ProjectionAttemptReservation, ProjectionReceiptStore,
    ProjectionStoreError,
};
pub(crate) use projection_work_index::{
    ProjectionCompletedReceipt, ProjectionDirectCompletionAuthority, ProjectionWorkBlockAuthority,
    ProjectionWorkCompletionAuthority,
};
pub use projection_work_index::{
    ProjectionPendingActivation, ProjectionPendingCursor, ProjectionPendingPage, ProjectionWork,
    ProjectionWorkCursor, ProjectionWorkError, ProjectionWorkId, ProjectionWorkIndex,
    ProjectionWorkIndexStats, ProjectionWorkPage, ProjectionWorkStatus, ProjectionWorkTarget,
};
pub(crate) use receipt::managed_component_is_portable;
pub use receipt::{
    AnnotatedIdentity, BaseBlob, BlobDescription, CrdtPeerCounter, DocumentCausalDigest,
    DocumentDependencies, FrontierV2, ImportInventoryEntry, ImportInventoryState, ImportLocator,
    LogicalCompletionId, ManagedPath, ManagedTextKind, PortablePathKey, PortablePathKeyDigest,
    ProjectionClaimEvidence, ProjectionClaimParticipant, ProjectionCompletion, ProjectionIntent,
    ProjectionIntentId, ProjectionPrecondition, ReceiptError, StructuralLocator, StructuralSpan,
    DIFF_SCHEMA_VERSION, MANAGED_ENTITY_SET_VERSION, PORTABLE_PATH_CASE_FOLD_UNICODE_VERSION,
    PORTABLE_PATH_KEY_VERSION, PORTABLE_PATH_NORMALIZATION_UNICODE_VERSION,
    PROJECTION_POLICY_VERSION, PROJECTION_SCHEMA_VERSION, RECEIPT_SCHEMA_VERSION,
};
pub use reference_catalog::{
    BlockReferenceFactV1, BlockReferenceKindV1, PageNameReferenceFactV1, PageReferenceKindV1,
    ReferenceCatalogDeltaV2, ReferenceCatalogError, ReferenceCatalogPolicyV1,
    ReferenceCatalogRootV2, ReferenceFactV1, ReferenceSourceLocatorV1, ReferenceSourcePostingV2,
    MAX_REFERENCE_CATALOG_DELTA_BYTES, MAX_REFERENCE_CATALOG_DELTA_SOURCES,
    REFERENCE_CATALOG_EXTRACTOR_VERSION, REFERENCE_CATALOG_POLICY_VERSION,
    REFERENCE_CATALOG_ROOT_SCHEMA_VERSION, REFERENCE_CATALOG_SCHEMA_VERSION,
};
pub use semantic::{
    BlockDelta, BlockOwner, BlockState, CanonicalSnapshot, LogicalPageName, LogicalPageNameError,
    LogseqIdentityOrigin, MembershipClaim, MembershipDelta, PageDelta, PageNameKeyDigest,
    PagePreambleDelta, PagePreambleState, PageState, PolicyGeneratedAnchorReason, SemanticEffect,
    SemanticError, VisibleMembership, CATALOG_PAGE_STATE_SCHEMA_VERSION,
    MAX_LOGICAL_PAGE_NAME_BYTES, PAGE_NAME_KEY_VERSION, SEMANTIC_EFFECT_SCHEMA_VERSION,
};
pub use simulator::{
    CoordinatorAction, CoordinatorDurableBoundary, CoordinatorExpectedState,
    CoordinatorFailureWitness, CoordinatorFault, CoordinatorHandoffState, CoordinatorObservation,
    CoordinatorOracle, CoordinatorOracleIdentity, CoordinatorReadGate, CoordinatorRunOutcome,
    CoordinatorSqliteMutation, DeterministicSimulator, FailureCapsule, FailureIdentity,
    FrozenCandidateId, InvariantFailureKind, MinimizedScenario, Scenario, ScenarioAction,
    ScenarioDevice, ScenarioError, SimulatorDeviceState, FAILURE_CAPSULE_SCHEMA_VERSION,
    SCENARIO_SCHEMA_VERSION,
};
pub use sqlite::{
    AcceptedBatchEvent, ApplicationRuntimeRoot, ApplyDisposition, ForensicEvidence,
    FrontierReferenceHit, FrontierReferenceQuery, FrontierReferenceResults, FrontierRenamePlan,
    OpenProjection, ProjectionClaim, ProjectionError as SqliteProjectionError, ProjectionRecovery,
    RebuildInstrumentation, RebuildSource, ReferenceQueryInstrumentation, SqliteFrontier,
    TailOverlay, TailOverlayError, TailOverlayStatus, TailReservation, SQLITE_APPLICATION_ID,
    SQLITE_SCHEMA_VERSION, TAIL_MAX_BATCHES, TAIL_MAX_BYTES,
};
pub use sqlite_materialization::{
    MaterializationChange, MaterializationError, MaterializedBlockInput, MaterializedBlockRow,
    MaterializedEntityId, MaterializedPageInput, MaterializedPageRow, MaterializedProperty,
    MaterializedPropertyRow, MaterializedReference, MaterializedReferenceKind,
    MaterializedReferrerRow, MaterializedSearchHit, MaterializedTagRow, MaterializedTask,
    MaterializedTaskRow, SqliteMaterializedRead, MAX_MATERIALIZATION_CHANGE_BLOCKS,
    MAX_MATERIALIZATION_CHANGE_BYTES, MAX_MATERIALIZATION_CHANGE_FACET_VALUES,
    MAX_MATERIALIZATION_CHANGE_PAGES, MAX_MATERIALIZATION_FACET_BYTES,
    MAX_MATERIALIZATION_FACET_VALUES, MAX_MATERIALIZATION_FIELD_BYTES,
    MAX_MATERIALIZATION_PREAMBLE_BYTES, MAX_MATERIALIZATION_QUERY_BYTES,
    MAX_MATERIALIZATION_QUERY_ROWS, MAX_MATERIALIZATION_READ_BYTES,
};
