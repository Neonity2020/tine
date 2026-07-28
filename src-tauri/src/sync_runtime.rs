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
    SyncAmbiguousEvidence, SyncLocalActivationIdentities, SyncLocalActivationRequest,
    SyncLocalActivationResult, SyncLocalActivationStage, SyncLocalActivationStatus,
    SyncNonActiveStage, SyncRuntimeComponent, SyncRuntimeHandle, SyncRuntimeLifecycle,
    SyncRuntimeOpenRequest, SyncRuntimeOpenResult, SyncRuntimeOpenStatus, SyncRuntimeRecovery,
    SyncRuntimeStatusSnapshot, SyncRuntimeTick, SyncShutdownOutcome, SyncStorageProfile,
};
use uuid::Uuid;

const BINDING_SCHEMA_VERSION: u32 = 1;
const SPARSE_BINDING_DIR: &str = "sparse-v2";
const SPARSE_BINDING_FILE: &str = "binding.json";
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
            archive_root: PathBuf::from(&self.graph_root).join(".tine-sync/v2"),
            enrollment_root: private.join("enrollment"),
            receipt_root: private.join("receipts"),
            database_path: private.join("projection/materialization.sqlite"),
            application_runtime_root: private.join("runtime"),
        })
    }

    fn activation_request(
        &self,
        app: &tauri::AppHandle,
    ) -> Result<SyncLocalActivationRequest, String> {
        let private = self.private_root(app)?;
        Ok(SyncLocalActivationRequest {
            graph_root: PathBuf::from(&self.graph_root),
            archive_root: PathBuf::from(&self.graph_root).join(".tine-sync/v2"),
            enrollment_root: private.join("enrollment"),
            receipt_root: private.join("receipts"),
            database_path: private.join("projection/materialization.sqlite"),
            application_runtime_root: private.join("runtime"),
            migration_backup_root: private.join("migration-backup"),
            capture_root: private.join("capture"),
            preparation_root: private.join("preparation"),
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
                SparseV2Availability::Retryable { stage, .. } if stage == "local_active"
            ) =>
            {
                SparseV2BindingAction::ReopenActive
            }
            None => SparseV2BindingAction::ActivateOrResume,
        }
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
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct SparseV2StatusDto {
    #[serde(flatten)]
    availability: SparseV2Availability,
    runtime: Option<SparseV2RuntimeStatusDto>,
    can_activate: bool,
    can_retry: bool,
    binding_generation: u64,
}

impl SparseV2StatusDto {
    pub(crate) fn legacy(binding_generation: u64) -> Self {
        Self {
            availability: SparseV2Availability::LegacyDefault,
            runtime: None,
            can_activate: true,
            can_retry: false,
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
            binding_generation,
        }
    }
}

pub(crate) fn runtime_status(snapshot: SyncRuntimeStatusSnapshot) -> SparseV2RuntimeStatusDto {
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
pub(crate) const SPARSE_V2_NOT_ACTIVE: &str = "sparse v2 is not active";

fn active_handle(
    slot: &crate::state::GraphSlot,
) -> Result<&tine_core::sync_runtime::SyncRuntimeHandle, String> {
    slot.sparse_runtime()
        .ok_or_else(|| SPARSE_V2_NOT_ACTIVE.to_string())
}

#[tauri::command]
pub(crate) fn sparse_v2_status(
    state: crate::state::GraphContext<'_>,
) -> Result<SparseV2StatusDto, String> {
    let slot = crate::state::slot_for_context(&state)?;
    Ok(match slot.sparse_binding() {
        Some(binding) => SparseV2StatusDto::from_binding(binding, slot.binding_generation),
        None => SparseV2StatusDto::legacy(slot.binding_generation),
    })
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
            return Ok(SparseV2StatusDto::from_binding(
                binding,
                slot.binding_generation,
            ));
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
        return Ok(SparseV2StatusDto::from_binding(
            replacement
                .sparse_binding()
                .expect("replacement is sparse v2"),
            replacement.binding_generation,
        ));
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
    Ok(SparseV2StatusDto::from_binding(
        replacement
            .sparse_binding()
            .expect("replacement is sparse v2"),
        replacement.binding_generation,
    ))
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
    use tine_core::model::Graph;
    use tine_core::sync_runtime::{
        SyncEditorBlockDto, SyncEditorBlockKey, SyncEditorLoadOutcome, SyncEditorLoadRequest,
        SyncEditorPageSelector, SyncEditorSaveOutcome, SyncEditorSaveRequest, SyncEditorSaveTarget,
        SyncEntityId, SyncPageKind, SyncPageNameResolutionDto, SyncRuntimeQueryReply,
        SyncRuntimeQueryRequest, SyncSearchHitDto, SyncWatcherObservation,
    };

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
            archive_root: graph_root.join(".tine-sync/v2"),
            enrollment_root: private.join("enrollment"),
            receipt_root: private.join("receipts"),
            database_path: private.join("projection/materialization.sqlite"),
            application_runtime_root: private.join("runtime"),
            migration_backup_root: private.join("migration-backup"),
            capture_root: private.join("capture"),
            preparation_root: private.join("preparation"),
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
