//! Graph's page inventory: listing pages, the published page-inventory
//! snapshot, and the referenced-page-names set.

use super::*;

impl Graph {
    /// List all pages and journals in the graph, for display: a graph whose
    /// text cannot be read lists nothing. A caller that acts on the listing
    /// (exports it, checks a name against it) uses [`Graph::try_list_pages`].
    pub fn list_pages(&self) -> Vec<PageEntry> {
        self.page_listing(false).unwrap_or_default()
    }

    /// [`Graph::list_pages`] for a caller that acts on the listing: a graph
    /// whose text cannot be read is an error, never an empty graph.
    pub fn try_list_pages(&self) -> io::Result<Vec<PageEntry>> {
        self.exact_read(|| self.page_listing(true))
    }

    /// Forget the memoized page list, as a reopen that never parsed has none:
    /// the next listing asks the index (statement census).
    #[cfg(test)]
    pub(crate) fn forget_page_list_test(&self) {
        *self.page_list_cache.write().unwrap() = None;
    }

    fn page_listing(&self, exact: bool) -> io::Result<Vec<PageEntry>> {
        let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        if let Some((g, entries)) = self.page_list_cache.read().unwrap().as_ref() {
            if *g == generation {
                return Ok(entries.clone());
            }
        }
        // One base inventory: the ready index's, or else the shared page-build
        // flight's. Cold inventory used to run its own whole-graph parse,
        // competing with the flight for every core and for storage and
        // starving the journal feed (GH #550).
        //
        // A graph whose text cannot be read, or a display read on a retired
        // graph, lists nothing for display and is an error to an acting
        // caller; neither answer is memoized, so it cannot outlive its cause.
        let base = match self.indexed_or_fallback(|| self.direct_projection_page_inventory()) {
            Ok((_, entries)) => entries,
            Err(PageFallback::Cache(pages)) => {
                pages.iter().map(|(entry, _)| entry.clone()).collect()
            }
            Err(PageFallback::Parse) => match self.page_snapshot(!exact) {
                Ok(Some(pages)) => pages.iter().map(|(entry, _)| entry.clone()).collect(),
                Ok(None) => return Ok(Vec::new()),
                Err(error) if exact => return Err(error),
                Err(_) => return Ok(Vec::new()),
            },
        };
        let failures = self.page_index_failures.read().unwrap().to_vec();
        let entries = if failures.is_empty() {
            base
        } else {
            match self.with_failed_paths_revalidated(base, &failures) {
                Ok(entries) => entries,
                Err(error) if exact => return Err(error),
                Err(_) => return Ok(Vec::new()),
            }
        };
        if self.answer_is_complete()
            && self.cache_gen.load(std::sync::atomic::Ordering::Acquire) == generation
        {
            *self.page_list_cache.write().unwrap() = Some((generation, entries.clone()));
        }
        Ok(entries)
    }

