//! Read-only classification of the retired device-local enrollment journal,
//! plus the binding and private-root records still consumed by current runtime
//! discovery. No writer, lease, transition, or recovery authority remains.

#[cfg(windows)]
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
#[cfg(windows)]
use cap_std::fs::OpenOptions;
#[cfg(windows)]
use cap_std::fs::{MetadataExt as _, OpenOptionsExt as _};
use cap_std::{ambient_authority, fs::Dir};
use crc32fast::hash as crc32;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
#[cfg(unix)]
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
#[cfg(unix)]
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt as _;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use super::identity::parse_digest;
use super::import::BootstrapStreamingImportError;
use super::object_store::open_dir_nofollow;
use super::sqlite::ProjectionError;
use super::sync_layout::{
    ENROLLMENT_AUTHORITY_FILE as AUTHORITY_FILE,
    ENROLLMENT_AUTHORITY_TEMP_PREFIX as AUTHORITY_TEMP_PREFIX,
    ENROLLMENT_DIR as ENROLLMENT_DIRECTORY, ENROLLMENT_HEAD_FILE as HEAD_FILE,
    ENROLLMENT_HEAD_TEMP_PREFIX as HEAD_TEMP_PREFIX, ENROLLMENT_LEASE_FILE as LEASE_FILE,
    ENROLLMENT_LOCAL_DIR as LOCAL_DIRECTORY, ENROLLMENT_RECORDS_DIR as RECORDS_DIRECTORY,
    ENROLLMENT_RECORD_SUFFIX as RECORD_SUFFIX, ENROLLMENT_STORAGE_DIR as SPARSE_STORAGE_DIRECTORY,
    ENROLLMENT_VERSION_DIR as STORAGE_VERSION_DIRECTORY, LOCAL_ACTIVATION_RESERVATION_FILE,
};
use super::{
    BatchId, BlobDescription, CanonicalArchiveResourceId, CanonicalGraphResourceId, ContentDigest,
    DeviceId, DocumentId, GraphTextScopeBinding, ImportId, LineageDigest, ProjectionEndpointId,
    ProjectionReceiptStoreId, SessionId, WorkspaceId, DIFF_SCHEMA_VERSION,
    MANAGED_ENTITY_SET_VERSION, MANIFEST_ENCODING_VERSION, OBJECT_ENVELOPE_SCHEMA_VERSION,
    OPERATION_SCHEMA_VERSION, OPLOG_PROTOCOL_VERSION, PROJECTION_POLICY_VERSION,
    PROJECTION_SCHEMA_VERSION, RECEIPT_SCHEMA_VERSION,
};

pub(crate) const ENROLLMENT_RECORD_SCHEMA_VERSION: u32 = 6;
pub(crate) const PUBLISHED_RECOVERY_PACKET_SCHEMA_VERSION: u32 = 1;
pub(crate) const SHARED_ENROLLMENT_DESCRIPTOR_SCHEMA_VERSION: u32 = 1;
pub(crate) const JOINER_WORKSPACE_ARCHIVE_SCHEMA_VERSION: u32 = 1;
pub(crate) const LOCAL_ACTIVATION_RESERVATION_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_ENROLLMENT_RECORD_BYTES: usize = 32 * 1024;
pub(crate) const MAX_ENROLLMENT_JSON_DEPTH: usize = 16;
/// All lifecycle records remain bounded and read with a single fixed parser
/// budget.  Shared enrollment carries one exact descriptor plus local archive
/// proof, so its authenticated record is intentionally larger than the legacy
/// LocalActive handoff record while still far below the 32 KiB byte ceiling.
pub(crate) const MAX_ENROLLMENT_JSON_TOKENS: usize = 768;
pub(crate) const MAX_ENROLLMENT_OPEN_CHAIN_RECORDS: usize = 64;
pub(crate) const MAX_ENROLLMENT_NAMESPACE_ENTRIES: usize = 2048;
pub(crate) const MAX_BLOCKED_REASON_CODE_BYTES: usize = 64;

const HEAD_BYTES: usize = 65;
const MAX_LOCAL_ACTIVATION_RESERVATION_BYTES: usize = 4 * 1024;
const ENROLLMENT_AUTHORITY_SCHEMA_VERSION: u32 = 2;
const ENROLLMENT_CHECKPOINT_SCHEMA_VERSION: u32 = 3;
const MAX_ENROLLMENT_AUTHORITY_BYTES: usize = 4 * 1024;

#[cfg(test)]
thread_local! {
    static ENROLLMENT_RECORD_READS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static ENROLLMENT_HEAD_READS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static ENROLLMENT_NAMESPACE_SCANS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static ENROLLMENT_DIRECTORY_OPENS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static ENROLLMENT_LEASE_ACQUISITIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static ENROLLMENT_AUTHORITY_CLAIM_READS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static FAIL_NEXT_ENROLLMENT_HEAD_READ: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

#[cfg(test)]
pub(crate) fn fail_next_enrollment_head_read() {
    FAIL_NEXT_ENROLLMENT_HEAD_READ.with(|fault| fault.set(true));
}

/// Exact causal accounting for the enrollment journal's filesystem work.
///
/// Every field is an operation count, never a duration, so a bounded-admission
/// assertion is deterministic and machine independent.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct EnrollmentInstrumentation {
    /// Authenticated content-addressed record reads. This is the record-chain
    /// walk a per-mutation admission must never perform.
    pub(crate) record_reads: usize,
    /// Reads of the tiny fixed-size committed head file.
    pub(crate) head_reads: usize,
    /// Enrollment namespace enumerations.
    pub(crate) namespace_scans: usize,
    /// Enrollment directory-tree opens.
    pub(crate) directory_opens: usize,
    /// OS enrollment-lease acquisitions.
    pub(crate) lease_acquisitions: usize,
    /// Authority-claim file reads.
    pub(crate) authority_claim_reads: usize,
}

#[cfg(test)]
impl EnrollmentInstrumentation {
    pub(crate) fn capture() -> Self {
        Self {
            record_reads: ENROLLMENT_RECORD_READS.with(std::cell::Cell::get),
            head_reads: ENROLLMENT_HEAD_READS.with(std::cell::Cell::get),
            namespace_scans: ENROLLMENT_NAMESPACE_SCANS.with(std::cell::Cell::get),
            directory_opens: ENROLLMENT_DIRECTORY_OPENS.with(std::cell::Cell::get),
            lease_acquisitions: ENROLLMENT_LEASE_ACQUISITIONS.with(std::cell::Cell::get),
            authority_claim_reads: ENROLLMENT_AUTHORITY_CLAIM_READS.with(std::cell::Cell::get),
        }
    }

    /// The work performed since `self` was captured.
    pub(crate) fn since(self) -> Self {
        let now = Self::capture();
        Self {
            record_reads: now.record_reads - self.record_reads,
            head_reads: now.head_reads - self.head_reads,
            namespace_scans: now.namespace_scans - self.namespace_scans,
            directory_opens: now.directory_opens - self.directory_opens,
            lease_acquisitions: now.lease_acquisitions - self.lease_acquisitions,
            authority_claim_reads: now.authority_claim_reads - self.authority_claim_reads,
        }
    }
}

#[cfg(test)]
fn count(counter: &'static std::thread::LocalKey<std::cell::Cell<usize>>) {
    counter.with(|value| value.set(value.get().saturating_add(1)));
}

/// A private application-data root selected by Tine, never a graph path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EnrollmentApplicationRoot {
    path: PathBuf,
}

impl EnrollmentApplicationRoot {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Retain one already-existing enrollment application root without creating
/// or repairing any namespace below it.
///
/// Runtime discovery has already classified this path, but the actor uses this
/// fresh capability so no advisory path value itself becomes writer authority.
pub(crate) fn open_existing_enrollment_application_root(
    path: &Path,
) -> Result<EnrollmentApplicationRoot, EnrollmentError> {
    open_existing_application_root(path)?.ok_or_else(|| {
        EnrollmentError::UnsafeNamespace(
            "discovered enrollment application root no longer exists".into(),
        )
    })
}

/// Opaque identity of one non-mutating enrollment preparation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct PreparationId(Uuid);

impl PreparationId {
    pub(crate) const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }
}

/// Exact caller identities fixed before any graph-local archive namespace is
/// opened. This private reservation identity is deliberately stricter than the
/// eventual runtime binding: an honest crash resume must use the same
/// preparation and activation session as the call that first reserved it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalActivationIdentityV1 {
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    catalog_document_id: DocumentId,
    endpoint_id: ProjectionEndpointId,
    device_id: DeviceId,
    graph_resource_id: CanonicalGraphResourceId,
    preparation_id: PreparationId,
    session_id: SessionId,
}

impl LocalActivationIdentityV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) const fn new(
        workspace_id: WorkspaceId,
        lineage_digest: LineageDigest,
        catalog_document_id: DocumentId,
        endpoint_id: ProjectionEndpointId,
        device_id: DeviceId,
        graph_resource_id: CanonicalGraphResourceId,
        preparation_id: PreparationId,
        session_id: SessionId,
    ) -> Self {
        Self {
            workspace_id,
            lineage_digest,
            catalog_document_id,
            endpoint_id,
            device_id,
            graph_resource_id,
            preparation_id,
            session_id,
        }
    }
}

/// Complete private pre-enrollment reservation binding. Receipt-store,
/// graph-scope, and source-inventory evidence are freshly derived before this
/// is published, so archive construction never relies on a pathname-only
/// assertion.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalActivationReservationBindingV1 {
    identity: LocalActivationIdentityV1,
    receipt_store_id: ProjectionReceiptStoreId,
    graph_text_scope_binding: GraphTextScopeBinding,
    source_inventory_digest: ContentDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalActivationReservationV1 {
    schema_version: u32,
    binding: LocalActivationReservationBindingV1,
    archive_instance_id: Uuid,
}

/// Authenticated-by-private-root, bounded evidence that makes an archive
/// construction crash explicitly resumable before ShadowImport exists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LocalActivationReservation {
    record: LocalActivationReservationV1,
}

impl LocalActivationReservation {
    pub(crate) const fn identity(&self) -> &LocalActivationIdentityV1 {
        &self.record.binding.identity
    }
}

/// Read one existing private reservation without creating the application
/// root, enrollment namespace, archive, or writer lease.
pub(crate) fn inspect_local_activation_reservation_at(
    root_path: &Path,
) -> Result<Option<LocalActivationReservation>, EnrollmentError> {
    let Some(root) = open_existing_application_root(root_path)? else {
        return Ok(None);
    };
    open_local_activation_reservation(&root)
}

