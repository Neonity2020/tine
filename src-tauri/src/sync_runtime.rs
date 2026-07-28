//! Inactive Tauri-facing sparse-runtime opener.
//!
//! This module deliberately has no `#[tauri::command]` entry point. It does not
//! retain a runtime handle: a future explicit integration must transfer any
//! active handle directly into `GraphSlot::from_sparse_v2`.

use tine_core::sync_runtime::{SyncRuntimeHandle, SyncRuntimeOpenRequest, SyncRuntimeOpenResult};

#[derive(Default)]
pub(crate) struct SyncRuntimeFacade;

impl SyncRuntimeFacade {
    /// Explicit opt-in only. The core opener preserves typed startup statuses
    /// and returns ownership of an active handle to its caller. In particular,
    /// `LegacyDefault` inspects no path and returns no handle.
    pub(crate) fn open_explicit(&self, request: SyncRuntimeOpenRequest) -> SyncRuntimeOpenResult {
        SyncRuntimeHandle::open(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tine_core::sync_runtime::{SyncRuntimeOpenStatus, SyncStorageProfile};

    #[test]
    fn facade_returns_open_ownership_and_legacy_default_retains_nothing() {
        let facade = SyncRuntimeFacade;

        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SyncRuntimeFacade>();

        let _open: fn(&SyncRuntimeFacade, SyncRuntimeOpenRequest) -> SyncRuntimeOpenResult =
            SyncRuntimeFacade::open_explicit;

        let root =
            std::env::temp_dir().join(format!("tine-sync-facade-legacy-{}", uuid::Uuid::new_v4()));
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
}
