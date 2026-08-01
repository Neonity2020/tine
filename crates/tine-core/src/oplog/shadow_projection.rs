//! Durable exact-source semantic projection proof for an inactive bootstrap.
//!
//! This module owns no graph writer, enrollment transition, activation pointer,
//! or `LocalActive` capability. It renders through the normal sparse projector
//! and publishes only below a retained device-local root that is physically
//! and structurally disjoint from the live graph.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::os::fd::{AsFd as _, AsRawFd as _};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::bootstrap_import::{BOOTSTRAP_FRONTIER_SCHEMA_VERSION, BOOTSTRAP_IMPORT_SCHEMA_VERSION};
use super::hot_engine::{
    CurrentPathCatalogBinding, CurrentPathCatalogRow, ProjectionPageState,
    BOOTSTRAP_MATERIALIZATION_CHUNK_PAGES, MAX_CURRENT_PATH_CURSOR_PAGE_ROWS,
};
use super::import::{
    bootstrap_authoritative_source_paths, InactiveBootstrapAcceptedAuthority,
    InactiveBootstrapAcceptedAuthorityBinding, InactiveBootstrapPreparedPublication,
    InactiveBootstrapVerifiedPublication,
};
use super::migration_backup::{MigrationBackupError, MigrationBackupRoot, VerifiedSourceBackup};
use super::object_store::{open_dir_nofollow, open_file_nofollow, sync_dir_required};
#[cfg(test)]
use super::plan_projection;
use super::projection::{
    plan_projection_adopting_exact_source, ExactSourceProjectionError,
    ExactSourceSemanticDifference, ProjectionPlan,
};
use super::sqlite::{OpenProjection, VerifiedBootstrapSqliteProjection};
use super::{
    BlobDescription, CanonicalGraphResourceId, ContentDigest, DeviceId, LineageDigest, ManagedPath,
    ManagedTextKind, PageId, ProjectionEndpointId, ProjectionIntent, ProjectionPrecondition,
    ProjectionReceiptStoreId, WorkspaceId, CATALOG_PAGE_STATE_SCHEMA_VERSION, DIFF_SCHEMA_VERSION,
    MANAGED_ENTITY_SET_VERSION, MANIFEST_ENCODING_VERSION, OBJECT_ENVELOPE_SCHEMA_VERSION,
    OPERATION_SCHEMA_VERSION, OPLOG_PROTOCOL_VERSION, PROJECTION_POLICY_VERSION,
    PROJECTION_SCHEMA_VERSION, RECEIPT_SCHEMA_VERSION, SEMANTIC_EFFECT_SCHEMA_VERSION,
    SQLITE_SCHEMA_VERSION,
};
use crate::model::{
    move_file_noreplace, BootstrapSourceCapture, BootstrapSourceChunkCursor, BootstrapSourceEntry,
    Graph, BOOTSTRAP_SOURCE_CAPTURE_SCHEMA, BOOTSTRAP_SOURCE_CHUNK_BYTES,
    BOOTSTRAP_SOURCE_MAX_DIRECTORIES, BOOTSTRAP_SOURCE_MAX_DIRECTORY_DEPTH,
    BOOTSTRAP_SOURCE_MAX_FILES, BOOTSTRAP_SOURCE_MAX_FILE_BYTES,
    BOOTSTRAP_SOURCE_MAX_LOGICAL_NAME_BYTES, BOOTSTRAP_SOURCE_MAX_PATH_BYTES,
    BOOTSTRAP_SOURCE_MAX_TOTAL_BYTES,
};

const SHADOW_PROJECTION_SCHEMA_VERSION: u32 = 1;
const SHADOW_PROOF_SCHEMA_VERSION: u32 = 1;
const SHADOW_COMMIT_MARKER_SCHEMA_VERSION: u32 = 1;
const SHADOW_ROOT_DIRECTORY: &str = "inactive-shadow-projections-v1";
const PAYLOAD_DIRECTORY: &str = "payload";
const MANIFEST_FILE: &str = "manifest.bin";
const PROOF_FILE: &str = "proof.bin";
const PROOF_STAGE_FILE: &str = ".proof.bin.staging";
const COMMIT_MARKER_FILE: &str = "committed.bin";
const COMMIT_MARKER_STAGE_FILE: &str = ".committed.bin.staging";
const MANIFEST_MAGIC: &[u8; 8] = b"TINESH1\0";
const PROOF_MAGIC: &[u8; 8] = b"TINESP1\0";
const COMMIT_MARKER_MAGIC: &[u8; 8] = b"TINESC1\0";
const IO_BUFFER_BYTES: usize = 64 * 1024;
const CATALOG_PAGE_ROWS: usize = 128;
const MAX_MANIFEST_ENTRY_BYTES: usize = BOOTSTRAP_SOURCE_MAX_FILE_BYTES as usize * 3;
const MAX_MANIFEST_BYTES: u64 = BOOTSTRAP_SOURCE_MAX_TOTAL_BYTES * 4;
const MAX_SMALL_EVIDENCE_BYTES: u64 = 1024 * 1024;
const PROMOTED_BOOTSTRAP_PROJECTION_BINDING_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_BOOTSTRAP_PROJECTION_LOCATOR_RETAINED_BYTES: u64 = 256 * 1024 * 1024;

#[cfg(test)]
fn complete_shadow_verification_calls() -> &'static std::sync::Mutex<HashMap<WorkspaceId, u64>> {
    static CALLS: std::sync::OnceLock<std::sync::Mutex<HashMap<WorkspaceId, u64>>> =
        std::sync::OnceLock::new();
    CALLS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn reset_complete_shadow_verification_calls_for_test(workspace: WorkspaceId) {
    complete_shadow_verification_calls()
        .lock()
        .unwrap()
        .insert(workspace, 0);
}

#[cfg(test)]
pub(crate) fn complete_shadow_verification_calls_for_test(workspace: WorkspaceId) -> u64 {
    complete_shadow_verification_calls()
        .lock()
        .unwrap()
        .get(&workspace)
        .copied()
        .unwrap_or(0)
}

#[cfg(test)]
fn aggregate_reopen_calls() -> &'static std::sync::Mutex<HashMap<WorkspaceId, u64>> {
    static CALLS: std::sync::OnceLock<std::sync::Mutex<HashMap<WorkspaceId, u64>>> =
        std::sync::OnceLock::new();
    CALLS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn reset_aggregate_reopen_calls_for_test(workspace: WorkspaceId) {
    aggregate_reopen_calls()
        .lock()
        .unwrap()
        .insert(workspace, 0);
}

#[cfg(test)]
pub(crate) fn aggregate_reopen_calls_for_test(workspace: WorkspaceId) -> u64 {
    aggregate_reopen_calls()
        .lock()
        .unwrap()
        .get(&workspace)
        .copied()
        .unwrap_or(0)
}

