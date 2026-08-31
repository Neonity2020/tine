//! Compatibility import surface for managed-storage layout names.
//!
//! The definitions and their frozen manifest are owned by
//! `tine_storage::formats`; this module contains no path values.

pub use tine_storage::formats::{
    ARCHIVE_BATCHES_DIR, ARCHIVE_OBJECTS_DIR, BOOTSTRAP_SOURCE_CAPTURE_CHUNKS_DIR,
    BOOTSTRAP_SOURCE_CAPTURE_DIR, BOOTSTRAP_SOURCE_CAPTURE_MANIFEST_FILE,
    BOOTSTRAP_SOURCE_CHUNKS_FILE, BOOTSTRAP_SOURCE_ENTRIES_FILE, BOOTSTRAP_SOURCE_INVENTORY_FILE,
    ENGINE_HISTORY_CLAIM_FILE, ENGINE_HISTORY_DIR, ENGINE_HISTORY_HEAD_FILE,
    ENGINE_HISTORY_NODES_DIR, ENGINE_HISTORY_ROOTS_DIR, ENGINE_HISTORY_ROOT_SUFFIX,
    ENGINE_HISTORY_TRANSITION_LOCK_FILE, ENROLLMENT_AUTHORITY_FILE,
    ENROLLMENT_AUTHORITY_TEMP_PREFIX, ENROLLMENT_DIR, ENROLLMENT_HEAD_FILE,
    ENROLLMENT_HEAD_TEMP_PREFIX, ENROLLMENT_LEASE_FILE, ENROLLMENT_LOCAL_DIR,
    ENROLLMENT_RECORDS_DIR, ENROLLMENT_RECORD_SUFFIX, ENROLLMENT_STORAGE_DIR,
    ENROLLMENT_VERSION_DIR, LAZY_GENESIS_COMMIT_FILE, LAZY_GENESIS_MANIFEST_FILE,
    LINEAGE_CLAIM_FILE, LOCAL_ACTIVATION_RESERVATION_FILE, MANAGED_LOCAL_JOURNAL_DIR,
    MUTATION_AUTHORITY_LEASE_SUFFIX, MUTATION_AUTHORITY_SUFFIX, PRIVATE_BINDING_DIR,
    PRIVATE_BINDING_FILE, PRIVATE_RECOVERY_DIR, PROJECTION_ATTEMPTS_DIR, PROJECTION_BASES_DIR,
    PROJECTION_CLEANUP_ROUND_0_DIR, PROJECTION_CLEANUP_ROUND_1_DIR,
    PROJECTION_CLEANUP_ROUND_STATE_FILE, PROJECTION_COMPLETIONS_DIR, PROJECTION_FORENSICS_DIR,
    PROJECTION_INTENTS_DIR, PROJECTION_PENDING_CLEANUP_AUTHORITY_FILE,
    PROJECTION_PENDING_CLEANUP_DIR, PROJECTION_PENDING_CLEANUP_SUFFIX, PROJECTION_STORE_CLAIM_FILE,
    PROJECTION_STORE_INIT_FILE, PROVIDER_DEVICE_AUTHORITY_FILE, PROVIDER_INBOX_DIR,
    PROVIDER_OUTBOX_DIR, PROVIDER_PENDING_PUBLICATION_DIR, SHARED_ENROLLMENT_DESCRIPTOR_PATH,
    SHARED_ENROLLMENT_DIR, SHARED_FRONTIER_HEADS_DIR, SHARED_MANIFESTS_DIR,
    SHARED_MANIFEST_RECOVERY_BLOBS_DIR, SHARED_MANIFEST_RECOVERY_LINKS_DIR, SHARED_OBJECTS_DIR,
    SHARED_PUBLICATION_INTENTS_DIR, SHARED_REMOVED_DIR, SHARED_RENAME_EVIDENCE_DIR,
    SHARED_TEMP_DIR, SQLITE_APPLIER_LOCK_FILE, SQLITE_RUNTIME_DIR, SQLITE_WORKSPACES_DIR,
};

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    #[test]
    fn compatibility_surface_has_no_layout_definitions_and_reexports_exactly_named_consumers() {
        const SOURCE: &str = include_str!("sync_layout.rs");
        let production = SOURCE
            .split("#[cfg(test)]")
            .next()
            .expect("compatibility surface has a production region");
        assert!(
            !production.contains("pub const"),
            "Tine core must not regain a second managed-layout source of truth"
        );

        let manifest_names = tine_storage::formats::FORMAT_MANIFEST
            .iter()
            .filter(|row| row.artifact == "managed storage layout")
            .map(|row| row.name)
            .collect::<std::collections::BTreeSet<_>>();
        let reexports = production
            .split_once('{')
            .and_then(|(_, tail)| tail.split_once('}'))
            .expect("compatibility surface has one re-export list")
            .0
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        for name in &reexports {
            assert!(
                manifest_names.contains(name),
                "core compatibility surface reexports unknown storage atom {name}",
            );
        }
        let digest = Sha256::digest(reexports.join("\n").as_bytes());
        assert_eq!(
            format!("{digest:x}"),
            "b9449666c769fd90decac9ecdcd89e0e2012cd03c57b5bdcc7e94e977a08ea0f",
            "the exact managed-layout compatibility surface changed"
        );
    }
}
