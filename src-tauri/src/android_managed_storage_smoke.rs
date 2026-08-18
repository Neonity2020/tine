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
use std::time::Instant;
use tine_core::oplog::{
    DeviceId, DocumentId, LineageDigest, ProjectionEndpointId, SessionId, WorkspaceId,
};
use tine_core::sync_runtime::{
    SyncApplicationPageLoadOutcome, SyncApplicationPageLoadRequest, SyncApplicationPageSaveOutcome,
    SyncApplicationPageSaveRequest, SyncApplicationPageSaveTarget, SyncApplicationPageSelector,
    SyncLocalActivationIdentities, SyncLocalActivationRequest, SyncLocalActivationStatus,
    SyncRuntimeHandle, SyncRuntimeOpenRequest, SyncRuntimeOpenStatus, SyncShutdownOutcome,
    SyncStorageProfile,
};
use uuid::Uuid;

fn run(graph_root: PathBuf, private_root: PathBuf) -> String {
    let journey_started = Instant::now();
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
    let mut progress_receipt = Vec::new();
    let activation = SyncRuntimeHandle::activate_or_resume_local_with_detailed_progress(
        activation_request,
        |progress| {
            last_progress = format!("{progress:?}");
            progress_receipt.push(format!(
                "{}@{}ms",
                progress.diagnostic_name(),
                journey_started.elapsed().as_millis()
            ));
        },
    );
    let activation_ms = journey_started.elapsed().as_millis();
    if activation.status != SyncLocalActivationStatus::Active {
        return format!(
            "activation failed after {last_progress}: {:?}",
            activation.status
        );
    }
    let Some(handle) = activation.handle else {
        return "activation returned Active without a handle".into();
    };
    let first_page_started = Instant::now();
    let (mut page, revision) = match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: "pages/Smoke.md".into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, revision }) => (page, revision),
        outcome => return format!("post-activation page load failed: {outcome:?}"),
    };
    let first_page_ms = first_page_started.elapsed().as_millis();
    let Some(first) = page.blocks.first_mut() else {
        return "post-activation page has no editable block".into();
    };
    first.raw = "Android managed storage edited".into();
    let save_outcome = handle.save_application_page(SyncApplicationPageSaveRequest {
        target: SyncApplicationPageSaveTarget::Existing {
            path: page.path.clone(),
            revision,
        },
        page,
    });
    // A clean-runtime save can succeed only after settling a RETAINED
    // publication, and settling it consumes the failure that caused it. Read
    // the report either way: a converged retry still costs a retry on every
    // write, and a bare "ok" receipt would hide the underlying cause.
    let retained = match handle.last_retained_publication() {
        Ok(Some(report)) => format!(
            "retained_publication=batch:{} phase:{:?} settled:{} turns:{} detail:{}",
            report.batch_id, report.phase, report.settled, report.settle_turns, report.detail
        ),
        Ok(None) => "retained_publication=none".to_owned(),
        Err(error) => format!("retained_publication=unavailable:{error}"),
    };
    match save_outcome {
        Ok(SyncApplicationPageSaveOutcome::Saved { .. }) => {}
        // The instrumentation boundary can only report the returned value, so
        // carry everything that distinguishes one refusal from another: the
        // Display form (which names the stage and reason code), the debug
        // detail, and the runtime's own status.
        outcome => {
            let detail = match &outcome {
                Err(error) => format!(
                    "display={error}; debug_detail={:?}",
                    error.debug_detail().unwrap_or("none")
                ),
                Ok(_) => "not a refusal".to_owned(),
            };
            return format!(
                "post-activation page save failed: {outcome:?}; {detail}; {retained}; status={}; progress={}",
                describe_status(&handle),
                progress_receipt.join("|")
            );
        }
    }
    // Match a force-closed app: do not ask the actor for a clean drain.  Drop
    // the last sender, let the actor stop, and prove the exact durable edit can
    // be recovered by a fresh runtime open before testing sharing.
    drop(handle);

    let reopen_started = Instant::now();
    let crashed_reopen = SyncRuntimeHandle::open(open_request.clone());
    let crash_reopen_ms = reopen_started.elapsed().as_millis();
    if crashed_reopen.status != SyncRuntimeOpenStatus::Active {
        return format!("crash-style reopen failed: {:?}", crashed_reopen.status);
    }
    let Some(handle) = crashed_reopen.handle else {
        return "crash-style reopen returned Active without a handle".into();
    };
    match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: "pages/Smoke.md".into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. })
            if page.blocks.first().map(|block| block.raw.as_str())
                == Some("Android managed storage edited") => {}
        outcome => return format!("crash-style reopened page mismatch: {outcome:?}"),
    }
    let shared = match handle.prepare_shared() {
        Ok(descriptor) => format!("shared_descriptor={}", descriptor.descriptor_digest),
        Err(error) => return format!("prepare shared failed: {error}"),
    };
    // A successful enrollment cut retires the actor, so this shutdown reads
    // the runtime's own final state rather than reaching a live thread.
    // `ActorUnavailable` carries no payload at all, so never report a shutdown
    // refusal bare: `status=` distinguishes "the runtime stopped Safe and this
    // is merely a report of it" from "the snapshot itself is unreachable, so
    // the actor died some way the retirement contract does not describe".
    match handle.clean_shutdown() {
        Ok(SyncShutdownOutcome::Safe(_)) => {}
        outcome => {
            return format!(
                "clean shutdown failed: {outcome:?}; {shared}; status={}; {retained}; progress={}",
                describe_status(&handle),
                progress_receipt.join("|")
            )
        }
    }
    drop(handle);

    let reopened = SyncRuntimeHandle::open(open_request);
    if reopened.status != SyncRuntimeOpenStatus::Active {
        return format!(
            "reopen after sharing failed: {:?}; {shared}",
            reopened.status
        );
    }
    let Some(handle) = reopened.handle else {
        return "reopen returned Active without a handle".into();
    };
    match handle.clean_shutdown() {
        Ok(SyncShutdownOutcome::Safe(_)) => format!(
            "ok activation_ms={activation_ms} first_page_ms={first_page_ms} crash_reopen_ms={crash_reopen_ms} total_ms={} {retained} {shared} progress={}",
            journey_started.elapsed().as_millis(),
            progress_receipt.join("|")
        ),
        outcome => format!(
            "reopened clean shutdown failed: {outcome:?}; {shared}; status={}",
            describe_status(&handle)
        ),
    }
}

/// The instrumentation boundary can report only returned values, and a
/// `SyncRuntimeRequestError` carries no state. Name the runtime's own view of
/// itself next to every refusal so one CI round trip localises it.
fn describe_status(handle: &SyncRuntimeHandle) -> String {
    match handle.status() {
        Ok(status) => format!(
            "lifecycle:{:?} recovery:{:?} shared_role:{:?} shared_phase:{:?} provider_pending:{} managed_local_pending:{} detail:{}",
            status.lifecycle,
            status.recovery,
            status.shared_role,
            status.shared_phase,
            status.provider_pending,
            status.managed_local_pending,
            status.detail.as_deref().unwrap_or("none")
        ),
        Err(error) => format!("unavailable:{error}"),
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