#[cfg(test)]
thread_local! {
    static SHADOW_PROJECTION_CRASH_CUT: std::cell::Cell<Option<ShadowProjectionCrashCut>> =
        const { std::cell::Cell::new(None) };
    static SHADOW_BEFORE_FINAL_SOURCE_VERIFY:
        std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> =
        const { std::cell::RefCell::new(None) };
    static SHADOW_DURABILITY_BARRIERS:
        std::cell::RefCell<Vec<ShadowProjectionDurabilityBarrier>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum ShadowProjectionCrashCut {
    AfterShadowBaseCreation,
    AfterShadowWorkspaceCreation,
    PartialPayloadWrite,
    AfterPayloadPublication,
    PartialManifestWrite,
    AfterManifestPublication,
    AfterStagingRename,
    PartialProofWrite,
    AfterProofPublication,
    PartialCommitMarkerWrite,
    AfterCommitMarkerPublication,
}

impl ShadowProjectionCrashCut {
    const fn label(self) -> &'static str {
        match self {
            Self::AfterShadowBaseCreation => "after_shadow_base_creation",
            Self::AfterShadowWorkspaceCreation => "after_shadow_workspace_creation",
            Self::PartialPayloadWrite => "partial_payload_write",
            Self::AfterPayloadPublication => "after_payload_publication",
            Self::PartialManifestWrite => "partial_manifest_write",
            Self::AfterManifestPublication => "after_manifest_publication",
            Self::AfterStagingRename => "after_staging_rename",
            Self::PartialProofWrite => "partial_proof_write",
            Self::AfterProofPublication => "after_proof_publication",
            Self::PartialCommitMarkerWrite => "partial_commit_marker_write",
            Self::AfterCommitMarkerPublication => "after_commit_marker_publication",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShadowProjectionDurabilityBarrier {
    BackupRootAfterShadowBase,
    ShadowBaseAfterWorkspace,
    PublicationParentAfterFinal,
}

fn take_crash_cut(cut: ShadowProjectionCrashCut) -> bool {
    #[cfg(test)]
    {
        return SHADOW_PROJECTION_CRASH_CUT.with(|pending| {
            if pending.get() == Some(cut) {
                pending.set(None);
                true
            } else {
                false
            }
        });
    }
    #[cfg(not(test))]
    {
        let _ = cut;
        false
    }
}

fn inject_crash_cut(cut: ShadowProjectionCrashCut) -> Result<(), ShadowProjectionError> {
    if take_crash_cut(cut) {
        Err(ShadowProjectionError::InjectedCrashCut(cut.label()))
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn before_final_source_verify_hook() -> io::Result<()> {
    SHADOW_BEFORE_FINAL_SOURCE_VERIFY.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn before_final_source_verify_hook() -> io::Result<()> {
    Ok(())
}

#[derive(Debug)]
pub(crate) enum ShadowProjectionError {
    Io(io::Error),
    Backup(MigrationBackupError),
    BindingMismatch(&'static str),
    NormalSparseMismatch {
        path: String,
        source_bytes: usize,
        projected_bytes: usize,
        detail: NormalSparseMismatchDetail,
    },
    SemanticMismatch {
        path: String,
        difference: ExactSourceSemanticDifference,
    },
    CorruptOrConflicting(&'static str),
    Projection(String),
    ResourceLimit {
        resource: &'static str,
        observed: u64,
        limit: u64,
    },
    InjectedCrashCut(&'static str),
}

#[derive(Debug)]
pub(crate) enum NormalSparseMismatchDetail {
    FirstDifferingByte(usize),
    CommonPrefixEnded,
    BindingChecks(Vec<&'static str>),
}

impl fmt::Display for ShadowProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Backup(error) => error.fmt(formatter),
            Self::BindingMismatch(detail) | Self::CorruptOrConflicting(detail) => {
                formatter.write_str(detail)
            }
            Self::NormalSparseMismatch {
                path,
                source_bytes,
                projected_bytes,
                detail,
            } => {
                write!(
                    formatter,
                    "normal sparse projection mismatch for {path}: source byte length={source_bytes}, projected byte length={projected_bytes}; "
                )?;
                match detail {
                    NormalSparseMismatchDetail::FirstDifferingByte(offset) => {
                        write!(formatter, "first differing byte offset {offset}")
                    }
                    NormalSparseMismatchDetail::CommonPrefixEnded => {
                        formatter.write_str("common prefix ended")
                    }
                    NormalSparseMismatchDetail::BindingChecks(checks) => write!(
                        formatter,
                        "bytes are equal; failed non-byte binding checks: {}",
                        checks.join(", ")
                    ),
                }
            }
            Self::SemanticMismatch { path, difference } => {
                write!(
                    formatter,
                    "bootstrap source semantic mismatch for {path}: {difference}"
                )
            }
            Self::Projection(detail) => formatter.write_str(detail),
            Self::ResourceLimit {
                resource,
                observed,
                limit,
            } => write!(
                formatter,
                "{resource} limit exceeded: observed {observed}, limit {limit}"
            ),
            Self::InjectedCrashCut(label) => write!(formatter, "injected crash cut: {label}"),
        }
    }
}

impl std::error::Error for ShadowProjectionError {}

impl From<io::Error> for ShadowProjectionError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<MigrationBackupError> for ShadowProjectionError {
    fn from(error: MigrationBackupError) -> Self {
        Self::Backup(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShadowProjectionSchemaBinding {
    pub(crate) shadow_projection: u32,
    pub(crate) shadow_proof: u32,
    pub(crate) shadow_commit_marker: u32,
    pub(crate) source_capture: u32,
    pub(crate) bootstrap_import: u32,
    pub(crate) bootstrap_frontier: u32,
    pub(crate) oplog_protocol: u32,
    pub(crate) operation: u32,
    pub(crate) object_envelope: u32,
    pub(crate) manifest: u32,
    pub(crate) semantic_effect: u32,
    pub(crate) catalog_page_state: u32,
    pub(crate) receipt: u32,
    pub(crate) projection: u32,
    pub(crate) projection_policy: u32,
    pub(crate) diff: u32,
    pub(crate) managed_entity_set: u32,
    pub(crate) sqlite: u32,
}

impl ShadowProjectionSchemaBinding {
    const CURRENT: Self = Self {
        shadow_projection: SHADOW_PROJECTION_SCHEMA_VERSION,
        shadow_proof: SHADOW_PROOF_SCHEMA_VERSION,
        shadow_commit_marker: SHADOW_COMMIT_MARKER_SCHEMA_VERSION,
        source_capture: BOOTSTRAP_SOURCE_CAPTURE_SCHEMA,
        bootstrap_import: BOOTSTRAP_IMPORT_SCHEMA_VERSION,
        bootstrap_frontier: BOOTSTRAP_FRONTIER_SCHEMA_VERSION,
        oplog_protocol: OPLOG_PROTOCOL_VERSION,
        operation: OPERATION_SCHEMA_VERSION,
        object_envelope: OBJECT_ENVELOPE_SCHEMA_VERSION,
        manifest: MANIFEST_ENCODING_VERSION,
        semantic_effect: SEMANTIC_EFFECT_SCHEMA_VERSION,
        catalog_page_state: CATALOG_PAGE_STATE_SCHEMA_VERSION,
        receipt: RECEIPT_SCHEMA_VERSION,
        projection: PROJECTION_SCHEMA_VERSION,
        projection_policy: PROJECTION_POLICY_VERSION,
        diff: DIFF_SCHEMA_VERSION,
        managed_entity_set: MANAGED_ENTITY_SET_VERSION,
        sqlite: SQLITE_SCHEMA_VERSION,
    };

    fn write(self, output: &mut Vec<u8>) {
        for value in [
            self.shadow_projection,
            self.shadow_proof,
            self.shadow_commit_marker,
            self.source_capture,
            self.bootstrap_import,
            self.bootstrap_frontier,
            self.oplog_protocol,
            self.operation,
            self.object_envelope,
            self.manifest,
            self.semantic_effect,
            self.catalog_page_state,
            self.receipt,
            self.projection,
            self.projection_policy,
            self.diff,
            self.managed_entity_set,
            self.sqlite,
        ] {
            output.extend_from_slice(&value.to_be_bytes());
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ShadowProjectionInstrumentation {
    pub(crate) catalog_rows: u64,
    pub(crate) source_files: u64,
    pub(crate) source_chunks: u64,
    pub(crate) source_bytes_read: u64,
    pub(crate) payload_bytes_written: u64,
    pub(crate) payload_bytes_read: u64,
    pub(crate) manifest_entries: u64,
    pub(crate) projection_plans: u64,
    pub(crate) bulk_materialization_chunks: u64,
    pub(crate) bulk_pages_materialized: u64,
    pub(crate) peak_bulk_pages: u64,
    pub(crate) accepted_frontier_session_hits: u64,
    pub(crate) accepted_frontier_session_misses: u64,
    pub(crate) accepted_frontier_session_evictions: u64,
    pub(crate) accepted_frontier_session_oversize: u64,
    pub(crate) accepted_frontier_session_peak_resident_bytes: u64,
    pub(crate) external_exact_session_hits: u64,
    pub(crate) external_exact_session_misses: u64,
    pub(crate) external_exact_session_evictions: u64,
    pub(crate) external_exact_session_oversize: u64,
    pub(crate) external_exact_session_peak_resident_bytes: u64,
    pub(crate) peak_owned_source_bytes: u64,
    pub(crate) peak_owned_catalog_rows: u64,
    pub(crate) tree_entries_visited: u64,
}

/// Constant-size durable binding for the immutable shadow publication retained
/// after bootstrap promotion.  It is inert serialized evidence; only
/// [`BootstrapProjectionAuthority::reopen`] turns it into a read-only
/// capability after authenticating the exact retained publication.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromotedBootstrapProjectionBindingV1 {
    schema_version: u32,
    binding_digest: ContentDigest,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    endpoint_id: ProjectionEndpointId,
    device_id: DeviceId,
    graph_resource_id: CanonicalGraphResourceId,
    receipt_store_id: ProjectionReceiptStoreId,
    archive_control_binding: ContentDigest,
    backup_root_identity: ContentDigest,
    publication_id: ContentDigest,
    bootstrap_publication_id: ContentDigest,
    bootstrap_aggregate_digest: ContentDigest,
    bootstrap_import_id: ContentDigest,
    bootstrap_part_count: u32,
    accepted_frontier_state_digest: ContentDigest,
    history_generation: u64,
    history_root: ContentDigest,
    catalog_root: ContentDigest,
    catalog_rows: u64,
    manifest_header: BlobDescription,
    manifest: BlobDescription,
    proof: BlobDescription,
    commit_marker: BlobDescription,
    shadow_evidence_digest: ContentDigest,
    staged_inventory_digest: ContentDigest,
    staged_file_count: u64,
    staged_total_bytes: u64,
}

impl PromotedBootstrapProjectionBindingV1 {
    pub(crate) fn from_verified(
        verified: &VerifiedShadowProjection,
    ) -> Result<Self, ShadowProjectionError> {
        let authority = verified.authority_binding();
        let storage = authority.storage_binding();
        let bootstrap = authority.bootstrap_binding();
        let header = manifest_header_from_verified(verified)?;
        let mut binding = Self {
            schema_version: PROMOTED_BOOTSTRAP_PROJECTION_BINDING_SCHEMA_VERSION,
            binding_digest: ContentDigest::of(b"pending promoted bootstrap binding"),
            workspace_id: verified.workspace_id(),
            lineage_digest: authority.lineage_digest(),
            endpoint_id: storage.endpoint.endpoint_id(),
            device_id: storage.endpoint.device_id(),
            graph_resource_id: verified.graph_resource(),
            receipt_store_id: storage.receipt_store_id,
            archive_control_binding: authority.archive_identity().binding_digest(),
            backup_root_identity: verified.physical_root_identity(),
            publication_id: verified.publication_id(),
            bootstrap_publication_id: ContentDigest::from_bytes(
                *bootstrap.publication_id().as_bytes(),
            ),
            bootstrap_aggregate_digest: ContentDigest::from_bytes(
                *bootstrap.aggregate_digest().as_bytes(),
            ),
            bootstrap_import_id: ContentDigest::from_bytes(*authority.import_id().as_bytes()),
            bootstrap_part_count: bootstrap.part_count(),
            accepted_frontier_state_digest: verified.catalog_binding().accepted_frontier(),
            history_generation: verified.catalog_binding().history_generation(),
            history_root: verified.catalog_binding().history_root(),
            catalog_root: verified.catalog_binding().catalog_root(),
            catalog_rows: verified.catalog_binding().catalog_rows(),
            manifest_header: BlobDescription::of(&header),
            manifest: verified.manifest(),
            proof: verified.proof(),
            commit_marker: verified.commit_marker(),
            shadow_evidence_digest: verified.evidence_digest(),
            staged_inventory_digest: verified.staged_inventory_digest(),
            staged_file_count: verified.staged_file_count(),
            staged_total_bytes: verified.staged_total_bytes(),
        };
        binding.binding_digest = binding.compute_binding_digest();
        binding.validate()?;
        Ok(binding)
    }

    pub(crate) fn validate(&self) -> Result<(), ShadowProjectionError> {
        if self.schema_version != PROMOTED_BOOTSTRAP_PROJECTION_BINDING_SCHEMA_VERSION {
            return Err(ShadowProjectionError::BindingMismatch(
                "promoted bootstrap projection binding schema is unsupported",
            ));
        }
        if self.binding_digest != self.compute_binding_digest() {
            return Err(ShadowProjectionError::BindingMismatch(
                "promoted bootstrap projection binding digest is invalid",
            ));
        }
        if self.catalog_rows != self.staged_file_count
            || self.staged_total_bytes > BOOTSTRAP_SOURCE_MAX_TOTAL_BYTES
            || self.catalog_rows > BOOTSTRAP_SOURCE_MAX_FILES
            || self.manifest.byte_length() > MAX_MANIFEST_BYTES
            || self.proof.byte_length() > MAX_SMALL_EVIDENCE_BYTES
            || self.commit_marker.byte_length() > MAX_SMALL_EVIDENCE_BYTES
            || self.manifest_header.byte_length() > MAX_SMALL_EVIDENCE_BYTES
        {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "promoted bootstrap projection binding exceeds retained bounds",
            ));
        }
        Ok(())
    }

    /// Structurally valid inert state used only by object-store tests whose
    /// subject is durable-history encoding rather than runtime promotion.
    #[cfg(test)]
    pub(crate) fn synthetic_for_object_store_test(
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        endpoint_id: ProjectionEndpointId,
        device_id: DeviceId,
        graph_resource_id: CanonicalGraphResourceId,
        receipt_store_id: ProjectionReceiptStoreId,
        archive_control_binding: ContentDigest,
        bootstrap_publication_id: ContentDigest,
        bootstrap_aggregate_digest: ContentDigest,
        bootstrap_import_id: ContentDigest,
        bootstrap_part_count: u32,
        accepted_frontier_state_digest: ContentDigest,
        history_generation: u64,
        history_root: ContentDigest,
    ) -> Self {
        let empty = BlobDescription::of(&[]);
        let mut binding = Self {
            schema_version: PROMOTED_BOOTSTRAP_PROJECTION_BINDING_SCHEMA_VERSION,
            binding_digest: ContentDigest::of(b"pending synthetic bootstrap binding"),
            workspace_id,
            lineage_digest,
            endpoint_id,
            device_id,
            graph_resource_id,
            receipt_store_id,
            archive_control_binding,
            backup_root_identity: ContentDigest::of(b"synthetic backup root"),
            publication_id: ContentDigest::of(b"synthetic shadow publication"),
            bootstrap_publication_id,
            bootstrap_aggregate_digest,
            bootstrap_import_id,
            bootstrap_part_count,
            accepted_frontier_state_digest,
            history_generation,
            history_root,
            catalog_root: ContentDigest::of(b"synthetic empty catalog"),
            catalog_rows: 0,
            manifest_header: empty,
            manifest: empty,
            proof: empty,
            commit_marker: empty,
            shadow_evidence_digest: ContentDigest::of(b"synthetic shadow evidence"),
            staged_inventory_digest: ContentDigest::of(b"synthetic empty inventory"),
            staged_file_count: 0,
            staged_total_bytes: 0,
        };
        binding.binding_digest = binding.compute_binding_digest();
        binding
    }

    fn compute_binding_digest(&self) -> ContentDigest {
        let mut hasher = Sha256::new();
        hasher.update(b"tine/promoted-bootstrap-projection-binding/v1\0");
        hasher.update(self.schema_version.to_be_bytes());
        hasher.update(self.workspace_id.as_uuid().as_bytes());
        hasher.update(self.lineage_digest.as_bytes());
        hasher.update(self.endpoint_id.as_uuid().as_bytes());
        hasher.update(self.device_id.as_uuid().as_bytes());
        hasher.update(self.graph_resource_id.as_bytes());
        hasher.update(self.receipt_store_id.as_bytes());
        for digest in [
            self.archive_control_binding,
            self.backup_root_identity,
            self.publication_id,
            self.bootstrap_publication_id,
            self.bootstrap_aggregate_digest,
            self.bootstrap_import_id,
            self.accepted_frontier_state_digest,
            self.history_root,
            self.catalog_root,
            self.shadow_evidence_digest,
            self.staged_inventory_digest,
        ] {
            hasher.update(digest.as_bytes());
        }
        hasher.update(self.bootstrap_part_count.to_be_bytes());
        hasher.update(self.history_generation.to_be_bytes());
        hasher.update(self.catalog_rows.to_be_bytes());
        for description in [
            self.manifest_header,
            self.manifest,
            self.proof,
            self.commit_marker,
        ] {
            hasher.update(description.sha256());
            hasher.update(description.byte_length().to_be_bytes());
        }
        hasher.update(self.staged_file_count.to_be_bytes());
        hasher.update(self.staged_total_bytes.to_be_bytes());
        ContentDigest::from_bytes(hasher.finalize().into())
    }

    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) const fn lineage_digest(&self) -> LineageDigest {
        self.lineage_digest
    }

    pub(crate) const fn endpoint_id(&self) -> ProjectionEndpointId {
        self.endpoint_id
    }

    pub(crate) const fn device_id(&self) -> DeviceId {
        self.device_id
    }

    pub(crate) const fn graph_resource_id(&self) -> CanonicalGraphResourceId {
        self.graph_resource_id
    }

    pub(crate) const fn receipt_store_id(&self) -> ProjectionReceiptStoreId {
        self.receipt_store_id
    }

    pub(crate) const fn archive_control_binding(&self) -> ContentDigest {
        self.archive_control_binding
    }

    pub(crate) const fn bootstrap_publication_id(&self) -> ContentDigest {
        self.bootstrap_publication_id
    }

    pub(crate) const fn bootstrap_aggregate_digest(&self) -> ContentDigest {
        self.bootstrap_aggregate_digest
    }

    pub(crate) const fn bootstrap_import_id(&self) -> ContentDigest {
        self.bootstrap_import_id
    }

    pub(crate) const fn bootstrap_part_count(&self) -> u32 {
        self.bootstrap_part_count
    }

    pub(crate) const fn accepted_frontier_state_digest(&self) -> ContentDigest {
        self.accepted_frontier_state_digest
    }

    pub(crate) const fn history_generation(&self) -> u64 {
        self.history_generation
    }

    pub(crate) const fn history_root(&self) -> ContentDigest {
        self.history_root
    }

    pub(crate) const fn catalog_rows(&self) -> u64 {
        self.catalog_rows
    }

    pub(crate) const fn authority_digest(&self) -> ContentDigest {
        self.shadow_evidence_digest
    }
}

/// Crate-private proof that an inactive accepted bootstrap renders exactly to
/// a committed, device-local shadow tree. It grants no graph-write or
/// enrollment authority.
#[derive(Clone, Debug)]
pub(crate) struct VerifiedShadowProjection {
    directory: PathBuf,
    workspace_id: WorkspaceId,
    graph_resource: CanonicalGraphResourceId,
    physical_root_identity: ContentDigest,
    publication_id: ContentDigest,
    source_capture: BlobDescription,
    source_inventory: BlobDescription,
    source_entries: BlobDescription,
    source_chunks: BlobDescription,
    file_count: u64,
    chunk_count: u64,
    directory_count: u64,
    total_bytes: u64,
    catalog_binding: CurrentPathCatalogBinding,
    authority_binding: InactiveBootstrapAcceptedAuthorityBinding,
    source_backup: VerifiedSourceBackup,
    sqlite_projection: VerifiedBootstrapSqliteProjection,
    manifest: BlobDescription,
    staged_inventory_digest: ContentDigest,
    staged_file_count: u64,
    staged_total_bytes: u64,
    proof: BlobDescription,
    commit_marker: BlobDescription,
    evidence_digest: ContentDigest,
    schema: ShadowProjectionSchemaBinding,
    instrumentation: ShadowProjectionInstrumentation,
}

impl PartialEq for VerifiedShadowProjection {
    fn eq(&self, other: &Self) -> bool {
        self.workspace_id == other.workspace_id
            && self.graph_resource == other.graph_resource
            && self.physical_root_identity == other.physical_root_identity
            && self.publication_id == other.publication_id
            && self.source_capture == other.source_capture
            && self.source_inventory == other.source_inventory
            && self.source_entries == other.source_entries
            && self.source_chunks == other.source_chunks
            && self.file_count == other.file_count
            && self.chunk_count == other.chunk_count
            && self.directory_count == other.directory_count
            && self.total_bytes == other.total_bytes
            && self.catalog_binding == other.catalog_binding
            && self.authority_binding == other.authority_binding
            && self.source_backup == other.source_backup
            && self.sqlite_projection == other.sqlite_projection
            && self.manifest == other.manifest
            && self.staged_inventory_digest == other.staged_inventory_digest
            && self.staged_file_count == other.staged_file_count
            && self.staged_total_bytes == other.staged_total_bytes
            && self.proof == other.proof
            && self.commit_marker == other.commit_marker
            && self.evidence_digest == other.evidence_digest
            && self.schema == other.schema
    }
}

impl Eq for VerifiedShadowProjection {}

impl VerifiedShadowProjection {
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) const fn graph_resource(&self) -> CanonicalGraphResourceId {
        self.graph_resource
    }

    pub(crate) const fn physical_root_identity(&self) -> ContentDigest {
        self.physical_root_identity
    }

    pub(crate) const fn publication_id(&self) -> ContentDigest {
        self.publication_id
    }

    pub(crate) const fn source_capture(&self) -> BlobDescription {
        self.source_capture
    }

    pub(crate) const fn file_count(&self) -> u64 {
        self.file_count
    }

    pub(crate) const fn chunk_count(&self) -> u64 {
        self.chunk_count
    }

    pub(crate) const fn directory_count(&self) -> u64 {
        self.directory_count
    }

    pub(crate) const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub(crate) const fn catalog_binding(&self) -> CurrentPathCatalogBinding {
        self.catalog_binding
    }

    pub(crate) const fn authority_binding(&self) -> &InactiveBootstrapAcceptedAuthorityBinding {
        &self.authority_binding
    }

    pub(crate) const fn source_backup(&self) -> &VerifiedSourceBackup {
        &self.source_backup
    }

    pub(crate) const fn sqlite_projection(&self) -> &VerifiedBootstrapSqliteProjection {
        &self.sqlite_projection
    }

    pub(crate) const fn manifest(&self) -> BlobDescription {
        self.manifest
    }

    pub(crate) const fn staged_inventory_digest(&self) -> ContentDigest {
        self.staged_inventory_digest
    }

    pub(crate) const fn staged_file_count(&self) -> u64 {
        self.staged_file_count
    }

    pub(crate) const fn staged_total_bytes(&self) -> u64 {
        self.staged_total_bytes
    }

    pub(crate) const fn proof(&self) -> BlobDescription {
        self.proof
    }

    pub(crate) const fn commit_marker(&self) -> BlobDescription {
        self.commit_marker
    }

    pub(crate) const fn evidence_digest(&self) -> ContentDigest {
        self.evidence_digest
    }

    pub(crate) const fn schema(&self) -> ShadowProjectionSchemaBinding {
        self.schema
    }

    pub(crate) fn schema_binding_digest(&self) -> ContentDigest {
        let mut bytes = Vec::with_capacity(18 * std::mem::size_of::<u32>());
        self.schema.write(&mut bytes);
        ContentDigest::of(&bytes)
    }

    pub(crate) const fn instrumentation(&self) -> &ShadowProjectionInstrumentation {
        &self.instrumentation
    }

    /// Reopen the committed per-file evidence without retaining a whole-graph
    /// snapshot. The cursor authenticates the complete manifest description.
    pub(crate) fn file_evidence_cursor(
        &self,
    ) -> Result<ShadowProjectionEvidenceCursor, ShadowProjectionError> {
        if describe_regular_file(&self.directory.join(MANIFEST_FILE), MAX_MANIFEST_BYTES)?
            != self.manifest
        {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "shadow manifest changed after proof",
            ));
        }
        ShadowProjectionEvidenceCursor::open(
            &self.directory.join(MANIFEST_FILE),
            self.file_count,
            manifest_header_from_verified(self)?,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShadowProjectionFileEvidence {
    path: ManagedPath,
    kind: ManagedTextKind,
    logical_name: String,
    page_id: PageId,
    source: BlobDescription,
    source_file_resource: ContentDigest,
    source_link_count: u64,
    source_chunk_count: u32,
    intent: ProjectionIntent,
}

impl ShadowProjectionFileEvidence {
    pub(crate) fn path(&self) -> &ManagedPath {
        &self.path
    }

    pub(crate) const fn kind(&self) -> ManagedTextKind {
        self.kind
    }

    pub(crate) fn logical_name(&self) -> &str {
        &self.logical_name
    }

    pub(crate) const fn page_id(&self) -> PageId {
        self.page_id
    }

    pub(crate) const fn source(&self) -> BlobDescription {
        self.source
    }

    pub(crate) const fn source_file_resource(&self) -> ContentDigest {
        self.source_file_resource
    }

    pub(crate) const fn source_link_count(&self) -> u64 {
        self.source_link_count
    }

    pub(crate) const fn source_chunk_count(&self) -> u32 {
        self.source_chunk_count
    }

    pub(crate) const fn intent(&self) -> &ProjectionIntent {
        &self.intent
    }
}

pub(crate) struct ShadowProjectionEvidenceCursor {
    reader: ManifestReader,
}

impl ShadowProjectionEvidenceCursor {
    fn open(
        path: &Path,
        file_count: u64,
        expected_header: Vec<u8>,
    ) -> Result<Self, ShadowProjectionError> {
        Ok(Self {
            reader: ManifestReader::open(path, file_count, expected_header)?,
        })
    }

    pub(crate) fn next(
        &mut self,
    ) -> Result<Option<ShadowProjectionFileEvidence>, ShadowProjectionError> {
        self.reader.next()
    }

    pub(crate) fn finish(self) -> Result<(), ShadowProjectionError> {
        self.reader.finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BootstrapProjectionEntryLocator {
    offset: u64,
    length: u64,
    entry: BlobDescription,
    page_id: PageId,
    kind: ManagedTextKind,
    source: BlobDescription,
}

#[derive(Default)]
struct BootstrapProjectionRuntimeCounters {
    manifest_scans: std::sync::atomic::AtomicU64,
    manifest_entry_reads: std::sync::atomic::AtomicU64,
    payload_reads: std::sync::atomic::AtomicU64,
    payload_bytes: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BootstrapProjectionAuthorityInstrumentation {
    pub(crate) manifest_scans: u64,
    pub(crate) manifest_entry_reads: u64,
    pub(crate) payload_reads: u64,
    pub(crate) payload_bytes: u64,
    pub(crate) locator_rows: usize,
    pub(crate) locator_retained_bytes: u64,
    pub(crate) graph_scans: u64,
    pub(crate) fsyncs: u64,
    pub(crate) journal_transitions: u64,
}

/// One authenticated bootstrap baseline loaded by exact path.  The entry and
/// payload are bounded and owned only for this point result; the aggregate
/// authority never retains source bytes.
pub(crate) struct BootstrapProjectionBaseline {
    intent: ProjectionIntent,
    kind: ManagedTextKind,
    source: Vec<u8>,
    owner_binding: ContentDigest,
}

impl BootstrapProjectionBaseline {
    pub(crate) const fn intent(&self) -> &ProjectionIntent {
        &self.intent
    }

    pub(crate) const fn kind(&self) -> ManagedTextKind {
        self.kind
    }

    pub(crate) fn source_bytes(&self) -> &[u8] {
        &self.source
    }

    pub(crate) const fn owner_binding(&self) -> ContentDigest {
        self.owner_binding
    }

    /// Rebind immutable bootstrap bytes to a causally newer page state only
    /// after the ordinary exact-source planner proves those bytes still encode
    /// the complete accepted semantics. All non-frontier intent fields must
    /// remain identical.
    pub(crate) fn rebind_semantic_successor(
        &self,
        workspace_id: WorkspaceId,
        state: &ProjectionPageState,
    ) -> Result<ProjectionPlan, ShadowProjectionError> {
        if self.intent.workspace_id() != workspace_id
            || self.intent.page_id() != state.page.page_id
            || self.intent.path() != &state.page.path
            || self.kind != state.page.kind
            || self.intent.target() != BlobDescription::of(&self.source)
        {
            return Err(ShadowProjectionError::BindingMismatch(
                "bootstrap baseline does not bind the current page identity",
            ));
        }
        let plan = plan_projection_adopting_exact_source(workspace_id, state, &self.source)
            .map_err(|error| match error {
                ExactSourceProjectionError::Projection(error) => {
                    ShadowProjectionError::Projection(error.to_string())
                }
                ExactSourceProjectionError::Semantic(difference) => {
                    ShadowProjectionError::SemanticMismatch {
                        path: state.page.path.as_str().to_owned(),
                        difference,
                    }
                }
            })?;
        if plan.target() != self.source
            || !self.intent.matches_replay_except_frontier(plan.intent())
            || plan.intent().frontier() != &state.frontier
        {
            return Err(ShadowProjectionError::BindingMismatch(
                "bootstrap semantic successor changes a non-frontier intent field",
            ));
        }
        Ok(plan)
    }
}

/// Read-only promoted bootstrap-baseline capability. Reopen authenticates the
/// compact promoted binding and retained roots. The first point lookup scans
/// the immutable manifest once to build bounded path locators; every lookup
/// then rereads and authenticates its named manifest entry and, when present,
/// its payload file through retained no-follow directory handles.
pub(crate) struct BootstrapProjectionAuthority {
    binding: PromotedBootstrapProjectionBindingV1,
    publication: Dir,
    payload: Dir,
    locators: std::sync::Mutex<Option<BootstrapProjectionLocators>>,
    counters: std::sync::Arc<BootstrapProjectionRuntimeCounters>,
}

struct BootstrapProjectionLocators {
    rows: HashMap<ManagedPath, BootstrapProjectionEntryLocator>,
    retained_bytes: u64,
}

impl fmt::Debug for BootstrapProjectionAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BootstrapProjectionAuthority")
            .field("workspace_id", &self.binding.workspace_id)
            .field("publication_id", &self.binding.publication_id)
            .field(
                "locator_rows",
                &self
                    .locators
                    .lock()
                    .ok()
                    .and_then(|locators| locators.as_ref().map(|locators| locators.rows.len()))
                    .unwrap_or_default(),
            )
            .finish_non_exhaustive()
    }
}

impl BootstrapProjectionAuthority {
    pub(crate) fn reopen(
        roots: &MigrationBackupRoot,
        binding: &PromotedBootstrapProjectionBindingV1,
    ) -> Result<Self, ShadowProjectionError> {
        #[cfg(test)]
        {
            let mut calls = aggregate_reopen_calls().lock().unwrap();
            let calls = calls.entry(binding.workspace_id()).or_default();
            *calls = calls.saturating_add(1);
        }
        binding.validate()?;
        roots.freshly_validate_retained_roots()?;
        if roots.root_identity() != binding.backup_root_identity
            || roots.graph_resource() != binding.graph_resource_id
        {
            return Err(ShadowProjectionError::BindingMismatch(
                "promoted bootstrap projection backup or graph resource changed",
            ));
        }

        let root = Dir::open_ambient_dir(roots.canonical_root(), ambient_authority())?;
        let base = open_dir_nofollow(&root, SHADOW_ROOT_DIRECTORY)
            .map_err(|error| ShadowProjectionError::Io(io::Error::other(error.to_string())))?;
        let workspace = open_dir_nofollow(&base, &binding.workspace_id.to_string())
            .map_err(|error| ShadowProjectionError::Io(io::Error::other(error.to_string())))?;
        let publication_name = hex(binding.publication_id.as_bytes());
        let publication = open_dir_nofollow(&workspace, &publication_name)
            .map_err(|error| ShadowProjectionError::Io(io::Error::other(error.to_string())))?;
        let payload = open_dir_nofollow(&publication, PAYLOAD_DIRECTORY)
            .map_err(|error| ShadowProjectionError::Io(io::Error::other(error.to_string())))?;

        let proof = read_capability_file(&publication, PROOF_FILE, MAX_SMALL_EVIDENCE_BYTES)?;
        if BlobDescription::of(&proof) != binding.proof {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "promoted shadow proof changed",
            ));
        }
        let marker =
            read_capability_file(&publication, COMMIT_MARKER_FILE, MAX_SMALL_EVIDENCE_BYTES)?;
        if BlobDescription::of(&marker) != binding.commit_marker
            || marker
                .get(marker.len().saturating_sub(32)..)
                .is_none_or(|suffix| suffix != binding.shadow_evidence_digest.as_bytes())
        {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "promoted shadow commit evidence changed",
            ));
        }

        let counters = std::sync::Arc::new(BootstrapProjectionRuntimeCounters::default());
        Ok(Self {
            binding: binding.clone(),
            publication,
            payload,
            locators: std::sync::Mutex::new(None),
            counters,
        })
    }

    pub(crate) const fn binding(&self) -> &PromotedBootstrapProjectionBindingV1 {
        &self.binding
    }

    pub(crate) fn baseline_at(
        &self,
        path: &ManagedPath,
    ) -> Result<Option<BootstrapProjectionBaseline>, ShadowProjectionError> {
        let locator = {
            let mut locators = self.locators.lock().map_err(|_| {
                ShadowProjectionError::CorruptOrConflicting(
                    "promoted shadow locator authority is poisoned",
                )
            })?;
            if locators.is_none() {
                let (rows, retained_bytes) =
                    scan_promoted_manifest_once(&self.publication, &self.binding, &self.counters)?;
                if rows.len() as u64 != self.binding.catalog_rows {
                    return Err(ShadowProjectionError::CorruptOrConflicting(
                        "promoted shadow locator count differs from its catalog binding",
                    ));
                }
                *locators = Some(BootstrapProjectionLocators {
                    rows,
                    retained_bytes,
                });
            }
            locators
                .as_ref()
                .and_then(|locators| locators.rows.get(path).copied())
        };
        let Some(locator) = locator else {
            return Ok(None);
        };
        let mut manifest = open_file_nofollow(&self.publication, MANIFEST_FILE)?;
        manifest.seek(SeekFrom::Start(locator.offset))?;
        let length =
            usize::try_from(locator.length).map_err(|_| ShadowProjectionError::ResourceLimit {
                resource: "promoted manifest entry allocation",
                observed: locator.length,
                limit: MAX_MANIFEST_ENTRY_BYTES as u64,
            })?;
        if length > MAX_MANIFEST_ENTRY_BYTES {
            return Err(ShadowProjectionError::ResourceLimit {
                resource: "promoted manifest entry bytes",
                observed: locator.length,
                limit: MAX_MANIFEST_ENTRY_BYTES as u64,
            });
        }
        let mut entry_bytes = vec![0_u8; length];
        manifest.read_exact(&mut entry_bytes).map_err(|_| {
            ShadowProjectionError::CorruptOrConflicting(
                "promoted shadow manifest entry is truncated",
            )
        })?;
        if BlobDescription::of(&entry_bytes) != locator.entry {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "promoted shadow manifest entry changed after reopen",
            ));
        }
        self.counters
            .manifest_entry_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut cursor = Cursor::new(entry_bytes.as_slice());
        let evidence = read_manifest_evidence(&mut cursor)?;
        if cursor.position() != locator.length
            || evidence.path != *path
            || evidence.page_id != locator.page_id
            || evidence.kind != locator.kind
            || evidence.source != locator.source
        {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "promoted shadow locator does not authenticate its named entry",
            ));
        }
        validate_promoted_entry(&self.binding, &evidence)?;
        let source = read_payload_at(&self.payload, path, evidence.source)?;
        self.counters
            .payload_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.counters
            .payload_bytes
            .fetch_add(source.len() as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(Some(BootstrapProjectionBaseline {
            owner_binding: bootstrap_entry_owner_binding(
                self.binding.shadow_evidence_digest,
                &evidence,
            )?,
            intent: evidence.intent,
            kind: evidence.kind,
            source,
        }))
    }

    #[cfg(test)]
    pub(crate) fn instrumentation(&self) -> BootstrapProjectionAuthorityInstrumentation {
        let (locator_rows, locator_retained_bytes) = self
            .locators
            .lock()
            .ok()
            .and_then(|locators| {
                locators
                    .as_ref()
                    .map(|locators| (locators.rows.len(), locators.retained_bytes))
            })
            .unwrap_or_default();
        BootstrapProjectionAuthorityInstrumentation {
            manifest_scans: self
                .counters
                .manifest_scans
                .load(std::sync::atomic::Ordering::Relaxed),
            manifest_entry_reads: self
                .counters
                .manifest_entry_reads
                .load(std::sync::atomic::Ordering::Relaxed),
            payload_reads: self
                .counters
                .payload_reads
                .load(std::sync::atomic::Ordering::Relaxed),
            payload_bytes: self
                .counters
                .payload_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            locator_rows,
            locator_retained_bytes,
            graph_scans: 0,
            fsyncs: 0,
            journal_transitions: 0,
        }
    }
}

struct ManifestScanReader {
    file: File,
    overall: Sha256,
    entry: Option<Sha256>,
    position: u64,
    entry_start: u64,
}

impl ManifestScanReader {
    fn new(file: File) -> Self {
        Self {
            file,
            overall: Sha256::new(),
            entry: None,
            position: 0,
            entry_start: 0,
        }
    }

    fn begin_entry(&mut self) {
        self.entry_start = self.position;
        self.entry = Some(Sha256::new());
    }

    fn finish_entry(&mut self) -> (u64, BlobDescription) {
        let length = self.position - self.entry_start;
        let digest = self
            .entry
            .take()
            .expect("entry hashing is active")
            .finalize();
        (length, BlobDescription::from_parts(digest.into(), length))
    }

    fn finish(self) -> BlobDescription {
        BlobDescription::from_parts(self.overall.finalize().into(), self.position)
    }
}

impl Read for ManifestScanReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let read = self.file.read(output)?;
        self.overall.update(&output[..read]);
        if let Some(entry) = &mut self.entry {
            entry.update(&output[..read]);
        }
        self.position = self.position.saturating_add(read as u64);
        Ok(read)
    }
}

fn scan_promoted_manifest_once(
    publication: &Dir,
    binding: &PromotedBootstrapProjectionBindingV1,
    counters: &BootstrapProjectionRuntimeCounters,
) -> Result<(HashMap<ManagedPath, BootstrapProjectionEntryLocator>, u64), ShadowProjectionError> {
    let file = open_file_nofollow(publication, MANIFEST_FILE)?;
    let metadata = file.metadata()?;
    if !metadata_is_real_file(&metadata) || metadata.len() != binding.manifest.byte_length() {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "promoted shadow manifest has the wrong no-follow shape or length",
        ));
    }
    let mut reader = ManifestScanReader::new(file);
    let header_length = usize::try_from(binding.manifest_header.byte_length()).map_err(|_| {
        ShadowProjectionError::CorruptOrConflicting(
            "promoted shadow manifest header does not fit this platform",
        )
    })?;
    let mut header = vec![0_u8; header_length];
    reader.read_exact(&mut header).map_err(|_| {
        ShadowProjectionError::CorruptOrConflicting("promoted shadow manifest header is truncated")
    })?;
    if BlobDescription::of(&header) != binding.manifest_header {
        return Err(ShadowProjectionError::BindingMismatch(
            "promoted shadow manifest header changed",
        ));
    }

    let row_count = usize::try_from(binding.catalog_rows).map_err(|_| {
        ShadowProjectionError::ResourceLimit {
            resource: "promoted shadow locator rows",
            observed: binding.catalog_rows,
            limit: BOOTSTRAP_SOURCE_MAX_FILES,
        }
    })?;
    let mut locators = HashMap::new();
    locators
        .try_reserve(row_count)
        .map_err(|_| ShadowProjectionError::ResourceLimit {
            resource: "promoted shadow locator allocation",
            observed: binding.catalog_rows,
            limit: BOOTSTRAP_SOURCE_MAX_FILES,
        })?;
    let mut retained = (locators.capacity() as u64)
        .saturating_mul(
            std::mem::size_of::<(ManagedPath, BootstrapProjectionEntryLocator)>() as u64,
        );
    let mut previous = None::<ManagedPath>;
    let mut inventory = Sha256::new();
    inventory.update(b"tine/inactive-shadow-projection-inventory/v1\0");
    for _ in 0..row_count {
        reader.begin_entry();
        let evidence = read_manifest_evidence(&mut reader)?;
        let start = reader.entry_start;
        let (length, entry) = reader.finish_entry();
        if previous
            .as_ref()
            .is_some_and(|prior| prior >= &evidence.path)
        {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "promoted shadow manifest paths are duplicated or reordered",
            ));
        }
        validate_promoted_entry(binding, &evidence)?;
        hash_file_evidence(&mut inventory, &evidence)?;
        retained = retained
            .checked_add(evidence.path.as_str().len() as u64)
            .ok_or(ShadowProjectionError::ResourceLimit {
                resource: "promoted shadow locator retained bytes",
                observed: u64::MAX,
                limit: MAX_BOOTSTRAP_PROJECTION_LOCATOR_RETAINED_BYTES,
            })?;
        enforce_limit(
            "promoted shadow locator retained bytes",
            retained,
            MAX_BOOTSTRAP_PROJECTION_LOCATOR_RETAINED_BYTES,
        )?;
        let locator = BootstrapProjectionEntryLocator {
            offset: start,
            length,
            entry,
            page_id: evidence.page_id,
            kind: evidence.kind,
            source: evidence.source,
        };
        if locators.insert(evidence.path.clone(), locator).is_some() {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "promoted shadow manifest contains a duplicate path",
            ));
        }
        previous = Some(evidence.path);
    }
    let mut trailing = [0_u8; 1];
    if reader.read(&mut trailing)? != 0 {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "promoted shadow manifest contains trailing entries",
        ));
    }
    if reader.finish() != binding.manifest
        || ContentDigest::from_bytes(inventory.finalize().into()) != binding.staged_inventory_digest
    {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "promoted shadow manifest or inventory digest changed",
        ));
    }
    counters
        .manifest_scans
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok((locators, retained))
}

fn validate_promoted_entry(
    binding: &PromotedBootstrapProjectionBindingV1,
    evidence: &ShadowProjectionFileEvidence,
) -> Result<(), ShadowProjectionError> {
    let intent_bytes = evidence
        .intent
        .encode()
        .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?;
    if evidence.intent.workspace_id() != binding.workspace_id
        || evidence.intent.page_id() != evidence.page_id
        || evidence.intent.path() != &evidence.path
        || evidence.intent.target() != evidence.source
        || evidence.intent.precondition() != &ProjectionPrecondition::Base(evidence.source)
        || ProjectionIntent::decode(&intent_bytes)
            .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?
            != evidence.intent
    {
        return Err(ShadowProjectionError::BindingMismatch(
            "promoted shadow entry page, path, source, or intent binding differs",
        ));
    }
    Ok(())
}

fn bootstrap_entry_owner_binding(
    authority: ContentDigest,
    evidence: &ShadowProjectionFileEvidence,
) -> Result<ContentDigest, ShadowProjectionError> {
    let mut hasher = Sha256::new();
    hasher.update(b"tine/promoted-bootstrap-projection-owner/v1\0");
    hasher.update(authority.as_bytes());
    hasher.update(evidence.page_id.as_uuid().as_bytes());
    hasher.update((evidence.path.as_str().len() as u64).to_be_bytes());
    hasher.update(evidence.path.as_str().as_bytes());
    hasher.update(
        evidence
            .intent
            .id()
            .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?
            .as_bytes(),
    );
    Ok(ContentDigest::from_bytes(hasher.finalize().into()))
}

fn read_capability_file(
    directory: &Dir,
    name: &str,
    maximum: u64,
) -> Result<Vec<u8>, ShadowProjectionError> {
    let mut file = open_file_nofollow(directory, name)?;
    let metadata = file.metadata()?;
    if !metadata_is_real_file(&metadata) {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "promoted shadow evidence is not a regular no-follow file",
        ));
    }
    enforce_limit("promoted shadow evidence bytes", metadata.len(), maximum)?;
    let capacity =
        usize::try_from(metadata.len()).map_err(|_| ShadowProjectionError::ResourceLimit {
            resource: "promoted shadow evidence allocation",
            observed: metadata.len(),
            limit: maximum,
        })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| ShadowProjectionError::ResourceLimit {
            resource: "promoted shadow evidence allocation",
            observed: metadata.len(),
            limit: maximum,
        })?;
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_payload_at(
    payload: &Dir,
    path: &ManagedPath,
    expected: BlobDescription,
) -> Result<Vec<u8>, ShadowProjectionError> {
    validate_managed_path_depth(path)?;
    let mut components = path.as_str().split('/').peekable();
    let mut directory = payload.try_clone()?;
    let mut leaf = None;
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            leaf = Some(component);
        } else {
            directory = open_dir_nofollow(&directory, component)
                .map_err(|error| ShadowProjectionError::Io(io::Error::other(error.to_string())))?;
        }
    }
    let leaf = leaf.ok_or(ShadowProjectionError::CorruptOrConflicting(
        "promoted shadow payload path is empty",
    ))?;
    let mut file = open_file_nofollow(&directory, leaf)?;
    let metadata = file.metadata()?;
    if !metadata_is_real_file(&metadata) || metadata.len() != expected.byte_length() {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "promoted shadow payload has the wrong no-follow shape or length",
        ));
    }
    enforce_limit(
        "promoted shadow payload bytes",
        metadata.len(),
        BOOTSTRAP_SOURCE_MAX_FILE_BYTES,
    )?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(metadata.len() as usize)
        .map_err(|_| ShadowProjectionError::ResourceLimit {
            resource: "promoted shadow payload allocation",
            observed: metadata.len(),
            limit: BOOTSTRAP_SOURCE_MAX_FILE_BYTES,
        })?;
    file.read_to_end(&mut bytes)?;
    if BlobDescription::of(&bytes) != expected {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "promoted shadow payload digest changed",
        ));
    }
    Ok(bytes)
}

