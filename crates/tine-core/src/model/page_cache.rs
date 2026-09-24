//! Graph's page cache: building, warming and streaming the parsed-page cache,
//! repairing it, invalidation, and cache_upsert / cache_remove.

use super::*;

/// The page-index failure recorded when the graph's text scope itself could
/// not be listed: nothing under it was read.
pub(super) const GRAPH_TEXT_SCOPE_FAILURE: &str = "graph-text-scope: ";

/// The graph-relative paths a page-index failure may name: a failure is a
/// bare path (a read or parse failure) or `path: error` (a listing skip),
/// and a path may itself contain `": "`, so every candidate is offered.
/// The one reader of failure text (GH #543): the fresh build's carried
/// sources and the survey's absences both ask it.
pub(super) fn failure_sources(failure: &str) -> impl Iterator<Item = &str> {
    failure
        .match_indices(": ")
        .map(move |(at, _)| &failure[..at])
        .chain(std::iter::once(failure))
}

/// Whether `failure` names `rel_path` or a directory above it, so the page
/// could not be read rather than being gone.
pub(super) fn failure_covers(failure: &str, rel_path: &str) -> bool {
    failure.starts_with(GRAPH_TEXT_SCOPE_FAILURE)
        || failure_sources(failure).any(|source| {
            source == rel_path
                || (rel_path.starts_with(source)
                    && rel_path.as_bytes().get(source.len()) == Some(&b'/'))
        })
}

/// What the launch survey achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SurveyOutcome {
    /// The survey's marks, or the fresh build it found owed, are queued.
    Owned,
    /// No projection can take them: none attached, the graph text scope or
    /// the stored image unreadable, the writer lease held elsewhere, or the
    /// worker gone. The class is why, for the attempt it costs (GH #594).
    Unavailable(crate::query::IndexFailureClass),
    Cancelled,
}

impl Graph {
    fn parse_page_entry_with_permit(
        &self,
        permit: &GraphTextWritePermit,
        entry: PageEntry,
    ) -> PageParseResult {
        let content = match self.graph_text_read_optional_text_with_identity(&permit, &entry.path) {
            Ok(Some((content, _))) => content,
            _ => return Err(entry.rel_path),
        };
        #[cfg(test)]
        self.count_page_parse_test();
        isolate_page_parse(entry, &self.journal_format, |entry| {
            Some(self.parse_session_page_content(entry, &content))
        })
    }

    /// The page inventory plus the entries the read walk skipped (GH #332).
    /// A listing failure is recorded as one failure naming the whole scope
    /// ([`GRAPH_TEXT_SCOPE_FAILURE`]).
    /// Skipped entries become page index failures, so the source is never
    /// reported complete while a page may be missing from it.
    fn page_build_entries(
        &self,
        permit: &GraphTextWritePermit,
    ) -> Result<(Vec<PageEntry>, Vec<String>), String> {
        #[cfg(test)]
        self.page_build_test
            .enumerations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.graph_text_entries_and_skipped(permit)
            .map_err(|error| format!("{GRAPH_TEXT_SCOPE_FAILURE}{error}"))
    }

    /// Enumerate, read and parse every page from disk once (recording unreadable
    /// files as failures). Used by the fast owner of the shared page-build flight;
    /// the paced warmer has the same inventory/parse boundary below.
    ///
    /// The per-file work (read → content_rev → parse → assign uuids) is independent,
    /// so on a large graph we fan it across cores. Result order is irrelevant: the
    /// cache is searched by `(kind, name)`, never by position.
    pub(super) fn load_all_pages_with_permit(
        &self,
        permit: &GraphTextWritePermit,
    ) -> PageCacheBuild {
        let (entries, skipped) = match self.page_build_entries(permit) {
            Ok(inventory) => inventory,
            Err(failure) => {
                return PageCacheBuild {
                    failures: vec![failure],
                    ..PageCacheBuild::with_capacity(0)
                };
            }
        };
        // Every whole-graph parse reports itself, not only the paced warm's:
        // a consumer's parse ran with the bar saying idle (GH #543, audit
        // R4-P2).
        let progress = self.indexing_progress.begin(
            crate::indexing_progress::IndexingPhase::Reading,
            entries.len(),
        );
        #[cfg(test)]
        {
            let pause = self.page_build_test.fast_parse_pause.lock().unwrap().take();
            if let Some(pause) = pause {
                pause.reached.wait();
                pause.release.wait();
            }
        }
        let mut built = self.parse_page_entries_with_permit(permit, entries, &progress);
        built.failures.extend(skipped);
        built
    }

    fn parse_page_entries_with_permit(
        &self,
        permit: &GraphTextWritePermit,
        entries: Vec<PageEntry>,
        progress: &crate::indexing_progress::ProgressPass<'_>,
    ) -> PageCacheBuild {
        let entry_count = entries.len();
        let workers = page_cache_worker_count();
        // Small graphs (or a single core): serial — the parse is fast and thread
        // spawn isn't worth it. Big graphs: split across `workers` threads.
        if workers <= 1 || entries.len() < 64 {
            let mut built = PageCacheBuild::with_capacity(entries.len());
            for entry in entries {
                built.collect(self.parse_page_entry_with_permit(permit, entry));
                progress.advance(1);
            }
            return built;
        }
        let per = (entries.len() + workers - 1) / workers;
        // Drain into owned contiguous chunks (no clone of PageEntry).
        let mut chunks: Vec<Vec<PageEntry>> = Vec::with_capacity(workers);
        let mut it = entries.into_iter();
        loop {
            let chunk: Vec<PageEntry> = it.by_ref().take(per).collect();
            if chunk.is_empty() {
                break;
            }
            chunks.push(chunk);
        }
        std::thread::scope(|s| {
            let handles: Vec<_> = chunks
                .into_iter()
                .map(|chunk| {
                    s.spawn(move || {
                        let mut built = PageCacheBuild::with_capacity(chunk.len());
                        for entry in chunk {
                            built.collect(self.parse_page_entry_with_permit(permit, entry));
                            progress.advance(1);
                        }
                        built
                    })
                })
                .collect();
            let mut built = PageCacheBuild::with_capacity(entry_count);
            for (worker, handle) in handles.into_iter().enumerate() {
                match handle.join() {
                    Ok(shard) => built.append(shard),
                    Err(_) => eprintln!(
                        "Tine search index worker {worker} panicked after per-page isolation; its shard was not indexed"
                    ),
                }
            }
            built
        })
    }

    pub(super) fn claim_page_build(
        &self,
        expected_generation: u64,
    ) -> (Arc<PageBuildFlight>, bool) {
        let mut active = self.page_build_flight.lock().unwrap();
        if let Some(flight) = active.as_ref() {
            #[cfg(test)]
            {
                let mut joined = self.page_build_test.joined.lock().unwrap();
                *joined += 1;
                self.page_build_test.joined_changed.notify_all();
            }
            return (Arc::clone(flight), false);
        }
        // Close the gap between a caller's earlier cold-cache observation and
        // flight publication. Holding the flight registry makes this decision
        // atomic with a finishing owner clearing its active flight. The nested
        // order is flight -> cache; installation holds only cache and releases it
        // before `finish_page_build` takes flight, so there is no inverse nesting.
        let (completed, structural) = {
            let cache = self.cache.read().unwrap();
            let current_generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
            let structural = self.cache_structural_gen.begin_pass();
            let completed = if current_generation != expected_generation {
                Some(PageBuildOutcome::GenerationDrift)
            } else if cache.is_some() {
                Some(PageBuildOutcome::AlreadyAvailable)
            } else {
                None
            };
            (completed, structural)
        };
        if let Some(outcome) = completed {
            let flight = Arc::new(PageBuildFlight::new(expected_generation, structural));
            flight.complete(outcome);
            return (flight, false);
        }
        let flight = Arc::new(PageBuildFlight::new(expected_generation, structural));
        *active = Some(Arc::clone(&flight));
        drop(active);
        #[cfg(test)]
        {
            let pause = self.page_build_test.owner_pause.lock().unwrap().clone();
            if let Some(pause) = pause {
                pause.reached.wait();
                pause.release.wait();
            }
        }
        (flight, true)
    }

