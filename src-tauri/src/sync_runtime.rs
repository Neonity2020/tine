//! Explicit Tauri-facing sparse-v2 runtime composition.
//!
//! A durable caller-owned binding in private app data is the opt-in marker.
//! Ordinary graph loading never creates it. Once present, startup discovers
//! sparse state and never falls back to a legacy `Graph` writer.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};
use tine_core::model::GraphMeta;
use tine_core::oplog::{
    DeviceId, DocumentId, LineageDigest, ProjectionEndpointId, SessionId, WorkspaceId,
};
use tine_core::sync_runtime::{
    inspect_shared_enrollment_for_cold_discovery, SyncAmbiguousEvidence,
    SyncApplicationMoveSubtreesOutcome, SyncApplicationMoveSubtreesRequest,
    SyncLocalActivationIdentities, SyncLocalActivationPhase, SyncLocalActivationProgress,
    SyncLocalActivationRequest, SyncLocalActivationResult, SyncLocalActivationStage,
    SyncLocalActivationStatus, SyncNonActiveStage, SyncRuntimeComponent, SyncRuntimeHandle,
    SyncRuntimeLifecycle, SyncRuntimeOpenPhase, SyncRuntimeOpenProgress, SyncRuntimeOpenRequest,
    SyncRuntimeOpenResult, SyncRuntimeOpenStatus, SyncRuntimeRecovery, SyncRuntimeStatusSnapshot,
    SyncRuntimeTick, SyncSharedEnrollmentDescriptor, SyncSharedPhase, SyncSharedRole,
    SyncShutdownOutcome, SyncStorageProfile,
};
use uuid::Uuid;

const BINDING_SCHEMA_VERSION: u32 = 2;
const SPARSE_BINDING_DIR: &str = "sparse-v2";
const SPARSE_BINDING_FILE: &str = "binding.json";
const SPARSE_RECOVERY_DIR: &str = "sparse-v2-recovery";
static BINDING_WRITE: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SparseV2ActivationRecord {
    schema_version: u32,
    graph_root: String,
    graph_meta: GraphMeta,
    workspace_id: WorkspaceId,
    lineage_digest: LineageDigest,
    catalog_document_id: DocumentId,
    endpoint_id: ProjectionEndpointId,
    device_id: DeviceId,
    preparation_id: Uuid,
    activation_session_id: SessionId,
}

impl SparseV2ActivationRecord {
    fn new(graph_root: &Path, graph_meta: GraphMeta, device_id: DeviceId) -> Self {
        let lineage_seed = Uuid::new_v4();
        Self {
            schema_version: BINDING_SCHEMA_VERSION,
            graph_root: graph_root.display().to_string(),
            graph_meta,
            workspace_id: WorkspaceId::new(),
            lineage_digest: LineageDigest::of(lineage_seed.as_bytes()),
            catalog_document_id: DocumentId::new(),
            endpoint_id: ProjectionEndpointId::new(),
            device_id,
            preparation_id: Uuid::new_v4(),
            activation_session_id: SessionId::new(),
        }
    }

    fn from_shared(
        graph_root: &Path,
        graph_meta: GraphMeta,
        device_id: DeviceId,
        descriptor: &SyncSharedEnrollmentDescriptor,
    ) -> Self {
        Self {
            schema_version: BINDING_SCHEMA_VERSION,
            graph_root: graph_root.display().to_string(),
            graph_meta,
            workspace_id: descriptor.workspace_id,
            lineage_digest: descriptor.lineage_digest,
            catalog_document_id: descriptor.catalog_document_id,
            endpoint_id: ProjectionEndpointId::new(),
            device_id,
            preparation_id: Uuid::new_v4(),
            activation_session_id: SessionId::new(),
        }
    }

    fn validate_for(&self, graph_root: &Path) -> Result<(), String> {
        if self.schema_version != BINDING_SCHEMA_VERSION {
            return Err("Tine-managed storage has an unsupported local setup version.".into());
        }
        if self.graph_root != graph_root.display().to_string()
            || self.graph_meta.root != self.graph_root
        {
            return Err("Tine-managed storage data belongs to a different graph.".into());
        }
        Ok(())
    }

    fn private_root(&self, app: &tauri::AppHandle) -> Result<PathBuf, String> {
        sparse_private_root(app, Path::new(&self.graph_root))
    }

    fn open_request(&self, app: &tauri::AppHandle) -> Result<SyncRuntimeOpenRequest, String> {
        let private = self.private_root(app)?;
        Ok(SyncRuntimeOpenRequest {
            profile: SyncStorageProfile::ExperimentalLocal,
            graph_root: PathBuf::from(&self.graph_root),
            archive_root: private.join("archive"),
            enrollment_root: private.join("enrollment"),
            receipt_root: private.join("receipts"),
            database_path: private.join("projection/materialization.sqlite"),
            application_runtime_root: private.join("runtime"),
            migration_backup_root: private.join("migration-backup"),
            provider_root: PathBuf::from(&self.graph_root).join(".tine-sync/v2/shared"),
            provider_journal_root: private.join("provider/device/journal"),
        })
    }

    fn activation_request(
        &self,
        app: &tauri::AppHandle,
    ) -> Result<SyncLocalActivationRequest, String> {
        let private = self.private_root(app)?;
        Ok(SyncLocalActivationRequest {
            graph_root: PathBuf::from(&self.graph_root),
            archive_root: private.join("archive"),
            enrollment_root: private.join("enrollment"),
            receipt_root: private.join("receipts"),
            database_path: private.join("projection/materialization.sqlite"),
            application_runtime_root: private.join("runtime"),
            migration_backup_root: private.join("migration-backup"),
            capture_root: private.join("capture"),
            preparation_root: private.join("preparation"),
            provider_root: PathBuf::from(&self.graph_root).join(".tine-sync/v2/shared"),
            provider_journal_root: private.join("provider/device/journal"),
            identities: SyncLocalActivationIdentities {
                workspace_id: self.workspace_id,
                lineage_digest: self.lineage_digest,
                catalog_document_id: self.catalog_document_id,
                endpoint_id: self.endpoint_id,
                device_id: self.device_id,
                preparation_id: self.preparation_id,
                session_id: self.activation_session_id,
            },
        })
    }
}

fn sparse_private_root(app: &tauri::AppHandle, graph_root: &Path) -> Result<PathBuf, String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("couldn't resolve private app-data directory: {error}"))?;
    let mut digest = Sha256::new();
    digest.update(b"tine/sparse-v2/app-binding/v1\0");
    digest.update(graph_root.as_os_str().as_encoded_bytes());
    let key = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(app_data.join(SPARSE_BINDING_DIR).join(key))
}

fn binding_path(app: &tauri::AppHandle, graph_root: &Path) -> Result<PathBuf, String> {
    Ok(sparse_private_root(app, graph_root)?.join(SPARSE_BINDING_FILE))
}

fn sparse_recovery_root(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|root| root.join(SPARSE_RECOVERY_DIR))
        .map_err(|error| format!("couldn't resolve private app-data directory: {error}"))
}

fn read_binding_at(
    path: &Path,
    graph_root: &Path,
) -> Result<Option<SparseV2ActivationRecord>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!("Couldn't read Tine-managed storage data: {error}"));
        }
    };
    let record: SparseV2ActivationRecord = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Tine-managed storage data is corrupted: {error}"))?;
    record.validate_for(graph_root)?;
    Ok(Some(record))
}

fn persist_binding_at(path: &Path, record: &SparseV2ActivationRecord) -> Result<(), String> {
    let encoded = serde_json::to_string_pretty(record)
        .map(|mut value| {
            value.push('\n');
            value
        })
        .map_err(|error| error.to_string())?;
    tine_core::model::atomic_update(path, &BINDING_WRITE, |existing| {
        if existing.trim().is_empty() || existing.trim() == "{}" {
            return Ok(encoded.clone());
        }
        let found: SparseV2ActivationRecord = serde_json::from_str(existing)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let found_value = serde_json::to_value(&found)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let expected_value = serde_json::to_value(record)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if found_value != expected_value {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "Tine-managed storage is already set up for this graph.",
            ));
        }
        Ok(encoded.clone())
    })
    .map_err(|error| format!("Couldn't save Tine-managed storage setup: {error}"))
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum SparseV2Availability {
    LegacyDefault,
    Joinable {
        descriptor_digest: String,
    },
    Active,
    Retryable {
        stage: String,
        detail: String,
    },
    Blocked {
        reason_code: String,
    },
    Refused {
        reason_code: String,
        detail: Option<String>,
    },
}

impl SparseV2Availability {
    fn from_open(status: SyncRuntimeOpenStatus) -> Self {
        match status {
            SyncRuntimeOpenStatus::LegacyDefault => Self::LegacyDefault,
            SyncRuntimeOpenStatus::Active => Self::Active,
            SyncRuntimeOpenStatus::Absent => Self::Retryable {
                stage: "absent".into(),
                detail: "Tine-managed storage setup has not completed.".into(),
            },
            SyncRuntimeOpenStatus::ExistingNonActive(stage) => Self::Retryable {
                stage: non_active_stage(stage).into(),
                detail: "Tine-managed storage setup can be resumed.".into(),
            },
            SyncRuntimeOpenStatus::Blocked { reason_code } => Self::Blocked { reason_code },
            SyncRuntimeOpenStatus::UnsupportedOrIncompatible(component) => Self::Refused {
                reason_code: format!("unsupported_{}", component_name(component)),
                detail: None,
            },
            SyncRuntimeOpenStatus::CorruptOrUnreadable(component) => Self::Refused {
                reason_code: format!("corrupt_{}", component_name(component)),
                detail: None,
            },
            SyncRuntimeOpenStatus::AmbiguousOrForeignResidue(evidence) => Self::Refused {
                reason_code: format!("ambiguous_{}", ambiguous_name(evidence)),
                detail: None,
            },
            SyncRuntimeOpenStatus::OpenRefused { detail } => Self::Retryable {
                stage: "local_active".into(),
                detail,
            },
        }
    }

    fn from_activation(status: SyncLocalActivationStatus) -> Self {
        match status {
            SyncLocalActivationStatus::Active => Self::Active,
            SyncLocalActivationStatus::Retryable {
                durable_stage,
                detail,
            } => Self::Retryable {
                stage: activation_stage(durable_stage).into(),
                detail,
            },
            SyncLocalActivationStatus::Blocked { reason_code } => Self::Blocked { reason_code },
            SyncLocalActivationStatus::UnsupportedOrIncompatible(component) => Self::Refused {
                reason_code: format!("unsupported_{}", component_name(component)),
                detail: None,
            },
            SyncLocalActivationStatus::CorruptOrUnreadable(component) => Self::Refused {
                reason_code: format!("corrupt_{}", component_name(component)),
                detail: None,
            },
            SyncLocalActivationStatus::AmbiguousOrForeignResidue(evidence) => Self::Refused {
                reason_code: format!("ambiguous_{}", ambiguous_name(evidence)),
                detail: None,
            },
        }
    }
}

fn activation_stage(stage: SyncLocalActivationStage) -> &'static str {
    match stage {
        SyncLocalActivationStage::Absent => "absent",
        SyncLocalActivationStage::ShadowImport => "shadow_import",
        SyncLocalActivationStage::VerifiedLocal => "verified_local",
        SyncLocalActivationStage::LocalActive => "local_active",
    }
}

fn non_active_stage(stage: SyncNonActiveStage) -> &'static str {
    match stage {
        SyncNonActiveStage::ShadowImport => "shadow_import",
        SyncNonActiveStage::VerifiedLocal => "verified_local",
    }
}

fn component_name(component: SyncRuntimeComponent) -> &'static str {
    match component {
        SyncRuntimeComponent::Enrollment => "enrollment",
        SyncRuntimeComponent::Archive => "archive",
    }
}

fn ambiguous_name(evidence: SyncAmbiguousEvidence) -> &'static str {
    match evidence {
        SyncAmbiguousEvidence::EnrollmentResidue => "enrollment_residue",
        SyncAmbiguousEvidence::EnrollmentNamespace => "enrollment_namespace",
        SyncAmbiguousEvidence::EnrollmentGraphBinding => "enrollment_graph_binding",
        SyncAmbiguousEvidence::ArchiveResidue => "archive_residue",
        SyncAmbiguousEvidence::ArchiveNamespace => "archive_namespace",
        SyncAmbiguousEvidence::ArchiveBinding => "archive_binding",
        SyncAmbiguousEvidence::ActiveArchiveMismatch => "active_archive_mismatch",
    }
}

pub(crate) struct SparseV2Binding {
    availability: SparseV2Availability,
    handle: Option<SyncRuntimeHandle>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SparseV2BindingAction {
    ReturnRetained,
    ReopenActive,
    ActivateOrResume,
}

fn action_for_runtime_lifecycle(lifecycle: &SyncRuntimeLifecycle) -> SparseV2BindingAction {
    match lifecycle {
        SyncRuntimeLifecycle::StoppedSafe | SyncRuntimeLifecycle::StoppedCrashed => {
            SparseV2BindingAction::ReopenActive
        }
        SyncRuntimeLifecycle::Active | SyncRuntimeLifecycle::Terminal => {
            SparseV2BindingAction::ReturnRetained
        }
    }
}

fn runtime_lifecycle_admits_application_pages(lifecycle: &SyncRuntimeLifecycle) -> bool {
    matches!(lifecycle, SyncRuntimeLifecycle::Active)
}

impl SparseV2Binding {
    fn from_open(result: SyncRuntimeOpenResult) -> Self {
        Self {
            availability: SparseV2Availability::from_open(result.status),
            handle: result.handle,
        }
    }

    fn from_activation(result: SyncLocalActivationResult) -> Self {
        Self {
            availability: SparseV2Availability::from_activation(result.status),
            handle: result.handle,
        }
    }

    pub(crate) fn handle(&self) -> Option<&SyncRuntimeHandle> {
        self.handle.as_ref()
    }

    /// The frontend may preflight a bulk page write only while the retained
    /// runtime has positively reported that it is still accepting work.  A
    /// stopped or terminal handle deliberately remains retained for recovery,
    /// so handle presence is not application-write authority.
    pub(crate) fn has_active_application_handle(&self) -> bool {
        self.handle
            .as_ref()
            .and_then(|handle| handle.status().ok())
            .is_some_and(|snapshot| runtime_lifecycle_admits_application_pages(&snapshot.lifecycle))
    }

    /// A managed binding with no live actor, for tests that only need the slot
    /// to *be* sparse -- e.g. proving that a read command is routed to the
    /// read-only view instead of being refused for lacking legacy authority.
    #[cfg(test)]
    pub(crate) fn without_actor_for_test() -> Self {
        Self {
            availability: SparseV2Availability::Active,
            handle: None,
        }
    }

    pub(crate) fn availability(&self) -> &SparseV2Availability {
        &self.availability
    }