#[derive(Clone, Copy)]
struct SourceSummary {
    file_count: u64,
    chunk_count: u64,
    directory_count: u64,
    total_bytes: u64,
    max_path_bytes: u64,
    max_depth: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ValidatedCurrentPathCatalog {
    binding: CurrentPathCatalogBinding,
    rows: BTreeMap<ManagedPath, CurrentPathCatalogRow>,
}

#[derive(Clone, Copy)]
struct StagedInventoryProof {
    digest: ContentDigest,
    file_count: u64,
    total_bytes: u64,
}

struct PublicationPaths {
    parent: PathBuf,
    stage: PathBuf,
    final_directory: PathBuf,
}

/// Build or resume the exact-source inactive shadow projection and return a
/// typed proof only after semantic equivalence plus fresh source, backup,
/// SQLite, authority, root, and committed-byte rereads.
#[allow(clippy::too_many_arguments)]
pub(crate) fn verify_inactive_bootstrap_shadow_projection(
    graph: &Graph,
    roots: &MigrationBackupRoot,
    prepared: &InactiveBootstrapPreparedPublication,
    verified_publication: &InactiveBootstrapVerifiedPublication,
    source_backup: &VerifiedSourceBackup,
    authority: &InactiveBootstrapAcceptedAuthority,
    sqlite: &OpenProjection,
    sqlite_projection: &VerifiedBootstrapSqliteProjection,
) -> Result<VerifiedShadowProjection, ShadowProjectionError> {
    verify_inactive_bootstrap_shadow_projection_with_lookup_budget(
        graph,
        roots,
        prepared,
        verified_publication,
        source_backup,
        authority,
        sqlite,
        sqlite_projection,
        super::hot_engine::BOOTSTRAP_LOOKUP_SESSION_BYTES_PER_ROOT,
    )
}

#[allow(clippy::too_many_arguments)]
fn verify_inactive_bootstrap_shadow_projection_with_lookup_budget(
    graph: &Graph,
    roots: &MigrationBackupRoot,
    prepared: &InactiveBootstrapPreparedPublication,
    verified_publication: &InactiveBootstrapVerifiedPublication,
    source_backup: &VerifiedSourceBackup,
    authority: &InactiveBootstrapAcceptedAuthority,
    sqlite: &OpenProjection,
    sqlite_projection: &VerifiedBootstrapSqliteProjection,
    session_budget_bytes_per_root: usize,
) -> Result<VerifiedShadowProjection, ShadowProjectionError> {
    #[cfg(test)]
    {
        let mut calls = complete_shadow_verification_calls().lock().unwrap();
        let calls = calls.entry(authority.binding().workspace_id()).or_default();
        *calls = calls.saturating_add(1);
    }
    validate_bindings(
        graph,
        roots,
        prepared,
        verified_publication,
        source_backup,
        authority,
        sqlite,
        sqlite_projection,
    )?;
    let capture = prepared.source_capture();
    let authoritative_paths = bootstrap_authoritative_source_paths(capture).map_err(|_| {
        ShadowProjectionError::CorruptOrConflicting(
            "source collision-authority selection is invalid",
        )
    })?;
    let summary = summarize_source(capture, &authoritative_paths)?;
    let catalog = traverse_complete_catalog(authority, &authoritative_paths)?;
    let catalog_binding = catalog.binding;
    let publication_id = shadow_publication_id(
        roots,
        prepared,
        source_backup,
        authority.binding(),
        sqlite_projection,
        catalog_binding,
        summary,
    )?;
    let paths = publication_paths(roots, authority.binding(), publication_id)?;

    // This is deliberately the last live-source action before staging.
    capture.verify_before_inactive_bootstrap_authoring(graph)?;

    ensure_publication_parent(roots, authority.binding(), &paths)?;
    let mut instrumentation = ShadowProjectionInstrumentation {
        catalog_rows: catalog_binding.catalog_rows(),
        source_files: summary.file_count,
        source_chunks: summary.chunk_count,
        peak_owned_catalog_rows: summary.file_count.min(CATALOG_PAGE_ROWS as u64),
        ..ShadowProjectionInstrumentation::default()
    };
    let final_exists = path_exists(&paths.final_directory)?;
    let stage_exists = path_exists(&paths.stage)?;
    if final_exists && stage_exists {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "both staged and final shadow projection directories exist",
        ));
    }
    let header = manifest_header(
        roots,
        prepared,
        source_backup,
        authority.binding(),
        sqlite_projection,
        catalog_binding,
        publication_id,
        summary,
    )?;
    let mut adjacent_construction = None;
    if !final_exists {
        ensure_real_directory_created(&paths.stage)?;
        ensure_real_directory_created(&paths.stage.join(PAYLOAD_DIRECTORY))?;
        let constructed = publish_payloads_and_manifest(
            &paths.stage.join(PAYLOAD_DIRECTORY),
            &paths.stage.join(MANIFEST_FILE),
            &header,
            prepared,
            authority,
            &catalog,
            &mut instrumentation,
            session_budget_bytes_per_root,
        )?;
        sync_tree(
            &paths.stage.join(PAYLOAD_DIRECTORY),
            summary,
            &mut instrumentation,
        )?;
        inject_crash_cut(ShadowProjectionCrashCut::AfterPayloadPublication)?;
        sync_file_and_parent(&paths.stage.join(MANIFEST_FILE))?;
        inject_crash_cut(ShadowProjectionCrashCut::AfterManifestPublication)?;
        validate_projection_root_entries(&paths.stage, false)?;
        verify_projection_directory_against_proof(
            &paths.stage,
            false,
            &header,
            summary,
            constructed,
            &mut instrumentation,
        )?;
        sync_directory(&paths.stage)?;
        move_file_noreplace(&paths.stage, &paths.final_directory).map_err(|_| {
            ShadowProjectionError::CorruptOrConflicting(
                "final shadow destination appeared or staged rename failed",
            )
        })?;
        inject_crash_cut(ShadowProjectionCrashCut::AfterStagingRename)?;
        adjacent_construction = Some(constructed);
    }
    sync_directory_barrier(
        &paths.parent,
        ShadowProjectionDurabilityBarrier::PublicationParentAfterFinal,
    )?;

    let (manifest, staged) = if let Some(constructed) = adjacent_construction {
        verify_projection_directory_against_proof(
            &paths.final_directory,
            true,
            &header,
            summary,
            constructed,
            &mut instrumentation,
        )?;
        constructed
    } else {
        verify_projection_directory(
            &paths.final_directory,
            true,
            &header,
            prepared,
            authority,
            &catalog,
            summary,
            &mut instrumentation,
            session_budget_bytes_per_root,
        )?
    };
    let proof_bytes = proof_bytes(
        roots,
        prepared,
        source_backup,
        authority.binding(),
        sqlite_projection,
        catalog_binding,
        publication_id,
        summary,
        manifest,
        staged,
    )?;
    let proof = publish_small_file_atomic(
        &paths.final_directory,
        PROOF_STAGE_FILE,
        PROOF_FILE,
        &proof_bytes,
        ShadowProjectionCrashCut::PartialProofWrite,
        "shadow proof conflicts with existing evidence",
    )?;
    inject_crash_cut(ShadowProjectionCrashCut::AfterProofPublication)?;
    let (marker_bytes, evidence_digest) = commit_marker_bytes(
        roots,
        prepared,
        source_backup,
        authority.binding(),
        sqlite_projection,
        catalog_binding,
        publication_id,
        summary,
        manifest,
        proof,
        staged,
    )?;
    let commit_marker = publish_small_file_atomic(
        &paths.final_directory,
        COMMIT_MARKER_STAGE_FILE,
        COMMIT_MARKER_FILE,
        &marker_bytes,
        ShadowProjectionCrashCut::PartialCommitMarkerWrite,
        "shadow commit marker conflicts with existing evidence",
    )?;
    inject_crash_cut(ShadowProjectionCrashCut::AfterCommitMarkerPublication)?;

    // Freshly reread all retained authorities and every committed staged byte.
    sqlite
        .database
        .freshly_verify_inactive_bootstrap(authority, sqlite_projection)
        .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?;
    roots.freshly_validate_retained_roots()?;
    let final_catalog = traverse_complete_catalog(authority, &authoritative_paths)?;
    if final_catalog != catalog {
        return Err(ShadowProjectionError::BindingMismatch(
            "accepted current-path catalog changed during shadow projection",
        ));
    }
    verify_projection_directory_against_proof(
        &paths.final_directory,
        true,
        &header,
        summary,
        (manifest, staged),
        &mut instrumentation,
    )?;
    compare_exact_small_file(
        &paths.final_directory.join(PROOF_FILE),
        &proof_bytes,
        "shadow proof changed before final proof",
    )?;
    compare_exact_small_file(
        &paths.final_directory.join(COMMIT_MARKER_FILE),
        &marker_bytes,
        "shadow commit marker changed before final proof",
    )?;
    validate_projection_root_entries(&paths.final_directory, true)?;
    if !path_exists(&paths.final_directory.join(PROOF_FILE))?
        || !path_exists(&paths.final_directory.join(COMMIT_MARKER_FILE))?
        || path_exists(&paths.final_directory.join(PROOF_STAGE_FILE))?
        || path_exists(&paths.final_directory.join(COMMIT_MARKER_STAGE_FILE))?
    {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "committed shadow projection is missing final proof evidence",
        ));
    }
    sync_directory(&paths.final_directory)?;
    // This is deliberately the final live-graph observation. No graph path is
    // opened for write anywhere in this module.
    before_final_source_verify_hook()?;
    capture.verify_before_inactive_bootstrap_authoring(graph)?;

    Ok(VerifiedShadowProjection {
        directory: paths.final_directory,
        workspace_id: authority.binding().workspace_id(),
        graph_resource: authority.binding().graph_resource(),
        physical_root_identity: roots.root_identity(),
        publication_id,
        source_capture: capture.capture_identity()?,
        source_inventory: capture.inventory_description(),
        source_entries: capture.entries_description(),
        source_chunks: capture.chunks_description(),
        file_count: summary.file_count,
        chunk_count: summary.chunk_count,
        directory_count: summary.directory_count,
        total_bytes: summary.total_bytes,
        catalog_binding,
        authority_binding: authority.binding().clone(),
        source_backup: source_backup.clone(),
        sqlite_projection: sqlite_projection.clone(),
        manifest,
        staged_inventory_digest: staged.digest,
        staged_file_count: staged.file_count,
        staged_total_bytes: staged.total_bytes,
        proof,
        commit_marker,
        evidence_digest,
        schema: ShadowProjectionSchemaBinding::CURRENT,
        instrumentation,
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_bindings(
    graph: &Graph,
    roots: &MigrationBackupRoot,
    prepared: &InactiveBootstrapPreparedPublication,
    verified: &InactiveBootstrapVerifiedPublication,
    backup: &VerifiedSourceBackup,
    authority: &InactiveBootstrapAcceptedAuthority,
    sqlite: &OpenProjection,
    sqlite_proof: &VerifiedBootstrapSqliteProjection,
) -> Result<(), ShadowProjectionError> {
    roots.freshly_validate_retained_roots()?;
    let aggregate = prepared.aggregate();
    let binding = authority.binding();
    if graph.canonical_resource_id()? != roots.graph_resource()
        || prepared.source_capture().graph_resource() != roots.graph_resource()
        || aggregate.workspace_id() != binding.workspace_id()
        || aggregate.lineage_digest() != binding.lineage_digest()
        || aggregate.graph_resource() != binding.graph_resource()
        || aggregate.publication_id() != binding.publication_id()
        || aggregate.aggregate_digest() != binding.aggregate_digest()
        || aggregate.import_id() != binding.import_id()
        || aggregate.parts().len() as u32 != binding.part_count()
        || aggregate.final_frontier().last_part() != binding.predecessor_terminal()
        || prepared
            .candidate()
            .accepted_frontier_root()
            .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?
            != *binding.accepted_frontier()
        || !prepared
            .candidate()
            .durable_history_binding()
            .same_replay_authority(binding.engine_binding())
        || verified.workspace_id() != binding.workspace_id()
        || verified.lineage_digest() != binding.lineage_digest()
        || verified.graph_resource() != binding.graph_resource()
        || verified.publication_id() != binding.publication_id()
        || verified.aggregate_digest() != binding.aggregate_digest()
        || verified.import_id() != binding.import_id()
        || verified.accepted_frontier() != binding.accepted_frontier()
        || verified.engine_binding() != binding.engine_binding()
        || verified.storage_binding() != binding.storage_binding()
        || verified.bootstrap_binding() != binding.bootstrap_binding()
        || verified.archive_identity() != binding.archive_identity()
        || verified.history_generation() != binding.history_generation()
        || verified.history_root() != binding.history_root()
        || verified.cold_record_count() != binding.cold_record_count()
        || backup.workspace_id() != binding.workspace_id()
        || backup.graph_resource() != binding.graph_resource()
        || backup.backup_root_identity() != roots.root_identity()
        || backup.publication_id() != binding.publication_id().as_bytes()
        || backup.aggregate_digest() != binding.aggregate_digest().as_bytes()
        || backup.source_inventory() != prepared.source_capture().inventory_description()
        || sqlite_proof.authority_binding() != binding
    {
        return Err(ShadowProjectionError::BindingMismatch(
            "shadow projection inputs do not bind the same inactive bootstrap",
        ));
    }
    sqlite
        .database
        .freshly_verify_inactive_bootstrap(authority, sqlite_proof)
        .map_err(|error| ShadowProjectionError::Projection(error.to_string()))
}

fn summarize_source(
    capture: &BootstrapSourceCapture,
    authoritative: &HashSet<ManagedPath>,
) -> Result<SourceSummary, ShadowProjectionError> {
    let mut entries = capture.entries_cursor()?;
    let mut file_count = 0_u64;
    let mut chunk_count = 0_u64;
    let mut captured_file_count = 0_u64;
    let mut captured_chunk_count = 0_u64;
    let mut directory_count = 0_u64;
    let mut total_bytes = 0_u64;
    let mut max_path_bytes = 0_u64;
    let mut max_depth = 0_usize;
    let mut previous_parents = Vec::<String>::new();
    while let Some(entry) = entries.next()? {
        validate_source_entry(&entry)?;
        captured_file_count = checked_add(captured_file_count, 1, "captured source files")?;
        captured_chunk_count = checked_add(
            captured_chunk_count,
            u64::from(entry.chunk_count()),
            "captured source chunks",
        )?;
        if !authoritative.contains(entry.path()) {
            continue;
        }
        file_count = checked_add(file_count, 1, "source files")?;
        chunk_count = checked_add(chunk_count, u64::from(entry.chunk_count()), "source chunks")?;
        total_bytes = checked_add(
            total_bytes,
            entry.description().byte_length(),
            "source bytes",
        )?;
        enforce_limit("source files", file_count, BOOTSTRAP_SOURCE_MAX_FILES)?;
        enforce_limit(
            "source bytes",
            total_bytes,
            BOOTSTRAP_SOURCE_MAX_TOTAL_BYTES,
        )?;
        max_path_bytes = max_path_bytes.max(entry.path().as_str().len() as u64);
        let components = entry.path().as_str().split('/').collect::<Vec<_>>();
        let depth = components.len();
        enforce_limit(
            "source path depth",
            depth as u64,
            BOOTSTRAP_SOURCE_MAX_DIRECTORY_DEPTH.saturating_add(1) as u64,
        )?;
        max_depth = max_depth.max(depth);
        let parents = &components[..components.len().saturating_sub(1)];
        let common = parents
            .iter()
            .zip(&previous_parents)
            .take_while(|(left, right)| **left == right.as_str())
            .count();
        directory_count = checked_add(
            directory_count,
            (parents.len() - common) as u64,
            "source directories",
        )?;
        enforce_limit(
            "source directories",
            directory_count,
            BOOTSTRAP_SOURCE_MAX_DIRECTORIES,
        )?;
        previous_parents.clear();
        previous_parents.extend(parents.iter().map(|value| (*value).to_owned()));
    }
    if captured_file_count != capture.source_file_count()
        || captured_chunk_count != capture.source_chunk_count()
    {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "source cursor counts differ from sealed capture",
        ));
    }
    Ok(SourceSummary {
        file_count,
        chunk_count,
        directory_count,
        total_bytes,
        max_path_bytes,
        max_depth,
    })
}

fn traverse_complete_catalog(
    authority: &InactiveBootstrapAcceptedAuthority,
    authoritative_paths: &HashSet<ManagedPath>,
) -> Result<ValidatedCurrentPathCatalog, ShadowProjectionError> {
    let engine = authority.accepted_engine();
    let binding = engine
        .current_path_catalog_binding()
        .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?;
    let mut cursor = Some(
        engine
            .begin_current_path_cursor()
            .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?,
    );
    let mut count = 0_u64;
    let mut seen = HashSet::new();
    let mut by_path = BTreeMap::new();
    while let Some(token) = cursor.take() {
        let page = engine
            .current_path_cursor_page(
                token,
                CATALOG_PAGE_ROWS.min(MAX_CURRENT_PATH_CURSOR_PAGE_ROWS),
            )
            .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?;
        let (page_rows, next) = page.into_parts();
        count = checked_add(count, page_rows.len() as u64, "current-path catalog rows")?;
        for row in page_rows {
            if !authoritative_paths.contains(row.path())
                || !seen.insert(row.path().clone())
                || by_path.insert(row.path().clone(), row).is_some()
            {
                return Err(ShadowProjectionError::BindingMismatch(
                    "current-path catalog grants authority outside the selected source winners",
                ));
            }
        }
        cursor = next;
    }
    let current = engine
        .current_path_catalog_binding()
        .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?;
    if binding != current {
        return Err(ShadowProjectionError::BindingMismatch(
            "current-path catalog binding changed during complete traversal",
        ));
    }
    if count != binding.catalog_rows()
        || count != authoritative_paths.len() as u64
        || seen != *authoritative_paths
    {
        return Err(ShadowProjectionError::BindingMismatch(
            "complete current-path catalog does not exactly cover the source capture",
        ));
    }
    if binding.workspace_id() != authority.binding().workspace_id()
        || binding.lineage_digest() != authority.binding().lineage_digest()
    {
        return Err(ShadowProjectionError::BindingMismatch(
            "current-path catalog workspace or lineage differs from bootstrap authority",
        ));
    }
    if binding.accepted_frontier() != authority.binding().accepted_frontier().state_digest() {
        return Err(ShadowProjectionError::BindingMismatch(
            "current-path catalog frontier differs from bootstrap authority",
        ));
    }
    Ok(ValidatedCurrentPathCatalog {
        binding,
        rows: by_path,
    })
}

fn read_source_file(
    capture: &BootstrapSourceCapture,
    entry: &BootstrapSourceEntry,
    chunks: &mut BootstrapSourceChunkCursor,
    instrumentation: &mut ShadowProjectionInstrumentation,
) -> Result<Vec<u8>, ShadowProjectionError> {
    let length = usize::try_from(entry.description().byte_length()).map_err(|_| {
        ShadowProjectionError::ResourceLimit {
            resource: "source file allocation",
            observed: entry.description().byte_length(),
            limit: usize::MAX as u64,
        }
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| ShadowProjectionError::ResourceLimit {
            resource: "source file allocation",
            observed: length as u64,
            limit: BOOTSTRAP_SOURCE_MAX_FILE_BYTES,
        })?;
    let mut buffer = [0_u8; IO_BUFFER_BYTES];
    for ordinal in 0..entry.chunk_count() {
        let chunk = chunks
            .next()?
            .ok_or(ShadowProjectionError::CorruptOrConflicting(
                "source chunk spool ended before its entry",
            ))?;
        if chunk.path() != entry.path() || chunk.ordinal() != ordinal {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "source chunk spool does not match source entry",
            ));
        }
        let mut reader = capture.open_chunk(&chunk)?;
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            instrumentation.source_bytes_read = checked_add(
                instrumentation.source_bytes_read,
                read as u64,
                "source bytes read",
            )?;
        }
        reader.finish()?;
    }
    instrumentation.peak_owned_source_bytes = instrumentation
        .peak_owned_source_bytes
        .max(bytes.capacity() as u64);
    if BlobDescription::of(&bytes) != entry.description() {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "reconstructed source bytes differ from sealed entry",
        ));
    }
    Ok(bytes)
}

fn plan_exact_source(
    catalog: &ValidatedCurrentPathCatalog,
    entry: &BootstrapSourceEntry,
    source: &[u8],
    state: Option<&ProjectionPageState>,
    instrumentation: &mut ShadowProjectionInstrumentation,
) -> Result<Option<(CurrentPathCatalogRow, ProjectionIntent)>, ShadowProjectionError> {
    let Some(row) = catalog.rows.get(entry.path()) else {
        // The complete catalog traversal proves no skipped path has authority.
        return Ok(None);
    };
    if row.path() != entry.path() || row.kind() != entry.kind() {
        return Err(ShadowProjectionError::BindingMismatch(
            "source path kind differs from accepted catalog",
        ));
    }
    let state = state.ok_or(ShadowProjectionError::BindingMismatch(
        "authoritative source path is missing its bounded materialization",
    ))?;
    if state.page.page_id != row.page_id() || state.page.path != *entry.path() {
        return Err(ShadowProjectionError::BindingMismatch(
            "materialized page identity or path differs from source and accepted catalog",
        ));
    }
    if state.page.kind != entry.kind() {
        let label = |kind| match kind {
            ManagedTextKind::Page => "page",
            ManagedTextKind::Journal => "journal",
        };
        return Err(ShadowProjectionError::SemanticMismatch {
            path: entry.path().as_str().to_owned(),
            difference: ExactSourceSemanticDifference::PageKind {
                accepted: label(state.page.kind),
                source: label(entry.kind()),
            },
        });
    }
    if state.page.name.as_str() != entry.logical_name() {
        return Err(ShadowProjectionError::SemanticMismatch {
            path: entry.path().as_str().to_owned(),
            difference: ExactSourceSemanticDifference::PageName {
                accepted: state.page.name.as_str().to_owned(),
                source: entry.logical_name().to_owned(),
            },
        });
    }
    if ContentDigest::of(state.page.name.as_str().as_bytes()) != row.accepted_name_digest() {
        return Err(ShadowProjectionError::BindingMismatch(
            "materialized page logical name differs from the accepted catalog digest",
        ));
    }
    let plan =
        plan_projection_adopting_exact_source(binding_workspace(catalog.binding), state, source)
            .map_err(|error| match error {
                ExactSourceProjectionError::Projection(error) => {
                    ShadowProjectionError::Projection(error.to_string())
                }
                ExactSourceProjectionError::Semantic(difference) => {
                    ShadowProjectionError::SemanticMismatch {
                        path: entry.path().as_str().to_owned(),
                        difference,
                    }
                }
            })?;
    instrumentation.projection_plans =
        checked_add(instrumentation.projection_plans, 1, "projection plans")?;
    require_exact_source_baseline(
        plan.target(),
        plan.intent(),
        source,
        row.page_id(),
        entry.path(),
        entry.description(),
    )?;
    Ok(Some((row.clone(), plan.intent().clone())))
}

fn binding_workspace(binding: CurrentPathCatalogBinding) -> WorkspaceId {
    binding.workspace_id()
}

fn require_exact_source_baseline(
    target: &[u8],
    intent: &ProjectionIntent,
    source: &[u8],
    page_id: PageId,
    path: &ManagedPath,
    description: BlobDescription,
) -> Result<(), ShadowProjectionError> {
    let detail = if target != source {
        target
            .iter()
            .zip(source)
            .position(|(projected, captured)| projected != captured)
            .map_or(
                NormalSparseMismatchDetail::CommonPrefixEnded,
                NormalSparseMismatchDetail::FirstDifferingByte,
            )
    } else {
        let mut checks = Vec::new();
        if intent.page_id() != page_id {
            checks.push("intent page");
        }
        if intent.path() != path {
            checks.push("intent path");
        }
        if intent.target() != description {
            checks.push("intent target");
        }
        if intent.precondition() != &ProjectionPrecondition::Base(description) {
            checks.push("intent precondition");
        }
        if checks.is_empty() {
            return Ok(());
        }
        NormalSparseMismatchDetail::BindingChecks(checks)
    };
    Err(ShadowProjectionError::NormalSparseMismatch {
        path: path.as_str().to_owned(),
        source_bytes: source.len(),
        projected_bytes: target.len(),
        detail,
    })
}

