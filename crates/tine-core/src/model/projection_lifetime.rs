//! Graph's Direct projection lifetime: attaching and detaching it, the Concord
//! ledger, enqueueing replacements and deletes, marking it stale, and the
//! projection-backed page inventory, on-demand parses and property facets.

use super::*;

impl Graph {
    /// Attach Direct Files' app-private disposable SQLite projection.
    ///
    /// This never reads or writes graph files, and it queues nothing: the
    /// graph's one index owner offers the projection its payload when the
    /// first warm installs the parsed cache. So attach comes first, before
    /// any parse, exactly as the app opens and refreshes a graph
    /// (`attach_direct_files_services`, whose order src-tauri's graph tests
    /// pin). A second producer of the full payload here, seeded from a cache
    /// parsed before attach, served only fixtures in the other order and
    /// raced the owner's own offer (GH #543, audit R8-14).
    pub fn attach_direct_projection(&self, path: PathBuf) -> io::Result<()> {
        debug_assert!(
            self.cache.read().unwrap().is_none(),
            "attach the Direct projection before the first parse; nothing \
             offers it a cache installed earlier (GH #543, R8-14)"
        );
        let projection = Arc::new(crate::direct_projection::DirectProjection::start(
            path,
            Some(Arc::new(self.config().parse_config())),
        )?);
        self.direct_projection.attach(projection);
        Ok(())
    }

    /// The parsed page set, if one is installed, as the very snapshot the
    /// projection was offered.
    #[cfg(test)]
    pub(crate) fn installed_page_snapshot_test(
        &self,
    ) -> Option<Arc<Vec<(PageEntry, Arc<Document>)>>> {
        self.cache.read().unwrap().as_ref().map(Arc::clone)
    }

    /// Detach the Direct Files projection and wait for its writer to exit.
    ///
    /// A configuration refresh reopens the same root and attaches a projection
    /// at the SAME path. A second `DirectProjection::start` while this worker
    /// still holds the exclusive writer lease races it (observed as a 15 s
    /// "did not converge" under load), so the old worker is retired first.
    /// Returns whether it exited within `timeout`; `false` is reported by the
    /// caller, never treated as fatal — the replacement attach then decides.
    pub fn detach_direct_projection(&self, timeout: std::time::Duration) -> bool {
        let projection = self.direct_projection.take();
        match projection {
            Some(projection) => projection.close_and_wait_for_worker(timeout),
            None => true,
        }
    }

    /// Register the application's existing watcher wake channel and observe the
    /// last committed-image notification. This is notification state only; query reads
    /// never compare it with an edit or wait for it to advance.
    pub fn observe_direct_projection_commits(
        &self,
        wake: std::sync::mpsc::Sender<()>,
    ) -> Option<u64> {
        self.direct_projection
            .get()
            .map(|projection| projection.observe_commits(wake))
    }

    /// What the graph-sized index work is doing, for the indexing progress bar
    /// (GH #543). `None` only when nothing graph-sized is running: a current
    /// index does not hide a whole-graph parse a page consumer started
    /// (audit R5-03), so the running pass is consulted before readiness.
    /// Presentation only.
    pub fn indexing_progress(&self) -> Option<crate::indexing_progress::IndexingProgress> {
        use crate::indexing_progress::{IndexingPhase, IndexingProgress};
        let projection = self.direct_projection.get();
        let Some(projection) = projection else {
            return self.indexing_progress.snapshot();
        };
        if let Some(build) = projection.build_progress() {
            return Some(build);
        }
        if let Some(pass) = self.indexing_progress.snapshot() {
            return Some(pass);
        }
        let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        if projection.ready_at(generation) {
            return None;
        }
        // Between passes: a queued snapshot or an owner's coming pass is
        // still graph-sized work, so keep the bar up rather than flicker it
        // off. A backoff is not: nothing runs until it ends.
        if projection.backing_off() {
            return None;
        }
        // The index's one decider says whether graph-sized work is owed; the
        // bar asks it, as the readers that wait do. A full snapshot applied
        // as a repair is `InHand` with no build progress and no pass
        // running, and an earlier rule of the bar's own hid it while readers
        // waited for it (GH #543, audit R10-04). An update turn shows the
        // bar only through the lowering loop's count, past one batch.
        use crate::direct_projection::IndexNeed;
        let (need, _) = projection.index_need_now();
        matches!(
            need,
            IndexNeed::SettingUp | IndexNeed::InHand | IndexNeed::Fresh | IndexNeed::Validate
        )
        .then(|| IndexingProgress::unmeasured(IndexingPhase::Indexing))
    }

