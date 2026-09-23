//! The graph's handle to its Direct projection.
//!
//! The slot's mutex guards only which projection is attached. Readers get a
//! clone of the handle with the lock already released, so no caller can hold
//! it while it waits for readiness or does work on the projection. Holding it
//! that way blocked every other reader and the progress bar until a
//! whole-graph SQL build finished (GH #543, indexing audit R2-04).

use std::sync::{Arc, Mutex};

use crate::direct_projection::DirectProjection;

pub(super) struct ProjectionSlot(Mutex<Option<Arc<DirectProjection>>>);

impl ProjectionSlot {
    pub(super) fn empty() -> Self {
        Self(Mutex::new(None))
    }

    /// The attached projection. The slot lock is released on return.
    pub(super) fn get(&self) -> Option<Arc<DirectProjection>> {
        self.0.lock().unwrap().clone()
    }

    pub(super) fn take(&self) -> Option<Arc<DirectProjection>> {
        self.0.lock().unwrap().take()
    }

    /// Attach `projection` unless one is attached; returns whether it
    /// attached.
    pub(super) fn attach(&self, projection: Arc<DirectProjection>) -> bool {
        let mut slot = self.0.lock().unwrap();
        if slot.is_some() {
            return false;
        }
        *slot = Some(projection);
        true
    }
}