    fn finish_page_build(&self, flight: &Arc<PageBuildFlight>, outcome: PageBuildOutcome) {
        flight.complete(outcome);
        let mut active = self.page_build_flight.lock().unwrap();
        if active
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, flight))
        {
            *active = None;
        }
    }

    pub(super) fn repair_page_cache_once(&self, permit: &GraphTextWritePermit) -> PageBuildOutcome {
        let expected_generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        let (flight, owner) = self.claim_page_build(expected_generation);
        if !owner {
            return flight.wait();
        }
        let built = self.load_all_pages_with_permit(permit);
        let outcome = PageBuildOutcome::from(self.install_reconciled(&flight, permit, built));
        self.finish_page_build(&flight, outcome);
        outcome
    }

    /// Install a whole-graph parse, first reparsing each page an edit, open or
    /// delete changed while it was read (see [`Graph::drift_since`]). A parse
    /// is discarded only when a change has no name, or pages keep changing
    /// under three reconciliations in a row.
    pub(super) fn install_reconciled(
        &self,
        flight: &PageBuildFlight,
        permit: &GraphTextWritePermit,
        mut built: PageCacheBuild,
    ) -> PageCacheInstallOutcome {
        for _ in 0..3 {
            match self.install_built(flight, built) {
                Ok(outcome) => return outcome,
                Err((back, stale)) => {
                    built = back;
                    self.reparse_into(permit, &mut built, &stale);
                }
            }
        }
        PageCacheInstallOutcome::GenerationDrift
    }

    /// Replace the named pages in a parse with their current bytes; a page
    /// that is gone leaves the parse. Returns each reparsed page's file
    /// identity and revision, for a caller that checks them again.
    fn reparse_into(
        &self,
        permit: &GraphTextWritePermit,
        built: &mut PageCacheBuild,
        paths: &std::collections::HashSet<PathBuf>,
    ) -> Vec<(PathBuf, ContentDigest, String)> {
        built
            .pages
            .retain(|(entry, _, _)| !paths.contains(&entry.path));
        built
            .failures
            .retain(|failure| !paths.contains(&self.root.join(failure)));
        // Noted before reading: an event after it is one this read missed.
        let read_at = self.cache_structural_gen.load();
        let mut baselines = Vec::with_capacity(paths.len());
        for path in paths {
            built.reread.insert(path.clone(), read_at);
            let Ok(Some(entry)) = self.graph_text_entry_for_path(path) else {
                continue;
            };
            match self.graph_text_read_optional_text_with_identity(permit, &entry.path) {
                Ok(Some((content, identity))) => {
                    #[cfg(test)]
                    self.count_page_parse_test();
                    let revision = content_rev(&content);
                    let parsed = isolate_page_parse(entry, &self.journal_format, |entry| {
                        Some(self.parse_session_page_content(entry, &content))
                    });
                    if built.collect(parsed) {
                        baselines.push((path.clone(), identity, revision));
                    }
                }
                Ok(None) => {}
                Err(_) => built.failures.push(entry.rel_path),
            }
        }
        baselines
    }

    /// Install a freshly-built whole-graph snapshot atomically: the parsed pages
    /// into the cache, their on-disk revs into `disk_revs`. Cache set BEFORE
    /// disk_revs so a reader never observes a fresh rev paired with a stale cache.
    /// `Err` hands the parse back with the pages changed since it was read;
    /// [`Graph::install_reconciled`] reparses them and installs again.
    pub(super) fn install_built(
        &self,
        flight: &PageBuildFlight,
        built: PageCacheBuild,
    ) -> Result<PageCacheInstallOutcome, (PageCacheBuild, std::collections::HashSet<PathBuf>)> {
        #[cfg(test)]
        if self
            .page_build_test
            .drift_before_install
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            // Real drift: a change with no name, which no reparse can follow.
            let coming = self.index_delta_coming();
            let cache = self.cache.write().unwrap();
            self.move_cache_generation(
                &cache,
                Some(graph_drift::StructuralChange::Unnamed),
                graph_drift::IndexEffect::Sent(&coming),
            );
        }
        let PageCacheBuild {
            pages: built,
            mut failures,
            reread,
        } = built;
        failures.sort();
        failures.dedup();
        let revs: std::collections::HashMap<PathBuf, String> = built
            .iter()
            .map(|(e, _, r)| (e.path.clone(), r.clone()))
            .collect();
        let pages: Vec<(PageEntry, Arc<Document>)> = built
            .into_iter()
            .map(|(e, d, _)| (e, Arc::new(d)))
            .collect();
        let index = build_page_cache_index(&pages);
        let page_list: Vec<PageEntry> = pages.iter().map(|(entry, _)| entry.clone()).collect();
        // Publish cache + revs atomically under the cache lock (cache → disk_revs
        // order), but only at a generation that still describes what the owner
        // parsed. A cache that another publisher already supplied is likewise
        // never overwritten.
        let mut guard = self.cache.write().unwrap();
        let read_at = graph_drift::PassReadAt {
            generation: flight.expected_generation,
            structural: flight.expected_structural.at(),
            reread: &reread,
        };
        let Some(drift) =
            self.drift_since(&guard, &read_at, |path| revs.get(path).map(String::as_str))
        else {
            return Ok(PageCacheInstallOutcome::GenerationDrift);
        };
        // A page that became unreadable after it was read is read again, so
        // its failure is recorded rather than overwritten by the older, clean
        // capture (audit R3-07).
        let (
            expected_generation,
            graph_drift::DriftPaths {
                changed,
                removed,
                reread: reread_since,
            },
        ) = drift.into_parts();
        let stale = changed
            .into_iter()
            .chain(reread_since)
            // A failed page that is gone leaves the failures as well.
            .chain(removed.into_iter().filter(|path| {
                revs.contains_key(path) || failures.iter().any(|f| self.root.join(f) == *path)
            }))
            .collect::<std::collections::HashSet<_>>();
        if !stale.is_empty() {
            drop(guard);
            let pages = pages
                .into_iter()
                .map(|(entry, document)| {
                    let revision = revs[&entry.path].clone();
                    let document = Arc::try_unwrap(document).unwrap_or_else(|d| (*d).clone());
                    (entry, document, revision)
                })
                .collect();
            return Err((
                PageCacheBuild {
                    pages,
                    failures,
                    reread,
                },
                stale,
            ));
        }
        let effective_index = Arc::new(build_effective_identity_index(
            expected_generation,
            &pages,
            failures.clone(),
        ));
        if guard.is_some() && self.page_index_failures.read().unwrap().is_empty() {
            return Ok(PageCacheInstallOutcome::AlreadyAvailable);
        }
        let pages = Arc::new(pages);
        *guard = Some(Arc::clone(&pages));
        *self.cache_index.write().unwrap() = Some(index);
        *self.disk_revs.write().unwrap() = revs.clone();
        *self.effective_identity_index.write().unwrap() = Some(effective_index);
        self.page_index_failures
            .write()
            .unwrap()
            .replace_after_full_read(failures);
        *self.page_list_cache.write().unwrap() = Some((expected_generation, page_list));
        #[cfg(test)]
        self.page_build_test
            .installs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        drop(guard);
        // A consumer's install offers the index nothing: every page change
        // reached it as an update, and whole-index work is the owner's to
        // start (GH #543, audit R10-03). The owner offers this cache when its
        // pass needs one.
        Ok(PageCacheInstallOutcome::Installed)
    }

    /// Run `f` over every parsed page, building the cache on first use.
    ///
    /// `f` scans a consistent snapshot: a concurrent save/delete may or may not be
    /// visible depending on whether it published before this method cloned the
    /// snapshot Arc, but the scan never sees torn or partially-mutated cache
    /// contents. The cache read lock is held only while cloning the Arc; mutations
    /// use copy-on-write under `cache.write()` when a scan still holds an older
    /// snapshot.
    pub fn with_pages<T>(&self, f: impl FnOnce(&[(PageEntry, Arc<Document>)]) -> T) -> T {
        match self.page_snapshot(true) {
            Ok(Some(snapshot)) => f(snapshot.as_slice()),
            // A display read cut short by retirement, or a graph whose text
            // cannot be read: the page set is empty.
            Ok(None) | Err(_) => f(&[]),
        }
    }

    /// [`Graph::with_pages`] for a read that acts on its answer (an asset
    /// listing the user trashes from, a Guide copy): an unreadable graph is an
    /// error here, never an empty page set, and a retired graph still parses.
    pub fn try_with_pages<T>(
        &self,
        f: impl FnOnce(&[(PageEntry, Arc<Document>)]) -> T,
    ) -> io::Result<T> {
        let snapshot = self
            .page_snapshot(false)?
            .expect("only a display read is cut short");
        Ok(f(snapshot.as_slice()))
    }

    /// The parsed page set, building it on first use. `None` only for a
    /// display read on a retired graph, which must not parse it.
    pub(super) fn page_snapshot(
        &self,
        display: bool,
    ) -> io::Result<Option<Arc<Vec<(PageEntry, Arc<Document>)>>>> {
        loop {
            let snapshot = self.cache.read().unwrap().as_ref().map(Arc::clone);
            if snapshot.is_some() {
                return Ok(snapshot);
            }
            if display && self.skip_display_parse() {
                return Ok(None);
            }
            // Admission precedes flight ownership. Query callers retain their
            // historical retry semantics, while Direct creation uses the bounded
            // `repair_page_cache_once` entry point instead.
            let permit = self.admit_retained_graph_text_writer()?;
            let expected_generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
            let (flight, owner) = self.claim_page_build(expected_generation);
            if owner {
                let built = self.load_all_pages_with_permit(&permit);
                let outcome =
                    PageBuildOutcome::from(self.install_reconciled(&flight, &permit, built));
                self.finish_page_build(&flight, outcome);
            } else {
                let _ = flight.wait();
            }
        }
    }

    /// Read one already-captured parsed snapshot without starting a page build.
    /// Pre-ready interactive search is allowed to use this owner, but must not
    /// turn an unavailable projection into a cold query-thread graph parse.
    pub(super) fn with_captured_pages<T>(
        &self,
        f: impl FnOnce(&[(PageEntry, Arc<Document>)]) -> T,
    ) -> Option<T> {
        let snapshot = self.cache.read().unwrap().as_ref().map(Arc::clone)?;
        Some(f(snapshot.as_slice()))
    }

    /// Eagerly build the page cache plus graph-open derived maps (call once after
    /// opening, off the hot path).
    pub fn warm_cache(&self) {
        let _ = self.warm_cache_cancellable(|| false);
    }

    /// Build graph-open caches while allowing a revoked window binding to stop
    /// between files and derived-map phases. Returns false when cancelled.
    pub fn warm_cache_cancellable(&self, cancelled: impl Fn() -> bool) -> bool {
        self.warm_cache_owned(self.register_index_owner(), cancelled)
    }

    /// One inline index pass under a registered owner, for callers that run
    /// no owner loop (the CLI, headless runs, tests): validate the image, or
    /// build and offer the parsed snapshot when it cannot be validated, wait
    /// for the index. The registration makes readers wait for this pass
    /// instead of parsing the graph beside it, and ends with the index phase.
    pub fn warm_cache_owned(&self, owner: IndexOwner, cancelled: impl Fn() -> bool) -> bool {
        let _owner_thread = super::derived_reads::OwnerThread::enter();
        let passed = match self.direct_projection.get() {
            Some(projection) => {
                let need = projection.wait_index_need(&cancelled);
                match self.inline_index_pass(&projection, need, &cancelled) {
                    None => return false,
                    Some(true) => {
                        // The worker may still be applying what the pass
                        // queued; the derived-map reads below would otherwise
                        // parse the whole graph (GH #543). Nothing is owed if
                        // readiness is no longer coming at this generation.
                        let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
                        let _ = projection.wait_until_ready_at(generation, &cancelled);
                        true
                    }
                    Some(false) => true,
                }
            }
            // No index: the parsed cache is all there is.
            None => self.warm_page_cache_cancellable(&cancelled),
        };
        drop(owner);
        self.before_settle_test_point();
        passed && !cancelled()
    }

    /// One owner iteration inline, for callers that run no owner loop (the
    /// CLI, headless runs, tests, and a failed query's repair with no owner):
    /// the pass the owner loop would run for `need`, then -- with no loop to
    /// come back -- one more pass for whatever the decider asks next. Whether
    /// the need is settled; `None` when `cancelled`.
    pub(super) fn inline_index_pass(
        &self,
        projection: &crate::direct_projection::DirectProjection,
        need: crate::direct_projection::IndexNeed,
        cancelled: &impl Fn() -> bool,
    ) -> Option<bool> {
        use crate::direct_projection::IndexNeed;
        // Two passes: a survey that finds too much stale owes a fresh build
        // next, and an unsettled pass gets one retry.
        let mut need = need;
        let mut settled = true;
        for _ in 0..2 {
            if !matches!(need, IndexNeed::Validate | IndexNeed::Fresh) {
                return Some(true);
            }
            settled = self.index_pass(projection, need, cancelled)?;
            need = projection.index_need_now().0;
        }
        Some(settled)
    }

    /// Test pause point: the owner (or an inline warm) is about to report
    /// its launch completion.
    fn before_settle_test_point(&self) {
        #[cfg(test)]
        {
            let pause = self.page_build_test.before_settle.lock().unwrap().take();
            if let Some(pause) = pause {
                pause.reached.wait();
                pause.release.wait();
            }
        }
    }

    /// The graph's index owner (GH #543): the one place that starts
    /// whole-graph index work while it is registered. It waits for what the
    /// index needs -- a validation of the stored image, or a fresh build --
    /// runs that pass holding `permit` (the app's process-wide warm permit,
    /// so two graphs never parse at once), and lets the worker take it from
    /// there. A pass that ends without the worker taking a payload is retried
    /// once at once and then backs off (see `note_unsettled`), so a
    /// deterministic failure cannot repeat whole-graph passes back to back.
    ///
    /// `settle` is the launch completion: it is called once, the first time
    /// nothing is coming any more -- the index is ready, backing off, or gone.
    /// The owner reads nothing for it: every read goes through the readiness
    /// wait, which waits while the owner has work to do, so an owner that
    /// read waited on itself -- forever, once a turn failed while it read
    /// (GH #543, audit R8-01). The frontend's own fetches after
    /// `warm-cache-done` are ordinary reads. The owner keeps running
    /// after it, answering later needs (a failed read, an unnamed deletion,
    /// a failed turn), until `cancelled` or the worker is gone.
    pub fn run_index_owner(
        &self,
        owner: IndexOwner,
        permit: &std::sync::Mutex<()>,
        cancelled: impl Fn() -> bool,
        mut settle: impl FnMut(),
    ) {
        use crate::direct_projection::{IndexNeed, OwnerStep};
        // A retired graph is never read again: its owner stops whatever the
        // caller's own revocation says. A refresh retires the old graph before
        // it cancels the old owner, which could otherwise walk the retired
        // graph once in between (GH #543, audit R7-06).
        let cancelled = || cancelled() || self.is_retired();
        let _owner_thread = super::derived_reads::OwnerThread::enter();
        let lock = || {
            permit
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        };
        let Some(projection) = self.direct_projection.get() else {
            // No index: the parsed cache is all there is.
            let built = {
                let _permit = lock();
                self.warm_page_cache_cancellable(&cancelled)
            };
            if built && !cancelled() {
                self.before_settle_test_point();
                settle();
            }
            drop(owner);
            return;
        };
        let mut settle_owed = true;
        loop {
            match projection.wait_owner_step(settle_owed, &cancelled) {
                OwnerStep::Cancelled => return,
                OwnerStep::Terminal => {
                    if settle_owed {
                        // No index will be ready: warm the parsed cache the
                        // readers fall back to, as a graph without one does.
                        let built = {
                            let _permit = lock();
                            self.warm_page_cache_cancellable(&cancelled)
                        };
                        if built && !cancelled() {
                            self.before_settle_test_point();
                            settle();
                        }
                    }
                    return;
                }
                OwnerStep::Settle => {
                    settle_owed = false;
                    self.before_settle_test_point();
                    settle();
                }
                OwnerStep::Pass(_) => {
                    let _permit = lock();
                    if cancelled() {
                        return;
                    }
                    // Another graph's pass may have held the permit a long
                    // time; the need may have been met or changed meanwhile.
                    let (need, backing_off) = projection.index_need_now();
                    if backing_off || !matches!(need, IndexNeed::Validate | IndexNeed::Fresh) {
                        continue;
                    }
                    let Some(outcome) = self.index_pass_outcome(&projection, need, &cancelled)
                    else {
                        return;
                    };
                    if cancelled() {
                        return;
                    }
                    if let Err(class) = outcome {
                        projection.note_unsettled_pass(class);
                    }
                }
            }
        }
    }

    /// One whole-graph index pass for `need`, the only one there is: the
    /// owner loop runs it, and so does a repair when no owner is registered
    /// (the CLI, headless runs, tests). `Validate` surveys the graph against
    /// the stored revisions and queues its differences as marks; `Fresh`
    /// builds and offers the parsed snapshot. Whether the pass settled the
    /// need; `None` when `cancelled`.
    ///
    /// Which pass comes next is the decider's alone (`index_need`): a survey
    /// that finds too much of the image stale owes a fresh build, and the
    /// decider answers `Fresh` (GH #543, audit R13-03).
    ///
    /// A pass settles its need only if the need has moved on when it ends. A
    /// pass that reports success while the same need stands did nothing, and
    /// counting it settled let the owner run it again at once, without the
    /// backoff, for as long as the graph stayed open (GH #543, audit R7-02).
    pub(super) fn index_pass(
        &self,
        projection: &crate::direct_projection::DirectProjection,
        need: crate::direct_projection::IndexNeed,
        cancelled: &impl Fn() -> bool,
    ) -> Option<bool> {
        self.index_pass_outcome(projection, need, cancelled)
            .map(|outcome| outcome.is_ok())
    }

    /// [`Self::index_pass`], saying why a pass did not settle its need: the
    /// owner counts each such pass as a failed attempt (GH #594, liveness L1)
    /// and records its class (L5).
    pub(super) fn index_pass_outcome(
        &self,
        projection: &crate::direct_projection::DirectProjection,
        need: crate::direct_projection::IndexNeed,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Result<(), crate::query::IndexFailureClass>> {
        #[cfg(test)]
        self.page_build_test
            .owner_passes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reported = if need == crate::direct_projection::IndexNeed::Fresh {
            match self.fresh_index_pass(cancelled) {
                Ok(()) => Ok(()),
                Err(None) => return None,
                Err(Some(class)) => Err(class),
            }
        } else {
            match self.survey_projection_cancellable(cancelled) {
                SurveyOutcome::Owned => Ok(()),
                SurveyOutcome::Cancelled => return None,
                SurveyOutcome::Unavailable(class) => Err(class),
            }
        };
        Some(reported.and_then(|()| {
            if projection.index_need_now().0 != need {
                Ok(())
            } else {
                Err(crate::query::IndexFailureClass::NoProgress)
            }
        }))
    }

    /// Build the parsed snapshot and offer it to the index: `Ok` when the
    /// index took it (or already was current at it), `Err(None)` when
    /// cancelled, `Err(Some(class))` when it failed.
    fn fresh_index_pass(
        &self,
        cancelled: &impl Fn() -> bool,
    ) -> Result<(), Option<crate::query::IndexFailureClass>> {
        self.build_page_cache_outcome(cancelled)?;
        if self.offer_installed_cache() == projection_lifetime::FullOfferOutcome::Queued {
            Ok(())
        } else {
            Err(Some(crate::query::IndexFailureClass::NoProgress))
        }
    }

    /// The launch survey (GH #543, the reconciler; design §4): compare the
    /// stored image with the graph and turn every difference into a mark.
    ///
    /// It loads `cache_gen` BEFORE it reads anything, so every mark it
    /// records is at most as new as its observation: a save or a watcher
    /// update that races it is newer and wins wherever the two meet. It never
    /// waits for a verdict, retries, or carries anything to a later pass.
    /// Its outputs are marks, a fresh build owed, and `validated`.
    ///
    /// A page that cannot be read or parsed keeps its stored rows and is
    /// recorded as a failure, the answer the fresh build gives. A stored page
    /// the survey did not find is deleted only after `validated`, under the
    /// graph-text identity gate, once a re-check confirms it is gone: no Tine
    /// save sits between retiring and publishing a file then (audit R14-03),
    /// and a rename holding the gate while it waits for readiness finds
    /// readiness already owed only to marks.
    pub(super) fn survey_projection_cancellable(
        &self,
        cancelled: &impl Fn() -> bool,
    ) -> SurveyOutcome {
        if cancelled() {
            return SurveyOutcome::Cancelled;
        }
        let Some(projection) = self.direct_projection.get() else {
            return SurveyOutcome::Unavailable(crate::query::IndexFailureClass::Other);
        };
        match projection.wait_index_need(cancelled) {
            crate::direct_projection::IndexNeed::Terminal
            | crate::direct_projection::IndexNeed::LeaseWait
            | crate::direct_projection::IndexNeed::Failed => {
                return SurveyOutcome::Unavailable(crate::query::IndexFailureClass::Other)
            }
            crate::direct_projection::IndexNeed::SettingUp => return SurveyOutcome::Cancelled,
            _ => {}
        }
        let started = std::time::Instant::now();
        crate::direct_projection::projection_diag(|| "survey announced".to_owned());
        #[cfg(test)]
        {
            // Bind first: an `if let` scrutinee would hold the guard across
            // the pause and block the very second pass the test provokes.
            let pause = self
                .page_build_test
                .warm_validation_pause
                .lock()
                .unwrap()
                .take();
            if let Some(pause) = pause {
                pause.reached.wait();
                pause.release.wait();
            }
        }
        let Ok(permit) = self.admit_retained_graph_text_writer() else {
            return SurveyOutcome::Unavailable(crate::query::IndexFailureClass::WriterRefused);
        };
        let parse_config = Arc::new(self.config().parse_config());
        let digest = parse_config.digest();
        // A complete installed parsed cache is the graph as of its generation:
        // it answers without reading a file. Taken under the cache lock, so
        // its generation, pages and revisions are one observation.
        let installed = {
            let cache = self.cache.read().unwrap();
            cache
                .as_ref()
                .filter(|_| self.page_index_failures.read().unwrap().is_empty())
                .map(|pages| {
                    (
                        self.cache_gen.load(std::sync::atomic::Ordering::Acquire),
                        Arc::clone(pages),
                        self.disk_revs.read().unwrap().clone(),
                    )
                })
        };
        let generation = installed.as_ref().map_or_else(
            || self.cache_gen.load(std::sync::atomic::Ordering::Acquire),
            |(generation, _, _)| *generation,
        );
        // Noted before the survey reads: a change to a path after it is newer
        // than what the survey found there (see `publish_page_index_failures`).
        let structural = self.cache_structural_gen.begin_pass();
        let Some(stored) = projection.stored_revisions() else {
            return SurveyOutcome::Unavailable(crate::query::IndexFailureClass::Corrupt);
        };
        let differs = |rel_path: &str, revision: &str| {
            stored.get(rel_path).map(String::as_str)
                != Some(
                    crate::direct_projection::projection_source_revision(revision, digest.clone())
                        .as_str(),
                )
        };
        let mut changes = Vec::new();
        // Changed files are parsed only once the change is known to be small
        // enough to repair: a survey that owes a fresh build parses nothing.
        let mut to_parse = Vec::new();
        let mut found = std::collections::HashSet::new();
        let mut failures = Vec::new();
        // The check stays reported until the survey has decided.
        let mut _checking = None;
        if let Some((_, pages, revisions)) = installed {
            for (entry, document) in pages.iter() {
                found.insert(entry.rel_path.clone());
                let Some(revision) = revisions.get(&entry.path) else {
                    continue;
                };
                if differs(&entry.rel_path, revision) {
                    changes.push(crate::direct_projection::PageSetChange::Replace {
                        entry: entry.clone(),
                        document: Arc::clone(document),
                        revision: revision.clone(),
                    });
                }
            }
        } else {
            let Ok((entries, skipped)) = self.page_build_entries(&permit) else {
                return SurveyOutcome::Unavailable(
                    crate::query::IndexFailureClass::GraphUnreadable,
                );
            };
            #[cfg(test)]
            self.vanish_after_listing_test();
            failures = skipped;
            let bound = entries.len().max(stored.len());
            let progress = self.indexing_progress.begin(
                crate::indexing_progress::IndexingPhase::Checking,
                entries.len(),
            );
            for (i, entry) in entries.into_iter().enumerate() {
                if cancelled() {
                    return SurveyOutcome::Cancelled;
                }
                progress.advance(1);
                match self.graph_text_read_optional_text_with_identity(&permit, &entry.path) {
                    Ok(Some((content, _))) => {
                        found.insert(entry.rel_path.clone());
                        if !differs(&entry.rel_path, &content_rev(&content)) {
                            continue;
                        }
                        to_parse.push((entry, content));
                        if !crate::direct_projection::repair_is_proportionate(to_parse.len(), bound)
                        {
                            #[cfg(test)]
                            self.pause_warm_read_done_test();
                            projection.survey_owes_fresh_build();
                            let paths = to_parse.iter().map(|(entry, _)| entry.path.clone());
                            self.announce_survey_findings(&projection, generation, paths.collect());
                            return SurveyOutcome::Owned;
                        }
                    }
                    // Gone since the listing: the absence check below decides.
                    Ok(None) => {}
                    // The file exists and could not be read: its rows stay.
                    Err(_) => {
                        found.insert(entry.rel_path.clone());
                        failures.push(entry.rel_path);
                    }
                }
                if i % 24 == 23 {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
            _checking = Some(progress);
        }
        #[cfg(test)]
        self.pause_warm_read_done_test();
        // A stored page the listing or a read failed on is unreadable, not
        // gone: it keeps its rows and does not count toward the repair bound.
        let absent = stored
            .keys()
            .filter(|rel_path| !found.contains(*rel_path))
            .filter(|rel_path| {
                !failures
                    .iter()
                    .any(|failure| failure_covers(failure, rel_path))
            })
            .cloned()
            .collect::<Vec<_>>();
        let total = found.len().max(stored.len());
        crate::direct_projection::projection_diag(|| {
            format!(
                "survey read in {}ms at generation={generation}: changed={} absent={} failures={} total={total}",
                started.elapsed().as_millis(),
                changes.len() + to_parse.len(),
                absent.len(),
                failures.len(),
            )
        });
        if !crate::direct_projection::repair_is_proportionate(
            changes.len() + to_parse.len() + absent.len(),
            total,
        ) {
            projection.survey_owes_fresh_build();
            let paths = changes
                .iter()
                .map(|change| change.entry().path.clone())
                .chain(to_parse.iter().map(|(entry, _)| entry.path.clone()))
                .chain(absent.iter().map(|rel_path| self.root.join(rel_path)));
            self.announce_survey_findings(&projection, generation, paths.collect());
            return SurveyOutcome::Owned;
        }
        for (entry, content) in to_parse {
            if cancelled() {
                return SurveyOutcome::Cancelled;
            }
            #[cfg(test)]
            self.page_build_test
                .repair_parses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let rel_path = entry.rel_path.clone();
            match isolate_page_parse(entry, &self.journal_format, |entry| {
                Some(self.parse_session_page_content(entry, &content))
            }) {
                Ok(Some((entry, document, revision))) => {
                    changes.push(crate::direct_projection::PageSetChange::Replace {
                        entry,
                        document: Arc::new(document),
                        revision,
                    });
                }
                _ => failures.push(rel_path),
            }
        }
        #[cfg(test)]
        {
            let pause = self
                .page_build_test
                .before_warm_enqueue
                .lock()
                .unwrap()
                .take();
            if let Some(pause) = pause {
                pause.reached.wait();
                pause.release.wait();
            }
        }
        let mut findings = changes
            .iter()
            .map(|change| change.entry().path.clone())
            .collect::<Vec<_>>();
        projection.record_survey_marks(generation, changes, Arc::clone(&parse_config));
        // Absences are confirmed before readiness when no writer holds the
        // gate, so a page deleted while closed is gone once the index is
        // ready. A writer holding it may be waiting for readiness itself, so
        // then they are confirmed after it (design §4.6).
        let absent = match self.try_lock_graph_text_identity_mutation() {
            Some(gate) if !absent.is_empty() => {
                findings.extend(self.confirm_absences(
                    &projection,
                    &permit,
                    absent,
                    &parse_config,
                    gate,
                ));
                Vec::new()
            }
            _ => absent,
        };
        let announced = self.announce_survey_findings(&projection, generation, findings);
        // The failures first: a reader woken by readiness asks them which
        // identities are unknown (name-only creation refuses on any).
        self.publish_page_index_failures(generation, &structural, failures);
        let generation = announced;
        projection.survey_validated(generation, Arc::clone(&parse_config));
        if !absent.is_empty() {
            if let Ok(gate) = self.lock_graph_text_identity_mutation() {
                let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
                let gone = self.confirm_absences(&projection, &permit, absent, &parse_config, gate);
                self.announce_survey_findings(&projection, generation, gone);
            }
        }
        SurveyOutcome::Owned
    }

    /// Announce what the survey found as a generation of its own, once its
    /// marks are recorded, so readiness at the new generation waits for
    /// them. A finding changes what the index says, and an answer cached at
    /// the generation the survey read (the page list, an entry lookup) would
    /// otherwise outlive it: nothing else moves the generation for a change
    /// Tine did not make (interleaving seed 2503). Returns the generation the
    /// graph is at after the announcement.
    fn announce_survey_findings(
        &self,
        projection: &Arc<crate::direct_projection::DirectProjection>,
        generation: u64,
        paths: Vec<PathBuf>,
    ) -> u64 {
        if paths.is_empty() {
            return generation;
        }
        let cache = self.cache.write().unwrap();
        let previous = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        let next = self.move_cache_generation(
            &cache,
            Some(graph_drift::StructuralChange::Reread(paths)),
            graph_drift::IndexEffect::Unchanged(Some(projection)),
        );
        // The parsed cache, if any, is unchanged: its identities hold at
        // the new generation.
        if let Some(index) = self.effective_identity_index.read().unwrap().as_ref() {
            let _ = index.retag_generation(previous, next);
        }
        drop(cache);
        next
    }

    /// Delete the rows of stored pages the survey did not find, each only
    /// once a re-check under the graph-text identity gate finds it gone. The
    /// generation is loaded before the re-check, so a page re-created after
    /// it is a newer mark than the deletion (design §4.6).
    fn confirm_absences(
        &self,
        projection: &crate::direct_projection::DirectProjection,
        permit: &GraphTextWritePermit,
        absent: Vec<String>,
        parse_config: &Arc<crate::config::ParseConfig>,
        _gate: GraphTextIdentityMutationGuard<'_>,
    ) -> Vec<PathBuf> {
        let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        let deletions = absent
            .into_iter()
            .filter_map(|rel_path| {
                let path = self.root.join(&rel_path);
                // Gone means the read finds no file. A file or directory that
                // cannot be read keeps its rows. GH #597: a stored spelling
                // that reads only because the filesystem ignores case (the
                // file was renamed `Contents.md` -> `contents.md`, or a row was
                // recorded under a name-built spelling) is gone too: the
                // listing found the file under its own spelling.
                if !path_uses_graph_text_alias(&self.root, &path)
                    && !matches!(self.graph_text_read_optional_text(permit, &path), Ok(None))
                {
                    return None;
                }
                Some(crate::direct_projection::PageSetChange::Delete {
                    entry: self.entry_for_path(&path).unwrap_or(PageEntry {
                        name: rel_path.clone(),
                        kind: PageKind::Page,
                        date_key: None,
                        path,
                        rel_path,
                    }),
                })
            })
            .collect::<Vec<_>>();
        let gone = deletions
            .iter()
            .map(|change| change.entry().path.clone())
            .collect();
        projection.record_survey_marks(generation, deletions, Arc::clone(parse_config));
        gone
    }

    /// Merge the survey's failures into the record by path. A path something
    /// changed after the survey read it (an edit, a watcher failure, a
    /// delete) keeps what that newer writer recorded; every other path takes
    /// the survey's observation. Publishing only at an unmoved generation
    /// dropped the whole list on any edit during the survey, and the name an
    /// unreadable page owned stopped being refused (audit R15-01).
    fn publish_page_index_failures(
        &self,
        generation: u64,
        structural: &graph_drift::PassWatermark,
        failures: Vec<String>,
    ) {
        let cache = self.cache.write().unwrap();
        let read_at = graph_drift::PassReadAt {
            generation,
            structural: structural.at(),
            reread: &std::collections::HashMap::new(),
        };
        let drift = self.drift_since(&cache, &read_at, |_| None);
        let mut record = self.page_index_failures.write().unwrap();
        match drift.map(graph_drift::GraphDrift::into_parts) {
            Some((_, paths)) => {
                let newer = |failure: &str| {
                    failure_sources(failure).any(|source| {
                        let path = self.root.join(source);
                        paths.changed.contains(&path)
                            || paths.removed.contains(&path)
                            || paths.reread.contains(&path)
                    })
                };
                record.merge_pass(failures, newer);
            }
            // A change with no name: keep both, the conservative answer.
            None => {
                for failure in failures {
                    record.record(failure);
                }
            }
        }
    }

    /// Build the parsed cache for a warm that owns readiness, and offer the
    /// cache it ends with to the index.
    pub(super) fn warm_page_cache_cancellable(&self, cancelled: &impl Fn() -> bool) -> bool {
        let built = self.build_page_cache_cancellable(cancelled);
        if built {
            let _ = self.offer_installed_cache();
        }
        built
    }

    fn build_page_cache_cancellable(&self, cancelled: &impl Fn() -> bool) -> bool {
        self.build_page_cache_outcome(cancelled).is_ok()
    }

    /// Build the parsed cache: `Err(None)` when cancelled, `Err(Some(class))`
    /// when no complete cache was installed, and why (GH #594 L5).
    fn build_page_cache_outcome(
        &self,
        cancelled: &impl Fn() -> bool,
    ) -> Result<(), Option<crate::query::IndexFailureClass>> {
        let done = |installed: bool, class: crate::query::IndexFailureClass| {
            if cancelled() {
                Err(None)
            } else if installed {
                Ok(())
            } else {
                Err(Some(class))
            }
        };
        #[cfg(test)]
        let _indexing = IndexingBuildTest::enter();
        if cancelled() {
            return Err(None);
        }
        // An installed cache is the parse this pass would do, failures and
        // all: a Fresh pass never retries an unreadable page, which keeps its
        // retained rows until the watcher or a save re-reads it (audit R15-11;
        // `claim_page_build` answers `AlreadyAvailable` the same way).
        if self.cache.read().unwrap().is_some() {
            return Ok(());
        }
        let permit = match self.admit_retained_graph_text_writer() {
            Ok(permit) => permit,
            Err(_) => return Err(Some(crate::query::IndexFailureClass::WriterRefused)),
        };
        let expected_generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        let (flight, owner) = self.claim_page_build(expected_generation);
        if !owner {
            return done(
                flight.wait().installed(),
                crate::query::IndexFailureClass::NoProgress,
            );
        }
        if cancelled() {
            self.finish_page_build(&flight, PageBuildOutcome::Cancelled);
            return Err(None);
        }
        // Build PACED without holding the flight mutex during the parse: on a
        // thermally throttled laptop the warm would otherwise peg a core in one
        // burst right after launch, competing with first scrolling/typing/the
        // first agenda query. Joiners wait for this same generation instead of
        // duplicating its parse.
        let (entries, skipped) = match self.page_build_entries(&permit) {
            Ok(inventory) => inventory,
            Err(failure) => {
                let built = PageCacheBuild {
                    failures: vec![failure],
                    ..PageCacheBuild::with_capacity(0)
                };
                let outcome =
                    PageBuildOutcome::from(self.install_reconciled(&flight, &permit, built));
                self.finish_page_build(&flight, outcome);
                return done(
                    outcome.installed(),
                    crate::query::IndexFailureClass::GraphUnreadable,
                );
            }
        };
        #[cfg(test)]
        self.vanish_after_listing_test();
        let mut built = PageCacheBuild::with_capacity(entries.len());
        built.failures.extend(skipped);
        let mut baselines: Vec<(PathBuf, ContentDigest, String)> =
            Vec::with_capacity(entries.len());
        let progress = self.indexing_progress.begin(
            crate::indexing_progress::IndexingPhase::Reading,
            entries.len(),
        );
        for (i, e) in entries.into_iter().enumerate() {
            progress.advance(1);
            if cancelled() {
                self.finish_page_build(&flight, PageBuildOutcome::Cancelled);
                return Err(None);
            }
            match self.graph_text_read_optional_text_with_identity(&permit, &e.path) {
                Ok(Some((content, identity))) => {
                    let path = e.path.clone();
                    let revision = content_rev(&content);
                    #[cfg(test)]
                    self.count_page_parse_test();
                    let indexed =
                        built.collect(isolate_page_parse(e, &self.journal_format, |entry| {
                            Some(self.parse_session_page_content(entry, &content))
                        }));
                    if indexed {
                        baselines.push((path, identity, revision));
                    }
                }
                // Gone since the listing: it leaves the parse, as in
                // `reparse_into`; it is not a page Tine failed to read.
                Ok(None) => {}
                Err(_) => built.failures.push(e.rel_path),
            }
            if i % 24 == 23 {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        #[cfg(test)]
        if self
            .page_build_test
            .force_warm_failure
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            self.finish_page_build(&flight, PageBuildOutcome::Failed);
            return Err(Some(crate::query::IndexFailureClass::Other));
        }
        #[cfg(test)]
        {
            let pause = self.page_build_test.cold_read_done.lock().unwrap().take();
            if let Some(pause) = pause {
                pause.reached.wait();
                pause.release.wait();
            }
        }
        // A page whose bytes changed after it was parsed (an external edit the
        // watcher has not delivered yet) is parsed again, not the whole graph.
        let mut rounds = 0;
        while !baselines.is_empty() {
            let changed = baselines
                .iter()
                .filter(|(path, identity, revision)| {
                    !matches!(
                        self.graph_text_read_optional_text_with_identity(&permit, path),
                        Ok(Some((content, current_identity)))
                            if current_identity == *identity && content_rev(&content) == *revision
                    )
                })
                .map(|(path, _, _)| path.clone())
                .collect::<std::collections::HashSet<_>>();
            if changed.is_empty() {
                break;
            }
            rounds += 1;
            if rounds > 3 {
                self.finish_page_build(&flight, PageBuildOutcome::Failed);
                return Err(Some(crate::query::IndexFailureClass::PagesKeptChanging));
            }
            baselines = self.reparse_into(&permit, &mut built, &changed);
        }
        if cancelled() {
            self.finish_page_build(&flight, PageBuildOutcome::Cancelled);
            return Err(None);
        }
        let outcome = PageBuildOutcome::from(self.install_reconciled(&flight, &permit, built));
        self.finish_page_build(&flight, outcome);
        done(
            outcome.installed(),
            crate::query::IndexFailureClass::NoProgress,
        )
    }

    /// Discard the parsed cache and owe the index a validation, as if many
    /// files had changed behind the graph with no event naming them. Nothing
    /// in the app does this: every change reaches the graph by path (audit
    /// R11-08), so fixtures that need a cold graph use this.
    #[cfg(test)]
    pub(crate) fn invalidate_cache_test(&self) {
        self.invalidate_guarded_graph_text_identity(
            "broad external cache invalidation has no exact path generation",
        );
        if let Some(projection) = self.direct_projection.get() {
            projection.owe_validation_test();
        }
        self.discard_parsed_cache_as(
            graph_drift::StructuralChange::Unnamed,
            graph_drift::IndexEffect::Unchanged(None),
        );
    }

    #[cfg(test)]
    pub(crate) fn warm_repair_parses_test(&self) -> usize {
        self.page_build_test
            .repair_parses
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    fn vanish_after_listing_test(&self) {
        let vanish = self
            .page_build_test
            .vanish_after_listing
            .lock()
            .unwrap()
            .take();
        if let Some(path) = vanish {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[cfg(test)]
    pub(crate) fn vanish_inside_next_listing_test(&self, path: PathBuf) {
        *self.page_build_test.vanish_inside_listing.lock().unwrap() = Some(path);
    }

    #[cfg(test)]
    pub(crate) fn vanish_after_next_listing_test(&self, path: PathBuf) {
        *self.page_build_test.vanish_after_listing.lock().unwrap() = Some(path);
    }

    #[cfg(test)]
    fn count_page_parse_test(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.page_build_test.parses.fetch_add(1, Relaxed);
        if !INDEXING_BUILD_TEST.with(std::cell::Cell::get) {
            self.page_build_test.consumer_parses.fetch_add(1, Relaxed);
        }
    }

    /// Whole-graph passes the index owner ran.
    #[cfg(test)]
    pub(crate) fn owner_passes_test(&self) -> usize {
        self.page_build_test
            .owner_passes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Page parses outside an indexing build: a warm's or repair's own
    /// rebuild is indexing, every other whole-graph parse is a consumer's.
    #[cfg(test)]
    pub(crate) fn consumer_page_parses_test(&self) -> usize {
        self.page_build_test
            .consumer_parses
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn page_build_parses_test(&self) -> usize {
        self.page_build_test
            .parses
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Move the cache generation mid-survey, as a racing change the index
    /// does not describe would. There is no page to check the move against,
    /// so it counts as structural; the index is told the new generation, as
    /// every production mover tells it (a mark or `advance_generation`).
    #[cfg(test)]
    pub(crate) fn drift_generation_test(&self) {
        let projection = self.direct_projection.get();
        let cache = self.cache.write().unwrap();
        self.move_cache_generation(
            &cache,
            Some(graph_drift::StructuralChange::Unnamed),
            graph_drift::IndexEffect::Unchanged(projection.as_ref()),
        );
    }

    /// The walk inventory as the warm sees it, parsing nothing.
    #[cfg(test)]
    pub(crate) fn walk_entries_test(&self) -> Vec<PageEntry> {
        let permit = self.admit_retained_graph_text_writer().unwrap();
        self.page_build_entries(&permit).unwrap().0
    }

    #[cfg(test)]
    pub(crate) fn on_demand_parses_test(&self) -> usize {
        self.page_build_test
            .on_demand_parses
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// GH #543 test hook: pause the NEXT fast whole-graph parse after it has
    /// listed the pages and before it parses them.
    #[cfg(test)]
    pub(crate) fn pause_next_fast_parse_test(&self) -> Arc<PageBuildTestPause> {
        let pause = Arc::new(PageBuildTestPause::new());
        *self.page_build_test.fast_parse_pause.lock().unwrap() = Some(Arc::clone(&pause));
        pause
    }

    /// GH #543 test hook: pause the NEXT launch survey after it has
    /// announced itself and before it reads page bytes.
    #[cfg(test)]
    pub(crate) fn pause_next_warm_validation_test(&self) -> Arc<PageBuildTestPause> {
        let pause = Arc::new(PageBuildTestPause::new());
        *self.page_build_test.warm_validation_pause.lock().unwrap() = Some(Arc::clone(&pause));
        pause
    }

    /// GH #543 test hook: pause the NEXT public query whose read failed,
    /// before it asks for repair.
    #[cfg(test)]
    pub(crate) fn pause_next_failed_read_repair_test(&self) -> Arc<PageBuildTestPause> {
        let pause = Arc::new(PageBuildTestPause::new());
        *self
            .page_build_test
            .failed_read_repair_pause
            .lock()
            .unwrap() = Some(Arc::clone(&pause));
        pause
    }

    /// GH #543 test hook: pause the NEXT page publication right after it
    /// releases the cache lock, with its new generation observable.
    #[cfg(test)]
    pub(crate) fn pause_next_page_publication_test(&self) -> Arc<PageBuildTestPause> {
        let pause = Arc::new(PageBuildTestPause::new());
        *self.page_build_test.upsert_published_pause.lock().unwrap() = Some(Arc::clone(&pause));
        pause
    }

    /// The survey has read what it will read: after the last file, or at
    /// the early exit that owes a fresh build.
    #[cfg(test)]
    fn pause_warm_read_done_test(&self) {
        let pause = self
            .page_build_test
            .warm_read_done_pause
            .lock()
            .unwrap()
            .take();
        if let Some(pause) = pause {
            pause.reached.wait();
            pause.release.wait();
        }
    }

    #[cfg(test)]
    pub(crate) fn pause_next_warm_after_read_test(&self) -> Arc<PageBuildTestPause> {
        let pause = Arc::new(PageBuildTestPause::new());
        *self.page_build_test.warm_read_done_pause.lock().unwrap() = Some(Arc::clone(&pause));
        pause
    }

    /// Pause the next derived read that leaves its answer to the parsed
    /// cache, after that decision and before its caller reads the cache.
    #[cfg(test)]
    pub(crate) fn pause_next_cache_decline_test(&self) -> Arc<PageBuildTestPause> {
        let pause = Arc::new(PageBuildTestPause::new());
        *self.page_build_test.cache_decline_pause.lock().unwrap() = Some(Arc::clone(&pause));
        pause
    }

    /// Pause the next derived read after it has taken its generation, before
    /// it waits for the index.
    #[cfg(test)]
    pub(crate) fn pause_next_derived_read_test(&self) -> Arc<PageBuildTestPause> {
        let pause = Arc::new(PageBuildTestPause::new());
        *self.page_build_test.derived_read_wait.lock().unwrap() = Some(Arc::clone(&pause));
        pause
    }

    /// Pause the next mutation that discards the parsed cache right after it
    /// has moved the generation, before it queues its delta.
    #[cfg(test)]
    pub(crate) fn pause_after_next_parsed_cache_discard_test(&self) -> Arc<PageBuildTestPause> {
        let pause = Arc::new(PageBuildTestPause::new());
        *self
            .page_build_test
            .after_parsed_cache_discard
            .lock()
            .unwrap() = Some(Arc::clone(&pause));
        pause
    }

    /// Pause the next owner (or inline warm) just before it reports its
    /// launch completion.
    #[cfg(test)]
    pub(crate) fn pause_next_warm_before_settle_test(&self) -> Arc<PageBuildTestPause> {
        let pause = Arc::new(PageBuildTestPause::new());
        *self.page_build_test.before_settle.lock().unwrap() = Some(Arc::clone(&pause));
        pause
    }

    /// Pause the next warm right before it offers its validation to the
    /// queue, after its drift check.
    #[cfg(test)]
    pub(crate) fn pause_next_warm_before_enqueue_test(&self) -> Arc<PageBuildTestPause> {
        let pause = Arc::new(PageBuildTestPause::new());
        *self.page_build_test.before_warm_enqueue.lock().unwrap() = Some(Arc::clone(&pause));
        pause
    }

    #[cfg(test)]
    pub(crate) fn has_parsed_cache_test(&self) -> bool {
        self.cache.read().unwrap().is_some()
    }

    /// Drop the parsed cache and every map derived from it after a mutation
    /// that moved, created, rewrote or retired the pages at `touched`;
    /// `effect` says how the index hears of it. A whole-graph pass in flight
    /// reads those paths again and keeps its work: the change has names.
    /// Recording it unnamed threw the launch walk away and parsed the whole
    /// graph after a journal migration (GH #543, audit R13-04).
    pub(super) fn discard_parsed_cache(
        &self,
        touched: Vec<PathBuf>,
        effect: graph_drift::IndexEffect<'_>,
    ) {
        self.discard_parsed_cache_as(graph_drift::StructuralChange::Reread(touched), effect);
    }

    fn discard_parsed_cache_as(
        &self,
        change: graph_drift::StructuralChange,
        effect: graph_drift::IndexEffect<'_>,
    ) {
        // Compatible IDs are owned by session_page_ids, independently of the
        // parsed cache. Reconciliation invalidates incompatible source revisions.
        let mut guard = self.cache.write().unwrap();
        *guard = None;
        // `page_index_failures` stays: it describes the disk, not the cache,
        // and the mutation that discards records what it changed there.
        *self.cache_index.write().unwrap() = None;
        *self.effective_identity_index.write().unwrap() = None;
        self.disk_revs.write().unwrap().clear(); // under the cache lock (cache → disk_revs)
                                                 // Bump the generation AFTER discarding the cache (under the cache lock), so
                                                 // a reader that loads the new gen then reads the cache sees None (and
                                                 // rebuilds from disk) rather than the stale pre-invalidation content — same
                                                 // gen-after-content ordering as cache_upsert.
        self.move_cache_generation(&guard, Some(change), effect);
        drop(guard);
        #[cfg(test)]
        {
            let pause = self
                .page_build_test
                .after_parsed_cache_discard
                .lock()
                .unwrap()
                .take();
            if let Some(pause) = pause {
                pause.reached.wait();
                pause.release.wait();
            }
        }
    }

    /// Publish one page delta after a write or external reconciliation, without
    /// building an absent parsed cache. `disk_rev` is `content_rev` of the
    /// exact on-disk bytes `doc` was produced from (the freshness key — see
    /// `disk_revs`).
    pub(super) fn cache_upsert(&self, entry: PageEntry, doc: Document, disk_rev: String) {
        // An exact one-page publication must not make an ordinary warm save pay
        // for graph-wide derived-cache maintenance. Content-only updates retain
        // their exact candidate index delta and conservatively clear dependent
        // query caches; an identity-changing update still rebuilds the identity
        // index before it can be used for name-only admission.
        self.cache_upsert_inner(entry, doc, disk_rev, true);
    }

    fn cache_upsert_inner(
        &self,
        entry: PageEntry,
        mut doc: Document,
        disk_rev: String,
        bounded_foreground: bool,
    ) {
        // Fill runtime ids for any block that lacks one (e.g. PDF-highlight writes)
        // from this physical owner. Blocks saved from the frontend already carry
        // live ids, which are deliberately kept through the in-memory save path.
        assign_doc_runtime_ids(&mut doc.roots, &entry.rel_path);
        // Only the alias map needs dropping when an `alias::` was added/changed/
        // removed — invalidating on every save would make a normal edit an O(P)
        // alias rescan on the next navigation.
        let new_aliases = crate::query::document_aliases(&doc);
        let mut alias_touched = !new_aliases.is_empty();
        let path_key = entry.path.clone();
        let doc = Arc::new(doc);
        // Keep the new content + identity for the scoped derived-cache pass below
        // (the original is moved into the cache slot; this clone is a refcount bump).
        let evict_doc = Arc::clone(&doc);
        let evict_entry = entry.clone();
        let projection_revision = disk_rev.clone();
        let mut previous_doc: Option<Arc<Document>> = None;
        let mut is_new_page = false;
        let mut identity_changed = false;
        // Taken before the cache lock: attaching holds the projection slot
        // while it reads the cache, so the slot is never locked under it.
        let projection = self.direct_projection.get();
        let coming = self.index_delta_coming();
        let mut guard = self.cache.write().unwrap();
        let mut failures_guard = self.page_index_failures.write().unwrap();
        self.publish_session_page_ids(
            &guard,
            evict_entry.path.clone(),
            SessionPageIds::capture(
                &projection_revision,
                self.config().parse_config().digest(),
                &evict_doc,
            ),
        );
        let mut resulting_failures = failures_guard.clone();
        let failures_changed = resulting_failures.retire(&evict_entry.rel_path);
        let cache_built = guard.is_some();
        if let Some(pages) = guard.as_mut() {
            let pages = Arc::make_mut(pages);
            match self.cached_page_index_for_path(pages, &entry.path) {
                Some(i) => {
                    let slot = &mut pages[i];
                    alias_touched = new_aliases != crate::query::document_aliases(&slot.1);
                    previous_doc = Some(Arc::clone(&slot.1));
                    identity_changed = slot.0.kind != entry.kind
                        || !crate::refs::same_page(&slot.0.name, &entry.name)
                        || slot.0.date_key != entry.date_key;
                    alias_touched |= identity_changed;
                    slot.0 = entry;
                    slot.1 = doc;
                    if identity_changed {
                        *self.cache_index.write().unwrap() = Some(build_page_cache_index(pages));
                    }
                }
                None => {
                    is_new_page = true;
                    let name_key = page_cache_key(entry.kind, &entry.name);
                    pages.push((entry, doc));
                    if let Some(index) = self.cache_index.write().unwrap().as_mut() {
                        let slot = pages.len() - 1;
                        index.by_path.insert(path_key.clone(), slot);
                        // Exact-path additions must never repoint the stable
                        // logical duplicate winner.
                        index.by_name.entry(name_key).or_insert(slot);
                    }
                }
            }
            // Update disk_revs WHILE STILL HOLDING the cache write lock, so the
            // cached doc and its freshness rev are published atomically and can
            // never diverge across concurrent same-page writers (e.g. an editor
            // save racing a PDF write_highlights on an hls__ page). If they could
            // diverge, the sync_file_content fast-path could match disk against a
            // rev that isn't the cached doc's and serve a stale doc. Lock order is
            // always cache → disk_revs; readers never hold disk_revs while taking
            // the cache lock, so this nesting can't deadlock. Sets only when the
            // page is actually cached (preserves "entry exists IFF cached").
            self.disk_revs.write().unwrap().insert(path_key, disk_rev);
        }
        // Bump cache_gen AFTER publishing the new content (and disk_revs), still
        // under the cache write lock. A reader loads cache_gen (Acquire) then takes
        // the cache read lock; because the bump (Release) happens-after the slot
        // write and before the lock is dropped, observing the new gen guarantees
        // the new doc is visible. So any derived result computed at gen G reflects
        // every edit whose gen is <= G — it can never be a stale whole-graph scan
        // that reads the OLD doc yet gets tagged (and served) at the fresh gen.
        // (Bumping FIRST left a window where the gen was new but the doc still old.)
        // The bump is unconditional — even on a cold cache (no slot to update) — so
        // a concurrent lock-free with_pages build still detects the race and retries.
        let newgen =
            self.move_cache_generation(&guard, None, graph_drift::IndexEffect::Sent(&coming));
        if bounded_foreground
            && cache_built
            && !identity_changed
            && !is_new_page
            && !failures_changed
        {
            // Exact content-only publication does not alter effective identity.
            // Advance the shared generation tag in O(1): creation requires warm
            // parsed semantic evidence and must not rebuild it merely because
            // unrelated bytes changed.
            let mut identity_guard = self.effective_identity_index.write().unwrap();
            let advanced = identity_guard
                .as_ref()
                .is_some_and(|index| index.retag_generation(newgen - 1, newgen));
            if !advanced {
                *identity_guard = None;
            }
        } else if let Some(pages) = guard.as_ref() {
            *self.effective_identity_index.write().unwrap() = Some(Arc::new(
                build_effective_identity_index(newgen, pages, resulting_failures.to_vec()),
            ));
        } else {
            self.advance_effective_identity_after_upsert(
                newgen,
                &evict_entry,
                resulting_failures.to_vec(),
            );
        }
        let page_inventory_complete = resulting_failures.is_empty();
        // A page that recovers from unreadable travels as its own update,
        // like any other: completeness is the index owner's to restore (with
        // no owner, the next read's repair). A complete snapshot offered from
        // here as well was a second decider, never taken while an owner
        // existed and a whole-index build without one (GH #543, audit R9-04).
        *failures_guard = resulting_failures;
        // The update is queued before the new generation can be observed. A
        // warm that reads this generation keeps its inventory and leaves this
        // page to its update (audit IT-03); queued after the lock, the warm
        // could publish readiness at this generation with the page's old
        // rows, and an answer cached at it would outlive the update.
        if let Some(projection) = projection.as_ref() {
            projection.enqueue_replace(
                newgen,
                evict_entry.clone(),
                Arc::clone(&evict_doc),
                projection_revision,
                Arc::new(self.config().parse_config()),
            );
        }
        drop(failures_guard);
        drop(guard);
        #[cfg(test)]
        {
            let pause = self
                .page_build_test
                .upsert_published_pause
                .lock()
                .unwrap()
                .take();
            if let Some(pause) = pause {
                pause.reached.wait();
                pause.release.wait();
            }
        }
        // Same treatment for the physical page inventory, and for the same
        // reason as other generation-bound derived inventories. `list_pages` is keyed on raw
        // `cache_gen` equality, and its ONLY way to rebuild is to walk the graph
        // text scope, re-read every file from disk and re-parse each one.
        // Measured at ~35 µs/file, dead linear from 506 to 8,006 pages: 243 ms on
        // a real 5,225-file graph, and `find_entry` rebuilds straight from it. So
        // every ordinary save made the next navigation or `[[` autocomplete stall
        // for a quarter of a second, repeating after every typing pause.
        // (Direct Files perf audit, 2026-08-09, F1.)
        //
        // The inventory is a function of the file set plus each file's parsed
        // identity, so it survives exactly the conditions the effective-identity
        // index above already tests: an existing page whose content changed but
        // whose identity, page-ness and failure set did not. Deliberately NOT
        // re-tagged more broadly — a `title::` edit IS an identity change and
        // does move an entry. `referenced_page_names` is likewise left to
        // rebuild: it is genuinely derived from block content, so a content edit
        // really can change it, and it recomputes from the warm in-memory cache
        // rather than from disk.
        //
        // The `generation + 1 == newgen` guard is load-bearing, and its absence
        // was a real regression: re-tagging is only sound for a list that was
        // CURRENT as of the previous generation. A list cached before an
        // unrelated page was created is stale at an older generation, and
        // stamping it as current republishes that staleness permanently —
        // `list_pages` is keyed on generation equality, so it never rebuilds and
        // the missing page becomes unloadable.
        let mut page_list_advanced = false;
        if cache_built && !failures_changed {
            if let Some((generation, entries)) = self.page_list_cache.write().unwrap().as_mut() {
                if *generation + 1 == newgen {
                    if let Some(existing) = entries
                        .iter_mut()
                        .find(|entry| entry.path == evict_entry.path)
                    {
                        *existing = evict_entry.clone();
                    } else {
                        entries.push(evict_entry.clone());
                    }
                    *generation = newgen;
                    page_list_advanced = true;
                }
            }
        }
        // Creation paths and watcher reconciliation may intentionally have no
        // prior list memo to retag. The warm parsed cache already contains the
        // exact newly parsed entry and every survivor, so rebuild the in-memory
        // inventory from it rather than reopening and reparsing the graph.
        if cache_built && !page_list_advanced && page_inventory_complete {
            self.publish_warm_page_inventory(newgen);
        }
        // Scoped query/backlink invalidation (#52): a content edit to one page
        // can't change a derived result the page doesn't participate in, so keep
        // those (advancing their generation) and recompute only the entries this
        // page is in or now matches. An alias change, a new page, or a cold cache
        // has graph-wide effects → drop everything. Guarded by the differential
        // fuzz oracle in tests/derived_cache_fuzz.rs.
        let scoped = cache_built && !alias_touched && !is_new_page && !identity_changed;
        self.scope_derived_invalidation(
            &evict_entry,
            previous_doc.as_deref(),
            &evict_doc,
            newgen,
            scoped,
        );
    }

    /// See `cache_upsert`. When `scoped`, evict only derived entries the edited
    /// page (`entry`, `doc`) participates in and re-tag the survivors to `newgen`;
    /// otherwise drop the whole derived cache.
    fn scope_derived_invalidation(
        &self,
        entry: &PageEntry,
        previous_doc: Option<&Document>,
        doc: &Document,
        newgen: u64,
        scoped: bool,
    ) {
        // Most edits have no derived query state to invalidate. Avoid building
        // graph-wide alias and real-page inputs merely to rediscover that both
        // memo families are absent. A memo installed after this observation is
        // computed against the already-published cache generation and content.
        if self.derived_cache.read().unwrap().is_none() {
            return;
        }
        // Resolve aliases BEFORE taking the derived lock (page_aliases may take the
        // cache lock); never hold derived while taking cache.
        let (aliases, real_pages) = if scoped {
            (self.page_aliases(), crate::query::real_page_names(self))
        } else {
            (Vec::new(), crate::query::RealPageNames::new())
        };
        let today = crate::date::JournalDate::today().ordinal_key();
        // Hold the derived write lock across the WHOLE prune+re-tag. This is
        // deliberately atomic: the keep/evict test (page_affects_*) is re-evaluated
        // against whatever entry is CURRENTLY in the map, so a result a concurrent
        // query inserted (possibly from an older page-doc) is re-judged and evicted
        // if this edit affects it — never kept on pointer identity. Combined with
        // the gen-after-content bump (cache_upsert), an entry that survives the
        // prune is provably unaffected by this edit and consistent at `newgen`.
        // (An earlier version evaluated off the lock and kept entries by Arc
        // ptr_eq; that could bless a stale concurrent recompute — reverted.)
        let pname = &entry.name;
        {
            let mut g = self.derived_cache.write().unwrap();
            let Some(dc) = g.as_mut() else {
                drop(g);
                return;
            };
            if !scoped || dc.today != today {
                *g = None; // full invalidate (alias/page-set/cold-cache, or day rollover)
                drop(g);
                return;
            }
            let mut removed_bytes = 0usize;
            dc.results.retain(|key, (result, result_bytes)| {
                // Evict iff this page is already in the result OR matches the key's
                // predicate in either the old or new page; keep (still correct)
                // otherwise. Comparing both parsed documents makes omitted overflow
                // matches visible without retaining a graph-sized membership set.
                if result
                    .result
                    .groups
                    .iter()
                    .any(|grp| crate::refs::same_page(&grp.page, pname))
                {
                    removed_bytes = removed_bytes.saturating_add(*result_bytes);
                    return false;
                }
                let page_affects = |candidate: &Document| match key.split_once('\0') {
                    Some(("b", target)) => crate::query::page_affects_backlinks(
                        &real_pages,
                        &aliases,
                        target,
                        entry,
                        candidate,
                    ),
                    Some(("u", target)) => crate::query::page_affects_unlinked(
                        &real_pages,
                        &aliases,
                        target,
                        entry,
                        candidate,
                    ),
                    Some(("br", uuid)) => {
                        crate::query::page_affects_block_referrers(uuid, candidate)
                    }
                    Some(("B", rest)) => rest.splitn(3, '\0').nth(2).is_none_or(|target| {
                        crate::query::page_affects_backlinks(
                            &real_pages,
                            &aliases,
                            target,
                            entry,
                            candidate,
                        )
                    }),
                    Some(("U" | "UI", rest)) => rest.splitn(3, '\0').nth(2).is_none_or(|target| {
                        crate::query::page_affects_unlinked(
                            &real_pages,
                            &aliases,
                            target,
                            entry,
                            candidate,
                        )
                    }),
                    Some(("R", rest)) => rest.splitn(3, '\0').nth(2).is_none_or(|uuid| {
                        crate::query::page_affects_block_referrers(uuid, candidate)
                    }),
                    _ => true, // unknown key shape → evict to stay safe
                };
                let affects = page_affects(doc) || previous_doc.is_some_and(&page_affects);
                if affects {
                    removed_bytes = removed_bytes.saturating_add(*result_bytes);
                }
                !affects
            });
            dc.bytes = dc.bytes.saturating_sub(removed_bytes);
            dc.lru.retain(|key| dc.results.contains_key(key));
            dc.generation = newgen; // survivors are valid for the post-bump generation
        }
    }

    /// Drop one page from the cache after a delete. `removed_file` is the
    /// inventory's answer, read under the identity lock the delete holds: the
    /// file it moved to the trash, or `None` when the page had no file (a page
    /// that exists only through references). So the projection delete is
    /// named in a session that holds no parsed cache (R6), and a page with no
    /// file changes nothing the index stores: marking the index stale for it
    /// walked the whole graph for a delete that touched no file (GH #543,
    /// audit R7-01).
    pub(super) fn cache_remove(&self, name: &str, kind: PageKind, removed_file: Option<PageEntry>) {
        // A page delete is a page-set change that can affect every backlink
        // and reference result, so drop the whole derived-reference cache.
        *self.derived_cache.write().unwrap() = None;
        let projection = self.direct_projection.get();
        let coming = self.index_delta_coming();
        let mut guard = self.cache.write().unwrap();
        let mut removed_entries: Vec<PageEntry> = removed_file.into_iter().collect();
        if let Some(pages) = guard.as_mut() {
            let pages = Arc::make_mut(pages);
            for (entry, _) in pages.iter().filter(|(entry, _)| {
                entry.kind == kind && crate::refs::same_page(&entry.name, name)
            }) {
                if !removed_entries
                    .iter()
                    .any(|removed| removed.path == entry.path)
                {
                    removed_entries.push(entry.clone());
                }
            }
            let removed_paths = pages
                .iter()
                .filter(|(e, _)| e.kind == kind && crate::refs::same_page(&e.name, name))
                .map(|(e, _)| e.path.clone())
                .collect::<Vec<_>>();
            pages.retain(|(e, _)| !(e.kind == kind && crate::refs::same_page(&e.name, name)));
            // Drop all exact revisions removed by this ambiguity-validated logical
            // delete under the cache lock (same cache → disk_revs order as
            // cache_upsert) so the two never diverge.
            let mut revs = self.disk_revs.write().unwrap();
            for path in removed_paths {
                revs.remove(&path);
            }
        }
        *self.cache_index.write().unwrap() =
            guard.as_ref().map(|pages| build_page_cache_index(pages));
        {
            // A deleted page owns nothing (audit R15-03).
            let mut failures = self.page_index_failures.write().unwrap();
            for entry in &removed_entries {
                failures.retire(&entry.rel_path);
            }
        }
        // Bump AFTER the removal is published (under the cache lock), so a reader
        // that loads the new gen is guaranteed to see the page gone — see the
        // gen-after-content note in cache_upsert.
        let newgen = if removed_entries.is_empty() {
            // No file left the graph: the page set and the index stand.
            self.move_cache_generation(
                &guard,
                None,
                graph_drift::IndexEffect::Unchanged(projection.as_ref()),
            )
        } else {
            self.move_cache_generation(
                &guard,
                Some(graph_drift::StructuralChange::Removed(
                    removed_entries
                        .iter()
                        .map(|entry| entry.path.clone())
                        .collect(),
                )),
                graph_drift::IndexEffect::Sent(&coming),
            )
        };
        if let Some(pages) = guard.as_ref() {
            *self.effective_identity_index.write().unwrap() =
                Some(Arc::new(build_effective_identity_index(
                    newgen,
                    pages,
                    self.page_index_failures.read().unwrap().to_vec(),
                )));
        } else {
            *self.effective_identity_index.write().unwrap() = None;
        }
        drop(guard);
        let mut page_list_advanced = false;
        if let Some((generation, entries)) = self.page_list_cache.write().unwrap().as_mut() {
            if *generation + 1 == newgen {
                entries.retain(|entry| {
                    !removed_entries
                        .iter()
                        .any(|removed| removed.path == entry.path)
                });
                *generation = newgen;
                page_list_advanced = true;
            }
        }
        if !page_list_advanced && self.page_index_failures.read().unwrap().is_empty() {
            self.publish_warm_page_inventory(newgen);
        }
        for entry in removed_entries {
            self.direct_projection_enqueue_delete(newgen, entry);
        }
    }

    /// Drop one physical page from the cache after its file disappears. Unlike
    /// `cache_remove`, this preserves same-name siblings and rebuilds the logical
    /// first-wins index from the surviving entries.
    pub(super) fn cache_remove_path(&self, entry: &PageEntry) {
        // A page delete is a page-set change (affects namespaces, exists-by-ref,
        // every backlink/query) — drop the whole derived cache.
        *self.derived_cache.write().unwrap() = None;
        let coming = self.index_delta_coming();
        let mut guard = self.cache.write().unwrap();
        // A page that is gone owns nothing: its failure goes with it, or its
        // name stays refused for the session (audit R15-03).
        self.page_index_failures
            .write()
            .unwrap()
            .retire(&entry.rel_path);
        if let Some(pages) = guard.as_mut() {
            let pages = Arc::make_mut(pages);
            if let Some(i) = self.cached_page_index_for_path(pages, &entry.path) {
                pages.remove(i);
                // Drop the rev under the cache lock (same cache → disk_revs order
                // as cache_upsert) so the two never diverge.
                self.disk_revs.write().unwrap().remove(&entry.path);
            }
            *self.cache_index.write().unwrap() = Some(build_page_cache_index(pages));
        }
        // Bump AFTER the removal is published (under the cache lock), so a reader
        // that loads the new gen is guaranteed to see the page gone — see the
        // gen-after-content note in cache_upsert.
        let newgen = self.move_cache_generation(
            &guard,
            Some(graph_drift::StructuralChange::Removed(vec![entry
                .path
                .clone()])),
            graph_drift::IndexEffect::Sent(&coming),
        );
        if let Some(pages) = guard.as_ref() {
            *self.effective_identity_index.write().unwrap() =
                Some(Arc::new(build_effective_identity_index(
                    newgen,
                    pages,
                    self.page_index_failures.read().unwrap().to_vec(),
                )));
        } else {
            *self.effective_identity_index.write().unwrap() = None;
        }
        drop(guard);
        let mut page_list_advanced = false;
        if let Some((generation, entries)) = self.page_list_cache.write().unwrap().as_mut() {
            if *generation + 1 == newgen {
                entries.retain(|candidate| candidate.path != entry.path);
                *generation = newgen;
                page_list_advanced = true;
            }
        }
        if !page_list_advanced && self.page_index_failures.read().unwrap().is_empty() {
            self.publish_warm_page_inventory(newgen);
        }
        self.direct_projection_enqueue_delete(newgen, entry.clone());
    }
}

#[cfg(test)]
thread_local! {
    static INDEXING_BUILD_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks this thread as running an indexing build for its lifetime, so the
/// parses it makes count as indexing rather than a consumer's.
#[cfg(test)]
struct IndexingBuildTest(bool);

#[cfg(test)]
impl IndexingBuildTest {
    fn enter() -> Self {
        Self(INDEXING_BUILD_TEST.with(|flag| flag.replace(true)))
    }
}

#[cfg(test)]
impl Drop for IndexingBuildTest {
    fn drop(&mut self) {
        INDEXING_BUILD_TEST.with(|flag| flag.set(self.0));
    }
}
