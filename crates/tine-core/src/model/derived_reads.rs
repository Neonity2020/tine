//! The readiness boundary for launch-time SQL answers.
use super::*;
use crate::direct_projection::{
    derived_reads::DerivedSelection, Currency, DirectProjection, ReadAt,
};
use crate::query::graph::PageFallback;
use std::collections::HashMap;

/// Whether this thread is serving a display read, and whether retirement cut
/// it short. See [`Graph::display_read`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum DisplayRead {
    Off,
    On,
    Skipped,
}

thread_local! {
    static DISPLAY_READ: std::cell::Cell<DisplayRead> = const { std::cell::Cell::new(DisplayRead::Off) };
    /// Set while this thread runs an index owner; see [`OwnerThread`].
    static INDEX_OWNER_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Set when this thread's last derived read declined because a parsed
    /// cache answers instead; see [`Graph::indexed_or_fallback`].
    static CACHE_DECLINED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Set inside [`Graph::exact_read`]: this display read also acts on its
    /// answer, which must be current.
    static EXACT_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Set when this thread's display read was answered from the stored image
    /// before the launch check validated it; see [`Graph::answer_is_complete`].
    static STORED_SERVED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// When this thread's derived read stops waiting; see [`ReadDeadline`].
    static READ_DEADLINE: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
}

/// How long one derived read (page list, aliases, block-ref counts, ...)
/// waits for index work before it answers the way it answers when none is
/// coming: from the parsed pages. Waiting is right while the index is about
/// to answer -- parsing instead reads every page to answer what that pass
/// settles (GH #543) -- but no read may wait without end: a build that never
/// finished kept every such read blocked (GH #594, index liveness L3). The
/// patience only bounds a stall; a working index answers long before it.
pub(crate) const DERIVED_READ_PATIENCE: std::time::Duration = std::time::Duration::from_secs(60);

/// The one deadline of a derived read, shared by every wait inside it: the
/// retries of [`Graph::indexed_or_fallback`], the generation follows of
/// [`Graph::indexed_read`] and the readiness wait each makes. The outermost
/// read sets it; a nested read inherits it (GH #594, L3).
pub(super) struct ReadDeadline(Option<std::time::Instant>);

impl ReadDeadline {
    pub(super) fn enter(graph: &Graph) -> Self {
        #[cfg(test)]
        let patience = graph
            .page_build_test
            .derived_read_patience
            .lock()
            .unwrap()
            .unwrap_or(DERIVED_READ_PATIENCE);
        #[cfg(not(test))]
        let patience = {
            let _ = graph;
            DERIVED_READ_PATIENCE
        };
        Self(READ_DEADLINE.with(|deadline| {
            let previous = deadline.get();
            if previous.is_none() {
                deadline.set(Some(std::time::Instant::now() + patience));
            }
            previous
        }))
    }

    /// Whether this thread's derived read has waited as long as it may.
    pub(super) fn passed() -> bool {
        READ_DEADLINE.with(|deadline| {
            deadline
                .get()
                .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        })
    }
}

impl Drop for ReadDeadline {
    fn drop(&mut self) {
        READ_DEADLINE.with(|deadline| deadline.set(self.0));
    }
}

/// Marks the current thread as running an index owner for its lifetime.
/// The readiness wait waits while the owner has work to do, so a read on the
/// owner's own thread waits on itself: in debug builds, entering it there
/// panics (GH #543, audit R8-01).
pub(super) struct OwnerThread(bool);

impl OwnerThread {
    pub(super) fn enter() -> Self {
        Self(INDEX_OWNER_THREAD.with(|owner| owner.replace(true)))
    }

    pub(super) fn current() -> bool {
        INDEX_OWNER_THREAD.with(std::cell::Cell::get)
    }
}

impl Drop for OwnerThread {
    fn drop(&mut self) {
        INDEX_OWNER_THREAD.with(|owner| owner.set(self.0));
    }
}

