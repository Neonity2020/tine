//! Explicit Tauri-facing sparse-v2 runtime composition.
//!
//! A durable caller-owned binding in private app data is the opt-in marker.
//! Ordinary graph loading never creates it. Once present, startup discovers
//! sparse state and never falls back to a legacy `Graph` writer.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Manager;
use tine_core::model::GraphMeta;
use tine_core::oplog::{
    DeviceId, DocumentId, LineageDigest, ProjectionEndpointId, SessionId, WorkspaceId,
};
use tine_core::sync_runtime::{
    inspect_shared_enrollment, SyncAmbiguousEvidence, SyncLocalActivationIdentities,
    SyncLocalActivationRequest, SyncLocalActivationResult, SyncLocalActivationStage,
    SyncLocalActivationStatus, SyncNonActiveStage, SyncRuntimeComponent, SyncRuntimeHandle,
    SyncRuntimeLifecycle, SyncRuntimeOpenRequest, SyncRuntimeOpenResult, SyncRuntimeOpenStatus,
    SyncRuntimeRecovery, SyncRuntimeStatusSnapshot, SyncRuntimeTick,
    SyncSharedEnrollmentDescriptor, SyncSharedPhase, SyncSharedRole, SyncShutdownOutcome,
    SyncStorageProfile,
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
            return Err(format!(
                "unsupported sparse-v2 binding schema {}",
                self.schema_version
            ));
        }
        if self.graph_root != graph_root.display().to_string()
            || self.graph_meta.root != self.graph_root
        {
            return Err("sparse-v2 binding belongs to another graph root".into());
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
        Err(error) => return Err(format!("couldn't read sparse-v2 binding: {error}")),
    };
    let record: SparseV2ActivationRecord = serde_json::from_slice(&bytes)
        .map_err(|error| format!("sparse-v2 binding is corrupt: {error}"))?;
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
                "another sparse-v2 activation binding already owns this graph",
            ));
        }
        Ok(encoded.clone())
    })
    .map_err(|error| format!("couldn't persist sparse-v2 binding: {error}"))
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
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
                detail: "explicit sparse-v2 activation has not completed".into(),
            },
            SyncRuntimeOpenStatus::ExistingNonActive(stage) => Self::Retryable {
                stage: non_active_stage(stage).into(),
                detail: "explicit sparse-v2 activation can be resumed".into(),
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
            SyncLocalActivationStatus::LegacyV1Refused => Self::Refused {
                reason_code: "legacy_v1_present".into(),
                detail: Some(
                    "sparse v2 never opens or rewrites the experimental legacy v1 store".into(),
                ),
            },
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

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
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

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct SparseV2TickDto {
    state: String,
    detail: Option<String>,
    epoch: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct SparseV2RuntimeStatusDto {
    lifecycle: String,
    recovery: Option<String>,
    watcher: SparseV2WatcherStatusDto,
    last_tick: Option<SparseV2TickDto>,
    detail: Option<String>,
    shared_role: Option<String>,
    shared_phase: Option<String>,
    provider_pending: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct SparseV2StatusDto {
    #[serde(flatten)]
    availability: SparseV2Availability,
    runtime: Option<SparseV2RuntimeStatusDto>,
    can_activate: bool,
    can_retry: bool,
    can_cancel: bool,
    cancel_reason: Option<String>,
    binding_generation: u64,
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
                "This graph exposes shared sparse-v2 enrollment evidence; cancellation is unsafe."
                    .into(),
            ),
            binding_generation,
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
                detail: "the prior sparse-v2 runtime stopped safely and must be reopened".into(),
            },
            Some("stopped_crashed") => SparseV2Availability::Retryable {
                stage: "local_active".into(),
                detail: "the prior sparse-v2 runtime stopped unexpectedly and must be reopened"
                    .into(),
            },
            None if retained_status.as_ref().is_some_and(Result::is_err) => {
                SparseV2Availability::Retryable {
                    stage: "local_active".into(),
                    detail: "the retained sparse-v2 runtime is unavailable and must be reopened"
                        .into(),
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

fn provider_namespace_has_evidence(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "couldn't inspect sparse-v2 provider evidence: {error}"
        )),
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

fn cancel_eligibility(binding: &SparseV2Binding, provider_namespace: &Path) -> Result<(), String> {
    let provider_evidence = provider_namespace_has_evidence(provider_namespace)?;
    if binding_names_shared_state(binding) {
        return Err(
            "This sparse-v2 binding has entered shared enrollment, so returning to standard Markdown mode is unsafe."
                .into(),
        );
    }
    if let Some(handle) = binding.handle() {
        let status = handle
            .status()
            .map_err(|error| format!("couldn't prove sparse-v2 rollback is local: {error}"))?;
        let names_shared_runtime = status.shared_role.is_some() || status.shared_phase.is_some();
        // A local-only core snapshot currently counts its absent provider
        // recovery-coverage sentinel as one pending item. It cannot represent
        // provider work when both shared runtime identity and the complete
        // graph-local provider namespace are absent.
        if names_shared_runtime || (status.provider_pending != 0 && provider_evidence) {
            return Err(
                "This sparse-v2 runtime names shared/provider work, so returning to standard Markdown mode is unsafe."
                    .into(),
            );
        }
    }
    if provider_evidence {
        return Err(
            "Shared/provider sparse-v2 evidence exists, so returning to standard Markdown mode is unsafe."
                .into(),
        );
    }
    Ok(())
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
                "sparse-v2 private app-data residue exists without a valid binding; legacy open refused"
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
        Ok(SparseV2Binding::from_open(SyncRuntimeHandle::open(
            record.open_request(app)?,
        )))
    }

    pub(crate) fn activate_record(
        &self,
        app: &tauri::AppHandle,
        record: &SparseV2ActivationRecord,
    ) -> Result<SparseV2Binding, String> {
        Ok(SparseV2Binding::from_activation(
            SyncRuntimeHandle::activate_or_resume_local(record.activation_request(app)?),
        ))
    }

    #[cfg(test)]
    fn open_explicit(&self, request: SyncRuntimeOpenRequest) -> SyncRuntimeOpenResult {
        SyncRuntimeHandle::open(request)
    }
}

const LEGACY_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const SPARSE_V2_NOT_ACTIVE: &str =
    "sparse-v2 authority has no live runtime; retry sparse-v2 activation or roll back to standard Markdown mode";

pub(crate) fn active_handle(
    slot: &crate::state::GraphSlot,
) -> Result<&tine_core::sync_runtime::SyncRuntimeHandle, String> {
    slot.sparse_runtime()
        .ok_or_else(|| SPARSE_V2_NOT_ACTIVE.to_string())
}

fn sparse_v2_status_for_slot(slot: &crate::state::GraphSlot) -> Result<SparseV2StatusDto, String> {
    Ok(match slot.sparse_binding() {
        Some(binding) => {
            let mut status = SparseV2StatusDto::from_binding(binding, slot.binding_generation);
            match cancel_eligibility(binding, &slot.root_key.join(".tine-sync/v2")) {
                Ok(()) => {
                    status.can_cancel = true;
                    status.cancel_reason = None;
                }
                Err(reason) => {
                    status.can_cancel = false;
                    status.cancel_reason = Some(reason);
                }
            }
            status
        }
        None => match inspect_shared_enrollment(&slot.root_key.join(".tine-sync/v2/shared"))? {
            Some(descriptor) => SparseV2StatusDto::joinable(slot.binding_generation, &descriptor),
            None => SparseV2StatusDto::legacy(slot.binding_generation),
        },
    })
}

#[tauri::command]
pub(crate) fn sparse_v2_status(
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2StatusDto, String> {
    let slot = crate::state::slot_for_context(&state)?;
    sparse_v2_status_for_slot(&slot)
}

/// Explicitly retire one legacy authority and activate/resume sparse v2.
///
/// The durable opt-in record is published only after the legacy watcher,
/// detached background work, and every in-flight legacy command have released
/// their tracked graph leases. Once the record exists, every result (including
/// retryable/blocked) is published as sparse authority; there is no writer
/// fallback.
#[tauri::command]
pub(crate) fn activate_sparse_v2(
    app: tauri::AppHandle,
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2StatusDto, String> {
    let label = state.window.label().to_string();
    let _transition = state.state.graph_load.lock().unwrap();
    let slot = crate::state::slot_for_context(&state)?;
    let root = slot.root_key.clone();

    if let Some(binding) = slot.sparse_binding() {
        let action = binding.action();
        if action == SparseV2BindingAction::ReturnRetained {
            return sparse_v2_status_for_slot(&slot);
        }
        let record = state
            .state
            .sync_runtime
            .binding_record(&app, &root)?
            .ok_or("sparse-v2 opt-in binding is missing")?;
        let graph_meta = SyncRuntimeFacade::graph_meta(&record);
        let binding = match action {
            SparseV2BindingAction::ReopenActive => {
                state.state.sync_runtime.open_record(&app, &record)?
            }
            SparseV2BindingAction::ActivateOrResume => {
                state.state.sync_runtime.activate_record(&app, &record)?
            }
            SparseV2BindingAction::ReturnRetained => {
                unreachable!("retained bindings return before replacement")
            }
        };
        let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
            binding, root, graph_meta,
        ));
        state
            .state
            .graphs
            .write()
            .unwrap()
            .bind(label, Arc::clone(&replacement))?;
        crate::state::poke_watcher(&state.state);
        return sparse_v2_status_for_slot(&replacement);
    }

    let graph = slot.legacy_graph()?;
    let graph_meta = graph.meta();
    drop(graph);
    let record =
        state
            .state
            .sync_runtime
            .prepare_binding_record(&app, &root, graph_meta.clone())?;

    slot.begin_legacy_retirement()?;
    let removed = state.state.graphs.write().unwrap().remove(&label);
    if removed
        .as_ref()
        .is_none_or(|removed| removed.binding_generation != slot.binding_generation)
    {
        slot.cancel_legacy_retirement()?;
        return Err("graph binding changed during sparse-v2 handoff".into());
    }
    crate::state::poke_watcher(&state.state);

    if let Err(error) = slot.wait_for_legacy_drain(LEGACY_DRAIN_TIMEOUT) {
        slot.cancel_legacy_retirement()?;
        state
            .state
            .graphs
            .write()
            .unwrap()
            .bind(label, Arc::clone(&slot))?;
        crate::state::poke_watcher(&state.state);
        return Err(format!("sparse-v2 handoff is retryable: {error}"));
    }

    if let Err(error) = state
        .state
        .sync_runtime
        .persist_binding_record(&app, &record)
    {
        slot.cancel_legacy_retirement()?;
        state
            .state
            .graphs
            .write()
            .unwrap()
            .bind(label, Arc::clone(&slot))?;
        crate::state::poke_watcher(&state.state);
        return Err(error);
    }

    let binding = state.state.sync_runtime.activate_record(&app, &record)?;
    let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
        binding, root, graph_meta,
    ));
    state
        .state
        .graphs
        .write()
        .unwrap()
        .bind(label, Arc::clone(&replacement))?;
    crate::state::poke_watcher(&state.state);
    sparse_v2_status_for_slot(&replacement)
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct SparseV2CancelResult {
    status: SparseV2StatusDto,
    binding_generation: u64,
    recovery_statement: String,
}

fn archive_private_root(private_root: &Path, recovery_root: &Path) -> Result<PathBuf, String> {
    let metadata = std::fs::symlink_metadata(private_root)
        .map_err(|error| format!("couldn't inspect sparse-v2 private state: {error}"))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err("sparse-v2 private state is not a local directory; rollback refused".into());
    }
    std::fs::create_dir_all(recovery_root)
        .map_err(|error| format!("couldn't prepare sparse-v2 recovery storage: {error}"))?;
    let key = private_root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("sparse-v2 private state has no valid local key")?;
    let destination = recovery_root.join(format!("{key}-{}", Uuid::new_v4()));
    std::fs::rename(private_root, &destination).map_err(|error| {
        format!("couldn't preserve sparse-v2 recovery state atomically: {error}")
    })?;
    Ok(destination)
}