    fn action(&self) -> SparseV2BindingAction {
        match &self.handle {
            Some(handle) => handle
                .status()
                .as_ref()
                .map(|snapshot| action_for_runtime_lifecycle(&snapshot.lifecycle))
                .unwrap_or(SparseV2BindingAction::ReopenActive),
            None if matches!(
                &self.availability,
                SparseV2Availability::Retryable { stage, .. }
                    if matches!(
                        stage.as_str(),
                        "local_active" | "share_prepared" | "joining" | "shared_active"
                    )
            ) =>
            {
                SparseV2BindingAction::ReopenActive
            }
            None => SparseV2BindingAction::ActivateOrResume,
        }
    }
}

fn retryable_binding(stage: &str, detail: String) -> SparseV2Binding {
    SparseV2Binding {
        availability: SparseV2Availability::Retryable {
            stage: stage.into(),
            detail,
        },
        handle: None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct SparseV2WatcherStatusDto {
    latest_enqueue: u64,
    acknowledged: u64,
    drain_in_flight: bool,
    pending: bool,
    pending_requires_full_scan: bool,
    deferred: bool,
    quiescing: bool,
    sequence_exhausted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct SparseV2TickDto {
    state: String,
    detail: Option<String>,
    epoch: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct SparseV2RuntimeStatusDto {
    lifecycle: String,
    recovery: Option<String>,
    watcher: SparseV2WatcherStatusDto,
    last_tick: Option<SparseV2TickDto>,
    detail: Option<String>,
    shared_role: Option<String>,
    shared_phase: Option<String>,
    provider_pending: usize,
    managed_local_pending: usize,
    managed_local_checkpointed_sequence: u64,
    managed_local_next_sequence: u64,
    managed_local_stage: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct SparseV2StatusDto {
    #[serde(flatten)]
    availability: SparseV2Availability,
    runtime: Option<SparseV2RuntimeStatusDto>,
    can_activate: bool,
    can_retry: bool,
    can_cancel: bool,
    cancel_reason: Option<String>,
    binding_generation: u64,
    application_page_admission: crate::state::ApplicationPageAdmission,
}

impl SparseV2StatusDto {
    pub(crate) fn legacy(binding_generation: u64) -> Self {
        Self {
            availability: SparseV2Availability::LegacyDefault,
            runtime: None,
            can_activate: true,
            can_retry: false,
            can_cancel: false,
            cancel_reason: None,
            binding_generation,
            application_page_admission: crate::state::ApplicationPageAdmission::direct(
                binding_generation,
            ),
        }
    }

    pub(crate) fn joinable(
        binding_generation: u64,
        descriptor: &SyncSharedEnrollmentDescriptor,
    ) -> Self {
        Self {
            availability: SparseV2Availability::Joinable {
                descriptor_digest: descriptor.descriptor_digest.clone(),
            },
            runtime: None,
            can_activate: false,
            can_retry: false,
            can_cancel: false,
            cancel_reason: Some(
                "This graph is already synced with another device, so returning to Direct files is unavailable."
                    .into(),
            ),
            binding_generation,
            // A joinable descriptor is discovered while this GraphSlot still
            // writes through Direct Files. `sparse_v2_status_for_slot` replaces
            // this from the actual slot as a single final step.
            application_page_admission: crate::state::ApplicationPageAdmission::direct(
                binding_generation,
            ),
        }
    }

    pub(crate) fn from_binding(binding: &SparseV2Binding, binding_generation: u64) -> Self {
        let retained_status = binding.handle().map(SyncRuntimeHandle::status);
        let runtime = retained_status
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .cloned()
            .map(runtime_status);
        let availability = match runtime.as_ref().map(|status| status.lifecycle.as_str()) {
            Some("stopped_safe") => SparseV2Availability::Retryable {
                stage: "local_active".into(),
                detail: "Tine-managed storage stopped safely and needs to be reopened.".into(),
            },
            Some("stopped_crashed") => SparseV2Availability::Retryable {
                stage: "local_active".into(),
                detail: "Tine-managed storage needs to be reopened after it stopped unexpectedly."
                    .into(),
            },
            None if retained_status.as_ref().is_some_and(Result::is_err) => {
                SparseV2Availability::Retryable {
                    stage: "local_active".into(),
                    detail: "Tine-managed storage needs to be reopened.".into(),
                }
            }
            _ => binding.availability().clone(),
        };
        let can_retry = matches!(availability, SparseV2Availability::Retryable { .. });
        Self {
            availability,
            runtime,
            can_activate: false,
            can_retry,
            can_cancel: false,
            cancel_reason: None,
            binding_generation,
            application_page_admission: if binding.has_active_application_handle() {
                crate::state::ApplicationPageAdmission::managed_writable(binding_generation)
            } else {
                crate::state::ApplicationPageAdmission::managed_unavailable(binding_generation)
            },
        }
    }

    fn from_binding_observation(
        binding: &SparseV2Binding,
        binding_generation: u64,
        snapshot: SyncRuntimeStatusSnapshot,
    ) -> Self {
        let availability = match snapshot.lifecycle {
            SyncRuntimeLifecycle::StoppedSafe => SparseV2Availability::Retryable {
                stage: "local_active".into(),
                detail: "Tine-managed storage stopped safely and needs to be reopened.".into(),
            },
            SyncRuntimeLifecycle::StoppedCrashed => SparseV2Availability::Retryable {
                stage: "local_active".into(),
                detail: "Tine-managed storage needs to be reopened after it stopped unexpectedly."
                    .into(),
            },
            SyncRuntimeLifecycle::Active | SyncRuntimeLifecycle::Terminal => {
                binding.availability().clone()
            }
        };
        let can_retry = matches!(availability, SparseV2Availability::Retryable { .. });
        let application_page_admission =
            crate::state::ApplicationPageAdmission::from_managed_runtime_lifecycle(
                binding_generation,
                &snapshot.lifecycle,
            );
        Self {
            availability,
            runtime: Some(runtime_status(snapshot)),
            can_activate: false,
            can_retry,
            can_cancel: false,
            cancel_reason: None,
            binding_generation,
            application_page_admission,
        }
    }
}

pub(crate) fn runtime_status(snapshot: SyncRuntimeStatusSnapshot) -> SparseV2RuntimeStatusDto {
    // Core's bounded provider diagnostic includes an uninitialized recovery-
    // coverage sentinel even for a purely local runtime. At the app boundary it
    // is provider work only once a shared role/phase exists.
    let provider_pending = if snapshot.shared_role.is_some() || snapshot.shared_phase.is_some() {
        snapshot.provider_pending
    } else {
        0
    };
    SparseV2RuntimeStatusDto {
        lifecycle: match snapshot.lifecycle {
            SyncRuntimeLifecycle::Active => "active",
            SyncRuntimeLifecycle::Terminal => "terminal",
            SyncRuntimeLifecycle::StoppedSafe => "stopped_safe",
            SyncRuntimeLifecycle::StoppedCrashed => "stopped_crashed",
        }
        .into(),
        recovery: snapshot.recovery.map(|recovery| {
            match recovery {
                SyncRuntimeRecovery::FirstPromotion => "first_promotion",
                SyncRuntimeRecovery::ResumedOwnUnsafe => "resumed_own_unsafe",
                SyncRuntimeRecovery::AdoptedSafeHandoff => "adopted_safe_handoff",
                SyncRuntimeRecovery::TookOverCrashedUnsafe => "took_over_crashed_unsafe",
            }
            .into()
        }),
        watcher: SparseV2WatcherStatusDto {
            latest_enqueue: snapshot.watcher.latest_enqueue,
            acknowledged: snapshot.watcher.acknowledged,
            drain_in_flight: snapshot.watcher.drain_in_flight,
            pending: snapshot.watcher.pending,
            pending_requires_full_scan: snapshot.watcher.pending_requires_full_scan,
            deferred: snapshot.watcher.deferred,
            quiescing: snapshot.watcher.quiescing,
            sequence_exhausted: snapshot.watcher.sequence_exhausted,
        },
        last_tick: snapshot.last_tick.map(tick_dto),
        detail: snapshot.detail,
        shared_role: snapshot.shared_role.map(|role| match role {
            SyncSharedRole::Initiator => "initiator".into(),
            SyncSharedRole::Joiner => "joiner".into(),
        }),
        shared_phase: snapshot.shared_phase.map(|phase| match phase {
            SyncSharedPhase::SharePrepared => "share_prepared".into(),
            SyncSharedPhase::Joining => "joining".into(),
            SyncSharedPhase::Active => "active".into(),
        }),
        provider_pending,
        managed_local_pending: snapshot.managed_local_pending,
        managed_local_checkpointed_sequence: snapshot.managed_local_checkpointed_sequence,
        managed_local_next_sequence: snapshot.managed_local_next_sequence,
        managed_local_stage: snapshot.managed_local_stage,
    }
}

pub(crate) fn tick_dto(tick: SyncRuntimeTick) -> SparseV2TickDto {
    match tick {
        SyncRuntimeTick::Idle => tick_value("idle", None, None),
        SyncRuntimeTick::LocalMutation(outcome) => {
            tick_value("local_mutation", Some(format!("{outcome:?}")), None)
        }
        SyncRuntimeTick::RecoveryBlocked(detail) => {
            tick_value("recovery_blocked", Some(detail), None)
        }
        SyncRuntimeTick::Recovering => tick_value("recovering", None, None),
        SyncRuntimeTick::RetryFull => tick_value("retry_full", None, None),
        SyncRuntimeTick::Blocked(detail) => tick_value("blocked", Some(detail), None),
        SyncRuntimeTick::Failed(detail) => tick_value("failed", Some(detail), None),
        SyncRuntimeTick::AdmittedNoop { epoch } => tick_value("admitted_noop", None, Some(epoch)),
        SyncRuntimeTick::AdmittedComplete { epoch } => {
            tick_value("admitted_complete", None, Some(epoch))
        }
        SyncRuntimeTick::Terminal(detail) => tick_value("terminal", Some(detail), None),
    }
}

fn tick_value(state: &str, detail: Option<String>, epoch: Option<u64>) -> SparseV2TickDto {
    SparseV2TickDto {
        state: state.into(),
        detail,
        epoch,
    }
}

pub(crate) fn shutdown_status(outcome: SyncShutdownOutcome) -> SparseV2RuntimeStatusDto {
    match outcome {
        SyncShutdownOutcome::Safe(snapshot) | SyncShutdownOutcome::Terminal(snapshot) => {
            runtime_status(snapshot)
        }
    }
}

/// What the graph-local provider namespace proves for the narrowly scoped
/// "Return to Direct files" escape hatch.
///
/// The first local activation creates the provider directory skeleton before
/// any shared enrollment exists.  Its mere presence is therefore not proof
/// that another device can depend on this graph.  In contrast, a descriptor,
/// provider work, or anything that does not exactly match that empty local
/// skeleton is treated as shared/unknown and remains fail-closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderNamespaceEvidence {
    LocalOnly,
    SharedOrUnknown,
}

const PROVIDER_SCAFFOLD_TREES: [&str; 2] = ["inbox", "outbox"];
const PROVIDER_SCAFFOLD_NAMESPACES: [&str; 10] = [
    "objects",
    "manifests",
    "enrollment",
    "frontier-heads-v1",
    "publication-intents-v1",
    "manifest-recovery-links-v1",
    "manifest-recovery-blobs-v1",
    ".part",
    "removed",
    "rename-evidence",
];

fn sorted_directory_entries(path: &Path) -> Result<Vec<std::fs::DirEntry>, String> {
    let mut entries = std::fs::read_dir(path)
        .map_err(|error| format!("Couldn't inspect sync data: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("Couldn't inspect sync data: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn has_exact_directory_names(entries: &[std::fs::DirEntry], expected: &[&str]) -> bool {
    entries.len() == expected.len()
        && expected.iter().all(|expected| {
            entries
                .iter()
                .any(|entry| entry.file_name().to_string_lossy() == *expected)
        })
}

fn is_empty_local_provider_scaffold(shared_root: &Path) -> Result<bool, String> {
    let root_entries = sorted_directory_entries(shared_root)?;
    if !has_exact_directory_names(&root_entries, &PROVIDER_SCAFFOLD_TREES) {
        return Ok(false);
    }

    for tree in root_entries {
        let file_type = tree
            .file_type()
            .map_err(|error| format!("Couldn't inspect sync data: {error}"))?;
        if !file_type.is_dir() || file_type.is_symlink() {
            return Ok(false);
        }
        let namespaces = sorted_directory_entries(&tree.path())?;
        if !has_exact_directory_names(&namespaces, &PROVIDER_SCAFFOLD_NAMESPACES) {
            return Ok(false);
        }
        for namespace in namespaces {
            let file_type = namespace
                .file_type()
                .map_err(|error| format!("Couldn't inspect sync data: {error}"))?;
            if !file_type.is_dir()
                || file_type.is_symlink()
                || !sorted_directory_entries(&namespace.path())?.is_empty()
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn provider_namespace_evidence(path: &Path) -> Result<ProviderNamespaceEvidence, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProviderNamespaceEvidence::LocalOnly);
        }
        Err(error) => return Err(format!("Couldn't inspect sync data: {error}")),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(ProviderNamespaceEvidence::SharedOrUnknown);
    }

    let entries = sorted_directory_entries(path)?;
    if entries.is_empty() {
        return Ok(ProviderNamespaceEvidence::LocalOnly);
    }
    if entries.len() != 1 || entries[0].file_name() != "shared" {
        return Ok(ProviderNamespaceEvidence::SharedOrUnknown);
    }
    let shared = &entries[0];
    let file_type = shared
        .file_type()
        .map_err(|error| format!("Couldn't inspect sync data: {error}"))?;
    if !file_type.is_dir() || file_type.is_symlink() {
        return Ok(ProviderNamespaceEvidence::SharedOrUnknown);
    }

    match inspect_shared_enrollment_for_cold_discovery(&shared.path()) {
        Ok(Some(_)) => Ok(ProviderNamespaceEvidence::SharedOrUnknown),
        // A canonical empty provider topology is made by a first local
        // activation, before any authority/share publication.  Any other
        // descriptor-inspection failure remains evidence, rather than being
        // guessed to be local-only.
        Ok(None) | Err(_) if is_empty_local_provider_scaffold(&shared.path())? => {
            Ok(ProviderNamespaceEvidence::LocalOnly)
        }
        Ok(None) | Err(_) => Ok(ProviderNamespaceEvidence::SharedOrUnknown),
    }
}

fn binding_names_shared_state(binding: &SparseV2Binding) -> bool {
    matches!(
        binding.availability(),
        SparseV2Availability::Retryable { stage, .. }
            if matches!(
                stage.as_str(),
                "share_prepared" | "joining" | "shared_active"
            )
    )
}

/// A shared or malformed provider namespace is a warning for the explicit
/// archive-and-return action, not a reason to strand the user in a refused
/// managed runtime.  The action preserves the complete private managed state
/// before Direct Files is installed, so the user can later inspect or recover
/// it.  The frontend requires an acknowledgement before invoking the command.
fn cancel_warning(binding: &SparseV2Binding, provider_namespace: &Path) -> Option<String> {
    let provider_evidence = provider_namespace_evidence(provider_namespace)
        .unwrap_or(ProviderNamespaceEvidence::SharedOrUnknown);
    let mut shared_or_unknown = binding_names_shared_state(binding)
        || provider_evidence == ProviderNamespaceEvidence::SharedOrUnknown;
    if let Some(handle) = binding.handle() {
        match handle.status() {
            Ok(status) => {
                shared_or_unknown |= status.shared_role.is_some()
                    || status.shared_phase.is_some()
                    // A local-only snapshot can retain its absent-provider
                    // sentinel.  Pending work matters here only when provider
                    // evidence says this is not the exact local scaffold.
                    || (status.provider_pending != 0
                        && provider_evidence == ProviderNamespaceEvidence::SharedOrUnknown);
            }
            // The warning is deliberately conservative, but inability to
            // inspect an already-refused runtime must not suppress the escape.
            Err(_) => shared_or_unknown = true,
        }
    }
    shared_or_unknown.then(|| {
        "Managed storage may contain shared or pending provider state. Tine will archive the complete private managed-storage state before reopening this graph with Direct files. Other devices will not receive further managed-storage updates from this device.".into()
    })
}

fn cancel_warning_for_observation(
    binding: &SparseV2Binding,
    provider_namespace: &Path,
    snapshot: &SyncRuntimeStatusSnapshot,
) -> Option<String> {
    let provider_evidence = provider_namespace_evidence(provider_namespace)
        .unwrap_or(ProviderNamespaceEvidence::SharedOrUnknown);
    let shared_or_unknown = binding_names_shared_state(binding)
        || provider_evidence == ProviderNamespaceEvidence::SharedOrUnknown
        || snapshot.shared_role.is_some()
        || snapshot.shared_phase.is_some()
        || (snapshot.provider_pending != 0
            && provider_evidence == ProviderNamespaceEvidence::SharedOrUnknown);
    shared_or_unknown.then(|| {
        "Managed storage may contain shared or pending provider state. Tine will archive the complete private managed-storage state before reopening this graph with Direct files. Other devices will not receive further managed-storage updates from this device.".into()
    })
}

#[derive(Default)]
pub(crate) struct SyncRuntimeFacade;

impl SyncRuntimeFacade {
    pub(crate) fn binding_record(
        &self,
        app: &tauri::AppHandle,
        graph_root: &Path,
    ) -> Result<Option<SparseV2ActivationRecord>, String> {
        let private = sparse_private_root(app, graph_root)?;
        let record = read_binding_at(&private.join(SPARSE_BINDING_FILE), graph_root)?;
        if record.is_none() && std::fs::symlink_metadata(&private).is_ok() {
            return Err(
                "Tine-managed storage data is incomplete, so this graph could not be opened safely."
                    .into(),
            );
        }
        Ok(record)
    }

    pub(crate) fn prepare_binding_record(
        &self,
        app: &tauri::AppHandle,
        graph_root: &Path,
        graph_meta: GraphMeta,
    ) -> Result<SparseV2ActivationRecord, String> {
        match self.binding_record(app, graph_root)? {
            Some(record) => Ok(record),
            None => Ok(SparseV2ActivationRecord::new(
                graph_root,
                graph_meta,
                DeviceId::from_uuid(crate::settings::managed_sync_device_id(app)?),
            )),
        }
    }

    pub(crate) fn persist_binding_record(
        &self,
        app: &tauri::AppHandle,
        record: &SparseV2ActivationRecord,
    ) -> Result<(), String> {
        let root = Path::new(&record.graph_root);
        persist_binding_at(&binding_path(app, root)?, record)
    }

    pub(crate) fn graph_meta(record: &SparseV2ActivationRecord) -> GraphMeta {
        record.graph_meta.clone()
    }

    pub(crate) fn open_record(
        &self,
        app: &tauri::AppHandle,
        record: &SparseV2ActivationRecord,
    ) -> Result<SparseV2Binding, String> {
        self.open_record_with_progress(app, record, |_| {})
    }

    /// Open a retained managed runtime while forwarding only bounded phase
    /// names and elapsed time to the caller.  Detailed recovery telemetry
    /// remains in the terminal diagnostic stream; it must not be copied into a
    /// startup webview before a graph slot exists.
    fn open_record_with_progress(
        &self,
        app: &tauri::AppHandle,
        record: &SparseV2ActivationRecord,
        mut progress: impl FnMut(SyncRuntimeOpenProgress),
    ) -> Result<SparseV2Binding, String> {
        crate::debug::diag("managed storage open: begin authenticated existing-state recovery");
        let opened = SyncRuntimeHandle::open_with_progress(record.open_request(app)?, |update| {
            match &update {
                SyncRuntimeOpenProgress::Phase { phase, elapsed } => crate::debug::diag(format!(
                    "managed storage open: phase={} elapsed_ms={}",
                    managed_open_phase_name(*phase),
                    elapsed.as_millis()
                )),
                SyncRuntimeOpenProgress::Waiting { phase, elapsed } => crate::debug::diag(format!(
                    "managed storage open: phase={} elapsed_ms={}",
                    managed_open_waiting_phase_name(*phase),
                    elapsed.as_millis()
                )),
                SyncRuntimeOpenProgress::RecoveryDiagnostics { diagnostics } => {
                    crate::debug::diag(format!(
                            "managed storage open: recovery={} retention={} retained_runs={} resume_candidate={} detached_bootstrap_reconstruction={} full_bootstrap_replay={} manifests={} manifest_enumeration_ms={} resume_selection_ms={} bootstrap_reconstruction_attempted={} bootstrap_reconstruction_ms={} engine_open_ms={} sqlite_open_ms={} tail_construction_ms={} total_ms={}",
                            diagnostics.recovery,
                            diagnostics.retention_plan,
                            diagnostics.retained_run_count,
                            diagnostics.resume_candidate,
                            u8::from(diagnostics.detached_bootstrap_reconstruction),
                            u8::from(diagnostics.full_bootstrap_replay),
                            diagnostics.manifest_count,
                            diagnostics.manifest_enumeration.as_millis(),
                            diagnostics.resume_selection.as_millis(),
                            u8::from(diagnostics.bootstrap_reconstruction.is_some()),
                            diagnostics.bootstrap_reconstruction.unwrap_or_default().as_millis(),
                            diagnostics.engine_open.as_millis(),
                            diagnostics.sqlite_open.as_millis(),
                            diagnostics.tail_construction.as_millis(),
                            diagnostics.total.as_millis(),
                        ));
                    crate::debug::diag(format!(
                            "managed storage open: projection recovery={} sidecar_shape_ms={} checkpoint_auth_ms={} read_only_open_ms={} schema_claim_ms={} structural_ms={} materialization_stamp_ms={} forensics_ms={} rebuild_ms={} applied_batches={} bulk_pages_materialized={} ancestry_full_scans={}",
                            diagnostics.projection_recovery,
                            diagnostics.projection_sidecar_shape.as_millis(),
                            diagnostics.projection_checkpoint_authentication.as_millis(),
                            diagnostics.projection_read_only_open.as_millis(),
                            diagnostics.projection_schema_and_claim.as_millis(),
                            diagnostics.projection_structural_validation.as_millis(),
                            diagnostics.projection_materialization_stamp.as_millis(),
                            diagnostics.projection_forensics_preservation.as_millis(),
                            diagnostics.projection_rebuild.as_millis(),
                            diagnostics.projection_applied_batches,
                            diagnostics.projection_bulk_pages_materialized,
                            diagnostics.projection_ancestry_full_scans,
                        ));
                    crate::debug::diag(format!(
                            "managed storage open: engine stages prepare_replay_ms={} predecessor_restore_ms={} bootstrap_part_replay_ms={} archived_tail_replay_ms={} finish_replay_ms={} bootstrap_parts_replayed={} archived_manifests_offered={} archived_manifests_replayed={} resume_adopted={} resume_refused={} replay_base_generation={} live_history_generation={} replayed_generations={}",
                            diagnostics.prepare_replay.as_millis(),
                            diagnostics.predecessor_restore.as_millis(),
                            diagnostics.bootstrap_part_replay.as_millis(),
                            diagnostics.archived_tail_replay.as_millis(),
                            diagnostics.finish_replay.as_millis(),
                            diagnostics.bootstrap_parts_replayed,
                            diagnostics.archived_manifests_offered,
                            diagnostics.archived_manifests_replayed,
                            diagnostics.resume_adopted,
                            diagnostics.resume_refused,
                            diagnostics.replay_base_generation,
                            diagnostics.live_history_generation,
                            diagnostics.replayed_generations,
                        ));
                }
            }
            progress(update);
        });
        crate::debug::diag(format!(
            "managed storage open: completed outcome={}",
            managed_open_outcome_code(&opened.status)
        ));
        Ok(SparseV2Binding::from_open(opened))
    }

    /// The startup graph-open path has no `GraphSlot` until this operation
    /// succeeds.  Publish the same bounded progress vocabulary directly to
    /// that webview so a long recovery is visible without granting it any
    /// additional storage authority.
    pub(crate) fn open_record_for_window(
        &self,
        app: &tauri::AppHandle,
        label: &str,
        record: &SparseV2ActivationRecord,
    ) -> Result<SparseV2Binding, String> {
        let reporter = crate::graph::StartupProgressReporter::for_window(app, label);
        reporter.phase("managed_open.entry");
        let result = self.open_record_with_progress(app, record, |progress| match progress {
            SyncRuntimeOpenProgress::Phase { phase, .. } => {
                reporter.phase(managed_open_phase_name(phase));
            }
            SyncRuntimeOpenProgress::Waiting { phase, .. } => {
                reporter.phase(managed_open_waiting_phase_name(phase));
            }
            // The detailed receipt remains terminal-only.  The webview sees a
            // bounded phase update, never counts, error strings, or paths.
            SyncRuntimeOpenProgress::RecoveryDiagnostics { .. } => {
                reporter.phase("managed_open.recovery_diagnostics");
            }
        });
        reporter.terminal(
            "managed_open.complete",
            if result.is_ok() { "ok" } else { "error" },
        );
        result
    }

    pub(crate) fn activate_record(
        &self,
        app: &tauri::AppHandle,
        record: &SparseV2ActivationRecord,
    ) -> Result<SparseV2Binding, String> {
        self.activate_record_with_progress(app, record, |_| {})
    }

    pub(crate) fn activate_record_with_progress(
        &self,
        app: &tauri::AppHandle,
        record: &SparseV2ActivationRecord,
        progress: impl FnMut(SyncLocalActivationPhase),
    ) -> Result<SparseV2Binding, String> {
        Ok(SparseV2Binding::from_activation(
            SyncRuntimeHandle::activate_or_resume_local_with_progress(
                record.activation_request(app)?,
                progress,
            ),
        ))
    }

    pub(crate) fn activate_record_with_detailed_progress(
        &self,
        app: &tauri::AppHandle,
        record: &SparseV2ActivationRecord,
        progress: impl FnMut(SyncLocalActivationProgress),
    ) -> Result<SparseV2Binding, String> {
        Ok(SparseV2Binding::from_activation(
            SyncRuntimeHandle::activate_or_resume_local_with_detailed_progress(
                record.activation_request(app)?,
                progress,
            ),
        ))
    }

    #[cfg(test)]
    fn open_explicit(&self, request: SyncRuntimeOpenRequest) -> SyncRuntimeOpenResult {
        SyncRuntimeHandle::open(request)
    }
}

fn managed_open_phase_name(phase: SyncRuntimeOpenPhase) -> &'static str {
    match phase {
        SyncRuntimeOpenPhase::RetainingGraph => "managed_open.retaining_graph",
        SyncRuntimeOpenPhase::DiscoveringEnrollment => "managed_open.discovering_enrollment",
        SyncRuntimeOpenPhase::OpeningActorGraph => "managed_open.opening_actor_graph",
        SyncRuntimeOpenPhase::RevalidatingEnrollment => "managed_open.revalidating_enrollment",
        SyncRuntimeOpenPhase::OpeningEnrollment => "managed_open.opening_enrollment",
        SyncRuntimeOpenPhase::OpeningProjectionReceipts => {
            "managed_open.opening_projection_receipts"
        }
        SyncRuntimeOpenPhase::OpeningReconciliationBaseline => {
            "managed_open.opening_reconciliation_baseline"
        }
        SyncRuntimeOpenPhase::RecoveringPromotedRuntime => {
            "managed_open.recovering_promoted_runtime"
        }
        SyncRuntimeOpenPhase::AssemblingActor => "managed_open.assembling_actor",
    }
}

fn managed_open_waiting_phase_name(phase: SyncRuntimeOpenPhase) -> &'static str {
    match phase {
        SyncRuntimeOpenPhase::RetainingGraph => "managed_open.waiting_retaining_graph",
        SyncRuntimeOpenPhase::DiscoveringEnrollment => {
            "managed_open.waiting_discovering_enrollment"
        }
        SyncRuntimeOpenPhase::OpeningActorGraph => "managed_open.waiting_opening_actor_graph",
        SyncRuntimeOpenPhase::RevalidatingEnrollment => {
            "managed_open.waiting_revalidating_enrollment"
        }
        SyncRuntimeOpenPhase::OpeningEnrollment => "managed_open.waiting_opening_enrollment",
        SyncRuntimeOpenPhase::OpeningProjectionReceipts => {
            "managed_open.waiting_opening_projection_receipts"
        }
        SyncRuntimeOpenPhase::OpeningReconciliationBaseline => {
            "managed_open.waiting_opening_reconciliation_baseline"
        }
        SyncRuntimeOpenPhase::RecoveringPromotedRuntime => {
            "managed_open.waiting_recovering_promoted_runtime"
        }
        SyncRuntimeOpenPhase::AssemblingActor => "managed_open.waiting_assembling_actor",
    }
}

/// A safe, bounded terminal code for unconditional managed-open diagnostics.
/// `SyncRuntimeOpenStatus` also carries user/storage details for normal command
/// replies; those details must never be formatted into the startup trace.
fn managed_open_outcome_code(status: &SyncRuntimeOpenStatus) -> &'static str {
    match status {
        SyncRuntimeOpenStatus::LegacyDefault => "legacy_default",
        SyncRuntimeOpenStatus::Absent => "absent",
        SyncRuntimeOpenStatus::ExistingNonActive(SyncNonActiveStage::ShadowImport) => {
            "existing_shadow_import"
        }
        SyncRuntimeOpenStatus::ExistingNonActive(SyncNonActiveStage::VerifiedLocal) => {
            "existing_verified_local"
        }
        SyncRuntimeOpenStatus::Blocked { .. } => "blocked",
        SyncRuntimeOpenStatus::UnsupportedOrIncompatible(SyncRuntimeComponent::Enrollment) => {
            "unsupported_enrollment"
        }
        SyncRuntimeOpenStatus::UnsupportedOrIncompatible(SyncRuntimeComponent::Archive) => {
            "unsupported_archive"
        }
        SyncRuntimeOpenStatus::CorruptOrUnreadable(SyncRuntimeComponent::Enrollment) => {
            "corrupt_enrollment"
        }
        SyncRuntimeOpenStatus::CorruptOrUnreadable(SyncRuntimeComponent::Archive) => {
            "corrupt_archive"
        }
        SyncRuntimeOpenStatus::AmbiguousOrForeignResidue(
            SyncAmbiguousEvidence::EnrollmentResidue,
        ) => "ambiguous_enrollment_residue",
        SyncRuntimeOpenStatus::AmbiguousOrForeignResidue(
            SyncAmbiguousEvidence::EnrollmentNamespace,
        ) => "ambiguous_enrollment_namespace",
        SyncRuntimeOpenStatus::AmbiguousOrForeignResidue(
            SyncAmbiguousEvidence::EnrollmentGraphBinding,
        ) => "ambiguous_enrollment_graph_binding",
        SyncRuntimeOpenStatus::AmbiguousOrForeignResidue(SyncAmbiguousEvidence::ArchiveResidue) => {
            "ambiguous_archive_residue"
        }
        SyncRuntimeOpenStatus::AmbiguousOrForeignResidue(
            SyncAmbiguousEvidence::ArchiveNamespace,
        ) => "ambiguous_archive_namespace",
        SyncRuntimeOpenStatus::AmbiguousOrForeignResidue(SyncAmbiguousEvidence::ArchiveBinding) => {
            "ambiguous_archive_binding"
        }
        SyncRuntimeOpenStatus::AmbiguousOrForeignResidue(
            SyncAmbiguousEvidence::ActiveArchiveMismatch,
        ) => "ambiguous_active_archive_mismatch",
        SyncRuntimeOpenStatus::Active => "active",
        SyncRuntimeOpenStatus::OpenRefused { .. } => "open_refused",
    }
}

const LEGACY_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const ACTIVATION_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const ACTIVATION_PROGRESS_EVENT: &str = "sparse-v2-activation-progress";
pub(crate) const SPARSE_V2_NOT_ACTIVE: &str =
    "Tine-managed storage is not ready. Retry setup or return to Direct files.";

struct ActivationHeartbeat {
    stop: mpsc::Sender<()>,
    join: Option<JoinHandle<()>>,
}

fn latest_activation_progress_name(
    latest_progress: &Arc<Mutex<Option<SyncLocalActivationProgress>>>,
) -> String {
    latest_progress
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(SyncLocalActivationProgress::diagnostic_name)
        .unwrap_or_else(|| "core bootstrap setup".into())
}

impl ActivationHeartbeat {
    fn start(
        started: Instant,
        latest_progress: Arc<Mutex<Option<SyncLocalActivationProgress>>>,
    ) -> Self {
        let (stop, stopped) = mpsc::channel();
        let join = std::thread::spawn(move || loop {
            match stopped.recv_timeout(ACTIVATION_HEARTBEAT_INTERVAL) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let progress = latest_activation_progress_name(&latest_progress);
                    crate::debug::diag(format!(
                        "sparse-v2 activation heartbeat after {} ms: progress={progress}",
                        started.elapsed().as_millis()
                    ));
                }
            }
        });
        Self {
            stop,
            join: Some(join),
        }
    }
}

impl Drop for ActivationHeartbeat {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
struct SparseV2ActivationProgressEvent {
    binding_generation: u64,
    progress: SyncLocalActivationProgress,
}

fn activate_record_with_diagnostics(
    facade: &SyncRuntimeFacade,
    app: &tauri::AppHandle,
    label: &str,
    binding_generation: u64,
    record: &SparseV2ActivationRecord,
) -> Result<SparseV2Binding, String> {
    let started = Instant::now();
    let latest_progress = Arc::new(Mutex::new(None));
    let heartbeat = ActivationHeartbeat::start(started, Arc::clone(&latest_progress));
    let result = facade.activate_record_with_detailed_progress(app, record, |progress| {
        let diagnostic = progress.diagnostic_name();
        let _ = app.emit_to(
            label,
            ACTIVATION_PROGRESS_EVENT,
            SparseV2ActivationProgressEvent {
                binding_generation,
                progress: progress.clone(),
            },
        );
        *latest_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(progress);
        crate::debug::diag(format!(
            "sparse-v2 activation progress after {} ms: {diagnostic}",
            started.elapsed().as_millis(),
        ));
    });
    drop(heartbeat);
    result
}

pub(crate) fn active_handle(
    slot: &crate::state::GraphSlot,
) -> Result<&tine_core::sync_runtime::SyncRuntimeHandle, String> {
    slot.sparse_runtime()
        .ok_or_else(|| SPARSE_V2_NOT_ACTIVE.to_string())
}

fn sparse_v2_status_for_slot(slot: &crate::state::GraphSlot) -> Result<SparseV2StatusDto, String> {
    let mut status = match slot.sparse_binding() {
        Some(binding) => {
            let mut status = SparseV2StatusDto::from_binding(binding, slot.binding_generation);
            // Once the binding itself belongs to this graph, the explicit
            // archive-and-return command is always available.  Shared/pending
            // state is surfaced as a confirmation warning, never as a lockout.
            status.can_cancel = true;
            status.cancel_reason = cancel_warning(binding, &slot.root_key.join(".tine-sync/v2"));
            status
        }
        None => {
            match inspect_shared_enrollment_for_cold_discovery(
                &slot.root_key.join(".tine-sync/v2/shared"),
            )? {
                Some(descriptor) => {
                    SparseV2StatusDto::joinable(slot.binding_generation, &descriptor)
                }
                None => SparseV2StatusDto::legacy(slot.binding_generation),
            }
        }
    };
    // Status names describe enrollment/recovery. This record describes the
    // exact writer retained by the slot that will service `save_page`.
    status.application_page_admission = slot.application_page_admission();
    Ok(status)
}

fn sparse_v2_status_for_observation(
    slot: &crate::state::GraphSlot,
    snapshot: SyncRuntimeStatusSnapshot,
) -> Result<SparseV2StatusDto, String> {
    let binding = slot
        .sparse_binding()
        .ok_or_else(|| "managed cross-page move recovery requires managed storage".to_owned())?;
    let cancel_reason =
        cancel_warning_for_observation(binding, &slot.root_key.join(".tine-sync/v2"), &snapshot);
    let mut status =
        SparseV2StatusDto::from_binding_observation(binding, slot.binding_generation, snapshot);
    status.can_cancel = true;
    status.cancel_reason = cancel_reason;
    Ok(status)
}

fn move_outcome_episode_id(outcome: &SyncApplicationMoveSubtreesOutcome) -> &str {
    match outcome {
        SyncApplicationMoveSubtreesOutcome::Committed { episode_id, .. }
        | SyncApplicationMoveSubtreesOutcome::NoCommit { episode_id, .. }
        | SyncApplicationMoveSubtreesOutcome::Deferred { episode_id, .. } => episode_id,
    }
}

fn move_recovery_result(
    previous_binding_generation: u64,
    slot: &crate::state::GraphSlot,
    expected_episode_id: &str,
    outcome: SyncApplicationMoveSubtreesOutcome,
    snapshot: SyncRuntimeStatusSnapshot,
) -> Result<crate::commands::ManagedApplicationMoveSubtreesRecoveryResult, String> {
    if move_outcome_episode_id(&outcome) != expected_episode_id {
        return Err("managed cross-page move recovery returned a different episode".into());
    }
    if snapshot.lifecycle != SyncRuntimeLifecycle::Active {
        return Err(
            "managed cross-page move recovery requires process reopen after a terminal runtime"
                .into(),
        );
    }
    let status = sparse_v2_status_for_observation(slot, snapshot)?;
    let application_page_admission = status.application_page_admission.clone();
    Ok(
        crate::commands::ManagedApplicationMoveSubtreesRecoveryResult {
            previous_binding_generation,
            binding_generation: slot.binding_generation,
            status,
            application_page_admission,
            episode_id: expected_episode_id.to_owned(),
            outcome,
        },
    )
}

fn recover_managed_application_subtrees_with(
    state: &crate::state::AppState,
    label: &str,
    binding_generation: u64,
    request: SyncApplicationMoveSubtreesRequest,
    reopen: impl FnOnce(&Path) -> Result<(SparseV2Binding, tine_core::model::GraphMeta), String>,
) -> Result<crate::commands::ManagedApplicationMoveSubtreesRecoveryResult, String> {
    let _transition = state.graph_load.lock().unwrap();
    let predecessor = crate::state::slot_for_bound_window(state, label, Some(binding_generation))?;
    let root = predecessor.root_key.clone();
    let action = predecessor
        .sparse_binding()
        .ok_or_else(|| "managed cross-page move recovery requires managed storage".to_owned())?
        .action();

    match action {
        SparseV2BindingAction::ReturnRetained => {
            let handle = predecessor.sparse_runtime().ok_or_else(|| {
                "managed cross-page move recovery has no retained actor".to_owned()
            })?;
            let observation = handle
                .resolve_application_move_subtrees(request.clone())
                .map_err(|error| error.to_string())?;
            let result = move_recovery_result(
                binding_generation,
                &predecessor,
                &request.episode_id,
                observation.move_outcome,
                observation.runtime_snapshot,
            )?;
            crate::state::poke_watcher(state);
            Ok(result)
        }
        SparseV2BindingAction::ReopenActive => {
            let (binding, graph_meta) = reopen(&root)?;
            let successor = Arc::new(crate::state::GraphSlot::from_sparse_v2(
                binding,
                root.clone(),
                graph_meta,
            ));
            let observation = successor
                .sparse_runtime()
                .ok_or_else(|| {
                    "managed cross-page move recovery could not reopen an active actor".to_owned()
                })?
                .resolve_application_move_subtrees(request.clone())
                .map_err(|error| error.to_string())?;
            let result = move_recovery_result(
                binding_generation,
                &successor,
                &request.episode_id,
                observation.move_outcome,
                observation.runtime_snapshot,
            )?;

            state.graphs.write().unwrap().replace_if_current(
                label,
                binding_generation,
                &root,
                Arc::clone(&successor),
            )?;
            crate::state::poke_watcher(state);
            Ok(result)
        }
        SparseV2BindingAction::ActivateOrResume => {
            Err("managed cross-page move recovery cannot activate an incomplete runtime".into())
        }
    }
}

pub(crate) fn recover_managed_application_subtrees_blocking(
    app: &tauri::AppHandle,
    label: &str,
    binding_generation: u64,
    request: SyncApplicationMoveSubtreesRequest,
) -> Result<crate::commands::ManagedApplicationMoveSubtreesRecoveryResult, String> {
    let state = app.state::<crate::state::AppState>();
    recover_managed_application_subtrees_with(&state, label, binding_generation, request, |root| {
        let record = state
            .sync_runtime
            .binding_record(app, root)?
            .ok_or_else(|| "Tine-managed storage setup is missing.".to_owned())?;
        let graph_meta = SyncRuntimeFacade::graph_meta(&record);
        let binding = state.sync_runtime.open_record(app, &record)?;
        Ok((binding, graph_meta))
    })
}

#[tauri::command]
pub(crate) async fn sparse_v2_status(
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2StatusDto, String> {
    let (app, label, binding_generation) = crate::state::owned_graph_context(state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<crate::state::AppState>();
        let slot = crate::state::slot_for_bound_window(&state, &label, Some(binding_generation))?;
        sparse_v2_status_for_slot(&slot)
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Explicitly retire one legacy authority and activate/resume sparse v2.
///
/// The durable opt-in record is published only after the legacy watcher,
/// detached background work, and every in-flight legacy command have released
/// their tracked graph leases. Once the record exists, every result (including
/// retryable/blocked) is published as sparse authority; there is no writer
/// fallback.
#[tauri::command]
pub(crate) async fn activate_sparse_v2(
    app: tauri::AppHandle,
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2StatusDto, String> {
    let label = state.window.label().to_string();
    let binding_generation = state.binding_generation.ok_or("missing-graph-binding")?;
    drop(state);
    tauri::async_runtime::spawn_blocking(move || {
        activate_sparse_v2_blocking(&app, &label, binding_generation)
    })
    .await
    .map_err(|error| format!("Tine-managed storage setup worker failed: {error}"))?
}

fn activate_sparse_v2_blocking(
    app: &tauri::AppHandle,
    label: &str,
    binding_generation: u64,
) -> Result<SparseV2StatusDto, String> {
    let started = Instant::now();
    let state = app.state::<crate::state::AppState>();
    crate::debug::diag("sparse-v2 activation requested");
    let _transition = state.graph_load.lock().unwrap();
    let slot = crate::state::slot_for_bound_window(&state, label, Some(binding_generation))?;
    let root = slot.root_key.clone();

    if let Some(binding) = slot.sparse_binding() {
        let action = binding.action();
        crate::debug::diag(format!(
            "sparse-v2 activation resuming retained authority: action={action:?}, availability={:?}",
            binding.availability()
        ));
        if action == SparseV2BindingAction::ReturnRetained {
            let result = sparse_v2_status_for_slot(&slot);
            crate::debug::diag(format!(
                "sparse-v2 retained activation completed after {} ms: {result:?}",
                started.elapsed().as_millis()
            ));
            return result;
        }
        let record = state
            .sync_runtime
            .binding_record(app, &root)?
            .ok_or("Tine-managed storage setup is missing.")?;
        let graph_meta = SyncRuntimeFacade::graph_meta(&record);
        let core_started = Instant::now();
        let binding = match action {
            SparseV2BindingAction::ReopenActive => state.sync_runtime.open_record(app, &record)?,
            SparseV2BindingAction::ActivateOrResume => activate_record_with_diagnostics(
                &state.sync_runtime,
                app,
                label,
                binding_generation,
                &record,
            )?,
            SparseV2BindingAction::ReturnRetained => {
                unreachable!("retained bindings return before replacement")
            }
        };
        crate::debug::diag(format!(
            "sparse-v2 retained core operation completed after {} ms: availability={:?}",
            core_started.elapsed().as_millis(),
            binding.availability()
        ));
        let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
            binding, root, graph_meta,
        ));
        state
            .graphs
            .write()
            .unwrap()
            .bind(label.to_string(), Arc::clone(&replacement))?;
        crate::state::poke_watcher(&state);
        let result = sparse_v2_status_for_slot(&replacement);
        crate::debug::diag(format!(
            "sparse-v2 retained activation published after {} ms: {result:?}",
            started.elapsed().as_millis()
        ));
        return result;
    }

    let graph = slot.legacy_graph()?;
    let graph_meta = graph.meta();
    drop(graph);
    let record = state
        .sync_runtime
        .prepare_binding_record(app, &root, graph_meta.clone())?;
    crate::debug::diag(format!(
        "sparse-v2 fresh activation prepared private binding after {} ms",
        started.elapsed().as_millis()
    ));

    slot.begin_legacy_retirement()?;
    crate::debug::diag("sparse-v2 legacy authority retirement started");
    let removed = state.graphs.write().unwrap().remove(label);
    if removed
        .as_ref()
        .is_none_or(|removed| removed.binding_generation != slot.binding_generation)
    {
        slot.cancel_legacy_retirement()?;
        return Err(
            "The graph changed while Tine-managed storage was being set up. Retry setup.".into(),
        );
    }
    crate::state::poke_watcher(&state);

    if let Err(error) = slot.wait_for_legacy_drain(LEGACY_DRAIN_TIMEOUT) {
        crate::debug::diag(format!(
            "sparse-v2 legacy authority drain failed after {} ms: {error}",
            started.elapsed().as_millis()
        ));
        slot.cancel_legacy_retirement()?;
        state
            .graphs
            .write()
            .unwrap()
            .bind(label.to_string(), Arc::clone(&slot))?;
        crate::state::poke_watcher(&state);
        return Err(format!(
            "Tine-managed storage setup can be retried: {error}"
        ));
    }
    crate::debug::diag(format!(
        "sparse-v2 legacy authority drained after {} ms",
        started.elapsed().as_millis()
    ));

    if let Err(error) = state.sync_runtime.persist_binding_record(app, &record) {
        crate::debug::diag(format!(
            "sparse-v2 private binding persistence failed after {} ms: {error}",
            started.elapsed().as_millis()
        ));
        slot.cancel_legacy_retirement()?;
        state
            .graphs
            .write()
            .unwrap()
            .bind(label.to_string(), Arc::clone(&slot))?;
        crate::state::poke_watcher(&state);
        return Err(error);
    }
    crate::debug::diag(format!(
        "sparse-v2 private binding persisted after {} ms; starting core bootstrap",
        started.elapsed().as_millis()
    ));

    let core_started = Instant::now();
    let binding = activate_record_with_diagnostics(
        &state.sync_runtime,
        app,
        label,
        binding_generation,
        &record,
    )?;
    crate::debug::diag(format!(
        "sparse-v2 core bootstrap completed after {} ms: availability={:?}",
        core_started.elapsed().as_millis(),
        binding.availability()
    ));
    let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
        binding, root, graph_meta,
    ));
    state
        .graphs
        .write()
        .unwrap()
        .bind(label.to_string(), Arc::clone(&replacement))?;
    crate::state::poke_watcher(&state);
    let result = sparse_v2_status_for_slot(&replacement);
    crate::debug::diag(format!(
        "sparse-v2 fresh activation published after {} ms: {result:?}",
        started.elapsed().as_millis()
    ));
    result
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct SparseV2CancelResult {
    status: SparseV2StatusDto,
    binding_generation: u64,
    recovery_statement: String,
}

/// Cold startup recovery intentionally has the same public result shape as an
/// in-app Direct Files return.  The caller's attempt is validated at the
/// mutation boundary, not trusted as storage authority or reflected to the UI.
pub(crate) type SparseV2ColdCancelResult = SparseV2CancelResult;

fn archive_private_root(
    private_root: &Path,
    recovery_root: &Path,
) -> Result<Option<PathBuf>, String> {
    let metadata = match std::fs::symlink_metadata(private_root) {
        Ok(metadata) => metadata,
        // A failed or partially-created activation need not have retained any
        // app-private state.  There is then nothing to preserve, not a reason
        // to strand the user.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Couldn't inspect Tine-managed storage recovery state: {error}"
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err("Tine-managed storage recovery state is a symbolic link, so it could not be archived safely.".into());
    }
    std::fs::create_dir_all(recovery_root).map_err(|error| {
        format!("Couldn't prepare Tine-managed storage recovery state: {error}")
    })?;
    let recovery_metadata = std::fs::symlink_metadata(recovery_root).map_err(|error| {
        format!("Couldn't inspect Tine-managed storage recovery state: {error}")
    })?;
    if !recovery_metadata.is_dir() || recovery_metadata.file_type().is_symlink() {
        return Err("Tine-managed storage recovery state is not a local directory, so it could not be archived safely.".into());
    }
    let key = private_root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("Tine-managed storage recovery state has no valid local key.")?;
    let destination = recovery_root.join(format!("{key}-{}", Uuid::new_v4()));
    std::fs::rename(private_root, &destination).map_err(|error| {
        format!("Couldn't preserve Tine-managed storage recovery state: {error}")
    })?;
    Ok(Some(destination))
}

#[derive(Debug)]
enum ProviderNamespaceArchive {
    Absent,
    Moved {
        source: PathBuf,
        destination: PathBuf,
    },
}

/// Preserve graph-local provider state outside the live `.tine-sync/v2`
/// namespace.  A Direct Files restart deliberately refuses an unclaimed v2
/// namespace, so archiving only private app-data would leave a delayed
/// lockout.  This is a same-filesystem rename, never a delete or copy.
fn archive_graph_provider_namespace(graph_root: &Path) -> Result<ProviderNamespaceArchive, String> {
    let source = graph_root.join(".tine-sync/v2");
    let metadata = match std::fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProviderNamespaceArchive::Absent);
        }
        Err(error) => {
            return Err(format!(
                "Couldn't inspect graph-local managed-storage state before returning to Direct files: {error}"
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err("Graph-local managed-storage state is a symbolic link, so it could not be archived safely.".into());
    }
    let tine_sync = source
        .parent()
        .ok_or("Graph-local managed-storage state has no .tine-sync parent.")?;
    let parent_metadata = std::fs::symlink_metadata(tine_sync).map_err(|error| {
        format!(
            "Couldn't inspect graph-local managed-storage parent before returning to Direct files: {error}"
        )
    })?;
    if !parent_metadata.is_dir() || parent_metadata.file_type().is_symlink() {
        return Err("Graph-local managed-storage parent is not a local directory, so it could not be archived safely.".into());
    }
    let recovery = tine_sync.join("recovery");
    match std::fs::create_dir(&recovery) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(format!(
                "Couldn't prepare graph-local managed-storage recovery state: {error}"
            ));
        }
    }
    let recovery_metadata = std::fs::symlink_metadata(&recovery).map_err(|error| {
        format!("Couldn't inspect graph-local managed-storage recovery state: {error}")
    })?;
    if !recovery_metadata.is_dir() || recovery_metadata.file_type().is_symlink() {
        return Err("Graph-local managed-storage recovery state is not a local directory, so it could not be archived safely.".into());
    }
    let destination = recovery.join(format!("v2-{}", Uuid::new_v4()));
    std::fs::rename(&source, &destination)
        .map_err(|error| format!("Couldn't preserve graph-local managed-storage state: {error}"))?;
    Ok(ProviderNamespaceArchive::Moved {
        source,
        destination,
    })
}

fn restore_graph_provider_namespace(archive: ProviderNamespaceArchive) -> Result<(), String> {
    let ProviderNamespaceArchive::Moved {
        source,
        destination,
    } = archive
    else {
        return Ok(());
    };
    std::fs::rename(&destination, &source).map_err(|error| {
        format!(
            "Tine-managed storage could not restore graph-local provider state after preserving private recovery state failed: {error}"
        )
    })
}

#[derive(Debug)]
enum DirectFilesShutdown {
    Clean,
    Forced { detail: String },
}

impl DirectFilesShutdown {
    fn retry_detail(&self) -> String {
        match self {
            Self::Clean => "Tine-managed storage stopped before returning to Direct files, but archival did not complete. Retry setup can reopen the retained state.".into(),
            Self::Forced { detail } => format!(
                "Tine-managed storage was explicitly stopped without a clean drain ({detail}), and archival did not complete. Retry setup can reopen the retained state; any in-memory managed edits that had not reached durable storage may be absent."
            ),
        }
    }