fn publish_payloads_and_manifest(
    payload: &Path,
    manifest_path: &Path,
    header: &[u8],
    prepared: &InactiveBootstrapPreparedPublication,
    authority: &InactiveBootstrapAcceptedAuthority,
    catalog: &ValidatedCurrentPathCatalog,
    instrumentation: &mut ShadowProjectionInstrumentation,
    session_budget_bytes_per_root: usize,
) -> Result<(BlobDescription, StagedInventoryProof), ShadowProjectionError> {
    let mut output = ResumableExactFile::open(
        manifest_path,
        "shadow manifest conflicts with staged exact bytes",
    )?;
    if header.len() > 1 && take_crash_cut(ShadowProjectionCrashCut::PartialManifestWrite) {
        let prefix = (header.len() / 2).clamp(1, header.len() - 1);
        output.write_all(&header[..prefix]).map_err(|_| {
            ShadowProjectionError::CorruptOrConflicting(
                "shadow manifest conflicts with staged exact bytes",
            )
        })?;
        output.flush()?;
        return Err(ShadowProjectionError::InjectedCrashCut(
            ShadowProjectionCrashCut::PartialManifestWrite.label(),
        ));
    }
    output.write_all(header).map_err(|_| {
        ShadowProjectionError::CorruptOrConflicting(
            "shadow manifest conflicts with staged exact bytes",
        )
    })?;
    let capture = prepared.source_capture();
    let mut entries = capture.entries_cursor()?;
    let mut chunks = capture.chunks_cursor()?;
    let materializer = (!catalog.rows.is_empty())
        .then(|| {
            authority
                .accepted_engine()
                .bootstrap_bulk_materializer_with_session_budget(
                    authority.binding().accepted_frontier(),
                    session_budget_bytes_per_root,
                )
                .map_err(|error| ShadowProjectionError::Projection(error.to_string()))
        })
        .transpose()?;
    let mut first_write = true;
    let mut inventory = Sha256::new();
    inventory.update(b"tine/inactive-shadow-projection-inventory/v1\0");
    let mut file_count = 0_u64;
    let mut total_bytes = 0_u64;
    loop {
        let mut entry_chunk = Vec::with_capacity(BOOTSTRAP_MATERIALIZATION_CHUNK_PAGES);
        while entry_chunk.len() < BOOTSTRAP_MATERIALIZATION_CHUNK_PAGES {
            let Some(entry) = entries.next()? else {
                break;
            };
            entry_chunk.push(entry);
        }
        if entry_chunk.is_empty() {
            break;
        }
        let page_ids = entry_chunk
            .iter()
            .filter_map(|entry| {
                catalog
                    .rows
                    .get(entry.path())
                    .map(CurrentPathCatalogRow::page_id)
            })
            .collect::<Vec<_>>();
        if !page_ids.is_empty() {
            instrumentation.bulk_materialization_chunks = checked_add(
                instrumentation.bulk_materialization_chunks,
                1,
                "shadow bulk materialization chunks",
            )?;
            instrumentation.bulk_pages_materialized = checked_add(
                instrumentation.bulk_pages_materialized,
                page_ids.len() as u64,
                "shadow bulk materialized pages",
            )?;
            instrumentation.peak_bulk_pages =
                instrumentation.peak_bulk_pages.max(page_ids.len() as u64);
        }
        let states = materializer
            .as_ref()
            .expect("a nonempty catalog constructed a bulk materializer")
            .materialize_pages_for_projection(&page_ids)
            .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?;
        let states = page_ids
            .into_iter()
            .zip(states)
            .filter_map(|(page_id, state)| state.map(|state| (page_id, state)))
            .collect::<BTreeMap<_, _>>();
        for entry in entry_chunk {
            let source = read_source_file(capture, &entry, &mut chunks, instrumentation)?;
            let state = catalog
                .rows
                .get(entry.path())
                .and_then(|row| states.get(&row.page_id()));
            let Some((row, intent)) =
                plan_exact_source(catalog, &entry, &source, state, instrumentation)?
            else {
                continue;
            };
            let destination = payload_path(payload, entry.path())?;
            ensure_managed_parent_directories(payload, entry.path())?;
            if first_write
                && source.len() > 1
                && take_crash_cut(ShadowProjectionCrashCut::PartialPayloadWrite)
            {
                let mut output = ResumableExactFile::open(
                    &destination,
                    "shadow payload conflicts with staged exact bytes",
                )?;
                let prefix = (source.len() / 2).clamp(1, source.len() - 1);
                output.write_all(&source[..prefix]).map_err(|_| {
                    ShadowProjectionError::CorruptOrConflicting(
                        "shadow payload partial write failed",
                    )
                })?;
                output.flush()?;
                return Err(ShadowProjectionError::InjectedCrashCut(
                    ShadowProjectionCrashCut::PartialPayloadWrite.label(),
                ));
            }
            let mut payload_output = ResumableExactFile::open(
                &destination,
                "shadow payload conflicts with staged exact bytes",
            )?;
            payload_output.write_all(&source).map_err(|_| {
                ShadowProjectionError::CorruptOrConflicting(
                    "shadow payload conflicts with staged exact bytes",
                )
            })?;
            let description = payload_output.finish_payload()?;
            if description != entry.description() {
                return Err(ShadowProjectionError::CorruptOrConflicting(
                    "staged payload description differs from captured source",
                ));
            }
            emit_manifest_entry(&mut output, &entry, row.page_id(), &intent)?;
            instrumentation.manifest_entries =
                checked_add(instrumentation.manifest_entries, 1, "manifest entries")?;
            let evidence = ShadowProjectionFileEvidence {
                path: entry.path().clone(),
                kind: entry.kind(),
                logical_name: entry.logical_name().to_owned(),
                page_id: row.page_id(),
                source: entry.description(),
                source_file_resource: entry.file_resource(),
                source_link_count: entry.link_count(),
                source_chunk_count: entry.chunk_count(),
                intent,
            };
            hash_file_evidence(&mut inventory, &evidence)?;
            file_count = checked_add(file_count, 1, "published shadow files")?;
            total_bytes = checked_add(
                total_bytes,
                description.byte_length(),
                "published shadow bytes",
            )?;
            instrumentation.payload_bytes_written = checked_add(
                instrumentation.payload_bytes_written,
                source.len() as u64,
                "payload bytes written",
            )?;
            first_write = false;
        }
    }
    if chunks.next()?.is_some() {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "source chunk spool contains extra entries",
        ));
    }
    record_bulk_lookup_session_stats(instrumentation, materializer.as_ref())?;
    if authority
        .accepted_engine()
        .current_path_catalog_binding()
        .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?
        != catalog.binding
    {
        return Err(ShadowProjectionError::BindingMismatch(
            "accepted catalog changed during payload publication",
        ));
    }
    let description = output.finish()?;
    enforce_limit(
        "shadow manifest bytes",
        description.byte_length(),
        MAX_MANIFEST_BYTES,
    )?;
    Ok((
        description,
        StagedInventoryProof {
            digest: ContentDigest::from_bytes(inventory.finalize().into()),
            file_count,
            total_bytes,
        },
    ))
}

fn session_stat_u64(value: usize, resource: &'static str) -> Result<u64, ShadowProjectionError> {
    u64::try_from(value).map_err(|_| ShadowProjectionError::ResourceLimit {
        resource,
        observed: u64::MAX,
        limit: u64::MAX - 1,
    })
}

fn record_bulk_lookup_session_stats(
    instrumentation: &mut ShadowProjectionInstrumentation,
    materializer: Option<&super::hot_engine::BootstrapBulkMaterializer<'_>>,
) -> Result<(), ShadowProjectionError> {
    let Some(materializer) = materializer else {
        return Ok(());
    };
    let (accepted, external) = materializer.lookup_session_stats();
    instrumentation.accepted_frontier_session_hits = checked_add(
        instrumentation.accepted_frontier_session_hits,
        session_stat_u64(accepted.hits, "accepted-frontier session hits")?,
        "accepted-frontier session hits",
    )?;
    instrumentation.accepted_frontier_session_misses = checked_add(
        instrumentation.accepted_frontier_session_misses,
        session_stat_u64(accepted.misses, "accepted-frontier session misses")?,
        "accepted-frontier session misses",
    )?;
    instrumentation.accepted_frontier_session_evictions = checked_add(
        instrumentation.accepted_frontier_session_evictions,
        session_stat_u64(accepted.evictions, "accepted-frontier session evictions")?,
        "accepted-frontier session evictions",
    )?;
    instrumentation.accepted_frontier_session_oversize = checked_add(
        instrumentation.accepted_frontier_session_oversize,
        session_stat_u64(accepted.oversize, "accepted-frontier session oversize")?,
        "accepted-frontier session oversize",
    )?;
    instrumentation.accepted_frontier_session_peak_resident_bytes = instrumentation
        .accepted_frontier_session_peak_resident_bytes
        .max(session_stat_u64(
            accepted.peak_resident_bytes,
            "accepted-frontier session resident bytes",
        )?);
    instrumentation.external_exact_session_hits = checked_add(
        instrumentation.external_exact_session_hits,
        session_stat_u64(external.hits, "external-exact session hits")?,
        "external-exact session hits",
    )?;
    instrumentation.external_exact_session_misses = checked_add(
        instrumentation.external_exact_session_misses,
        session_stat_u64(external.misses, "external-exact session misses")?,
        "external-exact session misses",
    )?;
    instrumentation.external_exact_session_evictions = checked_add(
        instrumentation.external_exact_session_evictions,
        session_stat_u64(external.evictions, "external-exact session evictions")?,
        "external-exact session evictions",
    )?;
    instrumentation.external_exact_session_oversize = checked_add(
        instrumentation.external_exact_session_oversize,
        session_stat_u64(external.oversize, "external-exact session oversize")?,
        "external-exact session oversize",
    )?;
    instrumentation.external_exact_session_peak_resident_bytes = instrumentation
        .external_exact_session_peak_resident_bytes
        .max(session_stat_u64(
            external.peak_resident_bytes,
            "external-exact session resident bytes",
        )?);
    Ok(())
}

fn emit_manifest_entry(
    output: &mut impl Write,
    entry: &BootstrapSourceEntry,
    page_id: PageId,
    intent: &ProjectionIntent,
) -> Result<(), ShadowProjectionError> {
    write_len_prefixed(output, entry.path().as_str().as_bytes())?;
    output.write_all(&[match entry.kind() {
        ManagedTextKind::Page => 0,
        ManagedTextKind::Journal => 1,
    }])?;
    write_len_prefixed(output, entry.logical_name().as_bytes())?;
    output.write_all(page_id.as_uuid().as_bytes())?;
    write_description(output, entry.description())?;
    output.write_all(entry.file_resource().as_bytes())?;
    output.write_all(&entry.link_count().to_be_bytes())?;
    output.write_all(&entry.chunk_count().to_be_bytes())?;
    let intent_bytes = intent
        .encode()
        .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?;
    enforce_limit(
        "projection intent bytes",
        intent_bytes.len() as u64,
        MAX_MANIFEST_ENTRY_BYTES as u64,
    )?;
    write_len_prefixed(output, &intent_bytes)?;
    write_description(output, BlobDescription::of(&intent_bytes))
}

/// Recheck durable shape and the compact authenticated inventory without
/// replaying projection semantics or rereading every payload. The proof can
/// only be minted by the adjacent construction/semantic-recovery pass.
fn verify_projection_directory_against_proof(
    directory: &Path,
    final_directory: bool,
    header: &[u8],
    summary: SourceSummary,
    expected: (BlobDescription, StagedInventoryProof),
    instrumentation: &mut ShadowProjectionInstrumentation,
) -> Result<(), ShadowProjectionError> {
    require_real_directory(directory, "shadow projection is not a real directory")?;
    validate_projection_root_entries(directory, final_directory)?;
    let counts = traverse_tree_bounded(
        &directory.join(PAYLOAD_DIRECTORY),
        summary,
        false,
        instrumentation,
    )?;
    if counts.files != summary.file_count
        || counts.directories != summary.directory_count
        || counts.bytes != summary.total_bytes
    {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "shadow payload changed after its semantic construction pass",
        ));
    }
    let manifest_path = directory.join(MANIFEST_FILE);
    if describe_regular_file(&manifest_path, MAX_MANIFEST_BYTES)? != expected.0 {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "shadow manifest changed after its semantic construction pass",
        ));
    }
    let mut reader = ManifestReader::open(&manifest_path, summary.file_count, header.to_vec())?;
    let mut inventory = Sha256::new();
    inventory.update(b"tine/inactive-shadow-projection-inventory/v1\0");
    let mut file_count = 0_u64;
    let mut total_bytes = 0_u64;
    while let Some(evidence) = reader.next()? {
        hash_file_evidence(&mut inventory, &evidence)?;
        file_count = checked_add(file_count, 1, "authenticated shadow files")?;
        total_bytes = checked_add(
            total_bytes,
            evidence.source.byte_length(),
            "authenticated shadow bytes",
        )?;
    }
    reader.finish()?;
    let actual = StagedInventoryProof {
        digest: ContentDigest::from_bytes(inventory.finalize().into()),
        file_count,
        total_bytes,
    };
    if actual.digest != expected.1.digest
        || actual.file_count != expected.1.file_count
        || actual.total_bytes != expected.1.total_bytes
    {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "shadow inventory changed after its semantic construction pass",
        ));
    }
    Ok(())
}

fn verify_projection_directory(
    directory: &Path,
    final_directory: bool,
    header: &[u8],
    prepared: &InactiveBootstrapPreparedPublication,
    authority: &InactiveBootstrapAcceptedAuthority,
    catalog: &ValidatedCurrentPathCatalog,
    summary: SourceSummary,
    instrumentation: &mut ShadowProjectionInstrumentation,
    session_budget_bytes_per_root: usize,
) -> Result<(BlobDescription, StagedInventoryProof), ShadowProjectionError> {
    require_real_directory(directory, "shadow projection is not a real directory")?;
    validate_projection_root_entries(directory, final_directory)?;
    let payload = directory.join(PAYLOAD_DIRECTORY);
    let counts = traverse_tree_bounded(&payload, summary, false, instrumentation)?;
    if counts.files != summary.file_count
        || counts.directories != summary.directory_count
        || counts.bytes != summary.total_bytes
    {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "shadow payload has missing or extra files or directories",
        ));
    }
    let manifest_path = directory.join(MANIFEST_FILE);
    let manifest = describe_regular_file(&manifest_path, MAX_MANIFEST_BYTES)?;
    let mut reader = ManifestReader::open(&manifest_path, summary.file_count, header.to_vec())?;
    let capture = prepared.source_capture();
    let mut entries = capture.entries_cursor()?;
    let mut chunks = capture.chunks_cursor()?;
    let materializer = (!catalog.rows.is_empty())
        .then(|| {
            authority
                .accepted_engine()
                .bootstrap_bulk_materializer_with_session_budget(
                    authority.binding().accepted_frontier(),
                    session_budget_bytes_per_root,
                )
                .map_err(|error| ShadowProjectionError::Projection(error.to_string()))
        })
        .transpose()?;
    let mut inventory = Sha256::new();
    inventory.update(b"tine/inactive-shadow-projection-inventory/v1\0");
    let mut file_count = 0_u64;
    let mut total_bytes = 0_u64;
    loop {
        let mut entry_chunk = Vec::with_capacity(BOOTSTRAP_MATERIALIZATION_CHUNK_PAGES);
        while entry_chunk.len() < BOOTSTRAP_MATERIALIZATION_CHUNK_PAGES {
            let Some(entry) = entries.next()? else {
                break;
            };
            entry_chunk.push(entry);
        }
        if entry_chunk.is_empty() {
            break;
        }
        let page_ids = entry_chunk
            .iter()
            .filter_map(|entry| {
                catalog
                    .rows
                    .get(entry.path())
                    .map(CurrentPathCatalogRow::page_id)
            })
            .collect::<Vec<_>>();
        if !page_ids.is_empty() {
            instrumentation.bulk_materialization_chunks = checked_add(
                instrumentation.bulk_materialization_chunks,
                1,
                "shadow bulk verification chunks",
            )?;
            instrumentation.bulk_pages_materialized = checked_add(
                instrumentation.bulk_pages_materialized,
                page_ids.len() as u64,
                "shadow bulk verified pages",
            )?;
            instrumentation.peak_bulk_pages =
                instrumentation.peak_bulk_pages.max(page_ids.len() as u64);
        }
        let states = materializer
            .as_ref()
            .expect("a nonempty catalog constructed a bulk materializer")
            .materialize_pages_for_projection(&page_ids)
            .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?;
        let states = page_ids
            .into_iter()
            .zip(states)
            .filter_map(|(page_id, state)| state.map(|state| (page_id, state)))
            .collect::<BTreeMap<_, _>>();
        for entry in entry_chunk {
            let source = read_source_file(capture, &entry, &mut chunks, instrumentation)?;
            let state = catalog
                .rows
                .get(entry.path())
                .and_then(|row| states.get(&row.page_id()));
            let Some((row, expected_intent)) =
                plan_exact_source(catalog, &entry, &source, state, instrumentation)?
            else {
                continue;
            };
            let actual = reader
                .next()?
                .ok_or(ShadowProjectionError::CorruptOrConflicting(
                    "shadow manifest ended before source entries",
                ))?;
            if actual.path != *entry.path()
                || actual.kind != entry.kind()
                || actual.logical_name != entry.logical_name()
                || actual.page_id != row.page_id()
                || actual.source != entry.description()
                || actual.source_file_resource != entry.file_resource()
                || actual.source_link_count != entry.link_count()
                || actual.source_chunk_count != entry.chunk_count()
                || actual.intent != expected_intent
            {
                return Err(ShadowProjectionError::BindingMismatch(
                    "shadow manifest per-file evidence differs from source and accepted authority",
                ));
            }
            let staged_path = payload_path(&payload, entry.path())?;
            let staged = compare_regular_file_bytes(&staged_path, &source, instrumentation)?;
            if staged != entry.description() {
                return Err(ShadowProjectionError::CorruptOrConflicting(
                    "shadow payload differs from source bytes",
                ));
            }
            hash_file_evidence(&mut inventory, &actual)?;
            file_count = checked_add(file_count, 1, "verified shadow files")?;
            total_bytes = checked_add(total_bytes, staged.byte_length(), "verified shadow bytes")?;
        }
    }
    if chunks.next()?.is_some() || reader.next()?.is_some() {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "shadow source or manifest contains extra entries",
        ));
    }
    record_bulk_lookup_session_stats(instrumentation, materializer.as_ref())?;
    reader.finish()?;
    if file_count != summary.file_count || total_bytes != summary.total_bytes {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "shadow inventory totals differ from captured source",
        ));
    }
    Ok((
        manifest,
        StagedInventoryProof {
            digest: ContentDigest::from_bytes(inventory.finalize().into()),
            file_count,
            total_bytes,
        },
    ))
}

struct ManifestReader {
    file: File,
    remaining: u64,
    previous: Option<ManagedPath>,
}

impl ManifestReader {
    fn open(
        path: &Path,
        file_count: u64,
        expected_header: Vec<u8>,
    ) -> Result<Self, ShadowProjectionError> {
        let mut file = open_regular_readonly_nofollow(path)?;
        let mut actual = vec![0_u8; expected_header.len()];
        file.read_exact(&mut actual).map_err(|_| {
            ShadowProjectionError::CorruptOrConflicting("shadow manifest header is truncated")
        })?;
        if actual != expected_header {
            return Err(ShadowProjectionError::BindingMismatch(
                "shadow manifest header differs from current proof inputs",
            ));
        }
        Ok(Self {
            file,
            remaining: file_count,
            previous: None,
        })
    }

    fn next(&mut self) -> Result<Option<ShadowProjectionFileEvidence>, ShadowProjectionError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let evidence = read_manifest_evidence(&mut self.file)?;
        if self
            .previous
            .as_ref()
            .is_some_and(|previous| previous >= &evidence.path)
        {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "manifest paths are duplicated or reordered",
            ));
        }
        self.remaining -= 1;
        self.previous = Some(evidence.path.clone());
        Ok(Some(evidence))
    }

    fn finish(mut self) -> Result<(), ShadowProjectionError> {
        if self.remaining != 0 {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "shadow manifest ended before declared entries",
            ));
        }
        let mut trailing = [0_u8; 1];
        if self.file.read(&mut trailing)? != 0 {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "shadow manifest contains trailing entries",
            ));
        }
        Ok(())
    }
}

fn read_manifest_evidence(
    reader: &mut impl Read,
) -> Result<ShadowProjectionFileEvidence, ShadowProjectionError> {
    let path = ManagedPath::parse(
        String::from_utf8(read_bounded_bytes(
            reader,
            BOOTSTRAP_SOURCE_MAX_PATH_BYTES,
            "manifest path",
        )?)
        .map_err(|_| ShadowProjectionError::CorruptOrConflicting("manifest path is not UTF-8"))?,
    )
    .map_err(|_| ShadowProjectionError::CorruptOrConflicting("manifest path is unsafe"))?;
    let kind = match read_u8(reader)? {
        0 => ManagedTextKind::Page,
        1 => ManagedTextKind::Journal,
        _ => {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "manifest managed kind is invalid",
            ));
        }
    };
    let logical_name = String::from_utf8(read_bounded_bytes(
        reader,
        usize::try_from(BOOTSTRAP_SOURCE_MAX_LOGICAL_NAME_BYTES).map_err(|_| {
            ShadowProjectionError::CorruptOrConflicting(
                "logical-name bound does not fit this platform",
            )
        })?,
        "manifest logical name",
    )?)
    .map_err(|_| {
        ShadowProjectionError::CorruptOrConflicting("manifest logical name is not UTF-8")
    })?;
    let page_id = PageId::from_uuid(uuid::Uuid::from_bytes(read_array_16(reader)?));
    let source = read_description(reader)?;
    enforce_limit(
        "manifest source file bytes",
        source.byte_length(),
        BOOTSTRAP_SOURCE_MAX_FILE_BYTES,
    )?;
    let source_file_resource = ContentDigest::from_bytes(read_array_32(reader)?);
    let source_link_count = read_u64(reader)?;
    let source_chunk_count = read_u32(reader)?;
    let intent_bytes = read_bounded_bytes(reader, MAX_MANIFEST_ENTRY_BYTES, "projection intent")?;
    let intent_description = read_description(reader)?;
    if BlobDescription::of(&intent_bytes) != intent_description {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "manifest projection intent description differs",
        ));
    }
    let intent = ProjectionIntent::decode(&intent_bytes)
        .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?;
    if intent
        .encode()
        .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?
        != intent_bytes
    {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "manifest projection intent is not canonical",
        ));
    }
    Ok(ShadowProjectionFileEvidence {
        path,
        kind,
        logical_name,
        page_id,
        source,
        source_file_resource,
        source_link_count,
        source_chunk_count,
        intent,
    })
}

fn manifest_header(
    roots: &MigrationBackupRoot,
    prepared: &InactiveBootstrapPreparedPublication,
    backup: &VerifiedSourceBackup,
    authority: &InactiveBootstrapAcceptedAuthorityBinding,
    sqlite: &VerifiedBootstrapSqliteProjection,
    catalog: CurrentPathCatalogBinding,
    publication_id: ContentDigest,
    summary: SourceSummary,
) -> Result<Vec<u8>, ShadowProjectionError> {
    let capture = prepared.source_capture();
    let mut bytes = Vec::with_capacity(1024);
    bytes.extend_from_slice(MANIFEST_MAGIC);
    ShadowProjectionSchemaBinding::CURRENT.write(&mut bytes);
    bytes.extend_from_slice(authority.workspace_id().as_uuid().as_bytes());
    bytes.extend_from_slice(authority.lineage_digest().as_bytes());
    bytes.extend_from_slice(authority.graph_resource().as_bytes());
    bytes.extend_from_slice(roots.root_identity().as_bytes());
    bytes.extend_from_slice(publication_id.as_bytes());
    put_description(&mut bytes, capture.capture_identity()?);
    put_description(&mut bytes, capture.inventory_description());
    put_description(&mut bytes, capture.entries_description());
    put_description(&mut bytes, capture.chunks_description());
    put_u64(&mut bytes, summary.file_count);
    put_u64(&mut bytes, summary.chunk_count);
    put_u64(&mut bytes, summary.directory_count);
    put_u64(&mut bytes, summary.total_bytes);
    put_u64(&mut bytes, catalog.catalog_rows());
    bytes.extend_from_slice(catalog.accepted_frontier().as_bytes());
    put_u64(&mut bytes, catalog.history_generation());
    bytes.extend_from_slice(catalog.history_root().as_bytes());
    bytes.extend_from_slice(catalog.catalog_root().as_bytes());
    put_authority_binding(&mut bytes, authority)?;
    put_description(&mut bytes, backup.manifest());
    put_description(&mut bytes, backup.restore_proof());
    bytes.extend_from_slice(backup.evidence_digest().as_bytes());
    put_sqlite_binding(&mut bytes, sqlite);
    Ok(bytes)
}

fn manifest_header_from_verified(
    verified: &VerifiedShadowProjection,
) -> Result<Vec<u8>, ShadowProjectionError> {
    let mut bytes = Vec::with_capacity(1024);
    bytes.extend_from_slice(MANIFEST_MAGIC);
    verified.schema.write(&mut bytes);
    bytes.extend_from_slice(verified.workspace_id.as_uuid().as_bytes());
    bytes.extend_from_slice(verified.authority_binding.lineage_digest().as_bytes());
    bytes.extend_from_slice(verified.graph_resource.as_bytes());
    bytes.extend_from_slice(verified.physical_root_identity.as_bytes());
    bytes.extend_from_slice(verified.publication_id.as_bytes());
    put_description(&mut bytes, verified.source_capture);
    put_description(&mut bytes, verified.source_inventory);
    put_description(&mut bytes, verified.source_entries);
    put_description(&mut bytes, verified.source_chunks);
    put_u64(&mut bytes, verified.file_count);
    put_u64(&mut bytes, verified.chunk_count);
    put_u64(&mut bytes, verified.directory_count);
    put_u64(&mut bytes, verified.total_bytes);
    put_u64(&mut bytes, verified.catalog_binding.catalog_rows());
    bytes.extend_from_slice(verified.catalog_binding.accepted_frontier().as_bytes());
    put_u64(&mut bytes, verified.catalog_binding.history_generation());
    bytes.extend_from_slice(verified.catalog_binding.history_root().as_bytes());
    bytes.extend_from_slice(verified.catalog_binding.catalog_root().as_bytes());
    put_authority_binding(&mut bytes, &verified.authority_binding)?;
    put_description(&mut bytes, verified.source_backup.manifest());
    put_description(&mut bytes, verified.source_backup.restore_proof());
    bytes.extend_from_slice(verified.source_backup.evidence_digest().as_bytes());
    put_sqlite_binding(&mut bytes, &verified.sqlite_projection);
    Ok(bytes)
}

fn shadow_publication_id(
    roots: &MigrationBackupRoot,
    prepared: &InactiveBootstrapPreparedPublication,
    backup: &VerifiedSourceBackup,
    authority: &InactiveBootstrapAcceptedAuthorityBinding,
    sqlite: &VerifiedBootstrapSqliteProjection,
    catalog: CurrentPathCatalogBinding,
    summary: SourceSummary,
) -> Result<ContentDigest, ShadowProjectionError> {
    let mut bytes = b"tine/inactive-shadow-projection-publication/v1\0".to_vec();
    ShadowProjectionSchemaBinding::CURRENT.write(&mut bytes);
    bytes.extend_from_slice(roots.root_identity().as_bytes());
    put_description(&mut bytes, prepared.source_capture().capture_identity()?);
    put_description(
        &mut bytes,
        prepared.source_capture().inventory_description(),
    );
    put_u64(&mut bytes, summary.file_count);
    put_u64(&mut bytes, summary.total_bytes);
    put_authority_binding(&mut bytes, authority)?;
    bytes.extend_from_slice(backup.evidence_digest().as_bytes());
    put_sqlite_binding(&mut bytes, sqlite);
    bytes.extend_from_slice(catalog.catalog_root().as_bytes());
    put_u64(&mut bytes, catalog.catalog_rows());
    Ok(ContentDigest::of(&bytes))
}