fn require_safe_sparse_shutdown(slot: &crate::state::GraphSlot) -> Result<(), String> {
    let Some(handle) = slot.sparse_runtime() else {
        return Ok(());
    };
    match handle.clean_shutdown() {
        Ok(SyncShutdownOutcome::Safe(_)) => Ok(()),
        Ok(SyncShutdownOutcome::Terminal(_)) => {
            Err("sparse-v2 clean shutdown did not prove a safe local stop".into())
        }
        Err(error) => Err(format!("sparse-v2 clean shutdown refused: {error}")),
    }
}

fn restore_sparse_slot(
    state: &crate::state::AppState,
    label: &str,
    slot: Arc<crate::state::GraphSlot>,
    reason: String,
) -> Result<SparseV2CancelResult, String> {
    state
        .graphs
        .write()
        .unwrap()
        .bind(label.to_string(), slot)
        .map_err(|restore| {
            format!("{reason}; sparse authority could not be restored in memory: {restore}")
        })?;
    crate::state::poke_watcher(state);
    Err(reason)
}

fn cancel_sparse_v2_at_paths_with_archive(
    state: &crate::state::AppState,
    label: &str,
    slot: Arc<crate::state::GraphSlot>,
    private_root: &Path,
    recovery_root: &Path,
    approved_assets: Option<&Path>,
    shutdown: impl FnOnce(&crate::state::GraphSlot) -> Result<(), String>,
    archive: impl FnOnce(&Path, &Path) -> Result<PathBuf, String>,
) -> Result<SparseV2CancelResult, String> {
    let binding = slot
        .sparse_binding()
        .ok_or("this graph is already using standard Markdown authority")?;
    let record = read_binding_at(&private_root.join(SPARSE_BINDING_FILE), &slot.root_key)?
        .ok_or("the exact sparse-v2 binding for this graph is missing")?;
    if record.graph_meta.root != slot.root_key.display().to_string()
        || slot.graph_meta().root != record.graph_meta.root
    {
        return Err("the sparse-v2 slot and persisted binding do not name the exact graph".into());
    }
    cancel_eligibility(binding, &slot.root_key.join(".tine-sync/v2"))?;

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
        return Err("graph binding changed during sparse-v2 rollback".into());
    }

    if let Err(error) = shutdown(&slot) {
        return restore_sparse_slot(state, label, slot, error);
    }

    if let Err(error) = archive(private_root, recovery_root) {
        return restore_sparse_slot(state, label, slot, error);
    }

    let graph = tine_core::model::Graph::open_checked_with_assets(
        &record.graph_root,
        approved_assets,
    )
    .map_err(|error| {
        format!(
            "Sparse-v2 state was safely preserved, but standard Markdown could not reopen: {error}. Restart Tine to reopen the unchanged Markdown graph."
        )
    })?;
    let replacement = Arc::new(crate::state::GraphSlot::new(graph, slot.root_key.clone()));
    state
        .graphs
        .write()
        .unwrap()
        .bind(label.to_string(), Arc::clone(&replacement))
        .map_err(|error| {
            format!(
                "Sparse-v2 state was safely preserved, but standard Markdown could not be rebound: {error}. Restart Tine to reopen the unchanged Markdown graph."
            )
        })?;
    crate::state::poke_watcher(state);
    let status = SparseV2StatusDto::legacy(replacement.binding_generation);
    Ok(SparseV2CancelResult {
        binding_generation: replacement.binding_generation,
        status,
        recovery_statement:
            "Standard Markdown mode is active. The complete private sparse-v2 state was preserved in app recovery storage."
                .into(),
    })
}

