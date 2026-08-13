//! Android instrumentation boundary for the managed-storage filesystem contract.
//!
//! This module exists only in debug Android builds. The instrumentation test
//! invokes the real Rust runtime as Tine's app UID, with graph files on Android
//! shared storage and all private authority below `filesDir`. That is the
//! boundary ordinary host tests and cross-compilation cannot exercise.

use jni::objects::{JClass, JString};
use jni::sys::jstring;
use jni::JNIEnv;
use std::path::PathBuf;
use tine_core::oplog::{
    DeviceId, DocumentId, LineageDigest, ProjectionEndpointId, SessionId, WorkspaceId,
};
use tine_core::sync_runtime::{
    SyncLocalActivationIdentities, SyncLocalActivationRequest, SyncLocalActivationStatus,
    SyncRuntimeHandle, SyncRuntimeOpenRequest, SyncRuntimeOpenStatus, SyncShutdownOutcome,
    SyncStorageProfile,
};
use uuid::Uuid;

fn run(graph_root: PathBuf, private_root: PathBuf) -> String {
    let workspace_id = WorkspaceId::new();
    let lineage_seed = Uuid::new_v4();
    let identities = SyncLocalActivationIdentities {
        workspace_id,
        lineage_digest: LineageDigest::of(lineage_seed.as_bytes()),
        catalog_document_id: DocumentId::new(),
        endpoint_id: ProjectionEndpointId::new(),
        device_id: DeviceId::new(),
        preparation_id: Uuid::new_v4(),
        session_id: SessionId::new(),
    };
    let open_request = SyncRuntimeOpenRequest {
        profile: SyncStorageProfile::ExperimentalLocal,
        graph_root: graph_root.clone(),
        archive_root: private_root.join("archive"),
        enrollment_root: private_root.join("enrollment"),
        receipt_root: private_root.join("receipts"),
        database_path: private_root.join("projection/materialization.sqlite"),
        application_runtime_root: private_root.join("runtime"),
        migration_backup_root: private_root.join("migration-backup"),
        provider_root: graph_root.join(".tine-sync/v2/shared"),
        provider_journal_root: private_root.join("provider/device/journal"),
    };
    let activation_request = SyncLocalActivationRequest {
        graph_root,
        archive_root: open_request.archive_root.clone(),
        enrollment_root: open_request.enrollment_root.clone(),
        receipt_root: open_request.receipt_root.clone(),
        database_path: open_request.database_path.clone(),
        application_runtime_root: open_request.application_runtime_root.clone(),
        migration_backup_root: open_request.migration_backup_root.clone(),
        capture_root: private_root.join("capture"),
        preparation_root: private_root.join("preparation"),
        provider_root: open_request.provider_root.clone(),
        provider_journal_root: open_request.provider_journal_root.clone(),
        identities,
    };

    let mut last_progress = "activation-not-started".to_string();
    let activation = SyncRuntimeHandle::activate_or_resume_local_with_detailed_progress(
        activation_request,
        |progress| last_progress = format!("{progress:?}"),
    );
    if activation.status != SyncLocalActivationStatus::Active {
        return format!(
            "activation failed after {last_progress}: {:?}",
            activation.status
        );
    }
    let Some(handle) = activation.handle else {
        return "activation returned Active without a handle".into();
    };
    if let Err(error) = handle.prepare_shared() {
        return format!("prepare shared failed: {error}");
    }
    match handle.clean_shutdown() {
        Ok(SyncShutdownOutcome::Safe(_)) => {}
        outcome => return format!("clean shutdown failed: {outcome:?}"),
    }
    drop(handle);

    let reopened = SyncRuntimeHandle::open(open_request);
    if reopened.status != SyncRuntimeOpenStatus::Active {
        return format!("reopen failed: {:?}", reopened.status);
    }
    let Some(handle) = reopened.handle else {
        return "reopen returned Active without a handle".into();
    };
    match handle.clean_shutdown() {
        Ok(SyncShutdownOutcome::Safe(_)) => "ok".into(),
        outcome => format!("reopened clean shutdown failed: {outcome:?}"),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_page_tine_app_ManagedStorageSmoke_runManagedActivationSmoke(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    graph_root: JString<'_>,
    private_root: JString<'_>,
) -> jstring {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let graph_root = env
            .get_string(&graph_root)
            .map(|value| PathBuf::from(value.to_string_lossy().into_owned()))
            .map_err(|error| format!("invalid graph-root JNI string: {error}"))?;
        let private_root = env
            .get_string(&private_root)
            .map(|value| PathBuf::from(value.to_string_lossy().into_owned()))
            .map_err(|error| format!("invalid private-root JNI string: {error}"))?;
        Ok::<_, String>(run(graph_root, private_root))
    }))
    .map_err(|_| "managed-storage smoke probe panicked".to_string())
    .and_then(|result| result)
    .unwrap_or_else(|error| error);

    env.new_string(result)
        .expect("managed-storage smoke result is a valid Java string")
        .into_raw()
}
