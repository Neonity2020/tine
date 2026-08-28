//! Clean-path projection drain must complete in the PRODUCTION build.
//!
//! The in-crate sweep tests exercise this same scenario but compile the
//! library under `cfg(test)`, where `turn_attempt_id` silently substitutes a
//! deterministic attempt identity. Only a build without `cfg(test)` — this
//! integration-test harness, or the shipped binary — executes the arm that
//! fails with "managed projection mutation has no turn-derived attempt
//! identity". The 2026-08-28 absence-sweep E2E found master wedged here:
//! an external-deletion batch retained a recovery continuation whose
//! projection drain could never acquire a turn identity, escalating to a
//! repeating `RecoveryBlocked` toast forever.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use tine_core::oplog::{
    DeviceId, DocumentId, LineageDigest, ProjectionEndpointId, SessionId, WorkspaceId,
};
use tine_core::sync_runtime::{
    SyncAbsenceSweepTier, SyncLocalActivationIdentities, SyncLocalActivationRequest,
    SyncRuntimeHandle, SyncRuntimeTick, SyncWatcherObservation,
};
use uuid::Uuid;

struct Fixture {
    root: PathBuf,
    graph_root: PathBuf,
    request: SyncLocalActivationRequest,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fixture(label: &str, seed: u128) -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "tine-clean-recovery-turns-{label}-{}",
        Uuid::new_v4()
    ));
    let graph_root = root.join("graph");
    fs::create_dir_all(graph_root.join("logseq")).unwrap();
    fs::write(
        graph_root.join("logseq/config.edn"),
        br#"{:pages-directory "pages"
            :journals-directory "journals"}"#,
    )
    .unwrap();
    fs::write(
        graph_root.join("Root.md"),
        b"title:: Root logical\n\n- root content\n",
    )
    .unwrap();
    fs::create_dir_all(graph_root.join("pages")).unwrap();
    fs::write(graph_root.join("pages/Alpha.md"), b"- alpha body\n").unwrap();
    fs::write(graph_root.join("pages/Beta.md"), b"- beta body\n").unwrap();

    let private = root.join("private");
    let request = SyncLocalActivationRequest {
        archive_root: private.join("archive"),
        graph_root: graph_root.clone(),
        enrollment_root: private.join("enrollment"),
        receipt_root: private.join("receipts"),
        database_path: private.join("projection/bootstrap.sqlite"),
        application_runtime_root: private.join("runtime"),
        capture_root: private.join("capture"),
        preparation_root: private.join("preparation"),
        provider_root: graph_root.join(".tine-sync/v2/shared"),
        provider_journal_root: private.join("provider/device/journal"),
        identities: SyncLocalActivationIdentities {
            workspace_id: WorkspaceId::from_uuid(Uuid::from_u128(seed)),
            lineage_digest: LineageDigest::of(format!("lineage-{seed}").as_bytes()),
            catalog_document_id: DocumentId::from_uuid(Uuid::from_u128(seed + 1)),
            endpoint_id: ProjectionEndpointId::from_uuid(Uuid::from_u128(seed + 2)),
            device_id: DeviceId::from_uuid(Uuid::from_u128(seed + 3)),
            preparation_id: Uuid::from_u128(seed + 4),
            session_id: SessionId::from_uuid(Uuid::from_u128(seed + 5)),
        },
    };
    Fixture {
        root,
        graph_root,
        request,
    }
}

/// Tick until the watcher settles. Fails the test loudly if any tick carries
/// the missing-turn-identity wedge or the drain never settles.
fn drain_settled(handle: &SyncRuntimeHandle, ticks_budget: usize) -> Vec<SyncRuntimeTick> {
    let mut ticks = Vec::new();
    for _ in 0..ticks_budget {
        let tick = handle.tick().unwrap();
        let rendered = format!("{tick:?}");
        assert!(
            !rendered.contains("no turn-derived attempt identity"),
            "clean projection drain wedged without a turn identity: {rendered}"
        );
        let settled = matches!(
            tick,
            SyncRuntimeTick::Idle
                | SyncRuntimeTick::AdmittedNoop { .. }
                | SyncRuntimeTick::AdmittedComplete { .. }
        );
        ticks.push(tick);
        let watcher = handle.status().unwrap().watcher;
        if settled && !watcher.pending && !watcher.drain_in_flight {
            return ticks;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("clean drain did not settle within {ticks_budget} ticks: {ticks:?}");
}

#[test]
fn external_deletion_sweep_drains_and_restores_without_a_turn_identity_wedge() {
    let fixture = fixture("deletion-restore", 0xd6001);
    let activated = SyncRuntimeHandle::activate_or_resume_local(fixture.request.clone());
    let handle = activated.handle.expect("fixture activates");
    drain_settled(&handle, 256);

    fs::remove_file(fixture.graph_root.join("Root.md")).unwrap();
    handle
        .observe_watcher(vec![
            SyncWatcherObservation::managed_path("Root.md").unwrap()
        ])
        .unwrap();
    drain_settled(&handle, 256);

    let events = handle.absence_sweep_events().unwrap();
    let event = events.last().expect("the deletion forms a surfaced sweep");
    assert_eq!(event.tier, SyncAbsenceSweepTier::Tier3);
    assert_eq!(event.members.len(), 1);

    let restored = handle.restore_absence_sweep(&event.sweep_id).unwrap();
    assert!(!restored.authored_batch_ids.is_empty());
    drain_settled(&handle, 256);
    assert_eq!(
        fs::read(fixture.graph_root.join("Root.md")).unwrap(),
        b"title:: Root logical\n\n- root content\n",
    );
}
