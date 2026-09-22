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
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

/// A change to the page set that a page publication does not describe.
pub(super) enum StructuralChange {
    /// These page files left the page set.
    Removed(Vec<PathBuf>),
    /// This page file's state changed without a publication, such as becoming
    /// unreadable: a pass that read it reads it again.
    Reread(PathBuf),
    /// The page set changed in a way no path list describes: a broad
    /// invalidation, or a delete whose file could not be named.
    Unnamed,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PathEvent {
    Removed,
    Reread,
}

/// One sequence for every event a pass must account for: each page
/// publication ([`Graph::publish_session_page_ids`]) and each structural
/// change ([`StructuralGeneration::record`]) takes the next number. A pass
/// notes the sequence before it reads a page; an event with a higher number
/// happened after that read, and one with a lower number did not.
///
/// The latest event per path is kept, never evicted: a pass that has been
/// reading for a while must still be able to name every change it missed, or
/// it would discard its work (GH #543, audit R3-03). The map grows with the
/// distinct paths removed or reread in a session, a fraction of the page
/// cache it serves.
pub(super) struct StructuralGeneration {
    counter: AtomicU64,
    log: std::sync::Mutex<StructuralLog>,
}

#[derive(Default)]
struct StructuralLog {
    /// Sequence of the latest unnamed change; 0 when there has been none.
    unnamed_at: u64,
    paths: HashMap<PathBuf, (u64, PathEvent)>,
}

impl StructuralGeneration {
    pub(super) fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
            log: std::sync::Mutex::new(StructuralLog::default()),
        }
    }

    /// The sequence so far. A pass notes it before it reads.
    pub(super) fn load(&self) -> u64 {
        self.counter.load(Ordering::Acquire)
    }

    /// Record one change. The caller holds the cache write lock and bumps
    /// `cache_gen` after this, as every page-set mover does.
    pub(super) fn record(&self, change: StructuralChange) {
        let mut log = self.log.lock().unwrap();
        let at = self.counter.fetch_add(1, Ordering::AcqRel) + 1;
        match change {
            StructuralChange::Removed(paths) => {
                for path in paths {
                    log.paths.insert(path, (at, PathEvent::Removed));
                }
            }
            StructuralChange::Reread(path) => {
                log.paths.insert(path, (at, PathEvent::Reread));
            }
            StructuralChange::Unnamed => log.unnamed_at = at,
        }
    }

    /// The sequence number of a page publication happening now.
    pub(super) fn next_publication(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// The paths removed, and the paths to read again, after the read each
    /// path's `read_at` gives; `None` when a change since `since` is unnamed.
    fn events_since(
        &self,
        since: u64,
        read_at: impl Fn(&Path) -> u64,
    ) -> Option<(HashSet<PathBuf>, HashSet<PathBuf>)> {
        let log = self.log.lock().unwrap();
        if log.unnamed_at > since {
            return None;
        }
        let mut removed = HashSet::new();
        let mut reread = HashSet::new();
        for (path, (at, event)) in &log.paths {
            if *at > read_at(path) {
                match event {
                    PathEvent::Removed => removed.insert(path.clone()),
                    PathEvent::Reread => reread.insert(path.clone()),
                };
            }
        }
        Some((removed, reread))
    }
}

/// When a pass read the graph: the generation and structural sequence it
/// started at, and the later sequence at which it read some pages again.
pub(super) struct PassReadAt<'a> {
    pub(super) generation: u64,
    pub(super) structural: u64,
    pub(super) reread: &'a HashMap<PathBuf, u64>,
}

impl PassReadAt<'_> {
    fn of(&self, path: &Path) -> u64 {
        self.reread.get(path).copied().unwrap_or(self.structural)
    }
}

/// The page-set changes since a pass read the graph.
pub(super) struct GraphDrift {
    /// The generation the pass may install or queue at.
    pub(super) generation: u64,
    /// Pages published after the pass read them, at bytes or a parse
    /// configuration other than it saw, including pages it never listed.
    pub(super) changed: HashSet<PathBuf>,
    /// Pages removed after the pass read them. A page removed and then
    /// created again is in both sets; its publication describes it as it is now.
    pub(super) removed: HashSet<PathBuf>,
    /// Pages whose state changed after the pass read them without a
    /// publication, such as becoming unreadable.
    pub(super) reread: HashSet<PathBuf>,
}

impl Graph {
    /// Publish the runtime ids of a page read or written now, stamped with
    /// the event sequence so a pass can tell whether it read the page before
    /// or after this publication.
    pub(super) fn publish_session_page_ids(&self, path: PathBuf, mut ids: SessionPageIds) {
        ids.published = self.cache_structural_gen.next_publication();
        self.session_page_ids.write().unwrap().insert(path, ids);
    }

    /// The page-set changes since a pass read the graph at `read_at`, where
    /// `read` gives the revision the pass read for a path; `None` when a
    /// change has no name and the pass must read again.
    ///
    /// Each path is judged against when the pass last read it: an event
    /// before that read is already in what the pass read. So opening a page,
    /// which publishes it, is no change when the pass read the page after
    /// that or read the same bytes; and a page read again after its removal,
    /// or after a publication of older bytes, is settled (audit R3-01, R3-02).
    /// `_cache` is the cache lock's contents: every mover publishes its page
    /// record and moves the counters under that lock, so holding it keeps
    /// them together.
    pub(super) fn drift_since<'a>(
        &self,
        _cache: &Option<Arc<Vec<(PageEntry, Arc<Document>)>>>,
        read_at: &PassReadAt<'_>,
        read: impl Fn(&Path) -> Option<&'a str>,
    ) -> Option<GraphDrift> {
        let generation = self.cache_gen.load(Ordering::Acquire);
        if generation == read_at.generation {
            return Some(GraphDrift {
                generation,
                changed: HashSet::new(),
                removed: HashSet::new(),
                reread: HashSet::new(),
            });
        }
        let (removed, reread) = self
            .cache_structural_gen
            .events_since(read_at.structural, |path| read_at.of(path))?;
        let config = self.config().parse_config().digest();
        let changed = self
            .session_page_ids
            .read()
            .unwrap()
            .iter()
            .filter(|(path, ids)| {
                ids.published > read_at.of(path)
                    && (ids.config != config || read(path.as_path()) != Some(ids.revision.as_str()))
            })
            .map(|(path, _)| path.clone())
            .collect();
        Some(GraphDrift {
            generation,
            changed,
            removed,
            reread,
        })
    }
}
