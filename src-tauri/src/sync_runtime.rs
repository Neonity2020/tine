//! Inactive Tauri-facing retention and forwarding seam.
//!
//! This module is deliberately crate-private and has no `#[tauri::command]`
//! entry point. Normal graph loading and the existing watcher do not call it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tine_core::sync_runtime::{
    SyncRuntimeHandle, SyncRuntimeOpenRequest, SyncRuntimeOpenStatus, SyncRuntimeRequestError,
    SyncRuntimeStatusSnapshot, SyncRuntimeTick, SyncShutdownOutcome, SyncStorageProfile,
    SyncWatcherObservation,
};

#[derive(Default)]
pub(crate) struct SyncRuntimeFacade {
    handles: Mutex<HashMap<PathBuf, SyncRuntimeHandle>>,
}

impl SyncRuntimeFacade {
    /// Explicit opt-in only. A successful active open is retained by canonical
    /// graph root; every other typed startup status retains nothing.
    pub(crate) fn open_explicit(
        &self,
        request: SyncRuntimeOpenRequest,
    ) -> Result<SyncRuntimeOpenStatus, String> {
        if request.profile == SyncStorageProfile::LegacyDefault {
            return Ok(SyncRuntimeHandle::open(request).status);
        }
        let root = std::fs::canonicalize(&request.graph_root)
            .map_err(|error| format!("cannot canonicalize experimental graph root: {error}"))?;
        if self.handles.lock().unwrap().contains_key(&root) {
            return Err(format!(
                "experimental sync runtime is already retained for {}",
                root.display()
            ));
        }
        let opened = SyncRuntimeHandle::open(request);
        let status = opened.status;
        if let Some(handle) = opened.handle {
            self.handles.lock().unwrap().insert(root, handle);
        }
        Ok(status)
    }

    pub(crate) fn observe(
        &self,
        root: &Path,
        observations: Vec<SyncWatcherObservation>,
    ) -> Result<(), SyncRuntimeRequestError> {
        self.handle(root)?.observe_watcher(observations)
    }

    pub(crate) fn tick(&self, root: &Path) -> Result<SyncRuntimeTick, SyncRuntimeRequestError> {
        self.handle(root)?.tick()
    }

    pub(crate) fn status(
        &self,
        root: &Path,
    ) -> Result<SyncRuntimeStatusSnapshot, SyncRuntimeRequestError> {
        self.handle(root)?.status()
    }

    pub(crate) fn shutdown(
        &self,
        root: &Path,
    ) -> Result<SyncShutdownOutcome, SyncRuntimeRequestError> {
        let root = canonical_key(root)?;
        let handle = self
            .handles
            .lock()
            .unwrap()
            .get(&root)
            .cloned()
            .ok_or_else(|| {
                SyncRuntimeRequestError::InvalidRequest(format!(
                    "no experimental sync runtime for {}",
                    root.display()
                ))
            })?;
        let outcome = handle.clean_shutdown()?;
        self.handles.lock().unwrap().remove(&root);
        Ok(outcome)
    }

    pub(crate) fn retained(&self) -> usize {
        self.handles.lock().unwrap().len()
    }

    fn handle(&self, root: &Path) -> Result<SyncRuntimeHandle, SyncRuntimeRequestError> {
        let root = canonical_key(root)?;
        self.handles
            .lock()
            .unwrap()
            .get(&root)
            .cloned()
            .ok_or_else(|| {
                SyncRuntimeRequestError::InvalidRequest(format!(
                    "no experimental sync runtime for {}",
                    root.display()
                ))
            })
    }
}

fn canonical_key(root: &Path) -> Result<PathBuf, SyncRuntimeRequestError> {
    std::fs::canonicalize(root).map_err(|error| {
        SyncRuntimeRequestError::InvalidRequest(format!(
            "cannot canonicalize experimental graph root: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facade_defaults_inactive_and_retains_only_public_handles() {
        let facade = SyncRuntimeFacade::default();
        assert_eq!(facade.retained(), 0);

        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SyncRuntimeFacade>();

        let _open: fn(
            &SyncRuntimeFacade,
            SyncRuntimeOpenRequest,
        ) -> Result<SyncRuntimeOpenStatus, String> = SyncRuntimeFacade::open_explicit;
        let _observe: fn(
            &SyncRuntimeFacade,
            &Path,
            Vec<SyncWatcherObservation>,
        ) -> Result<(), SyncRuntimeRequestError> = SyncRuntimeFacade::observe;
        let _tick: fn(
            &SyncRuntimeFacade,
            &Path,
        ) -> Result<SyncRuntimeTick, SyncRuntimeRequestError> = SyncRuntimeFacade::tick;
        let _status: fn(
            &SyncRuntimeFacade,
            &Path,
        ) -> Result<SyncRuntimeStatusSnapshot, SyncRuntimeRequestError> = SyncRuntimeFacade::status;
        let _shutdown: fn(
            &SyncRuntimeFacade,
            &Path,
        ) -> Result<SyncShutdownOutcome, SyncRuntimeRequestError> = SyncRuntimeFacade::shutdown;

        let root =
            std::env::temp_dir().join(format!("tine-sync-facade-legacy-{}", uuid::Uuid::new_v4()));
        let status = facade
            .open_explicit(SyncRuntimeOpenRequest {
                profile: SyncStorageProfile::LegacyDefault,
                graph_root: root.join("missing-graph"),
                enrollment_root: root.join("missing-enrollment"),
                archive_root: root.join("missing-archive"),
                receipt_root: root.join("missing-receipts"),
                database_path: root.join("missing.sqlite"),
                application_runtime_root: root.join("missing-runtime"),
            })
            .unwrap();
        assert_eq!(status, SyncRuntimeOpenStatus::LegacyDefault);
        assert_eq!(facade.retained(), 0);
        assert!(!root.exists());
    }
}
