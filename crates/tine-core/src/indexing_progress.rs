//! How far the graph-sized index work has got, for the indexing progress bar
//! (GH #543).
//!
//! Opening a large graph runs up to three whole-graph passes in the
//! background: checking the stored search index against every page, reading
//! every page when that index is missing or stale, and building a fresh index.
//! Each pass counts the pages it has finished here, and so does any other
//! lowering longer than one batch. The counters are presentation only:
//! nothing reads them to decide what to do (guard
//! `index_progress_is_presentation_only`; the decision that once did is
//! GH #543, audit R12-01).

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

/// Which whole-graph pass is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexingPhase {
    /// Confirming the stored search index is current, one page at a time.
    Checking,
    /// Reading and parsing every page because the index cannot answer yet.
    Reading,
    /// Writing a fresh search index.
    Indexing,
}

impl IndexingPhase {
    fn code(self) -> u8 {
        match self {
            Self::Checking => 1,
            Self::Reading => 2,
            Self::Indexing => 3,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Checking),
            2 => Some(Self::Reading),
            3 => Some(Self::Indexing),
            _ => None,
        }
    }
}

/// A snapshot of the running pass. `total == 0` means the pass is known to be
/// running but has not counted its pages yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct IndexingProgress {
    pub phase: IndexingPhase,
    pub done: u64,
    pub total: u64,
}

impl IndexingProgress {
    pub(crate) fn unmeasured(phase: IndexingPhase) -> Self {
        Self {
            phase,
            done: 0,
            total: 0,
        }
    }
}

/// One pass at a time per counter. A pass that starts while another is still
/// counting takes the counter over; the older pass's guard then leaves it
/// alone when it ends.
#[derive(Default)]
pub(crate) struct ProgressCounter {
    phase: AtomicU8,
    token: AtomicU64,
    done: AtomicU64,
    total: AtomicU64,
}

impl ProgressCounter {
    pub(crate) fn begin(&self, phase: IndexingPhase, total: usize) -> ProgressPass<'_> {
        let token = self.token.fetch_add(1, Ordering::AcqRel) + 1;
        self.done.store(0, Ordering::Release);
        self.total.store(total as u64, Ordering::Release);
        self.phase.store(phase.code(), Ordering::Release);
        ProgressPass {
            counter: self,
            token,
        }
    }

    pub(crate) fn snapshot(&self) -> Option<IndexingProgress> {
        let phase = IndexingPhase::from_code(self.phase.load(Ordering::Acquire))?;
        let total = self.total.load(Ordering::Acquire);
        Some(IndexingProgress {
            phase,
            done: self.done.load(Ordering::Acquire).min(total),
            total,
        })
    }
}

pub(crate) struct ProgressPass<'a> {
    counter: &'a ProgressCounter,
    token: u64,
}

impl ProgressPass<'_> {
    pub(crate) fn advance(&self, pages: usize) {
        if self.counter.token.load(Ordering::Acquire) == self.token {
            self.counter.done.fetch_add(pages as u64, Ordering::AcqRel);
        }
    }
}

impl Drop for ProgressPass<'_> {
    fn drop(&mut self) {
        if self.counter.token.load(Ordering::Acquire) == self.token {
            self.counter.phase.store(0, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_counts_its_pages_and_clears_when_it_ends() {
        let counter = ProgressCounter::default();
        assert_eq!(counter.snapshot(), None);
        {
            let pass = counter.begin(IndexingPhase::Reading, 10);
            pass.advance(3);
            assert_eq!(
                counter.snapshot(),
                Some(IndexingProgress {
                    phase: IndexingPhase::Reading,
                    done: 3,
                    total: 10
                })
            );
        }
        assert_eq!(counter.snapshot(), None);
    }

    #[test]
    fn an_older_pass_ending_leaves_a_newer_pass_visible() {
        let counter = ProgressCounter::default();
        let older = counter.begin(IndexingPhase::Checking, 5);
        let newer = counter.begin(IndexingPhase::Reading, 8);
        older.advance(4);
        drop(older);
        newer.advance(2);
        assert_eq!(
            counter.snapshot(),
            Some(IndexingProgress {
                phase: IndexingPhase::Reading,
                done: 2,
                total: 8
            })
        );
    }
}