    /// `base` after known parse failures: its entries for healthy pages,
    /// with every failed path read and parsed again on its own. Both bases
    /// still hold a failed page as it was before it failed, and only a failed
    /// path can be stale, so only a failed path is revalidated: one that
    /// still fails to read or parse is left out, one that is gone is left
    /// out, and one that now parses is listed as it is now. An error when
    /// the graph text scope itself could not be read.
    ///
    /// This replaced a whole-graph read and parse taken whenever a failure
    /// was recorded and no parsed cache existed, as in every warm SQL
    /// session: one unreadable file cost a 10k-page parse per listing
    /// (GH #543, indexing audits IT-07, R3-04 and R4-03).
    fn with_failed_paths_revalidated(
        &self,
        base: Vec<PageEntry>,
        failures: &[String],
    ) -> io::Result<Vec<PageEntry>> {
        if let Some(scope) = failures
            .iter()
            .find(|failure| failure.starts_with(super::page_cache::GRAPH_TEXT_SCOPE_FAILURE))
        {
            return Err(io::Error::other(scope.clone()));
        }
        let permit = self.admit_retained_graph_text_writer()?;
        let mut recovered = Vec::new();
        for failure in failures {
            // A failure is a graph-relative path only when it names a file;
            // skip reasons ("<path>: <why>") name no file and list nothing.
            let path = self.root.join(failure);
            if !std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
                continue;
            }
            let Ok(Some((content, _))) =
                self.graph_text_read_optional_text_with_identity(&permit, &path)
            else {
                continue;
            };
            let Ok(Some(entry)) = self.graph_text_entry_for_path(&path) else {
                continue;
            };
            if let Ok((entry, _, _)) = parse_exact_page(self, &entry, &content) {
                recovered.push(entry);
            }
        }
        let failed = failures.iter().map(String::as_str).collect::<HashSet<_>>();
        let mut entries = base
            .into_iter()
            .filter(|entry| !failed.contains(entry.rel_path.as_str()))
            .collect::<Vec<_>>();
        entries.extend(recovered);
        Ok(entries)
    }

    /// Publish the exact physical/effective page inventory already represented by
    /// the warm parsed cache. This is used only after a successful scoped cache
    /// mutation; watcher parse failures deliberately leave the memo absent so a
    /// later listing revalidates the failed path from disk.
    pub(super) fn publish_warm_page_inventory(&self, generation: u64) {
        let entries = {
            let guard = self.cache.read().unwrap();
            if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) != generation {
                return;
            }
            let Some(pages) = guard.as_ref() else {
                return;
            };
            pages.iter().map(|(entry, _)| entry.clone()).collect()
        };
        if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) == generation {
            *self.page_list_cache.write().unwrap() = Some((generation, entries));
        }
    }

    /// Capture a current list memo before a transaction that must discard the
    /// parsed cache. The transaction may update this in memory from bytes it
    /// already owns, avoiding a second whole-graph read/parse after commit.
    pub(super) fn current_page_inventory_snapshot(&self) -> Option<Vec<PageEntry>> {
        let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        self.page_list_cache
            .read()
            .unwrap()
            .as_ref()
            .filter(|(memo_generation, _)| *memo_generation == generation)
            .map(|(_, entries)| entries.clone())
    }

    /// Publish the list memo a transaction updated. The failures it found
    /// are already recorded ([`Graph::note_graph_text_state`]): the record
    /// describes the disk and outlives the parsed cache.
    pub(super) fn publish_page_inventory_snapshot(&self, mut entries: Vec<PageEntry>) {
        entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
        let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        *self.page_list_cache.write().unwrap() = Some((generation, entries));
    }

    /// Page names referenced anywhere in the graph — inline `[[link]]`/`#tag`/
    /// `#[[..]]` plus `tags::`/`alias::` property values (block- and page-level) —
    /// display case preserved, deduped case-insensitively. These are the pages
    /// that "exist" by reference even without a file of their own (OG semantics),
    /// so autocomplete/quick-switch can offer them instead of "Create …".
    ///
    /// Read from the disposable SQLite fact projection at the exact current
    /// cache generation. If that projection is unavailable or stale, the
    /// already-parsed whole-graph cache is the correctness fallback; this never
    /// forces a full-graph parse (it runs on autocomplete keystrokes).
    pub fn referenced_page_names(&self) -> Vec<String> {
        self.referenced_page_names_versioned(None)
            .names
            .unwrap_or_default()
    }

    /// The same inventory, plus an order-independent digest of it, so a caller
    /// that already holds an identical set can skip transporting it.
    ///
    /// The frontend asks for this after every typing lull, because its resource
    /// is keyed on `dataRev` and a save bumps that. The answer almost never
    /// changes — typing inside a block rarely adds or removes a `[[link]]` — but
    /// several thousand strings were serialised, sent over IPC and re-parsed on
    /// the UI thread every time. Passing back the digest the caller already has
    /// turns the unchanged case into an integer. (Direct Files performance audit
    /// 2026-08-09, finding F7; 15–23 ms and ~5,000 names at 5,225 files.)
    ///
    /// The digest is a commutative fold, deliberately: the memo's own order comes
    /// from a `HashMap`, so a sequence-dependent hash would report spurious
    /// changes. It carries the count as well, so adding a name that collides with
    /// a removed one still shows up unless the count also matches.
    pub fn referenced_page_names_versioned(&self, known: Option<u64>) -> ReferencedPageNames {
        let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        if let Some((cached, digest, names)) = self.referenced_names_cache.read().unwrap().as_ref()
        {
            if *cached == generation {
                return ReferencedPageNames::answer(*digest, names, known);
            }
        }
        let indexed = self.indexed_or_fallback(|| self.direct_projection_referenced_page_names());
        if let Err(PageFallback::Cache(pages)) = &indexed {
            let names = referenced_page_names_from_snapshot(pages);
            let digest = referenced_names_digest(&names);
            return ReferencedPageNames::answer(digest, &names, known);
        }
        if let Ok(names) = indexed {
            let digest = referenced_names_digest(&names);
            let answer = ReferencedPageNames::answer(digest, &names, known);
            // Only a read that still matches the generation we keyed on may be
            // memoized. The projection revalidates internally, but the
            // generation can move between our load and its answer, and a set
            // recorded under a stale key would outlive the edit that
            // invalidated it — an autocomplete offering pages that no longer
            // exist, or missing one just linked.
            if self.answer_is_complete()
                && self.cache_gen.load(std::sync::atomic::Ordering::Acquire) == generation
            {
                *self.referenced_names_cache.write().unwrap() = Some((generation, digest, names));
            }
            return answer;
        }
        // The parser fallback is deliberately NOT memoized: it answers with an
        // empty set when the page cache is not yet warm (it must not force a
        // parse here), and caching that would make "this graph references no
        // pages" stick for the whole generation — autocomplete would silently
        // offer nothing until the next edit happened to bump it.
        let names = self.rebuild_referenced_page_names();
        let digest = referenced_names_digest(&names);
        ReferencedPageNames::answer(digest, &names, known)
    }

    fn rebuild_referenced_page_names(&self) -> Vec<String> {
        let guard = self.cache.read().unwrap();
        let Some(pages) = guard.as_ref() else {
            return Vec::new(); // cache not warm — don't force a parse
        };
        referenced_page_names_from_snapshot(pages)
    }
}

pub(super) fn referenced_page_names_from_snapshot(
    pages: &[(PageEntry, Arc<Document>)],
) -> Vec<String> {
    crate::query::referenced_page_names_from_snapshot_cancellable(pages, &|| false)
        .unwrap_or_default()
}