fn cancel_sparse_v2_at_paths(
    state: &crate::state::AppState,
    label: &str,
    slot: Arc<crate::state::GraphSlot>,
    private_root: &Path,
    recovery_root: &Path,
    approved_assets: Option<&Path>,
    shutdown: impl FnOnce(&crate::state::GraphSlot) -> Result<(), String>,
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

#[tauri::command]
pub(crate) fn cancel_sparse_v2(
    app: tauri::AppHandle,
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2CancelResult, String> {
    let label = state.window.label().to_string();
    let _transition = state.state.graph_load.lock().unwrap();
    let slot = crate::state::slot_for_context(&state)?;
    let private_root = sparse_private_root(&app, &slot.root_key)?;
    let recovery_root = sparse_recovery_root(&app)?;
    let approved_assets = crate::settings::approved_external_assets(&app, &slot.root_key);
    cancel_sparse_v2_at_paths(
        &state.state,
        &label,
        slot,
        &private_root,
        &recovery_root,
        approved_assets.as_deref(),
        require_safe_sparse_shutdown,
    )
}

/// Publish the already-safe local archive into the single shared v2
/// namespace, then reopen the same private device binding as SharedActive.
#[tauri::command]
pub(crate) fn prepare_sparse_v2_share(
    app: tauri::AppHandle,
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2StatusDto, String> {
    let label = state.window.label().to_string();
    let _transition = state.state.graph_load.lock().unwrap();
    let slot = crate::state::slot_for_context(&state)?;
    let record = state
        .state
        .sync_runtime
        .binding_record(&app, &slot.root_key)?
        .ok_or("sparse-v2 opt-in binding is missing")?;
    active_handle(&slot)?
        .prepare_shared()
        .map_err(|error| error.to_string())?;
    let binding = match state.state.sync_runtime.open_record(&app, &record) {
        Ok(binding) => binding,
        Err(error) => {
            let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
                retryable_binding("share_prepared", error.clone()),
                slot.root_key.clone(),
                SyncRuntimeFacade::graph_meta(&record),
            ));
            state
                .state
                .graphs
                .write()
                .unwrap()
                .bind(label, replacement)?;
            crate::state::poke_watcher(&state.state);
            return Err(error);
        }
    };
    let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
        binding,
        slot.root_key.clone(),
        SyncRuntimeFacade::graph_meta(&record),
    ));
    state
        .state
        .graphs
        .write()
        .unwrap()
        .bind(label, Arc::clone(&replacement))?;
    crate::state::poke_watcher(&state.state);
    sparse_v2_status_for_slot(&replacement)
}