#[allow(clippy::too_many_arguments)]
fn proof_bytes(
    roots: &MigrationBackupRoot,
    prepared: &InactiveBootstrapPreparedPublication,
    backup: &VerifiedSourceBackup,
    authority: &InactiveBootstrapAcceptedAuthorityBinding,
    sqlite: &VerifiedBootstrapSqliteProjection,
    catalog: CurrentPathCatalogBinding,
    publication_id: ContentDigest,
    summary: SourceSummary,
    manifest: BlobDescription,
    staged: StagedInventoryProof,
) -> Result<Vec<u8>, ShadowProjectionError> {
    let mut bytes = Vec::with_capacity(1024);
    bytes.extend_from_slice(PROOF_MAGIC);
    put_u32(&mut bytes, SHADOW_PROOF_SCHEMA_VERSION);
    bytes.extend_from_slice(&manifest_header(
        roots,
        prepared,
        backup,
        authority,
        sqlite,
        catalog,
        publication_id,
        summary,
    )?);
    put_description(&mut bytes, manifest);
    bytes.extend_from_slice(staged.digest.as_bytes());
    put_u64(&mut bytes, staged.file_count);
    put_u64(&mut bytes, staged.total_bytes);
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
fn commit_marker_bytes(
    roots: &MigrationBackupRoot,
    prepared: &InactiveBootstrapPreparedPublication,
    backup: &VerifiedSourceBackup,
    authority: &InactiveBootstrapAcceptedAuthorityBinding,
    sqlite: &VerifiedBootstrapSqliteProjection,
    catalog: CurrentPathCatalogBinding,
    publication_id: ContentDigest,
    summary: SourceSummary,
    manifest: BlobDescription,
    proof: BlobDescription,
    staged: StagedInventoryProof,
) -> Result<(Vec<u8>, ContentDigest), ShadowProjectionError> {
    let mut body = Vec::with_capacity(1152);
    body.extend_from_slice(COMMIT_MARKER_MAGIC);
    put_u32(&mut body, SHADOW_COMMIT_MARKER_SCHEMA_VERSION);
    body.extend_from_slice(&manifest_header(
        roots,
        prepared,
        backup,
        authority,
        sqlite,
        catalog,
        publication_id,
        summary,
    )?);
    put_description(&mut body, manifest);
    put_description(&mut body, proof);
    body.extend_from_slice(staged.digest.as_bytes());
    put_u64(&mut body, staged.file_count);
    put_u64(&mut body, staged.total_bytes);
    let mut hasher = Sha256::new();
    hasher.update(b"tine/verified-inactive-shadow-projection/v1\0");
    hasher.update(&body);
    let evidence = ContentDigest::from_bytes(hasher.finalize().into());
    body.extend_from_slice(evidence.as_bytes());
    Ok((body, evidence))
}

fn put_authority_binding(
    output: &mut Vec<u8>,
    binding: &InactiveBootstrapAcceptedAuthorityBinding,
) -> Result<(), ShadowProjectionError> {
    output.extend_from_slice(binding.workspace_id().as_uuid().as_bytes());
    output.extend_from_slice(binding.lineage_digest().as_bytes());
    output.extend_from_slice(binding.graph_resource().as_bytes());
    output.extend_from_slice(binding.publication_id().as_bytes());
    output.extend_from_slice(binding.aggregate_digest().as_bytes());
    output.extend_from_slice(binding.import_id().as_bytes());
    put_u32(output, binding.part_count());
    match binding.predecessor_terminal() {
        Some(part) => {
            output.push(1);
            output.extend_from_slice(part.as_bytes());
        }
        None => {
            output.push(0);
            output.extend_from_slice(&[0_u8; 32]);
        }
    }
    let frontier = binding.accepted_frontier();
    let frontier_bytes = postcard::to_allocvec(frontier)
        .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?;
    put_vec(output, &frontier_bytes)?;
    put_u64(output, frontier.acceptance_sequence());
    put_u64(output, frontier.document_count());
    put_u64(output, frontier.retained_bytes_total());
    output.extend_from_slice(frontier.state_digest().as_bytes());
    let storage = binding.storage_binding();
    output.extend_from_slice(storage.endpoint.endpoint_id().as_uuid().as_bytes());
    output.extend_from_slice(storage.endpoint.device_id().as_uuid().as_bytes());
    output.extend_from_slice(storage.endpoint.graph_resource_id().as_bytes());
    output.extend_from_slice(storage.receipt_store_id.as_bytes());
    let engine_binding = postcard::to_allocvec(binding.engine_binding())
        .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?;
    put_vec(output, &engine_binding)?;
    output.extend_from_slice(binding.bootstrap_binding().publication_id().as_bytes());
    output.extend_from_slice(binding.bootstrap_binding().aggregate_digest().as_bytes());
    put_u32(output, binding.bootstrap_binding().part_count());
    put_vec(
        output,
        &binding.bootstrap_binding().final_frontier().encode(),
    )?;
    output.extend_from_slice(binding.archive_identity().binding_digest().as_bytes());
    put_u64(output, binding.history_generation());
    output.extend_from_slice(binding.history_root().as_bytes());
    put_u64(output, binding.cold_record_count());
    Ok(())
}

fn put_sqlite_binding(output: &mut Vec<u8>, proof: &VerifiedBootstrapSqliteProjection) {
    output.extend_from_slice(proof.claim().workspace_id().as_uuid().as_bytes());
    output.extend_from_slice(proof.claim().lineage_digest().as_bytes());
    output.extend_from_slice(proof.frontier_root().state_digest().as_bytes());
    put_u64(output, proof.accepted_batch_count());
    output.extend_from_slice(proof.semantic_projection_digest().as_bytes());
    output.extend_from_slice(proof.materialized_row_digest().as_bytes());
}

fn publication_paths(
    roots: &MigrationBackupRoot,
    authority: &InactiveBootstrapAcceptedAuthorityBinding,
    publication_id: ContentDigest,
) -> Result<PublicationPaths, ShadowProjectionError> {
    let workspace = authority.workspace_id().to_string();
    let publication = hex(publication_id.as_bytes());
    let parent = roots
        .canonical_root()
        .join(SHADOW_ROOT_DIRECTORY)
        .join(workspace);
    let stage = parent.join(format!(".{publication}.staging"));
    let final_directory = parent.join(publication);
    for path in [&stage, &final_directory] {
        if path == roots.canonical_graph_root() || path.starts_with(roots.canonical_graph_root()) {
            return Err(ShadowProjectionError::BindingMismatch(
                "shadow projection would be inside the live graph",
            ));
        }
    }
    Ok(PublicationPaths {
        parent,
        stage,
        final_directory,
    })
}

fn ensure_publication_parent(
    roots: &MigrationBackupRoot,
    authority: &InactiveBootstrapAcceptedAuthorityBinding,
    paths: &PublicationPaths,
) -> Result<(), ShadowProjectionError> {
    let base = roots.canonical_root().join(SHADOW_ROOT_DIRECTORY);
    ensure_real_directory_created_before_parent_sync(
        &base,
        ShadowProjectionCrashCut::AfterShadowBaseCreation,
    )?;
    sync_directory_barrier(
        roots.canonical_root(),
        ShadowProjectionDurabilityBarrier::BackupRootAfterShadowBase,
    )?;
    let workspace = base.join(authority.workspace_id().to_string());
    if workspace != paths.parent {
        return Err(ShadowProjectionError::BindingMismatch(
            "shadow publication parent is not deterministic",
        ));
    }
    ensure_real_directory_created_before_parent_sync(
        &workspace,
        ShadowProjectionCrashCut::AfterShadowWorkspaceCreation,
    )?;
    sync_directory_barrier(
        &base,
        ShadowProjectionDurabilityBarrier::ShadowBaseAfterWorkspace,
    )
}

fn hash_file_evidence(
    hasher: &mut Sha256,
    evidence: &ShadowProjectionFileEvidence,
) -> Result<(), ShadowProjectionError> {
    hasher.update((evidence.path.as_str().len() as u64).to_be_bytes());
    hasher.update(evidence.path.as_str().as_bytes());
    hasher.update([match evidence.kind {
        ManagedTextKind::Page => 0,
        ManagedTextKind::Journal => 1,
    }]);
    hasher.update((evidence.logical_name.len() as u64).to_be_bytes());
    hasher.update(evidence.logical_name.as_bytes());
    hasher.update(evidence.page_id.as_uuid().as_bytes());
    hasher.update(evidence.source.sha256());
    hasher.update(evidence.source.byte_length().to_be_bytes());
    hasher.update(evidence.source_file_resource.as_bytes());
    hasher.update(evidence.source_link_count.to_be_bytes());
    hasher.update(evidence.source_chunk_count.to_be_bytes());
    let encoded = evidence
        .intent
        .encode()
        .map_err(|error| ShadowProjectionError::Projection(error.to_string()))?;
    hasher.update((encoded.len() as u64).to_be_bytes());
    hasher.update(&encoded);
    Ok(())
}

fn validate_source_entry(entry: &BootstrapSourceEntry) -> Result<(), ShadowProjectionError> {
    enforce_limit(
        "source path bytes",
        entry.path().as_str().len() as u64,
        BOOTSTRAP_SOURCE_MAX_PATH_BYTES as u64,
    )?;
    enforce_limit(
        "source logical name bytes",
        entry.logical_name().len() as u64,
        BOOTSTRAP_SOURCE_MAX_LOGICAL_NAME_BYTES,
    )?;
    enforce_limit(
        "source file bytes",
        entry.description().byte_length(),
        BOOTSTRAP_SOURCE_MAX_FILE_BYTES,
    )?;
    let maximum_chunks =
        BOOTSTRAP_SOURCE_MAX_FILE_BYTES.div_ceil(BOOTSTRAP_SOURCE_CHUNK_BYTES as u64);
    enforce_limit(
        "source file chunks",
        u64::from(entry.chunk_count()),
        maximum_chunks,
    )
}

fn payload_path(root: &Path, path: &ManagedPath) -> Result<PathBuf, ShadowProjectionError> {
    validate_managed_path_depth(path)?;
    let joined = root.join(path.as_str());
    if !joined.starts_with(root) {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "managed path escapes shadow payload root",
        ));
    }
    Ok(joined)
}

fn validate_managed_path_depth(path: &ManagedPath) -> Result<(), ShadowProjectionError> {
    enforce_limit(
        "managed path bytes",
        path.as_str().len() as u64,
        BOOTSTRAP_SOURCE_MAX_PATH_BYTES as u64,
    )?;
    enforce_limit(
        "managed path depth",
        path.as_str().split('/').count() as u64,
        BOOTSTRAP_SOURCE_MAX_DIRECTORY_DEPTH.saturating_add(1) as u64,
    )
}

fn ensure_managed_parent_directories(
    root: &Path,
    path: &ManagedPath,
) -> Result<(), ShadowProjectionError> {
    let mut current = root.to_path_buf();
    let component_count = path.as_str().split('/').count();
    for component in path
        .as_str()
        .split('/')
        .take(component_count.saturating_sub(1))
    {
        current.push(component);
        ensure_real_directory_created(&current)?;
    }
    Ok(())
}

#[derive(Default)]
struct TreeCounts {
    files: u64,
    directories: u64,
    bytes: u64,
}

struct TreeFrame {
    path: PathBuf,
    entries: fs::ReadDir,
}

fn traverse_tree_bounded(
    root: &Path,
    summary: SourceSummary,
    sync: bool,
    instrumentation: &mut ShadowProjectionInstrumentation,
) -> Result<TreeCounts, ShadowProjectionError> {
    require_real_directory(root, "shadow payload root is not a real directory")?;
    let mut counts = TreeCounts::default();
    let mut stack = vec![TreeFrame {
        path: root.to_path_buf(),
        entries: fs::read_dir(root)?,
    }];
    while !stack.is_empty() {
        let next = stack
            .last_mut()
            .expect("bounded traversal has a frame")
            .entries
            .next();
        let Some(entry) = next else {
            let completed = stack.pop().expect("bounded traversal has a frame");
            if sync {
                sync_directory(&completed.path)?;
            }
            continue;
        };
        instrumentation.tree_entries_visited = checked_add(
            instrumentation.tree_entries_visited,
            1,
            "tree entries visited",
        )?;
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(|_| {
            ShadowProjectionError::CorruptOrConflicting("shadow traversal escaped its root")
        })?;
        let relative = relative
            .to_str()
            .ok_or(ShadowProjectionError::CorruptOrConflicting(
                "shadow payload path is not UTF-8",
            ))?;
        enforce_limit(
            "shadow tree path bytes",
            relative.len() as u64,
            summary.max_path_bytes,
        )?;
        enforce_limit(
            "shadow tree depth",
            relative.split(['/', '\\']).count() as u64,
            summary.max_depth as u64,
        )?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata_is_real_directory(&metadata) {
            counts.directories = checked_add(counts.directories, 1, "shadow directories")?;
            enforce_limit(
                "shadow directories",
                counts.directories,
                summary.directory_count,
            )?;
            stack.push(TreeFrame {
                entries: fs::read_dir(&path)?,
                path,
            });
        } else if metadata_is_real_file(&metadata) {
            counts.files = checked_add(counts.files, 1, "shadow files")?;
            enforce_limit("shadow files", counts.files, summary.file_count)?;
            counts.bytes = checked_add(counts.bytes, metadata.len(), "shadow bytes")?;
            enforce_limit("shadow bytes", counts.bytes, summary.total_bytes)?;
            if sync {
                open_regular_for_sync(&path)?.sync_all()?;
            }
        } else {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "shadow tree contains a symlink, reparse point, or special entry",
            ));
        }
    }
    Ok(counts)
}

fn sync_tree(
    root: &Path,
    summary: SourceSummary,
    instrumentation: &mut ShadowProjectionInstrumentation,
) -> Result<(), ShadowProjectionError> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let _ = traverse_tree_bounded(root, summary, false, instrumentation)?;
        let directory = open_directory_nofollow_ambient(root)?;
        // SAFETY: the retained no-follow directory owns a live descriptor for
        // the filesystem containing every independently staged payload.
        let result = unsafe { libc::syncfs(directory.as_fd().as_raw_fd()) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = traverse_tree_bounded(root, summary, true, instrumentation)?;
    }
    Ok(())
}

fn validate_projection_root_entries(
    directory: &Path,
    final_directory: bool,
) -> Result<(), ShadowProjectionError> {
    let mut payload = false;
    let mut manifest = false;
    let mut proof = false;
    let mut proof_stage = false;
    let mut marker = false;
    let mut marker_stage = false;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        match entry.file_name().to_str() {
            Some(PAYLOAD_DIRECTORY) if metadata_is_real_directory(&metadata) && !payload => {
                payload = true
            }
            Some(MANIFEST_FILE) if metadata_is_real_file(&metadata) && !manifest => manifest = true,
            Some(PROOF_FILE) if final_directory && metadata_is_real_file(&metadata) && !proof => {
                proof = true
            }
            Some(PROOF_STAGE_FILE)
                if final_directory && metadata_is_real_file(&metadata) && !proof_stage =>
            {
                proof_stage = true
            }
            Some(COMMIT_MARKER_FILE)
                if final_directory && metadata_is_real_file(&metadata) && !marker =>
            {
                marker = true
            }
            Some(COMMIT_MARKER_STAGE_FILE)
                if final_directory && metadata_is_real_file(&metadata) && !marker_stage =>
            {
                marker_stage = true
            }
            _ => {
                return Err(ShadowProjectionError::CorruptOrConflicting(
                    "shadow projection directory contains an extra or malformed entry",
                ))
            }
        }
    }
    if !payload
        || !manifest
        || (marker && !proof)
        || (proof && proof_stage)
        || (marker && marker_stage)
    {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "shadow projection directory is missing required entries",
        ));
    }
    if !final_directory && (proof || proof_stage || marker || marker_stage) {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "staging shadow directory contains final evidence",
        ));
    }
    Ok(())
}

fn publish_small_file_atomic(
    directory: &Path,
    stage_name: &str,
    final_name: &str,
    expected: &[u8],
    partial_cut: ShadowProjectionCrashCut,
    conflict: &'static str,
) -> Result<BlobDescription, ShadowProjectionError> {
    enforce_limit(
        "small shadow evidence bytes",
        expected.len() as u64,
        MAX_SMALL_EVIDENCE_BYTES,
    )?;
    let stage = directory.join(stage_name);
    let final_path = directory.join(final_name);
    if path_exists(&final_path)? {
        if path_exists(&stage)? {
            return Err(ShadowProjectionError::CorruptOrConflicting(conflict));
        }
        compare_exact_small_file(&final_path, expected, conflict)?;
        return Ok(BlobDescription::of(expected));
    }
    let mut output = ResumableExactFile::open(&stage, conflict)?;
    if expected.len() > 1 && take_crash_cut(partial_cut) {
        let prefix = (expected.len() / 2).clamp(1, expected.len() - 1);
        output
            .write_all(&expected[..prefix])
            .map_err(|_| ShadowProjectionError::CorruptOrConflicting(conflict))?;
        output.flush()?;
        return Err(ShadowProjectionError::InjectedCrashCut(partial_cut.label()));
    }
    output
        .write_all(expected)
        .map_err(|_| ShadowProjectionError::CorruptOrConflicting(conflict))?;
    let description = output.finish()?;
    move_file_noreplace(&stage, &final_path)
        .map_err(|_| ShadowProjectionError::CorruptOrConflicting(conflict))?;
    sync_directory(directory)?;
    Ok(description)
}

fn compare_regular_file_bytes(
    path: &Path,
    expected: &[u8],
    instrumentation: &mut ShadowProjectionInstrumentation,
) -> Result<BlobDescription, ShadowProjectionError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata_is_real_file(&metadata) || metadata.len() != expected.len() as u64 {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "shadow payload is missing, special, or has wrong length",
        ));
    }
    let mut file = open_regular_readonly_nofollow(path)?;
    let mut hasher = Sha256::new();
    let mut offset = 0_usize;
    let mut buffer = [0_u8; IO_BUFFER_BYTES];
    while offset < expected.len() {
        let wanted = (expected.len() - offset).min(buffer.len());
        file.read_exact(&mut buffer[..wanted]).map_err(|_| {
            ShadowProjectionError::CorruptOrConflicting("shadow payload is truncated")
        })?;
        if buffer[..wanted] != expected[offset..offset + wanted] {
            return Err(ShadowProjectionError::CorruptOrConflicting(
                "shadow payload bytes differ from captured source",
            ));
        }
        hasher.update(&buffer[..wanted]);
        offset += wanted;
        instrumentation.payload_bytes_read = checked_add(
            instrumentation.payload_bytes_read,
            wanted as u64,
            "payload bytes read",
        )?;
    }
    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "shadow payload has trailing bytes",
        ));
    }
    Ok(BlobDescription::from_parts(
        hasher.finalize().into(),
        expected.len() as u64,
    ))
}

fn compare_exact_small_file(
    path: &Path,
    expected: &[u8],
    conflict: &'static str,
) -> Result<(), ShadowProjectionError> {
    if describe_regular_file(path, MAX_SMALL_EVIDENCE_BYTES)? != BlobDescription::of(expected) {
        return Err(ShadowProjectionError::CorruptOrConflicting(conflict));
    }
    let mut file = open_regular_readonly_nofollow(path)?;
    let mut actual = Vec::new();
    actual
        .try_reserve_exact(expected.len())
        .map_err(|_| ShadowProjectionError::ResourceLimit {
            resource: "small evidence allocation",
            observed: expected.len() as u64,
            limit: MAX_SMALL_EVIDENCE_BYTES,
        })?;
    file.read_to_end(&mut actual)?;
    if actual != expected {
        return Err(ShadowProjectionError::CorruptOrConflicting(conflict));
    }
    Ok(())
}

fn describe_regular_file(
    path: &Path,
    maximum_bytes: u64,
) -> Result<BlobDescription, ShadowProjectionError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata_is_real_file(&metadata) {
        return Err(ShadowProjectionError::CorruptOrConflicting(
            "evidence path is not a regular no-follow file",
        ));
    }
    enforce_limit("evidence file bytes", metadata.len(), maximum_bytes)?;
    let mut file = open_regular_readonly_nofollow(path)?;
    let mut hasher = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = [0_u8; IO_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        length = checked_add(length, read as u64, "evidence bytes read")?;
        enforce_limit("evidence file bytes", length, maximum_bytes)?;
        hasher.update(&buffer[..read]);
    }
    Ok(BlobDescription::from_parts(
        hasher.finalize().into(),
        length,
    ))
}

struct ResumableExactFile {
    existing: Option<File>,
    remaining_existing: u64,
    append: File,
    hasher: Sha256,
    expected_length: u64,
    conflict: &'static str,
}

impl ResumableExactFile {
    fn open(path: &Path, conflict: &'static str) -> Result<Self, ShadowProjectionError> {
        let (existing, remaining_existing, append) = match fs::symlink_metadata(path) {
            Ok(metadata) if !metadata_is_real_file(&metadata) => {
                return Err(ShadowProjectionError::CorruptOrConflicting(conflict))
            }
            Ok(metadata) => (
                Some(open_regular_readonly_nofollow(path)?),
                metadata.len(),
                open_regular_append_nofollow(path)?,
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                (None, 0, create_new_regular_nofollow(path)?)
            }
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            existing,
            remaining_existing,
            append,
            hasher: Sha256::new(),
            expected_length: 0,
            conflict,
        })
    }

    fn finish(mut self) -> Result<BlobDescription, ShadowProjectionError> {
        let description = self.finish_unflushed()?;
        self.append.sync_all()?;
        Ok(description)
    }

    fn finish_unflushed(&mut self) -> Result<BlobDescription, ShadowProjectionError> {
        if self.remaining_existing != 0 {
            return Err(ShadowProjectionError::CorruptOrConflicting(self.conflict));
        }
        self.append.flush()?;
        Ok(BlobDescription::from_parts(
            self.hasher.clone().finalize().into(),
            self.expected_length,
        ))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn finish_payload(mut self) -> Result<BlobDescription, ShadowProjectionError> {
        self.finish_unflushed()
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn finish_payload(self) -> Result<BlobDescription, ShadowProjectionError> {
        self.finish()
    }
}

impl Write for ResumableExactFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let compare_len = usize::try_from(self.remaining_existing.min(bytes.len() as u64))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, self.conflict))?;
        if compare_len != 0 {
            let mut actual = vec![0_u8; compare_len];
            self.existing
                .as_mut()
                .expect("existing prefix has reader")
                .read_exact(&mut actual)?;
            if actual != bytes[..compare_len] {
                return Err(io::Error::new(io::ErrorKind::InvalidData, self.conflict));
            }
            self.remaining_existing -= compare_len as u64;
        }
        if compare_len < bytes.len() {
            self.append.write_all(&bytes[compare_len..])?;
        }
        self.hasher.update(bytes);
        self.expected_length = self
            .expected_length
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.append.flush()
    }
}

fn write_len_prefixed(output: &mut impl Write, bytes: &[u8]) -> Result<(), ShadowProjectionError> {
    let length = u32::try_from(bytes.len()).map_err(|_| ShadowProjectionError::ResourceLimit {
        resource: "manifest field bytes",
        observed: bytes.len() as u64,
        limit: u32::MAX as u64,
    })?;
    output.write_all(&length.to_be_bytes())?;
    output.write_all(bytes)?;
    Ok(())
}

fn write_description(
    output: &mut impl Write,
    description: BlobDescription,
) -> Result<(), ShadowProjectionError> {
    output.write_all(description.sha256())?;
    output.write_all(&description.byte_length().to_be_bytes())?;
    Ok(())
}

fn put_description(output: &mut Vec<u8>, description: BlobDescription) {
    output.extend_from_slice(description.sha256());
    put_u64(output, description.byte_length());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn put_vec(output: &mut Vec<u8>, value: &[u8]) -> Result<(), ShadowProjectionError> {
    let length = u32::try_from(value.len()).map_err(|_| ShadowProjectionError::ResourceLimit {
        resource: "proof binding bytes",
        observed: value.len() as u64,
        limit: u32::MAX as u64,
    })?;
    put_u32(output, length);
    output.extend_from_slice(value);
    Ok(())
}

fn read_bounded_bytes(
    reader: &mut impl Read,
    maximum: usize,
    resource: &'static str,
) -> Result<Vec<u8>, ShadowProjectionError> {
    let length = read_u32(reader)? as usize;
    if length > maximum {
        return Err(ShadowProjectionError::ResourceLimit {
            resource,
            observed: length as u64,
            limit: maximum as u64,
        });
    }
    let mut bytes = vec![0_u8; length];
    reader
        .read_exact(&mut bytes)
        .map_err(|_| ShadowProjectionError::CorruptOrConflicting("manifest field is truncated"))?;
    Ok(bytes)
}

fn read_u8(reader: &mut impl Read) -> Result<u8, ShadowProjectionError> {
    let mut bytes = [0_u8; 1];
    reader.read_exact(&mut bytes).map_err(|_| {
        ShadowProjectionError::CorruptOrConflicting("manifest integer is truncated")
    })?;
    Ok(bytes[0])
}

fn read_u32(reader: &mut impl Read) -> Result<u32, ShadowProjectionError> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes).map_err(|_| {
        ShadowProjectionError::CorruptOrConflicting("manifest integer is truncated")
    })?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> Result<u64, ShadowProjectionError> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes).map_err(|_| {
        ShadowProjectionError::CorruptOrConflicting("manifest integer is truncated")
    })?;
    Ok(u64::from_be_bytes(bytes))
}

fn read_array_16(reader: &mut impl Read) -> Result<[u8; 16], ShadowProjectionError> {
    let mut bytes = [0_u8; 16];
    reader.read_exact(&mut bytes).map_err(|_| {
        ShadowProjectionError::CorruptOrConflicting("manifest identity is truncated")
    })?;
    Ok(bytes)
}

fn read_array_32(reader: &mut impl Read) -> Result<[u8; 32], ShadowProjectionError> {
    let mut bytes = [0_u8; 32];
    reader
        .read_exact(&mut bytes)
        .map_err(|_| ShadowProjectionError::CorruptOrConflicting("manifest digest is truncated"))?;
    Ok(bytes)
}

fn read_description(reader: &mut impl Read) -> Result<BlobDescription, ShadowProjectionError> {
    Ok(BlobDescription::from_parts(
        read_array_32(reader)?,
        read_u64(reader)?,
    ))
}

fn checked_add(
    current: u64,
    growth: u64,
    resource: &'static str,
) -> Result<u64, ShadowProjectionError> {
    current
        .checked_add(growth)
        .ok_or(ShadowProjectionError::ResourceLimit {
            resource,
            observed: u64::MAX,
            limit: u64::MAX - 1,
        })
}