    fn completion_note(&self) -> Option<&str> {
        match self {
            Self::Clean => None,
            Self::Forced { .. } => Some(
                "Managed storage could not complete a clean drain before this confirmed return. Its retained state was archived, but any in-memory managed edits that had not reached durable storage may be absent.",
            ),
        }
    }
}

/// Attempt the normal drain first.  A confirmed return to Direct Files is an
/// explicit escape hatch, so a refusal does not leave the user trapped: the
/// actor is then crash-stopped and joined before any managed files move.
fn shutdown_for_direct_files_escape(
    slot: &crate::state::GraphSlot,
) -> Result<DirectFilesShutdown, String> {
    let Some(handle) = slot.sparse_runtime() else {
        return Ok(DirectFilesShutdown::Clean);
    };
    match handle.clean_shutdown() {
        Ok(SyncShutdownOutcome::Safe(_)) => Ok(DirectFilesShutdown::Clean),
        Ok(SyncShutdownOutcome::Terminal(snapshot)) => {
            let detail = snapshot
                .detail
                .unwrap_or_else(|| "the managed actor reached a terminal state".into());
            handle.stop_without_clean_drain().map_err(|error| {
                format!("Tine-managed storage could not stop for the confirmed Direct Files return: {error}")
            })?;
            Ok(DirectFilesShutdown::Forced { detail })
        }
        Err(error) => {
            let detail = error.to_string();
            handle.stop_without_clean_drain().map_err(|stop| {
                format!("Tine-managed storage could not stop for the confirmed Direct Files return after its clean drain failed ({detail}): {stop}")
            })?;
            Ok(DirectFilesShutdown::Forced { detail })
        }
    }
}

fn publish_retryable_sparse_slot(
    state: &crate::state::AppState,
    label: &str,
    root_key: PathBuf,
    graph_meta: GraphMeta,
    detail: String,
) -> Result<Arc<crate::state::GraphSlot>, String> {
    let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
        retryable_binding("local_active", detail),
        root_key,
        graph_meta,
    ));
    state
        .graphs
        .write()
        .unwrap()
        .bind(label.to_string(), Arc::clone(&replacement))?;
    crate::state::poke_watcher(state);
    Ok(replacement)
}