/// Explicitly retire the second device's legacy reader/watcher, derive its
/// private identity from exact provider descriptor evidence, and join.
#[tauri::command]
pub(crate) fn join_sparse_v2_shared(
    app: tauri::AppHandle,
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2StatusDto, String> {
    let label = state.window.label().to_string();
    let _transition = state.state.graph_load.lock().unwrap();
    let slot = crate::state::slot_for_context(&state)?;
    let descriptor = inspect_shared_enrollment(&slot.root_key.join(".tine-sync/v2/shared"))?
        .ok_or("the shared enrollment descriptor is not provider-visible")?;
    if slot.sparse_binding().is_some() {
        let record = state
            .state
            .sync_runtime
            .binding_record(&app, &slot.root_key)?
            .ok_or("sparse-v2 opt-in binding is missing")?;
        active_handle(&slot)?
            .join_shared(descriptor)
            .map_err(|error| error.to_string())?;
        let reopened = match state.state.sync_runtime.open_record(&app, &record) {
            Ok(binding) => binding,
            Err(error) => {
                let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
                    retryable_binding("shared_active", error.clone()),
                    slot.root_key.clone(),
                    SyncRuntimeFacade::graph_meta(&record),
                ));
                state
                    .state
                    .graphs
                    .write()
                    .unwrap()
                    .bind(label, replacement)?;
                crate::state::poke_watcher(&state.state);
                return Err(error);
            }
        };
        let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
            reopened,
            slot.root_key.clone(),
            SyncRuntimeFacade::graph_meta(&record),
        ));
        state
            .state
            .graphs
            .write()
            .unwrap()
            .bind(label, Arc::clone(&replacement))?;
        crate::state::poke_watcher(&state.state);
        return sparse_v2_status_for_slot(&replacement);
    }
    let graph = slot.legacy_graph()?;
    let graph_meta = graph.meta();
    drop(graph);
    let record = SparseV2ActivationRecord::from_shared(
        &slot.root_key,
        graph_meta.clone(),
        DeviceId::from_uuid(crate::settings::managed_sync_device_id(&app)?),
        &descriptor,
    );

    slot.begin_legacy_retirement()?;
    let removed = state.state.graphs.write().unwrap().remove(&label);
    if removed
        .as_ref()
        .is_none_or(|removed| removed.binding_generation != slot.binding_generation)
    {
        slot.cancel_legacy_retirement()?;
        return Err("graph binding changed during sparse-v2 join handoff".into());
    }
    crate::state::poke_watcher(&state.state);
    if let Err(error) = slot.wait_for_legacy_drain(LEGACY_DRAIN_TIMEOUT) {
        slot.cancel_legacy_retirement()?;
        state
            .state
            .graphs
            .write()
            .unwrap()
            .bind(label, Arc::clone(&slot))?;
        crate::state::poke_watcher(&state.state);
        return Err(format!("sparse-v2 join handoff is retryable: {error}"));
    }
    if let Err(error) = state
        .state
        .sync_runtime
        .persist_binding_record(&app, &record)
    {
        slot.cancel_legacy_retirement()?;
        state
            .state
            .graphs
            .write()
            .unwrap()
            .bind(label, Arc::clone(&slot))?;
        crate::state::poke_watcher(&state.state);
        return Err(error);
    }
    let activated = match state.state.sync_runtime.activate_record(&app, &record) {
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
                .state
                .graphs
                .write()
                .unwrap()
                .bind(label, replacement)?;
            crate::state::poke_watcher(&state.state);
            return Err(error);
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
            .state
            .graphs
            .write()
            .unwrap()
            .bind(label, replacement)?;
        crate::state::poke_watcher(&state.state);
        return Err(detail);
    };
    if let Err(error) = handle.join_shared(descriptor) {
        let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
            activated,
            slot.root_key.clone(),
            graph_meta,
        ));
        state
            .state
            .graphs
            .write()
            .unwrap()
            .bind(label, replacement)?;
        crate::state::poke_watcher(&state.state);
        return Err(error.to_string());
    }
    let binding = match state.state.sync_runtime.open_record(&app, &record) {
        Ok(binding) => binding,
        Err(error) => {
            let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
                retryable_binding("shared_active", error.clone()),
                slot.root_key.clone(),
                SyncRuntimeFacade::graph_meta(&record),
            ));
            state
                .state
                .graphs
                .write()
                .unwrap()
                .bind(label, replacement)?;
            crate::state::poke_watcher(&state.state);
            return Err(error);
        }
    };
    let replacement = Arc::new(crate::state::GraphSlot::from_sparse_v2(
        binding,
        slot.root_key.clone(),
        graph_meta,
    ));
    state
        .state
        .graphs
        .write()
        .unwrap()
        .bind(label, Arc::clone(&replacement))?;
    crate::state::poke_watcher(&state.state);
    sparse_v2_status_for_slot(&replacement)
}

