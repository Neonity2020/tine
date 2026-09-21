//! Read-only observers of the projection worker's queue: how far a fresh build
//! has got. It does not change what the worker does.

use super::DirectProjection;

impl DirectProjection {
    /// The running fresh build's page count, for the progress bar only.
    pub(crate) fn build_progress(&self) -> Option<crate::indexing_progress::IndexingProgress> {
        self.shared.build_progress.snapshot()
    }
}