fn enforce_limit(
    resource: &'static str,
    observed: u64,
    limit: u64,
) -> Result<(), ShadowProjectionError> {
    if observed > limit {
        Err(ShadowProjectionError::ResourceLimit {
            resource,
            observed,
            limit,
        })
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn metadata_is_windows_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
}

#[cfg(not(windows))]
fn metadata_is_windows_reparse(_metadata: &fs::Metadata) -> bool {
    false
}

fn metadata_is_real_directory(metadata: &fs::Metadata) -> bool {
    metadata.is_dir()
        && !metadata.file_type().is_symlink()
        && !metadata_is_windows_reparse(metadata)
}

fn metadata_is_real_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
        && !metadata.file_type().is_symlink()
        && !metadata_is_windows_reparse(metadata)
}

fn require_supported_exact_filesystem() -> io::Result<()> {
    #[cfg(any(unix, windows))]
    {
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable no-follow shadow projection is unsupported on this platform",
        ))
    }
}

fn configure_file_nofollow(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = options;
}

fn validate_opened_regular(file: File) -> io::Result<File> {
    if metadata_is_real_file(&file.metadata()?) {
        Ok(file)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "opened shadow path is not a regular no-follow file",
        ))
    }
}

fn open_regular_readonly_nofollow(path: &Path) -> io::Result<File> {
    require_supported_exact_filesystem()?;
    let mut options = OpenOptions::new();
    options.read(true);
    configure_file_nofollow(&mut options);
    validate_opened_regular(options.open(path)?)
}

fn open_regular_append_nofollow(path: &Path) -> io::Result<File> {
    require_supported_exact_filesystem()?;
    let mut options = OpenOptions::new();
    options.append(true);
    configure_file_nofollow(&mut options);
    validate_opened_regular(options.open(path)?)
}

fn create_new_regular_nofollow(path: &Path) -> io::Result<File> {
    require_supported_exact_filesystem()?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    configure_file_nofollow(&mut options);
    validate_opened_regular(options.open(path)?)
}

fn open_regular_for_sync(path: &Path) -> io::Result<File> {
    require_supported_exact_filesystem()?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.write(true);
    configure_file_nofollow(&mut options);
    validate_opened_regular(options.open(path)?)
}

fn open_directory_nofollow_ambient(path: &Path) -> Result<Dir, ShadowProjectionError> {
    require_supported_exact_filesystem()?;
    let name = path.file_name().and_then(|name| name.to_str()).ok_or(
        ShadowProjectionError::BindingMismatch("directory path has no UTF-8 leaf"),
    )?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = Dir::open_ambient_dir(parent, ambient_authority())?;
    open_dir_nofollow(&parent, name)
        .map_err(|error| ShadowProjectionError::Io(io::Error::other(error.to_string())))
}

fn require_real_directory(path: &Path, detail: &'static str) -> Result<(), ShadowProjectionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_real_directory(&metadata) => Ok(()),
        Ok(_) => Err(ShadowProjectionError::CorruptOrConflicting(detail)),
        Err(error) => Err(error.into()),
    }
}

fn ensure_real_directory_created(path: &Path) -> Result<(), ShadowProjectionError> {
    ensure_real_directory_created_inner(path, None)
}

fn ensure_real_directory_created_before_parent_sync(
    path: &Path,
    cut: ShadowProjectionCrashCut,
) -> Result<(), ShadowProjectionError> {
    ensure_real_directory_created_inner(path, Some(cut))
}

fn ensure_real_directory_created_inner(
    path: &Path,
    cut_before_parent_sync: Option<ShadowProjectionCrashCut>,
) -> Result<(), ShadowProjectionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_real_directory(&metadata) => Ok(()),
        Ok(_) => Err(ShadowProjectionError::CorruptOrConflicting(
            "expected shadow directory is not a real directory",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(ShadowProjectionError::BindingMismatch(
                "shadow directory has no parent",
            ))?;
            fs::create_dir(path)?;
            if let Some(cut) = cut_before_parent_sync {
                inject_crash_cut(cut)?;
            }
            sync_directory(parent)
        }
        Err(error) => Err(error.into()),
    }
}

fn path_exists(path: &Path) -> Result<bool, ShadowProjectionError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn sync_file_and_parent(path: &Path) -> Result<(), ShadowProjectionError> {
    open_regular_for_sync(path)?.sync_all()?;
    sync_directory(
        path.parent()
            .ok_or(ShadowProjectionError::BindingMismatch("file has no parent"))?,
    )
}

fn sync_directory(path: &Path) -> Result<(), ShadowProjectionError> {
    let directory = open_directory_nofollow_ambient(path)?;
    sync_dir_required(&directory)
        .map_err(|error| ShadowProjectionError::Io(io::Error::other(error.to_string())))
}

fn sync_directory_barrier(
    path: &Path,
    barrier: ShadowProjectionDurabilityBarrier,
) -> Result<(), ShadowProjectionError> {
    sync_directory(path)?;
    #[cfg(test)]
    SHADOW_DURABILITY_BARRIERS.with(|barriers| barriers.borrow_mut().push(barrier));
    #[cfg(not(test))]
    let _ = barrier;
    Ok(())
}