impl Graph {
    /// The app no longer serves this graph: it was switched away from or
    /// replaced by a refresh. Display reads still running on it stop waiting
    /// and start no whole-graph parse; see [`Graph::display_read`] (GH #543).
    pub fn retire(&self) {
        self.retired
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub fn is_retired(&self) -> bool {
        self.retired.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Run a read whose answer is only displayed. `None` means the graph was
    /// retired and the answer would have needed a whole-graph parse of it;
    /// the caller asks the graph that replaced it instead. Reads that act on
    /// their answer (export, asset listing, creation checks) never go through
    /// here and keep their full answer on a retired graph.
    pub fn display_read<T>(&self, read: impl FnOnce() -> T) -> Option<T> {
        if self.is_retired() {
            return None;
        }
        let previous = DISPLAY_READ.with(|state| state.replace(DisplayRead::On));
        let stored_before = STORED_SERVED.with(|served| served.replace(false));
        let answer = read();
        // Only an enclosing display read inherits the mark; a top-level one
        // leaves the thread (a reused command thread) as it found it.
        let stored = STORED_SERVED.with(|served| served.replace(stored_before));
        if stored && previous != DisplayRead::Off {
            STORED_SERVED.with(|served| served.set(true));
        }
        let skipped = DISPLAY_READ.with(|state| state.replace(previous)) == DisplayRead::Skipped;
        if skipped && previous != DisplayRead::Off {
            DISPLAY_READ.with(|state| state.set(DisplayRead::Skipped));
        }
        (!skipped).then_some(answer)
    }

    /// False once this thread's display read skipped a parse, or was answered
    /// from the stored image before the launch check validated it: its answer
    /// is incomplete or unverified and must not be memoized, or a later read of
    /// this graph that acts on its answer (export, creation) would be served it.
    pub(super) fn answer_is_complete(&self) -> bool {
        DISPLAY_READ.with(|state| state.get() != DisplayRead::Skipped)
            && !STORED_SERVED.with(std::cell::Cell::get)
    }

    /// How current this thread's index reads must be (launch design D3): a
    /// display read may be answered from the stored image during the launch
    /// check; any other read, and a display read that acts on its answer
    /// ([`Graph::exact_read`]), waits for the check.
    pub(super) fn read_currency() -> Currency {
        let display = DISPLAY_READ.with(|state| state.get() != DisplayRead::Off);
        if display && !EXACT_READ.with(std::cell::Cell::get) {
            Currency::LaunchStored
        } else {
            Currency::Current
        }
    }

    /// Record that this thread's answer came from the stored image before the
    /// launch check validated it ([`Graph::answer_is_complete`]).
    pub(super) fn note_stored_served() {
        STORED_SERVED.with(|served| served.set(true));
    }

    /// Run `read`, whose answer is acted on, with index reads that must be
    /// current even inside a display read: creation evidence, a listing that
    /// decides a refusal, template text inserted into a page.
    pub(crate) fn exact_read<T>(&self, read: impl FnOnce() -> T) -> T {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                EXACT_READ.with(|exact| exact.set(self.0));
            }
        }
        let _restore = Restore(EXACT_READ.with(|exact| exact.replace(true)));
        read()
    }

    /// True when a display read on a retired graph must not parse the graph;
    /// records that its answer is incomplete.
    pub(super) fn skip_display_parse(&self) -> bool {
        if !self.is_retired() {
            return false;
        }
        DISPLAY_READ.with(|state| {
            if state.get() == DisplayRead::Off {
                return false;
            }
            state.set(DisplayRead::Skipped);
            true
        })
    }