fn cancel_sparse_v2_at_paths_with_archive_and_publish(
    state: &crate::state::AppState,
    label: &str,
    slot: Arc<crate::state::GraphSlot>,
    private_root: &Path,
    recovery_root: &Path,
    approved_assets: Option<&Path>,
    shutdown: impl FnOnce(&crate::state::GraphSlot) -> Result<DirectFilesShutdown, String>,
    archive: impl FnOnce(&Path, &Path) -> Result<Option<PathBuf>, String>,
    publish_direct: impl FnOnce(&Path, Option<&Path>) -> Result<u64, String>,
) -> Result<SparseV2CancelResult, String> {
    slot.sparse_binding()
        .ok_or("This graph is already using Direct files.")?;
    // The slot is the live, exact graph binding.  Explicit recovery must not
    // require parsing a possibly-corrupt or absent private binding merely to
    // learn a path we already own.
    let direct_root = slot.root_key.clone();
    let graph_meta = slot.graph_meta();
    let removed = state.graphs.write().unwrap().remove(label);
    if removed.is_some() {
        crate::state::poke_watcher(state);
    }
    if removed.as_ref().is_none_or(|current| {
        current.binding_generation != slot.binding_generation || current.root_key != slot.root_key
    }) {
        if let Some(current) = removed {
            state
                .graphs
                .write()
                .unwrap()
                .bind(label.to_string(), current)?;
            crate::state::poke_watcher(state);
        }
        return Err("The graph changed while returning to Direct files. Try again.".into());
    }

    let shutdown = match shutdown(&slot) {
        Ok(shutdown) => shutdown,
        Err(error) => {
            // No archive has started; the live slot remains usable when a
            // force-stop itself could not be completed.
            state
                .graphs
                .write()
                .unwrap()
                .bind(label.to_string(), slot)
                .map_err(|restore| {
                    format!(
                        "{error}; Tine-managed storage could not be restored in memory: {restore}"
                    )
                })?;
            crate::state::poke_watcher(state);
            return Err(error);
        }
    };

    // `clean_shutdown` consumes the live actor even when it succeeds.  If a
    // later archive step fails, re-publishing that old slot would advertise a
    // dead handle.  Publish a fresh no-handle retry route, then release every
    // reference to the retired actor before touching its storage.
    let retryable = publish_retryable_sparse_slot(
        state,
        label,
        direct_root.clone(),
        graph_meta,
        shutdown.retry_detail(),
    )?;
    let retry_generation = retryable.binding_generation;
    drop(removed);
    drop(slot);

    let provider_archive = match archive_graph_provider_namespace(&direct_root) {
        Ok(archive) => archive,
        Err(error) => return Err(error),
    };
    if let Err(error) = archive(private_root, recovery_root) {
        let reason = match restore_graph_provider_namespace(provider_archive) {
            Ok(()) => error,
            Err(restore) => format!("{error}; {restore}"),
        };
        return Err(reason);
    }

    let binding_generation = publish_direct(&direct_root, approved_assets).map_err(|error| {
        // Before Direct publication the retryable no-actor slot remains the
        // only authority.  Remove only that slot; if a later lifecycle step
        // already published Direct Files, leave its usable binding intact.
        let removed = {
            let mut graphs = state.graphs.write().unwrap();
            graphs
                .slot(label)
                .is_some_and(|slot| slot.binding_generation == retry_generation)
                .then(|| graphs.remove(label))
                .flatten()
        };
        if removed.is_some() {
            crate::state::poke_watcher(state);
        }
        format!(
            "Tine-managed storage recovery state was preserved, but Direct files could not reopen: {error}. Restart Tine to reopen the unchanged Markdown/Org graph."
        )
    })?;
    let status = SparseV2StatusDto::legacy(binding_generation);
    Ok(SparseV2CancelResult {
        binding_generation,
        status,
        recovery_statement: match shutdown.completion_note() {
            Some(note) => format!(
                "Direct file mode is active. Complete managed-storage recovery state was preserved. {note}"
            ),
            None => "Direct file mode is active. Complete managed-storage recovery state was preserved.".into(),
        },
    })
}

fn cancel_sparse_v2_at_paths_with_archive(
    state: &crate::state::AppState,
    label: &str,
    slot: Arc<crate::state::GraphSlot>,
    private_root: &Path,
    recovery_root: &Path,
    approved_assets: Option<&Path>,
    shutdown: impl FnOnce(&crate::state::GraphSlot) -> Result<DirectFilesShutdown, String>,
    archive: impl FnOnce(&Path, &Path) -> Result<Option<PathBuf>, String>,
) -> Result<SparseV2CancelResult, String> {
    cancel_sparse_v2_at_paths_with_archive_and_publish(
        state,
        label,
        slot,
        private_root,
        recovery_root,
        approved_assets,
        shutdown,
        archive,
        |direct_root, approved_assets| {
            let graph =
                tine_core::model::Graph::open_checked_with_assets(direct_root, approved_assets)
                    .map_err(|error| error.to_string())?;
            let replacement = Arc::new(crate::state::GraphSlot::new(
                graph,
                direct_root.to_path_buf(),
            ));
            let binding_generation = replacement.binding_generation;
            state
                .graphs
                .write()
                .unwrap()
                .bind(label.to_string(), replacement)?;
            crate::state::poke_watcher(state);
            Ok(binding_generation)
        },
    )
}

fn cancel_sparse_v2_at_paths(
    state: &crate::state::AppState,
    label: &str,
    slot: Arc<crate::state::GraphSlot>,
    private_root: &Path,
    recovery_root: &Path,
    approved_assets: Option<&Path>,
    shutdown: impl FnOnce(&crate::state::GraphSlot) -> Result<DirectFilesShutdown, String>,
) -> Result<SparseV2CancelResult, String> {
    cancel_sparse_v2_at_paths_with_archive(
        state,
        label,
        slot,
        private_root,
        recovery_root,
        approved_assets,
        shutdown,
        archive_private_root,
    )
}

fn cold_recovery_graph_meta(private_root: &Path, root_key: &Path) -> Result<GraphMeta, String> {
    match read_binding_at(&private_root.join(SPARSE_BINDING_FILE), root_key) {
        Ok(Some(record)) => Ok(SyncRuntimeFacade::graph_meta(&record)),
        // A second device is expected to have shared graph-local state before
        // it has any private binding.  The explicit Direct Files escape also
        // has to work when a failed setup left that private binding corrupt.
        // The Markdown graph supplies the reservation metadata; both the
        // private directory (if any) and the complete graph-local v2 namespace
        // are still archived byte-for-byte before Direct Files is published.
        Ok(None) | Err(_) => tine_core::model::Graph::open_checked(root_key)
            .map(|graph| graph.meta())
            .map_err(|error| {
                format!(
                    "Tine could not verify the Markdown/Org graph before returning to Direct files: {error}. Nothing was changed."
                )
            }),
    }
}

/// The caller must hold `graph_load`.  That lock serializes every graph-open,
/// sparse activation, and Direct Files return.  The registry check below is
/// therefore a quiescence proof, not a best-effort snapshot: an actor/open
/// worker cannot acquire a target slot between this check and the no-actor
/// recovery slot reservation.
fn reserve_cold_recovery_slot(
    state: &crate::state::AppState,
    label: &str,
    root_key: PathBuf,
    graph_meta: GraphMeta,
) -> Result<Arc<crate::state::GraphSlot>, String> {
    let mut graphs = state.graphs.write().unwrap();
    if graphs.slot(label).is_some() {
        return Err(
            "This window already owns a graph while recovery was requested. Nothing was changed; retry from the current recovery panel."
                .into(),
        );
    }
    if graphs.entries().into_iter().any(|(_, slot)| {
        slot.root_key.starts_with(&root_key) || root_key.starts_with(&slot.root_key)
    }) {
        return Err(
            "The remembered graph is already open or opening in another window. Nothing was changed."
                .into(),
        );
    }
    let slot = Arc::new(crate::state::GraphSlot::from_sparse_v2(
        retryable_binding(
            "local_active",
            "Cold-start recovery reserved this managed binding before reopening Direct files."
                .into(),
        ),
        root_key,
        graph_meta,
    ));
    graphs
        .bind(label.to_string(), Arc::clone(&slot))
        .map_err(|_| {
            "Tine could not reserve the remembered graph for recovery. Nothing was changed."
        })?;
    drop(graphs);
    crate::state::poke_watcher(state);
    Ok(slot)
}

/// Resolve the exact slot that appeared while a cold-start recovery request was
/// waiting for `graph_load`.  Callers hold that lock, so this is the mutation
/// boundary rather than a stale preflight: an exact managed slot is safe to
/// drain and archive through the established live-slot path; any other slot
/// means the recovery request has been superseded.
fn exact_live_cold_recovery_slot(
    state: &crate::state::AppState,
    label: &str,
    root_key: &Path,
) -> Result<Option<Arc<crate::state::GraphSlot>>, String> {
    let slot = state.graphs.read().unwrap().slot(label);
    let Some(slot) = slot else {
        return Ok(None);
    };
    if slot.root_key != root_key {
        return Err(
            "This recovery action is stale because the window opened a different graph. Nothing was changed."
                .into(),
        );
    }
    if !slot.is_sparse_v2() {
        return Err(
            "This recovery action is stale because the window is already using Direct files. Nothing was changed."
                .into(),
        );
    }
    Ok(Some(slot))
}

fn cancel_sparse_v2_cold_at_paths_with_archive(
    state: &crate::state::AppState,
    label: &str,
    root_key: PathBuf,
    private_root: &Path,
    recovery_root: &Path,
    approved_assets: Option<&Path>,
    archive: impl FnOnce(&Path, &Path) -> Result<Option<PathBuf>, String>,
) -> Result<SparseV2ColdCancelResult, String> {
    if let Some(slot) = exact_live_cold_recovery_slot(state, label, &root_key)? {
        return cancel_sparse_v2_at_paths_with_archive(
            state,
            label,
            slot,
            private_root,
            recovery_root,
            approved_assets,
            shutdown_for_direct_files_escape,
            archive,
        );
    }
    let graph_meta = cold_recovery_graph_meta(private_root, &root_key)?;
    let slot = reserve_cold_recovery_slot(state, label, root_key, graph_meta)?;
    cancel_sparse_v2_at_paths_with_archive(
        state,
        label,
        slot,
        private_root,
        recovery_root,
        approved_assets,
        shutdown_for_direct_files_escape,
        archive,
    )
}

fn cancel_sparse_v2_cold_at_paths_with_archive_and_publish(
    state: &crate::state::AppState,
    label: &str,
    root_key: PathBuf,
    private_root: &Path,
    recovery_root: &Path,
    approved_assets: Option<&Path>,
    archive: impl FnOnce(&Path, &Path) -> Result<Option<PathBuf>, String>,
    publish_direct: impl FnOnce(&Path, Option<&Path>) -> Result<u64, String>,
) -> Result<SparseV2ColdCancelResult, String> {
    if let Some(slot) = exact_live_cold_recovery_slot(state, label, &root_key)? {
        return cancel_sparse_v2_at_paths_with_archive_and_publish(
            state,
            label,
            slot,
            private_root,
            recovery_root,
            approved_assets,
            shutdown_for_direct_files_escape,
            archive,
            publish_direct,
        );
    }
    let graph_meta = cold_recovery_graph_meta(private_root, &root_key)?;
    let slot = reserve_cold_recovery_slot(state, label, root_key, graph_meta)?;
    cancel_sparse_v2_at_paths_with_archive_and_publish(
        state,
        label,
        slot,
        private_root,
        recovery_root,
        approved_assets,
        shutdown_for_direct_files_escape,
        archive,
        publish_direct,
    )
}

fn cancel_sparse_v2_cold_at_paths(
    state: &crate::state::AppState,
    label: &str,
    root_key: PathBuf,
    private_root: &Path,
    recovery_root: &Path,
    approved_assets: Option<&Path>,
) -> Result<SparseV2ColdCancelResult, String> {
    cancel_sparse_v2_cold_at_paths_with_archive(
        state,
        label,
        root_key,
        private_root,
        recovery_root,
        approved_assets,
        archive_private_root,
    )
}

#[tauri::command]
pub(crate) async fn cancel_sparse_v2_cold(
    path: String,
    attempt: u64,
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
) -> Result<SparseV2ColdCancelResult, String> {
    let label = window.label().to_string();
    tauri::async_runtime::spawn_blocking(move || {
        cancel_sparse_v2_cold_blocking(&app, &label, path, attempt)
    })
    .await
    .map_err(|_| "Cold Direct Files recovery worker stopped before completion.".to_string())?
}

