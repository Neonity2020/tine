//! Read-only observers of the projection worker's queue: how far its lowering
//! loop has got. It does not change what the worker does.

use super::DirectProjection;

impl DirectProjection {
    /// How far the running lowering loop has got -- a fresh build, or an
    /// update or repair past one batch -- for the progress bar only. Nothing
    /// decides by it; a fresh build's ownership is `fresh_build_running`
    /// (audit R12-01, guard `index_progress_is_presentation_only`).
    pub(crate) fn build_progress(&self) -> Option<crate::indexing_progress::IndexingProgress> {
        self.shared.build_progress.snapshot()
    }
}