    /// Test barrier for ordinary producer progression. Production queries read
    /// the current coherent image and never wait for a saved-edit generation.
    #[cfg(test)]
    pub(crate) fn wait_for_direct_projection_for_test(
        &self,
        timeout: std::time::Duration,
    ) -> io::Result<()> {
        use crate::direct_projection::ProjectionProgress;
        let Some(projection) = self.direct_projection.get() else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "snapshot query projection is not attached",
            ));
        };
        let started = std::time::Instant::now();
        loop {
            let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
            match projection.progress_at(generation) {
                ProjectionProgress::Ready => return Ok(()),
                ProjectionProgress::Working(_) => {}
                ProjectionProgress::Stale => {
                    return Err(io::Error::other(
                        "test projection stopped before the producer generation",
                    ))
                }
                ProjectionProgress::Stopped => {
                    return Err(io::Error::other(
                        "snapshot query projection worker is unavailable",
                    ))
                }
                ProjectionProgress::Failed(class) => {
                    return Err(io::Error::other(format!(
                        "snapshot query projection failed: {}",
                        class.as_str()
                    )))
                }
            }
            if started.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "snapshot query projection did not finish indexing in time",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Attach the Concord base ledger (ADR 0056) rooted at `dir` (an
    /// app-private directory OUTSIDE the graph tree). Idempotent; the first
    /// attach wins. Queues a background prune of unreferenced blobs.
    pub fn attach_concord_ledger(&self, dir: PathBuf) {
        let ledger = Arc::new(crate::concord_ledger::ConcordLedger::new(dir));
        if self.concord_ledger.set(Arc::clone(&ledger)).is_ok() {
            ledger.queue_prune();
        }
    }

    /// The attached ledger, if any (None ⇒ every Concord hook no-ops).
    pub fn concord_ledger(&self) -> Option<&Arc<crate::concord_ledger::ConcordLedger>> {
        self.concord_ledger.get()
    }

    /// Quitting: wait until `deadline` for this graph's queued ledger updates.
    /// True when there was nothing to wait for or the queue drained in time.
    pub fn drain_concord_ledger_for_exit(&self, deadline: std::time::Instant) -> bool {
        self.concord_ledger
            .get()
            .is_none_or(|ledger| ledger.drain_for_exit(deadline))
    }

    /// Best-effort ledger update: `content` is now the exact text Tine and the
    /// disk agree on for `path`. Called after a successful save commit and
    /// after an external-change admission. Foreground cost is one channel send;
    /// non-page paths (config, assets) are filtered out here.
    pub(super) fn concord_record_agreed(&self, path: &Path, content: &str) {
        let Some(ledger) = self.concord_ledger.get() else {
            return;
        };
        if self.entry_for_path(path).is_none() || path_is_sync_conflict(path) {
            return;
        }
        ledger.record(&self.rel_path(path), content);
    }

    /// The winner page a conflict copy shadows (same dir, same extension, base
    /// stem), as a graph-relative path — the identity the ledger pins under.
    pub(super) fn conflict_winner_rel(&self, conflict_path: &Path) -> Option<String> {
        let ext = text_extension_from_path(conflict_path)?;
        let stem = conflict_path.file_stem()?.to_str()?;
        let base_stem = sync_conflict_base(stem)?;
        let winner = conflict_path.parent()?.join(format!("{base_stem}.{ext}"));
        Some(self.rel_path(&winner))
    }

    /// Queue a parsed snapshot for a fresh build. Only the index owner
    /// offers, and only when the decider owes a fresh image
    /// ([`Graph::offer_installed_cache`]); a page consumer that installs the
    /// parsed cache offers nothing (GH #543, audits R4-04, R10-03).
    ///
    /// The outcome says whether the snapshot was refused as older than the
    /// queue.
    pub(super) fn direct_projection_enqueue_full(
        &self,
        generation: u64,
        pages: Arc<Vec<(PageEntry, Arc<Document>)>>,
        revisions: Arc<std::collections::HashMap<PathBuf, String>>,
        source_complete: bool,
    ) -> FullOfferOutcome {
        let Some(projection) = self.direct_projection.get() else {
            return FullOfferOutcome::NoIndex;
        };
        let retained = if source_complete {
            Vec::new()
        } else {
            self.unread_sources()
        };
        if projection.enqueue_full(
            generation,
            pages,
            revisions,
            Arc::new(self.config().parse_config()),
            retained,
        ) {
            FullOfferOutcome::Queued
        } else {
            FullOfferOutcome::Outdated
        }
    }

    /// What the installed parsed cache could not read, list or parse and
    /// still exists: pages and directories, as graph-relative paths, or `""`
    /// when the graph itself could not be listed. The index keeps the stored
    /// rows beneath them; a failed page that is gone is not listed, so its
    /// rows go.
    fn unread_sources(&self) -> Vec<String> {
        let failures = self.page_index_failures.read().unwrap();
        if failures
            .as_slice()
            .iter()
            .any(|failure| failure.starts_with(super::page_cache::GRAPH_TEXT_SCOPE_FAILURE))
        {
            return vec![String::new()];
        }
        failures
            .as_slice()
            .iter()
            .filter_map(|failure| {
                super::page_cache::failure_sources(failure)
                    .find(|source| std::fs::symlink_metadata(self.root.join(source)).is_ok())
            })
            .map(str::to_owned)
            .collect()
    }

    /// Offer the installed parsed cache to the index for a fresh build: the
    /// owner's `Fresh` pass, and the parsed-cache warm of a graph whose index
    /// is gone.
    pub(super) fn offer_installed_cache(&self) -> FullOfferOutcome {
        let captured = {
            let cache = self.cache.read().unwrap();
            cache.as_ref().map(|pages| {
                (
                    self.cache_gen.load(std::sync::atomic::Ordering::Acquire),
                    Arc::clone(pages),
                    Arc::new(self.disk_revs.read().unwrap().clone()),
                    self.page_index_failures.read().unwrap().is_empty(),
                )
            })
        };
        let Some((generation, pages, revisions, source_complete)) = captured else {
            return FullOfferOutcome::NoIndex;
        };
        self.direct_projection_enqueue_full(generation, pages, revisions, source_complete)
    }

    /// The ONE way a mutation that changes the page SET tells the index what it
    /// did. Renames, merges and file rescues all move or retire physical page
    /// paths, and each one that skipped this left the index holding rows for a
    /// file that no longer exists — with nothing queued, no progress shown, and
    /// search answering from it indefinitely, because a successful answer never
    /// reaches the repair a refusal would start (GH #543, third and fourth
    /// audits). `cache_upsert`/`cache_remove` are the one-page equivalents for
    /// an ordinary save and delete.
    ///
    /// The whole change goes in one call: published one delta at a time, the
    /// queue can empty between them and readiness is announced for a generation
    /// that is only half enqueued.
    pub(super) fn direct_projection_publish_page_set(
        &self,
        generation: u64,
        changes: Vec<crate::direct_projection::PageSetChange>,
    ) {
        for change in &changes {
            if let crate::direct_projection::PageSetChange::Delete { entry } = change {
                self.session_page_ids.write().unwrap().remove(&entry.path);
            }
        }
        if let Some(projection) = self.direct_projection.get() {
            projection.enqueue_page_set(
                generation,
                changes,
                Arc::new(self.config().parse_config()),
            );
        }
    }

    pub(super) fn direct_projection_enqueue_delete(&self, generation: u64, entry: PageEntry) {
        self.session_page_ids.write().unwrap().remove(&entry.path);
        if let Some(projection) = self.direct_projection.get() {
            // A delete lowers nothing, so it carries no parse config (F11).
            projection.enqueue_delete(generation, entry);
        }
    }

    /// Register this graph's index owner when its thread is scheduled, not
    /// when it starts. The app delays that thread so the first journal paint
    /// goes first, and the page list requested by that same paint used to
    /// find no warm announced, parse every page, and queue a full snapshot
    /// ahead of the warm (GH #543). Hand the returned value to
    /// [`Graph::run_index_owner`]; a thread cancelled before then drops it,
    /// so a reader never waits on an owner nobody runs.
    pub fn register_index_owner(&self) -> IndexOwner {
        IndexOwner(
            self.direct_projection
                .get()
                .map(|projection| projection.register_owner()),
        )
    }

    /// Readiness for a whole-graph derived read (page list, aliases, property
    /// owners, block-ref counts). Beyond the short delta wait, a read that
    /// finds the launch survey, a fresh build or queued edits in flight and
    /// no complete parsed cache keeps waiting for them: its only alternative
    /// is parsing every page, and on a warm reopen that parse queued a full
    /// snapshot which outranked the survey and doubled the time to a working
    /// search (GH #543). The survey finishes no later than such a parse
    /// would; if it gives up, the read falls back as before. With a parsed cache present the fallback is cheap, so no
    /// extra wait (this also keeps the warm thread from waiting on itself).
    /// Returns the generation the projection is ready at, which is newer than
    /// `generation` when a page was published during the wait. A
    /// `LaunchStored` read does not wait while the stored image is served
    /// (launch design D2): the display re-asks when the check lands.
    pub(super) fn wait_for_derived_read(
        &self,
        projection: &crate::direct_projection::DirectProjection,
        mut generation: u64,
        currency: crate::direct_projection::Currency,
    ) -> Option<u64> {
        use crate::direct_projection::ProjectionProgress;
        use crate::query::QueryReadinessReason as Reason;
        debug_assert!(
            !super::derived_reads::OwnerThread::current(),
            "an index owner read through the readiness wait, which waits for the owner itself \
             (GH #543, audit R8-01)"
        );
        #[cfg(test)]
        {
            let pause = self
                .page_build_test
                .derived_read_wait
                .lock()
                .unwrap()
                .take();
            if let Some(pause) = pause {
                pause.reached.wait();
                pause.release.wait();
            }
        }
        loop {
            let at = crate::direct_projection::ReadAt {
                generation,
                currency,
            };
            if projection.wait_ready_at(at) {
                return Some(generation);
            }
            // A replaced graph's reads are no longer anyone's to wait for,
            // and no read waits past its deadline: it answers from the
            // parsed pages then, as when nothing is coming (GH #594 L3).
            if self.is_retired() || super::derived_reads::ReadDeadline::passed() {
                return None;
            }
            // With an index owner registered, whether work is coming is the
            // one predicate the owner itself runs on, and only that one: a
            // rebuild it will run or a validation it owes is coming even
            // before it starts, and a backoff or a foreign lease is not. A
            // second rule ORed beside it (the worker's progress) outvoted the
            // owner's answer (GH #543, audit R8-13). Parsing here instead
            // reads every page to answer what that pass is about to settle.
            // Without an owner, only work already handed to the worker is
            // coming.
            let coming = if projection.owner_registered() {
                projection.coming()
            } else {
                match projection.progress_at(generation) {
                    // `Busy`: the worker has taken the queued warm or edit and is
                    // applying it.
                    ProjectionProgress::Working(Reason::Indexing | Reason::Busy) => true,
                    // An edit queued behind a turn -- today's journal and a save at
                    // launch, behind a slow disk -- lands on a validated image and
                    // readiness follows. Parsing the graph instead reads every page
                    // to answer what one delta settles. Before validation the edit
                    // may lower, but readiness waits for the inventory that
                    // validates the image, so it is not by itself coming.
                    ProjectionProgress::Working(Reason::PendingEdits) => projection.validated(),
                    _ => false,
                }
            };
            // The cache answers as the index would only when it holds every
            // page: for a page Tine cannot read, the index keeps its stored
            // rows and the cache has none, so an answer flipped with
            // readiness (audit R15-07). Then the read waits for the index.
            let cached = {
                let cache = self.cache.read().unwrap();
                cache.is_some() && self.page_index_failures.read().unwrap().is_empty()
            };
            if !coming || cached {
                if projection.ready_at(generation) {
                    return Some(generation);
                }
                // The graph moved on while this read waited, and the index
                // may already be ready at the new generation: exact-generation
                // readiness at the old one is not coming, and falling back
                // parsed the whole graph to answer what the index holds
                // (GH #543).
                let current = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
                if cached || current == generation {
                    if cached {
                        Self::note_cache_decline();
                    }
                    #[cfg(test)]
                    if cached {
                        let pause = self
                            .page_build_test
                            .cache_decline_pause
                            .lock()
                            .unwrap()
                            .take();
                        if let Some(pause) = pause {
                            pause.reached.wait();
                            pause.release.wait();
                        }
                    }
                    return None;
                }
                generation = current;
                continue;
            }
            // Opening today's journal publishes it and moves the generation;
            // the warm keeps going across such moves, so follow it.
            generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// R6: the page inventory from the ready projection, rebuilt into the
    /// walk's `PageEntry` shape. `pages.name` is the effective (title::-aware)
    /// name because the producer lowers the effective entry; kind and a
    /// journal's day come from the row. Parsing the day back from the name
    /// failed the whole inventory for a date `title::` under a title format
    /// without a year, and every page list parsed the graph (GH #543, audit
    /// R13-06).
    pub(super) fn direct_projection_page_inventory(&self) -> Option<(u64, Vec<PageEntry>)> {
        self.indexed_read(|projection, at| self.direct_projection_page_inventory_at(projection, at))
    }

    fn direct_projection_page_inventory_at(
        &self,
        projection: &Arc<crate::direct_projection::DirectProjection>,
        at: crate::direct_projection::ReadAt,
    ) -> Option<(u64, Vec<PageEntry>)> {
        let rows = projection.page_inventory(at)?;
        let days = projection.journal_days(at)?;
        let mut entries = Vec::with_capacity(rows.len());
        for (name, rel_path, kind) in rows {
            let date_key = match kind {
                PageKind::Journal => days.get(&rel_path).copied(),
                PageKind::Page => None,
            };
            entries.push(PageEntry {
                name,
                kind,
                date_key,
                path: self.root.join(&rel_path),
                rel_path,
            });
        }
        entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
        Some((at.generation, entries))
    }

    /// R6: parse exactly the named pages for reference/fuzzy hydration when no
    /// parsed cache exists. The documents are returned to the caller and
    /// dropped after use — nothing is installed or retained.
    ///
    /// The paths are the index's candidates, a superset hint: a candidate
    /// that is gone from disk or does not parse is left out, as the page walk
    /// this read replaces leaves it out. Declining instead sent the caller to
    /// that walk, a whole-graph parse per reference panel, for as long as one
    /// page stayed unparseable or a delete stayed unreported (GH #594 L6).
    pub(super) fn parse_pages_on_demand(
        &self,
        generation: u64,
        paths: Vec<PathBuf>,
    ) -> Option<Vec<(PageEntry, Arc<Document>)>> {
        self.parse_pages_on_demand_inner(
            generation,
            paths.into_iter().map(|path| (path, None)).collect(),
        )
    }

    pub(super) fn parse_pages_on_demand_with_revisions(
        &self,
        generation: u64,
        sources: Vec<(PathBuf, String)>,
    ) -> Option<Vec<(PageEntry, Arc<Document>)>> {
        self.parse_pages_on_demand_inner(
            generation,
            sources
                .into_iter()
                .map(|(path, revision)| (path, Some(revision)))
                .collect(),
        )
    }

    fn parse_pages_on_demand_inner(
        &self,
        generation: u64,
        sources: Vec<(PathBuf, Option<String>)>,
    ) -> Option<Vec<(PageEntry, Arc<Document>)>> {
        let permit = self.admit_retained_graph_text_writer().ok()?;
        let mut pages = Vec::with_capacity(sources.len());
        let config_digest = self.config().parse_config().digest();
        for (relative, projected_revision) in sources {
            // A revision-checked source (a query's) must decline on any
            // mismatch; a candidate hint skips a page it cannot hydrate.
            let hint = projected_revision.is_none();
            let absolute = self.root.join(&relative);
            let Some(entry) = self.graph_inventory_entry(&absolute).ok()? else {
                if hint {
                    continue;
                }
                return None;
            };
            if entry.rel_path != relative.to_string_lossy() {
                return None;
            }
            let Some((content, _)) = self
                .graph_text_read_optional_text_with_identity(&permit, &entry.path)
                .ok()?
            else {
                if hint {
                    continue;
                }
                return None;
            };
            if projected_revision.is_some_and(|expected| {
                crate::direct_projection::projection_source_revision(
                    &content_rev(&content),
                    config_digest,
                ) != expected
            }) {
                return None;
            }
            #[cfg(test)]
            self.page_build_test
                .on_demand_parses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let parsed = isolate_page_parse(entry, &self.journal_format, |entry| {
                Some(self.parse_session_page_content(entry, &content))
            });
            let Ok(Some((effective, document, _))) = parsed else {
                if hint {
                    continue;
                }
                return None;
            };
            pages.push((effective, Arc::new(document)));
        }
        #[cfg(test)]
        DIRECT_HYDRATED_PAGES.with(|recorded| {
            recorded.borrow_mut().extend(
                pages
                    .iter()
                    .map(|(entry, _)| PathBuf::from(&entry.rel_path)),
            );
        });
        (self.cache_gen.load(std::sync::atomic::Ordering::Acquire) == generation).then_some(pages)
    }

    pub(super) fn direct_projection_property_facets(
        &self,
        autocomplete: bool,
        max_items: usize,
        max_bytes: usize,
    ) -> Option<(Vec<(String, Vec<String>)>, bool)> {
        self.indexed_read(|projection, at| {
            projection.property_facets(
                at,
                autocomplete,
                &self.config().block_hidden_properties,
                max_items,
                max_bytes,
            )
        })
    }

    /// The §6.2 registry row source when the Direct Files projection is READY
    /// (CLOSURE §4): the shared raw property stream and its same-snapshot page
    /// map, or `None` when the projection is not ready or the read refused.
    ///
    /// The generation is re-checked after the read for the same reason every
    /// other projection reader re-checks it: a snapshot that straddles a
    /// rebuild is not a snapshot.
    #[cfg(test)]
    pub(super) fn direct_projection_property_owner_rows(
        &self,
    ) -> Option<(
        Vec<crate::query::registry::OwnerRow>,
        std::collections::HashMap<String, crate::query::registry::PageMeta>,
    )> {
        self.indexed_read(|projection, at| projection.property_owner_rows(at))
    }
}

/// A registered index owner; see `Graph::register_index_owner`.
pub struct IndexOwner(#[allow(dead_code)] Option<crate::direct_projection::IndexOwnerRegistration>);

/// What became of a full snapshot offered to the index; see
/// [`Graph::direct_projection_enqueue_full`].
#[must_use = "a refused snapshot may owe its page changes another way"]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FullOfferOutcome {
    /// No index is attached; nothing is owed to one.
    NoIndex,
    /// The index takes it.
    Queued,
    /// The snapshot is older than the queue.
    Outdated,
}