fn hex(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use uuid::Uuid;

    use super::*;
    use crate::oplog::bootstrap_import::MAX_OPERATIONS_PER_BOOTSTRAP_PART;
    use crate::oplog::enrollment::{
        compose_verified_local, compose_verified_local_at_cut_for_test,
        enrollment_application_root_for_test, reopen_verified_local, CommitCut,
        EnrollmentBindingV1, EnrollmentOpen, EnrollmentReader, PreparationId,
        VerifiedLocalCompositionError, VerifiedLocalProofSet,
    };
    use crate::oplog::hot_engine::{
        MaterializationStats, MaterializedBlock, MaterializedPage, ProjectionEndpointBinding,
        ProjectionPageState, ProjectionStorageBinding,
    };
    use crate::oplog::import::{
        prepare_inactive_bootstrap_import, publish_install_verify_inactive_bootstrap,
        reopen_inactive_bootstrap_accepted_authority,
    };
    use crate::oplog::migration_backup::verify_migration_source_backup;
    use crate::oplog::sqlite::{ApplicationRuntimeRoot, SqliteFrontier};
    use crate::oplog::{
        BlockId, CrdtPeerCounter, CrdtPeerId, DeviceId, DocumentDependencies, DocumentId,
        FrontierV2, LineageDigest, LogicalPageName, LogseqIdentityOrigin, LogseqUuid, ObjectStore,
        PolicyGeneratedAnchorReason, ProjectionClaimEvidence, ProjectionClaimParticipant,
        ProjectionEndpointId, ProjectionReceiptStoreId, ReferenceCatalogPolicyV1,
    };

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("tine-shadow-projection-{label}-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
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
        root: TestRoot,
        graph_root: PathBuf,
        graph: Graph,
        prepared: InactiveBootstrapPreparedPublication,
        verified: InactiveBootstrapVerifiedPublication,
        authority: InactiveBootstrapAcceptedAuthority,
        roots: MigrationBackupRoot,
        backup: VerifiedSourceBackup,
        sqlite: OpenProjection,
        sqlite_proof: VerifiedBootstrapSqliteProjection,
        archive_resource_id: crate::oplog::CanonicalArchiveResourceId,
        original_graph: BTreeMap<String, Vec<u8>>,
    }

    impl Fixture {
        fn new(label: &str, config: Option<&[u8]>, files: Vec<(String, Vec<u8>)>) -> Self {
            let root = TestRoot::new(label);
            let graph_root = root.path().join("graph");
            fs::create_dir(&graph_root).unwrap();
            if let Some(config) = config {
                fs::create_dir(graph_root.join("logseq")).unwrap();
                fs::write(graph_root.join("logseq/config.edn"), config).unwrap();
            }
            for (path, bytes) in &files {
                let destination = graph_root.join(path);
                fs::create_dir_all(destination.parent().unwrap()).unwrap();
                fs::write(destination, bytes).unwrap();
            }
            let original_graph = snapshot_files(&graph_root);
            let graph = Graph::open(&graph_root);
            let capture_root = root.path().join("capture");
            let preparation_root = root.path().join("preparation");
            fs::create_dir(&capture_root).unwrap();
            fs::create_dir(&preparation_root).unwrap();
            let capture = graph
                .capture_inactive_bootstrap_sources(&capture_root)
                .unwrap();
            let workspace = WorkspaceId::from_uuid(Uuid::from_u128(0x8100));
            let prepared = prepare_inactive_bootstrap_import(
                &graph,
                capture,
                workspace,
                LineageDigest::of(b"inactive-shadow-projection-test"),
                DocumentId::from_uuid(Uuid::from_u128(0x8101)),
                ReferenceCatalogPolicyV1::default(),
                &ObjectStore::open(&root.path().join("archive"), workspace)
                    .unwrap()
                    .bootstrap_authoring_capability()
                    .unwrap(),
                &preparation_root,
            )
            .unwrap();
            let storage_binding = ProjectionStorageBinding {
                endpoint: ProjectionEndpointBinding {
                    endpoint_id: ProjectionEndpointId::from_uuid(Uuid::from_u128(0x8102)),
                    device_id: DeviceId::from_uuid(Uuid::from_u128(0x8103)),
                    graph_resource_id: prepared.aggregate().graph_resource(),
                },
                receipt_store_id: ProjectionReceiptStoreId::from_capability_identity(
                    b"inactive-shadow-projection-test",
                    b"receipt-store",
                ),
            };
            let archive = root.path().join("archive");
            let verified = publish_install_verify_inactive_bootstrap(
                &prepared,
                ObjectStore::open(&archive, workspace).unwrap(),
                storage_binding,
            )
            .unwrap();
            let authority = reopen_inactive_bootstrap_accepted_authority(
                &verified,
                ObjectStore::open(&archive, workspace).unwrap(),
            )
            .unwrap();
            let device_root = root.path().join("device-local");
            fs::create_dir(&device_root).unwrap();
            let roots = MigrationBackupRoot::open(&device_root, &graph_root).unwrap();
            let backup = verify_migration_source_backup(&roots, &prepared, &verified).unwrap();
            let runtime =
                ApplicationRuntimeRoot::open_for_test(&root.path().join("runtime")).unwrap();
            let (sqlite, sqlite_proof) = SqliteFrontier::open_or_rebuild_inactive_bootstrap(
                &root.path().join("bootstrap.sqlite"),
                &runtime,
                &authority,
            )
            .unwrap();
            let archive_resource_id = authority
                .store()
                .provision_enrolled_archive_resource_id()
                .unwrap();
            Self {
                root,
                graph_root,
                graph,
                prepared,
                verified,
                authority,
                roots,
                backup,
                sqlite,
                sqlite_proof,
                archive_resource_id,
                original_graph,
            }
        }

        fn verify(&self) -> Result<VerifiedShadowProjection, ShadowProjectionError> {
            verify_inactive_bootstrap_shadow_projection(
                &self.graph,
                &self.roots,
                &self.prepared,
                &self.verified,
                &self.backup,
                &self.authority,
                &self.sqlite,
                &self.sqlite_proof,
            )
        }

        fn verify_with_lookup_budget(
            &self,
            session_budget_bytes_per_root: usize,
        ) -> Result<VerifiedShadowProjection, ShadowProjectionError> {
            verify_inactive_bootstrap_shadow_projection_with_lookup_budget(
                &self.graph,
                &self.roots,
                &self.prepared,
                &self.verified,
                &self.backup,
                &self.authority,
                &self.sqlite,
                &self.sqlite_proof,
                session_budget_bytes_per_root,
            )
        }

        fn reset_shadow(&self) {
            let path = self.roots.canonical_root().join(SHADOW_ROOT_DIRECTORY);
            if path.exists() {
                crate::test_support::remove_dir_all(path);
            }
        }

        fn assert_graph_unchanged(&self) {
            assert_eq!(snapshot_files(&self.graph_root), self.original_graph);
        }

        fn enrollment_binding(&self) -> EnrollmentBindingV1 {
            self.enrollment_binding_with_archive(self.archive_resource_id)
        }

        fn enrollment_binding_with_archive(
            &self,
            archive_resource_id: crate::oplog::CanonicalArchiveResourceId,
        ) -> EnrollmentBindingV1 {
            let accepted = self.authority.binding();
            let storage = accepted.storage_binding();
            EnrollmentBindingV1::new(
                accepted.workspace_id(),
                accepted.lineage_digest(),
                self.verified.catalog_document_id(),
                storage.endpoint.endpoint_id(),
                storage.endpoint.device_id(),
                accepted.graph_resource(),
                storage.receipt_store_id,
                archive_resource_id,
                self.graph.graph_text_scope_binding().unwrap(),
            )
            .unwrap()
        }

        fn enrollment_root(
            &self,
            label: &str,
        ) -> crate::oplog::enrollment::EnrollmentApplicationRoot {
            enrollment_application_root_for_test(
                &self
                    .root
                    .path()
                    .join(format!("enrollment-{}-{label}", Uuid::new_v4())),
            )
            .unwrap()
        }

        fn proofs<'a>(&'a self, shadow: &'a VerifiedShadowProjection) -> VerifiedLocalProofSet<'a> {
            VerifiedLocalProofSet {
                graph: &self.graph,
                roots: &self.roots,
                prepared: &self.prepared,
                verified_publication: &self.verified,
                source_backup: &self.backup,
                accepted_authority: &self.authority,
                sqlite: &self.sqlite,
                sqlite_projection: &self.sqlite_proof,
                shadow_projection: shadow,
            }
        }
    }

    fn snapshot_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut output = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            let mut entries = fs::read_dir(&directory)
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                if metadata.is_dir() {
                    stack.push(path);
                } else {
                    let relative = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    output.insert(relative, fs::read(path).unwrap());
                }
            }
        }
        output
    }

    fn rich_fixture(label: &str) -> Fixture {
        let mut deep = String::from("notes");
        for ordinal in 0..80 {
            deep.push_str(&format!("/層{ordinal:02}"));
        }
        deep.push_str("/Déjà___計画.markdown");
        let mut large = b"title:: Multi chunk\r\n\r\n- ".to_vec();
        large.extend(std::iter::repeat_n(
            b'x',
            BOOTSTRAP_SOURCE_CHUNK_BYTES + 257,
        ));
        large.extend_from_slice(b"\r\n");
        Fixture::new(
            label,
            Some(
                br#"{:pages-directory "notes"
                    :journals-directory "diary"
                    :file/name-format :triple-lowbar
                    :journal/file-name-format "dd-MM-yyyy"
                    :journal/page-title-format "yyyy-MM-dd"}"#,
            ),
            vec![
                (
                    "Root.md".into(),
                    b"title:: Root logical\r\n\r\n- CRLF\r\n".to_vec(),
                ),
                (
                    "notes/a/same.md".into(),
                    b"- same bytes, distinct identity\n".to_vec(),
                ),
                (
                    "notes/b/same-copy.org".into(),
                    b"- same bytes, distinct identity\n".to_vec(),
                ),
                (deep, "\u{feff}- Unicode café\r\n".as_bytes().to_vec()),
                ("diary/nested/25-07-2026.org".into(), Vec::new()),
                ("notes/nested/multi.md".into(), large),
            ],
        )
    }

    fn source_path_with_directory_depth(depth: usize) -> String {
        assert!(depth > 0);
        let mut components = Vec::with_capacity(depth.saturating_add(1));
        components.push("pages".to_owned());
        for ordinal in 1..depth {
            components.push(format!("d{ordinal}"));
        }
        components.push("deepest.md".to_owned());
        components.join("/")
    }

    fn first_payload_file(root: &Path) -> PathBuf {
        let mut stack = vec![root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            for entry in fs::read_dir(directory).unwrap().map(Result::unwrap) {
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                } else {
                    return entry.path();
                }
            }
        }
        panic!("expected payload file")
    }

    #[test]
    fn inactive_shadow_projection_zero_one_and_rich_are_exact_and_read_only() {
        let zero = Fixture::new("zero", None, Vec::new());
        let zero_proof = zero.verify().unwrap();
        assert_eq!(zero_proof.file_count(), 0);
        assert!(fs::read_dir(zero_proof.directory().join(PAYLOAD_DIRECTORY))
            .unwrap()
            .next()
            .is_none());
        zero.assert_graph_unchanged();

        let one = Fixture::new(
            "one",
            None,
            vec![("pages/one.md".into(), b"- one\n".to_vec())],
        );
        let one_proof = one.verify().unwrap();
        assert_eq!(one_proof.file_count(), 1);
        assert_eq!(
            fs::read(one_proof.directory().join("payload/pages/one.md")).unwrap(),
            b"- one\n"
        );
        let mut cursor = one_proof.file_evidence_cursor().unwrap();
        let evidence = cursor.next().unwrap().unwrap();
        assert_eq!(evidence.path().as_str(), "pages/one.md");
        assert_eq!(evidence.page_id(), evidence.intent().page_id());
        assert_eq!(evidence.source(), BlobDescription::of(b"- one\n"));
        assert!(cursor.next().unwrap().is_none());
        cursor.finish().unwrap();
        assert_eq!(one.verify().unwrap(), one_proof);
        one.assert_graph_unchanged();

        let rich = rich_fixture("rich");
        let proof = rich.verify().unwrap();
        assert_eq!(proof.file_count(), 6);
        assert_eq!(proof.total_bytes(), rich.backup.total_bytes());
        assert_eq!(proof.source_backup(), &rich.backup);
        assert_eq!(proof.sqlite_projection(), &rich.sqlite_proof);
        assert_eq!(proof.authority_binding(), rich.authority.binding());
        assert_eq!(proof.catalog_binding().catalog_rows(), proof.file_count());
        assert!(proof.instrumentation().peak_owned_source_bytes <= BOOTSTRAP_SOURCE_MAX_FILE_BYTES);
        assert_eq!(
            proof.instrumentation().bulk_pages_materialized,
            proof.file_count()
        );
        assert!(proof.instrumentation().bulk_materialization_chunks > 0);
        assert!(
            proof.instrumentation().peak_bulk_pages <= BOOTSTRAP_MATERIALIZATION_CHUNK_PAGES as u64
        );
        assert!(rich.sqlite.rebuild.bulk_materialization_chunks > 0);
        assert!(
            rich.sqlite.rebuild.peak_bulk_pages
                <= crate::oplog::hot_engine::BOOTSTRAP_MATERIALIZATION_CHUNK_PAGES
        );
        assert!(rich.sqlite.rebuild.accepted_frontier_session_misses > 0);
        assert!(rich.sqlite.rebuild.external_exact_session_misses > 0);
        assert!(
            rich.sqlite
                .rebuild
                .accepted_frontier_session_peak_resident_bytes
                <= crate::oplog::hot_engine::BOOTSTRAP_LOOKUP_SESSION_BYTES_PER_ROOT
        );
        assert!(
            rich.sqlite
                .rebuild
                .external_exact_session_peak_resident_bytes
                <= crate::oplog::hot_engine::BOOTSTRAP_LOOKUP_SESSION_BYTES_PER_ROOT
        );
        assert!(proof.instrumentation().accepted_frontier_session_misses > 0);
        assert!(proof.instrumentation().external_exact_session_misses > 0);
        assert!(
            proof
                .instrumentation()
                .accepted_frontier_session_peak_resident_bytes
                <= crate::oplog::hot_engine::BOOTSTRAP_LOOKUP_SESSION_BYTES_PER_ROOT as u64
        );
        assert!(
            proof
                .instrumentation()
                .external_exact_session_peak_resident_bytes
                <= crate::oplog::hot_engine::BOOTSTRAP_LOOKUP_SESSION_BYTES_PER_ROOT as u64
        );
        let mut cursor = proof.file_evidence_cursor().unwrap();
        let mut seen = 0;
        let mut page_ids = BTreeMap::new();
        while let Some(evidence) = cursor.next().unwrap() {
            assert_eq!(
                fs::read(
                    proof
                        .directory()
                        .join("payload")
                        .join(evidence.path().as_str())
                )
                .unwrap(),
                fs::read(rich.graph_root.join(evidence.path().as_str())).unwrap()
            );
            page_ids.insert(evidence.path().as_str().to_owned(), evidence.page_id());
            seen += 1;
        }
        cursor.finish().unwrap();
        assert_eq!(seen, 6);
        assert_ne!(
            page_ids["notes/a/same.md"],
            page_ids["notes/b/same-copy.org"]
        );
        rich.assert_graph_unchanged();
    }

    #[test]
    fn managed_shadow_projection_preserves_nested_blank_continuation_bytes() {
        let source = concat!(
            "- Synthetic parent\n",
            "\t- Child first line\n",
            "\t  wrapped continuation\n",
            "\t  \n",
            "\t  middle paragraph\n",
            "\t  \n",
            "\t  final paragraph\n",
            "- Synthetic sibling\n"
        )
        .as_bytes()
        .to_vec();
        let fixture = Fixture::new(
            "blank-continuation",
            None,
            vec![("pages/blank-continuation.md".into(), source.clone())],
        );

        let proof = fixture.verify().unwrap();
        assert_eq!(proof.file_count(), 1);
        assert_eq!(
            fs::read(
                proof
                    .directory()
                    .join("payload/pages/blank-continuation.md")
            )
            .unwrap(),
            source
        );
        fixture.assert_graph_unchanged();
    }

    #[test]
    fn managed_shadow_projection_admits_equivalent_layouts_as_exact_source_baselines() {
        let sources = vec![
            ("pages/no-final.md".into(), b"- no final newline".to_vec()),
            (
                "pages/between-trivia.md".into(),
                b"- first\n \t\n\n- second\n".to_vec(),
            ),
            (
                "pages/crlf.md".into(),
                b"title:: CRLF\r\n\r\n- first\r\n\r\n- second\r\n".to_vec(),
            ),
            (
                "pages/blank-continuation.md".into(),
                concat!(
                    "- parent\n",
                    "\t- child first line\n",
                    "\t  \n",
                    "\t  child final line\n"
                )
                .as_bytes()
                .to_vec(),
            ),
        ];
        let expected = sources.iter().cloned().collect::<BTreeMap<_, _>>();
        let fixture = Fixture::new("semantic-layout-variants", None, sources);

        let verified = fixture.verify().unwrap();
        let binding = PromotedBootstrapProjectionBindingV1::from_verified(&verified).unwrap();
        let authority = BootstrapProjectionAuthority::reopen(&fixture.roots, &binding).unwrap();
        for (path, bytes) in expected {
            assert_eq!(
                fs::read(verified.directory().join("payload").join(&path)).unwrap(),
                bytes,
                "shadow payload for {path} must be byte-exact source"
            );
            let path = ManagedPath::parse(path).unwrap();
            let baseline = authority.baseline_at(&path).unwrap().unwrap();
            let description = BlobDescription::of(&bytes);
            assert_eq!(baseline.source_bytes(), bytes);
            assert_eq!(baseline.intent().target(), description);
            assert_eq!(
                baseline.intent().precondition(),
                &ProjectionPrecondition::Base(description)
            );
            let state = fixture
                .authority
                .accepted_engine()
                .materialize_page_for_projection(baseline.intent().page_id())
                .unwrap();
            let replay = plan_projection(
                fixture.authority.binding().workspace_id(),
                &state,
                Some(baseline.source_bytes()),
            )
            .unwrap();
            assert_eq!(replay.intent(), baseline.intent());
            assert_eq!(replay.target(), baseline.source_bytes());
            let mut edited = state;
            edited.page.blocks[0].content.push_str(" edited");
            let next = crate::oplog::projection::plan_projection_with_layout_annotations(
                fixture.authority.binding().workspace_id(),
                &edited,
                Some(baseline.source_bytes()),
                Some(baseline.intent().annotations()),
            )
            .unwrap();
            assert_ne!(next.target(), baseline.source_bytes());
            assert_eq!(
                next.intent().precondition(),
                &ProjectionPrecondition::Base(description)
            );
        }
        fixture.assert_graph_unchanged();
    }

    #[test]
    fn managed_shadow_projection_admits_mixed_promoted_heading_layout() {
        let source = concat!(
            "- # Focus\n",
            "  - first child\n",
            "## One\n",
            "## Two\n",
            "## Three\n",
            "## Four\n",
            "  - final child\n"
        )
        .as_bytes()
        .to_vec();
        let path = "pages/mixed-headings.md";
        let fixture = Fixture::new(
            "mixed-promoted-heading-layout",
            None,
            vec![(path.into(), source.clone())],
        );

        let verified = fixture.verify().unwrap();
        assert_eq!(
            fs::read(verified.directory().join("payload").join(path)).unwrap(),
            source,
            "the shadow payload must remain the exact bootstrap source"
        );
        let binding = PromotedBootstrapProjectionBindingV1::from_verified(&verified).unwrap();
        let authority = BootstrapProjectionAuthority::reopen(&fixture.roots, &binding).unwrap();
        let baseline = authority
            .baseline_at(&ManagedPath::parse(path).unwrap())
            .unwrap()
            .unwrap();
        let description = BlobDescription::of(&source);
        assert_eq!(baseline.source_bytes(), source);
        assert_eq!(baseline.intent().target(), description);
        assert_eq!(
            baseline.intent().precondition(),
            &ProjectionPrecondition::Base(description)
        );

        let mut state = fixture
            .authority
            .accepted_engine()
            .materialize_page_for_projection(baseline.intent().page_id())
            .unwrap();
        let ordinary =
            plan_projection(fixture.authority.binding().workspace_id(), &state, None).unwrap();
        let ordinary = std::str::from_utf8(ordinary.target()).unwrap();
        for heading in ["One", "Two", "Three", "Four"] {
            assert!(ordinary.contains(&format!("- ## {heading}\n")));
        }
        assert_ne!(ordinary.as_bytes(), source);

        let replay = plan_projection(
            fixture.authority.binding().workspace_id(),
            &state,
            Some(baseline.source_bytes()),
        )
        .unwrap();
        assert_eq!(replay.intent(), baseline.intent());
        assert_eq!(replay.target(), baseline.source_bytes());

        state
            .page
            .blocks
            .iter_mut()
            .find(|block| block.content == "## Three")
            .unwrap()
            .content = "## Three revised".into();
        let edited = crate::oplog::projection::plan_projection_with_layout_annotations(
            fixture.authority.binding().workspace_id(),
            &state,
            Some(baseline.source_bytes()),
            Some(baseline.intent().annotations()),
        )
        .unwrap();
        assert_ne!(edited.target(), baseline.source_bytes());
        assert_ne!(edited.intent().target(), description);
        assert!(std::str::from_utf8(edited.target())
            .unwrap()
            .contains("## Three revised"));
        assert_eq!(
            edited.intent().precondition(),
            &ProjectionPrecondition::Base(description)
        );
        fixture.assert_graph_unchanged();
    }

    #[test]
    fn shadow_bytes_and_promotion_binding_are_identical_with_zero_and_cached_sessions() {
        let fixture = rich_fixture("lookup-session-differential");
        let zero = fixture.verify_with_lookup_budget(0).unwrap();
        let zero_bytes = snapshot_files(zero.directory());
        assert!(zero.instrumentation().accepted_frontier_session_misses > 0);
        assert!(zero.instrumentation().external_exact_session_misses > 0);
        assert!(zero.instrumentation().accepted_frontier_session_oversize > 0);
        assert!(zero.instrumentation().external_exact_session_oversize > 0);
        assert_eq!(
            zero.instrumentation()
                .accepted_frontier_session_peak_resident_bytes,
            0
        );
        assert_eq!(
            zero.instrumentation()
                .external_exact_session_peak_resident_bytes,
            0
        );
        let zero_promotion = PromotedBootstrapProjectionBindingV1::from_verified(&zero).unwrap();

        fixture.reset_shadow();
        let cached = fixture
            .verify_with_lookup_budget(
                crate::oplog::hot_engine::BOOTSTRAP_LOOKUP_SESSION_BYTES_PER_ROOT,
            )
            .unwrap();
        let cached_bytes = snapshot_files(cached.directory());
        assert_eq!(cached, zero);
        assert_eq!(cached_bytes, zero_bytes);
        assert!(cached.instrumentation().accepted_frontier_session_misses > 0);
        assert!(cached.instrumentation().external_exact_session_misses > 0);
        assert!(
            cached
                .instrumentation()
                .accepted_frontier_session_peak_resident_bytes
                <= crate::oplog::hot_engine::BOOTSTRAP_LOOKUP_SESSION_BYTES_PER_ROOT as u64
        );
        assert!(
            cached
                .instrumentation()
                .external_exact_session_peak_resident_bytes
                <= crate::oplog::hot_engine::BOOTSTRAP_LOOKUP_SESSION_BYTES_PER_ROOT as u64
        );
        assert_eq!(
            PromotedBootstrapProjectionBindingV1::from_verified(&cached).unwrap(),
            zero_promotion
        );
        fixture.assert_graph_unchanged();
    }

    #[test]
    fn promoted_bootstrap_authority_reopens_once_and_reads_only_named_exact_entries() {
        let fixture = rich_fixture("promoted-aggregate-point-lookup");
        let verified = fixture.verify().unwrap();
        let binding = PromotedBootstrapProjectionBindingV1::from_verified(&verified).unwrap();
        let mut expected = BTreeMap::new();
        let mut cursor = verified.file_evidence_cursor().unwrap();
        while let Some(entry) = cursor.next().unwrap() {
            expected.insert(
                entry.path().clone(),
                (
                    entry.page_id(),
                    fs::read(fixture.graph_root.join(entry.path().as_str())).unwrap(),
                ),
            );
        }
        cursor.finish().unwrap();
        drop(verified);

        let authority = BootstrapProjectionAuthority::reopen(&fixture.roots, &binding).unwrap();
        let opened = authority.instrumentation();
        assert_eq!(opened.manifest_scans, 0);
        assert_eq!(opened.locator_rows, 0);
        assert!(opened.locator_retained_bytes <= MAX_BOOTSTRAP_PROJECTION_LOCATOR_RETAINED_BYTES);
        assert_eq!(
            (
                opened.graph_scans,
                opened.fsyncs,
                opened.journal_transitions
            ),
            (0, 0, 0)
        );
        for (path, (page_id, bytes)) in &expected {
            let baseline = authority.baseline_at(path).unwrap().unwrap();
            assert_eq!(baseline.intent().path(), path);
            assert_eq!(baseline.intent().page_id(), *page_id);
            assert_eq!(baseline.intent().target(), BlobDescription::of(bytes));
            assert_eq!(baseline.source_bytes(), bytes);
        }
        let after_hits = authority.instrumentation();
        assert_eq!(after_hits.manifest_scans, 1);
        assert_eq!(after_hits.locator_rows, expected.len());
        assert_eq!(after_hits.manifest_entry_reads, expected.len() as u64);
        assert_eq!(after_hits.payload_reads, expected.len() as u64);
        assert!(authority
            .baseline_at(&ManagedPath::parse("notes/missing-計画.markdown").unwrap())
            .unwrap()
            .is_none());
        assert_eq!(authority.instrumentation(), after_hits);

        drop(authority);
        let reopened = BootstrapProjectionAuthority::reopen(&fixture.roots, &binding).unwrap();
        assert_eq!(reopened.instrumentation().manifest_scans, 0);
        let unicode = ManagedPath::parse(
            expected
                .keys()
                .find(|path| path.as_str().contains("Déjà"))
                .unwrap()
                .as_str(),
        )
        .unwrap();
        assert_eq!(
            reopened
                .baseline_at(&unicode)
                .unwrap()
                .unwrap()
                .source_bytes(),
            expected[&unicode].1.as_slice()
        );

        let mut wrong_endpoint = binding.clone();
        wrong_endpoint.endpoint_id = ProjectionEndpointId::from_uuid(Uuid::from_u128(0xdead));
        assert!(BootstrapProjectionAuthority::reopen(&fixture.roots, &wrong_endpoint).is_err());
        let mut wrong_archive = binding.clone();
        wrong_archive.archive_control_binding = ContentDigest::of(b"wrong archive");
        assert!(BootstrapProjectionAuthority::reopen(&fixture.roots, &wrong_archive).is_err());
        let mut wrong_workspace = binding.clone();
        wrong_workspace.workspace_id = WorkspaceId::from_uuid(Uuid::from_u128(0xbeef));
        assert!(BootstrapProjectionAuthority::reopen(&fixture.roots, &wrong_workspace).is_err());
        let mut wrong_graph = binding.clone();
        wrong_graph.graph_resource_id =
            CanonicalGraphResourceId::from_capability_identity(b"wrong", b"graph");
        assert!(BootstrapProjectionAuthority::reopen(&fixture.roots, &wrong_graph).is_err());
        let mut stale = binding.clone();
        stale.accepted_frontier_state_digest = ContentDigest::of(b"stale frontier");
        assert!(BootstrapProjectionAuthority::reopen(&fixture.roots, &stale).is_err());
        fixture.assert_graph_unchanged();
    }

    #[test]
    fn promoted_bootstrap_point_lookup_rejects_changed_payload_bytes() {
        let fixture = Fixture::new(
            "promoted-point-corruption",
            None,
            vec![("pages/exact.md".into(), b"- exact bytes\n".to_vec())],
        );
        let verified = fixture.verify().unwrap();
        let binding = PromotedBootstrapProjectionBindingV1::from_verified(&verified).unwrap();
        let path = ManagedPath::parse("pages/exact.md").unwrap();
        let authority = BootstrapProjectionAuthority::reopen(&fixture.roots, &binding).unwrap();
        fs::write(
            verified
                .directory()
                .join(PAYLOAD_DIRECTORY)
                .join(path.as_str()),
            b"- corrupt byte\n",
        )
        .unwrap();
        assert!(authority.baseline_at(&path).is_err());
    }

    #[test]
    fn promoted_bootstrap_reopen_rejects_duplicate_path_and_wrong_page_entry() {
        fn one(label: &str) -> (Fixture, VerifiedShadowProjection) {
            let fixture = Fixture::new(
                label,
                None,
                vec![("pages/exact.md".into(), b"- exact bytes\n".to_vec())],
            );
            let verified = fixture.verify().unwrap();
            (fixture, verified)
        }

        let (duplicate_fixture, duplicate_verified) = one("promoted-duplicate-path");
        let mut duplicate_binding =
            PromotedBootstrapProjectionBindingV1::from_verified(&duplicate_verified).unwrap();
        let manifest_path = duplicate_verified.directory().join(MANIFEST_FILE);
        let mut manifest = fs::read(&manifest_path).unwrap();
        let header = usize::try_from(duplicate_binding.manifest_header.byte_length()).unwrap();
        let entry = manifest[header..].to_vec();
        manifest.extend_from_slice(&entry);
        fs::write(&manifest_path, &manifest).unwrap();
        duplicate_binding.catalog_rows = 2;
        duplicate_binding.staged_file_count = 2;
        duplicate_binding.manifest = BlobDescription::of(&manifest);
        duplicate_binding.binding_digest = duplicate_binding.compute_binding_digest();
        let authority =
            BootstrapProjectionAuthority::reopen(&duplicate_fixture.roots, &duplicate_binding)
                .unwrap();
        let error = authority
            .baseline_at(&ManagedPath::parse("pages/exact.md").unwrap())
            .err()
            .expect("duplicate path must fail on first access");
        assert!(error.to_string().contains("duplicated"), "{error}");

        let (page_fixture, page_verified) = one("promoted-wrong-page");
        let mut page_binding =
            PromotedBootstrapProjectionBindingV1::from_verified(&page_verified).unwrap();
        let manifest_path = page_verified.directory().join(MANIFEST_FILE);
        let mut manifest = fs::read(&manifest_path).unwrap();
        let mut offset = usize::try_from(page_binding.manifest_header.byte_length()).unwrap();
        let path_length = u32::from_be_bytes(manifest[offset..offset + 4].try_into().unwrap());
        offset += 4 + usize::try_from(path_length).unwrap() + 1;
        let name_length = u32::from_be_bytes(manifest[offset..offset + 4].try_into().unwrap());
        offset += 4 + usize::try_from(name_length).unwrap();
        manifest[offset] ^= 0xff;
        fs::write(&manifest_path, &manifest).unwrap();
        page_binding.manifest = BlobDescription::of(&manifest);
        page_binding.binding_digest = page_binding.compute_binding_digest();
        let authority =
            BootstrapProjectionAuthority::reopen(&page_fixture.roots, &page_binding).unwrap();
        assert!(authority
            .baseline_at(&ManagedPath::parse("pages/exact.md").unwrap())
            .is_err());
    }

    #[test]
    #[ignore = "manual release 10,000-page aggregate authority receipt"]
    fn promoted_bootstrap_10000_page_point_lookup_release_receipt() {
        let mut files = Vec::with_capacity(10_000);
        for page in 0..10_000 {
            let path = format!(
                "notes/規模-{}/層-{}/Página-{page}-計画.{}",
                page % 17,
                page % 31,
                if page % 3 == 0 { "markdown" } else { "md" }
            );
            let mut bytes = format!("title:: Synthetic {page}\r\n\r\n").into_bytes();
            for block in 0..10 {
                bytes.extend_from_slice(
                    format!("- block {page}-{block} exact café 計画\r\n").as_bytes(),
                );
            }
            files.push((path, bytes));
        }
        let started = std::time::Instant::now();
        let fixture_started = std::time::Instant::now();
        let fixture = Fixture::new("promoted-aggregate-10000", None, files);
        let fixture_ms = fixture_started.elapsed().as_millis();
        let verify_started = std::time::Instant::now();
        let verified = fixture.verify().unwrap();
        let verify_ms = verify_started.elapsed().as_millis();
        let binding = PromotedBootstrapProjectionBindingV1::from_verified(&verified).unwrap();
        let cold_started = std::time::Instant::now();
        let authority = BootstrapProjectionAuthority::reopen(&fixture.roots, &binding).unwrap();
        let cold = cold_started.elapsed();
        let lookup_started = std::time::Instant::now();
        for page in 0..10_000 {
            let path = ManagedPath::parse(format!(
                "notes/規模-{}/層-{}/Página-{page}-計画.{}",
                page % 17,
                page % 31,
                if page % 3 == 0 { "markdown" } else { "md" }
            ))
            .unwrap();
            let baseline = authority.baseline_at(&path).unwrap().unwrap();
            assert_eq!(
                fs::read(fixture.graph_root.join(path.as_str())).unwrap(),
                baseline.source_bytes()
            );
            let missing = ManagedPath::parse(format!("notes/missing/不存在-{page}.md")).unwrap();
            assert!(authority.baseline_at(&missing).unwrap().is_none());
        }
        let lookup = lookup_started.elapsed();
        let counters = authority.instrumentation();
        assert_eq!(counters.manifest_scans, 1);
        assert_eq!(counters.locator_rows, 10_000);
        assert_eq!(counters.manifest_entry_reads, 10_000);
        assert_eq!(counters.payload_reads, 10_000);
        assert_eq!(
            (
                counters.graph_scans,
                counters.fsyncs,
                counters.journal_transitions
            ),
            (0, 0, 0)
        );
        assert!(counters.locator_retained_bytes <= MAX_BOOTSTRAP_PROJECTION_LOCATOR_RETAINED_BYTES);
        eprintln!(
            "aggregate-10000 total_ms={} fixture_ms={} shadow_verify_ms={} cold_reopen_ms={} lookups_ms={} locator_bytes={}",
            started.elapsed().as_millis(),
            fixture_ms,
            verify_ms,
            cold.as_millis(),
            lookup.as_millis(),
            counters.locator_retained_bytes
        );
    }

    #[test]
    fn inactive_shadow_projection_skips_collision_losers_and_preserves_all_source_bytes() {
        let fixture = Fixture::new(
            "collision-losers",
            None,
            vec![
                ("a/One.md".into(), b"title:: Shared\n\n- first\n".to_vec()),
                ("b/Two.org".into(), b"#+title: Shared\n\n* later\n".to_vec()),
                (
                    "pages/Foo.md".into(),
                    b"title:: Upper\n\n- upper\n".to_vec(),
                ),
                (
                    "pages/foo.MD".into(),
                    b"title:: Lower\n\n- lower\n".to_vec(),
                ),
                ("twins/Twin.markdown".into(), b"- markdown\n".to_vec()),
                ("twins/Twin.md".into(), b"- md\n".to_vec()),
                ("twins/Twin.org".into(), b"* org\n".to_vec()),
                (
                    "notes/Cafe\u{301}.MD".into(),
                    b"title:: Decomposed\n\n- nfd\n".to_vec(),
                ),
                (
                    "notes/Caf\u{e9}.md".into(),
                    b"title:: Composed\n\n- nfc\n".to_vec(),
                ),
                ("unrelated/Elsewhere.md".into(), b"- unrelated\n".to_vec()),
            ],
        );
        assert_eq!(fixture.prepared.source_capture().source_file_count(), 10);
        let selected =
            bootstrap_authoritative_source_paths(fixture.prepared.source_capture()).unwrap();
        let mut selected_paths = selected
            .iter()
            .map(|path| path.as_str())
            .collect::<Vec<_>>();
        selected_paths.sort_unstable();
        assert_eq!(
            selected_paths,
            vec![
                "a/One.md",
                "notes/Cafe\u{301}.MD",
                "pages/Foo.md",
                "twins/Twin.markdown",
                "unrelated/Elsewhere.md",
            ]
        );

        let proof = fixture.verify().unwrap();
        assert_eq!(proof.file_count(), 5);
        assert_eq!(proof.catalog_binding().catalog_rows(), 5);
        assert!(
            proof.total_bytes() < fixture.backup.total_bytes(),
            "the backup retains every physical source while projection grants only winner authority"
        );
        let payload = proof.directory().join(PAYLOAD_DIRECTORY);
        for path in &selected {
            assert_eq!(
                fs::read(payload.join(path.as_str())).unwrap(),
                fs::read(fixture.graph_root.join(path.as_str())).unwrap()
            );
        }
        for loser in [
            "b/Two.org",
            "notes/Caf\u{e9}.md",
            "pages/foo.MD",
            "twins/Twin.md",
            "twins/Twin.org",
        ] {
            assert!(!payload.join(loser).exists());
        }
        fixture.assert_graph_unchanged();
    }

    #[test]
    fn verified_local_composes_zero_one_and_multipart_terminal_identity_exactly() {
        let zero = Fixture::new("verified-local-zero", None, Vec::new());
        let zero_shadow = zero.verify().unwrap();
        let zero_root = zero.enrollment_root("zero");
        let zero_binding = zero.enrollment_binding();
        let zero_preparation = PreparationId::new();
        let zero_before = snapshot_files(&zero.graph_root);
        let zero_evidence = compose_verified_local(
            &zero_root,
            zero_binding.clone(),
            zero_preparation,
            &zero.proofs(&zero_shadow),
        )
        .unwrap();
        assert_eq!(zero.verified.part_count(), 0);
        assert_eq!(zero_evidence.bootstrap_batch_id(), None);
        assert_eq!(snapshot_files(&zero.graph_root), zero_before);
        let zero_repeat = compose_verified_local(
            &zero_root,
            zero_binding.clone(),
            zero_preparation,
            &zero.proofs(&zero_shadow),
        )
        .unwrap();
        assert_eq!(
            zero_repeat.enrollment_head(),
            zero_evidence.enrollment_head()
        );
        assert_eq!(
            reopen_verified_local(&zero_root, &zero_binding, &zero.proofs(&zero_shadow))
                .unwrap()
                .verification_digest(),
            zero_evidence.verification_digest()
        );

        let one = Fixture::new(
            "verified-local-one",
            None,
            vec![("pages/one.md".into(), b"- one\n".to_vec())],
        );
        let one_shadow = one.verify().unwrap();
        let one_root = one.enrollment_root("one");
        let one_binding = one.enrollment_binding();
        let one_evidence = compose_verified_local(
            &one_root,
            one_binding,
            PreparationId::new(),
            &one.proofs(&one_shadow),
        )
        .unwrap();
        assert_eq!(one.verified.part_count(), 1);
        assert_eq!(
            one_evidence.bootstrap_batch_id(),
            one.prepared
                .aggregate()
                .parts()
                .last()
                .map(|part| part.batch_id())
        );
        one.assert_graph_unchanged();

        let mut multipart_bytes = Vec::new();
        for ordinal in 0..4096 {
            multipart_bytes.extend_from_slice(format!("- operation {ordinal:04}\n").as_bytes());
        }
        let multipart = Fixture::new(
            "verified-local-4096",
            None,
            vec![("pages/multipart.md".into(), multipart_bytes)],
        );
        let multipart_shadow = multipart.verify().unwrap();
        assert_eq!(multipart.verified.part_count(), 2);
        let multipart_root = multipart.enrollment_root("multipart");
        let multipart_evidence = compose_verified_local(
            &multipart_root,
            multipart.enrollment_binding(),
            PreparationId::new(),
            &multipart.proofs(&multipart_shadow),
        )
        .unwrap();
        assert_eq!(
            multipart_evidence.bootstrap_batch_id(),
            multipart
                .prepared
                .aggregate()
                .parts()
                .last()
                .map(|part| part.batch_id())
        );
        multipart.assert_graph_unchanged();
    }

    fn enrollment_head(
        root: &crate::oplog::enrollment::EnrollmentApplicationRoot,
        binding: &EnrollmentBindingV1,
    ) -> ContentDigest {
        match EnrollmentReader::open_existing(root, binding).unwrap() {
            EnrollmentOpen::Present(reader) => reader.current().digest(),
            EnrollmentOpen::Absent => panic!("expected enrollment head"),
        }
    }

    fn enrollment_generation(
        root: &crate::oplog::enrollment::EnrollmentApplicationRoot,
        binding: &EnrollmentBindingV1,
    ) -> u64 {
        match EnrollmentReader::open_existing(root, binding).unwrap() {
            EnrollmentOpen::Present(reader) => reader.current().generation(),
            EnrollmentOpen::Absent => panic!("expected enrollment head"),
        }
    }

    fn enrollment_head_file(
        root: &crate::oplog::enrollment::EnrollmentApplicationRoot,
        binding: &EnrollmentBindingV1,
    ) -> PathBuf {
        root.path()
            .join("sparse-storage/v2/local")
            .join(binding.graph_resource_id().to_string())
            .join("enrollment/head")
    }

    fn enrollment_record_file(
        root: &crate::oplog::enrollment::EnrollmentApplicationRoot,
        binding: &EnrollmentBindingV1,
        digest: ContentDigest,
    ) -> PathBuf {
        enrollment_head_file(root, binding)
            .parent()
            .unwrap()
            .join("records")
            .join(format!("{digest}.enrollment"))
    }

    fn find_file_with_prefix(root: &Path, prefix: &str) -> PathBuf {
        let mut stack = vec![root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            for entry in fs::read_dir(directory).unwrap().map(Result::unwrap) {
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                } else if entry.file_name().to_string_lossy().starts_with(prefix) {
                    return entry.path();
                }
            }
        }
        panic!("missing file with prefix {prefix}");
    }

    #[test]
    fn verified_local_cross_proof_mismatches_never_advance_shadow_head() {
        let first = rich_fixture("verified-local-cross-first");
        let first_shadow = first.verify().unwrap();
        let second = Fixture::new(
            "verified-local-cross-second",
            None,
            vec![("pages/second.md".into(), b"- second\n".to_vec())],
        );
        let second_shadow = second.verify().unwrap();
        let root = first.enrollment_root("cross");
        let binding = first.enrollment_binding();
        let preparation = PreparationId::new();

        let wrong_backup = VerifiedLocalProofSet {
            source_backup: &second.backup,
            ..first.proofs(&first_shadow)
        };
        assert!(
            compose_verified_local(&root, binding.clone(), preparation, &wrong_backup).is_err()
        );
        let shadow_head = enrollment_head(&root, &binding);

        for proofs in [
            VerifiedLocalProofSet {
                accepted_authority: &second.authority,
                ..first.proofs(&first_shadow)
            },
            VerifiedLocalProofSet {
                sqlite_projection: &second.sqlite_proof,
                ..first.proofs(&first_shadow)
            },
            VerifiedLocalProofSet {
                shadow_projection: &second_shadow,
                ..first.proofs(&first_shadow)
            },
            VerifiedLocalProofSet {
                roots: &second.roots,
                ..first.proofs(&first_shadow)
            },
        ] {
            assert!(compose_verified_local(&root, binding.clone(), preparation, &proofs).is_err());
            assert_eq!(enrollment_head(&root, &binding), shadow_head);
        }
        let graph_before = snapshot_files(&first.graph_root);
        compose_verified_local(&root, binding, preparation, &first.proofs(&first_shadow)).unwrap();
        assert_eq!(snapshot_files(&first.graph_root), graph_before);
        first.assert_graph_unchanged();
        second.assert_graph_unchanged();
    }

    #[test]
    fn verified_local_foreign_archive_resource_id_never_advances_shadow_head() {
        // Archive A holds the genuine retained proofs and its own provisioned
        // archive-resource claim; archive B is a second, physically distinct
        // enrolled archive with its own genuine claim.
        let archive_a = Fixture::new(
            "verified-local-archive-a",
            None,
            vec![("pages/a.md".into(), b"- archive a\n".to_vec())],
        );
        let shadow_a = archive_a.verify().unwrap();
        let archive_b = Fixture::new(
            "verified-local-archive-b",
            None,
            vec![("pages/b.md".into(), b"- archive b\n".to_vec())],
        );
        assert_ne!(
            archive_a.archive_resource_id, archive_b.archive_resource_id,
            "two genuinely provisioned archives must have distinct resource ids"
        );

        // Compose archive A's valid proofs under a binding that carries archive
        // B's valid CanonicalArchiveResourceId. The composition must fail and
        // the enrollment head must remain exactly the initial ShadowImport.
        let mismatched = archive_a.enrollment_binding_with_archive(archive_b.archive_resource_id);
        let mismatch_root = archive_a.enrollment_root("foreign-archive");
        let preparation = PreparationId::new();
        let graph_before = snapshot_files(&archive_a.graph_root);

        assert!(compose_verified_local(
            &mismatch_root,
            mismatched.clone(),
            preparation,
            &archive_a.proofs(&shadow_a),
        )
        .is_err());
        let shadow_head = enrollment_head(&mismatch_root, &mismatched);
        assert_eq!(enrollment_generation(&mismatch_root, &mismatched), 1);
        // A retry does not launder the foreign claim into an advance either.
        assert!(compose_verified_local(
            &mismatch_root,
            mismatched.clone(),
            preparation,
            &archive_a.proofs(&shadow_a),
        )
        .is_err());
        assert_eq!(enrollment_head(&mismatch_root, &mismatched), shadow_head);
        assert_eq!(enrollment_generation(&mismatch_root, &mismatched), 1);

        // Archive A's own binding still composes and reopens cleanly.
        let valid = archive_a.enrollment_binding();
        assert_eq!(valid.archive_resource_id(), archive_a.archive_resource_id);
        let valid_root = archive_a.enrollment_root("own-archive");
        let evidence = compose_verified_local(
            &valid_root,
            valid.clone(),
            PreparationId::new(),
            &archive_a.proofs(&shadow_a),
        )
        .unwrap();
        let reopened =
            reopen_verified_local(&valid_root, &valid, &archive_a.proofs(&shadow_a)).unwrap();
        assert_eq!(reopened.enrollment_head(), evidence.enrollment_head());
        assert_eq!(
            reopened.verification_digest(),
            evidence.verification_digest()
        );

        // No path here may write a single byte into either live graph.
        assert_eq!(snapshot_files(&archive_a.graph_root), graph_before);
        archive_a.assert_graph_unchanged();
        archive_b.assert_graph_unchanged();
    }

    #[test]
    fn verified_local_all_enrollment_durability_cuts_resume_one_exact_head() {
        let fixture = Fixture::new(
            "verified-local-enrollment-cuts",
            None,
            vec![(
                "pages/cuts.md".into(),
                b"- enrollment durability\n".to_vec(),
            )],
        );
        let shadow = fixture.verify().unwrap();
        let binding = fixture.enrollment_binding();
        let cuts = [
            CommitCut::AfterRecordTempCreate,
            CommitCut::AfterRecordWrite,
            CommitCut::AfterRecordFileSync,
            CommitCut::AfterRecordLink,
            CommitCut::AfterRecordInsert,
            CommitCut::AfterRecordsDirectorySync,
            CommitCut::AfterHeadTempCreate,
            CommitCut::AfterHeadWrite,
            CommitCut::AfterHeadFileSync,
            CommitCut::AfterHeadReplace,
            CommitCut::AfterEnrollmentDirectorySync,
        ];
        for cut in cuts {
            let root = fixture.enrollment_root("cut");
            let preparation = PreparationId::new();
            assert!(matches!(
                compose_verified_local_at_cut_for_test(
                    &root,
                    binding.clone(),
                    preparation,
                    &fixture.proofs(&shadow),
                    cut,
                ),
                Err(VerifiedLocalCompositionError::Enrollment(
                    crate::oplog::enrollment::EnrollmentError::InjectedCrashCut(_)
                ))
            ));
            let resumed = compose_verified_local(
                &root,
                binding.clone(),
                preparation,
                &fixture.proofs(&shadow),
            )
            .unwrap();
            let repeated = compose_verified_local(
                &root,
                binding.clone(),
                preparation,
                &fixture.proofs(&shadow),
            )
            .unwrap();
            assert_eq!(resumed.enrollment_head(), repeated.enrollment_head());
            assert_eq!(
                resumed.verification_digest(),
                repeated.verification_digest()
            );
        }
        fixture.assert_graph_unchanged();
    }

    #[test]
    fn verified_local_partial_record_write_stays_explicitly_shadow_import() {
        let fixture = Fixture::new(
            "verified-local-partial-record",
            None,
            vec![("pages/partial.md".into(), b"- partial\n".to_vec())],
        );
        let shadow = fixture.verify().unwrap();
        let root = fixture.enrollment_root("partial");
        let binding = fixture.enrollment_binding();
        let preparation = PreparationId::new();
        assert!(compose_verified_local_at_cut_for_test(
            &root,
            binding.clone(),
            preparation,
            &fixture.proofs(&shadow),
            CommitCut::AfterRecordWrite,
        )
        .is_err());
        let shadow_head = enrollment_head(&root, &binding);
        let temp = find_file_with_prefix(root.path(), ".record-tmp-");
        let length = fs::metadata(&temp).unwrap().len();
        fs::OpenOptions::new()
            .write(true)
            .open(&temp)
            .unwrap()
            .set_len(length / 2)
            .unwrap();
        assert!(matches!(
            compose_verified_local(
                &root,
                binding.clone(),
                preparation,
                &fixture.proofs(&shadow),
            ),
            Err(VerifiedLocalCompositionError::Enrollment(
                crate::oplog::enrollment::EnrollmentError::AmbiguousRecordPublication
            ))
        ));
        assert_eq!(enrollment_head(&root, &binding), shadow_head);

        let head_root = fixture.enrollment_root("partial-head");
        let head_preparation = PreparationId::new();
        assert!(compose_verified_local_at_cut_for_test(
            &head_root,
            binding.clone(),
            head_preparation,
            &fixture.proofs(&shadow),
            CommitCut::AfterHeadWrite,
        )
        .is_err());
        let head_temp = find_file_with_prefix(head_root.path(), ".head-tmp-");
        fs::OpenOptions::new()
            .write(true)
            .open(&head_temp)
            .unwrap()
            .set_len(7)
            .unwrap();
        let resumed = compose_verified_local(
            &head_root,
            binding,
            head_preparation,
            &fixture.proofs(&shadow),
        )
        .unwrap();
        assert_eq!(
            reopen_verified_local(&head_root, resumed.binding(), &fixture.proofs(&shadow))
                .unwrap()
                .enrollment_head(),
            resumed.enrollment_head()
        );
        fixture.assert_graph_unchanged();
    }

    #[test]
    fn verified_local_corrupt_or_missing_proofs_fail_before_head_advance() {
        let backup_fixture = Fixture::new(
            "verified-local-corrupt-backup",
            None,
            vec![("pages/backup.md".into(), b"- backup\n".to_vec())],
        );
        let backup_shadow = backup_fixture.verify().unwrap();
        let backup_root = backup_fixture.enrollment_root("backup");
        let backup_binding = backup_fixture.enrollment_binding();
        let backup_preparation = PreparationId::new();
        fs::remove_file(backup_fixture.backup.directory().join("manifest.bin")).unwrap();
        assert!(compose_verified_local(
            &backup_root,
            backup_binding.clone(),
            backup_preparation,
            &backup_fixture.proofs(&backup_shadow),
        )
        .is_err());
        let backup_head = enrollment_head(&backup_root, &backup_binding);
        assert_eq!(enrollment_head(&backup_root, &backup_binding), backup_head);

        let sqlite_fixture = Fixture::new(
            "verified-local-missing-sqlite",
            None,
            vec![("pages/sqlite.md".into(), b"- sqlite\n".to_vec())],
        );
        let sqlite_shadow = sqlite_fixture.verify().unwrap();
        let sqlite_root = sqlite_fixture.enrollment_root("sqlite");
        let sqlite_binding = sqlite_fixture.enrollment_binding();
        let sqlite_preparation = PreparationId::new();
        let database_name = sqlite_fixture
            .sqlite
            .database
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy();
        let sqlite_checkpoint = sqlite_fixture
            .sqlite
            .database
            .path()
            .with_file_name(format!("{database_name}-auth"));
        fs::remove_file(&sqlite_checkpoint).unwrap();
        assert!(compose_verified_local(
            &sqlite_root,
            sqlite_binding.clone(),
            sqlite_preparation,
            &sqlite_fixture.proofs(&sqlite_shadow),
        )
        .is_err());
        let sqlite_head = enrollment_head(&sqlite_root, &sqlite_binding);
        fs::write(&sqlite_checkpoint, b"corrupt checkpoint").unwrap();
        assert!(compose_verified_local(
            &sqlite_root,
            sqlite_binding.clone(),
            sqlite_preparation,
            &sqlite_fixture.proofs(&sqlite_shadow),
        )
        .is_err());
        assert_eq!(enrollment_head(&sqlite_root, &sqlite_binding), sqlite_head);

        let shadow_fixture = Fixture::new(
            "verified-local-corrupt-shadow",
            None,
            vec![("pages/shadow.md".into(), b"- shadow\n".to_vec())],
        );
        let shadow_proof = shadow_fixture.verify().unwrap();
        let shadow_root = shadow_fixture.enrollment_root("shadow");
        let shadow_binding = shadow_fixture.enrollment_binding();
        let shadow_preparation = PreparationId::new();
        fs::write(shadow_proof.directory().join(PROOF_FILE), b"corrupt").unwrap();
        assert!(compose_verified_local(
            &shadow_root,
            shadow_binding.clone(),
            shadow_preparation,
            &shadow_fixture.proofs(&shadow_proof),
        )
        .is_err());
        let shadow_head = enrollment_head(&shadow_root, &shadow_binding);
        assert_eq!(enrollment_head(&shadow_root, &shadow_binding), shadow_head);
    }

    #[test]
    fn verified_local_reopen_rejects_enrollment_record_and_head_corruption() {
        for corrupt_head in [false, true] {
            let fixture = Fixture::new(
                if corrupt_head {
                    "verified-local-head-corruption"
                } else {
                    "verified-local-record-corruption"
                },
                None,
                vec![("pages/enrollment.md".into(), b"- enrollment\n".to_vec())],
            );
            let shadow = fixture.verify().unwrap();
            let root = fixture.enrollment_root("enrollment");
            let binding = fixture.enrollment_binding();
            let evidence = compose_verified_local(
                &root,
                binding.clone(),
                PreparationId::new(),
                &fixture.proofs(&shadow),
            )
            .unwrap();
            if corrupt_head {
                fs::write(enrollment_head_file(&root, &binding), b"corrupt\n").unwrap();
            } else {
                let record = enrollment_record_file(&root, &binding, evidence.enrollment_head());
                let mut bytes = fs::read(&record).unwrap();
                let last = bytes.len() - 1;
                bytes[last] ^= 1;
                fs::write(record, bytes).unwrap();
            }
            assert!(reopen_verified_local(&root, &binding, &fixture.proofs(&shadow)).is_err());
            fixture.assert_graph_unchanged();
        }
    }

    #[test]
    fn verified_local_source_mutation_and_blocked_lifecycle_cannot_advance() {
        let mutation = Fixture::new(
            "verified-local-source-mutation",
            None,
            vec![("pages/source.md".into(), b"- before\n".to_vec())],
        );
        let mutation_shadow = mutation.verify().unwrap();
        let mutation_root = mutation.enrollment_root("mutation");
        let mutation_binding = mutation.enrollment_binding();
        fs::write(
            mutation.graph_root.join("pages/source.md"),
            b"- changed before transition\n",
        )
        .unwrap();
        assert!(compose_verified_local(
            &mutation_root,
            mutation_binding.clone(),
            PreparationId::new(),
            &mutation.proofs(&mutation_shadow),
        )
        .is_err());
        let mutation_head = enrollment_head(&mutation_root, &mutation_binding);
        assert_eq!(
            enrollment_head(&mutation_root, &mutation_binding),
            mutation_head
        );

        let blocked = Fixture::new(
            "verified-local-blocked",
            None,
            vec![("pages/blocked.md".into(), b"- blocked\n".to_vec())],
        );
        let blocked_shadow = blocked.verify().unwrap();
        let blocked_root = blocked.enrollment_root("blocked");
        let blocked_binding = blocked.enrollment_binding();
        let preparation = PreparationId::new();
        let evidence = compose_verified_local(
            &blocked_root,
            blocked_binding.clone(),
            preparation,
            &blocked.proofs(&blocked_shadow),
        )
        .unwrap();
        let mut writer = match crate::oplog::enrollment::EnrollmentWriter::open_existing(
            &blocked_root,
            &blocked_binding,
        )
        .unwrap()
        {
            EnrollmentOpen::Present(writer) => writer,
            EnrollmentOpen::Absent => unreachable!(),
        };
        let blocked_head = writer
            .block_current(
                evidence.enrollment_head(),
                "proof.failed".into(),
                ContentDigest::of(b"blocked evidence"),
            )
            .unwrap()
            .digest();
        drop(writer);
        assert!(matches!(
            compose_verified_local(
                &blocked_root,
                blocked_binding.clone(),
                preparation,
                &blocked.proofs(&blocked_shadow),
            ),
            Err(VerifiedLocalCompositionError::WrongLifecycle(_))
        ));
        assert_eq!(
            enrollment_head(&blocked_root, &blocked_binding),
            blocked_head
        );
        blocked.assert_graph_unchanged();
    }

    #[test]
    fn inactive_shadow_projection_accepts_source_maximum_file_path_depth() {
        let accepted_path = source_path_with_directory_depth(BOOTSTRAP_SOURCE_MAX_DIRECTORY_DEPTH);
        assert_eq!(
            accepted_path.split('/').count(),
            BOOTSTRAP_SOURCE_MAX_DIRECTORY_DEPTH + 1
        );
        let accepted = Fixture::new(
            "maximum-depth",
            None,
            vec![(
                accepted_path.clone(),
                b"- deepest accepted source\n".to_vec(),
            )],
        );
        assert_eq!(accepted.backup.file_count(), 1);
        let proof = accepted.verify().unwrap();
        assert_eq!(
            fs::read(
                proof
                    .directory()
                    .join(PAYLOAD_DIRECTORY)
                    .join(&accepted_path)
            )
            .unwrap(),
            b"- deepest accepted source\n"
        );

        let rejected_root = TestRoot::new("over-maximum-depth");
        let rejected_graph_root = rejected_root.path().join("graph");
        let rejected_path =
            source_path_with_directory_depth(BOOTSTRAP_SOURCE_MAX_DIRECTORY_DEPTH + 1);
        let rejected_destination = rejected_graph_root.join(rejected_path);
        fs::create_dir_all(rejected_destination.parent().unwrap()).unwrap();
        fs::write(rejected_destination, b"- source must reject this depth\n").unwrap();
        let rejected_graph = Graph::open(&rejected_graph_root);
        let rejected_capture_root = rejected_root.path().join("capture");
        fs::create_dir(&rejected_capture_root).unwrap();
        let error = match rejected_graph.capture_inactive_bootstrap_sources(&rejected_capture_root)
        {
            Ok(_) => panic!("source capture accepted one directory beyond its depth cap"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("source directory depth cap exceeded"),
            "{error}"
        );
    }

    #[test]
    fn inactive_shadow_projection_retry_reissues_directory_parent_barriers() {
        let fixture = Fixture::new(
            "directory-parent-barriers",
            None,
            vec![("pages/barriers.md".into(), b"- barriers\n".to_vec())],
        );
        let cuts = [
            (
                ShadowProjectionCrashCut::AfterShadowBaseCreation,
                ShadowProjectionDurabilityBarrier::BackupRootAfterShadowBase,
            ),
            (
                ShadowProjectionCrashCut::AfterShadowWorkspaceCreation,
                ShadowProjectionDurabilityBarrier::ShadowBaseAfterWorkspace,
            ),
            (
                ShadowProjectionCrashCut::AfterStagingRename,
                ShadowProjectionDurabilityBarrier::PublicationParentAfterFinal,
            ),
        ];
        for (cut, expected_barrier) in cuts {
            fixture.reset_shadow();
            SHADOW_PROJECTION_CRASH_CUT.with(|pending| pending.set(Some(cut)));
            assert!(matches!(
                fixture.verify(),
                Err(ShadowProjectionError::InjectedCrashCut(label)) if label == cut.label()
            ));
            SHADOW_DURABILITY_BARRIERS.with(|barriers| barriers.borrow_mut().clear());
            let proof = fixture.verify().unwrap();
            let barriers = SHADOW_DURABILITY_BARRIERS
                .with(|barriers| std::mem::take(&mut *barriers.borrow_mut()));
            assert!(
                barriers.contains(&expected_barrier),
                "retry after {cut:?} did not reach {expected_barrier:?}: {barriers:?}"
            );
            assert_eq!(
                fs::read(proof.directory().join("payload/pages/barriers.md")).unwrap(),
                b"- barriers\n"
            );
        }
    }

    #[test]
    fn inactive_shadow_projection_phase_and_partial_cuts_resume_exactly() {
        let fixture = Fixture::new(
            "cuts",
            None,
            vec![(
                "pages/cuts.md".into(),
                b"- deterministic crash recovery payload\n".to_vec(),
            )],
        );
        let cuts = [
            ShadowProjectionCrashCut::PartialPayloadWrite,
            ShadowProjectionCrashCut::AfterPayloadPublication,
            ShadowProjectionCrashCut::PartialManifestWrite,
            ShadowProjectionCrashCut::AfterManifestPublication,
            ShadowProjectionCrashCut::AfterStagingRename,
            ShadowProjectionCrashCut::PartialProofWrite,
            ShadowProjectionCrashCut::AfterProofPublication,
            ShadowProjectionCrashCut::PartialCommitMarkerWrite,
            ShadowProjectionCrashCut::AfterCommitMarkerPublication,
        ];
        for cut in cuts {
            fixture.reset_shadow();
            SHADOW_PROJECTION_CRASH_CUT.with(|pending| pending.set(Some(cut)));
            assert!(matches!(
                fixture.verify(),
                Err(ShadowProjectionError::InjectedCrashCut(label)) if label == cut.label()
            ));
            let proof = fixture.verify().unwrap();
            assert_eq!(
                fs::read(proof.directory().join("payload/pages/cuts.md")).unwrap(),
                b"- deterministic crash recovery payload\n"
            );
            assert_eq!(
                fixture.verify().unwrap().evidence_digest(),
                proof.evidence_digest()
            );
            fixture.assert_graph_unchanged();
        }
    }

    #[test]
    fn inactive_shadow_projection_tamper_cannot_alter_backup_and_rebuilds_exactly() {
        let fixture = Fixture::new(
            "conflicts",
            None,
            vec![("pages/conflict.md".into(), b"- original bytes\n".to_vec())],
        );
        SHADOW_PROJECTION_CRASH_CUT
            .with(|pending| pending.set(Some(ShadowProjectionCrashCut::PartialPayloadWrite)));
        assert!(fixture.verify().is_err());
        let stage = fs::read_dir(
            fixture
                .roots
                .canonical_root()
                .join(SHADOW_ROOT_DIRECTORY)
                .join(fixture.authority.binding().workspace_id().to_string()),
        )
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_name().to_string_lossy().starts_with('.'))
        .unwrap()
        .path();
        let partial = first_payload_file(&stage.join(PAYLOAD_DIRECTORY));
        fs::write(&partial, b"x").unwrap();
        assert!(matches!(
            fixture.verify(),
            Err(ShadowProjectionError::CorruptOrConflicting(_))
        ));

        fixture.reset_shadow();
        let proof = fixture.verify().unwrap();
        let shadow_payload = proof.directory().join("payload/pages/conflict.md");
        let backup_payload = fixture.backup.directory().join("payload/pages/conflict.md");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            assert_ne!(
                fs::metadata(&shadow_payload).unwrap().ino(),
                fs::metadata(&backup_payload).unwrap().ino(),
                "shadow and migration backup payloads must be independent files"
            );
        }
        fs::write(&shadow_payload, b"- tampered bytes\n").unwrap();
        assert_eq!(fs::read(&backup_payload).unwrap(), b"- original bytes\n");
        assert!(fixture.verify().is_err());

        fixture.reset_shadow();
        let proof = fixture.verify().unwrap();
        assert_eq!(
            fs::read(proof.directory().join("payload/pages/conflict.md")).unwrap(),
            b"- original bytes\n"
        );
        fs::write(proof.directory().join("payload/extra.md"), b"- extra\n").unwrap();
        assert!(fixture.verify().is_err());

        fixture.reset_shadow();
        let proof = fixture.verify().unwrap();
        fs::remove_file(proof.directory().join("payload/pages/conflict.md")).unwrap();
        assert!(fixture.verify().is_err());
        fixture.assert_graph_unchanged();
    }

    #[cfg(unix)]
    #[test]
    fn inactive_shadow_projection_rejects_payload_symlink_retarget() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new(
            "payload-symlink",
            None,
            vec![("pages/symlink.md".into(), b"- protected bytes\n".to_vec())],
        );
        SHADOW_PROJECTION_CRASH_CUT
            .with(|pending| pending.set(Some(ShadowProjectionCrashCut::PartialPayloadWrite)));
        assert!(fixture.verify().is_err());
        let workspace_root = fixture
            .roots
            .canonical_root()
            .join(SHADOW_ROOT_DIRECTORY)
            .join(fixture.authority.binding().workspace_id().to_string());
        let stage = fs::read_dir(workspace_root)
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.file_name().to_string_lossy().starts_with('.'))
            .unwrap()
            .path();
        let partial = first_payload_file(&stage.join(PAYLOAD_DIRECTORY));
        let outside = fixture.root.path().join("outside.txt");
        fs::write(&outside, b"outside remains unchanged").unwrap();
        fs::remove_file(&partial).unwrap();
        symlink(&outside, &partial).unwrap();
        assert!(fixture.verify().is_err());
        assert_eq!(fs::read(outside).unwrap(), b"outside remains unchanged");
        fixture.assert_graph_unchanged();
    }

    #[test]
    fn inactive_shadow_projection_rejects_tampered_manifest_proof_marker_and_wrong_bindings() {
        for file in [MANIFEST_FILE, PROOF_FILE, COMMIT_MARKER_FILE] {
            let fixture = Fixture::new(
                file,
                None,
                vec![("pages/evidence.md".into(), b"- evidence\n".to_vec())],
            );
            let proof = fixture.verify().unwrap();
            fs::write(proof.directory().join(file), b"tampered").unwrap();
            assert!(fixture.verify().is_err(), "{file}");
            fixture.assert_graph_unchanged();
        }

        let first = Fixture::new(
            "wrong-first",
            None,
            vec![("pages/first.md".into(), b"- first\n".to_vec())],
        );
        let second = Fixture::new(
            "wrong-second",
            None,
            vec![("pages/second.md".into(), b"- second\n".to_vec())],
        );
        assert!(verify_inactive_bootstrap_shadow_projection(
            &first.graph,
            &first.roots,
            &first.prepared,
            &first.verified,
            &second.backup,
            &first.authority,
            &first.sqlite,
            &first.sqlite_proof,
        )
        .is_err());
        assert!(verify_inactive_bootstrap_shadow_projection(
            &first.graph,
            &first.roots,
            &first.prepared,
            &first.verified,
            &first.backup,
            &first.authority,
            &first.sqlite,
            &second.sqlite_proof,
        )
        .is_err());
        first.assert_graph_unchanged();
        second.assert_graph_unchanged();
    }

    #[test]
    fn inactive_shadow_projection_blocks_live_graph_mutation_between_checks() {
        let missing = Fixture::new(
            "source-missing",
            None,
            vec![("pages/missing.md".into(), b"- present\n".to_vec())],
        );
        fs::remove_file(missing.graph_root.join("pages/missing.md")).unwrap();
        assert!(missing.verify().is_err());

        let fixture = Fixture::new(
            "source-race",
            None,
            vec![("pages/race.md".into(), b"- before\n".to_vec())],
        );
        let source = fixture.graph_root.join("pages/race.md");
        let extra = fixture.graph_root.join("pages/extra.md");
        fs::write(&extra, b"- extra source\n").unwrap();
        assert!(fixture.verify().is_err());
        fs::remove_file(&extra).unwrap();
        SHADOW_BEFORE_FINAL_SOURCE_VERIFY.with(|hook| {
            *hook.borrow_mut() = Some(Box::new({
                let source = source.clone();
                move || fs::write(source, b"- changed externally\n")
            }));
        });
        assert!(fixture.verify().is_err());
        assert_eq!(fs::read(&source).unwrap(), b"- changed externally\n");
        assert!(fixture.verify().is_err());
    }

    fn synthetic_normal_sparse_mismatch() -> (
        PageId,
        ManagedPath,
        Vec<u8>,
        ProjectionIntent,
        ProjectionPageState,
    ) {
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(0x8200));
        let page_id = PageId::from_uuid(Uuid::from_u128(0x8201));
        let block_id = BlockId::from_uuid(Uuid::from_u128(0x8202));
        let home = DocumentId::from_uuid(Uuid::from_u128(0x8203));
        let logseq_uuid = LogseqUuid::parse("00000000-0000-0000-0000-000000008204").unwrap();
        let state = ProjectionPageState {
            page: MaterializedPage {
                page_id,
                home_document_id: home,
                name: LogicalPageName::parse("Renderer mismatch").unwrap(),
                path: ManagedPath::parse("pages/renderer.md").unwrap(),
                kind: ManagedTextKind::Page,
                preamble: None,
                blocks: vec![MaterializedBlock {
                    block_id,
                    home_document_id: home,
                    parent: None,
                    order: "a".into(),
                    logseq_uuid: Some(logseq_uuid),
                    logseq_identity_origin: Some(LogseqIdentityOrigin::PolicyGenerated {
                        reason: PolicyGeneratedAnchorReason::BlockReference,
                    }),
                    content: "target".into(),
                }],
                stats: MaterializationStats::default(),
            },
            frontier: FrontierV2::new(vec![DocumentDependencies::new(
                home,
                vec![CrdtPeerCounter::new(CrdtPeerId::from_u64(1), 0)],
                vec![],
            )
            .unwrap()])
            .unwrap(),
            claim_evidence: vec![ProjectionClaimEvidence::new(
                logseq_uuid,
                vec![ProjectionClaimParticipant::new(block_id, home)],
            )
            .unwrap()],
        };
        let path = ManagedPath::parse("pages/renderer.md").unwrap();
        let source = b"- target\r\n".to_vec();
        let plan = plan_projection(workspace, &state, Some(&source)).unwrap();
        (page_id, path, source, plan.intent().clone(), state)
    }

    #[test]
    fn bootstrap_baseline_rebinds_only_frontier_only_semantic_successors() {
        let (page_id, path, source, _, mut state) = synthetic_normal_sparse_mismatch();
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(0x8200));
        state.page.blocks[0].logseq_uuid = None;
        state.page.blocks[0].logseq_identity_origin = None;
        state.claim_evidence.clear();
        let baseline_plan =
            plan_projection_adopting_exact_source(workspace, &state, &source).unwrap();
        let baseline = BootstrapProjectionBaseline {
            intent: baseline_plan.intent().clone(),
            kind: state.page.kind,
            source: source.clone(),
            owner_binding: ContentDigest::of(b"semantic-successor-owner"),
        };
        let mut successor = state.clone();
        successor.frontier = FrontierV2::new(vec![DocumentDependencies::new(
            successor.page.home_document_id,
            vec![CrdtPeerCounter::new(CrdtPeerId::from_u64(1), 1)],
            vec![],
        )
        .unwrap()])
        .unwrap();

        let rebound = baseline
            .rebind_semantic_successor(workspace, &successor)
            .unwrap();
        assert_eq!(rebound.target(), source);
        assert_eq!(rebound.intent().page_id(), page_id);
        assert_eq!(rebound.intent().path(), &path);
        assert_eq!(rebound.intent().frontier(), &successor.frontier);

        successor.page.blocks[0].content = "semantic change".into();
        assert!(baseline
            .rebind_semantic_successor(workspace, &successor)
            .is_err());
    }

    #[test]
    fn normal_sparse_content_mismatch_reports_path_lengths_and_first_offset() {
        let (page_id, path, source, intent, _) = synthetic_normal_sparse_mismatch();
        let mut projected = source.clone();
        projected[3] = b'X';
        let error = require_exact_source_baseline(
            &projected,
            &intent,
            &source,
            page_id,
            &path,
            BlobDescription::of(&source),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("pages/renderer.md"), "{error}");
        assert!(error.contains("source byte length=10"), "{error}");
        assert!(error.contains("projected byte length=10"), "{error}");
        assert!(error.contains("first differing byte offset 3"), "{error}");
        assert!(!error.contains("target\r\n"), "{error}");
    }

    #[test]
    fn normal_sparse_prefix_mismatch_reports_common_prefix_end() {
        let (page_id, path, source, intent, _) = synthetic_normal_sparse_mismatch();
        let projected = &source[..source.len() - 2];
        let error = require_exact_source_baseline(
            projected,
            &intent,
            &source,
            page_id,
            &path,
            BlobDescription::of(&source),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("pages/renderer.md"), "{error}");
        assert!(error.contains("source byte length=10"), "{error}");
        assert!(error.contains("projected byte length=8"), "{error}");
        assert!(error.contains("common prefix ended"), "{error}");
    }

    #[test]
    fn normal_sparse_equal_bytes_name_failed_non_byte_binding_checks() {
        let (page_id, path, source, intent, _) = synthetic_normal_sparse_mismatch();
        let error = require_exact_source_baseline(
            &source,
            &intent,
            &source,
            page_id,
            &path,
            BlobDescription::of(&source),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("bytes are equal"), "{error}");
        assert!(error.contains("intent target"), "{error}");
    }

    #[test]
    fn inactive_shadow_projection_forced_multipart_4096_operations_uses_default_stack() {
        let mut source = String::new();
        for ordinal in 0..MAX_OPERATIONS_PER_BOOTSTRAP_PART {
            source.push_str(&format!("- operation {ordinal}\n"));
        }
        let fixture = Fixture::new(
            "multipart-4096",
            None,
            vec![("pages/multipart.md".into(), source.into_bytes())],
        );
        assert!(fixture.prepared.aggregate().parts().len() > 1);
        let proof = fixture.verify().unwrap();
        assert_eq!(proof.file_count(), 1);
        assert!(proof.instrumentation().peak_owned_catalog_rows <= 1);
        fixture.assert_graph_unchanged();
    }
}
