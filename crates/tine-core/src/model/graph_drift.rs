//! What changed in the page set since a whole-graph pass read it.
//!
//! Two passes read every page once and must then account for whatever the
//! app published while they read: the warm validation of a stored index and
//! the cold parse. On a 10,000-page graph either read takes seconds, and a
//! page opened, saved or deleted inside it is ordinary use. Discarding the
//! pass for it sent the next reader into a second whole-graph parse (GH #543,
//! audit R2-02 and R2-05). [`Graph::drift_since`] is the one account both
//! passes use; a pass is abandoned only when a change has no name.

use super::*;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

/// A change to the page set that a page publication does not describe.
pub(super) enum StructuralChange {
    /// These page files left the page set.
    Removed(Vec<PathBuf>),
    /// The page set changed in a way no path list describes: a broad
    /// invalidation, or a delete whose file could not be named.
    Unnamed,
}

/// Changes kept for passes still reading; a pass that started earlier than
/// the oldest one kept cannot name what it missed and reads again.
const RETAINED_CHANGES: usize = 1024;

/// The structural generation and the changes that moved it. Only
/// [`StructuralGeneration::record`] moves the counter, so every move is
/// either named or marked unnamed.
pub(super) struct StructuralGeneration {
    counter: AtomicU64,
    /// `(base, changes)`: `changes[i]` moved the counter from `base + i`.
    log: std::sync::Mutex<(u64, VecDeque<StructuralChange>)>,
}

impl StructuralGeneration {
    pub(super) fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
            log: std::sync::Mutex::new((0, VecDeque::new())),
        }
    }

    pub(super) fn load(&self) -> u64 {
        self.counter.load(Ordering::Acquire)
    }

    /// Record one change. The caller holds the cache write lock and bumps
    /// `cache_gen` after this, as every page-set mover does.
    pub(super) fn record(&self, change: StructuralChange) {
        let mut log = self.log.lock().unwrap();
        log.1.push_back(change);
        if log.1.len() > RETAINED_CHANGES {
            log.1.pop_front();
            log.0 += 1;
        }
        self.counter.fetch_add(1, Ordering::Release);
    }

    /// Every page removed since `since`; `None` when a change since then is
    /// unnamed or no longer kept.
    fn removed_since(&self, since: u64) -> Option<HashSet<PathBuf>> {
        let log = self.log.lock().unwrap();
        let skip = usize::try_from(since.checked_sub(log.0)?).ok()?;
        let mut removed = HashSet::new();
        for change in log.1.iter().skip(skip) {
            match change {
                StructuralChange::Removed(paths) => removed.extend(paths.iter().cloned()),
                StructuralChange::Unnamed => return None,
            }
        }
        Some(removed)
    }
}

/// The page-set changes since a pass read the graph.
pub(super) struct GraphDrift {
    /// The generation the pass may install or queue at.
    pub(super) generation: u64,
    /// Pages published since the read at bytes or a parse configuration other
    /// than it saw, including pages it never listed.
    pub(super) changed: HashSet<PathBuf>,
    /// Pages removed since the read. A page removed and then created again
    /// is in both sets; its publication describes it as it is now.
    pub(super) removed: HashSet<PathBuf>,
}

impl Graph {
    /// The page-set changes since a pass read the graph at `read_at` and
    /// `structural`, where `read` gives the revision the pass read for a path;
    /// `None` when a change has no name and the pass must read again.
    ///
    /// Opening a page publishes it even when its bytes are unchanged, and a
    /// launch opens today's journal while the pass is still reading: such a
    /// publication, at the revision the pass read, is no change. `_cache` is
    /// the cache lock's contents: every mover publishes its page record and
    /// bumps both counters under that lock, so holding it keeps them together.
    pub(super) fn drift_since<'a>(
        &self,
        _cache: &Option<Arc<Vec<(PageEntry, Arc<Document>)>>>,
        read_at: u64,
        structural: u64,
        read: impl Fn(&Path) -> Option<&'a str>,
    ) -> Option<GraphDrift> {
        let generation = self.cache_gen.load(Ordering::Acquire);
        if generation == read_at {
            return Some(GraphDrift {
                generation,
                changed: HashSet::new(),
                removed: HashSet::new(),
            });
        }
        let removed = self.cache_structural_gen.removed_since(structural)?;
        let config = self.config().parse_config().digest();
        let changed = self
            .session_page_ids
            .read()
            .unwrap()
            .iter()
            .filter(|(path, ids)| {
                ids.config != config || read(path.as_path()) != Some(ids.revision.as_str())
            })
            .map(|(path, _)| path.clone())
            .collect();
        Some(GraphDrift {
            generation,
            changed,
            removed,
        })
    }
}