fn cancel_sparse_v2_cold_blocking(
    app: &tauri::AppHandle,
    label: &str,
    path: String,
    attempt: u64,
) -> Result<SparseV2ColdCancelResult, String> {
    // Canonicalization is read-only.  It happens before `graph_load` only to
    // compare the submitted target with native authority; every state/storage
    // mutation remains inside the serialized transition below.
    let submitted_root = crate::state::canonical_graph_root(&path).map_err(|_| {
        "The selected recovery folder is unavailable. Retry graph lookup or choose another graph."
            .to_string()
    })?;
    let state = app.state::<crate::state::AppState>();
    let _transition = state.graph_load.lock().unwrap();

    // Re-read both the native attempt and exact canonical target *after*
    // acquiring the open lock.  A late result, picker, normal open, or another
    // recovery action cannot turn a stale frontend token into archive authority.
    let authorized_root = match state.authorized_startup_recovery_target(label, attempt) {
        Ok(root) => root,
        Err(_) if state.startup_recovery_attempt_is_current(label, attempt) => {
            // A lookup watchdog is observational: it cannot let a locally
            // cached path archive anything by itself.  The only timeout route
            // independently rereads native settings under this same lock and
            // accepts an exact canonical remembered/known graph only.
            if !crate::settings::startup_recovery_target_is_remembered(app, &submitted_root) {
                return Err(
                    "Tine could not verify this recovery target in its remembered graphs. Nothing was changed; retry graph lookup or choose another graph."
                        .into(),
                );
            }
            state.authorize_startup_recovery_target(label, attempt, Some(submitted_root.clone()));
            submitted_root.clone()
        }
        Err(error) => return Err(error),
    };
    if authorized_root != submitted_root {
        return Err(
            "The selected recovery target no longer matches Tine's remembered graph. Retry graph lookup before returning to Direct files."
                .into(),
        );
    }
    let private_root = sparse_private_root(app, &submitted_root)?;
    let recovery_root = sparse_recovery_root(app)?;
    let approved_assets = crate::settings::approved_external_assets(app, &submitted_root);
    cancel_sparse_v2_cold_at_paths_with_archive_and_publish(
        &state,
        label,
        submitted_root,
        &private_root,
        &recovery_root,
        approved_assets.as_deref(),
        archive_private_root,
        |direct_root, _| {
            crate::graph::open_and_publish_direct_files(
                app,
                label,
                &state,
                direct_root.to_path_buf(),
            )
            .map(|direct| direct.binding_generation)
        },
    )
}

#[tauri::command]
pub(crate) async fn cancel_sparse_v2(
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2CancelResult, String> {
    let (app, label, binding_generation) = crate::state::owned_graph_context(state)?;
    tauri::async_runtime::spawn_blocking(move || {
        cancel_sparse_v2_blocking(&app, &label, binding_generation)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn cancel_sparse_v2_blocking(
    app: &tauri::AppHandle,
    label: &str,
    binding_generation: u64,
) -> Result<SparseV2CancelResult, String> {
    let state = app.state::<crate::state::AppState>();
    let _transition = state.graph_load.lock().unwrap();
    let slot = crate::state::slot_for_bound_window(&state, label, Some(binding_generation))?;
    let private_root = sparse_private_root(&app, &slot.root_key)?;
    let recovery_root = sparse_recovery_root(&app)?;
    let approved_assets = crate::settings::approved_external_assets(&app, &slot.root_key);
    cancel_sparse_v2_at_paths(
        &state,
        label,
        slot,
        &private_root,
        &recovery_root,
        approved_assets.as_deref(),
        shutdown_for_direct_files_escape,
    )
}

/// Publish the already-safe local archive into the single shared v2
/// namespace, then reopen the same private device binding as SharedActive.
#[tauri::command]
pub(crate) async fn prepare_sparse_v2_share(
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2StatusDto, String> {
    let (app, label, binding_generation) = crate::state::owned_graph_context(state)?;
    tauri::async_runtime::spawn_blocking(move || {
        prepare_sparse_v2_share_blocking(&app, &label, binding_generation)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn prepare_sparse_v2_share_blocking(
    app: &tauri::AppHandle,
    label: &str,
    binding_generation: u64,
) -> Result<SparseV2StatusDto, String> {
    let state = app.state::<crate::state::AppState>();
    let _transition = state.graph_load.lock().unwrap();
    let slot = crate::state::slot_for_bound_window(&state, label, Some(binding_generation))?;
    let record = state
        .sync_runtime
        .binding_record(&app, &slot.root_key)?
        .ok_or("Tine-managed storage setup is missing.")?;
    active_handle(&slot)?
        .prepare_shared()
        .map_err(|error| error.to_string())?;
    let binding = match state.sync_runtime.open_record(&app, &record) {
        Ok(binding) => binding,
        Err(error) => {
            let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
                retryable_binding("share_prepared", error.clone()),
                slot.root_key.clone(),
                SyncRuntimeFacade::graph_meta(&record),
            ));
            state
                .graphs
                .write()
                .unwrap()
                .bind(label.to_string(), replacement)?;
            crate::state::poke_watcher(&state);
            return Err(error);
        }
    };
    let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
        binding,
        slot.root_key.clone(),
        SyncRuntimeFacade::graph_meta(&record),
    ));
    state
        .graphs
        .write()
        .unwrap()
        .bind(label.to_string(), Arc::clone(&replacement))?;
    crate::state::poke_watcher(&state);
    sparse_v2_status_for_slot(&replacement)
}

/// Explicitly retire the second device's legacy reader/watcher, derive its
/// private identity from exact provider descriptor evidence, and join.
#[tauri::command]
pub(crate) async fn join_sparse_v2_shared(
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2StatusDto, String> {
    let (app, label, binding_generation) = crate::state::owned_graph_context(state)?;
    tauri::async_runtime::spawn_blocking(move || {
        join_sparse_v2_shared_blocking(&app, &label, binding_generation)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn join_sparse_v2_shared_blocking(
    app: &tauri::AppHandle,
    label: &str,
    binding_generation: u64,
) -> Result<SparseV2StatusDto, String> {
    fn join_failure(stage: &str, error: impl std::fmt::Display) -> String {
        let detail = error.to_string();
        crate::debug::diag(format!(
            "managed sync join failed: stage={stage}; detail={detail}"
        ));
        format!("managed sync join failed at {stage}: {detail}")
    }

    let state = app.state::<crate::state::AppState>();
    let _transition = state.graph_load.lock().unwrap();
    let slot = crate::state::slot_for_bound_window(&state, label, Some(binding_generation))?;
    let descriptor =
        inspect_shared_enrollment_for_cold_discovery(&slot.root_key.join(".tine-sync/v2/shared"))
            .map_err(|error| join_failure("provider discovery", error))?
            .ok_or("This graph does not yet contain sync data from another device.")?;
    if slot.sparse_binding().is_some() {
        let record = state
            .sync_runtime
            .binding_record(app, &slot.root_key)?
            .ok_or("Tine-managed storage setup is missing.")?;
        active_handle(&slot)?
            .join_shared(descriptor)
            .map_err(|error| join_failure("provider scan", error))?;
        let reopened = match state.sync_runtime.open_record(app, &record) {
            Ok(binding) => binding,
            Err(error) => {
                let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
                    retryable_binding("shared_active", error.clone()),
                    slot.root_key.clone(),
                    SyncRuntimeFacade::graph_meta(&record),
                ));
                state
                    .graphs
                    .write()
                    .unwrap()
                    .bind(label.to_string(), replacement)?;
                crate::state::poke_watcher(&state);
                return Err(join_failure("runtime reopen", error));
            }
        };
        let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
            reopened,
            slot.root_key.clone(),
            SyncRuntimeFacade::graph_meta(&record),
        ));
        state
            .graphs
            .write()
            .unwrap()
            .bind(label.to_string(), Arc::clone(&replacement))?;
        crate::state::poke_watcher(&state);
        return sparse_v2_status_for_slot(&replacement);
    }
    let graph = slot.legacy_graph()?;
    let graph_meta = graph.meta();
    drop(graph);
    let record = SparseV2ActivationRecord::from_shared(
        &slot.root_key,
        graph_meta.clone(),
        DeviceId::from_uuid(crate::settings::managed_sync_device_id(app)?),
        &descriptor,
    );

    slot.begin_legacy_retirement()?;
    let removed = state.graphs.write().unwrap().remove(label);
    if removed
        .as_ref()
        .is_none_or(|removed| removed.binding_generation != slot.binding_generation)
    {
        slot.cancel_legacy_retirement()?;
        return Err("The graph changed while joining sync. Try again.".into());
    }
    crate::state::poke_watcher(&state);
    if let Err(error) = slot.wait_for_legacy_drain(LEGACY_DRAIN_TIMEOUT) {
        slot.cancel_legacy_retirement()?;
        state
            .graphs
            .write()
            .unwrap()
            .bind(label.to_string(), Arc::clone(&slot))?;
        crate::state::poke_watcher(&state);
        return Err(format!("Joining sync can be retried: {error}"));
    }
    if let Err(error) = state.sync_runtime.persist_binding_record(app, &record) {
        slot.cancel_legacy_retirement()?;
        state
            .graphs
            .write()
            .unwrap()
            .bind(label.to_string(), Arc::clone(&slot))?;
        crate::state::poke_watcher(&state);
        return Err(join_failure("binding publication", error));
    }
    let activated = match state.sync_runtime.activate_record(app, &record) {
        Ok(activated) => activated,
        Err(error) => {
            let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
                SparseV2Binding {
                    availability: SparseV2Availability::Retryable {
                        stage: "activation_request".into(),
                        detail: error.clone(),
                    },
                    handle: None,
                },
                slot.root_key.clone(),
                graph_meta,
            ));
            state
                .graphs
                .write()
                .unwrap()
                .bind(label.to_string(), replacement)?;
            crate::state::poke_watcher(&state);
            return Err(join_failure("local activation", error));
        }
    };
    let Some(handle) = activated.handle() else {
        let detail = format!(
            "join bootstrap did not reach LocalActive: {:?}",
            activated.availability()
        );
        let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
            activated,
            slot.root_key.clone(),
            graph_meta,
        ));
        state
            .graphs
            .write()
            .unwrap()
            .bind(label.to_string(), replacement)?;
        crate::state::poke_watcher(&state);
        return Err(detail);
    };
    if let Err(error) = handle.join_shared(descriptor) {
        let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
            activated,
            slot.root_key.clone(),
            graph_meta,
        ));
        state
            .graphs
            .write()
            .unwrap()
            .bind(label.to_string(), replacement)?;
        crate::state::poke_watcher(&state);
        return Err(join_failure("provider scan", error));
    }
    let binding = match state.sync_runtime.open_record(app, &record) {
        Ok(binding) => binding,
        Err(error) => {
            let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
                retryable_binding("shared_active", error.clone()),
                slot.root_key.clone(),
                SyncRuntimeFacade::graph_meta(&record),
            ));
            state
                .graphs
                .write()
                .unwrap()
                .bind(label.to_string(), replacement)?;
            crate::state::poke_watcher(&state);
            return Err(join_failure("runtime reopen", error));
        }
    };
    let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
        binding,
        slot.root_key.clone(),
        graph_meta,
    ));
    state
        .graphs
        .write()
        .unwrap()
        .bind(label.to_string(), Arc::clone(&replacement))?;
    crate::state::poke_watcher(&state);
    sparse_v2_status_for_slot(&replacement)
}

