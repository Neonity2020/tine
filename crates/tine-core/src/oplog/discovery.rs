//! Inert read-only discovery of an explicitly selected sparse-oplog profile.
//!
//! Discovery is advisory only. It never activates a profile, constructs writer
//! authority, repairs residue, or transfers an unsafe session. Every later
//! actor must independently reopen and authenticate the current runtime state.

use std::path::Path;

use super::enrollment::{
    inspect_existing_enrollment_at, EnrollmentBindingField, EnrollmentDiscoveryInspection,
    EnrollmentDiscoveryLifecycle, EnrollmentError,
};
use super::object_store::{inspect_existing_archive_at, ArchiveDiscoveryInspection, StoreError};
use super::{CanonicalGraphResourceId, ContentDigest, ManagedStorageRefusalScenario};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StartupStorageProfile {
    LegacyDefault,
    ExperimentalSparse,
}

pub(crate) struct DiscoveryRequest<'a> {
    pub(crate) profile: StartupStorageProfile,
    pub(crate) graph_resource_id: CanonicalGraphResourceId,
    pub(crate) runtime_root: &'a Path,
    pub(crate) archive_root: &'a Path,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DiscoveryClassification {
    LegacyDefault,
    Absent,
    ExistingLocalActive(LocalActiveAdvisory),
    ExistingNonActive(NonActiveAdvisory),
    Blocked(BlockedAdvisory),
    Retryable(DiscoveryComponent, String),
    UnsupportedOrIncompatible(DiscoveryComponent, ManagedStorageRefusalScenario),
    CorruptOrUnreadable(DiscoveryComponent, ManagedStorageRefusalScenario),
    AmbiguousOrForeignResidue(AmbiguousEvidence, ManagedStorageRefusalScenario),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiscoveryComponent {
    Enrollment,
    Archive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NonActiveStage {
    ShadowImport,
    VerifiedLocal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NonActiveAdvisory {
    pub(crate) stage: NonActiveStage,
    pub(crate) binding: super::enrollment::EnrollmentBindingV1,
    pub(crate) enrollment_head: ContentDigest,
    pub(crate) enrollment_generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BlockedAdvisory {
    pub(crate) binding: super::enrollment::EnrollmentBindingV1,
    pub(crate) enrollment_head: ContentDigest,
    pub(crate) enrollment_generation: u64,
    pub(crate) reason_code: String,
    pub(crate) evidence_digest: ContentDigest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LocalActiveAdvisory {
    pub(crate) binding: super::enrollment::EnrollmentBindingV1,
    pub(crate) enrollment_head: ContentDigest,
    pub(crate) enrollment_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AmbiguousEvidence {
    EnrollmentResidue,
    EnrollmentNamespace,
    EnrollmentGraphBinding,
    ArchiveResidue,
    ArchiveNamespace,
    ArchiveBinding,
}

/// Classify explicitly supplied graph/runtime/archive identities without
/// changing any filesystem state.
pub(crate) fn discover_startup(request: &DiscoveryRequest<'_>) -> DiscoveryClassification {
    // The legacy/default path is intentionally decided before either
    // experimental pathname is observed.
    if request.profile == StartupStorageProfile::LegacyDefault {
        return DiscoveryClassification::LegacyDefault;
    }

    let enrollment =
        match inspect_existing_enrollment_at(request.runtime_root, request.graph_resource_id) {
            Ok(inspection) => inspection,
            Err(error) => return classify_enrollment_error(error),
        };
    let evidence = match enrollment {
        EnrollmentDiscoveryInspection::Absent => {
            return match inspect_existing_archive_at(request.archive_root, None) {
                Ok(ArchiveDiscoveryInspection::Absent) => DiscoveryClassification::Absent,
                Ok(ArchiveDiscoveryInspection::Residue) => {
                    DiscoveryClassification::AmbiguousOrForeignResidue(
                        AmbiguousEvidence::ArchiveResidue,
                        ManagedStorageRefusalScenario::SyncConflict,
                    )
                }
                Err(error) => classify_archive_error(error),
            };
        }
        EnrollmentDiscoveryInspection::Residue => {
            return DiscoveryClassification::AmbiguousOrForeignResidue(
                AmbiguousEvidence::EnrollmentResidue,
                ManagedStorageRefusalScenario::CrashTruncated,
            );
        }
        EnrollmentDiscoveryInspection::Present(evidence) => evidence,
    };

    match &evidence.lifecycle {
        EnrollmentDiscoveryLifecycle::ShadowImport => {
            DiscoveryClassification::ExistingNonActive(NonActiveAdvisory {
                stage: NonActiveStage::ShadowImport,
                binding: evidence.binding,
                enrollment_head: evidence.head_digest,
                enrollment_generation: evidence.generation,
            })
        }
        EnrollmentDiscoveryLifecycle::VerifiedLocal => {
            DiscoveryClassification::ExistingNonActive(NonActiveAdvisory {
                stage: NonActiveStage::VerifiedLocal,
                binding: evidence.binding,
                enrollment_head: evidence.head_digest,
                enrollment_generation: evidence.generation,
            })
        }
        EnrollmentDiscoveryLifecycle::Blocked {
            reason_code,
            evidence_digest,
        } => DiscoveryClassification::Blocked(BlockedAdvisory {
            binding: evidence.binding,
            enrollment_head: evidence.head_digest,
            enrollment_generation: evidence.generation,
            reason_code: reason_code.clone(),
            evidence_digest: *evidence_digest,
        }),
        EnrollmentDiscoveryLifecycle::LocalActive(_) => {
            DiscoveryClassification::ExistingLocalActive(LocalActiveAdvisory {
                binding: evidence.binding,
                enrollment_head: evidence.head_digest,
                enrollment_generation: evidence.generation,
            })
        }
    }
}

pub(crate) fn classify_enrollment_error(error: EnrollmentError) -> DiscoveryClassification {
    let detail = error.to_string();
    match error {
        EnrollmentError::Io(_) => {
            DiscoveryClassification::Retryable(DiscoveryComponent::Enrollment, detail)
        }
        EnrollmentError::UnsupportedAuthoritySchema(_)
        | EnrollmentError::UnsupportedLocalActivationReservationSchema(_)
        | EnrollmentError::UnsupportedCheckpointSchema(_)
        | EnrollmentError::UnsupportedRecordSchema(_)
        | EnrollmentError::UnsupportedPacketSchema(_)
        | EnrollmentError::UnsupportedSharedEnrollmentDescriptorSchema(_)
        | EnrollmentError::UnsupportedJoinerWorkspaceArchiveSchema(_)
        | EnrollmentError::UnsupportedCompatibility { .. }
        | EnrollmentError::FutureUnsupportedLifecycle(_) => {
            DiscoveryClassification::UnsupportedOrIncompatible(
                DiscoveryComponent::Enrollment,
                ManagedStorageRefusalScenario::ProtocolIncompatible,
            )
        }
        EnrollmentError::BindingMismatch(EnrollmentBindingField::GraphResource) => {
            DiscoveryClassification::AmbiguousOrForeignResidue(
                AmbiguousEvidence::EnrollmentGraphBinding,
                ManagedStorageRefusalScenario::SyncConflict,
            )
        }
        EnrollmentError::UnsafeNamespace(_) => DiscoveryClassification::AmbiguousOrForeignResidue(
            AmbiguousEvidence::EnrollmentNamespace,
            ManagedStorageRefusalScenario::UnsafeFilesystemKind,
        ),
        EnrollmentError::NamespaceBoundExceeded => {
            DiscoveryClassification::AmbiguousOrForeignResidue(
                AmbiguousEvidence::EnrollmentNamespace,
                ManagedStorageRefusalScenario::Bounds,
            )
        }
        EnrollmentError::AmbiguousAuthorityProvisioning => {
            DiscoveryClassification::AmbiguousOrForeignResidue(
                AmbiguousEvidence::EnrollmentNamespace,
                ManagedStorageRefusalScenario::CrashTruncated,
            )
        }
        EnrollmentError::BindingMismatch(_)
        | EnrollmentError::UnsupportedArtifact(_)
        | EnrollmentError::PublishedBatchMismatch
        | EnrollmentError::SharedEnrollmentBindingMismatch
        | EnrollmentError::SharedEnrollmentDescriptorDigestMismatch
        | EnrollmentError::DirtyUniqueLocalTail => {
            DiscoveryClassification::AmbiguousOrForeignResidue(
                AmbiguousEvidence::EnrollmentNamespace,
                ManagedStorageRefusalScenario::SyncConflict,
            )
        }
        EnrollmentError::AuthorityClaimTooLarge(_)
        | EnrollmentError::RecordTooLarge(_)
        | EnrollmentError::JsonDepthExceeded
        | EnrollmentError::JsonTokenBoundExceeded => DiscoveryClassification::CorruptOrUnreadable(
            DiscoveryComponent::Enrollment,
            ManagedStorageRefusalScenario::Bounds,
        ),
        _ => DiscoveryClassification::CorruptOrUnreadable(
            DiscoveryComponent::Enrollment,
            ManagedStorageRefusalScenario::DiskCorrupt,
        ),
    }
}

fn classify_archive_error(error: StoreError) -> DiscoveryClassification {
    let detail = error.to_string();
    match error {
        StoreError::Io(_) => {
            DiscoveryClassification::Retryable(DiscoveryComponent::Archive, detail)
        }
        StoreError::UpgradeRequired { .. } | StoreError::UnsupportedStoreVersion { .. } => {
            DiscoveryClassification::UnsupportedOrIncompatible(
                DiscoveryComponent::Archive,
                ManagedStorageRefusalScenario::ProtocolIncompatible,
            )
        }
        StoreError::WorkspaceMismatch { .. } | StoreError::LineageMismatch { .. } => {
            DiscoveryClassification::AmbiguousOrForeignResidue(
                AmbiguousEvidence::ArchiveBinding,
                ManagedStorageRefusalScenario::SyncConflict,
            )
        }
        StoreError::UnsafeEntry(_) => DiscoveryClassification::AmbiguousOrForeignResidue(
            AmbiguousEvidence::ArchiveNamespace,
            ManagedStorageRefusalScenario::UnsafeFilesystemKind,
        ),
        StoreError::MalformedPath(_) => DiscoveryClassification::AmbiguousOrForeignResidue(
            AmbiguousEvidence::ArchiveNamespace,
            ManagedStorageRefusalScenario::MalformedImport,
        ),
        StoreError::StoredFileTooLarge { .. } | StoreError::PageNamePointBatchTooLarge { .. } => {
            DiscoveryClassification::CorruptOrUnreadable(
                DiscoveryComponent::Archive,
                ManagedStorageRefusalScenario::Bounds,
            )
        }
        StoreError::ObjectCollision(_)
        | StoreError::BatchCollision(_)
        | StoreError::LineageClaimCollision(_)
        | StoreError::ImmutableCollision(_) => DiscoveryClassification::CorruptOrUnreadable(
            DiscoveryComponent::Archive,
            ManagedStorageRefusalScenario::SyncConflict,
        ),
        _ => DiscoveryClassification::CorruptOrUnreadable(
            DiscoveryComponent::Archive,
            ManagedStorageRefusalScenario::DiskCorrupt,
        ),
    }
}