    #[cfg(test)]
    pub(crate) fn leave_indexed_reads_unanswered_test(&self, unanswered: bool) {
        self.page_build_test
            .unanswered_indexed_reads
            .store(unanswered, std::sync::atomic::Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn set_derived_read_patience_test(&self, patience: std::time::Duration) {
        *self.page_build_test.derived_read_patience.lock().unwrap() = Some(patience);
    }

    #[cfg(test)]
    pub(crate) fn indexed_read_attempts_test(&self) -> usize {
        self.page_build_test
            .indexed_read_attempts
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn open_page_during_next_derived_read_test(&self, path: Option<PathBuf>) {
        *self.page_build_test.derived_read_open_once.lock().unwrap() = path;
    }

    /// Record that this thread's derived read is leaving its answer to the
    /// installed parsed cache.
    pub(super) fn note_cache_decline() {
        CACHE_DECLINED.with(|declined| declined.set(true));
    }

    /// Ask the index with `indexed`; when it declines, say how the caller
    /// answers instead. Every read that falls back from the index to the
    /// parsed pages goes through here.
    ///
    /// A decline can mean "the parsed cache answers now" (an acting read
    /// installed one while the index was mid-turn). That decision and the
    /// caller's read of the cache were apart: a rename, merge or delete that
    /// discarded the cache in between left the caller no cache, and it parsed
    /// the whole graph although the discard had just queued its delta and
    /// the index was about to answer (GH #543, long-run seed 3012). Here the
    /// cache is read right after the decision, and a cache discarded
    /// meanwhile sends the read back to the index, which waits for that
    /// delta. Each retry follows a discard, so three bound it.
    pub(crate) fn indexed_or_fallback<T>(
        &self,
        mut indexed: impl FnMut() -> Option<T>,
    ) -> Result<T, PageFallback> {
        let _deadline = ReadDeadline::enter(self);
        for _ in 0..3 {
            CACHE_DECLINED.with(|declined| declined.set(false));
            if let Some(answer) = indexed() {
                return Ok(answer);
            }
            if !CACHE_DECLINED.with(|declined| declined.replace(false)) {
                return Err(PageFallback::Parse);
            }
            let cached = self.cache.read().unwrap().as_ref().map(Arc::clone);
            if let Some(pages) = cached {
                return Err(PageFallback::Cache(pages));
            }
        }
        Err(PageFallback::Parse)
    }

    fn derived_reader(&self) -> Option<(Arc<DirectProjection>, ReadAt)> {
        let projection = self.direct_projection.get()?;
        let currency = Self::read_currency();
        let generation =
            self.wait_for_derived_read(&projection, self.cache_generation(), currency)?;
        Some((
            projection,
            ReadAt {
                generation,
                currency,
            },
        ))
    }

    /// Run `read` against the index at a ready generation. A generation move
    /// during the read (a page opened or saved meanwhile) retries at the new
    /// generation once the index has applied it; only an index that cannot
    /// answer returns `None`, which sends the caller to the parser. Falling
    /// back on a move would parse the whole graph because a page opened at
    /// launch (GH #543).
    pub(super) fn indexed_read<T>(
        &self,
        mut read: impl FnMut(&Arc<DirectProjection>, ReadAt) -> Option<T>,
    ) -> Option<T> {
        let _deadline = ReadDeadline::enter(self);
        loop {
            let (projection, at) = self.derived_reader()?;
            let generation = at.generation;
            // Asked before the read: a check that lands during it must not
            // let a stored answer pass for a current one.
            let stored = at.currency == Currency::LaunchStored && !projection.ready_at(generation);
            let answer = read(&projection, at);
            if answer.is_some() && stored {
                Self::note_stored_served();
            }
            #[cfg(test)]
            let answer = {
                use std::sync::atomic::Ordering;
                self.page_build_test
                    .indexed_read_attempts
                    .fetch_add(1, Ordering::Relaxed);
                answer.filter(|_| {
                    !self
                        .page_build_test
                        .unanswered_indexed_reads
                        .load(Ordering::Acquire)
                })
            };
            #[cfg(test)]
            {
                let path = self
                    .page_build_test
                    .derived_read_open_once
                    .lock()
                    .unwrap()
                    .take();
                if let Some(path) = path {
                    let entry = self.entry_for_path(&path).expect("test page exists");
                    self.load_page(&entry).expect("test page opens");
                }
            }
            // A read that met damage has asked the one decider for a new
            // image; wait for it rather than parse the graph (audit R12-05),
            // and sleep while it comes rather than spin (audit R13-07).
            if answer.is_none() && projection.coming() && !ReadDeadline::passed() {
                projection.wait_while_coming(std::time::Duration::from_millis(50));
                continue;
            }
            if self.cache_generation() == generation {
                return answer;
            }
            // The graph kept moving under the read past its deadline: the
            // parsed pages answer instead (L3).
            if ReadDeadline::passed() {
                return None;
            }
        }
    }

    pub(super) fn indexed_derived_pages(
        &self,
        selection: DerivedSelection<'_>,
    ) -> Option<Vec<(PageEntry, Arc<Document>)>> {
        self.indexed_read(|projection, at| {
            self.indexed_derived_pages_at(projection, at, &selection)
        })
    }

    fn indexed_derived_pages_at(
        &self,
        projection: &Arc<DirectProjection>,
        at: ReadAt,
        selection: &DerivedSelection<'_>,
    ) -> Option<Vec<(PageEntry, Arc<Document>)>> {
        let rows = projection.derived_pages(at, selection)?;
        let mut pages = rows
            .into_iter()
            .map(|row| {
                if let Some((revision, preorder)) = row.session_ids {
                    if let Some(ids) = SessionPageIds::from_projection(
                        &revision,
                        self.config().parse_config().digest(),
                        preorder,
                    ) {
                        self.session_page_ids
                            .write()
                            .unwrap()
                            .entry(self.root.join(&row.path))
                            .or_insert(ids);
                    }
                }
                (
                    PageEntry {
                        name: row.name,
                        kind: row.kind,
                        date_key: (row.kind == PageKind::Journal)
                            .then_some(row.journal_day)
                            .flatten(),
                        path: self.root.join(&row.path),
                        rel_path: row.path,
                    },
                    Arc::new(row.document),
                )
            })
            .collect::<Vec<_>>();
        if let DerivedSelection::Resolve(ids) | DerivedSelection::Preview(ids) = *selection {
            let mut available = HashSet::new();
            for (_, doc) in &pages {
                let mut pending = doc.roots.iter().collect::<Vec<_>>();
                while let Some(block) = pending.pop() {
                    available.insert(block.uuid.clone());
                    if let Some(id) = block.property("id") {
                        available.insert(id);
                    }
                    pending.extend(&block.children);
                }
            }
            let missing = ids
                .iter()
                .filter(|id| !available.contains(*id))
                .cloned()
                .collect::<HashSet<_>>();
            if !missing.is_empty() {
                let cached = self.cache.read().unwrap().clone();
                if let Some(cached) = cached {
                    pages.extend(cached.iter().cloned());
                } else {
                    let config = self.config().parse_config().digest();
                    let sources = self
                        .session_page_ids
                        .read()
                        .unwrap()
                        .iter()
                        .filter(|(_, ids)| ids.config == config && ids.contains(&missing))
                        .filter_map(|(path, ids)| {
                            Some((
                                path.strip_prefix(&self.root).ok()?.to_path_buf(),
                                crate::direct_projection::projection_source_revision(
                                    &ids.revision,
                                    config,
                                ),
                            ))
                        })
                        .collect::<Vec<_>>();
                    // A stale locator is a miss. Never turn an exact-revision
                    // failure into a whole-graph parse after the index answered.
                    for source in sources {
                        if let Some(hydrated) =
                            self.parse_pages_on_demand_with_revisions(at.generation, vec![source])
                        {
                            pages.extend(hydrated);
                        }
                    }
                }
            }
        }
        Some(pages)
    }

    pub(super) fn indexed_page_icons(&self, names: &[String]) -> Option<HashMap<String, String>> {
        self.indexed_read(|projection, at| self.indexed_page_icons_at(projection, at, names))
    }

    fn indexed_page_icons_at(
        &self,
        projection: &Arc<DirectProjection>,
        at: ReadAt,
        names: &[String],
    ) -> Option<HashMap<String, String>> {
        let aliases = projection.page_aliases_with_owners(at)?;
        let mut keys = names
            .iter()
            .map(|name| crate::refs::page_key(name))
            .collect::<HashSet<_>>();
        for (alias, canonical, _) in &aliases {
            if keys.contains(&crate::refs::page_key(alias)) {
                keys.insert(crate::refs::page_key(canonical));
            }
        }
        let rows = projection.page_icon_rows(at, &keys.into_iter().collect::<Vec<_>>())?;
        let mut real = HashSet::new();
        let mut icons = HashMap::new();
        for (key, preamble) in rows {
            real.insert(key.clone());
            // Duplicate keys use the first icon in deterministic path order.
            if let Some(icon) = pre_block_icon(&preamble) {
                icons.entry(key).or_insert(icon);
            }
        }
        for (alias, canonical, _) in aliases {
            let key = crate::refs::page_key(&alias);
            if !real.contains(&key) {
                if let Some(icon) = icons.get(&crate::refs::page_key(&canonical)).cloned() {
                    icons.entry(key).or_insert(icon);
                }
            }
        }
        Some(
            names
                .iter()
                .filter_map(|name| {
                    icons
                        .get(&crate::refs::page_key(name))
                        .map(|icon| (name.clone(), icon.clone()))
                })
                .collect(),
        )
    }

    pub(super) fn indexed_journal_content_days(&self) -> Option<Vec<i64>> {
        self.indexed_read(|projection, at| projection.journal_content_days(at))
    }
}