#[tauri::command]
pub(crate) async fn sparse_v2_query(
    request: tine_core::sync_runtime::SyncRuntimeQueryRequest,
    state: crate::state::GraphContext<'_>,
) -> Result<tine_core::sync_runtime::SyncRuntimeQueryReply, String> {
    let (app, label, binding_generation) = crate::state::owned_graph_context(state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<crate::state::AppState>();
        let slot = crate::state::slot_for_bound_window(&state, &label, Some(binding_generation))?;
        active_handle(&slot)?
            .query(request)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub(crate) async fn sparse_v2_editor_load(
    request: tine_core::sync_runtime::SyncEditorLoadRequest,
    state: crate::state::GraphContext<'_>,
) -> Result<tine_core::sync_runtime::SyncEditorLoadOutcome, String> {
    let (app, label, binding_generation) = crate::state::owned_graph_context(state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<crate::state::AppState>();
        let slot = crate::state::slot_for_bound_window(&state, &label, Some(binding_generation))?;
        active_handle(&slot)?
            .load_editor_page(request)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub(crate) async fn sparse_v2_editor_save(
    request: tine_core::sync_runtime::SyncEditorSaveRequest,
    state: crate::state::GraphContext<'_>,
) -> Result<tine_core::sync_runtime::SyncEditorSaveOutcome, String> {
    let (app, label, binding_generation) = crate::state::owned_graph_context(state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<crate::state::AppState>();
        let slot = crate::state::slot_for_bound_window(&state, &label, Some(binding_generation))?;
        active_handle(&slot)?
            .save_editor_page(request)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub(crate) async fn sparse_v2_tick(
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2TickDto, String> {
    let (app, label, binding_generation) = crate::state::owned_graph_context(state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<crate::state::AppState>();
        let slot = crate::state::slot_for_bound_window(&state, &label, Some(binding_generation))?;
        active_handle(&slot)?
            .tick()
            .map(tick_dto)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub(crate) async fn sparse_v2_clean_shutdown(
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2RuntimeStatusDto, String> {
    let (app, label, binding_generation) = crate::state::owned_graph_context(state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<crate::state::AppState>();
        let slot = crate::state::slot_for_bound_window(&state, &label, Some(binding_generation))?;
        active_handle(&slot)?
            .clean_shutdown()
            .map(shutdown_status)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// A graph slot can authorize a process/window exit only after its managed
/// runtime has reached the specific `Safe` shutdown outcome. A terminal actor
/// has stopped accepting work, but it did not prove the clean-stop invariant;
/// collapsing both outcomes because they expose a status snapshot would let an
/// exit discard the recovery path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanShutdownSlot {
    Direct,
    Safe,
}

fn clean_shutdown_outcome(outcome: SyncShutdownOutcome) -> Result<CleanShutdownSlot, String> {
    match outcome {
        SyncShutdownOutcome::Safe(_) => Ok(CleanShutdownSlot::Safe),
        SyncShutdownOutcome::Terminal(snapshot) => Err(format!(
            "Tine-managed storage reached a terminal state and cannot authorize process exit: {}",
            snapshot
                .detail
                .unwrap_or_else(|| "no terminal detail was recorded".into())
        )),
    }
}

pub(crate) fn clean_shutdown_slot(
    slot: &crate::state::GraphSlot,
) -> Result<CleanShutdownSlot, String> {
    let Some(handle) = slot.sparse_runtime() else {
        return Ok(CleanShutdownSlot::Direct);
    };
    handle
        .clean_shutdown()
        .map_err(|error| error.to_string())
        .and_then(clean_shutdown_outcome)
}

#[cfg(test)]
mod clean_shutdown_slot_tests {
    use super::*;

    fn snapshot(
        lifecycle: SyncRuntimeLifecycle,
        detail: Option<&str>,
    ) -> SyncRuntimeStatusSnapshot {
        SyncRuntimeStatusSnapshot {
            lifecycle,
            recovery: None,
            watcher: Default::default(),
            last_tick: None,
            detail: detail.map(str::to_owned),
            shared_role: None,
            shared_phase: None,
            provider_pending: 0,
            managed_local_pending: 0,
            managed_local_checkpointed_sequence: 0,
            managed_local_next_sequence: 0,
            managed_local_stage: None,
        }
    }

    #[test]
    fn terminal_shutdown_outcome_cannot_authorize_an_exit() {
        let terminal = SyncShutdownOutcome::Terminal(snapshot(
            SyncRuntimeLifecycle::Terminal,
            Some("authority lease was revoked"),
        ));
        assert!(clean_shutdown_outcome(terminal)
            .unwrap_err()
            .contains("cannot authorize process exit"));

        let safe = SyncShutdownOutcome::Safe(snapshot(SyncRuntimeLifecycle::StoppedSafe, None));
        assert_eq!(clean_shutdown_outcome(safe), Ok(CleanShutdownSlot::Safe));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tine_core::model::Graph;
    use tine_core::sync_runtime::{
        SyncApplicationMoveAdmission, SyncApplicationMovePlacement, SyncApplicationMoveRoot,
        SyncApplicationPageLoadOutcome, SyncApplicationPageLoadRequest,
        SyncApplicationPageSaveOutcome, SyncApplicationPageSaveRequest,
        SyncApplicationPageSaveTarget, SyncApplicationPageSelector, SyncEditorBlockDto,
        SyncEditorBlockKey, SyncEditorLoadOutcome, SyncEditorLoadRequest, SyncEditorPageSelector,
        SyncEditorSaveOutcome, SyncEditorSaveRequest, SyncEditorSaveTarget, SyncEntityId,
        SyncLocalMutationOutcome, SyncPageKind, SyncPageNameResolutionDto, SyncRuntimeQueryReply,
        SyncRuntimeQueryRequest, SyncSearchHitDto, SyncWatcherObservation,
    };

    #[test]
    fn activation_command_owns_the_complete_transition_inside_spawn_blocking() {
        let source = include_str!("sync_runtime.rs");
        let start = source
            .find("pub(crate) async fn activate_sparse_v2")
            .expect("activation command must be async");
        let end = source[start..]
            .find("#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]")
            .map(|offset| start + offset)
            .expect("activation command boundary");
        let command = &source[start..end];
        let blocking = command
            .find("tauri::async_runtime::spawn_blocking(move ||")
            .expect("activation transition must enter spawn_blocking");
        let before_blocking = &command[..blocking];
        assert!(
            !before_blocking.contains("graph_load.lock")
                && !before_blocking.contains("slot_for_context"),
            "graph authority and the transition lock must be resolved inside the owned blocking operation"
        );
        assert!(
            command[blocking..]
                .contains("activate_sparse_v2_blocking(&app, &label, binding_generation)"),
            "the blocking operation must re-resolve the exact graph generation from owned inputs"
        );
    }

    #[test]
    fn every_explicit_managed_actor_command_re_resolves_off_the_async_command_thread() {
        let source = include_str!("sync_runtime.rs");
        for name in [
            "sparse_v2_status",
            "cancel_sparse_v2",
            "prepare_sparse_v2_share",
            "join_sparse_v2_shared",
            "sparse_v2_query",
            "sparse_v2_editor_load",
            "sparse_v2_editor_save",
            "sparse_v2_tick",
            "sparse_v2_clean_shutdown",
        ] {
            let signature = format!("pub(crate) async fn {name}(");
            let start = source
                .find(&signature)
                .expect("managed command stays async");
            let tail = &source[start..];
            let end = tail.find("\n#[tauri::command]").unwrap_or(tail.len());
            let command = &tail[..end];
            assert!(
                command.contains("owned_graph_context(state)?"),
                "{name} must own the exact window binding before await"
            );
            assert!(
                command.contains("tauri::async_runtime::spawn_blocking(move ||"),
                "{name} must move every possible managed actor wait to the blocking pool"
            );
            assert!(
                command.contains("slot_for_bound_window")
                    && command.contains("Some(binding_generation)"),
                "{name} must re-resolve the captured generation inside the blocking operation"
            );
        }
    }

    #[test]
    fn managed_open_stderr_receipts_use_bounded_codes_not_storage_details() {
        let source = include_str!("sync_runtime.rs");
        let start = source
            .find("fn open_record_with_progress(")
            .expect("managed open diagnostic boundary");
        let open = &source[start
            ..source[start..]
                .find("    /// The startup graph-open path")
                .map(|end| start + end)
                .expect("managed open diagnostic end")];
        assert!(open.contains("managed_open_phase_name(*phase)"));
        assert!(open.contains("managed_open_waiting_phase_name(*phase)"));
        assert!(open.contains("managed_open_outcome_code(&opened.status)"));
        assert!(
            !open.contains("diagnostics.projection_reason")
                && !open.contains("completed with {:?}"),
            "unconditional diagnostics must not format arbitrary storage details"
        );
        assert_eq!(
            managed_open_outcome_code(&SyncRuntimeOpenStatus::Blocked {
                reason_code: "/private/path/injected-detail".into(),
            }),
            "blocked"
        );
        assert_eq!(
            managed_open_outcome_code(&SyncRuntimeOpenStatus::OpenRefused {
                detail: "/private/path/injected-detail".into(),
            }),
            "open_refused"
        );
    }

    #[test]
    fn activation_heartbeat_stops_and_joins_without_waiting_for_the_interval() {
        let started = Instant::now();
        let heartbeat = ActivationHeartbeat::start(started, Arc::new(Mutex::new(None)));
        drop(heartbeat);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "heartbeat shutdown waited for the ten-second reporting interval"
        );
    }

    #[test]
    fn activation_heartbeat_reports_latest_detailed_part_progress() {
        let latest = Arc::new(Mutex::new(Some(
            SyncLocalActivationProgress::BootstrapDetachedAuthoring {
                completed: 2,
                total: 5,
            },
        )));
        assert_eq!(
            latest_activation_progress_name(&latest),
            "bootstrap preparation: detached authoring 2/5 parts"
        );
        let event = SparseV2ActivationProgressEvent {
            binding_generation: 17,
            progress: latest.lock().unwrap().clone().unwrap(),
        };
        let serialized = serde_json::to_value(event).unwrap();
        assert_eq!(serialized["binding_generation"], 17);
        assert_eq!(
            serialized["progress"]["kind"],
            "bootstrap_detached_authoring"
        );
        assert_eq!(serialized["progress"]["completed"], 2);
        assert_eq!(serialized["progress"]["total"], 5);
    }

    #[test]
    fn direct_slot_serializes_direct_application_page_admission() {
        let root = std::env::temp_dir().join(format!("tine-admission-direct-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let slot = crate::state::GraphSlot::new(Graph::open(&root), root.clone());

        let status = sparse_v2_status_for_slot(&slot).unwrap();
        let wire = serde_json::to_value(status).unwrap();

        assert_eq!(wire["binding_generation"], slot.binding_generation);
        assert_eq!(
            wire["application_page_admission"],
            serde_json::json!({
                "binding_generation": slot.binding_generation,
                "authority": "direct",
            })
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_an_active_runtime_lifecycle_admits_application_pages() {
        assert!(runtime_lifecycle_admits_application_pages(
            &SyncRuntimeLifecycle::Active
        ));
        assert!(!runtime_lifecycle_admits_application_pages(
            &SyncRuntimeLifecycle::StoppedSafe
        ));
        assert!(!runtime_lifecycle_admits_application_pages(
            &SyncRuntimeLifecycle::StoppedCrashed
        ));
        assert!(!runtime_lifecycle_admits_application_pages(
            &SyncRuntimeLifecycle::Terminal
        ));
    }

    struct RollbackFixture {
        root: PathBuf,
        graph_root: PathBuf,
        private_root: PathBuf,
        recovery_root: PathBuf,
        markdown_path: PathBuf,
        markdown_bytes: Vec<u8>,
        binding_bytes: Vec<u8>,
        state: crate::state::AppState,
        slot: Arc<crate::state::GraphSlot>,
    }

    impl RollbackFixture {
        fn new(stage: Option<&str>) -> Self {
            let root =
                std::env::temp_dir().join(format!("tine-sparse-rollback-{}", Uuid::new_v4()));
            let graph_root = root.join("graph");
            let private_root = root.join("app-data/sparse-v2/graph-key");
            let recovery_root = root.join("app-data/sparse-v2-recovery");
            let markdown_path = graph_root.join("pages/rollback.md");
            let markdown_bytes = b"- Markdown remains authoritative\n".to_vec();
            std::fs::create_dir_all(graph_root.join("pages")).unwrap();
            std::fs::create_dir_all(graph_root.join("journals")).unwrap();
            std::fs::write(&markdown_path, &markdown_bytes).unwrap();
            let graph = Graph::open(&graph_root);
            let meta = graph.meta();
            drop(graph);
            let record = SparseV2ActivationRecord::new(&graph_root, meta.clone(), DeviceId::new());
            persist_binding_at(&private_root.join(SPARSE_BINDING_FILE), &record).unwrap();
            std::fs::write(private_root.join("diagnostic-bytes"), b"preserve exactly").unwrap();
            let binding_bytes = std::fs::read(private_root.join(SPARSE_BINDING_FILE)).unwrap();
            let binding = retryable_binding(
                stage.unwrap_or("shadow_import"),
                "incomplete local activation".into(),
            );
            let slot = Arc::new(crate::state::GraphSlot::from_sparse_v2(
                binding,
                graph_root.clone(),
                meta,
            ));
            let state = crate::state::AppState {
                graphs: std::sync::RwLock::new(crate::state::GraphRegistry::default()),
                graph_load: Mutex::new(()),
                watch_ctl: Mutex::new(None),
                last_focused: Mutex::new(None),
                capture_graph: Mutex::new(None),
                startup_recovery: Mutex::new(std::collections::HashMap::new()),
                sync_runtime: SyncRuntimeFacade,
                #[cfg(desktop)]
                next_window: std::sync::atomic::AtomicU64::new(1),
            };
            state
                .graphs
                .write()
                .unwrap()
                .bind("main".into(), Arc::clone(&slot))
                .unwrap();
            Self {
                root,
                graph_root,
                private_root,
                recovery_root,
                markdown_path,
                markdown_bytes,
                binding_bytes,
                state,
                slot,
            }
        }

        fn make_active(&mut self) {
            let record = read_binding_at(
                &self.private_root.join(SPARSE_BINDING_FILE),
                &self.graph_root,
            )
            .unwrap()
            .unwrap();
            let activated =
                SyncRuntimeHandle::activate_or_resume_local(SyncLocalActivationRequest {
                    graph_root: self.graph_root.clone(),
                    archive_root: self.private_root.join("archive"),
                    enrollment_root: self.private_root.join("enrollment"),
                    receipt_root: self.private_root.join("receipts"),
                    database_path: self.private_root.join("projection/materialization.sqlite"),
                    application_runtime_root: self.private_root.join("runtime"),
                    migration_backup_root: self.private_root.join("migration-backup"),
                    capture_root: self.private_root.join("capture"),
                    preparation_root: self.private_root.join("preparation"),
                    provider_root: self.graph_root.join(".tine-sync/v2/shared"),
                    provider_journal_root: self.private_root.join("provider/device/journal"),
                    identities: SyncLocalActivationIdentities {
                        workspace_id: record.workspace_id,
                        lineage_digest: record.lineage_digest,
                        catalog_document_id: record.catalog_document_id,
                        endpoint_id: record.endpoint_id,
                        device_id: record.device_id,
                        preparation_id: record.preparation_id,
                        session_id: record.activation_session_id,
                    },
                });
            assert_eq!(activated.status, SyncLocalActivationStatus::Active);
            let active = Arc::new(crate::state::GraphSlot::from_sparse_v2(
                SparseV2Binding::from_activation(activated),
                self.graph_root.clone(),
                SyncRuntimeFacade::graph_meta(&record),
            ));
            self.state
                .graphs
                .write()
                .unwrap()
                .bind("main".into(), Arc::clone(&active))
                .unwrap();
            self.slot = active;
            self.binding_bytes =
                std::fs::read(self.private_root.join(SPARSE_BINDING_FILE)).unwrap();
        }
    }

    impl Drop for RollbackFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(base: &Path, current: &Path, found: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let mut entries = std::fs::read_dir(current)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    visit(base, &path, found);
                } else {
                    found.insert(
                        path.strip_prefix(base).unwrap().to_path_buf(),
                        std::fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut found = BTreeMap::new();
        if root.is_dir() {
            visit(root, root, &mut found);
        }
        found
    }

    fn create_empty_local_provider_scaffold(graph_root: &Path) {
        let shared = graph_root.join(".tine-sync/v2/shared");
        for tree in PROVIDER_SCAFFOLD_TREES {
            for namespace in PROVIDER_SCAFFOLD_NAMESPACES {
                std::fs::create_dir_all(shared.join(tree).join(namespace)).unwrap();
            }
        }
    }

    fn cold_fixture(stage: Option<&str>) -> RollbackFixture {
        let fixture = RollbackFixture::new(stage);
        let removed = fixture.state.graphs.write().unwrap().remove("main");
        assert!(
            removed.is_some(),
            "cold recovery starts before any graph slot"
        );
        fixture
    }

    #[test]
    fn joinable_shared_descriptor_still_serializes_direct_application_admission() {
        std::thread::Builder::new()
            .name("tine-joinable-direct-admission-test".into())
            .stack_size(32 * 1024 * 1024)
            .spawn(joinable_shared_descriptor_still_serializes_direct_application_admission_inner)
            .unwrap()
            .join()
            .unwrap();
    }

    fn joinable_shared_descriptor_still_serializes_direct_application_admission_inner() {
        let mut fixture = RollbackFixture::new(Some("shadow_import"));
        fixture.make_active();
        let descriptor = fixture
            .slot
            .sparse_runtime()
            .expect("active fixture must retain its runtime")
            .prepare_shared()
            .unwrap();

        // A second process discovering this literal shared descriptor has not
        // opted into sparse-v2 yet, so it keeps its Direct Files writer.
        let direct = crate::state::GraphSlot::new(
            Graph::open(&fixture.graph_root),
            fixture.graph_root.clone(),
        );
        let status = sparse_v2_status_for_slot(&direct).unwrap();
        assert!(matches!(
            status.availability,
            SparseV2Availability::Joinable { ref descriptor_digest }
                if descriptor_digest == &descriptor.descriptor_digest
        ));
        assert_eq!(
            serde_json::to_value(status).unwrap()["application_page_admission"],
            serde_json::json!({
                "binding_generation": direct.binding_generation,
                "authority": "direct",
            })
        );
    }

    #[test]
    fn cold_return_requires_native_lock_attempt_target_and_full_direct_lifecycle() {
        let source = include_str!("sync_runtime.rs");
        let start = source
            .find("fn cancel_sparse_v2_cold_blocking(")
            .expect("cold recovery blocking boundary");
        let command = &source[start
            ..source[start..]
                .find("#[tauri::command]\npub(crate) async fn cancel_sparse_v2(")
                .map(|end| start + end)
                .expect("next managed command")];
        for required in [
            "state.graph_load.lock()",
            "authorized_startup_recovery_target",
            "startup_recovery_target_is_remembered",
            "cancel_sparse_v2_cold_at_paths_with_archive_and_publish",
            "open_and_publish_direct_files",
        ] {
            assert!(
                command.contains(required),
                "cold recovery must retain `{required}`"
            );
        }
    }

    #[test]
    fn cold_return_without_slot_archives_local_and_shared_provider_evidence_preserving_bytes() {
        let local = cold_fixture(Some("shadow_import"));
        create_empty_local_provider_scaffold(&local.graph_root);
        let local_provider = snapshot_tree(&local.graph_root.join(".tine-sync/v2"));
        let local_result = cancel_sparse_v2_cold_at_paths(
            &local.state,
            "main",
            local.graph_root.clone(),
            &local.private_root,
            &local.recovery_root,
            None,
        )
        .unwrap();
        assert!(matches!(
            local_result.status.availability,
            SparseV2Availability::LegacyDefault
        ));
        assert!(!local.private_root.exists());
        assert_eq!(
            snapshot_tree(
                &std::fs::read_dir(local.graph_root.join(".tine-sync/recovery"))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path(),
            ),
            local_provider
        );
        assert_eq!(
            std::fs::read(&local.markdown_path).unwrap(),
            local.markdown_bytes
        );
        assert!(local
            .state
            .graphs
            .read()
            .unwrap()
            .slot("main")
            .unwrap()
            .legacy_graph()
            .is_ok());

        let shared = cold_fixture(Some("joining"));
        std::fs::create_dir_all(shared.graph_root.join(".tine-sync/v2/shared")).unwrap();
        std::fs::write(
            shared
                .graph_root
                .join(".tine-sync/v2/shared/provider-evidence"),
            b"shared provider bytes",
        )
        .unwrap();
        let shared_provider = snapshot_tree(&shared.graph_root.join(".tine-sync/v2"));
        cancel_sparse_v2_cold_at_paths(
            &shared.state,
            "main",
            shared.graph_root.clone(),
            &shared.private_root,
            &shared.recovery_root,
            None,
        )
        .unwrap();
        let archived_provider = std::fs::read_dir(shared.graph_root.join(".tine-sync/recovery"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(snapshot_tree(&archived_provider), shared_provider);
        assert!(!shared.private_root.exists());
        assert!(shared
            .state
            .graphs
            .read()
            .unwrap()
            .slot("main")
            .unwrap()
            .legacy_graph()
            .is_ok());
    }

    #[test]
    fn cold_return_archive_failure_keeps_private_provider_and_markdown_bytes_retryable() {
        let fixture = cold_fixture(Some("shadow_import"));
        create_empty_local_provider_scaffold(&fixture.graph_root);
        let private_before = snapshot_tree(&fixture.private_root);
        let provider_before = snapshot_tree(&fixture.graph_root.join(".tine-sync/v2"));
        let markdown_before = std::fs::read(&fixture.markdown_path).unwrap();
        let error = cancel_sparse_v2_cold_at_paths_with_archive(
            &fixture.state,
            "main",
            fixture.graph_root.clone(),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
            |_, _| Err("injected cold archive failure".into()),
        )
        .unwrap_err();
        assert!(error.contains("injected cold archive failure"));
        assert_eq!(snapshot_tree(&fixture.private_root), private_before);
        assert_eq!(
            snapshot_tree(&fixture.graph_root.join(".tine-sync/v2")),
            provider_before
        );
        assert_eq!(
            std::fs::read(&fixture.markdown_path).unwrap(),
            markdown_before
        );
        assert!(!fixture.recovery_root.exists());
        let retryable = fixture.state.graphs.read().unwrap().slot("main").unwrap();
        assert!(retryable.is_sparse_v2());
        assert!(retryable.sparse_runtime().is_none());
    }

    #[test]
    fn cold_return_archives_and_opens_direct_with_missing_or_corrupt_binding() {
        let missing = cold_fixture(Some("shadow_import"));
        std::fs::remove_file(missing.private_root.join(SPARSE_BINDING_FILE)).unwrap();
        let missing_enrollment = missing
            .graph_root
            .join(".tine-sync/v2/shared/outbox/enrollment");
        std::fs::create_dir_all(&missing_enrollment).unwrap();
        std::fs::write(
            missing_enrollment.join("shared-enrollment-v1.json"),
            b"partially delivered descriptor",
        )
        .unwrap();
        let missing_before = snapshot_tree(&missing.private_root);
        let missing_provider = snapshot_tree(&missing.graph_root.join(".tine-sync/v2"));
        let missing_result = cancel_sparse_v2_cold_at_paths(
            &missing.state,
            "main",
            missing.graph_root.clone(),
            &missing.private_root,
            &missing.recovery_root,
            None,
        )
        .unwrap();
        assert!(matches!(
            missing_result.status.availability,
            SparseV2Availability::LegacyDefault
        ));
        assert!(!missing.private_root.exists());
        assert_eq!(
            snapshot_tree(
                &std::fs::read_dir(&missing.recovery_root)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path(),
            ),
            missing_before
        );
        assert!(!missing.graph_root.join(".tine-sync/v2").exists());
        assert_eq!(
            snapshot_tree(
                &std::fs::read_dir(missing.graph_root.join(".tine-sync/recovery"))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path(),
            ),
            missing_provider
        );
        assert!(missing
            .state
            .graphs
            .read()
            .unwrap()
            .slot("main")
            .unwrap()
            .legacy_graph()
            .is_ok());

        let corrupt = cold_fixture(Some("shadow_import"));
        std::fs::write(corrupt.private_root.join(SPARSE_BINDING_FILE), b"{").unwrap();
        let corrupt_before = snapshot_tree(&corrupt.private_root);
        let corrupt_result = cancel_sparse_v2_cold_at_paths(
            &corrupt.state,
            "main",
            corrupt.graph_root.clone(),
            &corrupt.private_root,
            &corrupt.recovery_root,
            None,
        )
        .unwrap();
        assert!(matches!(
            corrupt_result.status.availability,
            SparseV2Availability::LegacyDefault
        ));
        assert!(!corrupt.private_root.exists());
        assert_eq!(
            snapshot_tree(
                &std::fs::read_dir(&corrupt.recovery_root)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path(),
            ),
            corrupt_before
        );
        assert!(corrupt
            .state
            .graphs
            .read()
            .unwrap()
            .slot("main")
            .unwrap()
            .legacy_graph()
            .is_ok());
    }

    #[test]
    fn cold_return_with_exact_live_managed_slot_uses_the_live_shutdown_and_archive_path() {
        std::thread::Builder::new()
            .name("tine-sparse-cold-live-return-test".into())
            .stack_size(32 * 1024 * 1024)
            .spawn(cold_return_with_exact_live_managed_slot_uses_the_live_shutdown_and_archive_path_inner)
            .unwrap()
            .join()
            .unwrap();
    }

    fn cold_return_with_exact_live_managed_slot_uses_the_live_shutdown_and_archive_path_inner() {
        let mut fixture = RollbackFixture::new(Some("shadow_import"));
        fixture.make_active();
        let markdown_before = std::fs::read(&fixture.markdown_path).unwrap();
        let result = cancel_sparse_v2_cold_at_paths(
            &fixture.state,
            "main",
            fixture.graph_root.clone(),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
        )
        .unwrap();
        assert!(matches!(
            result.status.availability,
            SparseV2Availability::LegacyDefault
        ));
        assert!(!fixture.private_root.exists());
        assert_eq!(
            std::fs::read(&fixture.markdown_path).unwrap(),
            markdown_before
        );
        assert!(fixture.recovery_root.exists());
        assert!(fixture
            .state
            .graphs
            .read()
            .unwrap()
            .slot("main")
            .unwrap()
            .legacy_graph()
            .is_ok());
    }

    #[test]
    fn cold_return_refuses_an_exact_direct_slot_without_archiving() {
        let fixture = RollbackFixture::new(Some("shadow_import"));
        let direct = Arc::new(crate::state::GraphSlot::new(
            Graph::open(&fixture.graph_root),
            fixture.graph_root.clone(),
        ));
        fixture
            .state
            .graphs
            .write()
            .unwrap()
            .bind("main".into(), direct)
            .unwrap();
        let private_before = snapshot_tree(&fixture.private_root);
        let error = cancel_sparse_v2_cold_at_paths(
            &fixture.state,
            "main",
            fixture.graph_root.clone(),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
        )
        .unwrap_err();
        assert!(error.contains("already using Direct files"));
        assert_eq!(snapshot_tree(&fixture.private_root), private_before);
        assert!(!fixture.recovery_root.exists());
    }

    #[test]
    fn cold_return_refuses_a_slot_for_a_different_root_without_archiving() {
        let fixture = RollbackFixture::new(Some("shadow_import"));
        let other_root = fixture.root.join("other-graph");
        std::fs::create_dir_all(&other_root).unwrap();
        let private_before = snapshot_tree(&fixture.private_root);
        let error = cancel_sparse_v2_cold_at_paths(
            &fixture.state,
            "main",
            other_root,
            &fixture.private_root,
            &fixture.recovery_root,
            None,
        )
        .unwrap_err();
        assert!(error.contains("opened a different graph"));
        assert_eq!(snapshot_tree(&fixture.private_root), private_before);
        assert!(!fixture.recovery_root.exists());
    }

    #[test]
    fn sparse_binding_without_live_handle_gives_actionable_recovery() {
        let fixture = RollbackFixture::new(Some("shadow_import"));
        assert_eq!(
            active_handle(&fixture.slot).unwrap_err(),
            SPARSE_V2_NOT_ACTIVE
        );
        assert!(SPARSE_V2_NOT_ACTIVE.contains("Retry setup"));
        assert!(SPARSE_V2_NOT_ACTIVE.contains("return to Direct files"));
        let status = sparse_v2_status_for_slot(&fixture.slot).unwrap();
        assert_eq!(
            serde_json::to_value(status).unwrap()["application_page_admission"]["authority"],
            "managed_unavailable"
        );
    }

    #[test]
    fn transition_status_keeps_archive_and_direct_escape_available_with_warnings() {
        let local = RollbackFixture::new(Some("shadow_import"));
        let local_status = sparse_v2_status_for_slot(&local.slot).unwrap();
        assert!(matches!(
            local_status.availability,
            SparseV2Availability::Retryable { ref stage, .. } if stage == "shadow_import"
        ));
        assert!(local_status.can_cancel);
        assert_eq!(local_status.cancel_reason, None);
        assert_eq!(
            local_status.binding_generation,
            local.slot.binding_generation
        );

        for stage in ["share_prepared", "joining", "shared_active"] {
            let shared = RollbackFixture::new(Some(stage));
            let shared_status = sparse_v2_status_for_slot(&shared.slot).unwrap();
            assert!(shared_status.can_cancel, "{stage}");
            assert!(
                shared_status
                    .cancel_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("archive the complete private")),
                "{stage}: {:?}",
                shared_status.cancel_reason
            );
        }

        let provider = RollbackFixture::new(Some("shadow_import"));
        std::fs::create_dir_all(provider.graph_root.join(".tine-sync/v2")).unwrap();
        std::fs::write(
            provider.graph_root.join(".tine-sync/v2/provider-evidence"),
            b"unclassifiable provider state",
        )
        .unwrap();
        let provider_status = sparse_v2_status_for_slot(&provider.slot).unwrap();
        assert!(provider_status.can_cancel);
        assert!(provider_status
            .cancel_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("archive the complete private")));
    }

    #[test]
    fn incomplete_local_activation_retires_without_touching_markdown_and_preserves_private_bytes() {
        let fixture = RollbackFixture::new(Some("shadow_import"));
        create_empty_local_provider_scaffold(&fixture.graph_root);
        let provider_before = snapshot_tree(&fixture.graph_root.join(".tine-sync/v2"));
        assert_eq!(
            provider_namespace_evidence(&fixture.graph_root.join(".tine-sync/v2")).unwrap(),
            ProviderNamespaceEvidence::LocalOnly
        );
        let result = cancel_sparse_v2_at_paths(
            &fixture.state,
            "main",
            Arc::clone(&fixture.slot),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
            shutdown_for_direct_files_escape,
        )
        .unwrap();

        assert!(matches!(
            result.status.availability,
            SparseV2Availability::LegacyDefault
        ));
        assert_eq!(result.binding_generation, result.status.binding_generation);
        assert!(result
            .recovery_statement
            .contains("Complete managed-storage recovery state was preserved"));
        assert!(!fixture.private_root.exists());
        let archives = std::fs::read_dir(&fixture.recovery_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(archives.len(), 1);
        assert_eq!(
            std::fs::read(archives[0].join(SPARSE_BINDING_FILE)).unwrap(),
            fixture.binding_bytes
        );
        assert_eq!(
            std::fs::read(archives[0].join("diagnostic-bytes")).unwrap(),
            b"preserve exactly"
        );
        assert_eq!(
            std::fs::read(&fixture.markdown_path).unwrap(),
            fixture.markdown_bytes
        );
        assert!(!fixture.graph_root.join(".tine-sync/v2").exists());
        let provider_archives = std::fs::read_dir(fixture.graph_root.join(".tine-sync/recovery"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(provider_archives.len(), 1);
        assert_eq!(snapshot_tree(&provider_archives[0]), provider_before);
        assert!(fixture
            .state
            .graphs
            .read()
            .unwrap()
            .slot("main")
            .unwrap()
            .legacy_graph()
            .is_ok());
        assert!(read_binding_at(
            &fixture.private_root.join(SPARSE_BINDING_FILE),
            &fixture.graph_root
        )
        .unwrap()
        .is_none());
        assert!(Graph::open_checked(&fixture.graph_root).is_ok());
    }

    #[test]
    fn rollback_reload_save_uses_current_disk_revision_and_later_external_write_conflicts() {
        let fixture = RollbackFixture::new(Some("shadow_import"));
        cancel_sparse_v2_at_paths(
            &fixture.state,
            "main",
            Arc::clone(&fixture.slot),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
            shutdown_for_direct_files_escape,
        )
        .unwrap();

        let replacement = fixture.state.graphs.read().unwrap().slot("main").unwrap();
        let graph = replacement.legacy_graph().unwrap();
        let mut reloaded = graph
            .load_named("rollback", tine_core::model::PageKind::Page)
            .unwrap()
            .unwrap();
        let baseline = reloaded.rev.take().unwrap();
        reloaded.blocks[0].raw = "ordinary edit after rollback".into();
        let saved = graph.save_page(&reloaded, Some(&baseline)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&fixture.markdown_path).unwrap(),
            "- ordinary edit after rollback\n"
        );

        let mut current = graph
            .load_named("rollback", tine_core::model::PageKind::Page)
            .unwrap()
            .unwrap();
        assert_eq!(current.rev.take().as_deref(), Some(saved.as_str()));
        current.blocks[0].raw = "must not overwrite external bytes".into();
        std::fs::write(&fixture.markdown_path, b"- genuinely external edit\n").unwrap();

        assert_eq!(
            graph.save_page(&current, Some(&saved)).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::read_to_string(&fixture.markdown_path).unwrap(),
            "- genuinely external edit\n"
        );
    }

    fn assert_return_to_direct_files_after_unreadable_private_state(fixture: &RollbackFixture) {
        let result = cancel_sparse_v2_at_paths(
            &fixture.state,
            "main",
            Arc::clone(&fixture.slot),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
            shutdown_for_direct_files_escape,
        )
        .unwrap();
        assert!(matches!(
            result.status.availability,
            SparseV2Availability::LegacyDefault
        ));
        assert!(fixture
            .state
            .graphs
            .read()
            .unwrap()
            .slot("main")
            .unwrap()
            .legacy_graph()
            .is_ok());
    }

    #[test]
    fn missing_private_binding_does_not_block_explicit_return_to_direct_files() {
        let fixture = RollbackFixture::new(Some("shadow_import"));
        std::fs::remove_file(fixture.private_root.join(SPARSE_BINDING_FILE)).unwrap();

        assert_return_to_direct_files_after_unreadable_private_state(&fixture);
        let archived = std::fs::read_dir(&fixture.recovery_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(archived.len(), 1);
        assert!(archived[0].join("diagnostic-bytes").is_file());
        assert!(!archived[0].join(SPARSE_BINDING_FILE).exists());
    }

    #[test]
    fn corrupt_private_binding_does_not_block_explicit_return_to_direct_files() {
        let fixture = RollbackFixture::new(Some("shadow_import"));
        std::fs::write(fixture.private_root.join(SPARSE_BINDING_FILE), b"{").unwrap();

        assert_return_to_direct_files_after_unreadable_private_state(&fixture);
        let archived = std::fs::read_dir(&fixture.recovery_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(archived.len(), 1);
        assert_eq!(
            std::fs::read(archived[0].join(SPARSE_BINDING_FILE)).unwrap(),
            b"{"
        );
    }

    #[test]
    fn absent_private_root_does_not_block_explicit_return_to_direct_files() {
        let fixture = RollbackFixture::new(Some("shadow_import"));
        std::fs::remove_dir_all(&fixture.private_root).unwrap();

        assert_return_to_direct_files_after_unreadable_private_state(&fixture);
        assert!(!fixture.recovery_root.exists());
    }

    #[test]
    fn active_local_rollback_requires_and_completes_a_clean_safe_shutdown() {
        std::thread::Builder::new()
            .name("tine-sparse-active-rollback-test".into())
            .stack_size(32 * 1024 * 1024)
            .spawn(active_local_rollback_requires_and_completes_a_clean_safe_shutdown_inner)
            .unwrap()
            .join()
            .unwrap();
    }

    fn active_local_rollback_requires_and_completes_a_clean_safe_shutdown_inner() {
        let mut fixture = RollbackFixture::new(Some("shadow_import"));
        fixture.make_active();
        let handle = fixture.slot.sparse_runtime().unwrap();
        for _ in 0..128 {
            let before = handle.status().unwrap();
            if !before.watcher.pending && before.provider_pending == 0 {
                break;
            }
            handle.tick().unwrap();
        }
        let before = fixture.slot.sparse_runtime().unwrap().status().unwrap();
        assert_eq!(before.lifecycle, SyncRuntimeLifecycle::Active);
        assert!(
            before.shared_role.is_none() && before.shared_phase.is_none(),
            "fresh local activation unexpectedly named shared work: {before:?}"
        );
        assert_eq!(runtime_status(before).provider_pending, 0);

        cancel_sparse_v2_at_paths(
            &fixture.state,
            "main",
            Arc::clone(&fixture.slot),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
            shutdown_for_direct_files_escape,
        )
        .unwrap();

        assert_eq!(
            fixture
                .slot
                .sparse_runtime()
                .unwrap()
                .status()
                .unwrap()
                .lifecycle,
            SyncRuntimeLifecycle::StoppedSafe
        );
        assert!(fixture
            .state
            .graphs
            .read()
            .unwrap()
            .slot("main")
            .unwrap()
            .legacy_graph()
            .is_ok());
    }

    #[test]
    fn terminal_runtime_uses_real_confirmed_override_and_reaches_direct_files() {
        std::thread::Builder::new()
            .name("tine-sparse-forced-direct-return-test".into())
            .stack_size(32 * 1024 * 1024)
            .spawn(terminal_runtime_uses_real_confirmed_override_and_reaches_direct_files_inner)
            .unwrap()
            .join()
            .unwrap();
    }

    fn terminal_runtime_uses_real_confirmed_override_and_reaches_direct_files_inner() {
        let mut fixture = RollbackFixture::new(Some("shadow_import"));
        fixture.make_active();
        let handle = fixture.slot.sparse_runtime().unwrap();
        handle.stop_without_clean_drain().unwrap();
        assert_eq!(
            handle.status().unwrap().lifecycle,
            SyncRuntimeLifecycle::StoppedCrashed
        );

        let result = cancel_sparse_v2_at_paths(
            &fixture.state,
            "main",
            Arc::clone(&fixture.slot),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
            shutdown_for_direct_files_escape,
        )
        .unwrap();
        assert!(matches!(
            result.status.availability,
            SparseV2Availability::LegacyDefault
        ));
        assert!(result
            .recovery_statement
            .contains("had not reached durable storage may be absent"));
        assert!(fixture
            .state
            .graphs
            .read()
            .unwrap()
            .slot("main")
            .unwrap()
            .legacy_graph()
            .is_ok());
    }

    #[test]
    fn shutdown_refusal_restores_sparse_authority_and_changes_no_durable_bytes() {
        let fixture = RollbackFixture::new(Some("shadow_import"));
        let private_before = snapshot_tree(&fixture.private_root);
        let markdown_before = std::fs::read(&fixture.markdown_path).unwrap();
        let generation = fixture.slot.binding_generation;

        let error = cancel_sparse_v2_at_paths(
            &fixture.state,
            "main",
            Arc::clone(&fixture.slot),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
            |_| Err("injected force-stop refusal".into()),
        )
        .unwrap_err();

        assert!(error.contains("injected force-stop refusal"));
        let restored = fixture.state.graphs.read().unwrap().slot("main").unwrap();
        assert!(restored.is_sparse_v2());
        assert_eq!(restored.binding_generation, generation);
        assert_eq!(snapshot_tree(&fixture.private_root), private_before);
        assert_eq!(
            std::fs::read(&fixture.markdown_path).unwrap(),
            markdown_before
        );
        assert!(!fixture.recovery_root.exists());
    }

    #[test]
    fn archive_failure_publishes_a_fresh_retryable_slot_after_shutdown() {
        let fixture = RollbackFixture::new(Some("shadow_import"));
        create_empty_local_provider_scaffold(&fixture.graph_root);
        let private_before = snapshot_tree(&fixture.private_root);
        let provider_before = snapshot_tree(&fixture.graph_root.join(".tine-sync/v2"));
        let markdown_before = std::fs::read(&fixture.markdown_path).unwrap();
        let error = cancel_sparse_v2_at_paths_with_archive(
            &fixture.state,
            "main",
            Arc::clone(&fixture.slot),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
            |_| {
                assert!(fixture.state.graphs.read().unwrap().slot("main").is_none());
                Ok(DirectFilesShutdown::Clean)
            },
            |private_root, recovery_root| {
                assert_eq!(private_root, fixture.private_root);
                assert_eq!(recovery_root, fixture.recovery_root);
                let retryable = fixture.state.graphs.read().unwrap().slot("main").unwrap();
                assert!(retryable.is_sparse_v2());
                assert!(retryable.sparse_runtime().is_none());
                Err("injected archive rename failure".into())
            },
        )
        .unwrap_err();

        assert!(error.contains("injected archive rename failure"));
        let restored = fixture.state.graphs.read().unwrap().slot("main").unwrap();
        assert!(!Arc::ptr_eq(&restored, &fixture.slot));
        assert!(restored.is_sparse_v2());
        assert!(restored.sparse_runtime().is_none());
        let retry_status = sparse_v2_status_for_slot(&restored).unwrap();
        assert!(retry_status.can_retry);
        assert!(retry_status.can_cancel);
        assert_eq!(snapshot_tree(&fixture.private_root), private_before);
        assert_eq!(
            snapshot_tree(&fixture.graph_root.join(".tine-sync/v2")),
            provider_before
        );
        assert_eq!(
            std::fs::read(&fixture.markdown_path).unwrap(),
            markdown_before
        );
        assert!(!fixture.recovery_root.exists());

        let result = cancel_sparse_v2_at_paths(
            &fixture.state,
            "main",
            Arc::clone(&restored),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
            shutdown_for_direct_files_escape,
        )
        .unwrap();
        assert!(matches!(
            result.status.availability,
            SparseV2Availability::LegacyDefault
        ));
        assert!(fixture
            .state
            .graphs
            .read()
            .unwrap()
            .slot("main")
            .unwrap()
            .legacy_graph()
            .is_ok());
    }

    #[test]
    fn real_clean_shutdown_archive_failure_never_republishes_a_dead_handle() {
        std::thread::Builder::new()
            .name("tine-sparse-archive-failure-retry-test".into())
            .stack_size(32 * 1024 * 1024)
            .spawn(real_clean_shutdown_archive_failure_never_republishes_a_dead_handle_inner)
            .unwrap()
            .join()
            .unwrap();
    }

    fn real_clean_shutdown_archive_failure_never_republishes_a_dead_handle_inner() {
        let mut fixture = RollbackFixture::new(Some("shadow_import"));
        fixture.make_active();
        let error = cancel_sparse_v2_at_paths_with_archive(
            &fixture.state,
            "main",
            Arc::clone(&fixture.slot),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
            shutdown_for_direct_files_escape,
            |_, _| Err("injected private archive failure".into()),
        )
        .unwrap_err();
        assert!(error.contains("injected private archive failure"));

        let retryable = fixture.state.graphs.read().unwrap().slot("main").unwrap();
        assert!(!Arc::ptr_eq(&retryable, &fixture.slot));
        assert!(retryable.sparse_runtime().is_none());
        let retry_status = sparse_v2_status_for_slot(&retryable).unwrap();
        assert!(retry_status.can_retry);
        assert!(retry_status.can_cancel);
        assert!(matches!(
            retry_status.availability,
            SparseV2Availability::Retryable { ref stage, .. } if stage == "local_active"
        ));

        let result = cancel_sparse_v2_at_paths(
            &fixture.state,
            "main",
            Arc::clone(&retryable),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
            shutdown_for_direct_files_escape,
        )
        .unwrap();
        assert!(matches!(
            result.status.availability,
            SparseV2Availability::LegacyDefault
        ));
    }

    #[test]
    fn shared_or_provider_evidence_is_archived_before_returning_to_direct_files() {
        let provider = RollbackFixture::new(Some("shadow_import"));
        std::fs::create_dir_all(provider.graph_root.join(".tine-sync/v2")).unwrap();
        std::fs::write(
            provider.graph_root.join(".tine-sync/v2/provider-evidence"),
            b"shared",
        )
        .unwrap();
        let provider_result = cancel_sparse_v2_at_paths(
            &provider.state,
            "main",
            Arc::clone(&provider.slot),
            &provider.private_root,
            &provider.recovery_root,
            None,
            shutdown_for_direct_files_escape,
        )
        .unwrap();
        assert!(matches!(
            provider_result.status.availability,
            SparseV2Availability::LegacyDefault
        ));
        assert!(!provider.private_root.exists());
        assert!(provider.recovery_root.is_dir());
        assert!(!provider.graph_root.join(".tine-sync/v2").exists());
        assert!(provider.graph_root.join(".tine-sync/recovery").is_dir());
        assert!(provider
            .state
            .graphs
            .read()
            .unwrap()
            .slot("main")
            .unwrap()
            .legacy_graph()
            .is_ok());

        let shared = RollbackFixture::new(Some("joining"));
        let shared_result = cancel_sparse_v2_at_paths(
            &shared.state,
            "main",
            Arc::clone(&shared.slot),
            &shared.private_root,
            &shared.recovery_root,
            None,
            shutdown_for_direct_files_escape,
        )
        .unwrap();
        assert!(matches!(
            shared_result.status.availability,
            SparseV2Availability::LegacyDefault
        ));
        assert!(!shared.private_root.exists());
        assert!(shared.recovery_root.is_dir());
    }

    #[test]
    fn facade_legacy_default_inspects_nothing_and_retains_nothing() {
        let facade = SyncRuntimeFacade;
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SyncRuntimeFacade>();

        let root = std::env::temp_dir().join(format!("tine-sync-facade-legacy-{}", Uuid::new_v4()));
        let opened = facade.open_explicit(SyncRuntimeOpenRequest {
            profile: SyncStorageProfile::LegacyDefault,
            graph_root: root.join("missing-graph"),
            enrollment_root: root.join("missing-enrollment"),
            archive_root: root.join("missing-archive"),
            receipt_root: root.join("missing-receipts"),
            database_path: root.join("missing.sqlite"),
            application_runtime_root: root.join("missing-runtime"),
            migration_backup_root: root.join("missing-migration-backup"),
            provider_root: root.join("missing-provider"),
            provider_journal_root: root.join("missing-provider-journal/device/journal"),
        });
        assert_eq!(opened.status, SyncRuntimeOpenStatus::LegacyDefault);
        assert!(opened.handle.is_none());
        assert!(!root.exists());
    }

    #[test]
    fn binding_record_rejects_unknown_fields_and_wrong_roots() {
        let root = std::env::temp_dir().join(format!("tine-sparse-binding-{}", Uuid::new_v4()));
        let graph = root.join("graph");
        let other = root.join("other");
        let meta = GraphMeta {
            root: graph.display().to_string(),
            journals_dir: "journals".into(),
            pages_dir: "pages".into(),
            preferred_workflow: "now".into(),
            shortcuts: Default::default(),
            start_of_week: 6,
            block_hidden_properties: Vec::new(),
            default_journal_template: None,
            favorites: Vec::new(),
            journal_page_title_format: "MMM do, yyyy".into(),
            journal_file_name_format: "yyyy_MM_dd".into(),
            preferred_format: "md".into(),
            macros: Default::default(),
            enable_timetracking: true,
            show_brackets: true,
            doc_mode_enter_for_new_block: false,
            logical_outdenting: false,
            logbook_with_second_support: true,
            logbook_enabled_in_timestamped_blocks: false,
            logbook_enabled_in_all_blocks: false,
            guide_announced: false,
        };
        let record = SparseV2ActivationRecord::new(&graph, meta, DeviceId::new());
        let path = root.join("binding.json");
        persist_binding_at(&path, &record).unwrap();
        let reopened = read_binding_at(&path, &graph).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(reopened).unwrap(),
            serde_json::to_value(&record).unwrap()
        );
        assert!(read_binding_at(&path, &other).is_err());

        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["unknown"] = serde_json::json!(true);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_binding_at(&path, &graph).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stopped_sparse_bindings_are_retryable_reopen_candidates() {
        assert_eq!(
            action_for_runtime_lifecycle(&SyncRuntimeLifecycle::StoppedSafe),
            SparseV2BindingAction::ReopenActive
        );
        assert_eq!(
            action_for_runtime_lifecycle(&SyncRuntimeLifecycle::StoppedCrashed),
            SparseV2BindingAction::ReopenActive
        );
        assert_eq!(
            action_for_runtime_lifecycle(&SyncRuntimeLifecycle::Active),
            SparseV2BindingAction::ReturnRetained
        );
        assert_eq!(
            action_for_runtime_lifecycle(&SyncRuntimeLifecycle::Terminal),
            SparseV2BindingAction::ReturnRetained
        );
        for stage in ["local_active", "share_prepared", "joining", "shared_active"] {
            assert_eq!(
                retryable_binding(stage, "transient reopen failure".into()).action(),
                SparseV2BindingAction::ReopenActive,
                "{stage} must not strand a stopped Tauri slot"
            );
        }
    }

    #[test]
    fn public_query_wire_uses_exact_kind_and_value_envelopes() {
        let request = SyncRuntimeQueryRequest::Search {
            query: "exact wire".into(),
            limit: 7,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "kind": "search",
                "query": "exact wire",
                "limit": 7
            })
        );

        let reply = SyncRuntimeQueryReply::Search(vec![SyncSearchHitDto {
            entity: SyncEntityId::Block("block-opaque".into()),
            page_id: "page-opaque".into(),
            text: "exact wire".into(),
            rank: -0.25,
        }]);
        assert_eq!(
            serde_json::to_value(reply).unwrap(),
            serde_json::json!({
                "kind": "search",
                "value": [{
                    "entity": {
                        "entity_type": "block",
                        "id": "block-opaque"
                    },
                    "page_id": "page-opaque",
                    "text": "exact wire",
                    "rank": -0.25
                }]
            })
        );
        assert_eq!(
            serde_json::to_value(SyncRuntimeQueryReply::PageName(
                SyncPageNameResolutionDto::Missing
            ))
            .unwrap(),
            serde_json::json!({
                "kind": "page_name",
                "value": {
                    "status": "missing"
                }
            })
        );
    }

    #[test]
    fn app_boundary_activation_editor_watcher_shutdown_and_reopen_journey() {
        std::thread::Builder::new()
            .name("tine-sparse-app-boundary-test".into())
            .stack_size(32 * 1024 * 1024)
            .spawn(app_boundary_activation_editor_watcher_shutdown_and_reopen_journey_inner)
            .unwrap()
            .join()
            .unwrap();
    }

    fn app_boundary_activation_editor_watcher_shutdown_and_reopen_journey_inner() {
        let root = std::env::temp_dir().join(format!("tine-sparse-app-journey-{}", Uuid::new_v4()));
        let graph_root = root.join("graph");
        let private = root.join("private");
        let relative = "archive/層/Résumé 日本語.md";
        std::fs::create_dir_all(graph_root.join("pages")).unwrap();
        std::fs::create_dir_all(graph_root.join("journals")).unwrap();
        std::fs::create_dir_all(graph_root.join("archive/層")).unwrap();
        std::fs::write(
            graph_root.join(relative),
            "- nested UTF original content — café 日本語\n",
        )
        .unwrap();

        let graph = Graph::open(&graph_root);
        let meta = graph.meta();
        drop(graph);
        let record = SparseV2ActivationRecord::new(&graph_root, meta.clone(), DeviceId::new());
        let request = SyncLocalActivationRequest {
            graph_root: graph_root.clone(),
            archive_root: private.join("archive"),
            enrollment_root: private.join("enrollment"),
            receipt_root: private.join("receipts"),
            database_path: private.join("projection/materialization.sqlite"),
            application_runtime_root: private.join("runtime"),
            migration_backup_root: private.join("migration-backup"),
            capture_root: private.join("capture"),
            preparation_root: private.join("preparation"),
            provider_root: graph_root.join(".tine-sync/v2/shared"),
            provider_journal_root: private.join("provider/device/journal"),
            identities: SyncLocalActivationIdentities {
                workspace_id: record.workspace_id,
                lineage_digest: record.lineage_digest,
                catalog_document_id: record.catalog_document_id,
                endpoint_id: record.endpoint_id,
                device_id: record.device_id,
                preparation_id: record.preparation_id,
                session_id: record.activation_session_id,
            },
        };
        let open_request = SyncRuntimeOpenRequest {
            profile: SyncStorageProfile::ExperimentalLocal,
            graph_root: request.graph_root.clone(),
            enrollment_root: request.enrollment_root.clone(),
            archive_root: request.archive_root.clone(),
            receipt_root: request.receipt_root.clone(),
            database_path: request.database_path.clone(),
            application_runtime_root: request.application_runtime_root.clone(),
            migration_backup_root: request.migration_backup_root.clone(),
            provider_root: request.provider_root.clone(),
            provider_journal_root: request.provider_journal_root.clone(),
        };

        let activated = SyncRuntimeHandle::activate_or_resume_local(request);
        assert_eq!(activated.status, SyncLocalActivationStatus::Active);
        let binding = SparseV2Binding::from_activation(activated);
        let slot =
            crate::state::GraphSlot::from_sparse_v2(binding, graph_root.clone(), meta.clone());
        assert_eq!(
            slot.legacy_graph().err().as_deref(),
            Some(crate::state::SPARSE_V2_UNSUPPORTED)
        );
        let handle = slot
            .sparse_runtime()
            .expect("active sparse slot must retain the actor");
        let admission = slot.application_page_admission();
        assert_eq!(admission.binding_generation, slot.binding_generation);
        assert!(matches!(
            admission.authority,
            crate::state::ApplicationPageAdmissionAuthority::ManagedWritable {
                application_save_page_blocks: tine_core::sync_runtime::MAX_SYNC_EDITOR_BLOCKS,
                application_page_request_text_bytes:
                    tine_core::sync_runtime::MAX_SYNC_EDITOR_REQUEST_BYTES,
                application_page_max_depth: tine_core::sync_runtime::MAX_SYNC_EDITOR_DEPTH,
            }
        ));
        assert_eq!(
            serde_json::to_value(sparse_v2_status_for_slot(&slot).unwrap()).unwrap()
                ["application_page_admission"]["application_save_page_blocks"],
            511
        );
        for _ in 0..128 {
            match handle.tick().unwrap() {
                SyncRuntimeTick::Idle
                | SyncRuntimeTick::AdmittedNoop { .. }
                | SyncRuntimeTick::AdmittedComplete { .. }
                    if !handle.status().unwrap().watcher.pending =>
                {
                    break;
                }
                SyncRuntimeTick::Idle
                | SyncRuntimeTick::AdmittedNoop { .. }
                | SyncRuntimeTick::AdmittedComplete { .. }
                | SyncRuntimeTick::Recovering
                | SyncRuntimeTick::RetryFull
                | SyncRuntimeTick::Failed(_) => {}
                other => panic!(
                    "initial app-boundary feed did not settle: {other:?}; status={:?}",
                    handle.status().unwrap()
                ),
            }
        }

        let loaded = handle
            .load_application_page(SyncApplicationPageLoadRequest {
                page: SyncApplicationPageSelector::ExactPath {
                    path: relative.into(),
                },
            })
            .unwrap();
        let SyncApplicationPageLoadOutcome::Loaded { mut page, revision } = loaded else {
            panic!(
                "activation did not expose the existing page through the app gateway: {loaded:?}"
            );
        };
        page.blocks[0]
            .raw
            .push_str(" sparse v2 saved existing UTF page");
        let saved = handle
            .save_application_page(SyncApplicationPageSaveRequest {
                target: SyncApplicationPageSaveTarget::Existing {
                    path: relative.into(),
                    revision,
                },
                page,
            })
            .unwrap();
        let SyncApplicationPageSaveOutcome::Saved {
            page: saved_page,
            revision: saved_revision,
            ..
        } = saved
        else {
            panic!(
                "activation-imported existing page did not save through the app gateway: {saved:?}"
            );
        };
        assert_eq!(saved_page.path, relative);
        assert_eq!(
            saved_page.blocks[0].raw,
            "nested UTF original content — café 日本語 sparse v2 saved existing UTF page"
        );
        let reloaded = handle
            .load_application_page(SyncApplicationPageLoadRequest {
                page: SyncApplicationPageSelector::ExactPath {
                    path: relative.into(),
                },
            })
            .unwrap();
        assert!(
            matches!(
                &reloaded,
                SyncApplicationPageLoadOutcome::Loaded {
                    page,
                    revision,
                } if page.path == relative
                    && page.blocks[0].raw
                        == "nested UTF original content — café 日本語 sparse v2 saved existing UTF page"
                    && revision == &saved_revision
            ),
            "application gateway did not reload its accepted semantic result: {reloaded:?}"
        );
        assert_eq!(
            std::fs::read_to_string(graph_root.join(relative)).unwrap(),
            "- nested UTF original content — café 日本語 sparse v2 saved existing UTF page\n"
        );
        let independently_parsed = Graph::open(&graph_root)
            .load_by_path(relative)
            .unwrap()
            .expect("materialized nested UTF page must remain independently parseable");
        assert_eq!(independently_parsed.path, relative);
        assert_eq!(
            independently_parsed.blocks[0].raw,
            "nested UTF original content — café 日本語 sparse v2 saved existing UTF page"
        );

        let loaded = handle
            .load_editor_page(SyncEditorLoadRequest {
                page: SyncEditorPageSelector::Name {
                    name: "Boundary page".into(),
                    page_kind: SyncPageKind::Page,
                },
            })
            .unwrap();
        let SyncEditorLoadOutcome::NewPage { draft } = loaded else {
            panic!("activation did not expose a frontier-bound new-page draft: {loaded:?}");
        };
        let saved = handle
            .save_editor_page(SyncEditorSaveRequest {
                target: SyncEditorSaveTarget::New {
                    name: draft.name,
                    page_kind: draft.page_kind,
                    revision: draft.revision,
                    format: None,
                },
                preamble: None,
                blocks: vec![SyncEditorBlockDto {
                    key: SyncEditorBlockKey::Temporary("first".into()),
                    parent: None,
                    content: "edited through Tauri boundary".into(),
                }],
            })
            .unwrap();
        if matches!(
            saved,
            SyncEditorSaveOutcome::Deferred {
                state: tine_core::sync_runtime::SyncEditorDeferred::RetryableRetainedPublication { .. },
                ..
            }
        ) {
            let mut durable = false;
            for _ in 0..64 {
                if matches!(
                    handle.tick().unwrap(),
                    SyncRuntimeTick::LocalMutation(SyncLocalMutationOutcome::Durable { .. })
                ) {
                    durable = true;
                    break;
                }
            }
            assert!(durable, "retained new-page save did not become durable");
        } else {
            assert!(
                matches!(saved, SyncEditorSaveOutcome::Durable { .. }),
                "new-page save after existing-page save was not accepted: {saved:?}"
            );
        }

        let searched = handle
            .query(SyncRuntimeQueryRequest::Search {
                query: "Tauri boundary".into(),
                limit: 10,
            })
            .unwrap();
        assert!(
            matches!(searched, SyncRuntimeQueryReply::Search(ref rows) if !rows.is_empty()),
            "actor query must see the durable editor save: {searched:?}"
        );
        std::fs::write(
            graph_root.join(relative),
            "- externally imported through watcher\n- second external block\n",
        )
        .unwrap();
        handle
            .observe_watcher(vec![SyncWatcherObservation::managed_path(relative).unwrap()])
            .unwrap();
        let mut imported = false;
        for _ in 0..64 {
            match handle.tick().unwrap() {
                SyncRuntimeTick::AdmittedComplete { .. } | SyncRuntimeTick::AdmittedNoop { .. } => {
                    imported = true;
                    break;
                }
                SyncRuntimeTick::Idle
                | SyncRuntimeTick::Recovering
                | SyncRuntimeTick::RetryFull
                | SyncRuntimeTick::LocalMutation(_) => {}
                other => panic!("watcher import failed at app boundary: {other:?}"),
            }
        }
        assert!(
            imported,
            "watcher import did not settle within its bounded turns"
        );
        let reloaded = handle
            .load_editor_page(SyncEditorLoadRequest {
                page: SyncEditorPageSelector::Name {
                    name: "Résumé 日本語".into(),
                    page_kind: SyncPageKind::Page,
                },
            })
            .unwrap();
        assert!(
            matches!(
                reloaded,
                SyncEditorLoadOutcome::Loaded { ref page }
                    if page.blocks.len() == 2
                        && page.blocks[0].content == "externally imported through watcher"
                        && page.blocks[1].content == "second external block"
            ),
            "editor load must observe the watcher-authored batch: {reloaded:?}"
        );
        let application_reloaded = handle
            .load_application_page(SyncApplicationPageLoadRequest {
                page: SyncApplicationPageSelector::ExactPath {
                    path: relative.into(),
                },
            })
            .unwrap();
        assert!(
            matches!(
                application_reloaded,
                SyncApplicationPageLoadOutcome::Loaded { ref page, .. }
                    if page.blocks.len() == 2
                        && page.blocks[0].raw == "externally imported through watcher"
                        && page.blocks[1].raw == "second external block"
            ),
            "application load must observe the watcher-authored batch: {application_reloaded:?}"
        );

        assert!(matches!(
            clean_shutdown_slot(&slot).unwrap(),
            CleanShutdownSlot::Safe
        ));
        let stopped = slot
            .sparse_binding()
            .expect("the stopped slot must remain sparse");
        assert_eq!(stopped.action(), SparseV2BindingAction::ReopenActive);
        let stopped_status = SparseV2StatusDto::from_binding(stopped, slot.binding_generation);
        assert!(matches!(
            stopped_status.availability,
            SparseV2Availability::Retryable { ref stage, ref detail }
                if stage == "local_active" && detail.contains("stopped safely")
        ));
        assert!(stopped_status.can_retry);
        assert_eq!(
            serde_json::to_value(stopped_status).unwrap()["application_page_admission"]
                ["authority"],
            "managed_unavailable"
        );
        drop(slot);

        let reopened = SyncRuntimeHandle::open(open_request);
        assert_eq!(reopened.status, SyncRuntimeOpenStatus::Active);
        let reopened = SparseV2Binding::from_open(reopened);
        let reopened_slot = crate::state::GraphSlot::from_sparse_v2(reopened, graph_root, meta);
        let reply = reopened_slot
            .sparse_runtime()
            .unwrap()
            .query(SyncRuntimeQueryRequest::Search {
                query: "externally imported".into(),
                limit: 10,
            })
            .unwrap();
        assert!(matches!(
            reply,
            SyncRuntimeQueryReply::Search(ref rows) if !rows.is_empty()
        ));
        assert!(matches!(
            clean_shutdown_slot(&reopened_slot).unwrap(),
            CleanShutdownSlot::Safe
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stopped_move_episode_reopens_and_replaces_one_window_writer() {
        std::thread::Builder::new()
            .name("tine-move-recovery-handoff-test".into())
            .stack_size(32 * 1024 * 1024)
            .spawn(stopped_move_episode_reopens_and_replaces_one_window_writer_inner)
            .unwrap()
            .join()
            .unwrap();
    }

    fn stopped_move_episode_reopens_and_replaces_one_window_writer_inner() {
        let root =
            std::env::temp_dir().join(format!("tine-move-recovery-handoff-{}", Uuid::new_v4()));
        let graph_root = root.join("graph");
        let private = root.join("private");
        let source_path = "pages/Recovery Source.md";
        let destination_path = "pages/Recovery Destination.md";
        std::fs::create_dir_all(graph_root.join("pages")).unwrap();
        std::fs::create_dir_all(graph_root.join("journals")).unwrap();
        std::fs::write(graph_root.join(source_path), b"- source root\n").unwrap();
        std::fs::write(graph_root.join(destination_path), b"- destination root\n").unwrap();

        let meta = Graph::open(&graph_root).meta();
        let record = SparseV2ActivationRecord::new(&graph_root, meta.clone(), DeviceId::new());
        let activation_request = SyncLocalActivationRequest {
            graph_root: graph_root.clone(),
            archive_root: private.join("archive"),
            enrollment_root: private.join("enrollment"),
            receipt_root: private.join("receipts"),
            database_path: private.join("projection/materialization.sqlite"),
            application_runtime_root: private.join("runtime"),
            migration_backup_root: private.join("migration-backup"),
            capture_root: private.join("capture"),
            preparation_root: private.join("preparation"),
            provider_root: graph_root.join(".tine-sync/v2/shared"),
            provider_journal_root: private.join("provider/device/journal"),
            identities: SyncLocalActivationIdentities {
                workspace_id: record.workspace_id,
                lineage_digest: record.lineage_digest,
                catalog_document_id: record.catalog_document_id,
                endpoint_id: record.endpoint_id,
                device_id: record.device_id,
                preparation_id: record.preparation_id,
                session_id: record.activation_session_id,
            },
        };
        let open_request = SyncRuntimeOpenRequest {
            profile: SyncStorageProfile::ExperimentalLocal,
            graph_root: activation_request.graph_root.clone(),
            enrollment_root: activation_request.enrollment_root.clone(),
            archive_root: activation_request.archive_root.clone(),
            receipt_root: activation_request.receipt_root.clone(),
            database_path: activation_request.database_path.clone(),
            application_runtime_root: activation_request.application_runtime_root.clone(),
            migration_backup_root: activation_request.migration_backup_root.clone(),
            provider_root: activation_request.provider_root.clone(),
            provider_journal_root: activation_request.provider_journal_root.clone(),
        };

        let activated = SyncRuntimeHandle::activate_or_resume_local(activation_request);
        assert_eq!(activated.status, SyncLocalActivationStatus::Active);
        let predecessor = Arc::new(crate::state::GraphSlot::from_sparse_v2(
            SparseV2Binding::from_activation(activated),
            graph_root.clone(),
            meta.clone(),
        ));
        let state = crate::state::AppState {
            graphs: std::sync::RwLock::new(crate::state::GraphRegistry::default()),
            graph_load: Mutex::new(()),
            watch_ctl: Mutex::new(None),
            last_focused: Mutex::new(None),
            capture_graph: Mutex::new(None),
            startup_recovery: Mutex::new(std::collections::HashMap::new()),
            sync_runtime: SyncRuntimeFacade,
            #[cfg(desktop)]
            next_window: std::sync::atomic::AtomicU64::new(1),
        };
        state
            .graphs
            .write()
            .unwrap()
            .bind("main".into(), Arc::clone(&predecessor))
            .unwrap();
        let handle = predecessor.sparse_runtime().unwrap();
        for _ in 0..128 {
            let status = handle.status().unwrap();
            if !status.watcher.pending && status.provider_pending == 0 {
                break;
            }
            handle.tick().unwrap();
        }
        let source = match handle
            .load_application_page(SyncApplicationPageLoadRequest {
                page: SyncApplicationPageSelector::ExactPath {
                    path: source_path.into(),
                },
            })
            .unwrap()
        {
            SyncApplicationPageLoadOutcome::Loaded { page, revision } => (page, revision),
            other => panic!("source page did not load: {other:?}"),
        };
        let destination = match handle
            .load_application_page(SyncApplicationPageLoadRequest {
                page: SyncApplicationPageSelector::ExactPath {
                    path: destination_path.into(),
                },
            })
            .unwrap()
        {
            SyncApplicationPageLoadOutcome::Loaded { page, revision } => (page, revision),
            other => panic!("destination page did not load: {other:?}"),
        };
        let request = SyncApplicationMoveSubtreesRequest {
            episode_id: Uuid::new_v4().to_string(),
            source_path: source.0.path.clone(),
            source_revision: source.1,
            destination_path: destination.0.path.clone(),
            destination_revision: destination.1,
            roots: vec![SyncApplicationMoveRoot {
                identity: source.0.blocks[0].id.clone(),
                raw_rewrite: None,
            }],
            placement: SyncApplicationMovePlacement::Root { position: 0 },
            admission: SyncApplicationMoveAdmission {
                application_save_page_blocks: tine_core::sync_runtime::MAX_SYNC_EDITOR_BLOCKS,
                application_page_request_text_bytes:
                    tine_core::sync_runtime::MAX_SYNC_EDITOR_REQUEST_BYTES,
                application_page_max_depth: tine_core::sync_runtime::MAX_SYNC_EDITOR_DEPTH,
            },
        };
        // The ordinary move surface creates and commits the durable episode.
        // `resolve_application_move_subtrees` is recovery-only: asking it to
        // resolve a missing episode must remain a NoCommit rather than
        // silently initiating a user mutation (GH #333).
        let committed = handle.move_application_subtrees(request.clone()).unwrap();
        assert!(matches!(
            committed,
            SyncApplicationMoveSubtreesOutcome::Committed { .. }
        ));
        assert!(matches!(
            clean_shutdown_slot(&predecessor).unwrap(),
            CleanShutdownSlot::Safe
        ));
        assert_eq!(
            predecessor.sparse_binding().unwrap().action(),
            SparseV2BindingAction::ReopenActive
        );
        let previous_generation = predecessor.binding_generation;

        let failed = recover_managed_application_subtrees_with(
            &state,
            "main",
            previous_generation,
            request.clone(),
            |_| Err("pre-publication reopen failure".into()),
        );
        assert_eq!(failed.unwrap_err(), "pre-publication reopen failure");
        assert_eq!(
            state
                .graphs
                .read()
                .unwrap()
                .slot("main")
                .unwrap()
                .binding_generation,
            previous_generation
        );

        let result = recover_managed_application_subtrees_with(
            &state,
            "main",
            previous_generation,
            request.clone(),
            |_| {
                let opened = SyncRuntimeHandle::open(open_request.clone());
                assert_eq!(opened.status, SyncRuntimeOpenStatus::Active);
                Ok((SparseV2Binding::from_open(opened), meta.clone()))
            },
        )
        .unwrap();
        assert_eq!(result.previous_binding_generation, previous_generation);
        assert_ne!(result.binding_generation, previous_generation);
        assert_eq!(result.episode_id, request.episode_id);
        assert_eq!(
            result.application_page_admission.binding_generation,
            result.binding_generation
        );
        assert_eq!(
            result.status.application_page_admission,
            result.application_page_admission
        );
        let result_generation = result.binding_generation;
        match result.outcome {
            SyncApplicationMoveSubtreesOutcome::Committed {
                recovered: true,
                source,
                destination,
                ..
            } => {
                assert!(source.page.blocks.is_empty());
                assert_eq!(destination.page.blocks.len(), 2);
            }
            other => panic!("reopened episode did not resolve: {other:?}"),
        }
        let current = state.graphs.read().unwrap().slot("main").unwrap();
        assert_eq!(current.binding_generation, result_generation);
        assert_eq!(
            predecessor
                .sparse_runtime()
                .unwrap()
                .status()
                .unwrap()
                .lifecycle,
            SyncRuntimeLifecycle::StoppedSafe
        );
        let duplicate = SyncRuntimeHandle::open(open_request);
        assert!(matches!(
            duplicate.status,
            SyncRuntimeOpenStatus::OpenRefused { .. }
        ));
        assert!(duplicate.handle.is_none());
        assert!(matches!(
            clean_shutdown_slot(&current).unwrap(),
            CleanShutdownSlot::Safe
        ));
        let _ = std::fs::remove_dir_all(root);
    }
}
