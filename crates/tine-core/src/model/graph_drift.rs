//! What changed in the page set since a whole-graph pass read it.
//!
//! Two passes read every page once and must then account for whatever the
//! app published while they read: the cold parse, which installs what it
//! read, and the launch survey, which records the pages it could not read.
//! (The survey's marks need no account: each is at most as new as what it
//! observed, and a newer publication wins where they meet.) On a
//! 10,000-page graph either read takes seconds, and a page opened, saved or
//! deleted inside it is ordinary use. Discarding the pass for it sent the
//! next reader into a second whole-graph parse (GH #543, audit R2-02 and
//! R2-05). [`Graph::drift_since`] is the one account both passes use; a
//! pass is abandoned only when a change has no name.

use super::*;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

/// A change to the page set that a page publication does not describe.
pub(super) enum StructuralChange {
    /// These page files left the page set.
    Removed(Vec<PathBuf>),
    /// These page files' state changed without a publication, such as
    /// becoming unreadable, or being moved, created or rewritten by a
    /// journal migration, rename or rescue: a pass that read them reads them
    /// again, and one that is gone leaves the pass.
    Reread(Vec<PathBuf>),
    /// The page set changed in a way no path list describes. Only tests make
    /// one: every change the app makes knows its paths, and an unnamed one
    /// throws away a whole-graph pass in flight (GH #543, audits R11-08,
    /// R13-04).
    #[cfg(test)]
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
/// The latest event per path is kept while a pass that started before it
/// runs: a pass that has been reading for a while must still be able to name
/// every change it missed, or it would discard its work (GH #543, audit
/// R3-03). An event no running pass started before is dropped, so the map
/// holds the paths changed during the passes in flight, not every path
/// changed in the session (audit R4-P1). A pass registers its start with
/// [`StructuralGeneration::begin_pass`].
pub(super) struct StructuralGeneration {
    counter: AtomicU64,
    log: Arc<std::sync::Mutex<StructuralLog>>,
}

/// The map is pruned when it reaches this size, and again whenever it
/// doubles after that, so pruning costs O(1) per event amortized.
const PRUNE_FLOOR: usize = 256;

struct StructuralLog {
    /// Sequence of the latest unnamed change; 0 when there has been none.
    unnamed_at: u64,
    paths: HashMap<PathBuf, (u64, PathEvent)>,
    /// The start of each running pass, with how many passes started there.
    passes: std::collections::BTreeMap<u64, usize>,
    prune_at: usize,
}

impl StructuralLog {
    fn prune(&mut self) {
        // No running pass can need an event at or before the earliest start:
        // each judges a path against a read no earlier than its own start.
        let floor = self.passes.keys().next().copied().unwrap_or(u64::MAX);
        self.paths.retain(|_, (at, _)| *at > floor);
        self.prune_at = (self.paths.len() * 2).max(PRUNE_FLOOR);
    }
}

/// A running pass's start in the structural sequence. Events after it are
/// kept until the pass drops this.
pub(super) struct PassWatermark {
    at: u64,
    log: Arc<std::sync::Mutex<StructuralLog>>,
}

impl PassWatermark {
    pub(super) fn at(&self) -> u64 {
        self.at
    }
}

impl Drop for PassWatermark {
    fn drop(&mut self) {
        let mut log = self.log.lock().unwrap();
        if let std::collections::btree_map::Entry::Occupied(mut passes) = log.passes.entry(self.at)
        {
            *passes.get_mut() -= 1;
            if *passes.get() == 0 {
                passes.remove();
            }
        }
    }
}

impl StructuralGeneration {
    pub(super) fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
            log: Arc::new(std::sync::Mutex::new(StructuralLog {
                unnamed_at: 0,
                paths: HashMap::new(),
                passes: std::collections::BTreeMap::new(),
                prune_at: PRUNE_FLOOR,
            })),
        }
    }

    /// Start a pass: the sequence so far, noted before the pass reads, and
    /// kept registered until the pass is done.
    pub(super) fn begin_pass(&self) -> PassWatermark {
        let mut log = self.log.lock().unwrap();
        let at = self.counter.load(Ordering::Acquire);
        *log.passes.entry(at).or_default() += 1;
        PassWatermark {
            at,
            log: Arc::clone(&self.log),
        }
    }

    /// The sequence so far, for a running pass that reads a page again.
    pub(super) fn load(&self) -> u64 {
        self.counter.load(Ordering::Acquire)
    }

    /// Entries the per-path map holds now.
    #[cfg(test)]
    pub(super) fn logged_paths_test(&self) -> usize {
        self.log.lock().unwrap().paths.len()
    }

    /// Record one change; only [`Graph::move_cache_generation`] calls this.
    fn record(&self, change: StructuralChange) {
        let mut log = self.log.lock().unwrap();
        let at = self.counter.fetch_add(1, Ordering::AcqRel) + 1;
        match change {
            StructuralChange::Removed(paths) => {
                for path in paths {
                    log.paths.insert(path, (at, PathEvent::Removed));
                }
            }
            StructuralChange::Reread(paths) => {
                for path in paths {
                    log.paths.insert(path, (at, PathEvent::Reread));
                }
            }
            #[cfg(test)]
            StructuralChange::Unnamed => log.unnamed_at = at,
        }
        if log.paths.len() >= log.prune_at {
            log.prune();
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
///
/// The fields are private and [`GraphDrift::into_parts`] is the only way out:
/// it hands every kind of change back positionally, so a consumer names each
/// one. A consumer that read `removed` and `changed` by field never saw
/// `reread` when it was added, and installed an index that dropped a newer
/// watcher failure (GH #543, audit R4-01).
pub(super) struct GraphDrift {
    generation: u64,
    changed: HashSet<PathBuf>,
    removed: HashSet<PathBuf>,
    reread: HashSet<PathBuf>,
}

/// Pages that need a look since a pass read them, by what happened.
pub(super) struct DriftPaths {
    /// Pages published after the pass read them, at bytes or a parse
    /// configuration other than it saw, including pages it never listed.
    pub(super) changed: HashSet<PathBuf>,
    /// Pages removed after the pass read them. A page removed and then
    /// created again is in both `removed` and `changed`; its publication
    /// describes it as it is now.
    pub(super) removed: HashSet<PathBuf>,
    /// Pages whose state changed after the pass read them without a
    /// publication, such as becoming unreadable: read them again.
    pub(super) reread: HashSet<PathBuf>,
}

impl GraphDrift {
    /// The generation the pass may install or queue at, and every change.
    pub(super) fn into_parts(self) -> (u64, DriftPaths) {
        let Self {
            generation,
            changed,
            removed,
            reread,
        } = self;
        (
            generation,
            DriftPaths {
                changed,
                removed,
                reread,
            },
        )
    }
}

/// What one move of the cache generation does to the SQL index.
pub(super) enum IndexEffect<'a> {
    /// The mover queues a page delta or page-set change for this move once it
    /// has released the cache lock, and holds `IndexDeltaComing` until then,
    /// so a reader in between waits for the delta instead of finding the
    /// index behind with nothing queued and parsing the graph (GH #543).
    Sent(&'a IndexDeltaComing),
    /// The move changes nothing the index describes, such as a page becoming
    /// unreadable (its last good content stays served) or recovering. The
    /// index is told the new generation directly: nothing else would ever
    /// reach it, and an index left behind the generation is not ready, so
    /// every indexed read fell back to parsing the graph (GH #543, audit
    /// R4-03). The projection is fetched before the cache lock is taken.
    Unchanged(Option<&'a Arc<crate::direct_projection::DirectProjection>>),
}

/// A mover's promise that the delta for its generation move follows; see
/// [`IndexEffect::Sent`]. Hold it until the delta is queued. Dropping it
/// without queuing leaves the index behind, and readers take their
/// ordinary route.
#[must_use]
pub(super) struct IndexDeltaComing {
    _coming: Option<crate::direct_projection::DeltaComing>,
}

impl Graph {
    /// Announce a generation move whose delta the caller queues next. Take it
    /// before the cache lock: no reader may see the moved generation without
    /// it, and the projection slot is never locked under the cache lock.
    pub(super) fn index_delta_coming(&self) -> IndexDeltaComing {
        IndexDeltaComing {
            _coming: self
                .direct_projection
                .get()
                .map(|projection| projection.delta_coming()),
        }
    }

    /// The one place `cache_gen` moves. The caller holds the cache write lock
    /// and has published the change the move stands for; `change` names a
    /// page-set change that no page publication describes, and `effect`
    /// states how the index hears about the move. Returns the new generation.
    pub(super) fn move_cache_generation(
        &self,
        _cache: &std::sync::RwLockWriteGuard<'_, Option<Arc<Vec<(PageEntry, Arc<Document>)>>>>,
        change: Option<StructuralChange>,
        effect: IndexEffect<'_>,
    ) -> u64 {
        if let Some(change) = change {
            self.cache_structural_gen.record(change);
        }
        let generation = self.cache_gen.fetch_add(1, Ordering::Release) + 1;
        match effect {
            IndexEffect::Unchanged(Some(projection)) => projection.advance_generation(generation),
            // The mover holds the promise until it has queued the delta.
            IndexEffect::Sent(_coming) => {}
            IndexEffect::Unchanged(None) => {}
        }
        generation
    }

    /// Publish the runtime ids of a page read or written now, stamped with
    /// the event sequence so a pass can tell whether it read the page before
    /// or after this publication.
    ///
    /// Only a page publication may call this, and it takes the cache write
    /// guard to prove it holds the lock the publication's delta and counters
    /// move under ([`Graph::drift_since`] relies on that). A publication is a
    /// claim that the index was sent these bytes; recording ids without the
    /// delta made a warm and the watcher treat an unsent edit as sent
    /// (GH #543, audit R4-02).
    pub(super) fn publish_session_page_ids(
        &self,
        _cache: &std::sync::RwLockWriteGuard<'_, Option<Arc<Vec<(PageEntry, Arc<Document>)>>>>,
        path: PathBuf,
        mut ids: SessionPageIds,
    ) {
        ids.published = self.cache_structural_gen.next_publication();
        self.session_page_ids.write().unwrap().insert(path, ids);
    }

    /// Whether `revision` of `path` is already everywhere a reconcile would
    /// put it, so a delivery of those bytes may publish nothing.
    ///
    /// Three stores can hold a page's bytes: the parsed cache (its recorded
    /// disk revision), this session's served record (the ids every
    /// publication writes), and the index (what it holds or was sent). Every
    /// store with an opinion about the page must hold `revision`, and at
    /// least one must. With a parsed cache, a page it lacks is an opinion
    /// ("not these bytes"); a store that knows nothing of the page abstains.
    ///
    /// One rule for all three, because each earlier rule trusted one store
    /// for another: the parsed cache for the index left an external edit out
    /// of search for the session (GH #543, audit R8-02); readiness for the
    /// index published unchanged bytes during the launch build (R9-02); and
    /// the parsed cache for "a page this session served" published unchanged
    /// bytes whenever the app ran from the index alone (R9-03). The caller
    /// passes the cache state it holds the lock for.
    pub(super) fn page_revision_current(
        &self,
        cache_is_none: bool,
        path: &Path,
        revision: &str,
    ) -> bool {
        let recorded = self
            .disk_revs
            .read()
            .unwrap()
            .get(path)
            .map(|known| known == revision);
        let cache = if cache_is_none {
            recorded
        } else {
            Some(recorded.unwrap_or(false))
        };
        let config = self.config().parse_config().digest();
        let served = self
            .session_page_ids
            .read()
            .unwrap()
            .get(path)
            .map(|ids| ids.revision == revision && ids.config == config);
        let index = self
            .direct_projection
            .get()
            .map(|_| self.index_has_revision(path, revision));
        let opinions = [cache, served, index];
        opinions.contains(&Some(true)) && !opinions.contains(&Some(false))
    }

    /// Whether the index holds, or was sent, `revision` of `path` under the
    /// current parse configuration; true when no index is attached. See
    /// [`crate::direct_projection::DirectProjection::holds_source_revision`].
    pub(super) fn index_has_revision(&self, path: &Path, revision: &str) -> bool {
        let Some(projection) = self.direct_projection.get() else {
            return true;
        };
        projection.holds_source_revision(
            self.cache_gen.load(Ordering::Acquire),
            &self.rel_path(path),
            revision,
            &self.config().parse_config().digest(),
        )
    }

    /// Publish the ids of a page opened at bytes the ready index already
    /// holds, with no delta: the claim a publication makes is true, and a
    /// delta would re-send bytes the index has, moving the generation under
    /// every memoized answer on each first open of a page. Returns false
    /// when the index may not hold them (none attached, not ready, a warm in
    /// flight, other bytes, or a parsed cache that must take the page too);
    /// the caller then publishes through the page-delta path (GH #543, audit
    /// R4-02).
    pub(super) fn publish_page_the_index_holds(
        &self,
        path: &Path,
        revision: &str,
        document: &Document,
    ) -> bool {
        let Some(projection) = self.direct_projection.get() else {
            return false;
        };
        let config = self.config().parse_config().digest();
        let generation = self.cache_gen.load(Ordering::Acquire);
        // Revision-exact: a stored row at these bytes is these bytes' rows
        // whether or not the launch check has run, so a page opened while the
        // stored image is served publishes nothing (launch design D2).
        if !projection.image_holds_source_revision(
            crate::direct_projection::ReadAt {
                generation,
                currency: crate::direct_projection::Currency::LaunchStored,
            },
            &self.rel_path(path),
            &crate::direct_projection::projection_source_revision(revision, config),
        ) {
            return false;
        }
        let cache = self.cache.write().unwrap();
        if cache.is_some() || self.cache_gen.load(Ordering::Acquire) != generation {
            return false;
        }
        self.publish_session_page_ids(
            &cache,
            path.to_path_buf(),
            SessionPageIds::capture(revision, config, document),
        );
        true
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