#[tauri::command]
pub(crate) fn sparse_v2_query(
    request: tine_core::sync_runtime::SyncRuntimeQueryRequest,
    state: crate::state::GraphContext<'_>,
) -> Result<tine_core::sync_runtime::SyncRuntimeQueryReply, String> {
    let slot = crate::state::slot_for_context(&state)?;
    active_handle(&slot)?
        .query(request)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn sparse_v2_editor_load(
    request: tine_core::sync_runtime::SyncEditorLoadRequest,
    state: crate::state::GraphContext<'_>,
) -> Result<tine_core::sync_runtime::SyncEditorLoadOutcome, String> {
    let slot = crate::state::slot_for_context(&state)?;
    active_handle(&slot)?
        .load_editor_page(request)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn sparse_v2_editor_save(
    request: tine_core::sync_runtime::SyncEditorSaveRequest,
    state: crate::state::GraphContext<'_>,
) -> Result<tine_core::sync_runtime::SyncEditorSaveOutcome, String> {
    let slot = crate::state::slot_for_context(&state)?;
    active_handle(&slot)?
        .save_editor_page(request)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn sparse_v2_tick(
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2TickDto, String> {
    let slot = crate::state::slot_for_context(&state)?;
    active_handle(&slot)?
        .tick()
        .map(tick_dto)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn sparse_v2_clean_shutdown(
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2RuntimeStatusDto, String> {
    let slot = crate::state::slot_for_context(&state)?;
    active_handle(&slot)?
        .clean_shutdown()
        .map(shutdown_status)
        .map_err(|error| error.to_string())
}

pub(crate) fn clean_shutdown_slot(
    slot: &crate::state::GraphSlot,
) -> Result<Option<SparseV2RuntimeStatusDto>, String> {
    let Some(handle) = slot.sparse_runtime() else {
        return Ok(None);
    };
    handle
        .clean_shutdown()
        .map(shutdown_status)
        .map(Some)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tine_core::model::Graph;
    use tine_core::sync_runtime::{
        SyncEditorBlockDto, SyncEditorBlockKey, SyncEditorLoadOutcome, SyncEditorLoadRequest,
        SyncEditorPageSelector, SyncEditorSaveOutcome, SyncEditorSaveRequest, SyncEditorSaveTarget,
        SyncEntityId, SyncPageKind, SyncPageNameResolutionDto, SyncRuntimeQueryReply,
        SyncRuntimeQueryRequest, SyncSearchHitDto, SyncWatcherObservation,
    };

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

    #[test]
    fn sparse_binding_without_live_handle_gives_actionable_recovery() {
        let fixture = RollbackFixture::new(Some("shadow_import"));
        assert_eq!(
            active_handle(&fixture.slot).unwrap_err(),
            SPARSE_V2_NOT_ACTIVE
        );
        assert!(SPARSE_V2_NOT_ACTIVE.contains("retry sparse-v2 activation"));
        assert!(SPARSE_V2_NOT_ACTIVE.contains("roll back"));
    }

    #[test]
    fn transition_status_uses_the_exact_slots_rollback_eligibility() {
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
            assert!(!shared_status.can_cancel, "{stage}");
            assert!(
                shared_status
                    .cancel_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("entered shared enrollment")),
                "{stage}: {:?}",
                shared_status.cancel_reason
            );
        }

        let provider = RollbackFixture::new(Some("shadow_import"));
        std::fs::create_dir_all(provider.graph_root.join(".tine-sync/v2")).unwrap();
        let provider_status = sparse_v2_status_for_slot(&provider.slot).unwrap();
        assert!(!provider_status.can_cancel);
        assert!(provider_status
            .cancel_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("Shared/provider")));
    }

    #[test]
    fn incomplete_local_activation_retires_without_touching_markdown_and_preserves_private_bytes() {
        let fixture = RollbackFixture::new(Some("shadow_import"));
        let result = cancel_sparse_v2_at_paths(
            &fixture.state,
            "main",
            Arc::clone(&fixture.slot),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
            require_safe_sparse_shutdown,
        )
        .unwrap();

        assert!(matches!(
            result.status.availability,
            SparseV2Availability::LegacyDefault
        ));
        assert_eq!(result.binding_generation, result.status.binding_generation);
        assert!(result
            .recovery_statement
            .contains("complete private sparse-v2 state"));
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
            require_safe_sparse_shutdown,
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
            |_| Err("injected clean shutdown refusal".into()),
        )
        .unwrap_err();

        assert!(error.contains("injected clean shutdown refusal"));
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
    fn archive_rename_failure_restores_the_same_sparse_slot_and_all_bytes() {
        let fixture = RollbackFixture::new(Some("shadow_import"));
        let private_before = snapshot_tree(&fixture.private_root);
        let markdown_before = std::fs::read(&fixture.markdown_path).unwrap();
        let generation = fixture.slot.binding_generation;

        let error = cancel_sparse_v2_at_paths_with_archive(
            &fixture.state,
            "main",
            Arc::clone(&fixture.slot),
            &fixture.private_root,
            &fixture.recovery_root,
            None,
            |_| {
                assert!(fixture.state.graphs.read().unwrap().slot("main").is_none());
                Ok(())
            },
            |private_root, recovery_root| {
                assert_eq!(private_root, fixture.private_root);
                assert_eq!(recovery_root, fixture.recovery_root);
                assert!(fixture.state.graphs.read().unwrap().slot("main").is_none());
                Err("injected archive rename failure".into())
            },
        )
        .unwrap_err();

        assert!(error.contains("injected archive rename failure"));
        let restored = fixture.state.graphs.read().unwrap().slot("main").unwrap();
        assert!(Arc::ptr_eq(&restored, &fixture.slot));
        assert_eq!(restored.binding_generation, generation);
        assert_eq!(snapshot_tree(&fixture.private_root), private_before);
        assert_eq!(
            std::fs::read(&fixture.markdown_path).unwrap(),
            markdown_before
        );
        assert!(!fixture.recovery_root.exists());
    }

    #[test]
    fn any_shared_or_provider_evidence_refuses_rollback_before_shutdown() {
        let provider = RollbackFixture::new(Some("shadow_import"));
        std::fs::create_dir_all(provider.graph_root.join(".tine-sync/v2")).unwrap();
        std::fs::write(
            provider.graph_root.join(".tine-sync/v2/provider-evidence"),
            b"shared",
        )
        .unwrap();
        let provider_error = cancel_sparse_v2_at_paths(
            &provider.state,
            "main",
            Arc::clone(&provider.slot),
            &provider.private_root,
            &provider.recovery_root,
            None,
            |_| panic!("provider evidence must refuse before shutdown"),
        )
        .unwrap_err();
        assert!(provider_error.contains("Shared/provider"));
        assert!(provider
            .state
            .graphs
            .read()
            .unwrap()
            .slot("main")
            .unwrap()
            .is_sparse_v2());

        let shared = RollbackFixture::new(Some("joining"));
        let shared_error = cancel_sparse_v2_at_paths(
            &shared.state,
            "main",
            Arc::clone(&shared.slot),
            &shared.private_root,
            &shared.recovery_root,
            None,
            |_| panic!("shared lifecycle must refuse before shutdown"),
        )
        .unwrap_err();
        assert!(shared_error.contains("entered shared enrollment"));
        assert!(shared.private_root.exists());
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
        let relative = "archive/層/計画.md";
        std::fs::create_dir_all(graph_root.join("pages")).unwrap();
        std::fs::create_dir_all(graph_root.join("journals")).unwrap();
        std::fs::create_dir_all(graph_root.join("archive/層")).unwrap();
        std::fs::write(
            graph_root.join(relative),
            "- present before sparse activation\n",
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
                },
                preamble: None,
                blocks: vec![SyncEditorBlockDto {
                    key: SyncEditorBlockKey::Temporary("first".into()),
                    parent: None,
                    content: "edited through Tauri boundary".into(),
                }],
            })
            .unwrap();
        assert!(matches!(saved, SyncEditorSaveOutcome::Durable { .. }));

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
            "- externally imported through watcher\n",
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
                    name: "計画".into(),
                    page_kind: SyncPageKind::Page,
                },
            })
            .unwrap();
        assert!(
            matches!(
                reloaded,
                SyncEditorLoadOutcome::Loaded { ref page }
                    if page.blocks[0].content == "externally imported through watcher"
            ),
            "editor load must observe the watcher-authored batch: {reloaded:?}"
        );

        assert!(matches!(
            clean_shutdown_slot(&slot).unwrap(),
            Some(status) if status.lifecycle == "stopped_safe"
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
            Some(status) if status.lifecycle == "stopped_safe"
        ));
        let _ = std::fs::remove_dir_all(root);
    }
}
