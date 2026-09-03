//! Android instrumentation boundary for the managed-storage filesystem contract.
//!
//! This module exists only in debug Android builds. The instrumentation test
//! invokes the real Rust runtime as Tine's app UID, with graph files on Android
//! shared storage and all private authority below `filesDir`. That is the
//! boundary ordinary host tests and cross-compilation cannot exercise.
//!
//! The journey itself — its graph fixture AND its call sequence — lives in
//! `tine_core::managed_storage_journey`, which the host test drives too. This
//! module is only the JNI shim and the request shape. Two hand-maintained
//! copies of a journey diverge silently, and one already did: the device hit a
//! reconciliation refusal the green CI journey never reached.

use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jstring};
use jni::JNIEnv;
use std::path::PathBuf;
use tine_core::managed_storage_journey::{
    run_managed_storage_journey, write_journey_graph_fixture,
};
use tine_core::oplog::{
    DeviceId, DocumentId, LineageDigest, ProjectionEndpointId, SessionId, WorkspaceId,
};
use tine_core::sync_runtime::{
    SyncLocalActivationIdentities, SyncLocalActivationRequest, SyncRuntimeOpenRequest,
    SyncStorageProfile,
};
use uuid::Uuid;

fn run(
    graph_root: PathBuf,
    private_root: PathBuf,
    write_fixture: bool,
    return_to_direct_files: bool,
) -> String {
    if write_fixture {
        if let Err(error) = write_journey_graph_fixture(&graph_root) {
            return format!("journey graph fixture could not be written: {error}");
        }
    }
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
        clean_identities: Some(identities.clone()),
        graph_root: graph_root.clone(),
        archive_root: private_root.join("archive"),
        enrollment_root: private_root.join("enrollment"),
        receipt_root: private_root.join("receipts"),
        database_path: private_root.join("projection/materialization.sqlite"),
        application_runtime_root: private_root.join("runtime"),
        provider_root: graph_root.join(".tine-sync/v2/shared"),
        provider_journal_root: private_root.join("provider/device/journal"),
    };
    let activation_request = SyncLocalActivationRequest {
        graph_root: graph_root.clone(),
        archive_root: open_request.archive_root.clone(),
        enrollment_root: open_request.enrollment_root.clone(),
        receipt_root: open_request.receipt_root.clone(),
        database_path: open_request.database_path.clone(),
        application_runtime_root: open_request.application_runtime_root.clone(),
        capture_root: private_root.join("capture"),
        preparation_root: private_root.join("preparation"),
        provider_root: open_request.provider_root.clone(),
        provider_journal_root: open_request.provider_journal_root.clone(),
        identities,
    };
    let receipt =
        run_managed_storage_journey(graph_root.clone(), open_request.clone(), activation_request);
    if !receipt.starts_with("ok ") {
        return receipt;
    }
    if !return_to_direct_files {
        return receipt;
    }
    match crate::sync_runtime::run_android_managed_return_to_direct_files(
        &graph_root,
        &private_root,
        open_request,
    ) {
        Ok(return_receipt) => format!("{receipt} {return_receipt}"),
        Err(error) => format!("Return to Direct Files failed after {receipt}: {error}"),
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_page_tine_app_ManagedStorageSmoke_runManagedActivationSmoke(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    graph_root: JString<'_>,
    private_root: JString<'_>,
    write_fixture: jboolean,
    return_to_direct_files: jboolean,
) -> jstring {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let graph_root = env
            .get_string(&graph_root)
            .map(|value| PathBuf::from(value.to_string_lossy().into_owned()))
            .map_err(|error| {
                crate::command_error::CommandError::prose(format!(
                    "invalid graph-root JNI string: {error}"
                ))
            })?;
        let private_root = env
            .get_string(&private_root)
            .map(|value| PathBuf::from(value.to_string_lossy().into_owned()))
            .map_err(|error| {
                crate::command_error::CommandError::prose(format!(
                    "invalid private-root JNI string: {error}"
                ))
            })?;
        Ok::<_, crate::command_error::CommandError>(run(
            graph_root,
            private_root,
            write_fixture != 0,
            return_to_direct_files != 0,
        ))
    }))
    .map_err(|_| crate::command_error::CommandError::prose("managed-storage smoke probe panicked"))
    .and_then(|result| result)
    // The JNI boundary hands Java a String, so this IS the serialization
    // point, not a bridge: `Display` for `CommandError` renders exactly the
    // same `wire()` bytes the frontend would receive.
    .unwrap_or_else(|error| error.to_string());

    env.new_string(result)
        .expect("managed-storage smoke result is a valid Java string")
        .into_raw()
}