fn open_local_activation_reservation(
    root: &EnrollmentApplicationRoot,
) -> Result<Option<LocalActivationReservation>, EnrollmentError> {
    let directory = Dir::open_ambient_dir(root.path(), ambient_authority())?;
    match directory.symlink_metadata(LOCAL_ACTIVATION_RESERVATION_FILE) {
        Ok(metadata) if !cap_metadata_is_authoritative_file(&metadata) => {
            return Err(EnrollmentError::UnsafeNamespace(
                "local activation reservation is not a regular no-follow file".into(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let (bytes, _) = read_bounded_authoritative_file(
        &directory,
        LOCAL_ACTIVATION_RESERVATION_FILE,
        MAX_LOCAL_ACTIVATION_RESERVATION_BYTES,
        "local activation reservation",
        true,
    )?;
    let record: LocalActivationReservationV1 = serde_json::from_slice(&bytes)
        .map_err(|error| EnrollmentError::Decode(error.to_string()))?;
    if record.schema_version != LOCAL_ACTIVATION_RESERVATION_SCHEMA_VERSION {
        return Err(
            EnrollmentError::UnsupportedLocalActivationReservationSchema(record.schema_version),
        );
    }
    let canonical =
        serde_json::to_vec(&record).map_err(|error| EnrollmentError::Encode(error.to_string()))?;
    if canonical != bytes {
        return Err(EnrollmentError::NonCanonicalLocalActivationReservation);
    }
    Ok(Some(LocalActivationReservation { record }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnrollmentCompatibilityV1 {
    oplog_protocol_version: u32,
    operation_schema_version: u32,
    object_envelope_schema_version: u32,
    manifest_encoding_version: u32,
    receipt_schema_version: u32,
    projection_schema_version: u32,
    projection_policy_version: u32,
    managed_entity_set_version: u32,
    diff_schema_version: u32,
}

impl EnrollmentCompatibilityV1 {
    pub(crate) const fn current() -> Self {
        Self {
            oplog_protocol_version: OPLOG_PROTOCOL_VERSION,
            operation_schema_version: OPERATION_SCHEMA_VERSION,
            object_envelope_schema_version: OBJECT_ENVELOPE_SCHEMA_VERSION,
            manifest_encoding_version: MANIFEST_ENCODING_VERSION,
            receipt_schema_version: RECEIPT_SCHEMA_VERSION,
            projection_schema_version: PROJECTION_SCHEMA_VERSION,
            projection_policy_version: PROJECTION_POLICY_VERSION,
            managed_entity_set_version: MANAGED_ENTITY_SET_VERSION,
            diff_schema_version: DIFF_SCHEMA_VERSION,
        }
    }

    fn validate_current(self) -> Result<(), EnrollmentError> {
        if self != Self::current() {
            return Err(EnrollmentError::UnsupportedCompatibility {
                expected: Self::current(),
                found: self,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnrollmentBindingV1 {
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    catalog_document_id: DocumentId,
    endpoint_id: ProjectionEndpointId,
    device_id: DeviceId,
    graph_resource_id: CanonicalGraphResourceId,
    receipt_store_id: ProjectionReceiptStoreId,
    archive_resource_id: CanonicalArchiveResourceId,
    graph_text_scope_binding: GraphTextScopeBinding,
    compatibility: EnrollmentCompatibilityV1,
}

impl EnrollmentBindingV1 {
    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) const fn lineage_digest(&self) -> LineageDigest {
        self.lineage_digest
    }

    pub(crate) const fn catalog_document_id(&self) -> DocumentId {
        self.catalog_document_id
    }

    pub(crate) const fn endpoint_id(&self) -> ProjectionEndpointId {
        self.endpoint_id
    }

    pub(crate) const fn device_id(&self) -> DeviceId {
        self.device_id
    }

    fn validate_internal(&self) -> Result<(), EnrollmentError> {
        self.compatibility.validate_current()?;
        if self.graph_text_scope_binding.graph_resource_id() != self.graph_resource_id {
            return Err(EnrollmentError::BindingMismatch(
                EnrollmentBindingField::GraphTextScope,
            ));
        }
        Ok(())
    }

    fn validate_exact(&self, expected: &Self) -> Result<(), EnrollmentError> {
        self.validate_internal()?;
        let mismatch = if self.workspace_id != expected.workspace_id {
            Some(EnrollmentBindingField::Workspace)
        } else if self.lineage_digest != expected.lineage_digest {
            Some(EnrollmentBindingField::Lineage)
        } else if self.catalog_document_id != expected.catalog_document_id {
            Some(EnrollmentBindingField::CatalogDocument)
        } else if self.endpoint_id != expected.endpoint_id {
            Some(EnrollmentBindingField::Endpoint)
        } else if self.device_id != expected.device_id {
            Some(EnrollmentBindingField::Device)
        } else if self.graph_resource_id != expected.graph_resource_id {
            Some(EnrollmentBindingField::GraphResource)
        } else if self.receipt_store_id != expected.receipt_store_id {
            Some(EnrollmentBindingField::ReceiptStore)
        } else if self.archive_resource_id != expected.archive_resource_id {
            Some(EnrollmentBindingField::ArchiveResource)
        } else if self.graph_text_scope_binding != expected.graph_text_scope_binding {
            Some(EnrollmentBindingField::GraphTextScope)
        } else if self.compatibility != expected.compatibility {
            Some(EnrollmentBindingField::Compatibility)
        } else {
            None
        };
        if let Some(field) = mismatch {
            return Err(EnrollmentError::BindingMismatch(field));
        }
        Ok(())
    }
}

/// The current claim deliberately keeps the historical filename: its exact
/// filesystem identity is bound into the lease protocol.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentAuthorityClaim {
    schema_version: u32,
    authority_id: Uuid,
    lease_resource_id: ContentDigest,
    binding: EnrollmentBindingV1,
    initial_preparation_id: PreparationId,
    initial_source_inventory_digest: ContentDigest,
}

impl EnrollmentAuthorityClaim {
    const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    const fn authority_id(&self) -> Uuid {
        self.authority_id
    }

    const fn lease_resource_id(&self) -> ContentDigest {
        self.lease_resource_id
    }

    fn binding(&self) -> &EnrollmentBindingV1 {
        &self.binding
    }
}

struct EnrollmentAuthorityMaterial {
    claim: EnrollmentAuthorityClaim,
    resource_id: ContentDigest,
}

impl EnrollmentAuthorityMaterial {
    fn from_claim(
        claim: EnrollmentAuthorityClaim,
        resource_id: ContentDigest,
        expected_binding: &EnrollmentBindingV1,
        expected_lease_resource_id: ContentDigest,
    ) -> Result<Self, EnrollmentError> {
        if claim.schema_version() != ENROLLMENT_AUTHORITY_SCHEMA_VERSION {
            return Err(EnrollmentError::UnsupportedAuthoritySchema(
                claim.schema_version(),
            ));
        }
        claim.binding().validate_exact(expected_binding)?;
        if claim.lease_resource_id() != expected_lease_resource_id {
            return Err(EnrollmentError::LeaseResourceMismatch);
        }
        Ok(Self { claim, resource_id })
    }

    fn verify_checkpoint(&self, record: &EnrollmentRecordV1) -> Result<(), EnrollmentError> {
        let checkpoint = record
            .checkpoint
            .as_ref()
            .ok_or(EnrollmentError::MissingAuthenticatedCheckpoint)?;
        match (record.schema_version, checkpoint) {
            (ENROLLMENT_RECORD_SCHEMA_VERSION, EnrollmentCheckpoint::Current(checkpoint)) => {
                if checkpoint.schema_version != ENROLLMENT_CHECKPOINT_SCHEMA_VERSION {
                    return Err(EnrollmentError::UnsupportedCheckpointSchema(
                        checkpoint.schema_version,
                    ));
                }
                if checkpoint.authority_id != self.claim.authority_id()
                    || checkpoint.authority_resource_id != self.resource_id
                {
                    return Err(EnrollmentError::AuthorityMismatch);
                }
                let message = current_checkpoint_message_bytes(
                    checkpoint.authority_id,
                    checkpoint.authority_resource_id,
                    record.generation,
                    record.previous,
                    record.history_accumulator,
                    record.lease_resource_id,
                    &record.binding,
                    &record.lifecycle,
                )?;
                if crc32(&message) == checkpoint.integrity_tag {
                    Ok(())
                } else {
                    Err(EnrollmentError::CheckpointIntegrityFailed)
                }
            }
            _ => Err(EnrollmentError::IllegalCheckpointPair),
        }
    }
}

struct EnrollmentAuthority {
    material: EnrollmentAuthorityMaterial,
    file: File,
    directory: Dir,
    identity: AuthoritativeFileIdentity,
}

impl EnrollmentAuthority {
    fn validate_current(&self) -> Result<(), EnrollmentError> {
        #[cfg(test)]
        count(&ENROLLMENT_AUTHORITY_CLAIM_READS);
        validate_authoritative_file(&self.file, "enrollment authority claim")?;
        if authoritative_file_identity(&self.file)? != self.identity {
            return Err(EnrollmentError::AuthorityMismatch);
        }
        let reopened = open_regular_readonly(&self.directory, AUTHORITY_FILE)
            .map_err(|_| EnrollmentError::AuthorityMismatch)?;
        validate_authoritative_file(&reopened, "enrollment authority claim")?;
        if authoritative_file_identity(&reopened)? != self.identity {
            return Err(EnrollmentError::AuthorityMismatch);
        }
        let expected = canonical_authority_claim_bytes(&self.material.claim)?;
        let mut bytes = Vec::with_capacity(expected.len());
        reopened
            .take((MAX_ENROLLMENT_AUTHORITY_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes != expected {
            return Err(EnrollmentError::AuthorityMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnrollmentBindingField {
    Workspace,
    Lineage,
    CatalogDocument,
    Endpoint,
    Device,
    GraphResource,
    ReceiptStore,
    ArchiveResource,
    GraphTextScope,
    Compatibility,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptedFrontierAnchorV1 {
    acceptance_sequence: u64,
    accepted_frontier_state_digest: ContentDigest,
    history_generation: u64,
    history_root: ContentDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShadowImportV1 {
    preparation_id: PreparationId,
    source_inventory_digest: ContentDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifiedLocalV1 {
    preparation_id: PreparationId,
    source_inventory_digest: ContentDigest,
    source_file_count: u64,
    source_chunk_count: u64,
    source_total_bytes: u64,
    backup_manifest: BlobDescription,
    backup_restore_proof: BlobDescription,
    backup_evidence_digest: ContentDigest,
    bootstrap_import_id: ContentDigest,
    bootstrap_part_count: u32,
    bootstrap_terminal_part_id: Option<ContentDigest>,
    bootstrap_batch_id: Option<BatchId>,
    accepted_frontier_anchor: AcceptedFrontierAnchorV1,
    accepted_history_record_count: u64,
    catalog_row_count: u64,
    sqlite_accepted_batch_count: u64,
    sqlite_semantic_projection_digest: ContentDigest,
    sqlite_materialized_row_digest: ContentDigest,
    staged_projection_manifest: BlobDescription,
    staged_projection_proof: BlobDescription,
    staged_file_count: u64,
    staged_total_bytes: u64,
    byte_compare_digest: ContentDigest,
    shadow_evidence_digest: ContentDigest,
    proof_binding_digest: ContentDigest,
}

impl VerifiedLocalV1 {
    fn validate_fields(&self) -> Result<(), EnrollmentError> {
        let part_count = u64::from(self.bootstrap_part_count);
        let zero = self.bootstrap_part_count == 0;
        if self.bootstrap_batch_id.is_none() != zero
            || self.bootstrap_terminal_part_id.is_none() != zero
            || (self.accepted_frontier_anchor.history_root
                == super::object_store::empty_engine_history_root_digest())
                != zero
            || self.accepted_frontier_anchor.acceptance_sequence != part_count
            || self.accepted_frontier_anchor.history_generation != part_count
            || self.accepted_history_record_count != part_count
            || self.sqlite_accepted_batch_count != part_count
            || (self.source_file_count == 0) != zero
            || (zero && (self.source_chunk_count != 0 || self.source_total_bytes != 0))
            || self.source_file_count != self.catalog_row_count
            || self.source_file_count != self.staged_file_count
            || self.source_total_bytes != self.staged_total_bytes
        {
            return Err(EnrollmentError::InvalidVerifiedLocalTerminal);
        }
        if self.bootstrap_batch_id
            == Some(BatchId::for_import(ImportId::from_digest(
                *self.bootstrap_import_id.as_bytes(),
            )))
        {
            return Err(EnrollmentError::InvalidVerifiedLocalTerminal);
        }
        Ok(())
    }

    fn verification_digest(&self) -> Result<ContentDigest, EnrollmentError> {
        self.validate_fields()?;
        let bytes =
            serde_json::to_vec(self).map_err(|error| EnrollmentError::Encode(error.to_string()))?;
        Ok(ContentDigest::of(&bytes))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum HandoffV1 {
    Safe,
    Unsafe { session_id: SessionId },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedRecoveryPacketV1 {
    packet_schema_version: u32,
    batch_id: BatchId,
    import_id: ImportId,
    manifest_digest: ContentDigest,
    archive_resource_id: CanonicalArchiveResourceId,
    published_from: AcceptedFrontierAnchorV1,
}

impl PublishedRecoveryPacketV1 {
    fn validate(&self) -> Result<(), EnrollmentError> {
        if self.packet_schema_version != PUBLISHED_RECOVERY_PACKET_SCHEMA_VERSION {
            return Err(EnrollmentError::UnsupportedPacketSchema(
                self.packet_schema_version,
            ));
        }
        if self.batch_id != BatchId::for_import(self.import_id) {
            return Err(EnrollmentError::PublishedBatchMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum LocalExclusionV1 {
    Idle,
    Published { packet: PublishedRecoveryPacketV1 },
}

/// The immutable bootstrap anchor every `LocalActive` record carries.
///
/// It is derived exactly once, at the sole `VerifiedLocal -> LocalActive`
/// transition, from the committed predecessor record and that record's own
/// content digest; every later `LocalActive -> LocalActive` handoff must repeat
/// it byte-for-byte. It therefore holds the complete durable data needed to
/// reconstruct and revalidate the original `VerifiedLocal`/bootstrap anchor in
/// O(1) from the head record alone.
///
/// It is authenticated by exactly the mechanism that authenticates the head:
/// the anchor lives inside `lifecycle`, which the hash-linked record digest, the
/// history accumulator, and the periodic authority-keyed checkpoint all commit
/// to. A fresh reopen therefore needs only the existing bounded checkpoint/open
/// proof — never a backward search for the `VerifiedLocal` record, whose
/// distance from the head grows without bound over a graph's lifetime.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalActiveAnchorV1 {
    verified_local_record_digest: ContentDigest,
    preparation_id: PreparationId,
    bootstrap_import_id: ContentDigest,
    bootstrap_part_count: u32,
    bootstrap_batch_id: Option<BatchId>,
    accepted_history_record_count: u64,
    accepted_frontier_anchor: AcceptedFrontierAnchorV1,
}

impl LocalActiveAnchorV1 {
    /// Derive the anchor from the actual committed `VerifiedLocal` predecessor
    /// and the exact content digest of the record that carries it.
    const fn from_verified_local(
        verified: &VerifiedLocalV1,
        verified_local_record_digest: ContentDigest,
    ) -> Self {
        Self {
            verified_local_record_digest,
            preparation_id: verified.preparation_id,
            bootstrap_import_id: verified.bootstrap_import_id,
            bootstrap_part_count: verified.bootstrap_part_count,
            bootstrap_batch_id: verified.bootstrap_batch_id,
            accepted_history_record_count: verified.accepted_history_record_count,
            accepted_frontier_anchor: verified.accepted_frontier_anchor,
        }
    }

    /// The zero/nonzero/multipart bootstrap identity rules
    /// [`VerifiedLocalV1::validate_fields`] enforces, restated over exactly the
    /// fields the anchor retains, so an anchor is rejected on its own terms
    /// without reading the record it names.
    fn validate(&self) -> Result<(), EnrollmentError> {
        let part_count = u64::from(self.bootstrap_part_count);
        let zero = self.bootstrap_part_count == 0;
        if self.bootstrap_batch_id.is_none() != zero
            || (self.accepted_frontier_anchor.history_root
                == super::object_store::empty_engine_history_root_digest())
                != zero
            || self.accepted_frontier_anchor.acceptance_sequence != part_count
            || self.accepted_frontier_anchor.history_generation != part_count
            || self.accepted_history_record_count != part_count
        {
            return Err(EnrollmentError::InvalidLocalActiveAnchor);
        }
        if self.bootstrap_batch_id
            == Some(BatchId::for_import(ImportId::from_digest(
                *self.bootstrap_import_id.as_bytes(),
            )))
        {
            return Err(EnrollmentError::InvalidLocalActiveAnchor);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalActiveV1 {
    verification_digest: ContentDigest,
    anchor: LocalActiveAnchorV1,
    handoff: HandoffV1,
    exclusion: LocalExclusionV1,
}

/// Exact bootstrap and projection/base facts that two honest local enrollments
/// must share before they can enter one shared lineage.  This deliberately
/// excludes device-local paths and resource identities: those are separately
/// bound by each enrollment record and cannot be compared across devices.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedProjectionBaseEvidenceV1 {
    bootstrap_import_id: ContentDigest,
    bootstrap_part_count: u32,
    bootstrap_terminal_part_id: Option<ContentDigest>,
    staged_file_count: u64,
    staged_total_bytes: u64,
}

impl SharedProjectionBaseEvidenceV1 {
    fn validate(&self) -> Result<(), EnrollmentError> {
        let zero = self.bootstrap_part_count == 0;
        if self.bootstrap_terminal_part_id.is_none() != zero {
            return Err(EnrollmentError::InvalidSharedProjectionBaseEvidence);
        }
        Ok(())
    }
}

/// The one portable, commit-last enrollment descriptor an initiator may hand
/// to a peer.  Its digest is its identity; there is intentionally no mutable
/// descriptor registry or descriptor discovery scan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SharedEnrollmentDescriptorV1 {
    schema_version: u32,
    compatibility: EnrollmentCompatibilityV1,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    catalog_document_id: DocumentId,
    initiator_graph_resource_id: CanonicalGraphResourceId,
    initiator_device_id: DeviceId,
    object_store_namespace: ContentDigest,
    initiator_local_active_head: ContentDigest,
    initiator_verification_digest: ContentDigest,
    initiator_handoff: HandoffV1,
    projection_base: SharedProjectionBaseEvidenceV1,
}

impl SharedEnrollmentDescriptorV1 {
    pub(crate) fn digest(&self) -> Result<ContentDigest, EnrollmentError> {
        self.validate()?;
        let bytes =
            serde_json::to_vec(self).map_err(|error| EnrollmentError::Encode(error.to_string()))?;
        Ok(ContentDigest::of(
            &[b"tine/shared-enrollment-descriptor/v1\0".as_slice(), &bytes].concat(),
        ))
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, EnrollmentError> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|error| EnrollmentError::Encode(error.to_string()))
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, EnrollmentError> {
        if bytes.len() > MAX_ENROLLMENT_RECORD_BYTES {
            return Err(EnrollmentError::RecordTooLarge(bytes.len()));
        }
        validate_json_bounds(bytes)?;
        reject_duplicate_json_fields(bytes)?;
        let descriptor: Self = serde_json::from_slice(bytes)
            .map_err(|error| EnrollmentError::Decode(error.to_string()))?;
        descriptor.validate()?;
        if descriptor.encode()? != bytes {
            return Err(EnrollmentError::NonCanonicalRecord);
        }
        Ok(descriptor)
    }

    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) const fn lineage_digest(&self) -> LineageDigest {
        self.lineage_digest
    }

    pub(crate) const fn catalog_document_id(&self) -> DocumentId {
        self.catalog_document_id
    }

    fn validate(&self) -> Result<(), EnrollmentError> {
        if self.schema_version != SHARED_ENROLLMENT_DESCRIPTOR_SCHEMA_VERSION {
            return Err(
                EnrollmentError::UnsupportedSharedEnrollmentDescriptorSchema(self.schema_version),
            );
        }
        self.compatibility.validate_current()?;
        if !matches!(self.initiator_handoff, HandoffV1::Safe) {
            return Err(EnrollmentError::UnsafeSharedEnrollmentHandoff);
        }
        self.projection_base.validate()
    }

    fn is_compatible_with(&self, binding: &EnrollmentBindingV1) -> bool {
        self.workspace_id == binding.workspace_id
            && self.lineage_digest == binding.lineage_digest
            && self.catalog_document_id == binding.catalog_document_id
            && self.compatibility == binding.compatibility
    }
}

/// Durable evidence that a joining device retired its former local workspace
/// only after proving it had no unique operation that was not projected.  The
/// archive bytes are retained by the caller's existing backup/archive path;
/// the journal stores the exact digest and pre-archive LocalActive witness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinerWorkspaceArchiveV1 {
    schema_version: u32,
    archived_workspace_digest: ContentDigest,
    source_local_active_head: ContentDigest,
    source_verification_digest: ContentDigest,
    unique_unprojected_operation_count: u64,
    projection_base: SharedProjectionBaseEvidenceV1,
}

impl JoinerWorkspaceArchiveV1 {
    fn validate(&self) -> Result<(), EnrollmentError> {
        if self.schema_version != JOINER_WORKSPACE_ARCHIVE_SCHEMA_VERSION {
            return Err(EnrollmentError::UnsupportedJoinerWorkspaceArchiveSchema(
                self.schema_version,
            ));
        }
        if self.unique_unprojected_operation_count != 0 {
            return Err(EnrollmentError::DirtyUniqueLocalTail);
        }
        self.projection_base.validate()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SharedEnrollmentRoleV1 {
    Initiator,
    Joiner,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharePreparedV1 {
    descriptor: SharedEnrollmentDescriptorV1,
    descriptor_digest: ContentDigest,
    local_active: LocalActiveV1,
}

impl SharePreparedV1 {
    fn validate(&self, binding: &EnrollmentBindingV1) -> Result<(), EnrollmentError> {
        self.descriptor.validate()?;
        if !self.descriptor.is_compatible_with(binding)
            || self.descriptor.initiator_graph_resource_id != binding.graph_resource_id
            || self.descriptor.initiator_device_id != binding.device_id
        {
            return Err(EnrollmentError::SharedEnrollmentBindingMismatch);
        }
        if self.descriptor.digest()? != self.descriptor_digest {
            return Err(EnrollmentError::SharedEnrollmentDescriptorDigestMismatch);
        }
        self.local_active.validate_for_shared_runtime(binding)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JoiningV1 {
    descriptor: SharedEnrollmentDescriptorV1,
    descriptor_digest: ContentDigest,
    archived_local_workspace: JoinerWorkspaceArchiveV1,
    local_active: LocalActiveV1,
}

impl JoiningV1 {
    fn validate(&self, binding: &EnrollmentBindingV1) -> Result<(), EnrollmentError> {
        self.descriptor.validate()?;
        self.archived_local_workspace.validate()?;
        if !self.descriptor.is_compatible_with(binding)
            || self.descriptor.digest()? != self.descriptor_digest
            || self.archived_local_workspace.projection_base != self.descriptor.projection_base
        {
            return Err(EnrollmentError::SharedEnrollmentBindingMismatch);
        }
        self.local_active.validate_for_shared_runtime(binding)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedActiveV1 {
    descriptor: SharedEnrollmentDescriptorV1,
    descriptor_digest: ContentDigest,
    role: SharedEnrollmentRoleV1,
    archived_local_workspace: Option<JoinerWorkspaceArchiveV1>,
    local_active: LocalActiveV1,
}

impl SharedActiveV1 {
    fn validate(&self, binding: &EnrollmentBindingV1) -> Result<(), EnrollmentError> {
        self.descriptor.validate()?;
        if self.descriptor.digest()? != self.descriptor_digest
            || !self.descriptor.is_compatible_with(binding)
        {
            return Err(EnrollmentError::SharedEnrollmentBindingMismatch);
        }
        self.local_active.validate_for_shared_runtime(binding)?;
        match (self.role, &self.archived_local_workspace) {
            (SharedEnrollmentRoleV1::Initiator, None) => Ok(()),
            (SharedEnrollmentRoleV1::Joiner, Some(archive)) => {
                archive.validate()?;
                if archive.projection_base != self.descriptor.projection_base {
                    return Err(EnrollmentError::SharedEnrollmentBindingMismatch);
                }
                Ok(())
            }
            _ => Err(EnrollmentError::IllegalLifecycle(
                "shared enrollment role and joiner archive evidence disagree",
            )),
        }
    }
}

impl LocalActiveV1 {
    fn validate_for_shared_runtime(
        &self,
        binding: &EnrollmentBindingV1,
    ) -> Result<(), EnrollmentError> {
        self.anchor.validate()?;
        if matches!(self.handoff, HandoffV1::Safe)
            && matches!(self.exclusion, LocalExclusionV1::Published { .. })
        {
            return Err(EnrollmentError::IllegalLifecycle(
                "a shared published exclusion cannot be marked handoff-safe",
            ));
        }
        if let LocalExclusionV1::Published { packet } = &self.exclusion {
            packet.validate()?;
            if packet.archive_resource_id != binding.archive_resource_id {
                return Err(EnrollmentError::BindingMismatch(
                    EnrollmentBindingField::ArchiveResource,
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockedV1 {
    prior_record_digest: ContentDigest,
    reason_code: String,
    evidence_digest: ContentDigest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum EnrollmentLifecycleV1 {
    ShadowImport(ShadowImportV1),
    VerifiedLocal(VerifiedLocalV1),
    LocalActive(LocalActiveV1),
    SharePrepared(SharePreparedV1),
    Joining(JoiningV1),
    SharedActive(SharedActiveV1),
    Blocked(BlockedV1),
}

impl EnrollmentLifecycleV1 {
    fn validate(
        &self,
        binding: &EnrollmentBindingV1,
        previous: Option<ContentDigest>,
    ) -> Result<(), EnrollmentError> {
        match self {
            Self::ShadowImport(_) => Ok(()),
            Self::VerifiedLocal(verified) => verified.validate_fields(),
            Self::LocalActive(active) => {
                active.anchor.validate()?;
                if matches!(active.handoff, HandoffV1::Safe)
                    && matches!(active.exclusion, LocalExclusionV1::Published { .. })
                {
                    return Err(EnrollmentError::IllegalLifecycle(
                        "a published exclusion cannot be marked handoff-safe",
                    ));
                }
                if let LocalExclusionV1::Published { packet } = &active.exclusion {
                    packet.validate()?;
                    if packet.archive_resource_id != binding.archive_resource_id {
                        return Err(EnrollmentError::BindingMismatch(
                            EnrollmentBindingField::ArchiveResource,
                        ));
                    }
                }
                Ok(())
            }
            Self::SharePrepared(prepared) => prepared.validate(binding),
            Self::Joining(joining) => joining.validate(binding),
            Self::SharedActive(active) => active.validate(binding),
            Self::Blocked(blocked) => {
                if Some(blocked.prior_record_digest) != previous {
                    return Err(EnrollmentError::IllegalLifecycle(
                        "blocked evidence does not identify the immediately prior record",
                    ));
                }
                validate_reason_code(&blocked.reason_code)
            }
        }
    }
}

fn validate_reason_code(reason: &str) -> Result<(), EnrollmentError> {
    if reason.is_empty()
        || reason.len() > MAX_BLOCKED_REASON_CODE_BYTES
        || reason
            .bytes()
            .any(|byte| !matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err(EnrollmentError::InvalidBlockedReason);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegrityCheckpointV3 {
    schema_version: u32,
    authority_id: Uuid,
    authority_resource_id: ContentDigest,
    integrity_tag: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum EnrollmentCheckpoint {
    Current(IntegrityCheckpointV3),
}

#[derive(Serialize)]
struct CheckpointMessageV3<'a> {
    domain: &'static str,
    record_schema_version: u32,
    checkpoint_schema_version: u32,
    authority_id: Uuid,
    authority_resource_id: ContentDigest,
    generation: u64,
    previous: Option<ContentDigest>,
    history_accumulator: ContentDigest,
    lease_resource_id: ContentDigest,
    binding: &'a EnrollmentBindingV1,
    lifecycle: &'a EnrollmentLifecycleV1,
}

fn current_checkpoint_message_bytes(
    authority_id: Uuid,
    authority_resource_id: ContentDigest,
    generation: u64,
    previous: Option<ContentDigest>,
    history_accumulator: ContentDigest,
    lease_resource_id: ContentDigest,
    binding: &EnrollmentBindingV1,
    lifecycle: &EnrollmentLifecycleV1,
) -> Result<Vec<u8>, EnrollmentError> {
    serde_json::to_vec(&CheckpointMessageV3 {
        domain: "tine/enrollment-checkpoint-integrity/v1",
        record_schema_version: ENROLLMENT_RECORD_SCHEMA_VERSION,
        checkpoint_schema_version: ENROLLMENT_CHECKPOINT_SCHEMA_VERSION,
        authority_id,
        authority_resource_id,
        generation,
        previous,
        history_accumulator,
        lease_resource_id,
        binding,
        lifecycle,
    })
    .map_err(|error| EnrollmentError::Encode(error.to_string()))
}

const fn generation_requires_checkpoint(generation: u64) -> bool {
    generation > 0 && (generation - 1).is_multiple_of(MAX_ENROLLMENT_OPEN_CHAIN_RECORDS as u64)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EnrollmentRecordV1 {
    schema_version: u32,
    generation: u64,
    previous: Option<ContentDigest>,
    history_accumulator: ContentDigest,
    lease_resource_id: ContentDigest,
    binding: EnrollmentBindingV1,
    lifecycle: EnrollmentLifecycleV1,
    checkpoint: Option<EnrollmentCheckpoint>,
}

impl EnrollmentRecordV1 {
    fn validate(&self) -> Result<(), EnrollmentError> {
        if self.schema_version != ENROLLMENT_RECORD_SCHEMA_VERSION {
            return Err(EnrollmentError::UnsupportedRecordSchema(
                self.schema_version,
            ));
        }
        if self.generation == 0 || (self.generation == 1) != self.previous.is_none() {
            return Err(EnrollmentError::NonmonotonicGeneration);
        }
        if self.checkpoint.is_some() != generation_requires_checkpoint(self.generation) {
            return Err(EnrollmentError::MissingAuthenticatedCheckpoint);
        }
        if let Some(checkpoint) = &self.checkpoint {
            match (self.schema_version, checkpoint) {
                (ENROLLMENT_RECORD_SCHEMA_VERSION, EnrollmentCheckpoint::Current(checkpoint))
                    if checkpoint.schema_version == ENROLLMENT_CHECKPOINT_SCHEMA_VERSION => {}
                (_, EnrollmentCheckpoint::Current(checkpoint))
                    if checkpoint.schema_version != ENROLLMENT_CHECKPOINT_SCHEMA_VERSION =>
                {
                    return Err(EnrollmentError::UnsupportedCheckpointSchema(
                        checkpoint.schema_version,
                    ));
                }
                _ => return Err(EnrollmentError::IllegalCheckpointPair),
            }
        }
        self.binding.validate_internal()?;
        self.lifecycle.validate(&self.binding, self.previous)
    }

    const fn lifecycle(&self) -> &EnrollmentLifecycleV1 {
        &self.lifecycle
    }
}

fn compute_history_accumulator(
    generation: u64,
    previous: Option<ContentDigest>,
    previous_accumulator: Option<ContentDigest>,
    binding: &EnrollmentBindingV1,
    lifecycle: &EnrollmentLifecycleV1,
) -> Result<ContentDigest, EnrollmentError> {
    let binding_bytes =
        serde_json::to_vec(binding).map_err(|error| EnrollmentError::Encode(error.to_string()))?;
    let lifecycle_bytes = serde_json::to_vec(lifecycle)
        .map_err(|error| EnrollmentError::Encode(error.to_string()))?;
    let mut hasher = Sha256::new();
    hasher.update(b"tine/enrollment-history-accumulator/v2\0");
    hasher.update(generation.to_be_bytes());
    match previous {
        Some(digest) => {
            hasher.update([1]);
            hasher.update(digest.as_bytes());
        }
        None => hasher.update([0]),
    }
    match previous_accumulator {
        Some(digest) => {
            hasher.update([1]);
            hasher.update(digest.as_bytes());
        }
        None => hasher.update([0]),
    }
    hasher.update((binding_bytes.len() as u64).to_be_bytes());
    hasher.update(binding_bytes);
    hasher.update((lifecycle_bytes.len() as u64).to_be_bytes());
    hasher.update(lifecycle_bytes);
    Ok(ContentDigest::from_bytes(hasher.finalize().into()))
}

fn validate_initial_record(record: &EnrollmentRecordV1) -> Result<(), EnrollmentError> {
    if record.generation != 1
        || record.previous.is_some()
        || !matches!(record.lifecycle, EnrollmentLifecycleV1::ShadowImport(_))
    {
        return Err(EnrollmentError::NonmonotonicGeneration);
    }
    let expected = compute_history_accumulator(1, None, None, &record.binding, &record.lifecycle)?;
    if record.history_accumulator != expected {
        return Err(EnrollmentError::HistoryAccumulatorMismatch);
    }
    Ok(())
}

fn validate_record_link(
    previous_digest: ContentDigest,
    previous: &EnrollmentRecordV1,
    current: &EnrollmentRecordV1,
) -> Result<(), EnrollmentError> {
    if current.previous != Some(previous_digest)
        || current.generation
            != previous
                .generation
                .checked_add(1)
                .ok_or(EnrollmentError::GenerationOverflow)?
    {
        return Err(EnrollmentError::NonmonotonicGeneration);
    }
    validate_transition(&previous.lifecycle, &current.lifecycle, previous_digest)?;
    let expected = compute_history_accumulator(
        current.generation,
        Some(previous_digest),
        Some(previous.history_accumulator),
        &current.binding,
        &current.lifecycle,
    )?;
    if current.history_accumulator != expected {
        return Err(EnrollmentError::HistoryAccumulatorMismatch);
    }
    Ok(())
}

fn validate_transition(
    current: &EnrollmentLifecycleV1,
    next: &EnrollmentLifecycleV1,
    current_digest: ContentDigest,
) -> Result<(), EnrollmentError> {
    let legal = match (current, next) {
        (
            EnrollmentLifecycleV1::ShadowImport(shadow),
            EnrollmentLifecycleV1::VerifiedLocal(verified),
        ) => {
            shadow.preparation_id == verified.preparation_id
                && shadow.source_inventory_digest == verified.source_inventory_digest
        }
        (EnrollmentLifecycleV1::ShadowImport(_), EnrollmentLifecycleV1::Blocked(blocked)) => {
            blocked.prior_record_digest == current_digest
        }
        (
            EnrollmentLifecycleV1::VerifiedLocal(verified),
            EnrollmentLifecycleV1::LocalActive(active),
        ) => {
            // The anchor is minted here and only here, from the actual
            // committed predecessor record and its exact content digest.
            verified
                .verification_digest()
                .is_ok_and(|digest| digest == active.verification_digest)
                && active.anchor
                    == LocalActiveAnchorV1::from_verified_local(verified, current_digest)
                && matches!(active.handoff, HandoffV1::Unsafe { .. })
                && matches!(active.exclusion, LocalExclusionV1::Idle)
        }
        (EnrollmentLifecycleV1::VerifiedLocal(_), EnrollmentLifecycleV1::Blocked(blocked)) => {
            blocked.prior_record_digest == current_digest
        }
        (EnrollmentLifecycleV1::LocalActive(current), EnrollmentLifecycleV1::LocalActive(next)) => {
            // The anchor is immutable for the whole `LocalActive` lifetime:
            // every handoff, session change, exclusion change and checkpoint
            // must repeat it exactly.
            current.verification_digest == next.verification_digest
                && current.anchor == next.anchor
                && legal_local_active_transition(current, next)
        }
        (EnrollmentLifecycleV1::LocalActive(_), EnrollmentLifecycleV1::Blocked(blocked)) => {
            blocked.prior_record_digest == current_digest
        }
        (
            EnrollmentLifecycleV1::LocalActive(current),
            EnrollmentLifecycleV1::SharePrepared(prepared),
        ) => {
            matches!(current.handoff, HandoffV1::Safe)
                && matches!(current.exclusion, LocalExclusionV1::Idle)
                && prepared.descriptor.initiator_verification_digest == current.verification_digest
                && prepared.local_active == *current
        }
        (EnrollmentLifecycleV1::LocalActive(current), EnrollmentLifecycleV1::Joining(joining)) => {
            matches!(current.handoff, HandoffV1::Safe)
                && matches!(current.exclusion, LocalExclusionV1::Idle)
                && joining.archived_local_workspace.source_local_active_head == current_digest
                && joining.archived_local_workspace.source_verification_digest
                    == current.verification_digest
                && joining.local_active == *current
        }
        (
            EnrollmentLifecycleV1::SharePrepared(prepared),
            EnrollmentLifecycleV1::SharedActive(active),
        ) => {
            active.role == SharedEnrollmentRoleV1::Initiator
                && active.archived_local_workspace.is_none()
                && active.descriptor == prepared.descriptor
                && active.descriptor_digest == prepared.descriptor_digest
                && active.local_active == prepared.local_active
        }
        (
            EnrollmentLifecycleV1::SharePrepared(current),
            EnrollmentLifecycleV1::SharePrepared(next),
        ) => {
            current.descriptor == next.descriptor
                && current.descriptor_digest == next.descriptor_digest
                && current.local_active.verification_digest == next.local_active.verification_digest
                && current.local_active.anchor == next.local_active.anchor
                && legal_local_active_transition(&current.local_active, &next.local_active)
        }
        (EnrollmentLifecycleV1::SharePrepared(_), EnrollmentLifecycleV1::Blocked(blocked))
        | (EnrollmentLifecycleV1::Joining(_), EnrollmentLifecycleV1::Blocked(blocked))
        | (EnrollmentLifecycleV1::SharedActive(_), EnrollmentLifecycleV1::Blocked(blocked)) => {
            blocked.prior_record_digest == current_digest
        }
        (EnrollmentLifecycleV1::Joining(joining), EnrollmentLifecycleV1::SharedActive(active)) => {
            active.role == SharedEnrollmentRoleV1::Joiner
                && active.descriptor == joining.descriptor
                && active.descriptor_digest == joining.descriptor_digest
                && active.archived_local_workspace.as_ref()
                    == Some(&joining.archived_local_workspace)
                && active.local_active == joining.local_active
        }
        (EnrollmentLifecycleV1::Joining(current), EnrollmentLifecycleV1::Joining(next)) => {
            current.descriptor == next.descriptor
                && current.descriptor_digest == next.descriptor_digest
                && current.archived_local_workspace == next.archived_local_workspace
                && current.local_active.verification_digest == next.local_active.verification_digest
                && current.local_active.anchor == next.local_active.anchor
                && legal_local_active_transition(&current.local_active, &next.local_active)
        }
        (
            EnrollmentLifecycleV1::SharedActive(current),
            EnrollmentLifecycleV1::SharedActive(next),
        ) => {
            current.descriptor == next.descriptor
                && current.descriptor_digest == next.descriptor_digest
                && current.role == next.role
                && current.archived_local_workspace == next.archived_local_workspace
                && current.local_active.verification_digest == next.local_active.verification_digest
                && current.local_active.anchor == next.local_active.anchor
                && legal_local_active_transition(&current.local_active, &next.local_active)
        }
        _ => false,
    };
    if !legal {
        return Err(EnrollmentError::IllegalTransition);
    }
    Ok(())
}

fn legal_local_active_transition(current: &LocalActiveV1, next: &LocalActiveV1) -> bool {
    match (
        current.handoff,
        &current.exclusion,
        next.handoff,
        &next.exclusion,
    ) {
        (
            HandoffV1::Safe,
            LocalExclusionV1::Idle,
            HandoffV1::Unsafe { .. },
            LocalExclusionV1::Idle,
        )
        | (
            HandoffV1::Unsafe { .. },
            LocalExclusionV1::Idle,
            HandoffV1::Safe,
            LocalExclusionV1::Idle,
        )
        | (
            HandoffV1::Unsafe { .. },
            LocalExclusionV1::Published { .. },
            HandoffV1::Unsafe { .. },
            LocalExclusionV1::Idle,
        ) => true,
        (
            HandoffV1::Unsafe {
                session_id: current,
            },
            LocalExclusionV1::Idle,
            HandoffV1::Unsafe { session_id: next },
            LocalExclusionV1::Idle,
        ) => current != next,
        (
            HandoffV1::Unsafe {
                session_id: current,
            },
            LocalExclusionV1::Idle,
            HandoffV1::Unsafe { session_id: next },
            LocalExclusionV1::Published { .. },
        ) => current == next,
        _ => false,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EnrollmentSnapshot {
    digest: ContentDigest,
    record: EnrollmentRecordV1,
}

/// Bounded, inert evidence from one authenticated enrollment head.
///
/// This value deliberately contains no directory capability, authority key,
/// lease handle, reader, writer, or transition method. A later runtime open
/// must independently reopen and authenticate the enrollment and acquire its
/// writer lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EnrollmentDiscoveryEvidence {
    pub(crate) binding: EnrollmentBindingV1,
    pub(crate) head_digest: ContentDigest,
    pub(crate) generation: u64,
    pub(crate) lifecycle: EnrollmentDiscoveryLifecycle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EnrollmentDiscoveryLifecycle {
    ShadowImport,
    VerifiedLocal,
    LocalActive(EnrollmentDiscoveryLocalActive),
    Blocked {
        reason_code: String,
        evidence_digest: ContentDigest,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EnrollmentDiscoveryLocalActive {
    pub(crate) verification_digest: ContentDigest,
    pub(crate) bootstrap_import_id: ContentDigest,
    pub(crate) bootstrap_part_count: u32,
    pub(crate) anchor_history_generation: u64,
    pub(crate) anchor_history_index_root: ContentDigest,
    pub(crate) anchor_acceptance_sequence: u64,
    pub(crate) anchor_accepted_frontier_state_digest: ContentDigest,
    pub(crate) handoff: EnrollmentDiscoveryHandoff,
    pub(crate) exclusion: EnrollmentDiscoveryExclusion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnrollmentDiscoveryHandoff {
    Safe,
    Unsafe { session_id: SessionId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnrollmentDiscoveryExclusion {
    Idle,
    Published,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EnrollmentDiscoveryInspection {
    Absent,
    Residue,
    Present(EnrollmentDiscoveryEvidence),
}

/// Inspect one explicit device-local application root without creating or
/// repairing any enrollment state.
///
/// The path is opened as an existing no-follow directory. A present head is
/// decoded through the canonical authority-claim and record decoders and
/// authenticated through the same bounded checkpoint/chain validation every
/// other reader here uses — [`open_discovered_enrollment_authority`] followed by
/// [`read_head_and_chain`]. The lease file is read only for its physical
/// identity and is never locked.
pub(crate) fn inspect_existing_enrollment_at(
    root_path: &Path,
    expected_graph_resource: CanonicalGraphResourceId,
) -> Result<EnrollmentDiscoveryInspection, EnrollmentError> {
    let Some(root) = open_existing_application_root(root_path)? else {
        return Ok(EnrollmentDiscoveryInspection::Absent);
    };
    let Some(directories) = open_directories(&root, expected_graph_resource)? else {
        return Ok(EnrollmentDiscoveryInspection::Absent);
    };
    validate_namespaces(&directories)?;
    let lease_resource_id = inspect_lease_resource_id(&directories)?;
    if read_head(&directories.enrollment)?.is_none() {
        return Ok(EnrollmentDiscoveryInspection::Residue);
    }
    let authority = open_discovered_enrollment_authority(
        &directories,
        expected_graph_resource,
        lease_resource_id,
    )?;
    let binding = authority.material.claim.binding().clone();
    let current = read_head_and_chain(
        &directories,
        &binding,
        lease_resource_id,
        &authority.material,
    )?
    .ok_or(EnrollmentError::MalformedHead)?;
    let lifecycle = match current.record.lifecycle() {
        EnrollmentLifecycleV1::ShadowImport(_) => EnrollmentDiscoveryLifecycle::ShadowImport,
        EnrollmentLifecycleV1::VerifiedLocal(_) => EnrollmentDiscoveryLifecycle::VerifiedLocal,
        EnrollmentLifecycleV1::LocalActive(active)
        | EnrollmentLifecycleV1::SharedActive(SharedActiveV1 {
            local_active: active,
            ..
        }) => EnrollmentDiscoveryLifecycle::LocalActive(EnrollmentDiscoveryLocalActive {
            verification_digest: active.verification_digest,
            bootstrap_import_id: active.anchor.bootstrap_import_id,
            bootstrap_part_count: active.anchor.bootstrap_part_count,
            anchor_history_generation: active.anchor.accepted_frontier_anchor.history_generation,
            anchor_history_index_root: active.anchor.accepted_frontier_anchor.history_root,
            anchor_acceptance_sequence: active.anchor.accepted_frontier_anchor.acceptance_sequence,
            anchor_accepted_frontier_state_digest: active
                .anchor
                .accepted_frontier_anchor
                .accepted_frontier_state_digest,
            handoff: match active.handoff {
                HandoffV1::Safe => EnrollmentDiscoveryHandoff::Safe,
                HandoffV1::Unsafe { session_id } => {
                    EnrollmentDiscoveryHandoff::Unsafe { session_id }
                }
            },
            exclusion: match active.exclusion {
                LocalExclusionV1::Idle => EnrollmentDiscoveryExclusion::Idle,
                LocalExclusionV1::Published { .. } => EnrollmentDiscoveryExclusion::Published,
            },
        }),
        EnrollmentLifecycleV1::SharePrepared(SharePreparedV1 {
            local_active: active,
            ..
        })
        | EnrollmentLifecycleV1::Joining(JoiningV1 {
            local_active: active,
            ..
        }) => EnrollmentDiscoveryLifecycle::LocalActive(EnrollmentDiscoveryLocalActive {
            verification_digest: active.verification_digest,
            bootstrap_import_id: active.anchor.bootstrap_import_id,
            bootstrap_part_count: active.anchor.bootstrap_part_count,
            anchor_history_generation: active.anchor.accepted_frontier_anchor.history_generation,
            anchor_history_index_root: active.anchor.accepted_frontier_anchor.history_root,
            anchor_acceptance_sequence: active.anchor.accepted_frontier_anchor.acceptance_sequence,
            anchor_accepted_frontier_state_digest: active
                .anchor
                .accepted_frontier_anchor
                .accepted_frontier_state_digest,
            handoff: match active.handoff {
                HandoffV1::Safe => EnrollmentDiscoveryHandoff::Safe,
                HandoffV1::Unsafe { session_id } => {
                    EnrollmentDiscoveryHandoff::Unsafe { session_id }
                }
            },
            exclusion: match active.exclusion {
                LocalExclusionV1::Idle => EnrollmentDiscoveryExclusion::Idle,
                LocalExclusionV1::Published { .. } => EnrollmentDiscoveryExclusion::Published,
            },
        }),
        EnrollmentLifecycleV1::Blocked(blocked) => EnrollmentDiscoveryLifecycle::Blocked {
            reason_code: blocked.reason_code.clone(),
            evidence_digest: blocked.evidence_digest,
        },
    };
    Ok(EnrollmentDiscoveryInspection::Present(
        EnrollmentDiscoveryEvidence {
            binding,
            head_digest: current.digest,
            generation: current.record.generation,
            lifecycle,
        },
    ))
}

fn open_existing_application_root(
    path: &Path,
) -> Result<Option<EnrollmentApplicationRoot>, EnrollmentError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(EnrollmentError::UnsafeNamespace(
            "private enrollment application root is not a real no-follow directory".into(),
        ));
    }
    let name = path.file_name().ok_or_else(|| {
        EnrollmentError::UnsafeNamespace(
            "private enrollment application root has no final component".into(),
        )
    })?;
    if !matches!(
        path.components().next_back(),
        Some(std::path::Component::Normal(_))
    ) {
        return Err(EnrollmentError::UnsafeNamespace(
            "private enrollment application root must end in a normal path component".into(),
        ));
    }
    let name = name.to_str().ok_or_else(|| {
        EnrollmentError::UnsafeNamespace(
            "private enrollment application root final component is not UTF-8".into(),
        )
    })?;
    let parent = path.parent().ok_or_else(|| {
        EnrollmentError::UnsafeNamespace(
            "private enrollment application root has no existing parent".into(),
        )
    })?;
    let canonical_parent = fs::canonicalize(parent)?;
    let parent = Dir::open_ambient_dir(&canonical_parent, ambient_authority())?;
    let Some(directory) = open_component(&parent, name)? else {
        return Ok(None);
    };
    validate_private_directory(&directory, "private enrollment application root")?;
    Ok(Some(EnrollmentApplicationRoot {
        path: canonical_parent.join(name),
    }))
}

#[derive(Debug)]
struct EnrollmentDirectories {
    enrollment: Dir,
    records: Dir,
}

#[derive(Debug)]
pub(crate) enum VerifiedLocalCompositionError {
    Enrollment(EnrollmentError),
    Bootstrap(BootstrapStreamingImportError),
    Sqlite(ProjectionError),
}

impl fmt::Display for VerifiedLocalCompositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enrollment(error) => error.fmt(formatter),
            Self::Bootstrap(error) => error.fmt(formatter),
            Self::Sqlite(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for VerifiedLocalCompositionError {}

impl From<EnrollmentError> for VerifiedLocalCompositionError {
    fn from(error: EnrollmentError) -> Self {
        Self::Enrollment(error)
    }
}

impl From<BootstrapStreamingImportError> for VerifiedLocalCompositionError {
    fn from(error: BootstrapStreamingImportError) -> Self {
        Self::Bootstrap(error)
    }
}

impl From<ProjectionError> for VerifiedLocalCompositionError {
    fn from(error: ProjectionError) -> Self {
        Self::Sqlite(error)
    }
}

impl From<std::io::Error> for VerifiedLocalCompositionError {
    fn from(error: std::io::Error) -> Self {
        Self::Enrollment(EnrollmentError::from(error))
    }
}

/// Durable handoff state of a committed `LocalActive` enrollment.
///
/// `Unsafe` is the only state a mutation may be admitted under, and it always
/// names the exact session that owns graph text. A crash therefore always
/// resumes conservatively unsafe.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentRecordWire {
    schema_version: u32,
    generation: u64,
    previous: Option<ContentDigest>,
    history_accumulator: ContentDigest,
    lease_resource_id: ContentDigest,
    binding: EnrollmentBindingV1,
    lifecycle: EnrollmentLifecycleV1,
    checkpoint: Option<IntegrityCheckpointV3>,
}

// The normalized model is deliberately not deserializable. Test mutation
// helpers still need an unchecked canonical-shaped serializer to manufacture
// malformed bytes for the decoder's negative cases.
impl Serialize for EnrollmentRecordV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.schema_version {
            ENROLLMENT_RECORD_SCHEMA_VERSION => EnrollmentRecordWire {
                schema_version: self.schema_version,
                generation: self.generation,
                previous: self.previous,
                history_accumulator: self.history_accumulator,
                lease_resource_id: self.lease_resource_id,
                binding: self.binding.clone(),
                lifecycle: self.lifecycle.clone(),
                checkpoint: match &self.checkpoint {
                    Some(EnrollmentCheckpoint::Current(checkpoint)) => Some(checkpoint.clone()),
                    None => None,
                },
            }
            .serialize(serializer),
            schema => Err(serde::ser::Error::custom(format!(
                "unsupported enrollment schema {schema}"
            ))),
        }
    }
}

fn canonical_record_bytes(record: &EnrollmentRecordV1) -> Result<Vec<u8>, EnrollmentError> {
    record.validate()?;
    let bytes = match record.schema_version {
        ENROLLMENT_RECORD_SCHEMA_VERSION => {
            let checkpoint = match &record.checkpoint {
                Some(EnrollmentCheckpoint::Current(checkpoint)) => Some(checkpoint.clone()),
                None => None,
            };
            serde_json::to_vec(&EnrollmentRecordWire {
                schema_version: record.schema_version,
                generation: record.generation,
                previous: record.previous,
                history_accumulator: record.history_accumulator,
                lease_resource_id: record.lease_resource_id,
                binding: record.binding.clone(),
                lifecycle: record.lifecycle.clone(),
                checkpoint,
            })
        }
        schema => return Err(EnrollmentError::UnsupportedRecordSchema(schema)),
    }
    .map_err(|error| EnrollmentError::Encode(error.to_string()))?;
    if bytes.len() > MAX_ENROLLMENT_RECORD_BYTES {
        return Err(EnrollmentError::RecordTooLarge(bytes.len()));
    }
    Ok(bytes)
}

fn decode_record(bytes: &[u8]) -> Result<EnrollmentRecordV1, EnrollmentError> {
    validate_json_bounds(bytes)?;
    reject_duplicate_json_fields(bytes)?;
    let probe: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| EnrollmentError::Decode(error.to_string()))?;
    let schema = probe
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| EnrollmentError::Decode("record schema_version is missing".into()))?;
    let schema = u32::try_from(schema).unwrap_or(u32::MAX);
    let lifecycle_state = probe
        .get("lifecycle")
        .and_then(|value| value.get("state"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| EnrollmentError::Decode("record lifecycle state is missing".into()))?;
    if !matches!(
        lifecycle_state,
        "shadow_import"
            | "verified_local"
            | "local_active"
            | "share_prepared"
            | "joining"
            | "shared_active"
            | "blocked"
    ) {
        return Err(EnrollmentError::FutureUnsupportedLifecycle(
            lifecycle_state.to_owned(),
        ));
    }
    let record = match schema {
        ENROLLMENT_RECORD_SCHEMA_VERSION => {
            let record: EnrollmentRecordWire = serde_json::from_slice(bytes)
                .map_err(|error| EnrollmentError::Decode(error.to_string()))?;
            EnrollmentRecordV1 {
                schema_version: record.schema_version,
                generation: record.generation,
                previous: record.previous,
                history_accumulator: record.history_accumulator,
                lease_resource_id: record.lease_resource_id,
                binding: record.binding,
                lifecycle: record.lifecycle,
                checkpoint: record.checkpoint.map(EnrollmentCheckpoint::Current),
            }
        }
        schema => return Err(EnrollmentError::UnsupportedRecordSchema(schema)),
    };
    record.validate()?;
    if canonical_record_bytes(&record)? != bytes {
        return Err(EnrollmentError::NonCanonicalRecord);
    }
    Ok(record)
}

#[derive(Clone, Copy)]
struct RejectDuplicateJsonFields;

impl<'de> DeserializeSeed<'de> for RejectDuplicateJsonFields {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for RejectDuplicateJsonFields {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded JSON without duplicate object fields")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON field {key:?}"
                )));
            }
            map.next_value_seed(self)?;
        }
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element_seed(self)?.is_some() {}
        Ok(())
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }
}

fn reject_duplicate_json_fields(bytes: &[u8]) -> Result<(), EnrollmentError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    RejectDuplicateJsonFields
        .deserialize(&mut deserializer)
        .map_err(|error| EnrollmentError::Decode(error.to_string()))?;
    deserializer
        .end()
        .map_err(|error| EnrollmentError::Decode(error.to_string()))
}

fn validate_json_bounds(bytes: &[u8]) -> Result<(), EnrollmentError> {
    if bytes.len() > MAX_ENROLLMENT_RECORD_BYTES {
        return Err(EnrollmentError::RecordTooLarge(bytes.len()));
    }
    if std::str::from_utf8(bytes).is_err() {
        return Err(EnrollmentError::Decode("record is not UTF-8".into()));
    }
    let mut in_string = false;
    let mut escaped = false;
    let mut depth = 0usize;
    let mut tokens = 0usize;
    for byte in bytes.iter().copied() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth = depth.saturating_add(1);
                tokens = tokens.saturating_add(1);
                if depth > MAX_ENROLLMENT_JSON_DEPTH {
                    return Err(EnrollmentError::JsonDepthExceeded);
                }
            }
            b'}' | b']' => {
                if depth == 0 {
                    return Err(EnrollmentError::Decode(
                        "record has unbalanced JSON delimiters".into(),
                    ));
                }
                depth -= 1;
                tokens = tokens.saturating_add(1);
            }
            b',' | b':' => tokens = tokens.saturating_add(1),
            _ => {}
        }
        if tokens > MAX_ENROLLMENT_JSON_TOKENS {
            return Err(EnrollmentError::JsonTokenBoundExceeded);
        }
    }
    if in_string || escaped || depth != 0 {
        return Err(EnrollmentError::Decode(
            "record has unterminated JSON structure".into(),
        ));
    }
    Ok(())
}

fn read_head_and_chain(
    directories: &EnrollmentDirectories,
    expected_binding: &EnrollmentBindingV1,
    expected_lease_resource_id: ContentDigest,
    authority: &EnrollmentAuthorityMaterial,
) -> Result<Option<EnrollmentSnapshot>, EnrollmentError> {
    let Some(head) = read_head(&directories.enrollment)? else {
        return Ok(None);
    };
    let current = read_record(&directories.records, head)?;
    validate_record_authority(
        &current,
        expected_binding,
        expected_lease_resource_id,
        authority,
    )?;

    let mut seen = BTreeSet::new();
    let mut digest = head;
    let mut record = current.clone();
    for count in 0..MAX_ENROLLMENT_OPEN_CHAIN_RECORDS {
        if !seen.insert(digest) {
            return Err(EnrollmentError::ChainCycle);
        }
        if record.checkpoint.is_some() {
            authority.verify_checkpoint(&record)?;
            if record.previous.is_none() {
                validate_initial_record(&record)?;
            }
            return Ok(Some(EnrollmentSnapshot {
                digest: head,
                record: current,
            }));
        }
        match record.previous {
            None => return Err(EnrollmentError::MissingAuthenticatedCheckpoint),
            Some(previous_digest) => {
                let previous = read_record(&directories.records, previous_digest)?;
                validate_record_authority(
                    &previous,
                    expected_binding,
                    expected_lease_resource_id,
                    authority,
                )?;
                validate_record_link(previous_digest, &previous, &record)?;
                digest = previous_digest;
                record = previous;
            }
        }
        if count + 1 == MAX_ENROLLMENT_OPEN_CHAIN_RECORDS {
            return Err(EnrollmentError::MissingAuthenticatedCheckpoint);
        }
    }
    unreachable!("bounded chain loop returns at its limit")
}

fn validate_record_authority(
    record: &EnrollmentRecordV1,
    expected_binding: &EnrollmentBindingV1,
    expected_lease_resource_id: ContentDigest,
    authority: &EnrollmentAuthorityMaterial,
) -> Result<(), EnrollmentError> {
    record.binding.validate_exact(expected_binding)?;
    record.validate()?;
    if record.lease_resource_id != expected_lease_resource_id {
        return Err(EnrollmentError::LeaseResourceMismatch);
    }
    if record.checkpoint.is_some() {
        authority.verify_checkpoint(record)?;
    }
    Ok(())
}

fn read_head(directory: &Dir) -> Result<Option<ContentDigest>, EnrollmentError> {
    #[cfg(test)]
    count(&ENROLLMENT_HEAD_READS);
    #[cfg(test)]
    if FAIL_NEXT_ENROLLMENT_HEAD_READ.with(|fault| fault.replace(false)) {
        return Err(EnrollmentError::Io(
            "injected transient enrollment head read failure".into(),
        ));
    }
    let metadata = match directory.symlink_metadata(HEAD_FILE) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !cap_metadata_is_authoritative_file(&metadata) {
        return Err(EnrollmentError::UnsafeNamespace(
            "enrollment head is not a regular no-follow file".into(),
        ));
    }
    if metadata.len() != HEAD_BYTES as u64 {
        return Err(EnrollmentError::MalformedHead);
    }
    let file = open_regular_readonly(directory, HEAD_FILE)?;
    validate_authoritative_file(&file, "enrollment head")?;
    if file.metadata()?.len() != HEAD_BYTES as u64 {
        return Err(EnrollmentError::MalformedHead);
    }
    let mut bytes = Vec::with_capacity(HEAD_BYTES);
    file.take((HEAD_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() != HEAD_BYTES || bytes[64] != b'\n' {
        return Err(EnrollmentError::MalformedHead);
    }
    let text = std::str::from_utf8(&bytes[..64]).map_err(|_| EnrollmentError::MalformedHead)?;
    let digest = parse_digest(text).map_err(|_| EnrollmentError::MalformedHead)?;
    Ok(Some(ContentDigest::from_bytes(digest)))
}

fn read_record(
    records: &Dir,
    expected_digest: ContentDigest,
) -> Result<EnrollmentRecordV1, EnrollmentError> {
    #[cfg(test)]
    ENROLLMENT_RECORD_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
    let name = format!("{expected_digest}{RECORD_SUFFIX}");
    let metadata = match records.symlink_metadata(&name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(EnrollmentError::MissingChainRecord(expected_digest));
        }
        Err(error) => return Err(error.into()),
    };
    if !cap_metadata_is_authoritative_file(&metadata) {
        return Err(EnrollmentError::UnsafeNamespace(format!(
            "enrollment record {name} is not a regular no-follow file"
        )));
    }
    if metadata.len() > MAX_ENROLLMENT_RECORD_BYTES as u64 {
        return Err(EnrollmentError::RecordTooLarge(
            usize::try_from(metadata.len()).unwrap_or(usize::MAX),
        ));
    }
    let file = open_regular_readonly(records, &name)?;
    validate_authoritative_file(&file, "enrollment record")?;
    let opened_len = file.metadata()?.len();
    if opened_len > MAX_ENROLLMENT_RECORD_BYTES as u64 {
        return Err(EnrollmentError::RecordTooLarge(
            usize::try_from(opened_len).unwrap_or(usize::MAX),
        ));
    }
    let mut bytes = Vec::with_capacity(opened_len as usize);
    file.take((MAX_ENROLLMENT_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if ContentDigest::of(&bytes) != expected_digest {
        return Err(EnrollmentError::RecordDigestMismatch(expected_digest));
    }
    decode_record(&bytes)
}

fn validate_namespaces(directories: &EnrollmentDirectories) -> Result<(), EnrollmentError> {
    #[cfg(test)]
    count(&ENROLLMENT_NAMESPACE_SCANS);
    validate_private_directory(&directories.enrollment, "enrollment directory")?;
    validate_private_directory(&directories.records, "enrollment records directory")?;

    let mut count = 0usize;
    for entry in directories.enrollment.entries()? {
        let entry = entry?;
        count += 1;
        if count > MAX_ENROLLMENT_NAMESPACE_ENTRIES {
            return Err(EnrollmentError::NamespaceBoundExceeded);
        }
        let name = entry.file_name().into_string().map_err(|_| {
            EnrollmentError::UnsupportedArtifact("non-UTF-8 enrollment artifact".into())
        })?;
        let accepted = match name.as_str() {
            RECORDS_DIRECTORY => entry.file_type()?.is_dir(),
            HEAD_FILE | LEASE_FILE | AUTHORITY_FILE => regular_entry(&entry)?,
            _ if name.starts_with(HEAD_TEMP_PREFIX) => regular_entry(&entry)?,
            _ if name.starts_with(AUTHORITY_TEMP_PREFIX) => {
                if !regular_entry(&entry)? {
                    return Err(EnrollmentError::AmbiguousAuthorityProvisioning);
                }
                true
            }
            _ => false,
        };
        if !accepted {
            return Err(EnrollmentError::UnsupportedArtifact(name));
        }
    }

    // Immutable record history has no lifetime cardinality bound. Authoritative
    // records are classified and authenticated only when addressed by a head
    // or opaque audit cursor; unrelated artifacts are retained but inert.
    Ok(())
}

fn regular_entry(entry: &cap_std::fs::DirEntry) -> Result<bool, EnrollmentError> {
    Ok(cap_metadata_is_authoritative_file(&entry.metadata()?))
}

/// Open the existing enrollment namespace, or report its absence.
///
/// There is deliberately no `create` here: this module is read-only
/// classification, and a creation flag is how that stops being true one caller
/// at a time. `tests::the_enrollment_namespace_has_no_creation_authority` is the
/// architectural fact.
fn open_directories(
    root: &EnrollmentApplicationRoot,
    graph_resource: CanonicalGraphResourceId,
) -> Result<Option<EnrollmentDirectories>, EnrollmentError> {
    #[cfg(test)]
    count(&ENROLLMENT_DIRECTORY_OPENS);
    let root_dir = Dir::open_ambient_dir(root.path(), ambient_authority())?;
    let sparse = open_component(&root_dir, SPARSE_STORAGE_DIRECTORY)?;
    let Some(sparse) = sparse else {
        return Ok(None);
    };
    let version = open_component(&sparse, STORAGE_VERSION_DIRECTORY)?;
    let Some(version) = version else {
        return Ok(None);
    };
    let local = open_component(&version, LOCAL_DIRECTORY)?;
    let Some(local) = local else {
        return Ok(None);
    };
    let graph_name = graph_resource.to_string();
    let graph = open_component(&local, &graph_name)?;
    let Some(graph) = graph else {
        return Ok(None);
    };
    let enrollment = open_component(&graph, ENROLLMENT_DIRECTORY)?;
    let Some(enrollment) = enrollment else {
        return Ok(None);
    };
    let records = open_component(&enrollment, RECORDS_DIRECTORY)?;
    let Some(records) = records else {
        return Err(EnrollmentError::UnsafeNamespace(
            "enrollment exists without its records directory".into(),
        ));
    };
    Ok(Some(EnrollmentDirectories {
        enrollment,
        records,
    }))
}

/// Open one existing component of the enrollment namespace, no-follow.
///
/// Absence is `Ok(None)`, never a repair. This function used to take
/// `create: bool` and, when true, `mkdir` + `fchmod(0o700)` + fsync the parent —
/// live writer authority behind a plain flag, in a module whose header says none
/// remains. Every live caller passed `false`; the branch is gone rather than
/// dormant, because a dormant one is an invitation.
fn open_component(parent: &Dir, name: &str) -> Result<Option<Dir>, EnrollmentError> {
    match parent.symlink_metadata(name) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || cap_metadata_is_windows_reparse(&metadata) =>
        {
            return Err(EnrollmentError::UnsafeNamespace(format!(
                "{name} is not a real no-follow directory"
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let directory = open_dir_nofollow(parent, name)
        .map_err(|error| EnrollmentError::UnsafeNamespace(error.to_string()))?;
    validate_private_directory(&directory, name)?;
    Ok(Some(directory))
}

fn validate_private_directory(directory: &Dir, name: &str) -> Result<(), EnrollmentError> {
    let metadata = directory.try_clone()?.into_std_file().metadata()?;
    if !metadata.is_dir() {
        return Err(EnrollmentError::UnsafeNamespace(format!(
            "{name} is not an opened directory"
        )));
    }
    Ok(())
}

fn inspect_lease_resource_id(
    directories: &EnrollmentDirectories,
) -> Result<ContentDigest, EnrollmentError> {
    match directories.enrollment.symlink_metadata(LEASE_FILE) {
        Ok(metadata) if !cap_metadata_is_authoritative_file(&metadata) => {
            return Err(EnrollmentError::UnsafeNamespace(
                "enrollment lease is not a regular no-follow file".into(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(EnrollmentError::UnsafeNamespace(
                "existing enrollment has no lease authority".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    let file = open_regular_readonly(&directories.enrollment, LEASE_FILE)?;
    validate_authoritative_file(&file, "enrollment lease")?;
    Ok(lease_resource_id(&authoritative_file_identity(&file)?))
}

fn open_discovered_enrollment_authority(
    directories: &EnrollmentDirectories,
    expected_graph_resource: CanonicalGraphResourceId,
    lease_resource_id: ContentDigest,
) -> Result<EnrollmentAuthority, EnrollmentError> {
    #[cfg(test)]
    count(&ENROLLMENT_AUTHORITY_CLAIM_READS);
    let (bytes, identity) = read_bounded_authoritative_file(
        &directories.enrollment,
        AUTHORITY_FILE,
        MAX_ENROLLMENT_AUTHORITY_BYTES,
        "enrollment authority claim",
        false,
    )?;
    let file = open_regular_readonly(&directories.enrollment, AUTHORITY_FILE)?;
    validate_authoritative_file(&file, "enrollment authority claim")?;
    if authoritative_file_identity(&file)? != identity {
        return Err(EnrollmentError::AuthorityMismatch);
    }
    let claim = decode_authority_claim(&bytes)?;
    if claim.binding().graph_resource_id != expected_graph_resource {
        return Err(EnrollmentError::BindingMismatch(
            EnrollmentBindingField::GraphResource,
        ));
    }
    let binding = claim.binding().clone();
    let material = EnrollmentAuthorityMaterial::from_claim(
        claim,
        authority_resource_id(&identity),
        &binding,
        lease_resource_id,
    )?;
    let authority = EnrollmentAuthority {
        material,
        file,
        directory: directories.enrollment.try_clone()?,
        identity,
    };
    authority.validate_current()?;
    Ok(authority)
}

fn canonical_authority_claim_bytes(
    claim: &EnrollmentAuthorityClaim,
) -> Result<Vec<u8>, EnrollmentError> {
    let bytes =
        serde_json::to_vec(claim).map_err(|error| EnrollmentError::Encode(error.to_string()))?;
    if bytes.len() > MAX_ENROLLMENT_AUTHORITY_BYTES {
        return Err(EnrollmentError::AuthorityClaimTooLarge(bytes.len()));
    }
    Ok(bytes)
}

fn decode_authority_claim(bytes: &[u8]) -> Result<EnrollmentAuthorityClaim, EnrollmentError> {
    if bytes.len() > MAX_ENROLLMENT_AUTHORITY_BYTES {
        return Err(EnrollmentError::AuthorityClaimTooLarge(bytes.len()));
    }
    reject_duplicate_json_fields(bytes)?;
    validate_json_bounds(bytes)?;
    let probe: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| EnrollmentError::Decode(error.to_string()))?;
    let schema = probe
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| EnrollmentError::Decode("authority schema_version is missing".into()))?;
    let schema = u32::try_from(schema).unwrap_or(u32::MAX);
    if schema != ENROLLMENT_AUTHORITY_SCHEMA_VERSION {
        return Err(EnrollmentError::UnsupportedAuthoritySchema(schema));
    }
    let claim: EnrollmentAuthorityClaim = serde_json::from_slice(bytes)
        .map_err(|error| EnrollmentError::Decode(error.to_string()))?;
    if canonical_authority_claim_bytes(&claim)? != bytes {
        return Err(EnrollmentError::NonCanonicalAuthorityClaim);
    }
    Ok(claim)
}

fn read_bounded_authoritative_file(
    directory: &Dir,
    name: &str,
    maximum: usize,
    description: &str,
    allow_link_gap: bool,
) -> Result<(Vec<u8>, AuthoritativeFileIdentity), EnrollmentError> {
    let metadata = directory.symlink_metadata(name)?;
    if !cap_metadata_is_authoritative_file(&metadata) || metadata.len() > maximum as u64 {
        return Err(EnrollmentError::UnsafeNamespace(format!(
            "{description} is not a bounded regular no-follow file"
        )));
    }
    let file = open_regular_readonly(directory, name)?;
    validate_authoritative_file_with_link_gap(&file, description, allow_link_gap)?;
    let identity = authoritative_file_identity(&file)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((maximum + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(EnrollmentError::UnsafeNamespace(format!(
            "{description} exceeds its byte bound"
        )));
    }
    Ok((bytes, identity))
}

fn validate_authoritative_file(file: &File, name: &str) -> Result<(), EnrollmentError> {
    validate_authoritative_file_with_link_gap(file, name, false)
}

fn validate_authoritative_file_with_link_gap(
    file: &File,
    name: &str,
    allow_link_gap: bool,
) -> Result<(), EnrollmentError> {
    validate_authoritative_file_without_link_count(file, name)?;
    let link_count = authoritative_file_link_count(file)?;
    if link_count != 1 && !(allow_link_gap && link_count == 2) {
        return Err(EnrollmentError::UnsafeNamespace(format!(
            "opened {name} has unexpected links"
        )));
    }
    Ok(())
}

fn validate_authoritative_file_without_link_count(
    file: &File,
    name: &str,
) -> Result<(), EnrollmentError> {
    let metadata = file.metadata()?;
    if !authoritative_file_kind_allowed(
        metadata.is_file(),
        false,
        std_metadata_is_windows_reparse(&metadata),
    ) {
        return Err(EnrollmentError::UnsafeNamespace(format!(
            "opened {name} is not a regular file"
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn authoritative_file_link_count(file: &File) -> Result<u64, EnrollmentError> {
    Ok(file.metadata()?.nlink())
}

#[cfg(windows)]
fn authoritative_file_link_count(file: &File) -> Result<u64, EnrollmentError> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` retains the exact live handle and `information` is a
    // correctly sized writable result value.
    let result = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) };
    if result == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(u64::from(information.nNumberOfLinks))
}

#[cfg(not(any(unix, windows)))]
fn authoritative_file_link_count(_file: &File) -> Result<u64, EnrollmentError> {
    Err(unsupported_filesystem().into())
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthoritativeFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(windows)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthoritativeFileIdentity {
    volume: u64,
    file_id: [u8; 16],
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct AuthoritativeFileIdentity;

#[cfg(unix)]
fn authoritative_file_identity(file: &File) -> Result<AuthoritativeFileIdentity, EnrollmentError> {
    let metadata = file.metadata()?;
    Ok(AuthoritativeFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn authoritative_file_identity(file: &File) -> Result<AuthoritativeFileIdentity, EnrollmentError> {
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let mut information = FILE_ID_INFO::default();
    // SAFETY: `file` retains the exact live handle and `information` is a
    // correctly sized writable FILE_ID_INFO value.
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut information as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(AuthoritativeFileIdentity {
        volume: information.VolumeSerialNumber,
        file_id: information.FileId.Identifier,
    })
}

#[cfg(not(any(unix, windows)))]
fn authoritative_file_identity(_file: &File) -> Result<AuthoritativeFileIdentity, EnrollmentError> {
    Err(unsupported_filesystem().into())
}

#[cfg(unix)]
fn lease_resource_id(identity: &AuthoritativeFileIdentity) -> ContentDigest {
    let mut hasher = Sha256::new();
    hasher.update(b"tine/enrollment-lease-resource/v1\0unix-dev-inode\0");
    hasher.update(identity.device.to_be_bytes());
    hasher.update(identity.inode.to_be_bytes());
    ContentDigest::from_bytes(hasher.finalize().into())
}

#[cfg(windows)]
fn lease_resource_id(identity: &AuthoritativeFileIdentity) -> ContentDigest {
    let mut hasher = Sha256::new();
    hasher.update(b"tine/enrollment-lease-resource/v1\0windows-volume-file-id\0");
    hasher.update(identity.volume.to_be_bytes());
    hasher.update(identity.file_id);
    ContentDigest::from_bytes(hasher.finalize().into())
}

#[cfg(not(any(unix, windows)))]
fn lease_resource_id(_identity: &AuthoritativeFileIdentity) -> ContentDigest {
    ContentDigest::from_bytes([0; 32])
}

#[cfg(unix)]
fn authority_resource_id(identity: &AuthoritativeFileIdentity) -> ContentDigest {
    let mut hasher = Sha256::new();
    hasher.update(b"tine/enrollment-authority-resource/v1\0unix-dev-inode\0");
    hasher.update(identity.device.to_be_bytes());
    hasher.update(identity.inode.to_be_bytes());
    ContentDigest::from_bytes(hasher.finalize().into())
}

#[cfg(windows)]
fn authority_resource_id(identity: &AuthoritativeFileIdentity) -> ContentDigest {
    let mut hasher = Sha256::new();
    hasher.update(b"tine/enrollment-authority-resource/v1\0windows-volume-file-id\0");
    hasher.update(identity.volume.to_be_bytes());
    hasher.update(identity.file_id);
    ContentDigest::from_bytes(hasher.finalize().into())
}

#[cfg(not(any(unix, windows)))]
fn authority_resource_id(_identity: &AuthoritativeFileIdentity) -> ContentDigest {
    ContentDigest::from_bytes([0; 32])
}

#[cfg(windows)]
fn std_metadata_is_windows_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
}

#[cfg(not(windows))]
fn std_metadata_is_windows_reparse(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn cap_metadata_is_windows_reparse(metadata: &cap_std::fs::Metadata) -> bool {
    metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
}

#[cfg(not(windows))]
fn cap_metadata_is_windows_reparse(_metadata: &cap_std::fs::Metadata) -> bool {
    false
}

fn authoritative_file_kind_allowed(is_file: bool, is_symlink: bool, is_reparse: bool) -> bool {
    is_file && !is_symlink && !is_reparse
}

fn cap_metadata_is_authoritative_file(metadata: &cap_std::fs::Metadata) -> bool {
    authoritative_file_kind_allowed(
        metadata.is_file(),
        metadata.file_type().is_symlink(),
        cap_metadata_is_windows_reparse(metadata),
    )
}

#[cfg(unix)]
fn open_regular_readonly(directory: &Dir, name: &str) -> std::io::Result<File> {
    openat_regular(directory, name, libc::O_RDONLY, 0)
}

#[cfg(windows)]
fn open_regular_readonly(directory: &Dir, name: &str) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    Ok(directory.open_with(name, &options)?.into_std())
}

#[cfg(not(any(unix, windows)))]
fn open_regular_readonly(_directory: &Dir, _name: &str) -> std::io::Result<File> {
    Err(unsupported_filesystem())
}

#[cfg(unix)]
fn openat_regular(directory: &Dir, name: &str, flags: i32, mode: u32) -> std::io::Result<File> {
    let name = CString::new(name)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "invalid file name"))?;
    // SAFETY: name is a live relative C string and directory is retained.
    let fd = unsafe {
        libc::openat(
            directory.as_fd().as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: openat returned one newly owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

// Must exist wherever ANY caller does. Two different exclusion lists reach it:
// the `not(any(unix, windows))` helpers above, and `rename_noreplace`'s fallback
// below, which is `not(any(linux, macos, ios, android, windows))` and therefore
// compiles on tvOS/BSD — all of which ARE `unix`. Gating this on
// `not(any(unix, windows))` made it vanish exactly where that fallback needed it,
// breaking the iOS build from 6162b381 (2026-07-26) until 2026-08-08. The list
// below is the union of both caller sets.
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "android",
    windows
)))]
fn unsupported_filesystem() -> std::io::Error {
    std::io::Error::new(
        ErrorKind::Unsupported,
        "durable no-follow enrollment files are unsupported on this target",
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EnrollmentError {
    Io(String),
    UnsafeNamespace(String),
    UnsupportedArtifact(String),
    NamespaceBoundExceeded,
    AmbiguousAuthorityProvisioning,
    LeaseResourceMismatch,
    AuthorityMismatch,
    AuthorityClaimTooLarge(usize),
    NonCanonicalAuthorityClaim,
    UnsupportedAuthoritySchema(u32),
    NonCanonicalLocalActivationReservation,
    UnsupportedLocalActivationReservationSchema(u32),
    UnsupportedCheckpointSchema(u32),
    MissingAuthenticatedCheckpoint,
    /// A v6 CRC checkpoint does not bind its canonical record fields.
    CheckpointIntegrityFailed,
    /// Record and checkpoint codecs are version-paired and never interchangeable.
    IllegalCheckpointPair,
    MalformedHead,
    MissingChainRecord(ContentDigest),
    RecordDigestMismatch(ContentDigest),
    UnsupportedRecordSchema(u32),
    UnsupportedPacketSchema(u32),
    UnsupportedCompatibility {
        expected: EnrollmentCompatibilityV1,
        found: EnrollmentCompatibilityV1,
    },
    FutureUnsupportedLifecycle(String),
    Decode(String),
    Encode(String),
    NonCanonicalRecord,
    RecordTooLarge(usize),
    JsonDepthExceeded,
    JsonTokenBoundExceeded,
    BindingMismatch(EnrollmentBindingField),
    PublishedBatchMismatch,
    InvalidVerifiedLocalTerminal,
    InvalidLocalActiveAnchor,
    InvalidSharedProjectionBaseEvidence,
    UnsupportedSharedEnrollmentDescriptorSchema(u32),
    UnsupportedJoinerWorkspaceArchiveSchema(u32),
    UnsafeSharedEnrollmentHandoff,
    SharedEnrollmentBindingMismatch,
    SharedEnrollmentDescriptorDigestMismatch,
    DirtyUniqueLocalTail,
    InvalidBlockedReason,
    IllegalLifecycle(&'static str),
    IllegalTransition,
    GenerationOverflow,
    NonmonotonicGeneration,
    HistoryAccumulatorMismatch,
    ChainCycle,
}

impl fmt::Display for EnrollmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "enrollment I/O failed: {error}"),
            Self::UnsafeNamespace(detail) => {
                write!(formatter, "unsafe enrollment namespace: {detail}")
            }
            Self::UnsupportedArtifact(name) => {
                write!(formatter, "unsupported enrollment artifact: {name}")
            }
            Self::NamespaceBoundExceeded => {
                formatter.write_str("enrollment namespace entry bound exceeded")
            }
            Self::AmbiguousAuthorityProvisioning => {
                formatter.write_str("ambiguous enrollment authority provisioning state")
            }
            Self::LeaseResourceMismatch => {
                formatter.write_str("enrollment lease resource was replaced or unlinked")
            }
            Self::AuthorityMismatch => {
                formatter.write_str("enrollment authority claim was replaced or substituted")
            }
            Self::AuthorityClaimTooLarge(bytes) => {
                write!(
                    formatter,
                    "enrollment authority claim is too large: {bytes} bytes"
                )
            }
            Self::NonCanonicalAuthorityClaim => {
                formatter.write_str("enrollment authority claim is not canonical")
            }
            Self::UnsupportedAuthoritySchema(schema) => {
                write!(
                    formatter,
                    "unsupported enrollment authority schema {schema}"
                )
            }
            Self::NonCanonicalLocalActivationReservation => {
                formatter.write_str("local activation reservation is not canonical")
            }
            Self::UnsupportedLocalActivationReservationSchema(schema) => {
                write!(
                    formatter,
                    "unsupported local activation reservation schema {schema}"
                )
            }
            Self::UnsupportedCheckpointSchema(schema) => {
                write!(
                    formatter,
                    "unsupported enrollment checkpoint schema {schema}"
                )
            }
            Self::MissingAuthenticatedCheckpoint => {
                formatter.write_str("enrollment history suffix has no authenticated checkpoint")
            }
            Self::CheckpointIntegrityFailed => {
                formatter.write_str("enrollment checkpoint integrity check failed")
            }
            Self::IllegalCheckpointPair => {
                formatter.write_str("illegal enrollment record/checkpoint schema pair")
            }
            Self::MalformedHead => formatter.write_str("enrollment head is malformed"),
            Self::MissingChainRecord(digest) => {
                write!(formatter, "enrollment chain record is missing: {digest}")
            }
            Self::RecordDigestMismatch(digest) => {
                write!(formatter, "enrollment record digest mismatch: {digest}")
            }
            Self::UnsupportedRecordSchema(schema) => {
                write!(formatter, "unsupported enrollment schema {schema}")
            }
            Self::UnsupportedPacketSchema(schema) => {
                write!(formatter, "unsupported published packet schema {schema}")
            }
            Self::UnsupportedCompatibility { .. } => {
                formatter.write_str("unsupported enrollment compatibility bundle")
            }
            Self::FutureUnsupportedLifecycle(state) => {
                write!(formatter, "unsupported future/shared lifecycle {state}")
            }
            Self::Decode(error) => write!(formatter, "enrollment decode failed: {error}"),
            Self::Encode(error) => write!(formatter, "enrollment encode failed: {error}"),
            Self::NonCanonicalRecord => formatter.write_str("enrollment record is not canonical"),
            Self::RecordTooLarge(bytes) => {
                write!(formatter, "enrollment record is too large: {bytes} bytes")
            }
            Self::JsonDepthExceeded => formatter.write_str("enrollment JSON depth exceeded"),
            Self::JsonTokenBoundExceeded => {
                formatter.write_str("enrollment JSON token bound exceeded")
            }
            Self::BindingMismatch(field) => {
                write!(formatter, "enrollment binding mismatch: {field:?}")
            }
            Self::PublishedBatchMismatch => {
                formatter.write_str("published packet batch/import identity mismatch")
            }
            Self::InvalidVerifiedLocalTerminal => formatter.write_str(
                "verified-local terminal bootstrap identity or proof counts are inconsistent",
            ),
            Self::InvalidLocalActiveAnchor => formatter.write_str(
                "local-active bootstrap anchor identity or proof counts are inconsistent",
            ),
            Self::InvalidSharedProjectionBaseEvidence => {
                formatter.write_str("shared enrollment projection/base evidence is inconsistent")
            }
            Self::UnsupportedSharedEnrollmentDescriptorSchema(schema) => {
                write!(
                    formatter,
                    "unsupported shared enrollment descriptor schema {schema}"
                )
            }
            Self::UnsupportedJoinerWorkspaceArchiveSchema(schema) => {
                write!(
                    formatter,
                    "unsupported joiner workspace archive schema {schema}"
                )
            }
            Self::UnsafeSharedEnrollmentHandoff => {
                formatter.write_str("shared enrollment requires a safe handoff")
            }
            Self::SharedEnrollmentBindingMismatch => formatter
                .write_str("shared enrollment binding or projection/base evidence mismatch"),
            Self::SharedEnrollmentDescriptorDigestMismatch => {
                formatter.write_str("shared enrollment descriptor digest mismatch")
            }
            Self::DirtyUniqueLocalTail => {
                formatter.write_str("joiner has unique unprojected local operations")
            }
            Self::InvalidBlockedReason => formatter.write_str("invalid blocked reason code"),
            Self::IllegalLifecycle(detail) => {
                write!(formatter, "illegal enrollment lifecycle: {detail}")
            }
            Self::IllegalTransition => formatter.write_str("illegal enrollment transition"),
            Self::GenerationOverflow => formatter.write_str("enrollment generation overflow"),
            Self::NonmonotonicGeneration => {
                formatter.write_str("nonmonotonic enrollment generation")
            }
            Self::HistoryAccumulatorMismatch => {
                formatter.write_str("enrollment history accumulator mismatch")
            }
            Self::ChainCycle => formatter.write_str("enrollment chain contains a cycle"),
        }
    }
}

impl std::error::Error for EnrollmentError {}

impl From<std::io::Error> for EnrollmentError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    /// The module header says "No writer, lease, transition, or recovery
    /// authority remains." It was prose only: `open_component` carried a live
    /// `mkdir` + `fchmod(0o700)` + fsync branch behind a `create: bool` that every
    /// caller passed `false`, so nothing but inspection stopped the next caller
    /// passing `true`. The branch is gone; this keeps it gone.
    ///
    /// A new primitive here is not necessarily wrong — but it contradicts the
    /// header, so it is a deliberate edit of both, not an addition nobody notices.
    #[test]
    fn the_enrollment_namespace_has_no_creation_authority() {
        const WRITE_PRIMITIVES: &[&str] = &[
            "create_dir",
            "create_new",
            "remove_file",
            "remove_dir",
            "write_all",
            "set_permissions",
            "fchmod",
            "sync_all",
            "sync_data",
            "sync_reconstructible_directory",
            "lock_exclusive",
            "try_lock",
            "O_CREAT",
            "O_WRONLY",
            "O_RDWR",
        ];
        // Comments describe the retired branch on purpose, and this module's own
        // list names every primitive verbatim; scan the production code only.
        let source = include_str!("enrollment.rs");
        let production = source
            .split_once("\nmod tests {")
            .map(|(before, _)| before)
            .expect("this test module terminates the production source");
        let code: String = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for primitive in WRITE_PRIMITIVES {
            assert!(
                !code.contains(primitive),
                "enrollment.rs uses `{primitive}`, but its header says no writer, lease, \
                 transition, or recovery authority remains. Either the authority is real \
                 (fix the header, and say which in-scope failure the write defends against) \
                 or the call is a mistake."
            );
        }
    }
}
